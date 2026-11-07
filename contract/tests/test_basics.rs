use std::sync::Arc;

use contract::{
    calculate_day_index, compute_storage_shortfall, DailySpendingUsage, ExecutionOutcome,
    ExecutionPreflightOutcome, PayShield, PaymentPolicy, PaymentProposal, PaymentRecord,
    PaymentStatus, ProposalStatus, RecipientStatus, SettlementSource, PAYMENT_SETTLEMENT_TIMEOUT,
    STORAGE_SAFETY_BUFFER,
};
use near_api::{
    types::{
        transaction::result::{ExecutionSuccess, TransactionResult},
        AccountId, NearGas, NearToken,
    },
    Account, Contract, NetworkConfig, Signer,
};
#[allow(deprecated)]
use near_sdk::test_utils::testing_env_with_promise_results;
use near_sdk::{
    serde::{de::DeserializeOwned, Serialize},
    serde_json::{self, json},
    test_utils::{accounts, VMContextBuilder},
    testing_env, PromiseResult, Timestamp,
};
use testresult::TestResult;

const EXPECTED_WASM_HASH: [u8; 32] = [
    0x1b, 0x9e, 0xdd, 0x1b, 0x42, 0x10, 0x18, 0x9c, 0x5e, 0x95, 0x34, 0x6d, 0x01, 0xf9, 0xae, 0x3b,
    0xc4, 0x30, 0x64, 0xd0, 0x31, 0x2b, 0xdd, 0x5b, 0xc4, 0xe0, 0x93, 0x15, 0x98, 0x97, 0x08, 0xf4,
];

fn load_and_verify_wasm() -> TestResult<Vec<u8>> {
    let wasm_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-unknown-unknown/release/contract.wasm");
    assert!(wasm_path.exists(), "WASM file not found at {:?}", wasm_path);
    let wasm_bytes = std::fs::read(&wasm_path)?;
    let computed_hash = near_api::types::CryptoHash::hash(&wasm_bytes);
    assert_eq!(
        computed_hash.0, EXPECTED_WASM_HASH,
        "WASM hash mismatch: expected 1b9edd1b4210189c5e95346d01f9ae3bc43064d0312bdd5bc4e09315989708f4, got {}",
        computed_hash
    );
    Ok(wasm_bytes)
}

struct TestHarness {
    _sandbox: near_sandbox::Sandbox,
    network: NetworkConfig,
    signer: Arc<Signer>,
    contract_id: AccountId,
    owner_id: AccountId,
    agent_id: AccountId,
    merchant_id: AccountId,
    unauthorized_id: AccountId,
}

impl TestHarness {
    async fn setup_with_policy(policy: serde_json::Value) -> TestResult<Option<Self>> {
        let sandbox = match near_sandbox::Sandbox::start_sandbox().await {
            Ok(sb) => sb,
            Err(e) => {
                if std::env::var("REQUIRE_SANDBOX").as_deref() == Ok("true") {
                    panic!(
                        "FATAL (REQUIRE_SANDBOX=true): near_sandbox failed to start: {:?}. \
                         In CI environments, sandbox availability is strictly mandatory.",
                        e
                    );
                }

                eprintln!(
                    "SKIPPING SANDBOX TEST: near_sandbox failed to start ({:?}). \
                     Host CPU lacks BMI2/AVX2 support required by precompiled near-sandbox binary (SIGILL).",
                    e
                );

                return Ok(None);
            }
        };

        let network = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
        let signer = Signer::from_secret_key(
            near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?,
        )?;

        let contract_id: AccountId = "contract.sandbox".parse()?;
        let owner_id: AccountId = "owner.sandbox".parse()?;
        let agent_id: AccountId = "agent.sandbox".parse()?;
        let merchant_id: AccountId = "merchant.sandbox".parse()?;
        let unauthorized_id: AccountId = "unauthorized.sandbox".parse()?;

        let initial_balance = NearToken::from_near(100);
        let accounts_to_create = [
            contract_id.clone(),
            owner_id.clone(),
            agent_id.clone(),
            merchant_id.clone(),
            unauthorized_id.clone(),
        ];
        for acct in accounts_to_create {
            sandbox
                .create_account(acct)
                .initial_balance(initial_balance)
                .send()
                .await?;
        }

        let wasm_bytes = load_and_verify_wasm()?;
        let init_args = json!({
            "owner_id": owner_id,
            "agent_id": agent_id,
            "initial_policy": policy,
        });

        Contract::deploy(contract_id.clone())
            .use_code(wasm_bytes)
            .with_init_call("init", init_args)?
            .with_signer(signer.clone())
            .send_to(&network)
            .await?
            .assert_success();

        Ok(Some(Self {
            _sandbox: sandbox,
            network,
            signer,
            contract_id,
            owner_id,
            agent_id,
            merchant_id,
            unauthorized_id,
        }))
    }

    async fn setup_default() -> TestResult<Option<Self>> {
        let default_policy = json!({
            "max_amount_per_tx": NearToken::from_near(10).as_yoctonear().to_string(),
            "daily_spending_limit": NearToken::from_near(5).as_yoctonear().to_string(),
            "require_approval_above": NearToken::from_near(1).as_yoctonear().to_string(),
        });
        Self::setup_with_policy(default_policy).await
    }

    pub async fn view<Args: Serialize, Res: Send + Sync + DeserializeOwned>(
        &self,
        method: &str,
        args: Args,
    ) -> TestResult<Res> {
        let res: near_api::types::Data<Res> = Contract(self.contract_id.clone())
            .call_function(method, args)
            .read_only()
            .fetch_from(&self.network)
            .await?;
        Ok(res.data)
    }

    pub async fn call_tx<Args: Serialize>(
        &self,
        signer_id: AccountId,
        method: &str,
        args: Args,
    ) -> TestResult<ExecutionSuccess> {
        let res: TransactionResult = Contract(self.contract_id.clone())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(100))
            .with_signer(signer_id, self.signer.clone())
            .send_to(&self.network)
            .await?;
        Ok(res.assert_success())
    }

    pub async fn assert_tx_failure<Args: Serialize>(
        &self,
        signer_id: AccountId,
        method: &str,
        args: Args,
    ) -> TestResult<()> {
        let b = Contract(self.contract_id.clone())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(100))
            .with_signer(signer_id, self.signer.clone());
        match b.send_to(&self.network).await {
            Ok(tr) => {
                tr.assert_failure();
            }
            Err(_) => {}
        }
        Ok(())
    }

    pub async fn assert_tx_failure_contains<Args: Serialize>(
        &self,
        signer_id: AccountId,
        method: &str,
        args: Args,
        expected_msg: &str,
    ) -> TestResult<()> {
        let b = Contract(self.contract_id.clone())
            .call_function(method, args)
            .transaction()
            .gas(NearGas::from_tgas(100))
            .with_signer(signer_id, self.signer.clone());
        match b.send_to(&self.network).await {
            Ok(tr) => {
                let failure = tr.assert_failure();
                let debug_str = format!("{:?}", failure);
                assert!(
                    debug_str.contains(expected_msg)
                        || debug_str.contains("WasmTrap(Unreachable)")
                        || debug_str
                            .contains("WebAssembly trap: An `unreachable` opcode was executed."),
                    "Expected failure containing '{}' or unreachable WasmTrap, got: {}",
                    expected_msg,
                    debug_str
                );
            }
            Err(e) => {
                panic!("Transaction submission or RPC failure: {:?}", e);
            }
        }
        Ok(())
    }
}

// =============================================================================
// SANDBOX INTEGRATION TESTS
// =============================================================================

#[tokio::test]
async fn test_ps_int_01_deployment_and_initialization() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    let owner: AccountId = harness.view("get_owner", ()).await?;
    assert_eq!(owner, harness.owner_id);

    let agent: AccountId = harness.view("get_agent", ()).await?;
    assert_eq!(agent, harness.agent_id);

    let policy: PaymentPolicy = harness.view("get_policy", ()).await?;
    assert_eq!(policy.max_amount_per_tx, NearToken::from_near(10));
    assert_eq!(policy.daily_spending_limit, NearToken::from_near(5));
    assert_eq!(policy.require_approval_above, NearToken::from_near(1));

    let next_prop_id: u64 = harness.view("get_next_proposal_id", ()).await?;
    assert_eq!(next_prop_id, 1);

    let next_pay_id: u64 = harness.view("get_next_payment_id", ()).await?;
    assert_eq!(next_pay_id, 1);

    Ok(())
}

#[tokio::test]
async fn test_ps_int_02_recipient_registry() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    let initial_status: RecipientStatus = harness
        .view(
            "get_recipient_status",
            json!({ "account_id": harness.merchant_id }),
        )
        .await?;
    assert_eq!(initial_status, RecipientStatus::Unknown);

    let is_allowed_initial: bool = harness
        .view(
            "is_recipient_allowed",
            json!({ "account_id": harness.merchant_id }),
        )
        .await?;
    assert!(!is_allowed_initial);

    // Owner marks recipient as Allowed
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    let allowed_status: RecipientStatus = harness
        .view(
            "get_recipient_status",
            json!({ "account_id": harness.merchant_id }),
        )
        .await?;
    assert_eq!(allowed_status, RecipientStatus::Allowed);

    let is_allowed_after: bool = harness
        .view(
            "is_recipient_allowed",
            json!({ "account_id": harness.merchant_id }),
        )
        .await?;
    assert!(is_allowed_after);

    // Owner marks recipient as Blocked
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Blocked" }),
        )
        .await?;

    let blocked_status: RecipientStatus = harness
        .view(
            "get_recipient_status",
            json!({ "account_id": harness.merchant_id }),
        )
        .await?;
    assert_eq!(blocked_status, RecipientStatus::Blocked);

    let is_allowed_blocked: bool = harness
        .view(
            "is_recipient_allowed",
            json!({ "account_id": harness.merchant_id }),
        )
        .await?;
    assert!(!is_allowed_blocked);

    Ok(())
}

#[tokio::test]
async fn test_ps_int_03_direct_payment() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // Allow merchant
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // Agent executes direct payment of 0.5 NEAR (below 1 NEAR approval threshold)
    let payment_id: u64 = harness
        .call_tx(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_millinear(500).as_yoctonear().to_string(),
            }),
        )
        .await?
        .json()?;
    assert_eq!(payment_id, 1);

    // Verify payment record 1 is Succeeded with SettlementSource::Callback
    let payment: Option<PaymentRecord> = harness
        .view("get_payment", json!({ "payment_id": 1 }))
        .await?;
    let payment = payment.expect("Payment record 1 must exist");
    assert_eq!(payment.payment_id, 1);
    assert_eq!(payment.recipient, harness.merchant_id);
    assert_eq!(payment.amount, NearToken::from_millinear(500));
    assert_eq!(payment.status, PaymentStatus::Succeeded);
    assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));
    assert_eq!(payment.proposal_id, None);

    let next_pay_id: u64 = harness.view("get_next_payment_id", ()).await?;
    assert_eq!(next_pay_id, 2);

    let next_prop_id: u64 = harness.view("get_next_proposal_id", ()).await?;
    assert_eq!(next_prop_id, 1);

    Ok(())
}

#[tokio::test]
async fn test_ps_int_04_direct_payment_restrictions() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // 1. Direct payment to Unknown recipient rejected
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_millinear(500).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // 2. Direct payment to Blocked recipient rejected
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Blocked" }),
        )
        .await?;
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_millinear(500).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // Allow merchant for remaining policy checks
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 3. Direct payment with zero amount rejected
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_yoctonear(0).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // 4. Direct payment exceeding approval threshold (1.5 NEAR > 1 NEAR) rejected
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_millinear(1500).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // 5. Zero approval threshold disables direct payment entirely
    let zero_thresh_policy = json!({
        "max_amount_per_tx": NearToken::from_near(10).as_yoctonear().to_string(),
        "daily_spending_limit": NearToken::from_near(5).as_yoctonear().to_string(),
        "require_approval_above": NearToken::from_yoctonear(0).as_yoctonear().to_string(),
    });
    let harness_zero = match TestHarness::setup_with_policy(zero_thresh_policy).await? {
        Some(h) => h,
        None => return Ok(()),
    };
    harness_zero
        .call_tx(
            harness_zero.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness_zero.merchant_id, "status": "Allowed" }),
        )
        .await?;
    harness_zero
        .assert_tx_failure(
            harness_zero.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness_zero.merchant_id,
                "amount": NearToken::from_yoctonear(1).as_yoctonear().to_string(),
            }),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_ps_int_05_proposal_creation() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // 1. Propose payment for Unknown recipient succeeds with Pending status
    let prop_id1: u64 = harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Payment to unknown recipient requiring review",
            }),
        )
        .await?
        .json()?;
    assert_eq!(prop_id1, 1);

    let prop1: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 1 }))
        .await?;
    assert_eq!(prop1.unwrap().status, ProposalStatus::Pending);

    // 2. Propose payment for Allowed recipient above threshold (2 NEAR > 1 NEAR)
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    let prop_id2: u64 = harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Payment to allowed recipient above threshold",
            }),
        )
        .await?
        .json()?;
    assert_eq!(prop_id2, 2);

    let prop2: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 2 }))
        .await?;
    assert_eq!(prop2.unwrap().status, ProposalStatus::Pending);

    // 3. Propose payment to Blocked recipient rejected
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Blocked" }),
        )
        .await?;

    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Attempting payment to blocked recipient",
            }),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_ps_int_06_proposal_lifecycle() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // Allow merchant
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 1. Propose payment of 2 NEAR
    let prop_id: u64 = harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Lifecycle proposal 2 NEAR",
            }),
        )
        .await?
        .json()?;
    assert_eq!(prop_id, 1);

    let prop_pending: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 1 }))
        .await?;
    assert_eq!(prop_pending.unwrap().status, ProposalStatus::Pending);

    // 2. Owner approves proposal
    let approved_status: ProposalStatus = harness
        .call_tx(
            harness.owner_id.clone(),
            "approve_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?
        .json()?;
    assert_eq!(approved_status, ProposalStatus::Approved);

    let prop_approved: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 1 }))
        .await?;
    assert_eq!(prop_approved.unwrap().status, ProposalStatus::Approved);

    // 3. Owner executes approved proposal
    let outcome: ExecutionOutcome = harness
        .call_tx(
            harness.owner_id.clone(),
            "execute_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?
        .json()?;
    assert_eq!(outcome, ExecutionOutcome::Executed { payment_id: 1 });

    // 4. Verify proposal is Executed
    let prop_exec: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 1 }))
        .await?;
    let prop_exec = prop_exec.expect("Proposal 1 must exist");
    assert_eq!(prop_exec.status, ProposalStatus::Executed);

    // 5. Verify payment record 1 is Succeeded with proposal_id: Some(1)
    let payment: Option<PaymentRecord> = harness
        .view("get_payment", json!({ "payment_id": 1 }))
        .await?;
    let payment = payment.expect("Payment record 1 must exist");
    assert_eq!(payment.payment_id, 1);
    assert_eq!(payment.proposal_id, Some(1));
    assert_eq!(payment.status, PaymentStatus::Succeeded);
    assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));

    Ok(())
}

#[tokio::test]
async fn test_ps_int_07_preflight_and_expiry() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 1. Propose and approve proposal 1
    harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Ready preflight validation test",
            }),
        )
        .await?;

    harness
        .call_tx(
            harness.owner_id.clone(),
            "approve_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    // Preflight on approved valid proposal succeeds and returns Ready
    let outcome: ExecutionPreflightOutcome = harness
        .call_tx(
            harness.agent_id.clone(),
            "preflight_proposal_execution",
            json!({ "proposal_id": 1 }),
        )
        .await?
        .json()?;
    match outcome {
        ExecutionPreflightOutcome::Ready {
            proposal_id,
            recipient,
            amount,
            ..
        } => {
            assert_eq!(proposal_id, 1);
            assert_eq!(recipient, harness.merchant_id);
            assert_eq!(amount, NearToken::from_near(2));
        }
        _ => panic!("Expected ExecutionPreflightOutcome::Ready"),
    }

    let proposal: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 1 }))
        .await?;
    assert_eq!(proposal.unwrap().status, ProposalStatus::Approved);

    // 2. Preflight on a pending (non-approved) proposal fails
    harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Pending proposal preflight test",
            }),
        )
        .await?;

    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "preflight_proposal_execution",
            json!({ "proposal_id": 2 }),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_ps_int_08_proposal_rejection_and_revocation() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 1. Reject Pending proposal
    harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Pending rejection test",
            }),
        )
        .await?;

    harness
        .call_tx(
            harness.owner_id.clone(),
            "reject_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    let prop1: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 1 }))
        .await?;
    assert_eq!(prop1.unwrap().status, ProposalStatus::Rejected);

    // 2. Revoke Approved proposal
    harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Revocation of approved proposal test",
            }),
        )
        .await?;

    harness
        .call_tx(
            harness.owner_id.clone(),
            "approve_proposal",
            json!({ "proposal_id": 2 }),
        )
        .await?;

    harness
        .call_tx(
            harness.owner_id.clone(),
            "reject_proposal",
            json!({ "proposal_id": 2 }),
        )
        .await?;

    let prop2: Option<PaymentProposal> = harness
        .view("get_proposal", json!({ "proposal_id": 2 }))
        .await?;
    assert_eq!(prop2.unwrap().status, ProposalStatus::Rejected);

    // 3. Execution of rejected proposal is denied
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "execute_proposal",
            json!({ "proposal_id": 2 }),
        )
        .await?;

    // 4. Double rejection is rejected
    harness
        .assert_tx_failure(
            harness.owner_id.clone(),
            "reject_proposal",
            json!({ "proposal_id": 2 }),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_ps_int_09_daily_spending_and_reservations() -> TestResult<()> {
    // Isolated setup: daily_spending_limit: 3 NEAR, approval_threshold: 1 NEAR
    let policy = json!({
        "max_amount_per_tx": NearToken::from_near(5).as_yoctonear().to_string(),
        "daily_spending_limit": NearToken::from_near(3).as_yoctonear().to_string(),
        "require_approval_above": NearToken::from_near(1).as_yoctonear().to_string(),
    });
    let harness = match TestHarness::setup_with_policy(policy).await? {
        Some(h) => h,
        None => return Ok(()),
    };

    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 1. Direct payment of 1 NEAR (confirmed)
    harness
        .call_tx(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(1).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // 2. Proposal payment of 2 NEAR (confirmed via execute)
    harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Quota test proposal",
            }),
        )
        .await?;

    harness
        .call_tx(
            harness.owner_id.clone(),
            "approve_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    harness
        .call_tx(
            harness.owner_id.clone(),
            "execute_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    // 3. Attempting another payment (0.1 NEAR direct payment) fails: daily limit (3 NEAR) reached
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_millinear(100).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // Verify daily spending tracking
    let current_day: u64 = harness.view("get_current_day_index", ()).await?;
    let daily_usage: DailySpendingUsage = harness
        .view("get_daily_spending", json!({ "day_id": current_day }))
        .await?;
    assert_eq!(daily_usage.confirmed_amount, NearToken::from_near(3));
    assert_eq!(daily_usage.reserved_amount, NearToken::from_yoctonear(0));

    Ok(())
}

#[tokio::test]
async fn test_ps_int_10_s_sandbox_solvency() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // 1. Query contract account balance via account view
    let account_info = Account(harness.contract_id.clone())
        .view()
        .fetch_from(&harness.network)
        .await?;
    let contract_balance = account_info.data.amount;

    // 2. Query required liquid balance for a safe amount and verify solvency against contract balance
    let safe_amount = NearToken::from_near(1);
    let required_liquid: NearToken = harness
        .view(
            "get_required_liquid_balance",
            json!({ "transfer_amount": safe_amount.as_yoctonear().to_string() }),
        )
        .await?;
    assert!(
        contract_balance >= required_liquid,
        "Contract balance should safely cover transfer, storage backing, and safety buffer"
    );

    // 3. Calling check_liquid_balance_safety with transfer amount equal to current contract balance
    // must fail because required liquid balance includes storage shortfall + 0.02 NEAR buffer
    let fail_res: Result<near_api::types::Data<()>, _> = Contract(harness.contract_id.clone())
        .call_function(
            "check_liquid_balance_safety",
            json!({ "transfer_amount": contract_balance.as_yoctonear().to_string() }),
        )
        .read_only()
        .fetch_from(&harness.network)
        .await;
    assert!(
        fail_res.is_err(),
        "Expected check_liquid_balance_safety to fail when transfer_amount equals total balance"
    );
    let err_str = format!("{:?}", fail_res.unwrap_err());
    assert!(
        err_str.contains("WasmTrap(Unreachable)")
            || err_str.contains("WebAssembly trap: An `unreachable` opcode was executed.")
            || err_str.contains(
                "Insufficient available balance for transfer, storage backing, and safety buffer",
            ),
        "Expected error containing WasmTrap(Unreachable), got: {}",
        err_str
    );

    Ok(())
}

#[tokio::test]
async fn test_ps_int_11_s_reconciliation() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 1. Non-owner caller cannot reconcile
    harness
        .assert_tx_failure_contains(
            harness.agent_id.clone(),
            "reconcile_stuck_payment",
            json!({
                "payment_id": 1,
                "transfer_succeeded": true,
                "evidence_ref": "https://explorer.near.org/tx/1",
            }),
            "Unauthorized: caller is not the contract owner",
        )
        .await?;

    // 2. Empty evidence reference is rejected
    harness
        .assert_tx_failure_contains(
            harness.owner_id.clone(),
            "reconcile_stuck_payment",
            json!({
                "payment_id": 1,
                "transfer_succeeded": true,
                "evidence_ref": "",
            }),
            "Reconciliation evidence reference cannot be empty or whitespace",
        )
        .await?;

    // 3. Whitespace-only evidence reference is rejected
    harness
        .assert_tx_failure_contains(
            harness.owner_id.clone(),
            "reconcile_stuck_payment",
            json!({
                "payment_id": 1,
                "transfer_succeeded": true,
                "evidence_ref": "   ",
            }),
            "Reconciliation evidence reference cannot be empty or whitespace",
        )
        .await?;

    // 4. Missing payment ID is rejected
    harness
        .assert_tx_failure_contains(
            harness.owner_id.clone(),
            "reconcile_stuck_payment",
            json!({
                "payment_id": 999,
                "transfer_succeeded": true,
                "evidence_ref": "https://explorer.near.org/tx/valid",
            }),
            "Payment not found",
        )
        .await?;

    // 5. Create and settle a payment via pay_direct
    let payment_id: u64 = harness
        .call_tx(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_millinear(500).as_yoctonear().to_string(),
            }),
        )
        .await?
        .json()?;
    assert_eq!(payment_id, 1);

    let settled_opt: Option<PaymentRecord> = harness
        .view("get_payment", json!({ "payment_id": payment_id }))
        .await?;
    let settled_payment = settled_opt.expect("Payment record must exist after pay_direct");
    assert_eq!(settled_payment.status, PaymentStatus::Succeeded);

    // Attempting to reconcile an already settled payment (status Succeeded) fails
    harness
        .assert_tx_failure_contains(
            harness.owner_id.clone(),
            "reconcile_stuck_payment",
            json!({
                "payment_id": 1,
                "transfer_succeeded": true,
                "evidence_ref": "https://explorer.near.org/tx/reconcile",
            }),
            "Payment is not in-flight",
        )
        .await?;

    let post_opt: Option<PaymentRecord> = harness
        .view("get_payment", json!({ "payment_id": payment_id }))
        .await?;
    let post_reconciliation_payment =
        post_opt.expect("Payment record must still exist after rejected reconciliation");
    assert_eq!(
        post_reconciliation_payment, settled_payment,
        "Settled payment state must remain unmodified after rejected reconciliation attempt"
    );

    Ok(())
}

#[tokio::test]
async fn test_ps_int_12_s_private_callback_access() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // Calling private callback from external account (agent or unauthorized) must fail
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "on_payment_completed",
            json!({ "payment_id": 1 }),
        )
        .await?;

    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "on_payment_completed",
            json!({ "payment_id": 1 }),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_ps_int_13_unauthorized_access() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    // 1. Agent cannot set recipient status
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 2. Unauthorized cannot set recipient status
    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 3. Unauthorized cannot propose payment
    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(1).as_yoctonear().to_string(),
                "reason": "Unauthorized attempt",
            }),
        )
        .await?;

    // Set recipient allowed and create proposal 1 as agent
    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    harness
        .call_tx(
            harness.agent_id.clone(),
            "propose_payment",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(2).as_yoctonear().to_string(),
                "reason": "Authorization test proposal",
            }),
        )
        .await?;

    // 4. Unauthorized cannot approve proposal
    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "approve_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    // 5. Unauthorized cannot reject proposal
    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "reject_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    // Approve proposal 1 as owner
    harness
        .call_tx(
            harness.owner_id.clone(),
            "approve_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    // 6. Unauthorized cannot execute proposal
    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "execute_proposal",
            json!({ "proposal_id": 1 }),
        )
        .await?;

    // 7. Unauthorized cannot reconcile payment
    harness
        .assert_tx_failure(
            harness.unauthorized_id.clone(),
            "reconcile_stuck_payment",
            json!({
                "payment_id": 1,
                "transfer_succeeded": true,
                "evidence_ref": "evidence_proof_uri",
            }),
        )
        .await?;

    Ok(())
}

#[tokio::test]
async fn test_ps_int_14_amount_boundaries() -> TestResult<()> {
    let harness = match TestHarness::setup_default().await? {
        Some(h) => h,
        None => return Ok(()),
    };

    harness
        .call_tx(
            harness.owner_id.clone(),
            "set_recipient_status",
            json!({ "account_id": harness.merchant_id, "status": "Allowed" }),
        )
        .await?;

    // 1. Amount 0 direct payment fails
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_yoctonear(0).as_yoctonear().to_string(),
            }),
        )
        .await?;

    // 2. Exact threshold payment (1 NEAR == require_approval_above) succeeds
    let pay_id: u64 = harness
        .call_tx(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": NearToken::from_near(1).as_yoctonear().to_string(),
            }),
        )
        .await?
        .json()?;
    assert_eq!(pay_id, 1);

    // 3. Exactly 1 yocto above threshold (1 NEAR + 1 yocto) fails direct payment
    let thresh_plus_one = NearToken::from_yoctonear(NearToken::from_near(1).as_yoctonear() + 1);
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": thresh_plus_one.as_yoctonear().to_string(),
            }),
        )
        .await?;

    // 4. Over max_amount_per_tx (11 NEAR > 10 NEAR) fails direct payment
    let over_max = NearToken::from_near(11);
    harness
        .assert_tx_failure(
            harness.agent_id.clone(),
            "pay_direct",
            json!({
                "recipient": harness.merchant_id,
                "amount": over_max.as_yoctonear().to_string(),
            }),
        )
        .await?;

    let next_pay_id: u64 = harness.view("get_next_payment_id", ()).await?;
    assert_eq!(next_pay_id, 2);

    Ok(())
}

// =============================================================================
// DETERMINISTIC UNIT TESTS
// =============================================================================

fn get_unit_context(predecessor: AccountId) -> VMContextBuilder {
    let mut builder = VMContextBuilder::new();
    builder
        .current_account_id(accounts(0))
        .signer_account_id(predecessor.clone())
        .predecessor_account_id(predecessor)
        .account_balance(NearToken::from_near(10))
        .account_locked_balance(NearToken::from_millinear(400))
        .storage_usage(307_200)
        .block_timestamp(1_000_000_000_000);
    builder
}

#[allow(deprecated)]
fn set_promise_result(context: near_sdk::VMContext, result: PromiseResult) {
    testing_env_with_promise_results(context, result);
}

#[test]
fn test_ps_int_10_u_storage_solvency_boundaries() {
    let owner = accounts(1);
    let agent = accounts(2);

    let initial_balance = NearToken::from_near(10);
    let locked_balance = NearToken::from_millinear(400); // 0.4 NEAR

    let mut init_context = get_unit_context(owner.clone());
    init_context.account_balance(initial_balance);
    init_context.account_locked_balance(locked_balance);
    testing_env!(init_context.build());

    // Initialize contract (writes state and updates trie storage_usage)
    let contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

    let post_init_storage_usage = near_sdk::env::storage_usage();
    let byte_cost = near_sdk::env::storage_byte_cost();
    assert_eq!(
        byte_cost,
        NearToken::from_yoctonear(10_000_000_000_000_000_000)
    );

    let storage_cost =
        NearToken::from_yoctonear((post_init_storage_usage as u128) * byte_cost.as_yoctonear());

    let base_reserve = contract.get_required_liquid_balance(NearToken::from_yoctonear(0));
    let expected_shortfall = compute_storage_shortfall(storage_cost, locked_balance);
    assert_eq!(
        base_reserve,
        expected_shortfall
            .checked_add(STORAGE_SAFETY_BUFFER)
            .unwrap()
    );

    assert!(initial_balance >= base_reserve);
    let max_spendable =
        NearToken::from_yoctonear(initial_balance.as_yoctonear() - base_reserve.as_yoctonear());

    // Case A: Exact spendable boundary succeeds
    contract.check_liquid_balance_safety(max_spendable);
    assert_eq!(
        contract.get_required_liquid_balance(max_spendable),
        initial_balance
    );

    // Case B: Exactly 1 yoctoNEAR above boundary panics
    let over_by_one = NearToken::from_yoctonear(max_spendable.as_yoctonear() + 1);
    let result_over = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        contract.check_liquid_balance_safety(over_by_one);
    }));
    assert!(result_over.is_err(), "Must panic on max_spendable + 1");

    // Case C: Excess locked balance (locked_balance >= storage_cost)
    let mut excess_locked_context = get_unit_context(owner.clone());
    excess_locked_context.account_balance(initial_balance);
    excess_locked_context.storage_usage(post_init_storage_usage);
    let excess_locked = storage_cost.checked_add(NearToken::from_near(1)).unwrap();
    excess_locked_context.account_locked_balance(excess_locked);
    testing_env!(excess_locked_context.build());

    let reserve_with_excess_locked =
        contract.get_required_liquid_balance(NearToken::from_yoctonear(0));
    assert_eq!(
        reserve_with_excess_locked, STORAGE_SAFETY_BUFFER,
        "Base reserve must equal STORAGE_SAFETY_BUFFER when locked balance covers storage cost"
    );

    // Case D: Below-reserve (insolvent) balance
    let mut insolvent_context = get_unit_context(owner);
    insolvent_context.storage_usage(post_init_storage_usage);
    insolvent_context.account_locked_balance(locked_balance);
    let insolvent_balance = NearToken::from_yoctonear(
        base_reserve
            .as_yoctonear()
            .checked_sub(1)
            .expect("base_reserve > 0"),
    );
    insolvent_context.account_balance(insolvent_balance);
    testing_env!(insolvent_context.build());

    let safe_spendable = if insolvent_balance > base_reserve {
        insolvent_balance.saturating_sub(base_reserve)
    } else {
        NearToken::from_yoctonear(0)
    };
    assert_eq!(safe_spendable, NearToken::from_yoctonear(0));

    let result_insolvent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        contract.check_liquid_balance_safety(NearToken::from_yoctonear(1));
    }));
    assert!(
        result_insolvent.is_err(),
        "Must panic when contract balance is below base reserve"
    );
}

#[test]
fn test_ps_int_11_u_reconciliation_timeout() {
    let owner = accounts(1);
    let agent = accounts(2);
    let recipient = accounts(3);
    let amount = NearToken::from_near(1);
    let timestamp: Timestamp = 1_000_000_000_000;

    let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());
    let payment_id = 1;
    contract.next_payment_id = 2;
    let accounting_day = calculate_day_index(timestamp);

    let payment = PaymentRecord {
        payment_id,
        recipient: recipient.clone(),
        amount,
        accounting_day,
        proposal_id: None,
        initiated_at: timestamp,
        settled_at: None,
        status: PaymentStatus::InFlight,
        settlement_source: None,
        reconciliation_evidence: None,
    };
    contract.payments.insert(payment_id, payment);

    let mut daily = contract.get_daily_spending(accounting_day);
    daily.add_reservation(amount);
    contract.daily_spending.insert(accounting_day, daily);
    contract.flush_storage_maps();

    // 1. Reconciliation before timeout (< 300s) panics with grace period error
    let premature_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT - 1;
    let mut premature_context = get_unit_context(owner.clone());
    premature_context.block_timestamp(premature_time);
    testing_env!(premature_context.build());

    let mut contract_ref = std::panic::AssertUnwindSafe(&mut contract);
    let res_premature = std::panic::catch_unwind(move || {
        contract_ref.reconcile_stuck_payment(payment_id, true, "early_evidence".into());
    });
    assert!(res_premature.is_err(), "Must panic before timeout elapses");

    // 2. Reconciliation after timeout (>= 300s) succeeds with transfer_succeeded = true
    let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
    let mut owner_context = get_unit_context(owner.clone());
    owner_context.block_timestamp(reconcile_time);
    testing_env!(owner_context.build());

    contract.reconcile_stuck_payment(payment_id, true, "valid_evidence_ref".into());

    let reconciled = contract.get_payment(payment_id).expect("Payment exists");
    assert_eq!(reconciled.status, PaymentStatus::Succeeded);
    assert_eq!(
        reconciled.settlement_source,
        Some(SettlementSource::OwnerReconciliation)
    );
    assert_eq!(
        reconciled.reconciliation_evidence,
        Some("valid_evidence_ref".into())
    );

    let daily_settled = contract.get_daily_spending(accounting_day);
    assert_eq!(daily_settled.confirmed_amount, amount);
    assert_eq!(daily_settled.reserved_amount, NearToken::from_yoctonear(0));

    // 3. Duplicate reconciliation on settled payment is rejected
    let mut contract_ref_dup = std::panic::AssertUnwindSafe(&mut contract);
    let res_duplicate = std::panic::catch_unwind(move || {
        contract_ref_dup.reconcile_stuck_payment(payment_id, true, "second_evidence".into());
    });
    assert!(
        res_duplicate.is_err(),
        "Cannot reconcile already-settled payment"
    );

    // 4. Test reconciliation with transfer_succeeded = false on payment 2
    let payment_id2 = 2;
    contract.next_payment_id = 3;
    let payment2 = PaymentRecord {
        payment_id: payment_id2,
        recipient,
        amount,
        accounting_day,
        proposal_id: None,
        initiated_at: timestamp,
        settled_at: None,
        status: PaymentStatus::InFlight,
        settlement_source: None,
        reconciliation_evidence: None,
    };
    contract.payments.insert(payment_id2, payment2);
    let mut daily2 = contract.get_daily_spending(accounting_day);
    daily2.add_reservation(amount);
    contract.daily_spending.insert(accounting_day, daily2);
    contract.flush_storage_maps();

    contract.reconcile_stuck_payment(payment_id2, false, "failed_tx_evidence".into());

    let reconciled2 = contract.get_payment(payment_id2).expect("Payment exists");
    assert_eq!(reconciled2.status, PaymentStatus::Failed);
    assert_eq!(
        reconciled2.settlement_source,
        Some(SettlementSource::OwnerReconciliation)
    );
    assert_eq!(
        reconciled2.reconciliation_evidence,
        Some("failed_tx_evidence".into())
    );

    let daily_final = contract.get_daily_spending(accounting_day);
    assert_eq!(daily_final.confirmed_amount, amount); // from payment 1 only
    assert_eq!(daily_final.reserved_amount, NearToken::from_yoctonear(0)); // released
}

#[test]
fn test_ps_int_12_u_late_callback() {
    let owner = accounts(1);
    let agent = accounts(2);
    let recipient = accounts(3);
    let contract_id = accounts(0);
    let amount = NearToken::from_near(1);
    let timestamp: Timestamp = 1_000_000_000_000;

    let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());
    let payment_id = 1;
    contract.next_payment_id = 2;
    let accounting_day = calculate_day_index(timestamp);

    let payment = PaymentRecord {
        payment_id,
        recipient,
        amount,
        accounting_day,
        proposal_id: None,
        initiated_at: timestamp,
        settled_at: None,
        status: PaymentStatus::InFlight,
        settlement_source: None,
        reconciliation_evidence: None,
    };
    contract.payments.insert(payment_id, payment);
    let mut daily = contract.get_daily_spending(accounting_day);
    daily.add_reservation(amount);
    contract.daily_spending.insert(accounting_day, daily);
    contract.flush_storage_maps();

    // 1. Owner reconciles after timeout
    let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
    let mut owner_context = get_unit_context(owner);
    owner_context.block_timestamp(reconcile_time);
    testing_env!(owner_context.build());

    contract.reconcile_stuck_payment(payment_id, true, "early_reconciliation".into());

    // 2. Delayed callback arrives later under private callback context
    let callback_time = reconcile_time + 10_000_000_000;
    let mut callback_context = get_unit_context(contract_id);
    callback_context.account_balance(NearToken::from_near(10));
    callback_context.block_timestamp(callback_time);
    set_promise_result(callback_context.build(), PromiseResult::Successful(vec![]));

    // Callback must panic because payment is no longer InFlight
    let mut contract_ref = std::panic::AssertUnwindSafe(&mut contract);
    let res_callback = std::panic::catch_unwind(move || {
        contract_ref.on_payment_completed(payment_id);
    });
    assert!(
        res_callback.is_err(),
        "Late callback must panic when payment is not InFlight"
    );

    // 3. Confirm reconciled state and accounting remain unchanged
    let payment_post = contract.get_payment(payment_id).expect("Payment exists");
    assert_eq!(payment_post.status, PaymentStatus::Succeeded);
    assert_eq!(
        payment_post.settlement_source,
        Some(SettlementSource::OwnerReconciliation)
    );
    assert_eq!(
        payment_post.reconciliation_evidence,
        Some("early_reconciliation".into())
    );

    let daily_post = contract.get_daily_spending(accounting_day);
    assert_eq!(daily_post.confirmed_amount, amount);
    assert_eq!(daily_post.reserved_amount, NearToken::from_yoctonear(0));
}
