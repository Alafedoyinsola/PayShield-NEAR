use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise,
    PromiseResult, Timestamp,
};

/// Nanoseconds in one UTC calendar day (24 * 60 * 60 * 1_000_000_000).
pub const NANOS_PER_DAY: u64 = 86_400_000_000_000;

/// Fixed duration for which a proposal remains valid before expiring (24 hours).
pub const PROPOSAL_VALIDITY_NANOS: u64 = 86_400_000_000_000;

/// Provisional gas allocated for asynchronous payment callback execution (25 TGas).
pub const CALLBACK_GAS: Gas = Gas::from_tgas(25);

/// Operational grace period before an ambiguous in-flight payment may be reconciled by owner (5 minutes).
pub const PAYMENT_SETTLEMENT_TIMEOUT: Timestamp = 300_000_000_000;

/// Computes the UTC day index from a nanosecond timestamp.
pub const fn calculate_day_index(timestamp: Timestamp) -> u64 {
    timestamp / NANOS_PER_DAY
}

/// Computes the expiration timestamp for a proposal created at the given timestamp.
pub fn calculate_proposal_expiry(created_at: Timestamp) -> Timestamp {
    created_at
        .checked_add(PROPOSAL_VALIDITY_NANOS)
        .expect("Proposal expiry timestamp overflow")
}

/// Provisional safety buffer required above storage staking and transfer amounts (0.02 NEAR).
pub const STORAGE_SAFETY_BUFFER: NearToken =
    NearToken::from_yoctonear(20_000_000_000_000_000_000_000);

/// Calculates the storage-cost shortfall after taking locked balance into account.
/// Staking-locked funds satisfy the storage-staking collateral requirement.
/// If locked balance >= storage cost, the shortfall is zero.
pub fn compute_storage_shortfall(storage_cost: NearToken, locked_balance: NearToken) -> NearToken {
    if storage_cost > locked_balance {
        storage_cost
            .checked_sub(locked_balance)
            .expect("Storage shortfall calculation underflow")
    } else {
        NearToken::from_yoctonear(0)
    }
}

/// Calculates the minimum liquid balance required to safely fund a transfer of `transfer_amount`,
/// cover any storage-cost shortfall, and maintain the provisional safety buffer.
pub fn calculate_required_liquid_balance(
    storage_cost: NearToken,
    locked_balance: NearToken,
    transfer_amount: NearToken,
) -> NearToken {
    let storage_shortfall = compute_storage_shortfall(storage_cost, locked_balance);
    storage_shortfall
        .checked_add(transfer_amount)
        .expect("Required liquid balance transfer overflow")
        .checked_add(STORAGE_SAFETY_BUFFER)
        .expect("Required liquid balance safety buffer overflow")
}

/// Payment policy defining limits in exact yoctoNEAR (no floating point arithmetic).
///
/// Transactions exceeding `require_approval_above` require human owner approval.
/// `max_amount_per_tx` is an absolute limit that cannot be bypassed even with human approval.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentPolicy {
    /// Absolute maximum amount allowed for a single transaction.
    /// This limit cannot be bypassed even with human approval.
    #[serde(alias = "max_per_tx", alias = "per_tx_limit")]
    pub max_amount_per_tx: NearToken,

    /// Maximum aggregate spending allowed within a 24-hour window.
    #[serde(alias = "daily_limit")]
    pub daily_spending_limit: NearToken,

    /// Transactions above this threshold require human owner approval.
    /// A zero value is valid and means all transactions require approval.
    #[serde(alias = "approval_threshold")]
    pub require_approval_above: NearToken,
}

impl PaymentPolicy {
    /// Validates policy parameters using exact integer comparisons.
    pub fn validate(&self) {
        require!(
            self.max_amount_per_tx.as_yoctonear() > 0,
            "Max amount per transaction must be greater than zero"
        );
        require!(
            self.daily_spending_limit.as_yoctonear() > 0,
            "Daily spending limit must be greater than zero"
        );
        require!(
            self.require_approval_above <= self.max_amount_per_tx,
            "Approval threshold cannot exceed max amount per transaction"
        );
    }
}

impl Default for PaymentPolicy {
    fn default() -> Self {
        Self {
            max_amount_per_tx: NearToken::from_yoctonear(1_000_000_000_000_000_000_000_000), // 1 NEAR
            daily_spending_limit: NearToken::from_yoctonear(5_000_000_000_000_000_000_000_000), // 5 NEAR
            require_approval_above: NearToken::from_yoctonear(500_000_000_000_000_000_000_000), // 0.5 NEAR
        }
    }
}

/// Representation of a recipient's security status.
#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipientStatus {
    Allowed,
    Blocked,
    PendingApproval,
    Unknown,
}

/// Lifecycle status for payment proposals.
#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Executing,
    Executed,
    Failed,
}

/// High-level execution outcome returned by `execute_proposal`.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Executed { payment_id: u64 },
    Expired,
}

/// Outcome of execution preflight validation.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionPreflightOutcome {
    /// Proposal has expired; proposal status updated to Expired and persisted.
    Expired,
    /// All execution preconditions and balance-safety checks passed.
    /// Ready for execution and transfer scheduling in Stage 2D-B.
    Ready {
        proposal_id: u64,
        recipient: AccountId,
        amount: NearToken,
        accounting_day: u64,
    },
}

/// Lifecycle status for payment transactions.
#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
    InFlight,
    Succeeded,
    Failed,
}

/// Identifies the authority and mechanism that recorded settlement for a payment.
#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementSource {
    Callback,
    OwnerReconciliation,
}

/// Data structure representing a payment proposal requiring human oversight.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentProposal {
    #[serde(alias = "id")]
    pub proposal_id: u64,
    pub recipient: AccountId,
    pub amount: NearToken,
    pub reason: String,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub status: ProposalStatus,
}

/// Record of a payment transaction tracked on chain.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRecord {
    pub payment_id: u64,
    pub recipient: AccountId,
    pub amount: NearToken,
    pub accounting_day: u64,
    pub proposal_id: Option<u64>,
    pub initiated_at: Timestamp,
    pub settled_at: Option<Timestamp>,
    pub status: PaymentStatus,
    pub settlement_source: Option<SettlementSource>,
    pub reconciliation_evidence: Option<String>,
}

/// Tracks daily spending usage for a specific UTC calendar day.
#[near(serializers = [json, borsh])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailySpendingUsage {
    pub day_id: u64,
    pub confirmed_amount: NearToken,
    pub reserved_amount: NearToken,
}

impl DailySpendingUsage {
    /// Creates a new empty daily spending record for the specified UTC day.
    pub fn new(day_id: u64) -> Self {
        Self {
            day_id,
            confirmed_amount: NearToken::from_yoctonear(0),
            reserved_amount: NearToken::from_yoctonear(0),
        }
    }

    /// Returns the aggregate allocated spending (confirmed + reserved) for this day.
    pub fn total_allocated(&self) -> NearToken {
        NearToken::from_yoctonear(
            self.confirmed_amount
                .as_yoctonear()
                .checked_add(self.reserved_amount.as_yoctonear())
                .expect("Total allocated spending overflow"),
        )
    }

    /// Adds a spending reservation to this day's quota using checked arithmetic.
    pub fn add_reservation(&mut self, amount: NearToken) {
        self.reserved_amount = NearToken::from_yoctonear(
            self.reserved_amount
                .as_yoctonear()
                .checked_add(amount.as_yoctonear())
                .expect("Reserved amount overflow"),
        );
    }

    /// Converts an existing reservation into confirmed spending upon successful settlement.
    pub fn confirm_reservation(&mut self, amount: NearToken) {
        self.reserved_amount = NearToken::from_yoctonear(
            self.reserved_amount
                .as_yoctonear()
                .checked_sub(amount.as_yoctonear())
                .expect("Reserved amount underflow"),
        );
        self.confirmed_amount = NearToken::from_yoctonear(
            self.confirmed_amount
                .as_yoctonear()
                .checked_add(amount.as_yoctonear())
                .expect("Confirmed amount overflow"),
        );
    }

    /// Releases a spending reservation without confirming it after a verified failure.
    pub fn release_reservation(&mut self, amount: NearToken) {
        self.reserved_amount = NearToken::from_yoctonear(
            self.reserved_amount
                .as_yoctonear()
                .checked_sub(amount.as_yoctonear())
                .expect("Reserved amount underflow"),
        );
    }

    /// Calculates the prospective total allocation if `amount` were to be reserved,
    /// using checked arithmetic, without mutating the record.
    pub fn prospective_total_allocated(&self, amount: NearToken) -> NearToken {
        NearToken::from_yoctonear(
            self.total_allocated()
                .as_yoctonear()
                .checked_add(amount.as_yoctonear())
                .expect("Prospective total allocated spending overflow"),
        )
    }
}

/// Storage keys for persistent collections.
#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
pub enum StorageKey {
    Recipients,
    Proposals,
    Payments,
    DailySpending,
}

/// Main state structure for the PayShield smart contract.
#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct PayShield {
    pub owner_id: AccountId,
    pub agent_id: AccountId,
    pub policy: PaymentPolicy,
    pub recipients: LookupMap<AccountId, RecipientStatus>,
    pub proposals: LookupMap<u64, PaymentProposal>,
    pub next_proposal_id: u64,
    pub payments: LookupMap<u64, PaymentRecord>,
    pub next_payment_id: u64,
    pub daily_spending: LookupMap<u64, DailySpendingUsage>,
}

/// Canonical public contract interface.
#[near]
impl PayShield {
    /// Canonical one-time initialization of PayShield contract state.
    /// Rejects repeated initialization if state already exists.
    #[init]
    pub fn init(owner_id: AccountId, agent_id: AccountId, initial_policy: PaymentPolicy) -> Self {
        require!(!env::state_exists(), "The contract is already initialized");
        initial_policy.validate();

        Self {
            owner_id,
            agent_id,
            policy: initial_policy,
            recipients: LookupMap::new(StorageKey::Recipients),
            proposals: LookupMap::new(StorageKey::Proposals),
            next_proposal_id: 1,
            payments: LookupMap::new(StorageKey::Payments),
            next_payment_id: 1,
            daily_spending: LookupMap::new(StorageKey::DailySpending),
        }
    }

    /// Read-only method returning the active payment policy.
    pub fn get_policy(&self) -> PaymentPolicy {
        self.policy.clone()
    }

    /// Read-only method returning the status of a recipient account.
    pub fn get_recipient_status(&self, account_id: AccountId) -> RecipientStatus {
        self.recipients
            .get(&account_id)
            .copied()
            .unwrap_or(RecipientStatus::Unknown)
    }

    /// Read-only method checking if a recipient is explicitly allowed.
    pub fn is_recipient_allowed(&self, account_id: AccountId) -> bool {
        self.get_recipient_status(account_id) == RecipientStatus::Allowed
    }

    /// Read-only method returning the owner account ID.
    pub fn get_owner(&self) -> AccountId {
        self.owner_id.clone()
    }

    /// Read-only method returning the agent account ID.
    pub fn get_agent(&self) -> AccountId {
        self.agent_id.clone()
    }

    /// Read-only method returning daily spending usage for a specified UTC day index.
    /// Returns an empty daily spending record if no activity has been recorded for the day.
    pub fn get_daily_spending(&self, day_id: u64) -> DailySpendingUsage {
        self.daily_spending
            .get(&day_id)
            .cloned()
            .unwrap_or_else(|| DailySpendingUsage::new(day_id))
    }

    /// Read-only method returning the current UTC day index based on block timestamp.
    pub fn get_current_day_index(&self) -> u64 {
        calculate_day_index(env::block_timestamp())
    }

    /// Read-only method returning a proposal by ID.
    pub fn get_proposal(&self, proposal_id: u64) -> Option<PaymentProposal> {
        self.proposals.get(&proposal_id).cloned()
    }

    /// Read-only method returning the next proposal ID.
    pub fn get_next_proposal_id(&self) -> u64 {
        self.next_proposal_id
    }

    /// Read-only method returning a payment record by ID.
    pub fn get_payment(&self, payment_id: u64) -> Option<PaymentRecord> {
        self.payments.get(&payment_id).cloned()
    }

    /// Read-only method returning the next payment ID counter.
    pub fn get_next_payment_id(&self) -> u64 {
        self.next_payment_id
    }

    /// Updates the security status of a recipient account.
    /// Restricted to contract owner. Changes take effect immediately.
    pub fn set_recipient_status(&mut self, account_id: AccountId, status: RecipientStatus) {
        self.assert_owner();
        self.recipients.insert(account_id, status);
    }

    /// Creates a payment proposal requiring human owner oversight.
    /// Restricted to authorized agent.
    pub fn propose_payment(
        &mut self,
        recipient: AccountId,
        amount: NearToken,
        reason: String,
    ) -> u64 {
        self.assert_agent();

        require!(
            amount.as_yoctonear() > 0,
            "Payment amount must be greater than zero"
        );
        require!(
            amount <= self.policy.max_amount_per_tx,
            "Payment amount exceeds maximum per transaction limit"
        );

        let recipient_status = self.get_recipient_status(recipient.clone());
        require!(
            recipient_status != RecipientStatus::Blocked,
            "Cannot propose payment to a blocked recipient"
        );

        if recipient_status == RecipientStatus::Allowed {
            require!(
                amount > self.policy.require_approval_above,
                "Payment to allowed recipient at or below approval threshold does not require a proposal"
            );
        }

        require!(
            reason.as_bytes().len() <= 256,
            "Reason exceeds maximum length of 256 bytes"
        );

        let proposal_id = self.next_proposal_id;
        self.next_proposal_id = self
            .next_proposal_id
            .checked_add(1)
            .expect("Proposal ID overflow");

        let created_at = env::block_timestamp();
        let expires_at = calculate_proposal_expiry(created_at);

        let proposal = PaymentProposal {
            proposal_id,
            recipient,
            amount,
            reason,
            created_at,
            expires_at,
            status: ProposalStatus::Pending,
        };

        self.proposals.insert(proposal_id, proposal);
        proposal_id
    }

    /// Evaluates and approves a pending payment proposal.
    /// Restricted to contract owner.
    /// If the proposal has expired, its status is persistently updated to Expired
    /// and `ProposalStatus::Expired` is returned without aborting the transaction.
    pub fn approve_proposal(&mut self, proposal_id: u64) -> ProposalStatus {
        self.assert_owner();

        let mut proposal = self
            .proposals
            .get(&proposal_id)
            .cloned()
            .expect("Proposal not found");

        require!(
            proposal.status == ProposalStatus::Pending,
            "Only pending proposals can be approved"
        );

        let current_timestamp = env::block_timestamp();
        if current_timestamp >= proposal.expires_at {
            proposal.status = ProposalStatus::Expired;
            self.proposals.insert(proposal_id, proposal);
            return ProposalStatus::Expired;
        }

        let current_recipient_status = self.get_recipient_status(proposal.recipient.clone());
        require!(
            current_recipient_status != RecipientStatus::Blocked,
            "Cannot approve proposal for a blocked recipient"
        );

        if current_recipient_status == RecipientStatus::Allowed {
            require!(
                proposal.amount > self.policy.require_approval_above,
                "Proposal no longer requires approval under current policy"
            );
        }

        require!(
            proposal.amount <= self.policy.max_amount_per_tx,
            "Proposal amount exceeds current max amount per transaction policy"
        );

        proposal.status = ProposalStatus::Approved;
        self.proposals.insert(proposal_id, proposal);
        ProposalStatus::Approved
    }

    /// Rejects a pending proposal or revokes an approved proposal prior to execution.
    /// Restricted to contract owner.
    pub fn reject_proposal(&mut self, proposal_id: u64) {
        self.assert_owner();

        let mut proposal = self
            .proposals
            .get(&proposal_id)
            .cloned()
            .expect("Proposal not found");

        require!(
            proposal.status == ProposalStatus::Pending
                || proposal.status == ProposalStatus::Approved,
            "Cannot reject proposal in current status"
        );

        proposal.status = ProposalStatus::Rejected;
        self.proposals.insert(proposal_id, proposal);
    }

    /// Flushes buffered modifications in persistent collections to trie storage.
    /// Required before relying on `env::storage_usage()` for accurate post-write measurements.
    pub fn flush_storage_maps(&mut self) {
        self.proposals.flush();
        self.payments.flush();
        self.daily_spending.flush();
        self.recipients.flush();
    }

    /// Checks whether the contract currently has sufficient available liquid balance
    /// to fund the proposed transfer amount while preserving required storage backing
    /// and the provisional safety buffer. Panics if balance is insufficient.
    pub fn check_liquid_balance_safety(&self, transfer_amount: NearToken) {
        let storage_cost = NearToken::from_yoctonear(
            (env::storage_usage() as u128)
                .checked_mul(env::storage_byte_cost().as_yoctonear())
                .expect("Storage cost overflow"),
        );
        let locked_balance = env::account_locked_balance();
        let required_liquid_balance =
            calculate_required_liquid_balance(storage_cost, locked_balance, transfer_amount);
        require!(
            env::account_balance() >= required_liquid_balance,
            "Insufficient available balance for transfer, storage backing, and safety buffer"
        );
    }

    /// Returns the minimum liquid balance required for the contract to safely fund
    /// `transfer_amount` while satisfying storage staking and the safety buffer.
    pub fn get_required_liquid_balance(&self, transfer_amount: NearToken) -> NearToken {
        let storage_cost = NearToken::from_yoctonear(
            (env::storage_usage() as u128)
                .checked_mul(env::storage_byte_cost().as_yoctonear())
                .expect("Storage cost overflow"),
        );
        let locked_balance = env::account_locked_balance();
        calculate_required_liquid_balance(storage_cost, locked_balance, transfer_amount)
    }

    /// Preflights and validates proposal execution preconditions and balance safety.
    ///
    /// Authorization: Contract owner or configured agent.
    ///
    /// If the proposal has expired, updates status to Expired and returns `ExecutionPreflightOutcome::Expired`
    /// without panicking.
    ///
    /// If all preconditions (approval, live recipient status, max tx policy, daily spending limit,
    /// payment counter overflow, and liquid balance safety) pass, returns `ExecutionPreflightOutcome::Ready`.
    ///
    /// Note: Does NOT mark proposal Executing, does NOT persist a spending reservation, does NOT
    /// create an InFlight payment record, and does NOT schedule a transfer promise (deferred to Stage 2D-B).
    pub fn preflight_proposal_execution(&mut self, proposal_id: u64) -> ExecutionPreflightOutcome {
        self.assert_owner_or_agent();

        let mut proposal = self
            .proposals
            .get(&proposal_id)
            .cloned()
            .expect("Proposal not found");

        require!(
            proposal.status == ProposalStatus::Approved,
            "Proposal is not in approved status for execution"
        );

        let current_timestamp = env::block_timestamp();
        if current_timestamp >= proposal.expires_at {
            proposal.status = ProposalStatus::Expired;
            self.proposals.insert(proposal_id, proposal);
            return ExecutionPreflightOutcome::Expired;
        }

        let current_recipient_status = self.get_recipient_status(proposal.recipient.clone());
        require!(
            current_recipient_status != RecipientStatus::Blocked,
            "Cannot execute proposal for a blocked recipient"
        );

        require!(
            proposal.amount.as_yoctonear() > 0,
            "Proposal payment amount must be greater than zero"
        );
        require!(
            proposal.amount <= self.policy.max_amount_per_tx,
            "Proposal amount exceeds current max amount per transaction policy"
        );

        let current_day = self.get_current_day_index();
        let usage = self.get_daily_spending(current_day);
        require!(
            usage.day_id == current_day,
            "Daily spending usage day mismatch"
        );
        let prospective_total = usage
            .total_allocated()
            .as_yoctonear()
            .checked_add(proposal.amount.as_yoctonear())
            .expect("Daily spending allocation overflow");
        require!(
            prospective_total <= self.policy.daily_spending_limit.as_yoctonear(),
            "Proposal amount would exceed daily spending limit"
        );

        self.next_payment_id
            .checked_add(1)
            .expect("Payment ID counter overflow");

        self.check_liquid_balance_safety(proposal.amount);

        ExecutionPreflightOutcome::Ready {
            proposal_id,
            recipient: proposal.recipient,
            amount: proposal.amount,
            accounting_day: current_day,
        }
    }

    /// Executes an approved payment proposal using a native NEAR transfer promise.
    ///
    /// Authorization: Contract owner or authorized agent.
    ///
    /// If the proposal has expired, its status is updated to Expired and
    /// `ExecutionOutcome::Expired` is returned.
    ///
    /// If preflight passes:
    /// - Transitions proposal to `ProposalStatus::Executing`.
    /// - Allocates `payment_id` and increments `next_payment_id` with checked arithmetic.
    /// - Persists `PaymentRecord` with `PaymentStatus::InFlight`.
    /// - Adds spending reservation to daily spending usage.
    /// - Flushes all modified storage maps.
    /// - Dispatches native NEAR transfer promise and chains private callback `on_payment_completed`.
    /// - Returns `ExecutionOutcome::Executed { payment_id }`.
    pub fn execute_proposal(&mut self, proposal_id: u64) -> ExecutionOutcome {
        self.assert_owner_or_agent();

        let preflight = self.preflight_proposal_execution(proposal_id);
        let (proposal_id, recipient, amount, accounting_day) = match preflight {
            ExecutionPreflightOutcome::Expired => return ExecutionOutcome::Expired,
            ExecutionPreflightOutcome::Ready {
                proposal_id,
                recipient,
                amount,
                accounting_day,
            } => (proposal_id, recipient, amount, accounting_day),
        };

        let payment_id = self.next_payment_id;
        self.next_payment_id = self
            .next_payment_id
            .checked_add(1)
            .expect("Payment ID counter overflow");

        let mut proposal = self
            .proposals
            .get(&proposal_id)
            .cloned()
            .expect("Proposal not found");
        proposal.status = ProposalStatus::Executing;
        self.proposals.insert(proposal_id, proposal);

        let payment = PaymentRecord {
            payment_id,
            recipient: recipient.clone(),
            amount,
            accounting_day,
            proposal_id: Some(proposal_id),
            initiated_at: env::block_timestamp(),
            settled_at: None,
            status: PaymentStatus::InFlight,
            settlement_source: None,
            reconciliation_evidence: None,
        };
        self.payments.insert(payment_id, payment);

        let mut daily_usage = self.get_daily_spending(accounting_day);
        daily_usage.add_reservation(amount);
        self.daily_spending.insert(accounting_day, daily_usage);

        self.flush_storage_maps();

        let _promise = Promise::new(recipient).transfer(amount).then(
            Self::ext(env::current_account_id())
                .with_static_gas(CALLBACK_GAS)
                .on_payment_completed(payment_id),
        );

        ExecutionOutcome::Executed { payment_id }
    }

    /// Executes a direct payment to an allowed recipient without requiring a prior proposal.
    ///
    /// Authorization: Authorized agent only (`self.assert_agent()`).
    ///
    /// Preconditions:
    /// - Caller must be the configured agent.
    /// - Recipient must have `RecipientStatus::Allowed`.
    /// - Amount must be greater than zero.
    /// - Amount must not exceed `max_amount_per_tx`.
    /// - Amount must not exceed `require_approval_above`.
    /// - Prospective daily spending must not exceed `daily_spending_limit`.
    /// - Contract liquid balance must safely satisfy the transfer, storage staking, and safety buffer.
    ///
    /// State mutations before promise dispatch:
    /// - Validates payment ID counter overflow.
    /// - Persists `PaymentRecord` with `proposal_id: None` and `status: PaymentStatus::InFlight`.
    /// - Records spending reservation in daily spending for current UTC accounting day.
    /// - Flushes storage maps to trie.
    ///
    /// Transfer dispatch:
    /// - Dispatches native NEAR transfer promise chained to `on_payment_completed(payment_id)`.
    /// - Returns the allocated `payment_id`.
    pub fn pay_direct(&mut self, recipient: AccountId, amount: NearToken) -> u64 {
        self.assert_agent();

        let recipient_status = self.get_recipient_status(recipient.clone());
        require!(
            recipient_status != RecipientStatus::Blocked,
            "Cannot make direct payment to a blocked recipient"
        );
        require!(
            recipient_status == RecipientStatus::Allowed,
            "Direct payments are only permitted for allowed recipients"
        );

        require!(
            amount.as_yoctonear() > 0,
            "Payment amount must be greater than zero"
        );
        require!(
            amount <= self.policy.max_amount_per_tx,
            "Payment amount exceeds maximum per transaction limit"
        );
        require!(
            amount <= self.policy.require_approval_above,
            "Payment amount exceeds approval threshold; proposal required"
        );

        let current_day = self.get_current_day_index();
        let mut daily_usage = self.get_daily_spending(current_day);
        require!(
            daily_usage.day_id == current_day,
            "Daily spending usage day mismatch"
        );
        let prospective_total = daily_usage
            .total_allocated()
            .as_yoctonear()
            .checked_add(amount.as_yoctonear())
            .expect("Daily spending allocation overflow");
        require!(
            prospective_total <= self.policy.daily_spending_limit.as_yoctonear(),
            "Payment amount would exceed daily spending limit"
        );

        self.next_payment_id
            .checked_add(1)
            .expect("Payment ID counter overflow");

        self.check_liquid_balance_safety(amount);

        let payment_id = self.next_payment_id;
        self.next_payment_id = self
            .next_payment_id
            .checked_add(1)
            .expect("Payment ID counter overflow");

        let payment = PaymentRecord {
            payment_id,
            recipient: recipient.clone(),
            amount,
            accounting_day: current_day,
            proposal_id: None,
            initiated_at: env::block_timestamp(),
            settled_at: None,
            status: PaymentStatus::InFlight,
            settlement_source: None,
            reconciliation_evidence: None,
        };
        self.payments.insert(payment_id, payment);

        daily_usage.add_reservation(amount);
        self.daily_spending.insert(current_day, daily_usage);

        self.flush_storage_maps();

        let _promise = Promise::new(recipient).transfer(amount).then(
            Self::ext(env::current_account_id())
                .with_static_gas(CALLBACK_GAS)
                .on_payment_completed(payment_id),
        );

        payment_id
    }

    /// Private callback invoked when the native NEAR transfer promise completes.
    ///
    /// Settles the in-flight payment and executing proposal based on the promise outcome:
    /// - If successful: sets status to Succeeded/Executed, confirms daily spending reservation.
    /// - If failed: sets status to Failed/Failed, releases daily spending reservation.
    ///
    /// Records settlement timestamp and marks settlement source as `SettlementSource::Callback`.
    #[private]
    pub fn on_payment_completed(&mut self, payment_id: u64) {
        require!(
            env::current_account_id() == env::predecessor_account_id(),
            "Method is private"
        );
        require!(
            env::promise_results_count() == 1,
            "Expected exactly one promise result"
        );

        let mut payment = self
            .payments
            .get(&payment_id)
            .cloned()
            .expect("Payment not found");
        require!(
            payment.status == PaymentStatus::InFlight,
            "Payment is not in-flight"
        );

        let mut maybe_proposal = if let Some(proposal_id) = payment.proposal_id {
            let proposal = self
                .proposals
                .get(&proposal_id)
                .cloned()
                .expect("Proposal not found");
            require!(
                proposal.status == ProposalStatus::Executing,
                "Proposal is not in executing status"
            );
            Some((proposal_id, proposal))
        } else {
            None
        };

        let mut daily_usage = self.get_daily_spending(payment.accounting_day);

        #[allow(deprecated)]
        let transfer_succeeded = match env::promise_result(0) {
            PromiseResult::Successful(_) => true,
            PromiseResult::Failed => false,
        };

        if transfer_succeeded {
            payment.status = PaymentStatus::Succeeded;
            if let Some((_, ref mut proposal)) = maybe_proposal {
                proposal.status = ProposalStatus::Executed;
            }
            daily_usage.confirm_reservation(payment.amount);
        } else {
            payment.status = PaymentStatus::Failed;
            if let Some((_, ref mut proposal)) = maybe_proposal {
                proposal.status = ProposalStatus::Failed;
            }
            daily_usage.release_reservation(payment.amount);
        }

        payment.settled_at = Some(env::block_timestamp());
        payment.settlement_source = Some(SettlementSource::Callback);

        self.payments.insert(payment_id, payment);
        if let Some((proposal_id, proposal)) = maybe_proposal {
            self.proposals.insert(proposal_id, proposal);
        }
        self.daily_spending.insert(daily_usage.day_id, daily_usage);
        self.flush_storage_maps();
    }

    /// Reconciles an in-flight payment that has exceeded the settlement timeout.
    ///
    /// Can only be called by the contract owner.
    /// Requires non-empty evidence reference (<= 256 bytes UTF-8).
    /// Requires payment to be in `InFlight` status and proposal in `Executing` status.
    /// Requires current timestamp >= initiated_at + PAYMENT_SETTLEMENT_TIMEOUT.
    ///
    /// If `transfer_succeeded` is true:
    /// - Payment marked Succeeded
    /// - Proposal marked Executed
    /// - Daily spending reservation confirmed
    ///
    /// If `transfer_succeeded` is false:
    /// - Payment marked Failed
    /// - Proposal marked Failed
    /// - Daily spending reservation released
    ///
    /// Records settlement timestamp, settlement source (`SettlementSource::OwnerReconciliation`),
    /// and reconciliation evidence reference.
    pub fn reconcile_stuck_payment(
        &mut self,
        payment_id: u64,
        transfer_succeeded: bool,
        evidence_ref: String,
    ) {
        self.assert_owner();

        require!(
            !evidence_ref.trim().is_empty(),
            "Reconciliation evidence reference cannot be empty or whitespace"
        );
        require!(
            evidence_ref.as_bytes().len() <= 256,
            "Reconciliation evidence reference exceeds 256 UTF-8 bytes"
        );

        let mut payment = self
            .payments
            .get(&payment_id)
            .cloned()
            .expect("Payment not found");
        require!(
            payment.status == PaymentStatus::InFlight,
            "Payment is not in-flight"
        );

        let mut maybe_proposal = if let Some(proposal_id) = payment.proposal_id {
            let proposal = self
                .proposals
                .get(&proposal_id)
                .cloned()
                .expect("Proposal not found");
            require!(
                proposal.status == ProposalStatus::Executing,
                "Proposal is not in executing status"
            );
            Some((proposal_id, proposal))
        } else {
            None
        };

        let timeout_threshold = payment
            .initiated_at
            .checked_add(PAYMENT_SETTLEMENT_TIMEOUT)
            .expect("Settlement timeout calculation overflow");
        require!(
            env::block_timestamp() >= timeout_threshold,
            "Settlement grace period has not elapsed"
        );

        let mut daily_usage = self.get_daily_spending(payment.accounting_day);

        if transfer_succeeded {
            payment.status = PaymentStatus::Succeeded;
            if let Some((_, ref mut proposal)) = maybe_proposal {
                proposal.status = ProposalStatus::Executed;
            }
            daily_usage.confirm_reservation(payment.amount);
        } else {
            payment.status = PaymentStatus::Failed;
            if let Some((_, ref mut proposal)) = maybe_proposal {
                proposal.status = ProposalStatus::Failed;
            }
            daily_usage.release_reservation(payment.amount);
        }

        payment.settled_at = Some(env::block_timestamp());
        payment.settlement_source = Some(SettlementSource::OwnerReconciliation);
        payment.reconciliation_evidence = Some(evidence_ref);

        self.payments.insert(payment_id, payment);
        if let Some((proposal_id, proposal)) = maybe_proposal {
            self.proposals.insert(proposal_id, proposal);
        }
        self.daily_spending.insert(daily_usage.day_id, daily_usage);
        self.flush_storage_maps();
    }
}

/// Inherent private implementation for internal authorization and invariants.
/// These helpers are intentionally not exposed in the public Wasm contract interface.
impl PayShield {
    /// Owner-only authorization helper. Panics if predecessor is not the owner.
    fn assert_owner(&self) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "Unauthorized: caller is not the contract owner"
        );
    }

    /// Agent-only authorization helper. Panics if predecessor is not the authorized agent.
    fn assert_agent(&self) {
        require!(
            env::predecessor_account_id() == self.agent_id,
            "Unauthorized: caller is not the authorized agent"
        );
    }

    /// Owner or authorized agent authorization helper. Panics if predecessor is neither.
    fn assert_owner_or_agent(&self) {
        let predecessor = env::predecessor_account_id();
        require!(
            predecessor == self.owner_id || predecessor == self.agent_id,
            "Unauthorized: caller is neither contract owner nor authorized agent"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(deprecated)]
    use near_sdk::test_utils::testing_env_with_promise_results;
    use near_sdk::test_utils::{accounts, get_created_receipts, VMContextBuilder};
    use near_sdk::testing_env;

    fn get_context(predecessor: AccountId) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .signer_account_id(predecessor.clone())
            .predecessor_account_id(predecessor);
        builder
    }

    #[test]
    fn test_initialization() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_near(1),
        };

        let contract = PayShield::init(owner.clone(), agent.clone(), policy.clone());

        assert_eq!(contract.get_owner(), owner);
        assert_eq!(contract.get_agent(), agent);
        assert_eq!(contract.get_policy(), policy);
        assert_eq!(
            contract.get_recipient_status(accounts(3)),
            RecipientStatus::Unknown
        );
        assert!(!contract.is_recipient_allowed(accounts(3)));
        let current_day = contract.get_current_day_index();
        assert_eq!(
            contract.get_daily_spending(current_day).confirmed_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            contract.get_daily_spending(current_day).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(contract.get_next_proposal_id(), 1);
        assert_eq!(contract.get_next_payment_id(), 1);
    }

    #[test]
    fn test_zero_approval_threshold_is_valid() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_yoctonear(0),
        };

        let contract = PayShield::init(owner, agent, policy.clone());
        assert_eq!(contract.get_policy(), policy);
        assert_eq!(
            contract.get_policy().require_approval_above,
            NearToken::from_yoctonear(0)
        );
    }

    #[test]
    #[should_panic(expected = "Max amount per transaction must be greater than zero")]
    fn test_init_panics_zero_max_amount_per_tx() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_yoctonear(0),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_yoctonear(0),
        };

        PayShield::init(owner, agent, policy);
    }

    #[test]
    #[should_panic(expected = "Daily spending limit must be greater than zero")]
    fn test_init_panics_zero_daily_spending_limit() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(1),
            daily_spending_limit: NearToken::from_yoctonear(0),
            require_approval_above: NearToken::from_yoctonear(0),
        };

        PayShield::init(owner, agent, policy);
    }

    #[test]
    #[should_panic(expected = "Approval threshold cannot exceed max amount per transaction")]
    fn test_init_panics_approval_threshold_exceeds_max_tx() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(1),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_near(2),
        };

        PayShield::init(owner, agent, policy);
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the contract owner")]
    fn test_assert_owner_fails_for_non_owner() {
        let owner = accounts(1);
        let agent = accounts(2);
        let non_owner = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());

        let caller_context = get_context(non_owner);
        testing_env!(caller_context.build());

        contract.assert_owner();
    }

    #[test]
    fn test_assert_owner_succeeds_for_owner() {
        let owner = accounts(1);
        let agent = accounts(2);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());
        contract.assert_owner();
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the authorized agent")]
    fn test_assert_agent_fails_for_non_agent() {
        let owner = accounts(1);
        let agent = accounts(2);
        let intruder = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());

        let caller_context = get_context(intruder);
        testing_env!(caller_context.build());

        contract.assert_agent();
    }

    #[test]
    fn test_assert_agent_succeeds_for_agent() {
        let owner = accounts(1);
        let agent = accounts(2);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let caller_context = get_context(agent);
        testing_env!(caller_context.build());

        contract.assert_agent();
    }

    #[test]
    fn test_stage2b_initial_payment_id_counter() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());
        assert_eq!(contract.get_next_payment_id(), 1);
        assert_eq!(contract.get_payment(1), None);
    }

    #[test]
    fn test_stage2b_initial_empty_daily_spending() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());
        let usage = contract.get_daily_spending(100);
        assert_eq!(usage.day_id, 100);
        assert_eq!(usage.confirmed_amount, NearToken::from_yoctonear(0));
        assert_eq!(usage.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(usage.total_allocated(), NearToken::from_yoctonear(0));
    }

    #[test]
    fn test_stage2b_utc_day_calculation() {
        // Day 0: 0 nanoseconds
        assert_eq!(calculate_day_index(0), 0);
        // Day 0: 1 second before end of day
        assert_eq!(calculate_day_index(NANOS_PER_DAY - 1), 0);
        // Day 1: Exactly at 24 hours
        assert_eq!(calculate_day_index(NANOS_PER_DAY), 1);
        // Day 1: Middle of day 1
        assert_eq!(calculate_day_index(NANOS_PER_DAY + 3_600_000_000_000), 1);
        // Day 20500: ~56 years after unix epoch
        let ts = 20_500 * NANOS_PER_DAY + 12345;
        assert_eq!(calculate_day_index(ts), 20_500);
    }

    #[test]
    fn test_stage2b_independent_spending_buckets_for_different_days() {
        let mut day_1 = DailySpendingUsage::new(1);
        let mut day_2 = DailySpendingUsage::new(2);

        day_1.add_reservation(NearToken::from_near(3));
        day_2.add_reservation(NearToken::from_near(5));

        assert_eq!(day_1.reserved_amount, NearToken::from_near(3));
        assert_eq!(day_2.reserved_amount, NearToken::from_near(5));
        assert_eq!(day_1.total_allocated(), NearToken::from_near(3));
        assert_eq!(day_2.total_allocated(), NearToken::from_near(5));
    }

    #[test]
    fn test_stage2b_reservation_arithmetic() {
        let mut usage = DailySpendingUsage::new(10);
        assert_eq!(usage.total_allocated(), NearToken::from_near(0));

        usage.add_reservation(NearToken::from_near(2));
        assert_eq!(usage.reserved_amount, NearToken::from_near(2));
        assert_eq!(usage.confirmed_amount, NearToken::from_near(0));
        assert_eq!(usage.total_allocated(), NearToken::from_near(2));

        usage.add_reservation(NearToken::from_near(1));
        assert_eq!(usage.reserved_amount, NearToken::from_near(3));
        assert_eq!(usage.confirmed_amount, NearToken::from_near(0));
        assert_eq!(usage.total_allocated(), NearToken::from_near(3));
    }

    #[test]
    fn test_stage2b_successful_settlement_arithmetic() {
        let mut usage = DailySpendingUsage::new(10);
        usage.add_reservation(NearToken::from_near(5));

        // Confirm partial amount
        usage.confirm_reservation(NearToken::from_near(2));
        assert_eq!(usage.reserved_amount, NearToken::from_near(3));
        assert_eq!(usage.confirmed_amount, NearToken::from_near(2));
        assert_eq!(usage.total_allocated(), NearToken::from_near(5));

        // Confirm remaining amount
        usage.confirm_reservation(NearToken::from_near(3));
        assert_eq!(usage.reserved_amount, NearToken::from_near(0));
        assert_eq!(usage.confirmed_amount, NearToken::from_near(5));
        assert_eq!(usage.total_allocated(), NearToken::from_near(5));
    }

    #[test]
    fn test_stage2b_failed_payment_reservation_release_arithmetic() {
        let mut usage = DailySpendingUsage::new(10);
        usage.add_reservation(NearToken::from_near(4));
        assert_eq!(usage.total_allocated(), NearToken::from_near(4));

        // Release reservation after failure
        usage.release_reservation(NearToken::from_near(4));
        assert_eq!(usage.reserved_amount, NearToken::from_near(0));
        assert_eq!(usage.confirmed_amount, NearToken::from_near(0));
        assert_eq!(usage.total_allocated(), NearToken::from_near(0));
    }

    #[test]
    #[should_panic(expected = "Reserved amount overflow")]
    fn test_stage2b_reservation_overflow_rejection() {
        let mut usage = DailySpendingUsage::new(1);
        usage.reserved_amount = NearToken::from_yoctonear(u128::MAX);
        usage.add_reservation(NearToken::from_yoctonear(1));
    }

    #[test]
    #[should_panic(expected = "Reserved amount underflow")]
    fn test_stage2b_reservation_confirm_underflow_rejection() {
        let mut usage = DailySpendingUsage::new(1);
        usage.reserved_amount = NearToken::from_near(1);
        // Attempting to confirm more than reserved must panic, not saturate
        usage.confirm_reservation(NearToken::from_near(2));
    }

    #[test]
    #[should_panic(expected = "Reserved amount underflow")]
    fn test_stage2b_reservation_release_underflow_rejection() {
        let mut usage = DailySpendingUsage::new(1);
        usage.reserved_amount = NearToken::from_near(1);
        // Attempting to release more than reserved must panic, not saturate
        usage.release_reservation(NearToken::from_near(2));
    }

    #[test]
    fn test_stage2b_proposal_expiry_calculation() {
        let created_at = 1_000_000_000_000;
        let expires_at = calculate_proposal_expiry(created_at);
        assert_eq!(expires_at, created_at + PROPOSAL_VALIDITY_NANOS);
        assert_eq!(expires_at - created_at, 86_400_000_000_000);
    }

    // ==========================================
    // Stage 2C Unit Tests: Recipient Management
    // ==========================================

    #[test]
    fn test_stage2c_owner_can_set_all_recipient_statuses() {
        let owner = accounts(1);
        let agent = accounts(2);
        let target = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());

        assert_eq!(
            contract.get_recipient_status(target.clone()),
            RecipientStatus::Unknown
        );

        contract.set_recipient_status(target.clone(), RecipientStatus::Allowed);
        assert_eq!(
            contract.get_recipient_status(target.clone()),
            RecipientStatus::Allowed
        );
        assert!(contract.is_recipient_allowed(target.clone()));

        contract.set_recipient_status(target.clone(), RecipientStatus::PendingApproval);
        assert_eq!(
            contract.get_recipient_status(target.clone()),
            RecipientStatus::PendingApproval
        );
        assert!(!contract.is_recipient_allowed(target.clone()));

        contract.set_recipient_status(target.clone(), RecipientStatus::Blocked);
        assert_eq!(
            contract.get_recipient_status(target.clone()),
            RecipientStatus::Blocked
        );
        assert!(!contract.is_recipient_allowed(target.clone()));

        contract.set_recipient_status(target.clone(), RecipientStatus::Unknown);
        assert_eq!(
            contract.get_recipient_status(target.clone()),
            RecipientStatus::Unknown
        );
        assert!(!contract.is_recipient_allowed(target.clone()));
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the contract owner")]
    fn test_stage2c_set_recipient_status_fails_for_non_owner() {
        let owner = accounts(1);
        let agent = accounts(2);
        let stranger = accounts(3);
        let target = accounts(4);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());

        let stranger_context = get_context(stranger);
        testing_env!(stranger_context.build());

        contract.set_recipient_status(target, RecipientStatus::Allowed);
    }

    #[test]
    fn test_stage2c_recipient_status_changes_take_effect_immediately() {
        let owner = accounts(1);
        let agent = accounts(2);
        let target = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());

        contract.set_recipient_status(target.clone(), RecipientStatus::Allowed);
        assert_eq!(
            contract.get_recipient_status(target.clone()),
            RecipientStatus::Allowed
        );

        contract.set_recipient_status(target.clone(), RecipientStatus::Blocked);
        assert_eq!(
            contract.get_recipient_status(target),
            RecipientStatus::Blocked
        );
    }

    // ==========================================
    // Stage 2C Unit Tests: Proposal Creation
    // ==========================================

    #[test]
    fn test_stage2c_agent_can_create_valid_proposal_unknown_recipient() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        let proposal_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(100_000_000_000_000_000_000_000), // 0.1 NEAR
            "Payment for SaaS indexing".to_string(),
        );

        assert_eq!(proposal_id, 1);
        assert_eq!(contract.get_next_proposal_id(), 2);

        let proposal = contract.get_proposal(1).expect("Proposal not found");
        assert_eq!(proposal.proposal_id, 1);
        assert_eq!(proposal.recipient, recipient);
        assert_eq!(
            proposal.amount,
            NearToken::from_yoctonear(100_000_000_000_000_000_000_000)
        );
        assert_eq!(proposal.reason, "Payment for SaaS indexing");
        assert_eq!(proposal.created_at, 1_000_000_000_000);
        assert_eq!(
            proposal.expires_at,
            1_000_000_000_000 + PROPOSAL_VALIDITY_NANOS
        );
        assert_eq!(proposal.status, ProposalStatus::Pending);
    }

    #[test]
    fn test_stage2c_agent_can_create_valid_proposal_pending_approval_recipient() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        contract.set_recipient_status(recipient.clone(), RecipientStatus::PendingApproval);

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        let proposal_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000), // 0.2 NEAR
            "Vendor initial deposit".to_string(),
        );

        assert_eq!(proposal_id, 1);
        let prop = contract.get_proposal(1).unwrap();
        assert_eq!(prop.status, ProposalStatus::Pending);
    }

    #[test]
    fn test_stage2c_agent_can_create_valid_proposal_allowed_recipient_above_threshold() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        // Allowed recipient, threshold is 0.5 NEAR, max is 1.0 NEAR
        contract.set_recipient_status(recipient.clone(), RecipientStatus::Allowed);

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        let proposal_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(600_000_000_000_000_000_000_000), // 0.6 NEAR > 0.5 NEAR
            "High-value compute replenishment".to_string(),
        );

        assert_eq!(proposal_id, 1);
        let prop = contract.get_proposal(1).unwrap();
        assert_eq!(prop.status, ProposalStatus::Pending);
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the authorized agent")]
    fn test_stage2c_propose_payment_fails_for_non_agent() {
        let owner = accounts(1);
        let agent = accounts(2);
        let stranger = accounts(3);
        let recipient = accounts(4);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());

        let stranger_context = get_context(stranger);
        testing_env!(stranger_context.build());

        contract.propose_payment(
            recipient,
            NearToken::from_near(1),
            "Unauthorized propose".to_string(),
        );
    }

    #[test]
    #[should_panic(expected = "Cannot propose payment to a blocked recipient")]
    fn test_stage2c_propose_payment_fails_for_blocked_recipient() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        contract.set_recipient_status(recipient.clone(), RecipientStatus::Blocked);

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        contract.propose_payment(
            recipient,
            NearToken::from_near(1),
            "Attempt to blocked".to_string(),
        );
    }

    #[test]
    #[should_panic(
        expected = "Payment to allowed recipient at or below approval threshold does not require a proposal"
    )]
    fn test_stage2c_propose_payment_fails_for_allowed_at_or_below_threshold() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        contract.set_recipient_status(recipient.clone(), RecipientStatus::Allowed);

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        // Default threshold is 0.5 NEAR. 0.5 NEAR exactly must be rejected by propose_payment.
        contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(500_000_000_000_000_000_000_000),
            "Direct payment amount proposed".to_string(),
        );
    }

    #[test]
    #[should_panic(expected = "Payment amount must be greater than zero")]
    fn test_stage2c_propose_payment_fails_for_zero_amount() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(0),
            "Zero amount".to_string(),
        );
    }

    #[test]
    #[should_panic(expected = "Payment amount exceeds maximum per transaction limit")]
    fn test_stage2c_propose_payment_fails_for_amount_above_max_per_tx() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        // Default max per tx is 1 NEAR. 1.01 NEAR must be rejected.
        contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(1_010_000_000_000_000_000_000_000),
            "Over max limit".to_string(),
        );
    }

    #[test]
    fn test_stage2c_propose_payment_accepts_exactly_256_utf8_bytes() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        // 252 ASCII bytes + 4 bytes crab emoji = exactly 256 bytes
        let exact_256_bytes = "a".repeat(252) + "🦀";
        assert_eq!(exact_256_bytes.as_bytes().len(), 256);

        let proposal_id =
            contract.propose_payment(recipient, NearToken::from_near(1), exact_256_bytes.clone());

        let prop = contract.get_proposal(proposal_id).unwrap();
        assert_eq!(prop.reason, exact_256_bytes);
    }

    #[test]
    #[should_panic(expected = "Reason exceeds maximum length of 256 bytes")]
    fn test_stage2c_propose_payment_rejects_more_than_256_utf8_bytes_ascii() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        let over_256_bytes = "a".repeat(257);
        contract.propose_payment(recipient, NearToken::from_near(1), over_256_bytes);
    }

    #[test]
    #[should_panic(expected = "Reason exceeds maximum length of 256 bytes")]
    fn test_stage2c_propose_payment_rejects_more_than_256_utf8_bytes_multibyte() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        // 254 ASCII bytes + 4 bytes emoji = 258 UTF-8 bytes (though only 255 chars)
        let over_256_bytes = "a".repeat(254) + "🦀";
        assert_eq!(over_256_bytes.chars().count(), 255);
        assert_eq!(over_256_bytes.as_bytes().len(), 258);

        contract.propose_payment(recipient, NearToken::from_near(1), over_256_bytes);
    }

    #[test]
    fn test_stage2c_proposal_ids_increment_sequentially() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        let p1 = contract.propose_payment(recipient.clone(), NearToken::from_near(1), "P1".into());
        let p2 = contract.propose_payment(recipient.clone(), NearToken::from_near(1), "P2".into());
        let p3 = contract.propose_payment(recipient, NearToken::from_near(1), "P3".into());

        assert_eq!(p1, 1);
        assert_eq!(p2, 2);
        assert_eq!(p3, 3);
        assert_eq!(contract.get_next_proposal_id(), 4);
    }

    #[test]
    #[should_panic(expected = "Proposal ID overflow")]
    fn test_stage2c_propose_payment_checked_id_overflow_rejection() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        contract.next_proposal_id = u64::MAX;

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());

        contract.propose_payment(recipient, NearToken::from_near(1), "Overflow".into());
    }

    #[test]
    fn test_stage2c_proposal_expiry_is_24_hours_after_creation() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.block_timestamp(5_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(5_000_000_000_000);
        testing_env!(agent_context.build());

        let proposal_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Expiry check".into());

        let prop = contract.get_proposal(proposal_id).unwrap();
        assert_eq!(prop.created_at, 5_000_000_000_000);
        assert_eq!(prop.expires_at, 5_000_000_000_000 + 86_400_000_000_000);
    }

    // ==========================================
    // Stage 2C Unit Tests: Proposal Approval
    // ==========================================

    #[test]
    fn test_stage2c_owner_can_approve_valid_pending_proposal() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Approve test".into());

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        let result = contract.approve_proposal(prop_id);
        assert_eq!(result, ProposalStatus::Approved);

        let prop = contract.get_proposal(prop_id).unwrap();
        assert_eq!(prop.status, ProposalStatus::Approved);
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the contract owner")]
    fn test_stage2c_approve_proposal_fails_for_non_owner() {
        let owner = accounts(1);
        let agent = accounts(2);
        let stranger = accounts(3);
        let recipient = accounts(4);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Approve auth".into());

        let stranger_context = get_context(stranger);
        testing_env!(stranger_context.build());

        contract.approve_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal not found")]
    fn test_stage2c_approve_proposal_fails_for_missing_proposal() {
        let owner = accounts(1);
        let agent = accounts(2);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());
        contract.approve_proposal(999);
    }

    #[test]
    fn test_stage2c_expired_proposal_becomes_persistently_expired_without_panic() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_near(1),
            "Will expire test".into(),
        );

        // Advance timestamp beyond 24 hours (1_000_000_000_000 + 86_400_000_000_000 + 1)
        let mut owner_context = get_context(owner);
        owner_context.block_timestamp(1_000_000_000_000 + 86_400_000_000_000 + 1);
        testing_env!(owner_context.build());

        // Must NOT panic, must return Expired and persist it
        let status = contract.approve_proposal(prop_id);
        assert_eq!(status, ProposalStatus::Expired);

        let prop = contract.get_proposal(prop_id).unwrap();
        assert_eq!(prop.status, ProposalStatus::Expired);
    }

    #[test]
    fn test_stage2c_exact_expiry_boundary_conditions() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient_1 = accounts(3);
        let recipient_2 = accounts(4);

        let created_at = 10_000_000_000_000;
        let expires_at = created_at + 86_400_000_000_000;

        let mut context = get_context(owner.clone());
        context.block_timestamp(created_at);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(created_at);
        testing_env!(agent_context.build());

        let p1 = contract.propose_payment(recipient_1, NearToken::from_near(1), "P1".into());
        let p2 = contract.propose_payment(recipient_2, NearToken::from_near(1), "P2".into());

        // 1. Exactly 1 nanosecond BEFORE expiry -> Approved
        let mut owner_context = get_context(owner.clone());
        owner_context.block_timestamp(expires_at - 1);
        testing_env!(owner_context.build());
        let status_p1 = contract.approve_proposal(p1);
        assert_eq!(status_p1, ProposalStatus::Approved);

        // 2. EXACTLY at expiry boundary (current_timestamp >= expires_at) -> Expired
        let mut owner_context_exact = get_context(owner);
        owner_context_exact.block_timestamp(expires_at);
        testing_env!(owner_context_exact.build());
        let status_p2 = contract.approve_proposal(p2);
        assert_eq!(status_p2, ProposalStatus::Expired);
    }

    #[test]
    #[should_panic(expected = "Cannot approve proposal for a blocked recipient")]
    fn test_stage2c_approve_proposal_fails_if_recipient_becomes_blocked() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient.clone(), NearToken::from_near(1), "Recheck".into());

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        // Owner blocks recipient after proposal was created
        contract.set_recipient_status(recipient, RecipientStatus::Blocked);

        // Attempt to approve must panic
        contract.approve_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal amount exceeds current max amount per transaction policy")]
    fn test_stage2c_approve_proposal_fails_if_amount_exceeds_new_policy_max() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_near(1),
        };
        let mut contract = PayShield::init(owner.clone(), agent.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        // Propose 1.5 NEAR (valid under 2 NEAR max)
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(1_500_000_000_000_000_000_000_000),
            "Policy limit test".into(),
        );

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        // Policy is lowered to 1 NEAR max
        contract.policy.max_amount_per_tx = NearToken::from_near(1);

        // Approval must fail policy recheck
        contract.approve_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Only pending proposals can be approved")]
    fn test_stage2c_approve_proposal_fails_for_invalid_status() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Double approve".into());

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        // First approval succeeds
        contract.approve_proposal(prop_id);

        // Second approval must panic
        contract.approve_proposal(prop_id);
    }

    // ==========================================
    // Stage 2C Unit Tests: Rejection & Revocation
    // ==========================================

    #[test]
    fn test_stage2c_owner_can_reject_pending_proposal() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Reject pending".into());

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        contract.reject_proposal(prop_id);
        let prop = contract.get_proposal(prop_id).unwrap();
        assert_eq!(prop.status, ProposalStatus::Rejected);
    }

    #[test]
    fn test_stage2c_owner_can_revoke_approved_proposal() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Revoke test".into());

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        // First approve
        contract.approve_proposal(prop_id);
        assert_eq!(
            contract.get_proposal(prop_id).unwrap().status,
            ProposalStatus::Approved
        );

        // Revoke before execution
        contract.reject_proposal(prop_id);
        assert_eq!(
            contract.get_proposal(prop_id).unwrap().status,
            ProposalStatus::Rejected
        );
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the contract owner")]
    fn test_stage2c_reject_proposal_fails_for_non_owner() {
        let owner = accounts(1);
        let agent = accounts(2);
        let stranger = accounts(3);
        let recipient = accounts(4);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Reject auth".into());

        let stranger_context = get_context(stranger);
        testing_env!(stranger_context.build());

        contract.reject_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Cannot reject proposal in current status")]
    fn test_stage2c_reject_proposal_fails_for_terminal_or_executing_states() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Terminal state".into());

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        // Reject once -> Rejected
        contract.reject_proposal(prop_id);

        // Attempting to reject again from Rejected state must panic
        contract.reject_proposal(prop_id);
    }

    // ==========================================
    // Stage 2C Unit Tests: Accounting & Scope
    // ==========================================

    #[test]
    fn test_stage2c_propose_and_approve_do_not_alter_daily_spending() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());
        let day_idx = contract.get_current_day_index();

        let initial_spending = contract.get_daily_spending(day_idx);
        assert_eq!(
            initial_spending.confirmed_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            initial_spending.reserved_amount,
            NearToken::from_yoctonear(0)
        );

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id =
            contract.propose_payment(recipient, NearToken::from_near(1), "Zero spending".into());

        // Propose must NOT change daily spending
        let spending_after_propose = contract.get_daily_spending(day_idx);
        assert_eq!(
            spending_after_propose.confirmed_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            spending_after_propose.reserved_amount,
            NearToken::from_yoctonear(0)
        );

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Approve must NOT change daily spending or create reservation
        let spending_after_approve = contract.get_daily_spending(day_idx);
        assert_eq!(
            spending_after_approve.confirmed_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            spending_after_approve.reserved_amount,
            NearToken::from_yoctonear(0)
        );
    }

    // ========================================================
    // Stage 2C Corrections: Specific Non-Rejectable States Tests
    // ========================================================

    #[test]
    #[should_panic(expected = "Cannot reject proposal in current status")]
    fn test_stage2c_reject_proposal_fails_for_expired_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Expired proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Expired,
        };
        contract.proposals.insert(prop_id, proposal);

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        contract.reject_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Cannot reject proposal in current status")]
    fn test_stage2c_reject_proposal_fails_for_executing_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Executing proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Executing,
        };
        contract.proposals.insert(prop_id, proposal);

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        contract.reject_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Cannot reject proposal in current status")]
    fn test_stage2c_reject_proposal_fails_for_executed_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Executed proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Executed,
        };
        contract.proposals.insert(prop_id, proposal);

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        contract.reject_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Cannot reject proposal in current status")]
    fn test_stage2c_reject_proposal_fails_for_failed_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Failed proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Failed,
        };
        contract.proposals.insert(prop_id, proposal);

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());

        contract.reject_proposal(prop_id);
    }

    // ========================================================
    // Stage 2C Corrections: Dynamic Threshold Revalidation Tests
    // ========================================================

    #[test]
    #[should_panic(expected = "Proposal no longer requires approval under current policy")]
    fn test_stage2c_approve_proposal_fails_if_recipient_allowed_and_at_or_below_threshold() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        // 1. Create a proposal while recipient is Unknown (0.4 NEAR <= 0.5 NEAR threshold)
        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000), // 0.4 NEAR
            "Under threshold test".into(),
        );

        // 2. Change recipient to Allowed
        let owner_context = get_context(owner);
        testing_env!(owner_context.build());
        contract.set_recipient_status(recipient, RecipientStatus::Allowed);

        // Proposal is currently Pending
        assert_eq!(
            contract.get_proposal(prop_id).unwrap().status,
            ProposalStatus::Pending
        );

        // 3. Attempt approval as owner -> Must panic
        contract.approve_proposal(prop_id);
    }

    #[test]
    fn test_stage2c_proposal_remains_pending_when_approval_rejected_for_threshold() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000), // 0.4 NEAR
            "Under threshold test".into(),
        );

        let owner_context = get_context(owner);
        testing_env!(owner_context.build());
        contract.set_recipient_status(recipient, RecipientStatus::Allowed);

        // Verify Pending before
        assert_eq!(
            contract.get_proposal(prop_id).unwrap().status,
            ProposalStatus::Pending
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            contract.approve_proposal(prop_id);
        }));
        assert!(
            result.is_err(),
            "Approval must panic when approval is no longer required"
        );

        // Verify Pending after: proposal was not modified or removed
        assert_eq!(
            contract.get_proposal(prop_id).unwrap().status,
            ProposalStatus::Pending
        );
    }

    #[test]
    fn test_stage2c_proposal_approved_when_recipient_becomes_allowed_above_threshold() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let context = get_context(owner.clone());
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        // 1. Create proposal while recipient is Unknown (0.8 NEAR > 0.5 NEAR threshold)
        let mut agent_context = get_context(agent);
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000), // 0.8 NEAR
            "Above threshold test".into(),
        );

        // 2. Change recipient to Allowed
        let owner_context = get_context(owner);
        testing_env!(owner_context.build());
        contract.set_recipient_status(recipient, RecipientStatus::Allowed);

        // 3. Owner approves proposal -> Must succeed because 0.8 NEAR > 0.5 NEAR
        let status = contract.approve_proposal(prop_id);
        assert_eq!(status, ProposalStatus::Approved);
        assert_eq!(
            contract.get_proposal(prop_id).unwrap().status,
            ProposalStatus::Approved
        );
    }

    // ==========================================
    // Stage 2D-A Unit Tests: Authorization
    // ==========================================

    #[test]
    fn test_stage2da_assert_owner_or_agent_succeeds_for_owner() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());
        contract.assert_owner_or_agent();
    }

    #[test]
    fn test_stage2da_assert_owner_or_agent_succeeds_for_agent() {
        let owner = accounts(1);
        let agent = accounts(2);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        let agent_context = get_context(agent);
        testing_env!(agent_context.build());
        contract.assert_owner_or_agent();
    }

    #[test]
    #[should_panic(
        expected = "Unauthorized: caller is neither contract owner nor authorized agent"
    )]
    fn test_stage2da_assert_owner_or_agent_fails_for_stranger() {
        let owner = accounts(1);
        let agent = accounts(2);
        let stranger = accounts(3);
        let context = get_context(owner.clone());
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());
        let stranger_context = get_context(stranger);
        testing_env!(stranger_context.build());
        contract.assert_owner_or_agent();
    }

    // ==========================================
    // Stage 2D-A Unit Tests: Preflight Execution
    // ==========================================

    #[test]
    fn test_stage2da_preflight_succeeds_for_owner() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        // Agent creates proposal for 0.8 NEAR
        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Preflight test".into(),
        );

        // Owner approves proposal
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        let status = contract.approve_proposal(prop_id);
        assert_eq!(status, ProposalStatus::Approved);

        // Preflight as owner
        let outcome = contract.preflight_proposal_execution(prop_id);
        assert_eq!(
            outcome,
            ExecutionPreflightOutcome::Ready {
                proposal_id: prop_id,
                recipient,
                amount: NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
                accounting_day: calculate_day_index(1_000_000_000_000),
            }
        );
    }

    #[test]
    fn test_stage2da_preflight_succeeds_for_agent() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent.clone());
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Agent preflight test".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Preflight as agent
        testing_env!(agent_context.build());
        let outcome = contract.preflight_proposal_execution(prop_id);
        assert_eq!(
            outcome,
            ExecutionPreflightOutcome::Ready {
                proposal_id: prop_id,
                recipient,
                amount: NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
                accounting_day: calculate_day_index(1_000_000_000_000),
            }
        );
    }

    #[test]
    #[should_panic(
        expected = "Unauthorized: caller is neither contract owner nor authorized agent"
    )]
    fn test_stage2da_preflight_fails_for_unauthorized_caller() {
        let owner = accounts(1);
        let agent = accounts(2);
        let stranger = accounts(3);
        let recipient = accounts(4);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Unauthorized preflight".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Caller is stranger
        let mut stranger_context = get_context(stranger);
        stranger_context.account_balance(NearToken::from_near(10));
        stranger_context.block_timestamp(1_000_000_000_000);
        testing_env!(stranger_context.build());

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2da_preflight_fails_for_pending_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Pending test".into(),
        );

        // Preflight without approval -> must panic
        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2da_preflight_fails_for_rejected_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Reject preflight".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.reject_proposal(prop_id);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2da_preflight_fails_for_executing_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Executing proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Executing,
        };
        contract.proposals.insert(prop_id, proposal);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2da_preflight_fails_for_executed_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Executed proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Executed,
        };
        contract.proposals.insert(prop_id, proposal);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2da_preflight_fails_for_failed_state() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());

        let prop_id = 1;
        let proposal = PaymentProposal {
            proposal_id: prop_id,
            recipient,
            amount: NearToken::from_near(1),
            reason: "Failed proposal".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Failed,
        };
        contract.proposals.insert(prop_id, proposal);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal not found")]
    fn test_stage2da_preflight_fails_for_missing_proposal() {
        let owner = accounts(1);
        let agent = accounts(2);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        testing_env!(context.build());

        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());
        contract.preflight_proposal_execution(999);
    }

    #[test]
    fn test_stage2da_expired_proposal_persists_expired_without_panic() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let created_at = 1_000_000_000_000;
        let expires_at = created_at + PROPOSAL_VALIDITY_NANOS;

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(created_at);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(created_at);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Expiry preflight test".into(),
        );

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(created_at);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Advance timestamp past 24 hours
        let mut exec_context = get_context(owner);
        exec_context.account_balance(NearToken::from_near(10));
        exec_context.block_timestamp(expires_at + 1);
        testing_env!(exec_context.build());

        // Preflight must NOT panic, must return Expired and persist ProposalStatus::Expired
        let outcome = contract.preflight_proposal_execution(prop_id);
        assert_eq!(outcome, ExecutionPreflightOutcome::Expired);

        let stored_proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(stored_proposal.status, ProposalStatus::Expired);
    }

    #[test]
    fn test_stage2da_exact_boundary_preflight_expiry() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient_1 = accounts(3);
        let recipient_2 = accounts(4);

        let created_at = 10_000_000_000_000;
        let expires_at = created_at + PROPOSAL_VALIDITY_NANOS;

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(created_at);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(created_at);
        testing_env!(agent_context.build());
        let p1 = contract.propose_payment(
            recipient_1.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "P1".into(),
        );
        let p2 = contract.propose_payment(
            recipient_2,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "P2".into(),
        );

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(created_at);
        testing_env!(owner_context.build());
        contract.approve_proposal(p1);
        contract.approve_proposal(p2);

        // 1. Exactly 1 nanosecond BEFORE expiry -> Ready
        let mut exec_context_p1 = get_context(owner.clone());
        exec_context_p1.account_balance(NearToken::from_near(10));
        exec_context_p1.block_timestamp(expires_at - 1);
        testing_env!(exec_context_p1.build());
        let outcome_p1 = contract.preflight_proposal_execution(p1);
        assert_eq!(
            outcome_p1,
            ExecutionPreflightOutcome::Ready {
                proposal_id: p1,
                recipient: recipient_1,
                amount: NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
                accounting_day: calculate_day_index(expires_at - 1),
            }
        );

        // 2. EXACTLY at expiry boundary -> Expired
        let mut exec_context_p2 = get_context(owner);
        exec_context_p2.account_balance(NearToken::from_near(10));
        exec_context_p2.block_timestamp(expires_at);
        testing_env!(exec_context_p2.build());
        let outcome_p2 = contract.preflight_proposal_execution(p2);
        assert_eq!(outcome_p2, ExecutionPreflightOutcome::Expired);
    }

    #[test]
    #[should_panic(expected = "Cannot execute proposal for a blocked recipient")]
    fn test_stage2da_preflight_fails_when_recipient_becomes_blocked() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Blocked recipient test".into(),
        );

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Block recipient after approval
        contract.set_recipient_status(recipient, RecipientStatus::Blocked);

        // Preflight must reject execution for blocked recipient
        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal amount exceeds current max amount per transaction policy")]
    fn test_stage2da_preflight_fails_when_amount_exceeds_new_policy_max() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_near(1),
        };
        let mut contract = PayShield::init(owner.clone(), agent.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        // Propose 1.5 NEAR (valid under 2 NEAR initial max)
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(1_500_000_000_000_000_000_000_000),
            "Max tx change test".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Policy is lowered to 1 NEAR max
        contract.policy.max_amount_per_tx = NearToken::from_near(1);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal amount would exceed daily spending limit")]
    fn test_stage2da_preflight_fails_when_daily_spending_limit_exceeded() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        // Default daily spending limit is 5 NEAR
        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_near(1), // 1 NEAR proposal
            "Daily limit test".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Simulate existing spending already at 4.5 NEAR (4.5 + 1.0 = 5.5 > 5.0 NEAR limit)
        let day_idx = contract.get_current_day_index();
        let mut day_usage = DailySpendingUsage::new(day_idx);
        day_usage.confirmed_amount = NearToken::from_yoctonear(4_500_000_000_000_000_000_000_000); // 4.5 NEAR
        contract.daily_spending.insert(day_idx, day_usage);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Daily spending allocation overflow")]
    fn test_stage2da_preflight_prospective_daily_spending_overflow_rejection() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Overflow test".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Simulate u128::MAX allocated spending
        let day_idx = contract.get_current_day_index();
        let mut day_usage = DailySpendingUsage::new(day_idx);
        day_usage.confirmed_amount = NearToken::from_yoctonear(u128::MAX);
        contract.daily_spending.insert(day_idx, day_usage);

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    #[should_panic(expected = "Payment ID counter overflow")]
    fn test_stage2da_preflight_checked_payment_id_overflow_rejection() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient,
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Counter overflow test".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        // Simulate next_payment_id at u64::MAX
        contract.next_payment_id = u64::MAX;

        contract.preflight_proposal_execution(prop_id);
    }

    #[test]
    fn test_stage2da_preflight_does_not_mutate_spending_or_proposal_or_payments() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(1_000_000_000_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(1_000_000_000_000);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Invariance test".into(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(1_000_000_000_000);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        let day_idx = contract.get_current_day_index();
        let spending_before = contract.get_daily_spending(day_idx);
        assert_eq!(
            spending_before.total_allocated(),
            NearToken::from_yoctonear(0)
        );

        // Run preflight
        let outcome = contract.preflight_proposal_execution(prop_id);
        assert_eq!(
            outcome,
            ExecutionPreflightOutcome::Ready {
                proposal_id: prop_id,
                recipient,
                amount: NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
                accounting_day: day_idx,
            }
        );

        // Invariants must strictly hold:
        // 1. Proposal status remains Approved (NOT Executing)
        let prop = contract.get_proposal(prop_id).unwrap();
        assert_eq!(prop.status, ProposalStatus::Approved);

        // 2. Daily spending reserved amount remains 0 (NOT reserved)
        let spending_after = contract.get_daily_spending(day_idx);
        assert_eq!(spending_after.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(
            spending_after.confirmed_amount,
            NearToken::from_yoctonear(0)
        );

        // 3. No payment record created
        assert_eq!(contract.get_payment(1), None);
        assert_eq!(contract.get_next_payment_id(), 1);
    }

    // ==========================================
    // Stage 2D-A Unit Tests: Storage & Balance
    // ==========================================

    #[test]
    fn test_stage2da_compute_storage_shortfall_when_cost_exceeds_locked() {
        let storage_cost = NearToken::from_near(5);
        let locked_balance = NearToken::from_near(3);
        let shortfall = compute_storage_shortfall(storage_cost, locked_balance);
        assert_eq!(shortfall, NearToken::from_near(2));
    }

    #[test]
    fn test_stage2da_compute_storage_shortfall_when_locked_exceeds_cost() {
        let storage_cost = NearToken::from_near(2);
        let locked_balance = NearToken::from_near(5);
        let shortfall = compute_storage_shortfall(storage_cost, locked_balance);
        assert_eq!(shortfall, NearToken::from_yoctonear(0));
    }

    #[test]
    fn test_stage2da_calculate_required_liquid_balance_with_buffer() {
        // Storage cost 5 NEAR, locked 3 NEAR -> Shortfall 2 NEAR
        // Transfer amount: 1 NEAR
        // Buffer: 0.02 NEAR
        // Required: 2 + 1 + 0.02 = 3.02 NEAR
        let storage_cost = NearToken::from_near(5);
        let locked_balance = NearToken::from_near(3);
        let transfer_amount = NearToken::from_near(1);

        let required =
            calculate_required_liquid_balance(storage_cost, locked_balance, transfer_amount);
        let expected = NearToken::from_yoctonear(3_020_000_000_000_000_000_000_000);
        assert_eq!(required, expected);
    }

    #[test]
    fn test_stage2da_check_liquid_balance_safety_passes_with_sufficient_balance() {
        let owner = accounts(1);
        let agent = accounts(2);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());
        contract.check_liquid_balance_safety(NearToken::from_near(1));
    }

    #[test]
    #[should_panic(
        expected = "Insufficient available balance for transfer, storage backing, and safety buffer"
    )]
    fn test_stage2da_check_liquid_balance_safety_fails_with_insufficient_balance() {
        let owner = accounts(1);
        let agent = accounts(2);

        let mut context = get_context(owner.clone());
        // Set account balance to 0.01 NEAR (lower than 0.02 NEAR buffer alone)
        context.account_balance(NearToken::from_millinear(10));
        testing_env!(context.build());

        let contract = PayShield::init(owner, agent, PaymentPolicy::default());
        contract.check_liquid_balance_safety(NearToken::from_near(1));
    }

    #[test]
    fn test_stage2da_locked_balance_contributes_without_double_counting() {
        // When staking collateral covers storage cost:
        // storage_cost = 10 NEAR, locked = 10 NEAR -> shortfall = 0
        // transfer = 2 NEAR, buffer = 0.02 NEAR -> required = 2.02 NEAR
        let storage_cost = NearToken::from_near(10);
        let locked = NearToken::from_near(10);
        let transfer = NearToken::from_near(2);

        let required = calculate_required_liquid_balance(storage_cost, locked, transfer);
        assert_eq!(
            required,
            NearToken::from_yoctonear(2_020_000_000_000_000_000_000_000)
        );
    }

    #[test]
    fn test_stage2da_prospective_daily_spending_helper() {
        let mut usage = DailySpendingUsage::new(5);
        usage.add_reservation(NearToken::from_near(1));

        let prospective = usage.prospective_total_allocated(NearToken::from_near(2));
        assert_eq!(prospective, NearToken::from_near(3));

        // Original record remains unmutated
        assert_eq!(usage.total_allocated(), NearToken::from_near(1));
        assert_eq!(usage.reserved_amount, NearToken::from_near(1));
        assert_eq!(usage.confirmed_amount, NearToken::from_near(0));
    }

    #[test]
    fn test_stage2da_storage_growth_measurement_isolated() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(100));
        context.storage_usage(10_000);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent, PaymentPolicy::default());
        contract.flush_storage_maps();
        let usage_init = env::storage_usage();

        // 1. Measure proposal insertion and flush
        let mut proposal = PaymentProposal {
            proposal_id: 1,
            recipient: recipient.clone(),
            amount: NearToken::from_near(1),
            reason: "Isolated storage measurement".into(),
            created_at: 100,
            expires_at: 200,
            status: ProposalStatus::Approved,
        };
        contract.proposals.insert(1, proposal.clone());
        contract.flush_storage_maps();
        let usage_after_proposal = env::storage_usage();

        // 2. Measure daily spending insertion and flush
        let mut daily_usage = DailySpendingUsage::new(10);
        daily_usage.add_reservation(NearToken::from_near(1));
        contract.daily_spending.insert(10, daily_usage.clone());
        contract.flush_storage_maps();
        let usage_after_daily = env::storage_usage();

        // 3. Measure payment record insertion and flush
        let payment_record = PaymentRecord {
            payment_id: 1,
            recipient: recipient.clone(),
            amount: NearToken::from_near(1),
            accounting_day: 10,
            proposal_id: Some(1),
            initiated_at: 100,
            settled_at: None,
            status: PaymentStatus::InFlight,
            settlement_source: None,
            reconciliation_evidence: None,
        };
        contract.payments.insert(1, payment_record.clone());
        contract.flush_storage_maps();
        let usage_after_payment = env::storage_usage();

        // 4. Measure proposal update and flush
        proposal.status = ProposalStatus::Executing;
        contract.proposals.insert(1, proposal.clone());
        contract.flush_storage_maps();
        let usage_after_update = env::storage_usage();

        // Also measure exact serialized payload sizes using borsh
        let proposal_serialized = near_sdk::borsh::to_vec(&proposal).unwrap();
        let daily_serialized = near_sdk::borsh::to_vec(&daily_usage).unwrap();
        let payment_serialized = near_sdk::borsh::to_vec(&payment_record).unwrap();

        // Maximum size with a 256-byte UTF-8 evidence reference and owner reconciliation
        let max_evidence = "a".repeat(256);
        let max_reconciled_payment = PaymentRecord {
            payment_id: 1,
            recipient,
            amount: NearToken::from_near(1),
            accounting_day: 10,
            proposal_id: Some(1),
            initiated_at: 100,
            settled_at: Some(200),
            status: PaymentStatus::Succeeded,
            settlement_source: Some(SettlementSource::OwnerReconciliation),
            reconciliation_evidence: Some(max_evidence),
        };
        let max_reconciled_serialized = near_sdk::borsh::to_vec(&max_reconciled_payment).unwrap();

        eprintln!(
            "MEASURED_SIZES: proposal_bytes={}, daily_bytes={}, payment_bytes={}, max_reconciled_bytes={}, usage_init={}, usage_proposal={}, usage_daily={}, usage_payment={}, usage_update={}",
            proposal_serialized.len(),
            daily_serialized.len(),
            payment_serialized.len(),
            max_reconciled_serialized.len(),
            usage_init,
            usage_after_proposal,
            usage_after_daily,
            usage_after_payment,
            usage_after_update,
        );

        assert!(!proposal_serialized.is_empty());
        assert!(!daily_serialized.is_empty());
        assert!(!payment_serialized.is_empty());
        assert!(max_reconciled_serialized.len() > payment_serialized.len());

        // Assert storage tracking is non-decreasing
        assert!(usage_after_proposal >= usage_init);
        assert!(usage_after_daily >= usage_after_proposal);
        assert!(usage_after_payment >= usage_after_daily);
        assert!(usage_after_update >= usage_after_payment);
    }

    // ========================================================
    // Stage 2D-B2/B3 Unit Tests: Execution and Settlement
    // ========================================================

    fn setup_approved_proposal(
        owner: AccountId,
        agent: AccountId,
        recipient: AccountId,
        amount: NearToken,
        timestamp: Timestamp,
    ) -> (PayShield, u64) {
        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(timestamp);
        testing_env!(context.build());

        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(recipient, amount, "Execution test".into());

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        (contract, prop_id)
    }

    fn testing_env_with_promise_result(context: near_sdk::VMContext, result: PromiseResult) {
        #[allow(deprecated)]
        testing_env_with_promise_results(context, result);
    }

    #[test]
    fn test_stage2db_execute_proposal_as_owner_succeeds() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient.clone(), amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());

        let outcome = contract.execute_proposal(prop_id);
        assert_eq!(outcome, ExecutionOutcome::Executed { payment_id: 1 });

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Executing);

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.payment_id, 1);
        assert_eq!(payment.recipient, recipient);
        assert_eq!(payment.amount, amount);
        assert_eq!(payment.proposal_id, Some(prop_id));
        assert_eq!(payment.status, PaymentStatus::InFlight);
        assert_eq!(payment.settled_at, None);
        assert_eq!(payment.settlement_source, None);
        assert_eq!(payment.reconciliation_evidence, None);
        assert_eq!(payment.accounting_day, calculate_day_index(timestamp));

        let day_usage = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(day_usage.reserved_amount, amount);
        assert_eq!(day_usage.confirmed_amount, NearToken::from_yoctonear(0));
        assert_eq!(contract.next_payment_id, 2);

        let receipts = get_created_receipts();
        assert!(
            !receipts.is_empty(),
            "Expected created receipts for promise transfer and callback"
        );
    }

    #[test]
    fn test_stage2db_execute_proposal_as_agent_succeeds() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner, agent.clone(), recipient.clone(), amount, timestamp);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let outcome = contract.execute_proposal(prop_id);
        assert_eq!(outcome, ExecutionOutcome::Executed { payment_id: 1 });

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Executing);

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.payment_id, 1);
        assert_eq!(payment.recipient, recipient);
        assert_eq!(payment.status, PaymentStatus::InFlight);
    }

    #[test]
    #[should_panic(
        expected = "Unauthorized: caller is neither contract owner nor authorized agent"
    )]
    fn test_stage2db_execute_proposal_unauthorized_fails() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let stranger = accounts(4);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner, agent, recipient, amount, timestamp);

        let mut stranger_context = get_context(stranger);
        stranger_context.account_balance(NearToken::from_near(10));
        testing_env!(stranger_context.build());

        contract.execute_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2db_execute_proposal_non_approved_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        context.block_timestamp(timestamp);
        testing_env!(context.build());
        let mut contract = PayShield::init(owner.clone(), agent.clone(), PaymentPolicy::default());

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());
        let prop_id = contract.propose_payment(recipient, amount, "Execution test".into());

        // Attempt execution while still Pending (not Approved)
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());

        contract.execute_proposal(prop_id);
    }

    #[test]
    fn test_stage2db_execute_proposal_expired_persists_and_returns_expired() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp + PROPOSAL_VALIDITY_NANOS);
        testing_env!(owner_context.build());

        let outcome = contract.execute_proposal(prop_id);
        assert_eq!(outcome, ExecutionOutcome::Expired);

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Expired);
        assert_eq!(contract.next_payment_id, 1);
        assert_eq!(contract.get_payment(1), None);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in approved status for execution")]
    fn test_stage2db_execute_proposal_duplicate_execution_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());

        let outcome = contract.execute_proposal(prop_id);
        assert_eq!(outcome, ExecutionOutcome::Executed { payment_id: 1 });

        // Second execution attempt must panic
        contract.execute_proposal(prop_id);
    }

    #[test]
    #[should_panic(expected = "Payment ID counter overflow")]
    fn test_stage2db_execute_proposal_payment_id_overflow_rejection() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        contract.next_payment_id = u64::MAX;

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());

        contract.execute_proposal(prop_id);
    }

    #[test]
    #[should_panic(
        expected = "Insufficient available balance for transfer, storage backing, and safety buffer"
    )]
    fn test_stage2db_execute_proposal_fails_if_balance_insufficient() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_yoctonear(100)); // Insufficient balance
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());

        contract.execute_proposal(prop_id);
    }

    #[test]
    fn test_stage2db_callback_successful_transfer_settlement() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let settlement_time = timestamp + 5_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.settled_at, Some(settlement_time));
        assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Executed);

        let usage = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(usage.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(usage.confirmed_amount, amount);
    }

    #[test]
    fn test_stage2db_callback_failed_transfer_settlement() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let settlement_time = timestamp + 5_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(callback_context.build(), PromiseResult::Failed);

        contract.on_payment_completed(1);

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.settled_at, Some(settlement_time));
        assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Failed);

        let usage = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(usage.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(usage.confirmed_amount, NearToken::from_yoctonear(0));
    }

    #[test]
    #[should_panic(expected = "Method is private")]
    fn test_stage2db_callback_private_guard_rejects_external_callers() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        // Predecessor is owner (accounts(1)), not current contract (accounts(0))
        contract.on_payment_completed(1);
    }

    #[test]
    #[should_panic(expected = "Expected exactly one promise result")]
    fn test_stage2db_callback_rejects_incorrect_promise_results_count() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        testing_env!(
            callback_context.build(),
            near_sdk::test_vm_config(),
            near_sdk::RuntimeFeesConfig::test(),
            Default::default(),
            vec![],
        );

        contract.on_payment_completed(1);
    }

    #[test]
    #[should_panic(expected = "Payment not found")]
    fn test_stage2db_callback_rejects_missing_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let contract_id = accounts(0);

        let mut context = get_context(owner.clone());
        context.account_balance(NearToken::from_near(10));
        testing_env!(context.build());
        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());

        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(999);
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2db_callback_rejects_payment_not_in_flight() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);

        // Second callback on settled payment must panic
        contract.on_payment_completed(1);
    }

    #[test]
    #[should_panic(expected = "Proposal is not in executing status")]
    fn test_stage2db_callback_rejects_proposal_not_executing() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        // Tamper proposal status
        let mut proposal = contract.get_proposal(prop_id).unwrap();
        proposal.status = ProposalStatus::Approved;
        contract.proposals.insert(prop_id, proposal);

        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);
    }

    #[test]
    fn test_stage2db_callback_preserves_accounting_day_across_midnight() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);

        let day5_timestamp = 5 * NANOS_PER_DAY + 3_600_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, day5_timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(day5_timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        assert_eq!(contract.get_daily_spending(5).reserved_amount, amount);
        assert_eq!(
            contract.get_daily_spending(6).reserved_amount,
            NearToken::from_yoctonear(0)
        );

        // Day 6 settlement timestamp (24 hours later)
        let day6_timestamp = 6 * NANOS_PER_DAY + 3_600_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(day6_timestamp);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);

        // Invariant: confirmed_amount belongs to day 5, not day 6
        assert_eq!(
            contract.get_daily_spending(5).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(contract.get_daily_spending(5).confirmed_amount, amount);
        assert_eq!(
            contract.get_daily_spending(6).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            contract.get_daily_spending(6).confirmed_amount,
            NearToken::from_yoctonear(0)
        );
    }

    #[test]
    fn test_stage2db4_reconcile_as_owner_success_succeeds() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        let evidence = "https://nearblocks.io/txns/sample_tx_hash_success".to_string();
        contract.reconcile_stuck_payment(1, true, evidence.clone());

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.settled_at, Some(reconcile_time));
        assert_eq!(
            payment.settlement_source,
            Some(SettlementSource::OwnerReconciliation)
        );
        assert_eq!(payment.reconciliation_evidence, Some(evidence));

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Executed);

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, amount);
    }

    #[test]
    fn test_stage2db4_reconcile_as_owner_failure_succeeds() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 5_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        let evidence = "https://nearblocks.io/txns/sample_tx_hash_failed".to_string();
        contract.reconcile_stuck_payment(1, false, evidence.clone());

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.settled_at, Some(reconcile_time));
        assert_eq!(
            payment.settlement_source,
            Some(SettlementSource::OwnerReconciliation)
        );
        assert_eq!(payment.reconciliation_evidence, Some(evidence));

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Failed);

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the contract owner")]
    fn test_stage2db4_reconcile_rejects_agent() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent.clone(), recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(reconcile_time);
        testing_env!(agent_context.build());

        contract.reconcile_stuck_payment(1, true, "tx_hash_proof".into());
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the contract owner")]
    fn test_stage2db4_reconcile_rejects_arbitrary_caller() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let stranger = accounts(4);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut stranger_context = get_context(stranger);
        stranger_context.account_balance(NearToken::from_near(10));
        stranger_context.block_timestamp(reconcile_time);
        testing_env!(stranger_context.build());

        contract.reconcile_stuck_payment(1, true, "tx_hash_proof".into());
    }

    #[test]
    #[should_panic(expected = "Reconciliation evidence reference cannot be empty or whitespace")]
    fn test_stage2db4_reconcile_rejects_empty_evidence() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "".to_string());
    }

    #[test]
    #[should_panic(expected = "Reconciliation evidence reference cannot be empty or whitespace")]
    fn test_stage2db4_reconcile_rejects_whitespace_evidence() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "   \t\n   ".to_string());
    }

    #[test]
    #[should_panic(expected = "Reconciliation evidence reference exceeds 256 UTF-8 bytes")]
    fn test_stage2db4_reconcile_rejects_oversized_evidence() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        let oversized_evidence = "a".repeat(257);
        contract.reconcile_stuck_payment(1, true, oversized_evidence);
    }

    #[test]
    fn test_stage2db4_reconcile_accepts_max_evidence_256_bytes() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        let max_evidence = "a".repeat(256);
        contract.reconcile_stuck_payment(1, true, max_evidence.clone());

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.reconciliation_evidence, Some(max_evidence));
    }

    #[test]
    fn test_stage2db4_reconcile_accepts_multibyte_utf8_evidence() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        let crab_evidence = "🦀".repeat(64);
        assert_eq!(crab_evidence.as_bytes().len(), 256);
        contract.reconcile_stuck_payment(1, true, crab_evidence.clone());

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.reconciliation_evidence, Some(crab_evidence));
    }

    #[test]
    #[should_panic(expected = "Payment not found")]
    fn test_stage2db4_reconcile_rejects_nonexistent_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, _prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(999, true, "tx_hash_proof".into());
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2db4_reconcile_rejects_already_succeeded_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "first_reconciliation".into());
        // Second reconciliation on already succeeded payment must panic
        contract.reconcile_stuck_payment(1, true, "second_reconciliation".into());
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2db4_reconcile_rejects_already_failed_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, false, "failed_tx_proof".into());
        // Second reconciliation on already failed payment must panic
        contract.reconcile_stuck_payment(1, false, "duplicate_proof".into());
    }

    #[test]
    #[should_panic(expected = "Proposal is not in executing status")]
    fn test_stage2db4_reconcile_rejects_proposal_not_executing() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        // Tamper proposal status
        let mut proposal = contract.get_proposal(prop_id).unwrap();
        proposal.status = ProposalStatus::Approved;
        contract.proposals.insert(prop_id, proposal);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "tx_hash_proof".into());
    }

    #[test]
    #[should_panic(expected = "Settlement grace period has not elapsed")]
    fn test_stage2db4_reconcile_rejects_premature_before_timeout() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        // 1 nanosecond before timeout expires
        let premature_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT - 1;
        let mut premature_context = get_context(owner);
        premature_context.account_balance(NearToken::from_near(10));
        premature_context.block_timestamp(premature_time);
        testing_env!(premature_context.build());

        contract.reconcile_stuck_payment(1, true, "tx_hash_proof".into());
    }

    #[test]
    fn test_stage2db4_reconcile_accepts_exact_timeout_boundary() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let exact_boundary_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(exact_boundary_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "exact_boundary_proof".into());

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.settled_at, Some(exact_boundary_time));
    }

    #[test]
    fn test_stage2db4_reconcile_accepts_well_past_timeout() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let later_time = timestamp + 10 * 3_600_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(later_time);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "well_past_timeout_proof".into());

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.settled_at, Some(later_time));
    }

    #[test]
    fn test_stage2db4_reconcile_preserves_accounting_day_across_midnight() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);

        let day5_timestamp = 5 * NANOS_PER_DAY + 3_600_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, day5_timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(day5_timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        assert_eq!(contract.get_daily_spending(5).reserved_amount, amount);
        assert_eq!(
            contract.get_daily_spending(6).reserved_amount,
            NearToken::from_yoctonear(0)
        );

        let day6_timestamp = 6 * NANOS_PER_DAY + 3_600_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(day6_timestamp);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, true, "midnight_proof".into());

        assert_eq!(
            contract.get_daily_spending(5).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(contract.get_daily_spending(5).confirmed_amount, amount);
        assert_eq!(
            contract.get_daily_spending(6).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            contract.get_daily_spending(6).confirmed_amount,
            NearToken::from_yoctonear(0)
        );
    }

    #[test]
    fn test_stage2db4_reconcile_releases_reservation_across_midnight() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);

        let day5_timestamp = 5 * NANOS_PER_DAY + 3_600_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, day5_timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(day5_timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        assert_eq!(contract.get_daily_spending(5).reserved_amount, amount);

        let day6_timestamp = 6 * NANOS_PER_DAY + 3_600_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(day6_timestamp);
        testing_env!(reconcile_context.build());

        contract.reconcile_stuck_payment(1, false, "failed_midnight_proof".into());

        assert_eq!(
            contract.get_daily_spending(5).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            contract.get_daily_spending(5).confirmed_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            contract.get_daily_spending(6).reserved_amount,
            NearToken::from_yoctonear(0)
        );
        assert_eq!(
            contract.get_daily_spending(6).confirmed_amount,
            NearToken::from_yoctonear(0)
        );
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2db4_late_callback_rejected_after_reconciliation_success() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());
        contract.reconcile_stuck_payment(1, true, "early_reconciliation_proof".into());

        let callback_time = reconcile_time + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(callback_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2db4_late_callback_rejected_after_reconciliation_failure() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());
        contract.reconcile_stuck_payment(1, false, "early_reconciliation_failed_proof".into());

        let callback_time = reconcile_time + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(callback_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);
    }

    #[test]
    fn test_stage2db4_reconcile_does_not_dispatch_promise() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner.clone());
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let execution_receipts = get_created_receipts().len();
        assert!(execution_receipts > 0, "Execution should create receipts");

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut reconcile_context = get_context(owner);
        reconcile_context.account_balance(NearToken::from_near(10));
        reconcile_context.block_timestamp(reconcile_time);
        testing_env!(reconcile_context.build());

        let receipts_before_reconciliation = get_created_receipts().len();
        assert_eq!(
            receipts_before_reconciliation, 0,
            "Fresh invocation context should have 0 receipts"
        );

        contract.reconcile_stuck_payment(1, true, "no_dispatch_proof".into());

        let receipts_after_reconciliation = get_created_receipts().len();
        assert_eq!(
            receipts_after_reconciliation, 0,
            "Reconciliation must not dispatch any promises or create new receipts"
        );
    }

    fn setup_inflight_direct_payment(
        owner: AccountId,
        agent: AccountId,
        recipient: AccountId,
        amount: NearToken,
        timestamp: Timestamp,
    ) -> (PayShield, u64) {
        let mut contract = PayShield::init(owner, agent, PaymentPolicy::default());
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

        (contract, payment_id)
    }

    #[test]
    fn test_stage2e_step1_callback_successful_direct_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner, agent, recipient.clone(), amount, timestamp);

        let settlement_time = timestamp + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(payment_id);

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.settled_at, Some(settlement_time));
        assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));
        assert_eq!(payment.reconciliation_evidence, None);

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, amount);

        assert_eq!(contract.get_proposal(1), None);
    }

    #[test]
    fn test_stage2e_step1_callback_failed_direct_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner, agent, recipient, amount, timestamp);

        let settlement_time = timestamp + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(callback_context.build(), PromiseResult::Failed);

        contract.on_payment_completed(payment_id);

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.settled_at, Some(settlement_time));
        assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));
        assert_eq!(payment.reconciliation_evidence, None);

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));

        assert_eq!(contract.get_proposal(1), None);
    }

    #[test]
    fn test_stage2e_step1_reconcile_direct_payment_success() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner.clone(), agent, recipient, amount, timestamp);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        let evidence = "https://nearblocks.io/txns/sample_direct_success".to_string();
        contract.reconcile_stuck_payment(payment_id, true, evidence.clone());

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.settled_at, Some(reconcile_time));
        assert_eq!(
            payment.settlement_source,
            Some(SettlementSource::OwnerReconciliation)
        );
        assert_eq!(payment.reconciliation_evidence, Some(evidence));

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, amount);

        assert_eq!(contract.get_proposal(1), None);
    }

    #[test]
    fn test_stage2e_step1_reconcile_direct_payment_failure() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner.clone(), agent, recipient, amount, timestamp);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        let evidence = "https://nearblocks.io/txns/sample_direct_failure".to_string();
        contract.reconcile_stuck_payment(payment_id, false, evidence.clone());

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.settled_at, Some(reconcile_time));
        assert_eq!(
            payment.settlement_source,
            Some(SettlementSource::OwnerReconciliation)
        );
        assert_eq!(payment.reconciliation_evidence, Some(evidence));

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));

        assert_eq!(contract.get_proposal(1), None);
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2e_step1_reconcile_rejected_after_callback_settlement() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner.clone(), agent, recipient, amount, timestamp);

        let settlement_time = timestamp + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(payment_id);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        contract.reconcile_stuck_payment(payment_id, true, "duplicate_evidence".into());
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2e_step1_late_callback_rejected_after_successful_reconciliation() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner.clone(), agent, recipient, amount, timestamp);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        contract.reconcile_stuck_payment(payment_id, true, "early_reconciliation".into());

        let callback_time = reconcile_time + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(callback_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(payment_id);
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2e_step1_late_callback_rejected_after_failed_reconciliation() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, payment_id) =
            setup_inflight_direct_payment(owner.clone(), agent, recipient, amount, timestamp);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        contract.reconcile_stuck_payment(payment_id, false, "failed_reconciliation".into());

        let callback_time = reconcile_time + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(callback_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(payment_id);
    }

    #[test]
    fn test_stage2e_step1_proposal_backed_callback_and_reconciliation_unchanged() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(800_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let (mut contract, prop_id) =
            setup_approved_proposal(owner.clone(), agent, recipient, amount, timestamp);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.execute_proposal(prop_id);

        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(timestamp + 5_000_000_000);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.proposal_id, Some(prop_id));

        let proposal = contract.get_proposal(prop_id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Executed);
    }

    fn setup_direct_payment_contract(
        owner: AccountId,
        agent: AccountId,
        recipient: AccountId,
        policy: PaymentPolicy,
    ) -> PayShield {
        let owner_context = get_context(owner.clone());
        testing_env!(owner_context.build());
        let mut contract = PayShield::init(owner, agent, policy);
        contract.set_recipient_status(recipient, RecipientStatus::Allowed);
        contract
    }

    #[test]
    fn test_stage2e_step2_agent_initiates_valid_direct_payment() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000); // 0.4 NEAR <= 0.5 threshold
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient.clone(), amount);
        assert_eq!(payment_id, 1);

        let payment = contract.get_payment(1).unwrap();
        assert_eq!(payment.payment_id, 1);
        assert_eq!(payment.recipient, recipient);
        assert_eq!(payment.amount, amount);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.status, PaymentStatus::InFlight);
        assert_eq!(payment.initiated_at, timestamp);
        assert_eq!(payment.settled_at, None);
        assert_eq!(payment.settlement_source, None);
        assert_eq!(payment.reconciliation_evidence, None);

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, amount);
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the authorized agent")]
    fn test_stage2e_step2_owner_without_agent_authorization_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner.clone(),
            agent,
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        testing_env!(owner_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
    }

    #[test]
    #[should_panic(expected = "Unauthorized: caller is not the authorized agent")]
    fn test_stage2e_step2_unauthorized_stranger_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let stranger = accounts(4);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent,
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut stranger_context = get_context(stranger);
        stranger_context.account_balance(NearToken::from_near(10));
        testing_env!(stranger_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
    }

    #[test]
    #[should_panic(expected = "Cannot make direct payment to a blocked recipient")]
    fn test_stage2e_step2_blocked_recipient_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let owner_context = get_context(owner.clone());
        testing_env!(owner_context.build());
        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        contract.set_recipient_status(recipient.clone(), RecipientStatus::Blocked);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
    }

    #[test]
    #[should_panic(expected = "Direct payments are only permitted for allowed recipients")]
    fn test_stage2e_step2_unknown_recipient_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = PayShield::init(owner, agent.clone(), PaymentPolicy::default());
        // recipient left as Unknown

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
    }

    #[test]
    #[should_panic(expected = "Payment amount must be greater than zero")]
    fn test_stage2e_step2_zero_amount_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(recipient, NearToken::from_yoctonear(0));
    }

    #[test]
    fn test_stage2e_step2_positive_below_threshold_accepted() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(300_000_000_000_000_000_000_000), // 0.3 NEAR < 0.5 NEAR
        );
        assert_eq!(payment_id, 1);
    }

    #[test]
    fn test_stage2e_step2_exact_threshold_accepted() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        // Exactly 0.5 NEAR = default require_approval_above
        let payment_id = contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(500_000_000_000_000_000_000_000),
        );
        assert_eq!(payment_id, 1);
    }

    #[test]
    #[should_panic(expected = "Payment amount exceeds approval threshold; proposal required")]
    fn test_stage2e_step2_above_threshold_by_one_yocto_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let amount =
            NearToken::from_yoctonear(contract.policy.require_approval_above.as_yoctonear() + 1);
        contract.pay_direct(recipient, amount);
    }

    #[test]
    #[should_panic(expected = "Payment amount exceeds approval threshold; proposal required")]
    fn test_stage2e_step2_zero_threshold_disables_direct_payments() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_yoctonear(0),
        };
        let mut contract =
            setup_direct_payment_contract(owner, agent.clone(), recipient.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(recipient, NearToken::from_yoctonear(1));
    }

    #[test]
    #[should_panic(expected = "Payment amount exceeds maximum per transaction limit")]
    fn test_stage2e_step2_above_max_per_tx_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(1),
            daily_spending_limit: NearToken::from_near(10),
            require_approval_above: NearToken::from_near(1),
        };
        let mut contract =
            setup_direct_payment_contract(owner, agent.clone(), recipient.clone(), policy);

        // Lower policy max_amount_per_tx directly in struct to 0.4 NEAR while require_approval_above is 0.5 NEAR
        contract.policy.max_amount_per_tx =
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(450_000_000_000_000_000_000_000), // > 0.4 NEAR max per tx
        );
    }

    #[test]
    #[should_panic(expected = "Payment amount would exceed daily spending limit")]
    fn test_stage2e_step2_daily_limit_exceeded_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(1),
            require_approval_above: NearToken::from_near(2),
        };
        let mut contract =
            setup_direct_payment_contract(owner, agent.clone(), recipient.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(1_100_000_000_000_000_000_000_000), // 1.1 NEAR > 1.0 daily
        );
    }

    #[test]
    fn test_stage2e_step2_daily_reservation_persisted() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(350_000_000_000_000_000_000_000);
        let timestamp = 2_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        contract.pay_direct(recipient, amount);

        let day = calculate_day_index(timestamp);
        let daily = contract.get_daily_spending(day);
        assert_eq!(daily.reserved_amount, amount);
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));
    }

    #[test]
    fn test_stage2e_step2_exact_daily_limit_accepted() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(1),
            require_approval_above: NearToken::from_near(1),
        };
        let mut contract =
            setup_direct_payment_contract(owner, agent.clone(), recipient.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, NearToken::from_near(1));
        assert_eq!(payment_id, 1);
    }

    #[test]
    #[should_panic(expected = "Payment amount would exceed daily spending limit")]
    fn test_stage2e_step2_daily_limit_exceeded_by_one_yocto_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_near(1),
            require_approval_above: NearToken::from_near(2),
        };
        let mut contract =
            setup_direct_payment_contract(owner, agent.clone(), recipient.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        // 1 NEAR + 1 yocto > 1 NEAR daily limit
        let amount = NearToken::from_yoctonear(NearToken::from_near(1).as_yoctonear() + 1);
        contract.pay_direct(recipient, amount);
    }

    #[test]
    #[should_panic(
        expected = "Insufficient available balance for transfer, storage backing, and safety buffer"
    )]
    fn test_stage2e_step2_insufficient_liquid_balance_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_yoctonear(100)); // Insufficient balance
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
    }

    #[test]
    fn test_stage2e_step2_exact_liquid_balance_safety_boundary_accepted() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(400_000_000_000_000_000_000_000);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let required_balance = contract.get_required_liquid_balance(amount);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(required_balance);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, amount);
        assert_eq!(payment_id, 1);
    }

    #[test]
    fn test_stage2e_step2_payment_ids_increment_sequentially() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(200_000_000_000_000_000_000_000);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let id1 = contract.pay_direct(recipient.clone(), amount);
        let id2 = contract.pay_direct(recipient, amount);

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(contract.get_next_payment_id(), 3);
    }

    #[test]
    #[should_panic(expected = "Payment ID counter overflow")]
    fn test_stage2e_step2_payment_id_overflow_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );
        contract.next_payment_id = u64::MAX;

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000),
        );
    }

    #[test]
    fn test_stage2e_step2_new_payment_recorded_as_inflight() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let id = contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000),
        );
        let payment = contract.get_payment(id).unwrap();

        assert_eq!(payment.status, PaymentStatus::InFlight);
    }

    #[test]
    fn test_stage2e_step2_new_payment_has_none_proposal_id() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let id = contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000),
        );
        let payment = contract.get_payment(id).unwrap();

        assert_eq!(payment.proposal_id, None);
    }

    #[test]
    fn test_stage2e_step2_records_correct_utc_accounting_day() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let timestamp = 4 * NANOS_PER_DAY + 7_200_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let id = contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000),
        );
        let payment = contract.get_payment(id).unwrap();

        assert_eq!(payment.accounting_day, 4);
    }

    #[test]
    fn test_stage2e_step2_transfer_and_callback_dispatched() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000),
        );

        let receipts = get_created_receipts();
        assert!(
            !receipts.is_empty(),
            "Expected created receipts for transfer and callback"
        );
    }

    #[test]
    fn test_stage2e_step2_direct_callback_success_settles_and_confirms() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(300_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, amount);

        let settlement_time = timestamp + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(payment_id);

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.settled_at, Some(settlement_time));
        assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, amount);
    }

    #[test]
    fn test_stage2e_step2_direct_callback_failure_settles_and_releases() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(300_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner,
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, amount);

        let settlement_time = timestamp + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(settlement_time);
        testing_env_with_promise_result(callback_context.build(), PromiseResult::Failed);

        contract.on_payment_completed(payment_id);

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.proposal_id, None);
        assert_eq!(payment.settled_at, Some(settlement_time));
        assert_eq!(payment.settlement_source, Some(SettlementSource::Callback));

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));
    }

    #[test]
    fn test_stage2e_step2_direct_reconciliation_success_works() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(300_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner.clone(),
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, amount);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        let evidence = "https://nearblocks.io/txns/reconcile_step2_success".to_string();
        contract.reconcile_stuck_payment(payment_id, true, evidence.clone());

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Succeeded);
        assert_eq!(payment.settled_at, Some(reconcile_time));
        assert_eq!(
            payment.settlement_source,
            Some(SettlementSource::OwnerReconciliation)
        );
        assert_eq!(payment.reconciliation_evidence, Some(evidence));

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, amount);
    }

    #[test]
    fn test_stage2e_step2_direct_reconciliation_failure_works() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let amount = NearToken::from_yoctonear(300_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner.clone(),
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, amount);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        let evidence = "https://nearblocks.io/txns/reconcile_step2_failure".to_string();
        contract.reconcile_stuck_payment(payment_id, false, evidence.clone());

        let payment = contract.get_payment(payment_id).unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.settled_at, Some(reconcile_time));
        assert_eq!(
            payment.settlement_source,
            Some(SettlementSource::OwnerReconciliation)
        );
        assert_eq!(payment.reconciliation_evidence, Some(evidence));

        let daily = contract.get_daily_spending(payment.accounting_day);
        assert_eq!(daily.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(daily.confirmed_amount, NearToken::from_yoctonear(0));
    }

    #[test]
    #[should_panic(expected = "Payment is not in-flight")]
    fn test_stage2e_step2_late_callback_after_reconciliation_rejected() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let amount = NearToken::from_yoctonear(300_000_000_000_000_000_000_000);
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner.clone(),
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());

        let payment_id = contract.pay_direct(recipient, amount);

        let reconcile_time = timestamp + PAYMENT_SETTLEMENT_TIMEOUT + 1_000_000_000;
        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(reconcile_time);
        testing_env!(owner_context.build());

        contract.reconcile_stuck_payment(payment_id, true, "step2_late_callback_proof".into());

        let callback_time = reconcile_time + 10_000_000_000;
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(callback_time);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(payment_id);
    }

    #[test]
    #[should_panic(expected = "Payment amount would exceed daily spending limit")]
    fn test_stage2e_step2_multiple_direct_payments_share_daily_quota() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);

        let policy = PaymentPolicy {
            max_amount_per_tx: NearToken::from_near(2),
            daily_spending_limit: NearToken::from_yoctonear(800_000_000_000_000_000_000_000), // 0.8 NEAR
            require_approval_above: NearToken::from_near(1),
        };
        let mut contract =
            setup_direct_payment_contract(owner, agent.clone(), recipient.clone(), policy);

        let mut agent_context = get_context(agent);
        agent_context.account_balance(NearToken::from_near(10));
        testing_env!(agent_context.build());

        let id1 = contract.pay_direct(
            recipient.clone(),
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
        assert_eq!(id1, 1);

        let id2 = contract.pay_direct(
            recipient.clone(),
            NearToken::from_yoctonear(400_000_000_000_000_000_000_000),
        );
        assert_eq!(id2, 2);

        // Third payment exceeds 0.8 NEAR daily limit -> panics
        contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(100_000_000_000_000_000_000_000),
        );
    }

    #[test]
    fn test_stage2e_step2_interleaved_direct_and_proposal_payments() {
        let owner = accounts(1);
        let agent = accounts(2);
        let recipient = accounts(3);
        let contract_id = accounts(0);
        let timestamp = 1_000_000_000_000;

        let mut contract = setup_direct_payment_contract(
            owner.clone(),
            agent.clone(),
            recipient.clone(),
            PaymentPolicy::default(),
        );

        // 1. Direct payment: 0.3 NEAR (below 0.5 threshold)
        let mut agent_context = get_context(agent.clone());
        agent_context.account_balance(NearToken::from_near(10));
        agent_context.block_timestamp(timestamp);
        testing_env!(agent_context.build());
        let direct_id1 = contract.pay_direct(
            recipient.clone(),
            NearToken::from_yoctonear(300_000_000_000_000_000_000_000),
        );
        assert_eq!(direct_id1, 1);

        // 2. Proposal payment: 0.8 NEAR (above 0.5 threshold)
        let prop_id = contract.propose_payment(
            recipient.clone(),
            NearToken::from_yoctonear(800_000_000_000_000_000_000_000),
            "Interleaved proposal".into(),
        );
        assert_eq!(prop_id, 1);

        let mut owner_context = get_context(owner);
        owner_context.account_balance(NearToken::from_near(10));
        owner_context.block_timestamp(timestamp);
        testing_env!(owner_context.build());
        contract.approve_proposal(prop_id);

        let outcome = contract.execute_proposal(prop_id);
        assert_eq!(outcome, ExecutionOutcome::Executed { payment_id: 2 });

        // 3. Second direct payment: 0.2 NEAR
        let mut agent_context2 = get_context(agent);
        agent_context2.account_balance(NearToken::from_near(10));
        agent_context2.block_timestamp(timestamp);
        testing_env!(agent_context2.build());
        let direct_id2 = contract.pay_direct(
            recipient,
            NearToken::from_yoctonear(200_000_000_000_000_000_000_000),
        );
        assert_eq!(direct_id2, 3);
        assert_eq!(contract.get_next_payment_id(), 4);

        // Total daily reserved = 0.3 + 0.8 + 0.2 = 1.3 NEAR
        let day = calculate_day_index(timestamp);
        let daily = contract.get_daily_spending(day);
        assert_eq!(
            daily.reserved_amount,
            NearToken::from_yoctonear(1_300_000_000_000_000_000_000_000)
        );

        // Settle all three via callback
        let mut callback_context = get_context(contract_id);
        callback_context.account_balance(NearToken::from_near(10));
        callback_context.block_timestamp(timestamp + 5_000_000_000);
        testing_env_with_promise_result(
            callback_context.build(),
            PromiseResult::Successful(vec![]),
        );

        contract.on_payment_completed(1);
        contract.on_payment_completed(2);
        contract.on_payment_completed(3);

        let p1 = contract.get_payment(1).unwrap();
        let p2 = contract.get_payment(2).unwrap();
        let p3 = contract.get_payment(3).unwrap();

        assert_eq!(p1.status, PaymentStatus::Succeeded);
        assert_eq!(p1.proposal_id, None);

        assert_eq!(p2.status, PaymentStatus::Succeeded);
        assert_eq!(p2.proposal_id, Some(1));

        assert_eq!(p3.status, PaymentStatus::Succeeded);
        assert_eq!(p3.proposal_id, None);

        let daily_settled = contract.get_daily_spending(day);
        assert_eq!(daily_settled.reserved_amount, NearToken::from_yoctonear(0));
        assert_eq!(
            daily_settled.confirmed_amount,
            NearToken::from_yoctonear(1_300_000_000_000_000_000_000_000)
        );
    }
}
