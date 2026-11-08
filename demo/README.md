# PayShield NEAR Dashboard

The PayShield dashboard is a React, TypeScript, and Vite frontend for monitoring the PayShield smart contract on **NEAR Testnet**.

It reads contract policy and account information from the deployed contract and presents payment activity, proposal status, daily spending, and vault balance in one interface.

**Live demo:** https://payshield-near.vercel.app/  
**Contract account:** `payshield-agent-demo.testnet`

## Dashboard features

- **Overview:** Vault balance, daily spending limit, maximum payment amount, and pending proposal count.
- **Contract connection:** Displays the contract account, owner, authorized agent, and contract data-read status.
- **Approval threshold:** Shows the on-chain amount above which a payment requires owner approval.
- **On-chain usage:** Shows the current UTC accounting day, confirmed and reserved spending, and next proposal/payment IDs.
- **Payments:** Lists payment records, recipients, amounts, lifecycle status, accounting day, and settlement timestamps.
- **Approvals:** Lists payment proposals, recipients, reasons, amounts, creation times, and current status.
- **Wallet connection:** Allows a NEAR Testnet wallet to be connected or disconnected for account display.
- **Theme:** Supports dark and light display modes.

## Important interaction and safety boundary

The current dashboard is a **read-only monitoring interface** for contract data.

- Contract data is retrieved through NEAR Testnet JSON-RPC view calls.
- Connecting a wallet displays the connected account; it does not submit a contract transaction.
- The dashboard does not currently provide buttons to create, approve, reject, or execute payment proposals.
- It does not initiate payments, sign transactions, or broadcast transactions.
- Contract state changes must be performed separately through an authorized NEAR transaction interface by the appropriate owner or agent account.

The smart contract remains the enforcement boundary for recipient restrictions, payment limits, daily spending reservations, approvals, and settlement.

## Technology

- React 19
- TypeScript
- Vite 8
- `@hot-labs/near-connect` for NEAR Testnet wallet connection
- NEAR Testnet JSON-RPC for read-only contract queries

## Requirements

- Node.js 22.12 or newer
- npm

## Run locally

From the repository root:

```bash
cd demo
npm install
npm run dev
```

Vite prints a local development URL, usually:

```text
http://localhost:5173/
```

The dashboard reads from the deployed Testnet contract. A wallet connection is not required to load the read-only dashboard data.

## Validate the frontend

Run the production TypeScript and Vite build:

```bash
npm run build
```

Run Oxlint:

```bash
npm run lint
```

Preview a production build locally:

```bash
npm run preview
```

## NEAR Testnet configuration

The contract account is configured in `src/lib/nearRpc.ts`:

```ts
export const CONTRACT_ID = 'payshield-agent-demo.testnet'
```

The frontend sends JSON-RPC requests to the local `/near-rpc` path. In Vercel deployments, `vercel.json` rewrites that path to the official NEAR Testnet RPC endpoint:

```text
/near-rpc  ->  https://rpc.testnet.near.org/
```

This rewrite keeps the frontend's RPC path consistent between local application code and the deployed Vercel environment. The dashboard uses finality-final view queries and does not require a private key or RPC secret.

## Suggested demo walkthrough

1. Open the [live PayShield dashboard](https://payshield-near.vercel.app/).
2. Wait for the contract data to load.
3. Point out the Testnet contract account, owner, and authorized agent.
4. Explain the approval threshold and the maximum-per-payment and daily spending limits.
5. Show the current UTC accounting day and the confirmed/reserved spending figures.
6. Open **Payments** to explain how successful, failed, and in-flight payment records are displayed.
7. Open **Approvals** to explain the human-review queue and proposal lifecycle statuses.
8. Explain that this deployed frontend currently monitors state; it does not submit payment or approval transactions.

If the proposal or payment lists are empty, that is a valid empty state when no corresponding records have been created on the deployed contract.

## Related documentation

- [Root project overview and security model](../README.md)
- [Smart contract technical documentation](../contract/README.md)
- [Contract implementation](../contract/src/lib.rs)
