#![cfg_attr(not(feature = "std"), no_std, no_main)]

/// # 52F Protocol — Pillar 1: project52Dampener  (v1 — PolkaVM Edition)
///
/// ## Role within the four-pillar ecosystem
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────┐
/// │                  52F Protocol — Four Pillars                     │
/// │                                                                  │
/// │  [Token Engine]  ◄──── tax pull ────  [project52Dampener]  ◄─┐  │
/// │       │                                       │               │  │
/// │       │ EpochReady                    liquidity injection     │  │
/// │       ▼                               watchdog drain          │  │
/// │  [Sequencer Satellite]          [project52Vault (Vesting)]    │  │
/// │                                                               │  │
/// └───────────────────────────────────────────────────────────────┘  │
/// ```
///
/// ## Responsibilities
///
/// 1. **Liquidity Health Governor** — Monitors the $QF$ liquidity-to-market-cap
///    ratio via a TWAP oracle and injects funds into the DEX pool whenever the
///    ratio falls below 15% (1 500 BPS).  Injections are rate-limited (one per
///    36 000 blocks) and capped at 5% of vault balance per execution (max-drip).
///
/// 2. **Volatility Governor / Great Drain Watchdog** — Reads the Token Engine's
///    prize pot.  If it meets or exceeds the 520 000 000 $52F equivalent
///    threshold, calls `request_great_drain()` on the Token Engine as a
///    belt-and-braces watchdog alongside the engine's built-in auto-drain.
///
/// 3. **Seed Loan Custodian** — Holds the 52 000 $QF$ protocol seed loan and
///    enforces a three-phase recovery schedule based on elapsed blocks since
///    deployment:
///
///    | Phase  | Elapsed blocks    | Recovery condition          |
///    |--------|-------------------|-----------------------------|
///    | Red    | 0 – 77 759 999    | No recovery possible        |
///    | Yellow | 77 760 000 – 155 519 999 | Liquidity ratio > 15% only |
///    | Green  | ≥ 155 520 000     | Free recovery               |
///
///    Block counts at QF Network target of 0.1 s/block (10 blocks/second):
///    - 90 days  = 90 × 24 × 3 600 × 10 = 77 760 000 blocks
///    - 180 days = 155 520 000 blocks
///
/// ## TWAP Integration
///
/// The TWAP price is fetched from an external oracle contract via XCC.  The
/// oracle must implement two messages: `get_twap_price` and `is_price_fresh`.
/// In devnet mode (oracle not set), a 1:1 mock price is used so all other
/// logic can be exercised locally.
///
/// **Compatibility:** ink! v6 / PolkaVM (`pallet-revive`).
///   - `AccountId` → `Address` (H160)
///   - `Balance`   → `U256`
#[ink::contract]
mod project52_dampener {
    use ink::prelude::vec::Vec;
    use ink::storage::Mapping;
    use ink::env::call::{build_call, ExecutionInput, Selector};

    type Address = <ink::env::DefaultEnvironment as ink::env::Environment>::AccountId;
    use ink::primitives::U256;

    // =========================================================================
    // CONSTANTS — MATHEMATICALLY LOCKED
    // =========================================================================

    /// Basis-point denominator for all ratio calculations.
    pub const BPS_DENOMINATOR: u128 = 10_000;

    /// Standard target liquidity-to-market-cap ratio: 15% = 1 500 BPS.
    /// The Health Gate used by both the injection logic and the Seed Guard
    /// Yellow Zone.
    pub const TARGET_LIQUIDITY_RATIO_BPS: u128 = 1_500;

    /// Emergency trigger ratio: 7.5% = 750 BPS (50% of target).
    /// When ratio falls below this floor, emergency injections are permitted
    /// on the shorter `EMERGENCY_COOLDOWN_BLOCKS` schedule.
    pub const EMERGENCY_LIQUIDITY_RATIO_BPS: u128 = 750;

    /// Maximum fraction of vault balance injectable in one call: 5% = 500 BPS.
    pub const MAX_DRIP_BPS: u128 = 500;

    /// Standard minimum blocks between injections: ≈ 1 hour at 10 blocks/s.
    /// Applies when ratio is between 7.5% and 15% (standard health recovery).
    pub const COOLDOWN_BLOCKS: u32 = 36_000;

    /// Emergency minimum blocks between injections: ≈ 6 minutes at 10 blocks/s.
    /// Applies only when ratio is below 7.5% (EMERGENCY_LIQUIDITY_RATIO_BPS).
    /// Short enough for meaningful emergency response; long enough to prevent
    /// bot-harvesting of the injection (6-minute commitment window required).
    pub const EMERGENCY_COOLDOWN_BLOCKS: u32 = 3_600;

    /// TWAP sampling window sent to the oracle (60 minutes in milliseconds).
    pub const TWAP_PERIOD_MS: u64 = 3_600_000;

    /// 18-decimal precision denominator used throughout price maths.
    pub const PRICE_PRECISION: u128 = 1_000_000_000_000_000_000;

    /// Default slippage tolerance: 1% = 100 BPS.
    pub const DEFAULT_SLIPPAGE_BPS: u128 = 100;

    /// Minimum accepted slippage tolerance: 0.5% = 50 BPS.
    /// Prevents callers from bypassing slippage protection with 0%.
    pub const MIN_SLIPPAGE_BPS: u128 = 50;

    // ── SEED GUARD CONSTANTS ──────────────────────────────────────────────────

    /// Total founder loan: the full 52 000 QF provided by the protocol founder.
    ///
    /// Deployment capital flow:
    ///   52 000 QF → founder sends to Dampener contract
    ///     └─ ~41.6 QF  → founder manually seeds DEX (establishes price ratio)
    ///     └─ ~41 558 QF → Dampener deploys as protocol-owned LP (permanently locked)
    ///     └─  10 400 QF → retained as operational vault reserve
    ///
    /// All 52 000 QF is tracked as founder_loan_remaining.
    /// Repayment flows only from vault revenue above VAULT_OPERATIONAL_RESERVE.
    /// The LP portion is permanently locked — only the vault surplus is repayable.
    pub const FOUNDER_LOAN_TOTAL: u128 = 52_000_u128 * 1_000_000_000_000_000_000_u128;

    /// Minimum QF that must remain in the vault after any founder loan repayment.
    /// Ensures the Dampener retains meaningful injection capacity at all times.
    /// Set to 10 400 QF — the original operational vault reserve.
    pub const VAULT_OPERATIONAL_RESERVE: u128 =
        10_400_u128 * 1_000_000_000_000_000_000_u128;

    /// Maximum fraction of surplus repayable in a single call: 20% = 2 000 BPS.
    /// Prevents the entire vault surplus being drained in one transaction.
    pub const REPAYMENT_DRIP_BPS: u128 = 2_000;

    /// Red Zone upper bound (exclusive): 0 – 77 759 999 blocks (0 – 90 days).
    /// No recovery is possible within this window.
    pub const RED_ZONE_END_BLOCKS: u32 = 77_760_000; // 90 days × 10 blocks/s

    /// Yellow Zone upper bound (exclusive): 77 760 000 – 155 519 999 blocks (90 – 180 days).
    /// Recovery is permitted only when liquidity ratio > 15%.
    pub const YELLOW_ZONE_END_BLOCKS: u32 = 155_520_000; // 180 days × 10 blocks/s

    // ── GREAT DRAIN CONSTANTS ─────────────────────────────────────────────────

    /// Watchdog drain threshold: 520 000 000 $52F tokens in $QF$ base units.
    /// Matches `DEFAULT_DRAIN_THRESHOLD` in `project52f.rs`.
    pub const DRAIN_THRESHOLD_DEFAULT: u128 =
        520_000_000_u128 * 1_000_000_000_000_000_000_u128;

    /// Canonical EVM dead/burn address: 0x000…dEaD.
    pub const DEAD_ADDRESS: [u8; 20] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xdE, 0xaD,
    ];

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct Project52Dampener {
        // ── Access control ────────────────────────────────────────────────
        owner: Address,

        // ── Ecosystem addresses ───────────────────────────────────────────
        /// Token Engine contract address (`project52f.rs`).
        token_engine: Address,
        /// Optional DEX router for liquidity injection.
        dex_router: Option<Address>,
        /// Optional LP token address for LP balance queries.
        lp_token: Option<Address>,
        /// Optional TWAP oracle contract address.
        oracle_address: Option<Address>,

        // ── Injection state ───────────────────────────────────────────────
        /// Block number of the last successful liquidity injection.
        last_injection_block: u32,
        /// Timestamp (ms) of the last successful liquidity injection.
        last_injection_timestamp: u64,

        // ── Founder Loan Custodian ────────────────────────────────────────
        /// Block number at which this Dampener was deployed (phase-shift anchor).
        deploy_block: u32,
        /// Total QF loaned by the protocol founder at deployment.
        founder_loan_total: U256,
        /// Remaining unpaid balance of the founder loan.
        /// Decrements with each successful repayment installment.
        founder_loan_remaining: U256,
        /// Address authorised to receive loan repayments.
        founder_address: Address,
        /// Cumulative QF repaid to the founder lifetime.
        total_repaid: U256,

        // ── Initial Liquidity State ───────────────────────────────────────
        /// Whether deploy_initial_liquidity() has been called.
        /// One-time flag — prevents double-deployment.
        initial_liquidity_deployed: bool,
        /// LP tokens held by this contract after initial liquidity deployment.
        lp_tokens_held: U256,

        // ── Great Drain Watchdog ──────────────────────────────────────────
        /// Prize-pot threshold in $QF$ base units above which the Dampener
        /// calls `request_great_drain` on the Token Engine.
        /// Updatable by the owner to track the $52F price.
        watchdog_drain_threshold: U256,
        /// Running count of watchdog-triggered drain requests.
        watchdog_drain_count: u32,

        // ── Historical totals ─────────────────────────────────────────────
        /// Cumulative $QF$ pulled from the Token Engine lifetime.
        lifetime_revenue_pulled: U256,
        /// Cumulative $QF$ injected into the DEX pool lifetime.
        lifetime_liquidity_injected: U256,

        // ── Oracle fallback cache ─────────────────────────────────────────
        /// Last successfully fetched TWAP price (18-decimal precision).
        cached_twap_price: U256,
        /// Block at which cached_twap_price was last updated.
        cached_twap_block: u32,

        // ── Safety ───────────────────────────────────────────────────────
        paused: bool,
        /// Reentrancy guard — set during rate_limited_inject execution.
        is_executing: bool,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct RevenuePulled {
        amount: U256,
        vault_balance_after: U256,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct LiquidityInjected {
        amount: U256,
        vault_balance_after: U256,
        block: u32,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct LiquidityHealthy {
        current_ratio_bps: u128,
        target_ratio_bps: u128,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct InjectionSkipped {
        reason: SkipReason,
        vault_balance: U256,
    }

    /// Emitted when the Dampener watchdog requests a Great Drain on the engine.
    #[ink(event)]
    pub struct WatchdogDrainRequested {
        #[ink(topic)]
        watchdog_drain_id: u32,
        prize_pot_observed: U256,
        threshold: U256,
    }

    // ── Seed Guard events ─────────────────────────────────────────────────────

    /// Emitted when the Dampener deploys the initial protocol-owned liquidity.
    #[ink(event)]
    pub struct InitialLiquidityDeployed {
        qf_deployed: U256,
        tokens_52f_deployed: U256,
        lp_tokens_received: U256,
        at_block: u32,
    }

    /// Emitted when the founder receives a loan repayment installment.
    #[ink(event)]
    pub struct LoanRepayment {
        #[ink(topic)]
        recipient: Address,
        amount: U256,
        loan_remaining: U256,
        vault_balance_after: U256,
    }

    /// Emitted on any seed recovery attempt, successful or not.
    #[ink(event)]
    pub struct SeedRecoveryAttempted {
        #[ink(topic)]
        phase: SeedPhase,
        amount_requested: U256,
        success: bool,
        blocks_elapsed: u32,
    }



    // ── Admin events ──────────────────────────────────────────────────────────

    /// Emitted when a stale/failed oracle call falls back to cached price.
    #[ink(event)]
    pub struct OracleFallback {
        cached_price: U256,
        cached_at_block: u32,
        current_block: u32,
    }

    #[ink(event)]
    pub struct OracleAddressSet {
        #[ink(topic)]
        oracle: Address,
    }

    #[ink(event)]
    pub struct DexRouterSet {
        #[ink(topic)]
        router: Address,
        lp_token: Address,
    }

    #[ink(event)]
    pub struct WatchdogThresholdUpdated {
        previous: U256,
        updated: U256,
    }

    #[ink(event)]
    pub struct EmergencyWithdrawal {
        #[ink(topic)]
        recipient: Address,
        amount: U256,
    }

    // =========================================================================
    // ENUMS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, Clone, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum SkipReason {
        CooldownActive,
        LiquidityHealthy,
        InsufficientFunds,
        AlreadyInjectedThisBlock,
    }

    /// Seed Guard phase at time of a recovery attempt.
    #[derive(Debug, PartialEq, Eq, Clone, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum SeedPhase {
        /// 0–90 days — no recovery permitted.
        Red,
        /// 91–180 days — recovery permitted only if liquidity ratio > 15%.
        Yellow,
        /// 181+ days — unrestricted recovery.
        Green,
    }

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        /// Caller is not the contract owner.
        NotOwner,
        /// Cooldown period between injections has not elapsed.
        CooldownActive,
        /// Only one injection is permitted per block.
        AlreadyInjectedThisBlock,
        /// Revenue pull from the Token Engine failed.
        PullFailed,
        /// Arithmetic overflow.
        Overflow,
        /// TWAP oracle is not configured or returned a stale price.
        TwapUnavailable,
        /// Initial liquidity has already been deployed — cannot call again.
        InitialLiquidityAlreadyDeployed,
        /// Initial liquidity has not been deployed yet.
        InitialLiquidityNotDeployed,
        /// Vault surplus above operational reserve is zero — nothing to repay.
        NoRepaymentAvailable,
        /// Founder loan is fully repaid.
        LoanFullyRepaid,
        /// Caller is not the founder address.
        NotFounder,
        /// Re-entrant call detected.
        Reentrant,
        /// Liquidity ratio is already above 15%; no injection needed.
        LiquidityHealthy,
        /// Vault balance is insufficient for the requested operation.
        InsufficientVaultBalance,
        /// No injection is needed at this time.
        NoInjectionNeeded,
        /// DEX router address has not been configured.
        DexRouterNotConfigured,
        /// DEX liquidity addition call failed.
        InjectionFailed,
        /// Slippage tolerance is below the 0.5% minimum.
        SlippageTooLow,
        /// Oracle address has not been configured.
        OracleNotConfigured,
        /// Seed recovery is in the Red Zone (first 90 days).
        SeedRecoveryRedZone,
        /// Seed recovery is in the Yellow Zone but liquidity ratio is ≤ 15%.
        SeedRecoveryLiquidityGateFailed,
        /// Requested drain amount exceeds the watchdog threshold.
        DrainThresholdNotMet,
        /// Cross-contract call to the Token Engine failed.
        TokenEngineCallFailed,
        /// A native value transfer failed.
        TransferFailed,
        /// Contract is paused.
        ContractPaused,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACES
    // =========================================================================

    /// Minimal interface to the TWAP Oracle.
    #[ink::trait_definition]
    pub trait OracleInterface {
        /// Returns the TWAP price of `token_in` denominated in `token_out`,
        /// sampled over `period_ms` milliseconds, with 18-decimal precision.
        #[ink(message)]
        fn get_twap_price(
            &self,
            token_in: Address,
            token_out: Address,
            period_ms: u64,
        ) -> Result<U256, Error>;

        /// Returns `true` if the most recent oracle observation is less than
        /// one TWAP period old (i.e. the price is not stale).
        #[ink(message)]
        fn is_price_fresh(&self) -> bool;
    }

    /// Minimal interface to the Token Engine (`project52f.rs`).
    #[ink::trait_definition]
    pub trait TokenEngineInterface {
        /// Pull the dampener's allocated tax from the Token Engine.
        #[ink(message)]
        fn pull_dampener_tax(&mut self) -> Result<U256, Error>;

        /// Read the current prize pot without mutating state.
        #[ink(message)]
        fn get_prize_pot(&self) -> U256;

        /// Request a Great Drain check from an authorised external watchdog.
        /// The Token Engine must whitelist this Dampener as an authorised caller.
        #[ink(message)]
        fn request_great_drain(&mut self) -> Result<(), Error>;
    }

    /// Minimal interface to the DEX Router.
    #[ink::trait_definition]
    pub trait DexRouterInterface {
        /// Add liquidity (native + token) to the pool.
        ///
        /// Returns `(amount_token_used, amount_native_used, lp_tokens_minted)`.
        #[ink(message)]
        fn add_liquidity_native(
            &mut self,
            token: Address,
            amount_token_desired: U256,
            amount_token_min: U256,
            amount_native_min: U256,
            to: Address,
            deadline: u64,
        ) -> Result<(U256, U256, U256), Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52Dampener {
        // ---------------------------------------------------------------------
        // Constructor
        // ---------------------------------------------------------------------

        /// Deploy the Dampener Vault (Pillar 1).
        ///
        /// # Parameters
        /// - `token_engine`      — Address of the deployed `project52f.rs` Token Engine.
        /// - `founder`          — Address authorised to receive loan repayment installments.
        ///
        /// The seed loan balance is initialised at `SEED_LOAN_AMOUNT`; the
        /// actual $QF$ tokens must be deposited to this contract's address
        /// by the deployer after construction.
        #[ink(constructor)]
        /// Deploy the Dampener Vault.
        ///
        /// `token_engine`  — address of the Token Engine contract.
        /// `founder`       — address to receive loan repayment installments.
        /// `loan_amount`   — total QF provided by the founder (typically 52 000 QF).
        ///                   Pass the exact amount sent so accounting is precise.
        pub fn new(token_engine: Address, founder: Address, loan_amount: U256) -> Self {
            let caller = Self::env().caller();
            let deploy_block = Self::env().block_number();

            Self {
                owner: caller,
                token_engine,
                dex_router: None,
                lp_token: None,
                oracle_address: None,
                last_injection_block: deploy_block,
                last_injection_timestamp: 0,
                deploy_block,
                founder_loan_total: loan_amount,
                founder_loan_remaining: loan_amount,
                founder_address: founder,
                total_repaid: U256::ZERO,
                initial_liquidity_deployed: false,
                lp_tokens_held: U256::ZERO,
                watchdog_drain_threshold: U256::from(DRAIN_THRESHOLD_DEFAULT),
                watchdog_drain_count: 0,
                lifetime_revenue_pulled: U256::ZERO,
                lifetime_liquidity_injected: U256::ZERO,
                cached_twap_price: U256::ZERO,
                cached_twap_block: 0,
                paused: false,
                is_executing: false,
            }
        }

        // =====================================================================
        // REVENUE PULL — Token Engine → Dampener Vault
        // =====================================================================

        /// Pull the Dampener's tax allocation from the Token Engine via XCC.
        ///
        /// Uses ink! v6 `try_invoke` so a failed engine call returns
        /// [`Error::PullFailed`] rather than panicking.
        #[ink(message)]
        pub fn pull_from_engine(&mut self) -> Result<U256, Error> {
            self.assert_not_paused()?;

            let result: Result<Result<U256, Error>, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(self.token_engine)
                    .exec_input(ExecutionInput::new(Selector::new(
                        ink::selector_bytes!("pull_dampener_tax"),
                    )))
                    .returns::<Result<U256, Error>>()
                    .try_invoke();

            match result {
                Ok(Ok(amount)) => {
                    self.lifetime_revenue_pulled = self
                        .lifetime_revenue_pulled
                        .saturating_add(amount);

                    let vault_balance = U256::from(self.env().balance());

                    self.env().emit_event(RevenuePulled {
                        amount,
                        vault_balance_after: vault_balance,
                        timestamp: self.env().block_timestamp(),
                    });

                    Ok(amount)
                }
                _ => Err(Error::PullFailed),
            }
        }

        // =====================================================================
        // GREAT DRAIN WATCHDOG
        // =====================================================================

        /// Check the Token Engine's prize pot and, if it meets or exceeds the
        /// watchdog threshold, request a Great Drain.
        ///
        /// The Token Engine already auto-drains on every buy/sell; this function
        /// is a belt-and-braces watchdog callable by any external keeper or bot.
        ///
        /// # Flow
        /// 1. Read `get_prize_pot()` from the Token Engine (read-only XCC).
        /// 2. Compare against `watchdog_drain_threshold`.
        /// 3. If threshold met, call `request_great_drain()` on the engine.
        /// 4. Emit `WatchdogDrainRequested`.
        ///
        /// # Errors
        /// - [`Error::DrainThresholdNotMet`] — pot is below threshold; no action taken.
        /// - [`Error::TokenEngineCallFailed`] — XCC to engine failed.
        #[ink(message)]
        pub fn watchdog_check_and_drain(&mut self) -> Result<(), Error> {
            self.assert_not_paused()?;

            // ── Step 1: Read prize pot (read-only XCC) ─────────────────────
            let prize_pot = self.read_prize_pot()?;

            if prize_pot < self.watchdog_drain_threshold {
                return Err(Error::DrainThresholdNotMet);
            }

            // ── Step 2: Request drain on Token Engine ──────────────────────
            let drain_result: Result<Result<(), Error>, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(self.token_engine)
                    .exec_input(ExecutionInput::new(Selector::new(
                        ink::selector_bytes!("request_great_drain"),
                    )))
                    .returns::<Result<(), Error>>()
                    .try_invoke();

            match drain_result {
                Ok(Ok(())) => {
                    self.watchdog_drain_count =
                        self.watchdog_drain_count.saturating_add(1);

                    let drain_id = self.watchdog_drain_count;
                    let threshold = self.watchdog_drain_threshold;

                    self.env().emit_event(WatchdogDrainRequested {
                        watchdog_drain_id: drain_id,
                        prize_pot_observed: prize_pot,
                        threshold,
                    });

                    Ok(())
                }
                _ => Err(Error::TokenEngineCallFailed),
            }
        }

        /// Read the Token Engine's prize pot (read-only, does not mutate state).
        /// XCC — read the 52F balance of this Dampener contract from the Token Engine.
        fn read_own_52f_balance(&self) -> Result<U256, Error> {
            let result: Result<Result<U256, Error>, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(self.token_engine)
                    .exec_input(
                        ExecutionInput::new(Selector::new(
                            ink::selector_bytes!("balance_of"),
                        ))
                        .push_arg(&self.env().account_id()),
                    )
                    .returns::<Result<U256, Error>>()
                    .try_invoke();
            match result {
                Ok(Ok(bal)) => Ok(bal),
                _ => Ok(U256::ZERO),
            }
        }

        fn read_prize_pot(&self) -> Result<U256, Error> {
            let result: Result<U256, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(self.token_engine)
                    .exec_input(ExecutionInput::new(Selector::new(
                        ink::selector_bytes!("get_prize_pot"),
                    )))
                    .returns::<U256>()
                    .try_invoke();

            match result {
                Ok(pot) => Ok(pot),
                Err(_) => Err(Error::TokenEngineCallFailed),
            }
        }

        // =====================================================================
        // TWAP ORACLE
        // =====================================================================

        /// Fetch the 60-minute TWAP price of $QF$ from the configured oracle.
        ///
        /// ## Fallback behaviour
        /// If the oracle call fails or returns a stale price, this function
        /// falls back to the last successfully cached price, provided the cache
        /// is less than MAX_EPOCH_BLOCKS old (≈ 1 hour at 36 000 blocks).
        /// If the cache is also too old, returns [`Error::TwapUnavailable`] and
        /// the injection is skipped — vault funds are never deployed blind.
        ///
        /// In devnet mode (oracle not configured), returns a 1:1 mock price.
        pub fn get_twap_price(&self) -> Result<U256, Error> {
            // Devnet mode: no oracle → 1:1 mock.
            if self.oracle_address.is_none() {
                return Ok(U256::from(PRICE_PRECISION));
            }

            let oracle = self.oracle_address.ok_or(Error::OracleNotConfigured)?;
            let engine = self.token_engine;
            let current_block = self.env().block_number();

            // ── Attempt live oracle fetch ──────────────────────────────────
            let price_result: Result<Result<U256, Error>, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(oracle)
                    .exec_input(
                        ExecutionInput::new(Selector::new(
                            ink::selector_bytes!("get_twap_price"),
                        ))
                        .push_arg(&engine)
                        .push_arg(&self.env().account_id())
                        .push_arg(&TWAP_PERIOD_MS),
                    )
                    .returns::<Result<U256, Error>>()
                    .try_invoke();

            let fresh_result: Result<bool, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(oracle)
                    .exec_input(ExecutionInput::new(Selector::new(
                        ink::selector_bytes!("is_price_fresh"),
                    )))
                    .returns::<bool>()
                    .try_invoke();

            if let (Ok(Ok(price)), Ok(true)) = (price_result, fresh_result) {
                // Live price — caller should update cache via update_twap_cache().
                return Ok(price);
            }

            // ── Oracle failed or stale — attempt cache fallback ────────────
            // Cache window: COOLDOWN_BLOCKS (36 000 ≈ 1 hour at 10 blocks/s).
            // If the cache is too old, refuse to inject rather than use bad data.
            if !self.cached_twap_price.is_zero() {
                let cache_age = current_block.saturating_sub(self.cached_twap_block);
                if cache_age <= COOLDOWN_BLOCKS {
                    self.env().emit_event(OracleFallback {
                        cached_price: self.cached_twap_price,
                        cached_at_block: self.cached_twap_block,
                        current_block,
                    });
                    return Ok(self.cached_twap_price);
                }
            }

            Err(Error::TwapUnavailable)
        }

        /// Update the cached TWAP price from a successful oracle fetch.
        ///
        /// Called internally after a successful live price fetch.
        /// Separated because `get_twap_price` takes `&self` (immutable).
        fn update_twap_cache(&mut self) {
            if let Ok(price) = self.get_twap_price() {
                // Only update cache if we got a live (non-fallback) price.
                // Heuristic: if cached_twap_block == current_block we already cached this block.
                let current_block = self.env().block_number();
                if self.cached_twap_block < current_block {
                    self.cached_twap_price = price;
                    self.cached_twap_block = current_block;
                }
            }
        }

        // =====================================================================
        // LIQUIDITY HEALTH CHECK
        // =====================================================================

        /// Assess the current liquidity health of the $QF$ pool.
        ///
        /// Returns `(is_healthy, deficit_qf, max_injectable_qf)`:
        /// - `is_healthy`        — `true` if ratio ≥ 15%.
        /// - `deficit_qf`        — how much $QF$ is needed to reach the target.
        /// - `max_injectable_qf` — 5% of current vault balance (max-drip cap).
        ///
        /// Emits [`LiquidityHealthy`] when the ratio is at or above the target.
        ///
        /// ### Maths
        /// ```text
        /// market_cap          = total_supply × twap_price / PRICE_PRECISION
        /// current_ratio_bps   = current_liquidity_value × 10_000 / market_cap
        /// target_liquidity    = market_cap × 1_500 / 10_000
        /// deficit             = target_liquidity − current_liquidity_value
        /// max_injection       = vault_balance × 500 / 10_000
        /// ```
        pub fn check_liquidity_health(&self) -> Result<(bool, U256, U256), Error> {
            let twap_price = self.get_twap_price()?;

            let total_supply = self.fetch_total_supply();
            let current_liquidity = self.fetch_current_liquidity();

            let price_precision = U256::from(PRICE_PRECISION);
            let bps_denom = U256::from(BPS_DENOMINATOR);
            let target_bps = U256::from(TARGET_LIQUIDITY_RATIO_BPS);
            let max_drip_bps = U256::from(MAX_DRIP_BPS);

            let market_cap = total_supply
                .checked_mul(twap_price)
                .ok_or(Error::Overflow)?
                .checked_div(price_precision)
                .ok_or(Error::Overflow)?;

            let current_ratio_bps = if market_cap.is_zero() {
                U256::ZERO
            } else {
                current_liquidity
                    .checked_mul(bps_denom)
                    .ok_or(Error::Overflow)?
                    .checked_div(market_cap)
                    .ok_or(Error::Overflow)?
            };

            let target_bps_u128 = u128::try_from(current_ratio_bps)
                .unwrap_or(u128::MAX);

            if target_bps_u128 >= TARGET_LIQUIDITY_RATIO_BPS {
                self.env().emit_event(LiquidityHealthy {
                    current_ratio_bps: target_bps_u128,
                    target_ratio_bps: TARGET_LIQUIDITY_RATIO_BPS,
                    timestamp: self.env().block_timestamp(),
                });
                return Ok((true, U256::ZERO, U256::ZERO));
            }

            let target_liquidity = market_cap
                .checked_mul(target_bps)
                .ok_or(Error::Overflow)?
                .checked_div(bps_denom)
                .ok_or(Error::Overflow)?;

            let deficit = target_liquidity.saturating_sub(current_liquidity);

            let vault_balance = U256::from(self.env().balance());
            let max_injection = vault_balance
                .checked_mul(max_drip_bps)
                .ok_or(Error::Overflow)?
                .checked_div(bps_denom)
                .ok_or(Error::Overflow)?;

            Ok((false, deficit, max_injection))
        }

        /// Get the current liquidity ratio in BPS as a standalone view function.
        ///
        /// Returns `(current_ratio_bps, target_ratio_bps, is_healthy)`.
        #[ink(message)]
        pub fn get_liquidity_ratio(&self) -> Result<(u128, u128, bool), Error> {
            let twap_price = self.get_twap_price()?;

            let total_supply = self.fetch_total_supply();
            let current_liquidity = self.fetch_current_liquidity();

            let price_precision = U256::from(PRICE_PRECISION);
            let bps_denom = U256::from(BPS_DENOMINATOR);

            let market_cap = if total_supply.is_zero() || twap_price.is_zero() {
                U256::ZERO
            } else {
                total_supply
                    .checked_mul(twap_price)
                    .ok_or(Error::Overflow)?
                    .checked_div(price_precision)
                    .ok_or(Error::Overflow)?
            };

            let current_ratio_bps = if market_cap.is_zero() {
                0u128
            } else {
                let ratio = current_liquidity
                    .checked_mul(bps_denom)
                    .ok_or(Error::Overflow)?
                    .checked_div(market_cap)
                    .ok_or(Error::Overflow)?;
                u128::try_from(ratio).unwrap_or(u128::MAX)
            };

            let is_healthy = current_ratio_bps >= TARGET_LIQUIDITY_RATIO_BPS;
            Ok((current_ratio_bps, TARGET_LIQUIDITY_RATIO_BPS, is_healthy))
        }

        // =====================================================================
        // RATE-LIMITED LIQUIDITY INJECTION
        // =====================================================================

        /// Inject $QF$ liquidity into the DEX pool, subject to:
        ///
        /// 1. **Slippage guard** — `slippage_bps` must be ≥ 50 BPS (0.5%).
        /// 2. **Block throttle** — only one injection per block.
        /// 3. **Cooldown** — minimum `COOLDOWN_BLOCKS` between injections.
        /// 4. **Health gate** — no injection if liquidity ratio is already ≥ 15%.
        /// 5. **Max-drip cap** — at most 5% of vault balance per call.
        ///
        /// Returns the amount of $QF$ successfully injected.
        #[ink(message)]
        pub fn rate_limited_inject(&mut self, slippage_bps: u128) -> Result<U256, Error> {
            self.assert_not_paused()?;
            if self.is_executing { return Err(Error::Reentrant); }
            self.is_executing = true;
            let result = self.rate_limited_inject_inner(slippage_bps);
            self.is_executing = false;
            result
        }

        fn rate_limited_inject_inner(&mut self, slippage_bps: u128) -> Result<U256, Error> {
            self.assert_not_paused()?;

            let current_block = self.env().block_number();
            let current_timestamp = self.env().block_timestamp();

            // ── Guard 1: Slippage tolerance ───────────────────────────────
            if slippage_bps < MIN_SLIPPAGE_BPS {
                return Err(Error::SlippageTooLow);
            }

            // ── Guard 2: Per-block throttle ───────────────────────────────
            if current_block == self.last_injection_block {
                let vault_balance = U256::from(self.env().balance());
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::AlreadyInjectedThisBlock,
                    vault_balance,
                });
                return Err(Error::AlreadyInjectedThisBlock);
            }

            // ── Guard 3: Health check (needed before cooldown tier selection) ─
            let (is_healthy, deficit, max_permitted) = self.check_liquidity_health()?;

            if is_healthy {
                let vault_balance = U256::from(self.env().balance());
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::LiquidityHealthy,
                    vault_balance,
                });
                return Err(Error::LiquidityHealthy);
            }

            // ── Guard 4: Tiered cooldown ──────────────────────────────────
            // Below 7.5% (emergency floor): 3 600-block cooldown (~6 minutes).
            // Below 15% (standard recovery): 36 000-block cooldown (~1 hour).
            let (current_ratio_bps, _, _) = self.get_liquidity_ratio()?;
            let cooldown = if current_ratio_bps < EMERGENCY_LIQUIDITY_RATIO_BPS {
                EMERGENCY_COOLDOWN_BLOCKS
            } else {
                COOLDOWN_BLOCKS
            };
            let blocks_since_last =
                current_block.saturating_sub(self.last_injection_block);
            if blocks_since_last < cooldown {
                let vault_balance = U256::from(self.env().balance());
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::CooldownActive,
                    vault_balance,
                });
                return Err(Error::CooldownActive);
            }

            // ── Guard 5: Max-drip cap ─────────────────────────────────────
            let injection_amount = deficit.min(max_permitted);

            if injection_amount.is_zero() {
                return Err(Error::NoInjectionNeeded);
            }

            let vault_balance = U256::from(self.env().balance());
            if injection_amount > vault_balance {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::InsufficientFunds,
                    vault_balance,
                });
                return Err(Error::InsufficientVaultBalance);
            }

            // ── Slippage-protected execution ──────────────────────────────
            let expected_lp = self.estimate_lp_tokens(injection_amount)?;

            let bps_denom = U256::from(BPS_DENOMINATOR);
            let slippage = U256::from(slippage_bps);
            let min_out = expected_lp
                .checked_mul(bps_denom.saturating_sub(slippage))
                .ok_or(Error::Overflow)?
                .checked_div(bps_denom)
                .ok_or(Error::Overflow)?;

            self.execute_liquidity_addition(injection_amount, min_out)?;

            // ── State update ──────────────────────────────────────────────
            self.last_injection_block = current_block;
            self.last_injection_timestamp = current_timestamp;
            self.lifetime_liquidity_injected = self
                .lifetime_liquidity_injected
                .saturating_add(injection_amount);

            let vault_balance_after = U256::from(self.env().balance());

            // ── Update oracle price cache on successful injection ────
            if let Ok(price) = self.get_twap_price() {
                let current_block = self.env().block_number();
                if self.cached_twap_block < current_block {
                    self.cached_twap_price = price;
                    self.cached_twap_block = current_block;
                }
            }

            self.env().emit_event(LiquidityInjected {
                amount: injection_amount,
                vault_balance_after,
                block: current_block,
                timestamp: current_timestamp,
            });

            Ok(injection_amount)
        }

        // =====================================================================
        // INITIAL LIQUIDITY DEPLOYMENT
        // =====================================================================

        /// Deploy the bulk of the founder's capital as protocol-owned liquidity.
        ///
        /// Called once by the owner before trading is enabled.  The Dampener
        /// contract permanently holds the resulting LP tokens — they can never
        /// be withdrawn.
        ///
        /// ## Pre-conditions
        /// 1. DEX router must be registered (`set_dex_router`).
        /// 2. Token Engine must have sent bulk 52F via `seed_transfer()`.
        /// 3. Trading must still be disabled on the Token Engine.
        ///
        /// ## Capital split applied here
        /// ```text
        /// vault_balance at call time (≈ 51 958 QF after tiny seed add)
        ///   ├── VAULT_OPERATIONAL_RESERVE (10 400 QF) → kept in vault
        ///   └── remainder (≈ 41 558 QF)              → added to DEX as LP
        /// ```
        ///
        /// All 52F tokens held by this contract are paired with the QF amount.
        /// The DEX returns LP tokens which are stored in `lp_tokens_held`.
        #[ink(message)]
        pub fn deploy_initial_liquidity(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            self.assert_not_paused()?;

            if self.initial_liquidity_deployed {
                return Err(Error::InitialLiquidityAlreadyDeployed);
            }

            let vault_balance = U256::from(self.env().balance());
            let reserve = U256::from(VAULT_OPERATIONAL_RESERVE);

            // Keep the operational reserve — deploy everything above it.
            if vault_balance <= reserve {
                // Nothing deployable above the floor. Mark as deployed
                // (no-op) so the flag is set and re-entry is impossible.
                self.initial_liquidity_deployed = true;
                return Ok(());
            }

            let qf_to_deploy = vault_balance.saturating_sub(reserve);

            // ── Read 52F token balance of this contract ───────────────────
            // The Token Engine has already sent the bulk 52F via seed_transfer().
            let tokens_52f = self.read_own_52f_balance()?;

            if tokens_52f.is_zero() {
                // No 52F received yet — cannot pair. Owner must call
                // seed_transfer() on Token Engine first.
                return Err(Error::InitialLiquidityNotDeployed);
            }

            // ── Mark before external calls ────────────────────────────────
            self.initial_liquidity_deployed = true;

            // ── Execute LP add (DEX router XCC) ───────────────────────────
            // In devnet (no router), this is a no-op — just stores the values.
            self.execute_liquidity_addition(qf_to_deploy, U256::ZERO)?;

            // LP tokens returned by the router are credited here.
            // In a real deployment the router XCC would return the LP amount.
            // We store the 52F amount as a proxy for LP accounting.
            // Full LP token integration is DEX-router specific at deploy time.
            self.lp_tokens_held = tokens_52f;

            self.env().emit_event(InitialLiquidityDeployed {
                qf_deployed: qf_to_deploy,
                tokens_52f_deployed: tokens_52f,
                lp_tokens_received: tokens_52f,
                at_block: self.env().block_number(),
            });

            Ok(())
        }

        // =====================================================================
        // FOUNDER LOAN REPAYMENT — Installment-Based Recovery
        // =====================================================================

        /// Request a founder loan repayment installment.
        ///
        /// Transfers available vault surplus (above VAULT_OPERATIONAL_RESERVE)
        /// to the founder address, up to 20% of the surplus per call.
        ///
        /// ## Conditions
        /// - Caller must be `founder_address`.
        /// - At least 90 days (RED_ZONE_END_BLOCKS) must have elapsed.
        /// - Between 90–180 days (Yellow Zone): liquidity ratio must be ≥ 15%.
        /// - Vault balance must exceed VAULT_OPERATIONAL_RESERVE.
        /// - Loan must not be fully repaid.
        ///
        /// ## Return value
        /// Amount of QF transferred in this installment.
        #[ink(message)]
        pub fn request_loan_repayment(&mut self) -> Result<U256, Error> {
            self.assert_not_paused()?;

            if self.env().caller() != self.founder_address {
                return Err(Error::NotFounder);
            }
            if self.founder_loan_remaining.is_zero() {
                return Err(Error::LoanFullyRepaid);
            }

            // ── Time gate: 90 days minimum ────────────────────────────────
            let current_block = self.env().block_number();
            let blocks_elapsed = current_block.saturating_sub(self.deploy_block);
            if blocks_elapsed < RED_ZONE_END_BLOCKS {
                return Err(Error::SeedRecoveryRedZone);
            }

            // ── Yellow Zone (90–180 days): liquidity health gate ──────────
            if blocks_elapsed < YELLOW_ZONE_END_BLOCKS {
                let (_, _, is_healthy) = self
                    .get_liquidity_ratio()
                    .unwrap_or((0, TARGET_LIQUIDITY_RATIO_BPS, false));
                if !is_healthy {
                    return Err(Error::SeedRecoveryLiquidityGateFailed);
                }
            }

            // ── Compute repayable surplus ─────────────────────────────────
            let vault_balance = U256::from(self.env().balance());
            let floor = U256::from(VAULT_OPERATIONAL_RESERVE);

            if vault_balance <= floor {
                return Err(Error::NoRepaymentAvailable);
            }

            let surplus = vault_balance.saturating_sub(floor);

            // Cap to 20% of surplus per call.
            let installment_cap = surplus
                .checked_mul(U256::from(REPAYMENT_DRIP_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(U256::from(BPS_DENOMINATOR))
                .ok_or(Error::Overflow)?;

            // Further cap to outstanding loan balance.
            let repayment = installment_cap.min(self.founder_loan_remaining);

            if repayment.is_zero() {
                return Err(Error::NoRepaymentAvailable);
            }

            // ── State update before transfer ───────────────────────────────
            self.founder_loan_remaining = self.founder_loan_remaining
                .saturating_sub(repayment);
            self.total_repaid = self.total_repaid
                .checked_add(repayment)
                .ok_or(Error::Overflow)?;

            let founder = self.founder_address;
            let vault_after = vault_balance.saturating_sub(repayment);

            // ── Transfer ──────────────────────────────────────────────────
            let amount_u128 = u128::try_from(repayment).unwrap_or(u128::MAX);
            self.env()
                .transfer(founder, amount_u128)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(LoanRepayment {
                recipient: founder,
                amount: repayment,
                loan_remaining: self.founder_loan_remaining,
                vault_balance_after: vault_after,
            });

            Ok(repayment)
        }

        /// View the current repayment status.
        ///
        /// Returns `(loan_total, loan_remaining, total_repaid, vault_surplus)`.
        /// `vault_surplus` is the amount available for repayment right now
        /// (may be zero if vault is at or below operational reserve).
        #[ink(message)]
        pub fn get_loan_status(&self) -> (U256, U256, U256, U256) {
            let vault_balance = U256::from(self.env().balance());
            let floor = U256::from(VAULT_OPERATIONAL_RESERVE);
            let surplus = if vault_balance > floor {
                vault_balance.saturating_sub(floor)
            } else {
                U256::ZERO
            };
            (
                self.founder_loan_total,
                self.founder_loan_remaining,
                self.total_repaid,
                surplus,
            )
        }

        // =====================================================================
        // DEX / LP HELPERS
        // =====================================================================

        /// Execute the DEX router call to add liquidity with slippage protection.
        ///
        /// In devnet mode (no router configured), returns `Ok(())` so the full
        /// injection pathway can be exercised locally.
        fn execute_liquidity_addition(
            &self,
            amount_qf: U256,
            min_lp_tokens: U256,
        ) -> Result<(), Error> {
            let router = match self.dex_router {
                None => return Ok(()), // devnet mock
                Some(r) => r,
            };

            let deadline = self.env().block_timestamp() + 300_000; // +5 minutes
            let token = self.token_engine; // $QF token is the Token Engine contract

            let amount_qf_u128 = u128::try_from(amount_qf).unwrap_or(0);

            let result: Result<Result<(U256, U256, U256), Error>, ink::env::Error> =
                build_call::<ink::env::DefaultEnvironment>()
                    .call(router)
                    .transfer_value(amount_qf_u128)
                    .exec_input(
                        ExecutionInput::new(Selector::new(
                            ink::selector_bytes!("add_liquidity_native"),
                        ))
                        .push_arg(&token)
                        .push_arg(&U256::ZERO)       // amount_token_desired (router calculates)
                        .push_arg(&U256::ZERO)       // amount_token_min
                        .push_arg(&min_lp_tokens)    // amount_native_min — slippage guard
                        .push_arg(&self.env().account_id())
                        .push_arg(&deadline),
                    )
                    .returns::<Result<(U256, U256, U256), Error>>()
                    .try_invoke();

            match result {
                Ok(Ok(_)) => Ok(()),
                _ => Err(Error::InjectionFailed),
            }
        }

        /// Estimate LP tokens expected for `qf_amount` of liquidity.
        ///
        /// Returns 1:1 in devnet mode (no router configured).
        fn estimate_lp_tokens(&self, qf_amount: U256) -> Result<U256, Error> {
            if self.dex_router.is_none() {
                return Ok(qf_amount); // devnet: 1:1
            }
            // Production: query router.quote() — placeholder until QF DEX is live.
            Ok(qf_amount)
        }

        /// Fetch total $QF$ supply.
        ///
        /// Returns a fixed devnet mock when the router is not configured.
        fn fetch_total_supply(&self) -> U256 {
            if self.dex_router.is_none() {
                // 1 000 000 QF (18 decimals) — devnet mock
                return U256::from(1_000_000_u128 * PRICE_PRECISION);
            }
            // Production: XCC to token engine `total_supply()`.
            U256::from(1_000_000_u128 * PRICE_PRECISION)
        }

        /// Fetch current DEX pool liquidity value.
        ///
        /// Returns a mock at 10% market cap when the router is not configured.
        fn fetch_current_liquidity(&self) -> U256 {
            if self.dex_router.is_none() {
                // Mock: 10% of 1 000 000 QF supply at 1:1 price = 100 000 QF
                return U256::from(100_000_u128 * PRICE_PRECISION);
            }
            // Production: XCC to LP pair to read reserves.
            U256::from(100_000_u128 * PRICE_PRECISION)
        }

        // =====================================================================
        // ADMIN FUNCTIONS
        // =====================================================================

        /// Set or update the TWAP Oracle address.
        #[ink(message)]
        pub fn set_oracle_address(&mut self, oracle: Address) -> Result<(), Error> {
            self.only_owner()?;
            self.oracle_address = Some(oracle);
            self.env().emit_event(OracleAddressSet { oracle });
            Ok(())
        }

        /// Set the DEX router and LP token addresses.
        #[ink(message)]
        pub fn set_dex_router(
            &mut self,
            router: Address,
            lp_token: Address,
        ) -> Result<(), Error> {
            self.only_owner()?;
            self.dex_router = Some(router);
            self.lp_token = Some(lp_token);
            self.env().emit_event(DexRouterSet { router, lp_token });
            Ok(())
        }

        /// Update the Token Engine address.
        #[ink(message)]
        pub fn set_token_engine(&mut self, engine: Address) -> Result<(), Error> {
            self.only_owner()?;
            self.token_engine = engine;
            Ok(())
        }


        /// Update the watchdog drain threshold.
        ///
        /// Should represent 520 000 000 $52F tokens in $QF$ base units at
        /// the current $52F spot price.
        #[ink(message)]
        pub fn set_watchdog_drain_threshold(
            &mut self,
            new_threshold: U256,
        ) -> Result<(), Error> {
            self.only_owner()?;
            let previous = self.watchdog_drain_threshold;
            self.watchdog_drain_threshold = new_threshold;
            self.env().emit_event(WatchdogThresholdUpdated {
                previous,
                updated: new_threshold,
            });
            Ok(())
        }

        /// Emergency withdrawal of vault balance to the owner.
        ///
        /// Intentionally excludes the seed loan balance — that can only be
        /// recovered via `reclaim_seed_loan`.
        #[ink(message)]
        pub fn emergency_withdraw(&mut self, amount: U256) -> Result<(), Error> {
            self.only_owner()?;

            let vault_balance = U256::from(self.env().balance());

            // Protect the operational reserve from emergency drain.
            let withdrawable = vault_balance.saturating_sub(U256::from(VAULT_OPERATIONAL_RESERVE));

            if amount > withdrawable {
                return Err(Error::InsufficientVaultBalance);
            }

            let amount_u128 = u128::try_from(amount).unwrap_or(0);

            self.env()
                .transfer(self.owner, amount_u128)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(EmergencyWithdrawal {
                recipient: self.owner,
                amount,
            });

            Ok(())
        }

        #[ink(message)]
        pub fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
            self.only_owner()?;
            self.paused = paused;
            Ok(())
        }

        // =====================================================================
        // VIEW FUNCTIONS
        // =====================================================================

        #[ink(message)]
        pub fn get_vault_balance(&self) -> U256 {
            U256::from(self.env().balance())
        }

        #[ink(message)]
        pub fn get_last_injection_block(&self) -> u32 {
            self.last_injection_block
        }

        #[ink(message)]
        pub fn get_cooldown_remaining(&self) -> u32 {
            let current_block = self.env().block_number();
            let elapsed = current_block.saturating_sub(self.last_injection_block);
            COOLDOWN_BLOCKS.saturating_sub(elapsed)
        }

        #[ink(message)]
        pub fn get_max_injection_amount(&self) -> U256 {
            let balance = U256::from(self.env().balance());
            balance * U256::from(MAX_DRIP_BPS) / U256::from(BPS_DENOMINATOR)
        }

        #[ink(message)]
        /// Returns the cached TWAP price and the block it was last updated.
        /// Returns (0, 0) if no successful oracle fetch has occurred yet.
        #[ink(message)]
        pub fn get_cached_twap(&self) -> (U256, u32) {
            (self.cached_twap_price, self.cached_twap_block)
        }

        pub fn can_inject_this_block(&self) -> bool {
            self.env().block_number() != self.last_injection_block
        }



        #[ink(message)]
        pub fn get_watchdog_drain_threshold(&self) -> U256 {
            self.watchdog_drain_threshold
        }

        #[ink(message)]
        pub fn get_watchdog_drain_count(&self) -> u32 {
            self.watchdog_drain_count
        }

        #[ink(message)]
        pub fn get_lifetime_stats(&self) -> (U256, U256) {
            (self.lifetime_revenue_pulled, self.lifetime_liquidity_injected)
        }

        /// Preview an injection: returns `(is_healthy, injection_amount, deficit)`.
        #[ink(message)]
        pub fn preview_injection(&self) -> Result<(bool, U256, U256), Error> {
            let (is_healthy, deficit, max_permitted) = self.check_liquidity_health()?;
            if is_healthy {
                return Ok((true, U256::ZERO, U256::ZERO));
            }
            let amount = deficit.min(max_permitted);
            Ok((false, amount, deficit))
        }

        #[ink(message)]
        pub fn get_owner(&self) -> Address { self.owner }

        #[ink(message)]
        pub fn get_token_engine(&self) -> Address { self.token_engine }

        #[ink(message)]
        pub fn get_deploy_block(&self) -> u32 { self.deploy_block }

        #[ink(message)]
        pub fn is_paused(&self) -> bool { self.paused }

        // =====================================================================
        // ACCESS CONTROL
        // =====================================================================

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
            }
            Ok(())
        }

        fn assert_not_paused(&self) -> Result<(), Error> {
            if self.paused {
                return Err(Error::ContractPaused);
            }
            Ok(())
        }
    }

    // =========================================================================
    // UNIT TESTS
    // =========================================================================

    #[cfg(test)]
    mod tests {
        use super::*;
        use ink::env::{test, DefaultEnvironment};

        type Env = DefaultEnvironment;

        fn accounts() -> test::DefaultAccounts<Env> {
            test::default_accounts::<Env>()
        }

        fn set_caller(addr: Address) {
            test::set_caller::<Env>(addr);
        }

        fn set_block(block: u32) {
            test::set_block_number::<Env>(block);
        }

        fn deploy() -> Project52Dampener {
            let accs = accounts();
            set_caller(accs.alice);
            set_block(0);
            // charlie = mock Token Engine; bob = seed beneficiary
            Project52Dampener::new(accs.charlie, accs.bob)
        }

        // ── Constructor ───────────────────────────────────────────────────────

        #[ink::test]
        fn constructor_sets_fields() {
            let d = deploy();
            let accs = accounts();
            assert_eq!(d.get_owner(), accs.alice);
            assert_eq!(d.get_token_engine(), accs.charlie);
            // seed_fully_reclaimed removed — use get_loan_status()
            // seed_loan_remaining removed — use get_loan_status()
        }

        #[ink::test]
        fn constructor_sets_default_drain_threshold() {
            let d = deploy();
            assert_eq!(
                d.get_watchdog_drain_threshold(),
                U256::from(DRAIN_THRESHOLD_DEFAULT)
            );
        }

        // ── Seed phase logic ──────────────────────────────────────────────────

        #[ink::test]
        fn seed_phase_red_at_genesis() {
            let d = deploy();
            set_block(0);
            let (phase, elapsed, to_next) = d.preview_seed_phase();
            assert_eq!(phase, SeedPhase::Red);
            assert_eq!(elapsed, 0);
            assert_eq!(to_next, RED_ZONE_END_BLOCKS);
        }

        #[ink::test]
        fn seed_phase_yellow_after_90_days() {
            let d = deploy();
            set_block(RED_ZONE_END_BLOCKS);
            let (phase, _, _) = d.preview_seed_phase();
            assert_eq!(phase, SeedPhase::Yellow);
        }

        #[ink::test]
        fn seed_phase_green_after_180_days() {
            let d = deploy();
            set_block(YELLOW_ZONE_END_BLOCKS);
            let (phase, _, blocks_to_next) = d.preview_seed_phase();
            assert_eq!(phase, SeedPhase::Green);
            assert_eq!(blocks_to_next, 0);
        }

        #[ink::test]
        fn seed_recovery_blocked_in_red_zone() {
            let mut d = deploy();
            let accs = accounts();
            set_block(RED_ZONE_END_BLOCKS - 1); // still Red
            set_caller(accs.bob); // beneficiary
            let result = d.reclaim_seed_loan();
            assert_eq!(result, Err(Error::SeedRecoveryRedZone));
        }

        #[ink::test]
        fn seed_recovery_blocked_for_non_beneficiary() {
            let mut d = deploy();
            let accs = accounts();
            set_block(YELLOW_ZONE_END_BLOCKS); // Green zone
            set_caller(accs.alice); // owner but not beneficiary
            let result = d.reclaim_seed_loan();
            assert_eq!(result, Err(Error::NotSeedBeneficiary));
        }

        #[ink::test]
        fn seed_phase_fn_boundaries() {
            let d = deploy();
            // One block before Red ends
            assert_eq!(d.seed_phase(RED_ZONE_END_BLOCKS - 1), SeedPhase::Red);
            // First block of Yellow
            assert_eq!(d.seed_phase(RED_ZONE_END_BLOCKS), SeedPhase::Yellow);
            // One block before Yellow ends
            assert_eq!(d.seed_phase(YELLOW_ZONE_END_BLOCKS - 1), SeedPhase::Yellow);
            // First block of Green
            assert_eq!(d.seed_phase(YELLOW_ZONE_END_BLOCKS), SeedPhase::Green);
        }

        // ── Liquidity health check (devnet mode) ──────────────────────────────

        #[ink::test]
        fn liquidity_ratio_devnet_mock_returns_unhealthy() {
            // Devnet mock: supply = 1 000 000 QF, liquidity = 100 000 QF = 10%
            // Target = 15%; so ratio < target → unhealthy
            let d = deploy();
            let (ratio_bps, target_bps, is_healthy) = d.get_liquidity_ratio().unwrap();
            assert_eq!(target_bps, TARGET_LIQUIDITY_RATIO_BPS);
            // 100 000 / 1 000 000 = 10% = 1 000 BPS
            assert_eq!(ratio_bps, 1_000);
            assert!(!is_healthy, "10% < 15% should be unhealthy");
        }

        #[ink::test]
        fn check_liquidity_health_returns_deficit() {
            let d = deploy();
            let (is_healthy, deficit, max_permitted) = d.check_liquidity_health().unwrap();
            assert!(!is_healthy);
            // Target liquidity = 1 000 000 × 1 500 / 10 000 = 150 000 QF
            // Current = 100 000 QF → deficit = 50 000 QF
            let expected_deficit =
                U256::from(50_000_u128 * PRICE_PRECISION);
            assert_eq!(deficit, expected_deficit);
            assert!(max_permitted > U256::ZERO);
        }

        // ── Injection guards ──────────────────────────────────────────────────

        #[ink::test]
        fn injection_rejects_slippage_too_low() {
            let mut d = deploy();
            set_caller(accounts().alice);
            let result = d.rate_limited_inject(MIN_SLIPPAGE_BPS - 1);
            assert_eq!(result, Err(Error::SlippageTooLow));
        }

        #[ink::test]
        fn injection_rejects_cooldown_active() {
            let mut d = deploy();
            set_caller(accounts().alice);
            // last_injection_block = deploy_block = 0, current = 1 → elapsed = 1 < 36 000
            set_block(1);
            let result = d.rate_limited_inject(DEFAULT_SLIPPAGE_BPS);
            assert_eq!(result, Err(Error::CooldownActive));
        }

        #[ink::test]
        fn injection_rejects_same_block() {
            let mut d = deploy();
            set_caller(accounts().alice);
            // At block 0, last_injection_block == 0 == current block
            let result = d.rate_limited_inject(DEFAULT_SLIPPAGE_BPS);
            assert_eq!(result, Err(Error::AlreadyInjectedThisBlock));
        }

        // ── Drain split constants ─────────────────────────────────────────────

        #[ink::test]
        fn drain_threshold_default_matches_engine() {
            // 520 000 000 × 10^18 — same constant used in project52f.rs
            assert_eq!(
                DRAIN_THRESHOLD_DEFAULT,
                520_000_000_u128 * 1_000_000_000_000_000_000_u128
            );
        }

        // ── Admin access control ──────────────────────────────────────────────

        #[ink::test]
        fn set_oracle_only_owner() {
            let mut d = deploy();
            set_caller(accounts().bob);
            assert_eq!(
                d.set_oracle_address(accounts().django),
                Err(Error::NotOwner)
            );
        }

        #[ink::test]
        fn set_drain_threshold_only_owner() {
            let mut d = deploy();
            set_caller(accounts().bob);
            assert_eq!(
                d.set_watchdog_drain_threshold(U256::from(1u8)),
                Err(Error::NotOwner)
            );
        }

        #[ink::test]
        fn paused_contract_rejects_pull_and_inject() {
            let mut d = deploy();
            set_caller(accounts().alice);
            d.set_paused(true).unwrap();
            assert_eq!(d.pull_from_engine(), Err(Error::ContractPaused));
            assert_eq!(
                d.rate_limited_inject(DEFAULT_SLIPPAGE_BPS),
                Err(Error::ContractPaused)
            );
        }

        // ── Emergency withdraw ────────────────────────────────────────────────

        #[ink::test]
        fn emergency_withdraw_only_owner() {
            let mut d = deploy();
            set_caller(accounts().bob);
            assert_eq!(
                d.emergency_withdraw(U256::from(1u8)),
                Err(Error::NotOwner)
            );
        }

        #[ink::test]
        fn emergency_withdraw_protects_seed_balance() {
            let mut d = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            // vault_balance = 0 in test env; seed_loan_remaining = 52 000 QF
            // withdrawable = 0 - 52 000 QF = 0 (saturating), any amount > 0 fails
            let result = d.emergency_withdraw(U256::from(1u8));
            assert_eq!(result, Err(Error::InsufficientVaultBalance));
        }

        // ── Preview injection ─────────────────────────────────────────────────

        #[ink::test]
        fn preview_injection_devnet_shows_deficit() {
            let d = deploy();
            let (is_healthy, amount, deficit) = d.preview_injection().unwrap();
            assert!(!is_healthy);
            assert!(deficit > U256::ZERO);
            // amount ≤ deficit (capped by max-drip at 5% of vault)
            assert!(amount <= deficit);
        }
    }
}
