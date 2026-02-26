#![cfg_attr(not(feature = "std"), no_std, no_main)]

/// # 52F Protocol — Token Engine
///
/// **Role:** Ground-truth ledger, tax collector, and epoch counter.
///
/// This contract is intentionally "dumb" with respect to game logic.
/// It collects buy/sell taxes, increments an epoch counter on every valid
/// transaction, emits `EpochReady` when the counter reaches 52, and exposes
/// a single `pull_prize_tax` entry-point exclusively for the authorised
/// Sequencer Satellite.
///
/// **What this contract does NOT contain:**
/// - Collision detection
/// - Winner selection
/// - Hash storage
/// - Any "King of the Hill" or throne-based logic
///
/// All mathematical sequencing is delegated to the Sequencer Satellite.
///
/// **Compatibility:** ink! v6 / PolkaVM (`pallet-revive`).
///   - `AccountId` → `Address` (H160)
///   - `Balance`   → `U256`
#[ink::contract]
mod project52f {
    use ink::prelude::string::String;
    use ink::storage::Mapping;

    // ink! v6 re-exports H160 as the default AccountId under pallet-revive.
    type Address = <ink::env::DefaultEnvironment as ink::env::Environment>::AccountId;
    use ink::primitives::U256;

    // =========================================================================
    // CONSTANTS — PROTOCOL MATHS
    // =========================================================================

    /// Denominator for all basis-point calculations.
    pub const BPS_DENOMINATOR: u128 = 10_000;

    /// Buy tax: 2.72% — approximation of Euler's number (e ≈ 2.71828…).
    pub const E_BUY_TAX_BPS: u128 = 272;

    /// Sell tax: 3.14% — approximation of Pi (π ≈ 3.14159…).
    pub const PI_SELL_TAX_BPS: u128 = 314;

    /// Portion of collected tax routed to the team, in basis points (0.75%).
    pub const TAX_TEAM_BPS: u128 = 75;

    /// Number of valid transactions that constitute one epoch.
    pub const EPOCH_SIZE: u32 = 52;

    /// Minimum transaction size in $QF$ base units (1 token, 18 decimals).
    /// Transactions below this threshold are rejected by the gatekeeper.
    pub const MIN_TRANSACTION_THRESHOLD: u128 = 1_000_000_000_000_000_000; // 1 QF

    /// The canonical EVM "dead" burn address: 0x000…dEaD.
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

        // ── Satellite socket ──────────────────────────────────────────────
        /// Address of the authorised Sequencer Satellite contract.
        /// Only this address may call `pull_prize_tax`.
        sequencer_satellite: Option<Address>,

        // ── Tax accumulators ──────────────────────────────────────────────
        /// Taxes earmarked for the team multisig (pushed periodically).
        team_tax_accumulated: U256,
        /// Prize pot available for the Sequencer Satellite to pull.
        prize_pot_accumulated: U256,

        // ── Epoch counter ─────────────────────────────────────────────────
        /// Running count of valid transactions within the current epoch.
        /// Resets to 0 each time it reaches `EPOCH_SIZE`.
        epoch_transaction_counter: u32,
        /// Monotonically increasing epoch identifier.
        epoch_id: u32,

        // ── Safety ───────────────────────────────────────────────────────
        paused: bool,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    /// Emitted on every token transfer.
    #[ink(event)]
    pub struct Transfer {
        #[ink(topic)]
        from: Option<Address>,
        #[ink(topic)]
        to: Option<Address>,
        value: U256,
    }

    /// Emitted when a spender allowance is set.
    #[ink(event)]
    pub struct Approval {
        #[ink(topic)]
        owner: Address,
        #[ink(topic)]
        spender: Address,
        value: U256,
    }

    /// Emitted when the epoch counter rolls over to 52.
    /// The Sequencer Satellite listens for this event to trigger collision logic.
    #[ink(event)]
    pub struct EpochReady {
        #[ink(topic)]
        epoch_id: u32,
        /// Size of the prize pot available for the Satellite to pull.
        prize_pot: U256,
    }

    /// Emitted when the Sequencer Satellite pulls the prize pot.
    #[ink(event)]
    pub struct PrizePotPulled {
        #[ink(topic)]
        epoch_id: u32,
        burn_amount: U256,
        yield_amount: U256,
        satellite: Address,
    }

    /// Emitted when the sequencer satellite address is updated.
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
        /// Sender's token balance is insufficient.
        InsufficientBalance,
        /// Spender's allowance is insufficient.
        InsufficientAllowance,
        /// Transaction value is below the 1 $QF minimum threshold.
        TransactionTooSmall,
        /// The prize pot is empty; nothing to pull.
        PrizePotEmpty,
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
        /// The entire `initial_supply` is minted to the deployer.
        /// No satellite is registered at deployment; call `set_sequencer_satellite`
        /// after deploying the Sequencer Satellite contract.
        #[ink(constructor)]
        pub fn new(
            initial_supply: U256,
            name: String,
            symbol: String,
        ) -> Self {
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
                epoch_transaction_counter: 0,
                epoch_id: 0,
                paused: false,
            }
        }

        // =====================================================================
        // THE GATEKEEPER — Buy & Sell Entry Points
        // =====================================================================

        /// Process a buy transaction.
        ///
        /// Tax breakdown (applied to the gross amount):
        ///   - 0.75% → team accumulator
        ///   - 1.97% → prize pot  (2.72% total buy tax − 0.75% team share)
        ///
        /// Every valid buy increments the epoch counter.
        ///
        /// # Errors
        /// - [`Error::ContractPaused`]         — contract is paused.
        /// - [`Error::TransactionTooSmall`]    — amount < 1 $QF.
        /// - [`Error::InsufficientBalance`]    — caller has insufficient tokens.
        #[ink(message)]
        pub fn buy(&mut self, amount: U256) -> Result<(), Error> {
            self.assert_not_paused()?;
            self.assert_above_threshold(amount)?;

            let caller = self.env().caller();
            let (team_share, prize_share, net_amount) =
                self.calculate_tax_split(amount, E_BUY_TAX_BPS)?;

            // Debit caller, credit the contract (net amount stays in circulation
            // as caller's adjusted balance post-swap; full accounting left to
            // the DEX integration layer).
            self.debit_balance(caller, amount)?;

            self.team_tax_accumulated = self
                .team_tax_accumulated
                .checked_add(team_share)
                .ok_or(Error::Overflow)?;
            self.prize_pot_accumulated = self
                .prize_pot_accumulated
                .checked_add(prize_share)
                .ok_or(Error::Overflow)?;

            // Credit net amount back to caller (post-tax receipt).
            self.credit_balance(caller, net_amount)?;

            self.env().emit_event(Transfer {
                from: Some(caller),
                to: Some(self.env().account_id()),
                value: team_share.saturating_add(prize_share),
            });

            self.tick_epoch_counter();
            Ok(())
        }

        /// Process a sell transaction.
        ///
        /// Tax breakdown (applied to the gross amount):
        ///   - 0.75% → team accumulator
        ///   - 2.39% → prize pot  (3.14% total sell tax − 0.75% team share)
        ///
        /// Every valid sell increments the epoch counter.
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

            self.tick_epoch_counter();
            Ok(())
        }

        // =====================================================================
        // SATELLITE SOCKET — Prize Pot Pull
        // =====================================================================

        /// Pull the prize pot from the Token Engine.
        ///
        /// **Caller:** Must be the registered `sequencer_satellite` address.
        ///
        /// **Split:**
        ///   - 10% of the pot → burned to `0x000…dEaD`
        ///   - 90% of the pot → transferred to the calling Satellite
        ///
        /// The Satellite is then responsible for distributing the 90% yield
        /// to winning addresses based on collision logic.
        ///
        /// State is updated before transfers (checks-effects-interactions).
        ///
        /// # Errors
        /// - [`Error::NotSequencerSatellite`] — caller is not the satellite.
        /// - [`Error::PrizePotEmpty`]         — nothing to pull.
        /// - [`Error::TransferFailed`]        — a native transfer failed.
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

            // ── 90 / 10 split ────────────────────────────────────────────
            let burn_amount = total_pot
                .checked_div(U256::from(10u8))
                .ok_or(Error::Overflow)?;
            let yield_amount = total_pot.saturating_sub(burn_amount);

            // ── State update (before external calls) ─────────────────────
            self.prize_pot_accumulated = U256::ZERO;
            let epoch_id = self.epoch_id;

            // ── Burn → 0x000…dEaD ────────────────────────────────────────
            let dead = Address::from(DEAD_ADDRESS);
            self.env()
                .transfer(dead, burn_amount)
                .map_err(|_| Error::TransferFailed)?;

            // ── Yield → Satellite ─────────────────────────────────────────
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
        pub fn total_supply(&self) -> U256 {
            self.total_supply
        }

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
            let allowance = self.allowance(from, caller);
            if allowance < value {
                return Err(Error::InsufficientAllowance);
            }
            self.allowances
                .insert((from, caller), &allowance.saturating_sub(value));
            self.transfer_impl(from, to, value)
        }

        // =====================================================================
        // VIEW FUNCTIONS
        // =====================================================================

        #[ink(message)]
        pub fn name(&self) -> String {
            self.name.clone()
        }

        #[ink(message)]
        pub fn symbol(&self) -> String {
            self.symbol.clone()
        }

        #[ink(message)]
        pub fn decimals(&self) -> u8 {
            self.decimals
        }

        #[ink(message)]
        pub fn get_prize_pot(&self) -> U256 {
            self.prize_pot_accumulated
        }

        #[ink(message)]
        pub fn get_team_accumulated(&self) -> U256 {
            self.team_tax_accumulated
        }

        #[ink(message)]
        pub fn get_epoch_counter(&self) -> u32 {
            self.epoch_transaction_counter
        }

        #[ink(message)]
        pub fn get_epoch_id(&self) -> u32 {
            self.epoch_id
        }

        #[ink(message)]
        pub fn get_sequencer_satellite(&self) -> Option<Address> {
            self.sequencer_satellite
        }

        #[ink(message)]
        pub fn is_paused(&self) -> bool {
            self.paused
        }

        // =====================================================================
        // ADMIN
        // =====================================================================

        /// Register (or update) the Sequencer Satellite address.
        ///
        /// Only the owner may call this.  There is no zero-address guard
        /// intentionally — the owner may wish to disable the satellite by
        /// setting it to `None` via a separate `clear_sequencer` message.
        #[ink(message)]
        pub fn set_sequencer_satellite(&mut self, addr: Address) -> Result<(), Error> {
            self.only_owner()?;
            self.sequencer_satellite = Some(addr);
            self.env().emit_event(SequencerUpdated { new_satellite: addr });
            Ok(())
        }

        /// Remove the Sequencer Satellite registration.
        #[ink(message)]
        pub fn clear_sequencer_satellite(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            self.sequencer_satellite = None;
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

        /// Increment the epoch counter; emit `EpochReady` and reset when EPOCH_SIZE
        /// is reached. O(1), no loops.
        fn tick_epoch_counter(&mut self) {
            self.epoch_transaction_counter = self
                .epoch_transaction_counter
                .saturating_add(1);

            if self.epoch_transaction_counter >= EPOCH_SIZE {
                self.epoch_id = self.epoch_id.saturating_add(1);
                self.epoch_transaction_counter = 0;

                self.env().emit_event(EpochReady {
                    epoch_id: self.epoch_id,
                    prize_pot: self.prize_pot_accumulated,
                });
            }
        }

        /// Split `amount` into (team_share, prize_share, net_amount).
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

        fn transfer_impl(
            &mut self,
            from: Address,
            to: Address,
            value: U256,
        ) -> Result<(), Error> {
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

        fn set_caller(addr: Address) {
            test::set_caller::<Env>(addr);
        }

        /// 1 000 000 $QF (18 decimals)
        const SUPPLY: u128 = 1_000_000_000_000_000_000_000_000;
        const ONE_QF: u128 = 1_000_000_000_000_000_000;

        fn deploy() -> Project52F {
            let accs = accounts();
            set_caller(accs.alice);
            Project52F::new(
                U256::from(SUPPLY),
                String::from("Project 52F"),
                String::from("QF"),
            )
        }

        #[ink::test]
        fn constructor_mints_to_owner() {
            let engine = deploy();
            let accs = accounts();
            assert_eq!(engine.balance_of(accs.alice), U256::from(SUPPLY));
            assert_eq!(engine.total_supply(), U256::from(SUPPLY));
        }

        #[ink::test]
        fn buy_below_threshold_is_rejected() {
            let mut engine = deploy();
            let accs = accounts();
            set_caller(accs.bob);
            // Give bob a balance to spend
            test::set_account_balance::<Env>(accs.bob, 1_000_000_000_000_000_000_000_000);
            let result = engine.buy(U256::from(ONE_QF - 1));
            assert_eq!(result, Err(Error::TransactionTooSmall));
        }

        #[ink::test]
        fn buy_accumulates_prize_and_team_tax() {
            let mut engine = deploy();
            let accs = accounts();
            // Give alice a large balance
            engine.balances.insert(accs.alice, &U256::from(SUPPLY));
            set_caller(accs.alice);

            let amount = U256::from(ONE_QF * 100); // 100 QF
            engine.buy(amount).unwrap();

            let prize = engine.get_prize_pot();
            let team = engine.get_team_accumulated();

            // Total tax = 100 * 272 / 10000 = 2.72 QF
            // Team share = 100 * 75 / 10000 = 0.75 QF
            // Prize share = 2.72 - 0.75 = 1.97 QF
            assert!(prize > U256::ZERO, "prize pot should be non-zero");
            assert!(team > U256::ZERO, "team accumulator should be non-zero");
            assert!(prize > team, "prize share should exceed team share");
        }

        #[ink::test]
        fn epoch_counter_increments_and_resets() {
            let mut engine = deploy();
            let accs = accounts();
            engine.balances.insert(accs.alice, &U256::from(SUPPLY * 100));
            set_caller(accs.alice);

            let amount = U256::from(ONE_QF * 10);

            // 51 buys — counter should be 51, epoch_id still 0
            for _ in 0..51 {
                engine.buy(amount).unwrap();
            }
            assert_eq!(engine.get_epoch_counter(), 51);
            assert_eq!(engine.get_epoch_id(), 0);

            // 52nd buy triggers epoch rollover
            engine.buy(amount).unwrap();
            assert_eq!(engine.get_epoch_counter(), 0, "counter should reset");
            assert_eq!(engine.get_epoch_id(), 1, "epoch_id should increment");
        }

        #[ink::test]
        fn pull_prize_tax_rejected_for_non_satellite() {
            let mut engine = deploy();
            let accs = accounts();
            engine.sequencer_satellite = Some(accs.bob);
            set_caller(accs.charlie); // not the satellite
            let result = engine.pull_prize_tax();
            assert_eq!(result, Err(Error::NotSequencerSatellite));
        }

        #[ink::test]
        fn pull_prize_tax_rejected_when_pot_empty() {
            let mut engine = deploy();
            let accs = accounts();
            engine.sequencer_satellite = Some(accs.bob);
            set_caller(accs.bob);
            let result = engine.pull_prize_tax();
            assert_eq!(result, Err(Error::PrizePotEmpty));
        }

        #[ink::test]
        fn paused_contract_rejects_buys() {
            let mut engine = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            engine.set_paused(true).unwrap();
            let result = engine.buy(U256::from(ONE_QF * 10));
            assert_eq!(result, Err(Error::ContractPaused));
        }

        #[ink::test]
        fn set_sequencer_satellite_only_owner() {
            let mut engine = deploy();
            let accs = accounts();
            set_caller(accs.bob); // not the owner
            let result = engine.set_sequencer_satellite(accs.charlie);
            assert_eq!(result, Err(Error::NotOwner));
        }

        #[ink::test]
        fn transfer_updates_balances() {
            let mut engine = deploy();
            let accs = accounts();
            set_caller(accs.alice);
            engine.transfer(accs.bob, U256::from(ONE_QF * 500)).unwrap();
            assert_eq!(
                engine.balance_of(accs.bob),
                U256::from(ONE_QF * 500)
            );
        }
    }
}
