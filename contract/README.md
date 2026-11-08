# PayShield NEAR Smart Contract

PayShield NEAR is an owner-governed smart contract on the NEAR Protocol designed to constrain autonomous AI agents. The contract acts as an on-chain policy firewall and spending-control vault: it holds operational funds and enforces policy limits, recipient allowlists, UTC daily spending quotas, balance safety checks, and multi-step human-in-the-loop approvals before releasing funds via native NEAR transfer promises.

---

## 1. High-Level Architecture & Roles

The contract defines three primary security entities:

1. **Owner (`owner_id`):** The human administrator or multi-sig governance account. The owner holds exclusive authorization to:
   - Configure recipient security statuses (`Allowed`, `PendingApproval`, `Blocked`).
   - Approve or reject payment proposals submitted by the agent.
   - Reconcile stuck in-flight payments if an asynchronous transfer callback fails to settle within the designated timeout.
2. **Authorized Agent (`agent_id`):** The operational AI agent account. The agent holds authorization to:
   - Initiate direct payments under the configured approval threshold to explicitly allowed recipients (`pay_direct`).
   - Submit formal payment proposals for review when transactions exceed the approval threshold or target recipients requiring oversight (`propose_payment`).
   - Preflight and execute approved payment proposals (`execute_proposal`).
3. **Recipient Security Classifications (`RecipientStatus`):**
   - `Allowed`: Approved counterparties. Eligible for autonomous direct payments (up to the approval threshold) as well as proposal-backed payments.
   - `PendingApproval`: Counterparties requiring explicit human review. Direct payments are strictly forbidden; payments must proceed through the proposal and owner approval workflow.
   - `Blocked`: Explicitly prohibited or malicious addresses. All transactions—both direct payments and proposals—are hard-rejected at the contract boundary.
   - *(Unregistered / Unknown):* Evaluated as `PendingApproval` for proposal routing, but strictly rejected for direct payments.

---

## 2. Initialization Parameters

The contract is initialized once via the `init` method. Re-initialization is blocked (`!env::state_exists()`):

```bash
near contract call-function as-transaction <CONTRACT_ACCOUNT_ID> init json-args '{
  "owner_id": "<OWNER_ACCOUNT_ID>",
  "agent_id": "<AGENT_ACCOUNT_ID>",
  "initial_policy": {
    "max_amount_per_tx": "1000000000000000000000000",
    "require_approval_above": "500000000000000000000000",
    "daily_spending_limit": "5000000000000000000000000"
  }
}' prepaid-gas '30.0 Tgas' attached-deposit '0 NEAR'
```

### Policy Struct (`PaymentPolicy`)
- `max_amount_per_tx`: Absolute upper bound per single transaction (in yoctoNEAR). Any transaction exceeding this is hard-rejected.
- `require_approval_above`: Approval threshold (in yoctoNEAR). Transactions strictly less than or equal to this threshold may proceed directly if the recipient is `Allowed`. Transactions exceeding this require an owner-approved proposal. Setting this to 0 disables direct payments entirely.
- `daily_spending_limit`: Maximum cumulative spending allocation permitted within a single UTC calendar day.

---

## 3. Payment Lifecycles & Execution Paths

PayShield supports two distinct execution paths depending on transaction size and counterparty status:

### Path A: Human-in-the-Loop Proposal Workflow
1. **Proposal Creation (`propose_payment`):** Agent submits recipient, transfer amount, and memo reason. Recipient must not be `Blocked`. Allocates `proposal_id` in `Pending` status.
2. **Owner Review:**
   - **Approval (`approve_proposal`):** Owner transitions proposal from `Pending` to `Approved`. If the proposal has expired (TTL: 24 hours), it is persistently marked `Expired`.
   - **Rejection (`reject_proposal`):** Owner rejects a `Pending` proposal or revokes an `Approved` proposal prior to execution.
3. **Preflight Validation (`preflight_proposal_execution`):** An owner-or-agent authorized contract call (classified as `call`, not `view`, in the ABI). Validates whether an approved proposal can execute by verifying recipient status, policy limits, prospective daily spending quota, payment ID allocation safety, and liquid balance solvency. If an approved proposal has passed its 24-hour TTL, this call mutates contract state by updating the proposal's status to `Expired`.
4. **Execution Dispatch (`execute_proposal`):** Callable by owner or agent. Transitions proposal to `Executing`, allocates a monotonic `payment_id`, creates an `InFlight` payment record, locks a daily spending reservation, flushes storage maps, and dispatches a native transfer promise chained to the private settlement callback.

### Path B: Agent Direct Payment Workflow (`pay_direct`)
- Executed autonomously by the agent for low-risk routine expenses.
- Preconditions:
  - Caller must be `agent_id`.
  - Recipient must have `RecipientStatus::Allowed`.
  - `amount > 0`, `amount <= max_amount_per_tx`, and `amount <= require_approval_above`.
  - Prospective daily spending (`spent + reserved + amount`) must not exceed `daily_spending_limit`.
  - Contract liquid balance must satisfy the transfer amount, storage staking costs, and safety buffer.
- Execution: Creates an `InFlight` payment record (with `proposal_id: None`), reserves daily quota, flushes storage maps, and dispatches the native transfer promise chained to `on_payment_completed`.

---

## 4. Authorization Matrix

| Method | Role Authorization | Mutability | Invariants & Restrictions |
|---|---|---|---|
| `init` | Public (once) | State Init | Single-use only (`assert!(!env::state_exists())`). |
| `set_recipient_status` | Owner only | Mutation | Sets recipient to `Allowed`, `PendingApproval`, or `Blocked`. |
| `propose_payment` | Agent only | Mutation | Creates pending proposal. Rejects `Blocked` recipients. |
| `approve_proposal` | Owner only | Mutation | Approves proposal. Sets `Expired` if 24-hour TTL elapsed. |
| `reject_proposal` | Owner only | Mutation | Rejects pending or revokes approved proposal. |
| `preflight_proposal_execution` | Owner or Agent | Mutation | Validates limits and safety before execution. |
| `execute_proposal` | Owner or Agent | Mutation | Dispatches transfer promise + callback. Locks quota reservation. |
| `pay_direct` | Agent only | Mutation | Direct transfer below threshold to `Allowed` recipient. |
| `on_payment_completed` | **Contract only** (`#[private]`) | Callback | Enforces `current_account == predecessor_account`. Settles payment. |
| `reconcile_stuck_payment` | Owner only | Mutation | Fallback settlement after 5-minute timeout with evidence reference. |
| `flush_storage_maps` | Public utility | Mutation | Flushes in-memory collection cache pages to storage trie. |
| View Methods (13 contract + 1 SDK) | Public | Read-only | 13 contract-defined view methods (query status, policy, day index, daily spending, proposals, payments, and balance safety) plus 1 SDK-provided `contract_source_metadata` NEP-0330 view method. |

---

## 5. Daily Spending Quota Accounting

- **UTC Accounting Days:** The daily spending window resets strictly on UTC midnight boundaries, calculated as `block_timestamp_ns / (86_400 * 1_000_000_000)`.
- **Two-Phase Allocation Accounting:**
  - `spent`: Confirmed spending settled from successful transfer callbacks or confirmed reconciliations.
  - `reserved`: Active spending reservations locked by in-flight payments.
  - **Prospective Limit Check:** Before dispatching any payment:
    $$\text{spent} + \text{reserved} + \text{transfer\_amount} \le \text{daily\_spending\_limit}$$
- **Settlement Transitions:**
  - Upon transfer success: `reserved` is decremented and `spent` is incremented by the transfer amount.
  - Upon transfer failure: `reserved` is decremented and returned to available daily quota.
- **Original Accounting Day Preserved:** If a payment is dispatched at 23:59 UTC on Day $N$ and settles at 00:01 UTC on Day $N+1$, settlement updates Day $N$'s reservation record, preventing cross-day quota contamination.

---

## 6. Settlement Invariants & Reconciliation

### Asynchronous Callback Settlement
- All transfers dispatch native NEAR transfer promises (`Promise::new(recipient).transfer(amount)`) with 25 TGas allocated to `on_payment_completed`.
- **First Settlement Wins:** Payment records transition to terminal status (`Succeeded` or `Failed`). Once terminal, records are immutable; subsequent callbacks or duplicate settlement attempts are rejected.
- **No Automatic Retries:** Transfers are **never automatically retried** by the contract. If a transfer fails (e.g., target account does not exist or storage limits are exceeded), the payment is marked `Failed`, the reserved daily spending is released, and a new transaction must be initiated.

### Owner Emergency Reconciliation (`reconcile_stuck_payment`)
If an asynchronous receipt or cross-contract callback is lost or delayed due to network conditions:
- **Eligibility:**
  - Caller must be `owner_id`.
  - Payment must currently be in `InFlight` status.
  - Proposal (if applicable) must be in `Executing` status.
  - At least **5 minutes** (`300_000_000_000` ns) must have elapsed since payment initiation.
- **Independent Verification:** The owner must inspect off-chain explorer or RPC transaction receipts to verify whether the recipient account received funds.
- **Audit Evidence:** The owner must provide an explicit `evidence_ref` string (1 to 256 bytes UTF-8, non-empty, non-whitespace) citing the off-chain receipt or proof.
- **Outcome:** The owner explicitly specifies `transfer_succeeded` (`true` confirms reservation; `false` releases reservation). Marks settlement source as `SettlementSource::OwnerReconciliation`.

---

## 7. Contract ABI Location

The official contract ABI is generated using `cargo-near` and is located at:
```text
contract/target/near/contract_abi.json
```

To regenerate the ABI artifact safely without modifying dependencies:
```bash
cargo near abi --locked
```

---

## 8. Local Verification & Build Commands

> [!NOTE]
> The following commands execute locally in an offline development environment. They do **not** deploy contracts or broadcast transactions to NEAR networks.

### Run Unit Tests
The test suite consists of 160 unit tests covering lifecycle states, accounting invariants, balance safety checks, and edge cases:
```bash
cargo test --lib
```

### Check Rust Formatting
```bash
rustfmt --check src/lib.rs
```

### Build Release WASM Artifact
Compiles the optimized release binary targeting the NEAR runtime:
```bash
cargo build --target wasm32-unknown-unknown --release
```
The resulting binary is generated at:
```text
contract/target/wasm32-unknown-unknown/release/contract.wasm
```

---

## 9. Security Limitations & Current Implementation Boundaries

To ensure strict engineering integrity, note the following boundaries of this smart contract:

1. **No External Reputation Service or Dynamic Scoring:** The contract does not connect to oracles, external blocklists, or automated threat scoring feeds. Counterparty statuses (`Allowed`, `PendingApproval`, `Blocked`) are maintained entirely through manual owner transactions.
2. **No Agent-Side Signing Wrapper:** This crate contains only the on-chain smart contract. It does not provide client-side interceptors that stop an agent from signing or broadcasting transactions targeting blocked addresses.
3. **No Prompt-Injection Defense:** Prompt-injection mitigation must be implemented at the AI agent/runtime layer. The contract acts strictly as an immutable on-chain policy enforcement boundary.
4. **No Pre-Provisioned Access Keys:** Function-call access keys for the agent have not been provisioned. In production, the agent must be restricted to function-call keys limited to `propose_payment` and `pay_direct`.
5. **Network Deployment:** The contract is deployed on NEAR Testnet at `payshield-agent-demo.testnet`. The frontend uses read-only RPC calls to this contract. No Mainnet deployment is claimed. The contract's deployment and initialized state have been checked through read-only Testnet RPC calls.
6. **Unit Tests Are Not Live Proof:** 160 passing unit tests demonstrate internal algorithmic correctness under mocked runtime environments (`testing_env!`), but do not substitute for live testnet verification or integration audits.
