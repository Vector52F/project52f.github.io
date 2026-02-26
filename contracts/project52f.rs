#![cfg_attr(not(feature = "std"), no_std, no_main)]

/// # 52F Protocol — Token Engine  (v5 — Sovereign Refactor)
///
/// **Role:** Ground-truth ledger, tax collector, epoch counter,
/// asymmetric Volatility Governor, and phase-aware Great Drain executor.
///
/// ## Two-Phase Protocol Lifecycle
///
/// The protocol operates across two permanent phases determined by blocks
/// elapsed since deployment.  The phase boundary is `SHIELD_END_BLOCK`
/// (52 000 000 blocks ≈ 60 days at 0.1 s/block).
///
/// ### Phase 1 — Hardening (blocks 0 – 52 000 000)
///
/// Goal: build deep, resilient liquidity before the token reaches wider
/// distribution.  Every mechanism prioritises liquidity depth.
///
/// ```text
/// TAX ROUTING (SHIELD ACTIVE — team share redirected to Dampener):
///   BUY  (2.72%):  dampener 1.25%  |  prize 1.47%  |  team 0%
///   SELL (3.14%):  dampener 1.25%  |  prize 1.89%  |  team 0%
///
/// GREAT DRAIN (prize_pot ≥ 520 000 000 $52F):
///   Seize 50% of pot (260M tokens at threshold)
///   ├── Burn  50% of seized = 130M $52F → DEAD_ADDRESS  (supply pruning)
///   └── Pair  50% of seized = 130M $52F → lp_pair_token_reserve
///       Dampener calls claim_lp_pair_tokens() and pairs with its $QF reserves
///       creating protocol-owned liquidity (POL).
/// ```
///
/// ### Phase 2 — Scarcity (blocks > 52 000 000)
///
/// Goal: maximise deflationary pressure now that liquidity is established.
/// Team tax returns; the Dampener reverts to base-rate feed; Great Drain
/// doubles the burn.
///
/// ```text
/// TAX ROUTING (SHIELD INACTIVE):
///   BUY  (2.72%):  dampener 0.50%  |  prize 1.47%  |  team 0.75%
///   SELL (3.14%):  dampener 0.50%  |  prize 1.89%  |  team 0.75%
///
/// GREAT DRAIN:
///   Seize 50% of pot (260M tokens at threshold)
///   ├── Burn  50% of seized = 130M $52F → DEAD_ADDRESS  (supply pruning)
///   └── Burn  50% of seized = 130M $52F → DEAD_ADDRESS  (DOUBLE BURN)
///       Dampener independently burns its $QF equivalent from its own reserves.
/// ```
///
/// ## 0.5% Base Dampener Feed — Mathematical Guarantee
///
/// Under buy-dominant conditions (typical when the drain fires during a
/// price-appreciation event), the 0.5% base feed provides ≥ 1.33× the $QF$
/// required to fund the 130M-token LP pairing:
///
/// ```text
/// Proof (pure-buy scenario — worst case for dampener accumulation):
///   BUY_PRIZE_RATE  = 1.47% = 0.0147
///   DAMPENER_RATE   = 0.50% = 0.0050  (base, non-shield)
///
///   Volume V to fill prize pot to threshold T:
///     V × 0.0147 = T  →  V = T / 0.0147
///
///   Dampener accumulated at drain:
///     D = V × 0.0050 = T × (0.0050 / 0.0147) = T × 0.3401
///
///   At threshold: T = 520M × P  (P = $QF$ per $52F$ token)
///   Pair requirement: Q_needed = 130M × P  (= T × 0.25)
///
///   Ratio = D / Q_needed
///          = (520M × P × 0.3401) / (130M × P)
///          = (520 / 130) × 0.3401
///          = 4 × 0.3401
///          = 1.360×  ≥ 1.33× ✓
///
///   Shield phase multiplier (1.25% rate): 1.360 × (1.25/0.50) = 3.40×
///   Mixed 50/50 buy-sell non-shield:
///     avg prize rate = (0.0147+0.0189)/2 = 0.0168
///     ratio = 4 × (0.0050/0.0168) = 1.190×  (floor, never below ~1.19×)
///   Assumption: drain threshold is kept current with $52F spot price.
/// ```
///
/// ## Dampener Security Parameters
/// - `TWAP_WINDOW_BLOCKS = 36 000`  (1-hour smoothing at 0.1 s/block)
/// - `MAX_DRIP_BPS       = 500`     (5% of vault per injection)
/// - `COOLDOWN_BLOCKS    = 36 000`  (minimum 1 hour between injections)
///
/// **Compatibility:** ink! v6 / PolkaVM (`pallet-revive`).
///   - `AccountId` → `Address` (H160)
///   - `Balance`   → `U256`
#[ink::contract]
mod project52f {
    use ink::prelude::string::String;
    use ink::storage::Mapping;

    type Address = <ink::env::DefaultEnvironment as ink::env::Environment>::AccountId;
    use ink::primitives::U256;

    // =========================================================================
    // CONSTANTS — SOVEREIGN REFACTOR v5
    // =========================================================================

    /// Denominator for all basis-point calculations.
    pub const BPS_DENOMINATOR: u128 = 10_000;

    // ── Tax rates ─────────────────────────────────────────────────────────────

    /// Buy tax in BPS: 2.72% — Euler's number (e ≈ 2.71828…).
    pub const E_BUY_TAX_BPS: u128 = 272;

    /// Sell tax in BPS: 3.14% — Pi (π ≈ 3.14159…).
    pub const PI_SELL_TAX_BPS: u128 = 314;

    /// Team share of every transaction in BPS (0.75%).
    /// During Shield phase this is redirected to the Dampener accumulator.
    pub const TAX_TEAM_BPS: u128 = 75;

    // ── 2-WAY DAMPENER FEED ───────────────────────────────────────────────────
    //
    // BASE rate (applies to BOTH buy and sell in BOTH phases):
    //   dampener_base = 0.50%  (50 BPS)
    //
    // SHIELD bonus (blocks 0 – SHIELD_END_BLOCK): team's 0.75% is ALSO
    // redirected to the Dampener, giving:
    //   dampener_shielded = 0.50% + 0.75% = 1.25%  (125 BPS)
    //
    // Full routing table:
    //   ┌──────────────┬────────────┬────────────┬───────────┬──────────┐
    //   │ Phase/Dir    │ Total Tax  │ Dampener   │ Prize Pot │ Team     │
    //   ├──────────────┼────────────┼────────────┼───────────┼──────────┤
    //   │ Shield  BUY  │ 2.72%      │ 1.25%      │ 1.47%     │ 0%       │
    //   │ Shield  SELL │ 3.14%      │ 1.25%      │ 1.89%     │ 0%       │
    //   │ Scarcity BUY │ 2.72%      │ 0.50%      │ 1.47%     │ 0.75%    │
    //   │ Scarcity SELL│ 3.14%      │ 0.50%      │ 1.89%     │ 0.75%    │
    //   └──────────────┴────────────┴────────────┴───────────┴──────────┘
    //   Check Shield BUY:  125 + 147 + 0   = 272 ✓
    //   Check Shield SELL: 125 + 189 + 0   = 314 ✓
    //   Check Scarcity BUY:  50 + 147 + 75 = 272 ✓
    //   Check Scarcity SELL: 50 + 189 + 75 = 314 ✓

    /// Base Dampener feed: 0.50% = 50 BPS.
    /// Applied to BOTH buy and sell in every phase.
    pub const TAX_DAMPENER_BASE_BPS: u128 = 50;

    /// Dampener feed during Shield phase: 0.50% base + 0.75% team redirect
    /// = 1.25% = 125 BPS.
    pub const TAX_DAMPENER_SHIELD_BPS: u128 = 125; // 50 + 75

    /// Prize pot share of the BUY tax in BPS (1.47%).
    /// 272 − 50 − 75 = 147.  Constant in BOTH phases (prize rate is invariant).
    pub const BUY_TAX_PRIZE_BPS: u128 = 147;

    /// Prize pot share of the SELL tax in BPS (1.89%).
    /// 314 − 50 − 75 = 189.  Constant in BOTH phases.
    pub const SELL_TAX_PRIZE_BPS: u128 = 189;

    // ── PHASE BOUNDARY ────────────────────────────────────────────────────────

    /// Block count marking the end of the Hardening/Shield phase.
    /// At QF Network target of 10 blocks/second: 52 000 000 blocks ≈ 60.2 days.
    ///
    /// Before this block (relative to `deploy_block`):
    ///   - Team tax is shielded to the Dampener accumulator.
    ///   - Great Drain LP-pairs 130M $52F tokens instead of burning them.
    ///
    /// At or after this block:
    ///   - Team tax resumes normally.
    ///   - Great Drain executes a Double Burn (both halves to DEAD_ADDRESS).
    pub const SHIELD_END_BLOCK: u32 = 52_000_000;

    // ── GREAT DRAIN CONSTANTS ─────────────────────────────────────────────────

    /// Default drain threshold: 520 000 000 $52F tokens in $QF$ base units.
    /// Update via `set_drain_threshold` as the $52F spot price changes.
    pub const DEFAULT_DRAIN_THRESHOLD: u128 =
        520_000_000_u128 * 1_000_000_000_000_000_000_u128;

    /// Fraction of the prize pot seized per Great Drain (50%).
    pub const DRAIN_SEIZED_BPS: u128 = 5_000;

    /// Of the seized 50%, the fraction burned as $52F$ supply pruning (50% of seized).
    /// Both phases burn this half identically to DEAD_ADDRESS.
    pub const DRAIN_BURN_BPS: u128 = 5_000;

    /// Of the seized 50%, the fraction routed to LP pairing (Hardening) or
    /// Double Burn (Scarcity).  Equals DRAIN_SEIZED_BPS − DRAIN_BURN_BPS.
    pub const DRAIN_PAIR_BPS: u128 = 5_000; // 50% of seized = 25% of pot

    // ── DAMPENER SECURITY PARAMETERS ─────────────────────────────────────────

    /// TWAP observation window in blocks (1 hour at 0.1 s/block).
    /// Prevents flash-loan price manipulation on the Dampener's health gate.
    pub const TWAP_WINDOW_BLOCKS: u32 = 36_000;

    /// Maximum fraction of Dampener Vault balance injectable per call (5%).
    /// Prevents bot-exit pumps by capping single-injection size.
    pub const MAX_DRIP_BPS: u128 = 500;

    /// Minimum blocks between consecutive liquidity injections (≈ 1 hour).
    pub const COOLDOWN_BLOCKS: u32 = 36_000;

    // ── MISC ──────────────────────────────────────────────────────────────────

    /// Transactions that constitute one epoch.
    pub const EPOCH_SIZE: u32 = 52;

    /// Minimum valid transaction size ($QF$ base units, 18 decimals = 1 token).
    pub const MIN_TRANSACTION_THRESHOLD: u128 = 1_000_000_000_000_000_000;

    /// Canonical EVM dead/burn address: 0x000…dEaD.
    pub const DEAD_ADDRESS: [u8; 20] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xdE, 0xaD,
    ];

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct Project52F {
        // ── Token metadata ────────────────────────────────────────────────
        name: String,
        symbol: String,
        decimals: u8,
        total_supply: U256,

        // ── Ledger ────────────────────────────────────────────────────────
        balances: Mapping<Address, U256>,
        allowances: Mapping<(Address, Address), U256>,

        // ── Access control ────────────────────────────────────────────────
        owner: Address,

        // ── Phase tracking ────────────────────────────────────────────────
        /// Block number at deployment.  Used to compute the current phase:
        /// `is_hardening = current_block < deploy_block + SHIELD_END_BLOCK`.
        deploy_block: u32,

        // ── Satellite socket ──────────────────────────────────────────────
        /// Address of the authorised Sequencer Satellite contract.
        sequencer_satellite: Option<Address>,

        // ── Dampener socket (Pillar 1) ────────────────────────────────────
        /// Address of the authorised project52Dampener contract.
        dampener_address: Option<Address>,

        // ── Tax accumulators ──────────────────────────────────────────────
        /// Taxes earmarked for the team multisig.
        /// Zero during Shield phase (redirected to Dampener).
        team_tax_accumulated: U256,
        /// Prize pot available for the Sequencer Satellite to pull.
        prize_pot_accumulated: U256,
        /// Dampener feed: 0.50% of EVERY buy and sell (plus 0.75% team redirect
        /// during Shield phase).  Transferred to Dampener Vault on pull.
        dampener_tax_accumulated: U256,

        // ── Great Drain Governor ──────────────────────────────────────────
        /// Prize-pot ceiling in $QF$ base units.  Update when $52F price moves.
        prize_drain_threshold: U256,
        /// $52F$ tokens queued for LP pairing (Hardening) or awaiting
        /// confirmation that Double Burn has executed (Scarcity).
        /// Decremented when Dampener calls `claim_lp_pair_tokens`.
        lp_pair_token_reserve: U256,
        /// Running count of Great Drain events.
        drain_event_count: u32,

        // ── Epoch counter ─────────────────────────────────────────────────
        epoch_transaction_counter: u32,
        epoch_id: u32,

        // ── Safety ───────────────────────────────────────────────────────
        paused: bool,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct Transfer {
        #[ink(topic)]
        from: Option<Address>,
        #[ink(topic)]
        to: Option<Address>,
        value: U256,
    }

    #[ink(event)]
    pub struct Approval {
        #[ink(topic)]
        owner: Address,
        #[ink(topic)]
        spender: Address,
        value: U256,
    }

    /// Emitted when the epoch counter rolls over at 52 transactions.
    /// The Sequencer Satellite listens for this to trigger collision detection.
    #[ink(event)]
    pub struct EpochReady {
        #[ink(topic)]
        epoch_id: u32,
        prize_pot: U256,
    }

    /// Emitted when the Sequencer Satellite successfully pulls the prize pot.
    #[ink(event)]
    pub struct PrizePotPulled {
        #[ink(topic)]
        epoch_id: u32,
        burn_amount: U256,
        yield_amount: U256,
        satellite: Address,
    }

    /// Emitted whenever a Great Drain is triggered.
    ///
    /// | Field              | Meaning                                           |
    /// |--------------------|---------------------------------------------------|
    /// | `drain_id`         | Monotonic drain counter (1-indexed)               |
    /// | `is_hardening`     | `true` = LP pair; `false` = Double Burn           |
    /// | `pot_before`       | Prize pot immediately before the drain            |
    /// | `seized_52f`       | 50% of `pot_before` taken by the governor         |
    /// | `burned_52f`       | First half of seized: sent to `DEAD_ADDRESS`      |
    /// | `paired_52f`       | Second half: queued for LP pair (Hardening) or    |
    /// |                    |   also burned (Scarcity — Double Burn)            |
    /// | `pot_remaining`    | 50% of `pot_before` left for winners              |
    #[ink(event)]
    pub struct GreatDrain {
        #[ink(topic)]
        drain_id: u32,
        is_hardening: bool,
        pot_before: U256,
        seized_52f: U256,
        burned_52f: U256,
        paired_52f: U256,
        pot_remaining: U256,
    }

    /// Emitted when the Dampener claims $52F$ tokens for LP pairing (Hardening
    /// phase only).  The Dampener pairs these tokens with its own $QF$ reserves
    /// to create protocol-owned liquidity.
    #[ink(event)]
    pub struct LpPairClaimed {
        #[ink(topic)]
        drain_id: u32,
        dampener: Address,
        tokens_52f: U256,
    }

    /// Emitted when the drain threshold is updated by the owner.
    #[ink(event)]
    pub struct DrainThresholdUpdated {
        previous: U256,
        updated: U256,
    }

    /// Emitted at the first transaction that crosses the Hardening→Scarcity
    /// boundary, giving indexers a precise on-chain record of the transition.
    #[ink(event)]
    pub struct PhaseTransitioned {
        from_block: u32,
        shield_end_block: u32,
    }

    #[ink(event)]
    pub struct SequencerUpdated {
        #[ink(topic)]
        new_satellite: Address,
    }

    /// Emitted when the Dampener address is registered or updated.
    #[ink(event)]
    pub struct DampenerUpdated {
        #[ink(topic)]
        new_dampener: Address,
    }

    /// Emitted when the Dampener Vault pulls its accumulated sell-tax reserve.
    #[ink(event)]
    pub struct DampenerTaxPulled {
        #[ink(topic)]
        dampener: Address,
        amount: U256,
    }

    /// Emitted when the Dampener watchdog calls `request_great_drain` and the
    /// drain executes.  Complements the auto-drain already fired on buy/sell.
    #[ink(event)]
    pub struct WatchdogDrainExecuted {
        #[ink(topic)]
        drain_id: u32,
        triggered_by: Address,
        pot_before: U256,
    }

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        /// Caller is not the contract owner.
        NotOwner,
        /// Caller is not the registered Sequencer Satellite.
        NotSequencerSatellite,
        /// No Sequencer Satellite has been registered yet.
        NoSatelliteRegistered,
        /// Caller is not the registered Dampener (Pillar 1).
        NotDampener,
        /// No Dampener has been registered yet.
        NoDampenerRegistered,
        /// Sender's $QF$ balance is insufficient.
        InsufficientBalance,
        /// Spender's allowance is insufficient.
        InsufficientAllowance,
        /// Transaction value is below the 1 $QF minimum threshold.
        TransactionTooSmall,
        /// The prize pot is empty; nothing to pull.
        PrizePotEmpty,
        /// The dampener accumulator is empty; nothing to pull.
        DampenerPotEmpty,
        /// Requested LP pair claim exceeds `lp_pair_token_reserve`.
        InsufficientLpPairReserve,
        /// LP pair claim is only permitted during the Hardening phase.
        NotHardeningPhase,
        /// An arithmetic operation overflowed.
        Overflow,
        /// A native value transfer failed.
        TransferFailed,
        /// Contract is paused.
        ContractPaused,
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52F {
        // ---------------------------------------------------------------------
        // Constructor
        // ---------------------------------------------------------------------

        /// Deploy the Token Engine.
        ///
        /// Mints `initial_supply` entirely to the deployer.
        /// The Great Drain threshold is initialised to `DEFAULT_DRAIN_THRESHOLD`
        /// and can be updated via `set_drain_threshold` as the $52F price moves.
        #[ink(constructor)]
        pub fn new(initial_supply: U256, name: String, symbol: String) -> Self {
            let caller = Self::env().caller();
            let deploy_block = Self::env().block_number();
            let mut balances = Mapping::default();
            balances.insert(caller, &initial_supply);

            Self::env().emit_event(Transfer {
                from: None,
                to: Some(caller),
                value: initial_supply,
            });

            Self {
                name,
                symbol,
                decimals: 18,
                total_supply: initial_supply,
                balances,
                allowances: Mapping::default(),
                owner: caller,
                deploy_block,
                sequencer_satellite: None,
                dampener_address: None,
                team_tax_accumulated: U256::ZERO,
                prize_pot_accumulated: U256::ZERO,
                dampener_tax_accumulated: U256::ZERO,
                prize_drain_threshold: U256::from(DEFAULT_DRAIN_THRESHOLD),
                lp_pair_token_reserve: U256::ZERO,
                drain_event_count: 0,
                epoch_transaction_counter: 0,
                epoch_id: 0,
                paused: false,
            }
        }

        // =====================================================================
        // THE GATEKEEPER — Buy & Sell
        // =====================================================================

        /// Process a buy transaction.
        ///
        /// Tax routing is phase-dependent (see module doc for full table):
        ///
        /// ```text
        /// HARDENING (Shield active — block < deploy_block + 52 000 000):
        ///   dampener_share = amount × 125 / 10_000   (1.25% — base + team redirect)
        ///   prize_share    = amount × 147 / 10_000   (1.47%)
        ///   team_share     = 0
        ///   total_tax      = 272 BPS ✓
        ///
        /// SCARCITY (Shield inactive):
        ///   dampener_share = amount × 50  / 10_000   (0.50%)
        ///   prize_share    = amount × 147 / 10_000   (1.47%)
        ///   team_share     = amount × 75  / 10_000   (0.75%)
        ///   total_tax      = 272 BPS ✓
        /// ```
        #[ink(message)]
        pub fn buy(&mut self, amount: U256) -> Result<(), Error> {
            self.assert_not_paused()?;
            self.assert_above_threshold(amount)?;

            let caller = self.env().caller();
            let denom = U256::from(BPS_DENOMINATOR);
            let hardening = self.is_hardening_phase_internal();

            let dampener_bps = if hardening {
                TAX_DAMPENER_SHIELD_BPS
            } else {
                TAX_DAMPENER_BASE_BPS
            };

            let dampener_share = amount
                .checked_mul(U256::from(dampener_bps))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            let prize_share = amount
                .checked_mul(U256::from(BUY_TAX_PRIZE_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            let team_share = if hardening {
                U256::ZERO
            } else {
                amount
                    .checked_mul(U256::from(TAX_TEAM_BPS))
                    .ok_or(Error::Overflow)?
                    .checked_div(denom)
                    .ok_or(Error::Overflow)?
            };

            let total_tax = dampener_share
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?
                .checked_add(team_share)
                .ok_or(Error::Overflow)?;

            let net_amount = amount.saturating_sub(total_tax);

            self.debit_balance(caller, amount)?;

            self.dampener_tax_accumulated = self
                .dampener_tax_accumulated
                .checked_add(dampener_share)
                .ok_or(Error::Overflow)?;
            self.prize_pot_accumulated = self
                .prize_pot_accumulated
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?;
            if !team_share.is_zero() {
                self.team_tax_accumulated = self
                    .team_tax_accumulated
                    .checked_add(team_share)
                    .ok_or(Error::Overflow)?;
            }

            self.credit_balance(caller, net_amount)?;

            self.env().emit_event(Transfer {
                from: Some(caller),
                to: Some(self.env().account_id()),
                value: total_tax,
            });

            self.check_and_execute_drain()?;
            self.tick_epoch_counter();
            Ok(())
        }

        /// Process a sell transaction.
        ///
        /// Tax routing is phase-dependent (see module doc for full table):
        ///
        /// ```text
        /// HARDENING (Shield active):
        ///   dampener_share = amount × 125 / 10_000   (1.25%)
        ///   prize_share    = amount × 189 / 10_000   (1.89%)
        ///   team_share     = 0
        ///   total_tax      = 314 BPS ✓
        ///
        /// SCARCITY (Shield inactive):
        ///   dampener_share = amount × 50  / 10_000   (0.50%)
        ///   prize_share    = amount × 189 / 10_000   (1.89%)
        ///   team_share     = amount × 75  / 10_000   (0.75%)
        ///   total_tax      = 314 BPS ✓
        /// ```
        #[ink(message)]
        pub fn sell(&mut self, amount: U256) -> Result<(), Error> {
            self.assert_not_paused()?;
            self.assert_above_threshold(amount)?;

            let caller = self.env().caller();
            let denom = U256::from(BPS_DENOMINATOR);
            let hardening = self.is_hardening_phase_internal();

            let dampener_bps = if hardening {
                TAX_DAMPENER_SHIELD_BPS
            } else {
                TAX_DAMPENER_BASE_BPS
            };

            let dampener_share = amount
                .checked_mul(U256::from(dampener_bps))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            let prize_share = amount
                .checked_mul(U256::from(SELL_TAX_PRIZE_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            let team_share = if hardening {
                U256::ZERO
            } else {
                amount
                    .checked_mul(U256::from(TAX_TEAM_BPS))
                    .ok_or(Error::Overflow)?
                    .checked_div(denom)
                    .ok_or(Error::Overflow)?
            };

            let total_tax = dampener_share
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?
                .checked_add(team_share)
                .ok_or(Error::Overflow)?;

            let net_amount = amount.saturating_sub(total_tax);

            self.debit_balance(caller, amount)?;

            self.dampener_tax_accumulated = self
                .dampener_tax_accumulated
                .checked_add(dampener_share)
                .ok_or(Error::Overflow)?;
            self.prize_pot_accumulated = self
                .prize_pot_accumulated
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?;
            if !team_share.is_zero() {
                self.team_tax_accumulated = self
                    .team_tax_accumulated
                    .checked_add(team_share)
                    .ok_or(Error::Overflow)?;
            }

            self.credit_balance(caller, net_amount)?;

            self.env().emit_event(Transfer {
                from: Some(caller),
                to: Some(self.env().account_id()),
                value: total_tax,
            });

            self.check_and_execute_drain()?;
            self.tick_epoch_counter();
            Ok(())
        }

        // =====================================================================
        // GREAT DRAIN — Phase-Aware Volatility Governor
        // =====================================================================

        /// Test the prize pot against the drain threshold; execute if breached.
        ///
        /// ## Hardening phase (blocks < deploy + 52 000 000)
        ///
        /// ```text
        /// pot      = prize_pot_accumulated
        /// seized   = pot × 50%                          (DRAIN_SEIZED_BPS)
        /// burned   = seized × 50% → DEAD_ADDRESS         (supply pruning)
        /// paired   = seized × 50% → lp_pair_token_reserve
        ///            Dampener calls claim_lp_pair_tokens() + pairs with its QF
        /// remaining = pot − seized
        /// ```
        ///
        /// ## Scarcity phase (blocks ≥ deploy + 52 000 000)
        ///
        /// ```text
        /// seized   = pot × 50%
        /// burned   = seized × 50% → DEAD_ADDRESS         (first burn)
        /// doubled  = seized × 50% → DEAD_ADDRESS         (DOUBLE BURN)
        /// paired   = 0  (lp_pair_token_reserve unchanged)
        /// remaining = pot − seized
        ///             Dampener independently burns QF equivalent from its own reserves.
        /// ```
        ///
        /// State is updated before external calls (checks-effects-interactions).
        fn check_and_execute_drain(&mut self) -> Result<(), Error> {
            if self.prize_pot_accumulated < self.prize_drain_threshold {
                return Ok(());
            }

            let hardening = self.is_hardening_phase_internal();
            let pot = self.prize_pot_accumulated;
            let denom = U256::from(BPS_DENOMINATOR);

            // seized = 50% of pot
            let seized = pot
                .checked_mul(U256::from(DRAIN_SEIZED_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            // burned_52f = 50% of seized (always — both phases)
            let burned_52f = seized
                .checked_mul(U256::from(DRAIN_BURN_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            // paired_52f = remaining seized half
            let paired_52f = seized.saturating_sub(burned_52f);
            let pot_remaining = pot.saturating_sub(seized);

            // ── State update (before external calls) ──────────────────────
            self.prize_pot_accumulated = pot_remaining;
            self.drain_event_count = self.drain_event_count.saturating_add(1);
            let drain_id = self.drain_event_count;

            // First burn: always executed in both phases.
            self.total_supply = self.total_supply.saturating_sub(burned_52f);
            let dead = Address::from(DEAD_ADDRESS);
            self.env()
                .transfer(dead, burned_52f)
                .map_err(|_| Error::TransferFailed)?;

            if hardening {
                // Hardening: queue paired_52f for LP pairing by Dampener.
                self.lp_pair_token_reserve = self
                    .lp_pair_token_reserve
                    .checked_add(paired_52f)
                    .ok_or(Error::Overflow)?;
            } else {
                // Scarcity: Double Burn — paired_52f is also burned.
                self.total_supply = self.total_supply.saturating_sub(paired_52f);
                self.env()
                    .transfer(dead, paired_52f)
                    .map_err(|_| Error::TransferFailed)?;
            }

            self.env().emit_event(GreatDrain {
                drain_id,
                is_hardening: hardening,
                pot_before: pot,
                seized_52f: seized,
                burned_52f,
                paired_52f,
                pot_remaining,
            });

            Ok(())
        }

        /// Dampener Vault claims $52F$ tokens from `lp_pair_token_reserve`
        /// to create protocol-owned liquidity (POL).
        ///
        /// **Caller:** Registered `dampener_address` only.
        /// **Phase:**  Hardening only.  Reverts with `NotHardeningPhase` in Scarcity.
        ///
        /// The Dampener pairs the received $52F$ with $QF$ from its own
        /// accumulated reserves and calls the DEX router to add liquidity.
        /// Emits `LpPairClaimed` for off-chain indexers.
        ///
        /// # Errors
        /// - [`Error::NotHardeningPhase`]       — phase boundary has passed.
        /// - [`Error::NotDampener`]              — caller is not the Dampener.
        /// - [`Error::InsufficientLpPairReserve`] — amount exceeds reserve.
        #[ink(message)]
        pub fn claim_lp_pair_tokens(&mut self, amount: U256) -> Result<U256, Error> {
            self.assert_not_paused()?;

            if !self.is_hardening_phase_internal() {
                return Err(Error::NotHardeningPhase);
            }

            let caller = self.env().caller();
            let dampener = self
                .dampener_address
                .ok_or(Error::NoDampenerRegistered)?;
            if caller != dampener {
                return Err(Error::NotDampener);
            }

            if amount > self.lp_pair_token_reserve {
                return Err(Error::InsufficientLpPairReserve);
            }

            let drain_id = self.drain_event_count;

            // State update before transfer.
            self.lp_pair_token_reserve = self
                .lp_pair_token_reserve
                .saturating_sub(amount);

            self.env()
                .transfer(dampener, amount)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(LpPairClaimed {
                drain_id,
                dampener,
                tokens_52f: amount,
            });

            Ok(amount)
        }

        // =====================================================================
        // SATELLITE SOCKET — Prize Pot Pull
        // =====================================================================

        /// Pull the prize pot from the Token Engine.
        ///
        /// **Caller:** Must be the registered `sequencer_satellite`.
        ///
        /// **Split:**
        /// - 10% → burned to `0x000…dEaD`
        /// - 90% → transferred to the calling Satellite for winner distribution
        ///
        /// State is updated before transfers (checks-effects-interactions).
        #[ink(message)]
        pub fn pull_prize_tax(&mut self) -> Result<U256, Error> {
            self.assert_not_paused()?;

            let caller = self.env().caller();
            let satellite = self
                .sequencer_satellite
                .ok_or(Error::NoSatelliteRegistered)?;

            if caller != satellite {
                return Err(Error::NotSequencerSatellite);
            }

            let total_pot = self.prize_pot_accumulated;
            if total_pot.is_zero() {
                return Err(Error::PrizePotEmpty);
            }

            let burn_amount = total_pot
                .checked_div(U256::from(10u8))
                .ok_or(Error::Overflow)?;
            let yield_amount = total_pot.saturating_sub(burn_amount);

            // ── State update (before external calls) ──────────────────────
            self.prize_pot_accumulated = U256::ZERO;
            let epoch_id = self.epoch_id;

            let dead = Address::from(DEAD_ADDRESS);
            self.env()
                .transfer(dead, burn_amount)
                .map_err(|_| Error::TransferFailed)?;

            self.env()
                .transfer(satellite, yield_amount)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(PrizePotPulled {
                epoch_id,
                burn_amount,
                yield_amount,
                satellite,
            });

            Ok(yield_amount)
        }

        // =====================================================================
        // DAMPENER SOCKET (Pillar 1) — Tax Pull & Watchdog Drain
        // =====================================================================

        /// Pull the accumulated 1% sell-tax reserve to the Dampener Vault.
        ///
        /// **Caller:** Must be the registered `dampener_address` (Pillar 1).
        ///
        /// The entire `dampener_tax_accumulated` balance is transferred to the
        /// calling Dampener in one atomic operation.  The Dampener then uses
        /// these $QF$ reserves to "Buy the Floor" when the TWAP liquidity ratio
        /// drops below 15%.
        ///
        /// State is updated before the transfer (checks-effects-interactions).
        ///
        /// # Errors
        /// - [`Error::NoDampenerRegistered`]  — no Dampener has been set.
        /// - [`Error::NotDampener`]           — caller is not the Dampener.
        /// - [`Error::DampenerPotEmpty`]      — accumulator is empty.
        #[ink(message)]
        pub fn pull_dampener_tax(&mut self) -> Result<U256, Error> {
            self.assert_not_paused()?;

            let caller = self.env().caller();
            let dampener = self
                .dampener_address
                .ok_or(Error::NoDampenerRegistered)?;

            if caller != dampener {
                return Err(Error::NotDampener);
            }

            let amount = self.dampener_tax_accumulated;
            if amount.is_zero() {
                return Err(Error::DampenerPotEmpty);
            }

            // ── State update (before external call) ───────────────────────
            self.dampener_tax_accumulated = U256::ZERO;

            self.env()
                .transfer(dampener, amount)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(DampenerTaxPulled {
                dampener,
                amount,
            });

            Ok(amount)
        }

        /// External watchdog entry-point for the Dampener to trigger a Great Drain.
        ///
        /// **Caller:** Must be the registered `dampener_address` (Pillar 1).
        ///
        /// The Token Engine already auto-drains on every `buy`/`sell` via
        /// `check_and_execute_drain`.  This message provides a belt-and-braces
        /// watchdog so the Dampener can force a drain check between transactions
        /// — for example, after an admin update to the drain threshold, or when
        /// a keeper detects the pot has grown above the threshold without a
        /// recent transaction to trigger the auto-drain.
        ///
        /// Emits [`WatchdogDrainExecuted`] in addition to the standard
        /// [`GreatDrain`] event (which fires inside `check_and_execute_drain`).
        ///
        /// # Errors
        /// - [`Error::NoDampenerRegistered`] — no Dampener set.
        /// - [`Error::NotDampener`]          — caller is not the Dampener.
        /// - [`Error::ContractPaused`]       — contract is paused.
        #[ink(message)]
        pub fn request_great_drain(&mut self) -> Result<(), Error> {
            self.assert_not_paused()?;

            let caller = self.env().caller();
            let dampener = self
                .dampener_address
                .ok_or(Error::NoDampenerRegistered)?;

            if caller != dampener {
                return Err(Error::NotDampener);
            }

            let pot_before = self.prize_pot_accumulated;

            // Delegate to the shared internal drain logic.
            // If the pot is below threshold, check_and_execute_drain returns Ok(())
            // without executing; no error is returned so the watchdog call
            // itself succeeds (the Dampener can decide whether to retry).
            self.check_and_execute_drain()?;

            // Only emit the watchdog event if a drain actually fired
            // (drain_event_count incremented inside check_and_execute_drain).
            if self.prize_pot_accumulated < pot_before {
                self.env().emit_event(WatchdogDrainExecuted {
                    drain_id: self.drain_event_count,
                    triggered_by: caller,
                    pot_before,
                });
            }

            Ok(())
        }

        // =====================================================================
        // PSP22 — Standard Token Interface
        // =====================================================================

        #[ink(message)]
        pub fn total_supply(&self) -> U256 { self.total_supply }

        #[ink(message)]
        pub fn balance_of(&self, account: Address) -> U256 {
            self.balances.get(account).unwrap_or(U256::ZERO)
        }

        #[ink(message)]
        pub fn allowance(&self, owner: Address, spender: Address) -> U256 {
            self.allowances.get((owner, spender)).unwrap_or(U256::ZERO)
        }

        #[ink(message)]
        pub fn transfer(&mut self, to: Address, value: U256) -> Result<(), Error> {
            self.assert_not_paused()?;
            let from = self.env().caller();
            self.transfer_impl(from, to, value)
        }

        #[ink(message)]
        pub fn approve(&mut self, spender: Address, value: U256) -> Result<(), Error> {
            let owner = self.env().caller();
            self.allowances.insert((owner, spender), &value);
            self.env().emit_event(Approval { owner, spender, value });
            Ok(())
        }

        #[ink(message)]
        pub fn transfer_from(
            &mut self,
            from: Address,
            to: Address,
            value: U256,
        ) -> Result<(), Error> {
            self.assert_not_paused()?;
            let caller = self.env().caller();
            let current_allowance = self.allowance(from, caller);
            if current_allowance < value {
                return Err(Error::InsufficientAllowance);
            }
            self.allowances
                .insert((from, caller), &current_allowance.saturating_sub(value));
            self.transfer_impl(from, to, value)
        }

        // =====================================================================
        // VIEW FUNCTIONS
        // =====================================================================

        #[ink(message)]
        pub fn name(&self) -> String { self.name.clone() }

        #[ink(message)]
        pub fn symbol(&self) -> String { self.symbol.clone() }

        #[ink(message)]
        pub fn decimals(&self) -> u8 { self.decimals }

        #[ink(message)]
        pub fn get_prize_pot(&self) -> U256 { self.prize_pot_accumulated }

        #[ink(message)]
        pub fn get_team_accumulated(&self) -> U256 { self.team_tax_accumulated }

        #[ink(message)]
        pub fn get_epoch_counter(&self) -> u32 { self.epoch_transaction_counter }

        #[ink(message)]
        pub fn get_epoch_id(&self) -> u32 { self.epoch_id }

        #[ink(message)]
        pub fn get_sequencer_satellite(&self) -> Option<Address> { self.sequencer_satellite }

        #[ink(message)]
        pub fn get_dampener_address(&self) -> Option<Address> { self.dampener_address }

        /// Returns the Dampener's accumulated feed.
        /// Grows at 1.25% (Shield) or 0.50% (Scarcity) of all volume.
        #[ink(message)]
        pub fn get_dampener_accumulated(&self) -> U256 { self.dampener_tax_accumulated }

        /// Returns $52F$ tokens currently queued for LP pairing.
        /// Non-zero only during Hardening; zeroed out as Dampener claims and pairs.
        #[ink(message)]
        pub fn get_lp_pair_token_reserve(&self) -> U256 { self.lp_pair_token_reserve }

        #[ink(message)]
        pub fn get_drain_threshold(&self) -> U256 { self.prize_drain_threshold }

        #[ink(message)]
        pub fn get_drain_event_count(&self) -> u32 { self.drain_event_count }

        /// Returns `true` if the Shield / Hardening phase is still active.
        #[ink(message)]
        pub fn is_hardening_phase(&self) -> bool {
            self.is_hardening_phase_internal()
        }

        /// Returns the block at which the Scarcity phase begins.
        #[ink(message)]
        pub fn scarcity_start_block(&self) -> u32 {
            self.deploy_block.saturating_add(SHIELD_END_BLOCK)
        }

        /// Returns blocks remaining until Scarcity phase.  Zero if already in Scarcity.
        #[ink(message)]
        pub fn blocks_until_scarcity(&self) -> u32 {
            let boundary = self.deploy_block.saturating_add(SHIELD_END_BLOCK);
            let current = self.env().block_number();
            if current >= boundary { 0 } else { boundary - current }
        }

        #[ink(message)]
        pub fn get_deploy_block(&self) -> u32 { self.deploy_block }

        #[ink(message)]
        pub fn is_paused(&self) -> bool { self.paused }

        // =====================================================================
        // ADMIN
        // =====================================================================

        #[ink(message)]
        pub fn set_sequencer_satellite(&mut self, addr: Address) -> Result<(), Error> {
            self.only_owner()?;
            self.sequencer_satellite = Some(addr);
            self.env().emit_event(SequencerUpdated { new_satellite: addr });
            Ok(())
        }

        #[ink(message)]
        pub fn clear_sequencer_satellite(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            self.sequencer_satellite = None;
            Ok(())
        }

        /// Register the authorised Dampener Vault address (Pillar 1).
        ///
        /// Only this address may call `pull_dampener_tax` and
        /// `request_great_drain`.  Deploy the Dampener first, then call this
        /// function with the Dampener's contract address.
        #[ink(message)]
        pub fn set_dampener_address(&mut self, addr: Address) -> Result<(), Error> {
            self.only_owner()?;
            self.dampener_address = Some(addr);
            self.env().emit_event(DampenerUpdated { new_dampener: addr });
            Ok(())
        }

        #[ink(message)]
        pub fn clear_dampener_address(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            self.dampener_address = None;
            Ok(())
        }

        /// Update the Great Drain threshold.
        ///
        /// Should represent 520 000 000 $52F tokens denominated in $QF$ base
        /// units at the current spot price.  Call periodically via a keeper or
        /// governance vote as the $52F price changes.
        #[ink(message)]
        pub fn set_drain_threshold(&mut self, new_threshold: U256) -> Result<(), Error> {
            self.only_owner()?;
            let previous = self.prize_drain_threshold;
            self.prize_drain_threshold = new_threshold;
            self.env().emit_event(DrainThresholdUpdated {
                previous,
                updated: new_threshold,
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
        // INTERNAL HELPERS
        // =====================================================================

        fn tick_epoch_counter(&mut self) {
            self.epoch_transaction_counter =
                self.epoch_transaction_counter.saturating_add(1);

            if self.epoch_transaction_counter >= EPOCH_SIZE {
                self.epoch_id = self.epoch_id.saturating_add(1);
                self.epoch_transaction_counter = 0;

                self.env().emit_event(EpochReady {
                    epoch_id: self.epoch_id,
                    prize_pot: self.prize_pot_accumulated,
                });
            }
        }

        /// Returns `true` while `current_block < deploy_block + SHIELD_END_BLOCK`.
        /// All phase-dependent logic delegates to this single source of truth.
        fn is_hardening_phase_internal(&self) -> bool {
            let current = self.env().block_number();
            let boundary = self.deploy_block.saturating_add(SHIELD_END_BLOCK);
            current < boundary
        }

        fn transfer_impl(&mut self, from: Address, to: Address, value: U256) -> Result<(), Error> {
            self.debit_balance(from, value)?;
            self.credit_balance(to, value)?;
            self.env().emit_event(Transfer {
                from: Some(from),
                to: Some(to),
                value,
            });
            Ok(())
        }

        fn debit_balance(&mut self, account: Address, amount: U256) -> Result<(), Error> {
            let balance = self.balances.get(account).unwrap_or(U256::ZERO);
            if balance < amount {
                return Err(Error::InsufficientBalance);
            }
            self.balances.insert(account, &balance.saturating_sub(amount));
            Ok(())
        }

        fn credit_balance(&mut self, account: Address, amount: U256) -> Result<(), Error> {
            let balance = self.balances.get(account).unwrap_or(U256::ZERO);
            let new_balance = balance.checked_add(amount).ok_or(Error::Overflow)?;
            self.balances.insert(account, &new_balance);
            Ok(())
        }

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

        fn assert_above_threshold(&self, amount: U256) -> Result<(), Error> {
            if amount < U256::from(MIN_TRANSACTION_THRESHOLD) {
                return Err(Error::TransactionTooSmall);
            }
            Ok(())
        }
    }


    // =========================================================================
    // UNIT TESTS — v5 Sovereign Refactor
    // =========================================================================
    //
    // Tags:
    //   [H] = Hardening  (block 0 – SHIELD_END_BLOCK − 1, deploy_block = 0)
    //   [S] = Scarcity   (block ≥ SHIELD_END_BLOCK)
    //   [*] = Phase-invariant

    #[cfg(test)]
    mod tests {
        use super::*;
        use ink::env::{test, DefaultEnvironment};

        type Env = DefaultEnvironment;

        fn accounts() -> test::DefaultAccounts<Env> { test::default_accounts::<Env>() }
        fn set_caller(a: Address) { test::set_caller::<Env>(a); }
        fn set_block(n: u32)      { test::set_block_number::<Env>(n); }

        const ONE_QF: u128 = 1_000_000_000_000_000_000;
        const SUPPLY:  u128 = 1_000_000 * ONE_QF;

        fn deploy() -> Project52F {
            set_block(0);
            set_caller(accounts().alice);
            Project52F::new(U256::from(SUPPLY), "Project 52F".into(), "QF".into())
        }
        fn enter_scarcity() { set_block(SHIELD_END_BLOCK); }

        // ── Tax routing table — constant integrity [*] ────────────────────────

        #[ink::test]
        fn constants_shield_buy_sums_to_e_bps() {
            // 125 (dampener_shield) + 147 (prize) = 272 ✓
            assert_eq!(TAX_DAMPENER_SHIELD_BPS + BUY_TAX_PRIZE_BPS, E_BUY_TAX_BPS);
        }

        #[ink::test]
        fn constants_shield_sell_sums_to_pi_bps() {
            // 125 + 189 = 314 ✓
            assert_eq!(TAX_DAMPENER_SHIELD_BPS + SELL_TAX_PRIZE_BPS, PI_SELL_TAX_BPS);
        }

        #[ink::test]
        fn constants_scarcity_buy_sums_to_e_bps() {
            // 50 (dampener_base) + 147 (prize) + 75 (team) = 272 ✓
            assert_eq!(TAX_DAMPENER_BASE_BPS + BUY_TAX_PRIZE_BPS + TAX_TEAM_BPS, E_BUY_TAX_BPS);
        }

        #[ink::test]
        fn constants_scarcity_sell_sums_to_pi_bps() {
            // 50 + 189 + 75 = 314 ✓
            assert_eq!(TAX_DAMPENER_BASE_BPS + SELL_TAX_PRIZE_BPS + TAX_TEAM_BPS, PI_SELL_TAX_BPS);
        }

        #[ink::test]
        fn constants_shield_end_block() {
            assert_eq!(SHIELD_END_BLOCK, 52_000_000);
        }

        #[ink::test]
        fn constants_drain_split_50_25_25() {
            // seized = 50%, burned = 25%, paired/doubled = 25%, remaining = 50%
            let pot       = U256::from(1_000_000u64);
            let seized    = pot    * U256::from(DRAIN_SEIZED_BPS) / U256::from(BPS_DENOMINATOR);
            let burned    = seized * U256::from(DRAIN_BURN_BPS)   / U256::from(BPS_DENOMINATOR);
            let paired    = seized.saturating_sub(burned);
            let remaining = pot.saturating_sub(seized);
            assert_eq!(seized,    U256::from(500_000u64), "50% seized");
            assert_eq!(burned,    U256::from(250_000u64), "25% burned");
            assert_eq!(paired,    U256::from(250_000u64), "25% paired/doubled");
            assert_eq!(remaining, U256::from(500_000u64), "50% pot remains");
        }

        // ── Phase detection [H/S] ─────────────────────────────────────────────

        #[ink::test]
        fn phase_hardening_at_genesis() {
            assert!(deploy().is_hardening_phase());
        }

        #[ink::test]
        fn phase_hardening_at_shield_end_minus_one() {
            let e = deploy();
            set_block(SHIELD_END_BLOCK - 1);
            assert!(e.is_hardening_phase());
        }

        #[ink::test]
        fn phase_scarcity_at_shield_end_block() {
            let e = deploy();
            enter_scarcity();
            assert!(!e.is_hardening_phase());
        }

        #[ink::test]
        fn scarcity_start_block_view() {
            assert_eq!(deploy().scarcity_start_block(), SHIELD_END_BLOCK);
        }

        #[ink::test]
        fn blocks_until_scarcity_decrements() {
            let e = deploy();
            set_block(100);
            assert_eq!(e.blocks_until_scarcity(), SHIELD_END_BLOCK - 100);
        }

        #[ink::test]
        fn blocks_until_scarcity_zero_in_scarcity() {
            let e = deploy();
            enter_scarcity();
            assert_eq!(e.blocks_until_scarcity(), 0);
        }

        // ── BUY — exact amounts, Hardening [H] ───────────────────────────────

        #[ink::test]
        fn buy_below_threshold_rejected() {
            let mut e = deploy();
            set_caller(accounts().alice);
            assert_eq!(e.buy(U256::from(ONE_QF - 1)), Err(Error::TransactionTooSmall));
        }

        #[ink::test]
        fn buy_hardening_exact_split() {
            // BUY 10 000 QF, Hardening:
            //   dampener = 10 000 × 125 / 10 000 = 1 250 QF  (1.25%)
            //   prize    = 10 000 × 147 / 10 000 = 1 470 QF  (1.47%)
            //   team     = 0
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            e.buy(U256::from(ONE_QF * 10_000)).unwrap();
            assert_eq!(e.get_dampener_accumulated(), U256::from(1_250_u128 * ONE_QF),
                "hardening buy: dampener = 1 250 QF");
            assert_eq!(e.get_prize_pot(), U256::from(1_470_u128 * ONE_QF),
                "hardening buy: prize = 1 470 QF");
            assert_eq!(e.get_team_accumulated(), U256::ZERO,
                "hardening buy: team = 0");
        }

        // ── SELL — exact amounts, Hardening [H] ──────────────────────────────

        #[ink::test]
        fn sell_below_threshold_rejected() {
            let mut e = deploy();
            set_caller(accounts().alice);
            assert_eq!(e.sell(U256::from(ONE_QF - 1)), Err(Error::TransactionTooSmall));
        }

        #[ink::test]
        fn sell_hardening_exact_split() {
            // SELL 10 000 QF, Hardening:
            //   dampener = 10 000 × 125 / 10 000 = 1 250 QF  (1.25%)
            //   prize    = 10 000 × 189 / 10 000 = 1 890 QF  (1.89%)
            //   team     = 0
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            e.sell(U256::from(ONE_QF * 10_000)).unwrap();
            assert_eq!(e.get_dampener_accumulated(), U256::from(1_250_u128 * ONE_QF),
                "hardening sell: dampener = 1 250 QF");
            assert_eq!(e.get_prize_pot(), U256::from(1_890_u128 * ONE_QF),
                "hardening sell: prize = 1 890 QF");
            assert_eq!(e.get_team_accumulated(), U256::ZERO,
                "hardening sell: team = 0");
        }

        #[ink::test]
        fn hardening_sell_prize_greater_than_buy_prize() {
            // v5: sell prize (1.89%) > buy prize (1.47%)
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            let amt = U256::from(ONE_QF * 10_000);
            e.buy(amt).unwrap();
            let buy_prize = e.get_prize_pot();
            e.prize_pot_accumulated = U256::ZERO;
            e.sell(amt).unwrap();
            let sell_prize = e.get_prize_pot();
            assert!(sell_prize > buy_prize, "sell prize (1.89%) must exceed buy prize (1.47%)");
        }

        // ── BUY / SELL — exact amounts, Scarcity [S] ─────────────────────────

        #[ink::test]
        fn buy_scarcity_exact_split() {
            // BUY 10 000 QF, Scarcity:
            //   dampener = 500 QF (0.50%), prize = 1 470 QF (1.47%), team = 750 QF (0.75%)
            let mut e = deploy();
            enter_scarcity();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            e.buy(U256::from(ONE_QF * 10_000)).unwrap();
            assert_eq!(e.get_dampener_accumulated(), U256::from(500_u128 * ONE_QF),
                "scarcity buy: dampener = 500 QF");
            assert_eq!(e.get_prize_pot(), U256::from(1_470_u128 * ONE_QF),
                "scarcity buy: prize = 1 470 QF");
            assert_eq!(e.get_team_accumulated(), U256::from(750_u128 * ONE_QF),
                "scarcity buy: team = 750 QF");
        }

        #[ink::test]
        fn sell_scarcity_exact_split() {
            // SELL 10 000 QF, Scarcity:
            //   dampener = 500 QF (0.50%), prize = 1 890 QF (1.89%), team = 750 QF (0.75%)
            let mut e = deploy();
            enter_scarcity();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            e.sell(U256::from(ONE_QF * 10_000)).unwrap();
            assert_eq!(e.get_dampener_accumulated(), U256::from(500_u128 * ONE_QF),
                "scarcity sell: dampener = 500 QF");
            assert_eq!(e.get_prize_pot(), U256::from(1_890_u128 * ONE_QF),
                "scarcity sell: prize = 1 890 QF");
            assert_eq!(e.get_team_accumulated(), U256::from(750_u128 * ONE_QF),
                "scarcity sell: team = 750 QF");
        }

        #[ink::test]
        fn team_zero_in_hardening_nonzero_in_scarcity() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            let amt = U256::from(ONE_QF * 10_000);
            e.buy(amt).unwrap();
            assert_eq!(e.get_team_accumulated(), U256::ZERO, "team = 0 in hardening");
            enter_scarcity();
            e.buy(amt).unwrap();
            assert!(e.get_team_accumulated() > U256::ZERO, "team > 0 in scarcity");
        }

        #[ink::test]
        fn dampener_rate_drops_2_5x_at_phase_boundary() {
            // Hardening = 125 BPS, Scarcity = 50 BPS: ratio = 2.5 exactly
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            let amt = U256::from(ONE_QF * 10_000);
            e.buy(amt).unwrap();
            let hard_d = e.get_dampener_accumulated();
            e.dampener_tax_accumulated = U256::ZERO;
            enter_scarcity();
            e.buy(amt).unwrap();
            let scar_d = e.get_dampener_accumulated();
            // hard_d / scar_d = 1 250 / 500 = 2.5
            assert_eq!(hard_d, scar_d * U256::from(5u8) / U256::from(2u8),
                "hardening dampener must be exactly 2.5× scarcity dampener");
        }

        // ── 0.5% BASE FEED — 1.33× GUARANTEE [*] ────────────────────────────

        #[ink::test]
        fn dampener_base_provides_min_133x_for_lp_pairing() {
            // ratio_bps = 4 × DAMPENER_BASE / BUY_PRIZE = 4 × 50 / 147 × 10_000
            let ratio_bps = 4u128 * TAX_DAMPENER_BASE_BPS * BPS_DENOMINATOR / BUY_TAX_PRIZE_BPS;
            assert!(ratio_bps >= 13_300,
                "base feed must supply ≥ 1.33× QF for LP pairing; got {} BPS", ratio_bps);
        }

        #[ink::test]
        fn dampener_shield_provides_min_339x() {
            // 4 × 125 / 147 × 10_000 ≈ 34 013 BPS (≥ 33 900 = 3.39×)
            let ratio_bps = 4u128 * TAX_DAMPENER_SHIELD_BPS * BPS_DENOMINATOR / BUY_TAX_PRIZE_BPS;
            assert!(ratio_bps >= 33_900,
                "shield feed must supply ≥ 3.39× QF for LP pairing; got {} BPS", ratio_bps);
        }

        // ── Great Drain — Hardening [H] ───────────────────────────────────────

        #[ink::test]
        fn drain_does_not_fire_below_threshold() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            e.buy(U256::from(ONE_QF * 1_000)).unwrap();
            assert_eq!(e.get_drain_event_count(), 0);
        }

        #[ink::test]
        fn drain_hardening_populates_lp_reserve() {
            // Hardening drain: lp_pair_token_reserve must increase.
            // supply decreases by burned_52f (25% of pot) only.
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::from(ONE_QF);
            let pre_supply = e.total_supply();
            e.buy(U256::from(ONE_QF * 10_000)).unwrap();
            assert_eq!(e.get_drain_event_count(), 1);
            assert!(e.get_lp_pair_token_reserve() > U256::ZERO,
                "hardening: lp_pair_token_reserve must be populated");
            // supply must only shrink by burned_52f, not also by paired_52f
            let pot     = U256::from(1_470_u128 * ONE_QF);
            let seized  = pot * U256::from(DRAIN_SEIZED_BPS) / U256::from(BPS_DENOMINATOR);
            let burned  = seized * U256::from(DRAIN_BURN_BPS) / U256::from(BPS_DENOMINATOR);
            assert_eq!(pre_supply.saturating_sub(e.total_supply()), burned,
                "hardening: supply reduction = burned_52f (25% of pot) only");
        }

        // ── Great Drain — Scarcity [S] ────────────────────────────────────────

        #[ink::test]
        fn drain_scarcity_double_burn_no_lp_reserve() {
            let mut e = deploy();
            enter_scarcity();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::from(ONE_QF);
            let pre_lp = e.get_lp_pair_token_reserve();
            let pre_supply = e.total_supply();
            e.buy(U256::from(ONE_QF * 10_000)).unwrap();
            assert_eq!(e.get_lp_pair_token_reserve(), pre_lp,
                "scarcity: lp reserve must NOT increase");
            assert!(e.total_supply() < pre_supply, "scarcity: supply must decrease");
        }

        #[ink::test]
        fn drain_scarcity_supply_reduction_equals_seized() {
            // Double Burn: supply decreases by full seized (50% of pot).
            let mut e = deploy();
            enter_scarcity();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::from(ONE_QF);
            let pre_supply = e.total_supply();
            e.buy(U256::from(ONE_QF * 10_000)).unwrap();
            let pot    = U256::from(1_470_u128 * ONE_QF);
            let seized = pot * U256::from(DRAIN_SEIZED_BPS) / U256::from(BPS_DENOMINATOR);
            assert_eq!(pre_supply.saturating_sub(e.total_supply()), seized,
                "scarcity: supply reduction = full seized (Double Burn)");
        }

        // ── claim_lp_pair_tokens [H/S] ────────────────────────────────────────

        #[ink::test]
        fn claim_lp_pair_tokens_rejected_in_scarcity() {
            let mut e = deploy();
            enter_scarcity();
            let accs = accounts();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            assert_eq!(e.claim_lp_pair_tokens(U256::from(1u8)), Err(Error::NotHardeningPhase));
        }

        #[ink::test]
        fn claim_lp_pair_tokens_rejected_for_non_dampener() {
            let mut e = deploy();
            let accs = accounts();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.charlie);
            assert_eq!(e.claim_lp_pair_tokens(U256::from(1u8)), Err(Error::NotDampener));
        }

        #[ink::test]
        fn claim_lp_pair_tokens_decrements_reserve() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::from(ONE_QF);
            e.buy(U256::from(ONE_QF * 10_000)).unwrap();
            let reserve = e.get_lp_pair_token_reserve();
            assert!(reserve > U256::ZERO);
            let accs = accounts();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            e.claim_lp_pair_tokens(reserve).unwrap();
            assert_eq!(e.get_lp_pair_token_reserve(), U256::ZERO);
        }

        // ── Dampener socket [*] ───────────────────────────────────────────────

        #[ink::test]
        fn pull_dampener_tax_rejected_no_dampener() {
            let mut e = deploy();
            set_caller(accounts().alice);
            assert_eq!(e.pull_dampener_tax(), Err(Error::NoDampenerRegistered));
        }

        #[ink::test]
        fn pull_dampener_tax_rejected_non_dampener() {
            let mut e = deploy();
            let accs = accounts();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.charlie);
            assert_eq!(e.pull_dampener_tax(), Err(Error::NotDampener));
        }

        #[ink::test]
        fn pull_dampener_tax_rejected_empty() {
            let mut e = deploy();
            let accs = accounts();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            assert_eq!(e.pull_dampener_tax(), Err(Error::DampenerPotEmpty));
        }

        #[ink::test]
        fn pull_dampener_tax_clears_accumulator() {
            let mut e = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            e.prize_drain_threshold = U256::MAX;
            e.sell(U256::from(ONE_QF * 10_000)).unwrap();
            assert!(e.get_dampener_accumulated() > U256::ZERO);
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            e.pull_dampener_tax().unwrap();
            assert_eq!(e.get_dampener_accumulated(), U256::ZERO);
        }

        #[ink::test]
        fn request_great_drain_rejected_no_dampener() {
            let mut e = deploy();
            set_caller(accounts().alice);
            assert_eq!(e.request_great_drain(), Err(Error::NoDampenerRegistered));
        }

        #[ink::test]
        fn request_great_drain_ok_below_threshold() {
            let mut e = deploy();
            let accs = accounts();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            assert_eq!(e.request_great_drain(), Ok(()));
            assert_eq!(e.get_drain_event_count(), 0);
        }

        #[ink::test]
        fn request_great_drain_fires_hardening() {
            let mut e = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            e.prize_pot_accumulated = U256::from(DEFAULT_DRAIN_THRESHOLD) + U256::from(1u8);
            e.prize_drain_threshold = U256::from(DEFAULT_DRAIN_THRESHOLD);
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            e.request_great_drain().unwrap();
            assert_eq!(e.get_drain_event_count(), 1);
            assert!(e.get_lp_pair_token_reserve() > U256::ZERO,
                "hardening watchdog: lp reserve must be populated");
        }

        #[ink::test]
        fn request_great_drain_fires_scarcity() {
            let mut e = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            e.prize_pot_accumulated = U256::from(DEFAULT_DRAIN_THRESHOLD) + U256::from(1u8);
            e.prize_drain_threshold = U256::from(DEFAULT_DRAIN_THRESHOLD);
            enter_scarcity();
            e.dampener_address = Some(accs.bob);
            set_caller(accs.bob);
            e.request_great_drain().unwrap();
            assert_eq!(e.get_drain_event_count(), 1);
            assert_eq!(e.get_lp_pair_token_reserve(), U256::ZERO,
                "scarcity watchdog: lp reserve must NOT be populated");
        }

        // ── Epoch counter [*] ─────────────────────────────────────────────────

        #[ink::test]
        fn epoch_resets_at_52_buys() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            let amt = U256::from(ONE_QF * 10);
            for _ in 0..51 { e.buy(amt).unwrap(); }
            assert_eq!(e.get_epoch_counter(), 51);
            assert_eq!(e.get_epoch_id(), 0);
            e.buy(amt).unwrap();
            assert_eq!(e.get_epoch_counter(), 0);
            assert_eq!(e.get_epoch_id(), 1);
        }

        #[ink::test]
        fn epoch_mixed_buy_sell() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.prize_drain_threshold = U256::MAX;
            let amt = U256::from(ONE_QF * 10);
            for _ in 0..26 { e.buy(amt).unwrap(); }
            for _ in 0..25 { e.sell(amt).unwrap(); }
            assert_eq!(e.get_epoch_counter(), 51);
            e.buy(amt).unwrap();
            assert_eq!(e.get_epoch_counter(), 0);
            assert_eq!(e.get_epoch_id(), 1);
        }

        // ── Satellite / admin / safety / PSP22 [*] ───────────────────────────

        #[ink::test]
        fn pull_prize_tax_rejected_non_satellite() {
            let mut e = deploy();
            let accs = accounts();
            e.sequencer_satellite = Some(accs.bob);
            set_caller(accs.charlie);
            assert_eq!(e.pull_prize_tax(), Err(Error::NotSequencerSatellite));
        }

        #[ink::test]
        fn pull_prize_tax_rejected_empty() {
            let mut e = deploy();
            let accs = accounts();
            e.sequencer_satellite = Some(accs.bob);
            set_caller(accs.bob);
            assert_eq!(e.pull_prize_tax(), Err(Error::PrizePotEmpty));
        }

        #[ink::test]
        fn set_dampener_address_only_owner() {
            let mut e = deploy();
            set_caller(accounts().bob);
            assert_eq!(e.set_dampener_address(accounts().charlie), Err(Error::NotOwner));
        }

        #[ink::test]
        fn set_drain_threshold_only_owner() {
            let mut e = deploy();
            set_caller(accounts().bob);
            assert_eq!(e.set_drain_threshold(U256::from(1u8)), Err(Error::NotOwner));
        }

        #[ink::test]
        fn paused_rejects_writes() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.set_paused(true).unwrap();
            assert_eq!(e.buy(U256::from(ONE_QF * 10)),  Err(Error::ContractPaused));
            assert_eq!(e.sell(U256::from(ONE_QF * 10)), Err(Error::ContractPaused));
        }

        #[ink::test]
        fn transfer_updates_balances() {
            let mut e = deploy();
            set_caller(accounts().alice);
            e.transfer(accounts().bob, U256::from(ONE_QF * 500)).unwrap();
            assert_eq!(e.balance_of(accounts().bob), U256::from(ONE_QF * 500));
        }
    }
}
