#![cfg_attr(not(feature = "std"), no_std, no_main)]

/// # 52F Protocol — Token Engine  (v3 — Great Drain Edition)
///
/// **Role:** Ground-truth ledger, tax collector, epoch counter, and
/// Volatility Governor.
///
/// ## What this contract does
/// - Collects buy tax (e ≈ 2.72%) and sell tax (π ≈ 3.14%) on every
///   transaction above the 1 $QF floor.
/// - Routes 0.75% of each transaction to the team accumulator and the
///   remainder into the prize pot.
/// - Increments a zero-touch epoch counter; emits `EpochReady` on every
///   52nd valid transaction.
/// - Exposes `pull_prize_tax` exclusively to the Sequencer Satellite
///   (90% yield to Satellite, 10% burn to `0x000…dEaD`).
/// - Enforces the **Great Drain** Volatility Governor: if the prize pot
///   reaches the 520 000 000 $52F equivalent threshold, 50% of the pot is
///   automatically seized and split:
///     - 25% of total pot → $QF burned to `DEAD_ADDRESS`
///     - 25% of total pot → held as buyback reserve; `GreatDrain` event
///       emitted so an off-chain keeper can market-buy $52F and burn it.
///     - 50% of total pot → remains in `prize_pot_accumulated` for winners.
///
/// ## What this contract does NOT contain
/// - Collision detection or winner selection (→ Sequencer Satellite)
/// - Hash storage (→ Sequencer Satellite)
/// - Any throne / King-of-the-Hill logic (removed in v2)
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
    // CONSTANTS — PROTOCOL MATHS
    // =========================================================================

    /// Denominator for all basis-point calculations.
    pub const BPS_DENOMINATOR: u128 = 10_000;

    /// Buy tax in BPS: 2.72% — Euler's number (e ≈ 2.71828…).
    pub const E_BUY_TAX_BPS: u128 = 272;

    /// Sell tax in BPS: 3.14% — Pi (π ≈ 3.14159…).
    pub const PI_SELL_TAX_BPS: u128 = 314;

    /// Team share of every transaction in BPS (0.75%).
    pub const TAX_TEAM_BPS: u128 = 75;

    /// Transactions that constitute one epoch.
    pub const EPOCH_SIZE: u32 = 52;

    /// Minimum valid transaction size ($QF$ base units, 18 decimals = 1 token).
    pub const MIN_TRANSACTION_THRESHOLD: u128 = 1_000_000_000_000_000_000;

    /// Canonical EVM dead/burn address: 0x000…dEaD.
    pub const DEAD_ADDRESS: [u8; 20] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xdE, 0xaD,
    ];

    // ── GREAT DRAIN CONSTANTS ─────────────────────────────────────────────────

    /// Default drain threshold: 520 000 000 $52F tokens in $QF$ base units.
    ///
    /// At launch (£500k market cap / £0.0000062 per token) this is ≈ £3 224.
    /// Update via `set_drain_threshold` as the $52F spot price changes.
    pub const DEFAULT_DRAIN_THRESHOLD: u128 =
        520_000_000_u128 * 1_000_000_000_000_000_000_u128; // 520 M × 10^18

    /// Fraction of the pot seized during a Great Drain (50% = 5_000 BPS).
    pub const DRAIN_SEIZED_BPS: u128 = 5_000;

    /// Of the seized 50%, the fraction burned as $QF$ (50% of seized = 25% of pot).
    pub const DRAIN_QF_BURN_BPS: u128 = 5_000;

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

        // ── Satellite socket ──────────────────────────────────────────────
        /// Address of the authorised Sequencer Satellite contract.
        sequencer_satellite: Option<Address>,

        // ── Tax accumulators ──────────────────────────────────────────────
        /// Taxes earmarked for the team multisig.
        team_tax_accumulated: U256,
        /// Prize pot available for the Sequencer Satellite to pull.
        prize_pot_accumulated: U256,

        // ── Great Drain Governor ──────────────────────────────────────────
        /// Prize-pot ceiling in $QF$ base units before the Great Drain fires.
        /// Updatable by the owner to track the $52F spot price.
        prize_drain_threshold: U256,
        /// $QF$ reserved for the $52F buy-back-and-burn (populated by drain).
        buyback_reserve: U256,
        /// Running count of Great Drain events (for on-chain auditability).
        drain_event_count: u32,

        // ── Epoch counter ─────────────────────────────────────────────────
        /// Transactions within the current epoch (resets at EPOCH_SIZE).
        epoch_transaction_counter: u32,
        /// Monotonically increasing epoch identifier.
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
    /// | Field            | Meaning                                             |
    /// |------------------|-----------------------------------------------------|
    /// | `drain_id`       | Monotonic drain counter (1-indexed)                 |
    /// | `pot_before`     | Prize pot immediately before the drain              |
    /// | `seized_amount`  | 50% of `pot_before` taken by the governor           |
    /// | `qf_burned`      | 25% of `pot_before` sent to `DEAD_ADDRESS`          |
    /// | `buyback_amount` | 25% of `pot_before` held in `buyback_reserve`       |
    /// | `pot_remaining`  | 50% of `pot_before` left for winners                |
    ///
    /// An off-chain keeper listens for this event and uses `buyback_amount`
    /// to market-buy $52F on-chain, then sends those tokens to `DEAD_ADDRESS`.
    #[ink(event)]
    pub struct GreatDrain {
        #[ink(topic)]
        drain_id: u32,
        pot_before: U256,
        seized_amount: U256,
        qf_burned: U256,
        buyback_amount: U256,
        pot_remaining: U256,
    }

    /// Emitted when the owner updates the drain threshold.
    #[ink(event)]
    pub struct DrainThresholdUpdated {
        previous: U256,
        updated: U256,
    }

    /// Emitted when the buyback reserve is released to a keeper.
    #[ink(event)]
    pub struct BuybackReleased {
        #[ink(topic)]
        keeper: Address,
        amount: U256,
    }

    #[ink(event)]
    pub struct SequencerUpdated {
        #[ink(topic)]
        new_satellite: Address,
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
        /// Sender's $QF$ balance is insufficient.
        InsufficientBalance,
        /// Spender's allowance is insufficient.
        InsufficientAllowance,
        /// Transaction value is below the 1 $QF minimum threshold.
        TransactionTooSmall,
        /// The prize pot is empty; nothing to pull.
        PrizePotEmpty,
        /// Requested buyback release exceeds the current reserve.
        InsufficientBuybackReserve,
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
                sequencer_satellite: None,
                team_tax_accumulated: U256::ZERO,
                prize_pot_accumulated: U256::ZERO,
                prize_drain_threshold: U256::from(DEFAULT_DRAIN_THRESHOLD),
                buyback_reserve: U256::ZERO,
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
        /// Tax routing for transaction of `amount`:
        /// ```text
        ///   team_share  = amount × 75  / 10_000   (0.75%)
        ///   total_tax   = amount × 272 / 10_000   (2.72%)
        ///   prize_share = total_tax − team_share   (1.97%)
        ///   net_amount  = amount − total_tax
        /// ```
        ///
        /// After accumulation the prize pot is tested against the Great Drain
        /// threshold (see `check_and_execute_drain`).
        #[ink(message)]
        pub fn buy(&mut self, amount: U256) -> Result<(), Error> {
            self.assert_not_paused()?;
            self.assert_above_threshold(amount)?;

            let caller = self.env().caller();
            let (team_share, prize_share, net_amount) =
                self.calculate_tax_split(amount, E_BUY_TAX_BPS)?;

            self.debit_balance(caller, amount)?;

            self.team_tax_accumulated = self
                .team_tax_accumulated
                .checked_add(team_share)
                .ok_or(Error::Overflow)?;
            self.prize_pot_accumulated = self
                .prize_pot_accumulated
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?;

            self.credit_balance(caller, net_amount)?;

            self.env().emit_event(Transfer {
                from: Some(caller),
                to: Some(self.env().account_id()),
                value: team_share.saturating_add(prize_share),
            });

            self.check_and_execute_drain()?;
            self.tick_epoch_counter();
            Ok(())
        }

        /// Process a sell transaction.
        ///
        /// Tax routing for transaction of `amount`:
        /// ```text
        ///   team_share  = amount × 75  / 10_000   (0.75%)
        ///   total_tax   = amount × 314 / 10_000   (3.14%)
        ///   prize_share = total_tax − team_share   (2.39%)
        ///   net_amount  = amount − total_tax
        /// ```
        #[ink(message)]
        pub fn sell(&mut self, amount: U256) -> Result<(), Error> {
            self.assert_not_paused()?;
            self.assert_above_threshold(amount)?;

            let caller = self.env().caller();
            let (team_share, prize_share, net_amount) =
                self.calculate_tax_split(amount, PI_SELL_TAX_BPS)?;

            self.debit_balance(caller, amount)?;

            self.team_tax_accumulated = self
                .team_tax_accumulated
                .checked_add(team_share)
                .ok_or(Error::Overflow)?;
            self.prize_pot_accumulated = self
                .prize_pot_accumulated
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?;

            self.credit_balance(caller, net_amount)?;

            self.env().emit_event(Transfer {
                from: Some(caller),
                to: Some(self.env().account_id()),
                value: team_share.saturating_add(prize_share),
            });

            self.check_and_execute_drain()?;
            self.tick_epoch_counter();
            Ok(())
        }

        // =====================================================================
        // GREAT DRAIN — Volatility Governor
        // =====================================================================

        /// Test the prize pot against the drain threshold; execute if breached.
        ///
        /// **Split arithmetic (all integer, no rounding errors can grow the pot):**
        /// ```text
        /// pot      = prize_pot_accumulated
        /// seized   = pot × DRAIN_SEIZED_BPS / BPS_DENOMINATOR   (50%)
        /// qf_burn  = seized × DRAIN_QF_BURN_BPS / BPS_DENOMINATOR (50% of seized = 25% of pot)
        /// buyback  = seized − qf_burn                             (25% of pot)
        /// remaining = pot − seized                                (50% of pot)
        /// ```
        ///
        /// State updates occur before external calls (checks-effects-interactions).
        /// The `buyback_reserve` field accumulates the buyback allocation and is
        /// released to an authorised keeper via `release_buyback`.
        fn check_and_execute_drain(&mut self) -> Result<(), Error> {
            if self.prize_pot_accumulated < self.prize_drain_threshold {
                return Ok(());
            }

            let pot = self.prize_pot_accumulated;

            let seized = pot
                .checked_mul(U256::from(DRAIN_SEIZED_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(U256::from(BPS_DENOMINATOR))
                .ok_or(Error::Overflow)?;

            let qf_burn = seized
                .checked_mul(U256::from(DRAIN_QF_BURN_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(U256::from(BPS_DENOMINATOR))
                .ok_or(Error::Overflow)?;

            // Integer-safe: buyback is the exact remainder of the seized half.
            let buyback_amount = seized.saturating_sub(qf_burn);
            let pot_remaining = pot.saturating_sub(seized);

            // ── State update (before external calls) ──────────────────────
            self.prize_pot_accumulated = pot_remaining;
            self.buyback_reserve = self
                .buyback_reserve
                .checked_add(buyback_amount)
                .ok_or(Error::Overflow)?;
            self.drain_event_count = self.drain_event_count.saturating_add(1);
            let drain_id = self.drain_event_count;

            // Reduce total supply to reflect the burned $QF$.
            self.total_supply = self.total_supply.saturating_sub(qf_burn);

            // ── Execute QF burn ────────────────────────────────────────────
            let dead = Address::from(DEAD_ADDRESS);
            self.env()
                .transfer(dead, qf_burn)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(GreatDrain {
                drain_id,
                pot_before: pot,
                seized_amount: seized,
                qf_burned: qf_burn,
                buyback_amount,
                pot_remaining,
            });

            Ok(())
        }

        /// Release $QF$ from the buyback reserve to an authorised keeper.
        ///
        /// The keeper is responsible for using these funds to market-buy $52F
        /// on the open market and send the purchased tokens to `DEAD_ADDRESS`.
        /// Only the owner may trigger a release; `amount` must not exceed
        /// `buyback_reserve`.
        #[ink(message)]
        pub fn release_buyback(&mut self, keeper: Address, amount: U256) -> Result<(), Error> {
            self.only_owner()?;

            if amount > self.buyback_reserve {
                return Err(Error::InsufficientBuybackReserve);
            }

            self.buyback_reserve = self.buyback_reserve.saturating_sub(amount);

            self.env()
                .transfer(keeper, amount)
                .map_err(|_| Error::TransferFailed)?;

            self.env().emit_event(BuybackReleased { keeper, amount });
            Ok(())
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
        pub fn get_drain_threshold(&self) -> U256 { self.prize_drain_threshold }

        #[ink(message)]
        pub fn get_buyback_reserve(&self) -> U256 { self.buyback_reserve }

        #[ink(message)]
        pub fn get_drain_event_count(&self) -> u32 { self.drain_event_count }

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

        fn calculate_tax_split(
            &self,
            amount: U256,
            total_tax_bps: u128,
        ) -> Result<(U256, U256, U256), Error> {
            let denom = U256::from(BPS_DENOMINATOR);

            let total_tax = amount
                .checked_mul(U256::from(total_tax_bps))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            let team_share = amount
                .checked_mul(U256::from(TAX_TEAM_BPS))
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            let prize_share = total_tax.saturating_sub(team_share);
            let net_amount = amount.saturating_sub(total_tax);

            Ok((team_share, prize_share, net_amount))
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
        fn set_caller(addr: Address) { test::set_caller::<Env>(addr); }

        const ONE_QF: u128 = 1_000_000_000_000_000_000;
        const SUPPLY: u128 = 1_000_000 * ONE_QF;

        fn deploy() -> Project52F {
            let accs = accounts();
            set_caller(accs.alice);
            Project52F::new(
                U256::from(SUPPLY),
                String::from("Project 52F"),
                String::from("QF"),
            )
        }

        // ── Constructor ───────────────────────────────────────────────────────

        #[ink::test]
        fn constructor_mints_to_owner() {
            let engine = deploy();
            let accs = accounts();
            assert_eq!(engine.balance_of(accs.alice), U256::from(SUPPLY));
            assert_eq!(engine.total_supply(), U256::from(SUPPLY));
        }

        #[ink::test]
        fn constructor_sets_default_drain_threshold() {
            let engine = deploy();
            assert_eq!(
                engine.get_drain_threshold(),
                U256::from(DEFAULT_DRAIN_THRESHOLD)
            );
        }

        // ── Tax accumulation ──────────────────────────────────────────────────

        #[ink::test]
        fn buy_below_threshold_rejected() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            assert_eq!(
                engine.buy(U256::from(ONE_QF - 1)),
                Err(Error::TransactionTooSmall)
            );
        }

        #[ink::test]
        fn buy_accumulates_prize_and_team_tax() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            engine.buy(U256::from(ONE_QF * 100)).unwrap();
            assert!(engine.get_prize_pot() > U256::ZERO);
            assert!(engine.get_team_accumulated() > U256::ZERO);
            assert!(engine.get_prize_pot() > engine.get_team_accumulated());
        }

        // ── Great Drain split maths ───────────────────────────────────────────

        #[ink::test]
        fn drain_split_constants_are_correct() {
            // seized = pot × 50% = 500 000
            // qf_burn = seized × 50% = 250 000  (25% of pot)
            // buyback  = seized − qf_burn = 250 000  (25% of pot)
            // remaining = pot − seized = 500 000  (50% of pot)
            let pot = U256::from(1_000_000u64);
            let seized   = pot * U256::from(DRAIN_SEIZED_BPS) / U256::from(BPS_DENOMINATOR);
            let qf_burn  = seized * U256::from(DRAIN_QF_BURN_BPS) / U256::from(BPS_DENOMINATOR);
            let buyback  = seized - qf_burn;
            let remaining = pot - seized;

            assert_eq!(seized,    U256::from(500_000u64), "50% seized");
            assert_eq!(qf_burn,   U256::from(250_000u64), "25% QF burn");
            assert_eq!(buyback,   U256::from(250_000u64), "25% buyback reserve");
            assert_eq!(remaining, U256::from(500_000u64), "50% remains in pot");
        }

        #[ink::test]
        fn drain_does_not_fire_below_threshold() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            engine.prize_drain_threshold = U256::MAX;
            engine.buy(U256::from(ONE_QF * 1_000)).unwrap();
            assert_eq!(engine.get_drain_event_count(), 0);
        }

        #[ink::test]
        fn drain_fires_when_threshold_exceeded() {
            let mut engine = deploy();
            set_caller(accounts().alice);

            // Set threshold below the prize accumulation of a 1 000 QF buy.
            // Prize share ≈ 1.97% of 1_000 QF = ~19.7 QF. Set threshold to 1 QF.
            engine.prize_drain_threshold = U256::from(ONE_QF);

            let pre_supply = engine.total_supply();
            engine.buy(U256::from(ONE_QF * 1_000)).unwrap();

            assert!(engine.get_drain_event_count() >= 1, "drain must fire");
            assert!(engine.total_supply() < pre_supply, "supply must decrease");
            assert!(engine.get_buyback_reserve() > U256::ZERO, "buyback must be non-zero");
        }

        // ── Epoch counter ─────────────────────────────────────────────────────

        #[ink::test]
        fn epoch_resets_at_52() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            // Disable drain so it doesn't interfere.
            engine.prize_drain_threshold = U256::MAX;
            let amount = U256::from(ONE_QF * 10);
            for _ in 0..51 { engine.buy(amount).unwrap(); }
            assert_eq!(engine.get_epoch_counter(), 51);
            assert_eq!(engine.get_epoch_id(), 0);
            engine.buy(amount).unwrap();
            assert_eq!(engine.get_epoch_counter(), 0);
            assert_eq!(engine.get_epoch_id(), 1);
        }

        // ── Satellite / pull ──────────────────────────────────────────────────

        #[ink::test]
        fn pull_prize_tax_rejected_for_non_satellite() {
            let mut engine = deploy();
            let accs = accounts();
            engine.sequencer_satellite = Some(accs.bob);
            set_caller(accs.charlie);
            assert_eq!(engine.pull_prize_tax(), Err(Error::NotSequencerSatellite));
        }

        #[ink::test]
        fn pull_prize_tax_rejected_when_pot_empty() {
            let mut engine = deploy();
            let accs = accounts();
            engine.sequencer_satellite = Some(accs.bob);
            set_caller(accs.bob);
            assert_eq!(engine.pull_prize_tax(), Err(Error::PrizePotEmpty));
        }

        // ── Safety ────────────────────────────────────────────────────────────

        #[ink::test]
        fn paused_contract_rejects_all_writes() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            engine.set_paused(true).unwrap();
            assert_eq!(engine.buy(U256::from(ONE_QF * 10)),  Err(Error::ContractPaused));
            assert_eq!(engine.sell(U256::from(ONE_QF * 10)), Err(Error::ContractPaused));
        }

        #[ink::test]
        fn set_drain_threshold_owner_only() {
            let mut engine = deploy();
            set_caller(accounts().bob);
            assert_eq!(engine.set_drain_threshold(U256::from(1u8)), Err(Error::NotOwner));
        }

        #[ink::test]
        fn release_buyback_rejects_excess() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            // buyback_reserve is zero at deploy — any release must fail.
            assert_eq!(
                engine.release_buyback(accounts().bob, U256::from(1u8)),
                Err(Error::InsufficientBuybackReserve)
            );
        }

        // ── PSP22 ─────────────────────────────────────────────────────────────

        #[ink::test]
        fn transfer_updates_balances() {
            let mut engine = deploy();
            set_caller(accounts().alice);
            engine.transfer(accounts().bob, U256::from(ONE_QF * 500)).unwrap();
            assert_eq!(engine.balance_of(accounts().bob), U256::from(ONE_QF * 500));
        }
    }
}
