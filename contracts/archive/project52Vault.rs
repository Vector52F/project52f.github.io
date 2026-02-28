#![cfg_attr(not(feature = "std"), no_std, no_main)]

/// # Project 52F — Team Vesting Vault
///
/// **Purpose:** Sole responsibility is the 52-week (52-tranche) linear vesting
/// distribution of the team's earned token allocation (~12.07% of total supply).
///
/// **Pillar separation:** This contract holds *no* $QF$ tokens itself. All token
/// balances reside in the Fortress Ledger. This Vault acts exclusively as an
/// authorised requester, triggering transfers outward from the Fortress on a
/// vesting schedule. Seed capital, liquidity provisioning, and loans are wholly
/// outside the scope of this contract and are handled by the Fortress Ledger.
///
/// **Compatibility:** ink! v6 / PolkaVM (`pallet-revive`).
///   – `Balance`  → `U256`  (EVM-compatible 256-bit integer)
///   – `AccountId` → `Address` (H160 — 20-byte Ethereum-style address)
#[ink::contract]
mod project52_vault {
    // -------------------------------------------------------------------------
    // Imports
    // -------------------------------------------------------------------------

    use ink::env::call::{build_call, ExecutionInput, Selector};
    use ink::primitives::U256;

    // In pallet-revive / ink! v6 the native account type is H160.
    // Re-export via the environment so downstream code stays clean.
    type Address = <ink::env::DefaultEnvironment as ink::env::Environment>::AccountId;

    // =========================================================================
    // CONSTANTS — MATHEMATICALLY LOCKED
    // =========================================================================

    /// Team allocation in basis points: ≈12.07%
    ///
    /// Derivation:  π × e × √2
    ///   = 3.14159… × 2.71828… × 1.41421…
    ///   ≈ 12.0699…  → 1207 BPS (rounded to nearest integer)
    ///
    /// Verification (spot-check):
    ///   total_supply × 1207 / 10_000 = total_supply × 0.1207
    ///   e.g. 1 000 000 tokens → 120 700 tokens allocated to the team.
    pub const TEAM_ALLOCATION_BPS: u128 = 1_207;

    /// Total number of vesting tranches — one per week across a full year.
    pub const TOTAL_TRANCHES: u32 = 52;

    /// Block interval between tranches.
    ///
    /// At Polkadot's target of one block per 6 seconds (10 blocks/minute),
    /// 5 200 000 blocks ≈ 6.006 days, giving 52 × 6 days ≈ 52 weeks.
    ///
    /// *Note:* QF Network targets ~0.1 s/block, so this constant must be
    /// reviewed against the live block time before mainnet deployment.
    pub const TRANCHE_INTERVAL: u32 = 5_200_000;

    /// Denominator for basis-point arithmetic.
    pub const BPS_DENOMINATOR: u128 = 10_000;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct Project52Vault {
        /// Deployer / admin address.
        owner: Address,
        /// Recipient of vested tokens — the team multisig.
        team_wallet: Address,
        /// Address of the Fortress Ledger contract that holds all $QF$ tokens.
        fortress: Address,
        /// Block number at which vesting began.
        start_block: u32,
        /// Index of the last tranche that has been claimed (0 = none claimed).
        last_claimed_tranche: u32,
        /// Gross team allocation in $QF$ base units (U256).
        total_team_allocation: U256,
        /// Running total of tokens transferred to the team wallet so far.
        claimed_amount: U256,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct TeamVestingClaimed {
        #[ink(topic)]
        tranches_claimed: u32,
        amount: U256,
        new_total_claimed: U256,
        remaining_tranches: u32,
        block: u32,
        /// `true` when this claim included the final (52nd) tranche and cleared
        /// any integer-division dust, leaving a zero remaining balance.
        is_final_tranche: bool,
    }

    #[ink(event)]
    pub struct VestingScheduleInitialised {
        start_block: u32,
        total_allocation: U256,
        /// Base tranche size before dust correction on the final tranche.
        base_tranche_size: U256,
        total_tranches: u32,
    }

    #[ink(event)]
    pub struct TeamWalletUpdated {
        #[ink(topic)]
        previous_wallet: Address,
        #[ink(topic)]
        new_wallet: Address,
    }

    #[ink(event)]
    pub struct FortressUpdated {
        #[ink(topic)]
        new_fortress: Address,
    }

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        /// Caller is not authorised to perform this action.
        NotAuthorised,
        /// No tranches have become available since the last claim.
        NoTranchesAvailable,
        /// All 52 tranches have been claimed; vesting is complete.
        FullyVested,
        /// An arithmetic operation overflowed.
        Overflow,
        /// The cross-contract call to the Fortress failed.
        FortressTransferFailed,
        /// The supplied team wallet address is the zero address.
        InvalidTeamWallet,
        /// The supplied Fortress address is the zero address.
        InvalidFortress,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACE — FORTRESS LEDGER
    // =========================================================================

    /// Minimal interface this Vault uses to request token transfers from the
    /// Fortress Ledger.  The Fortress is responsible for all token custody;
    /// this Vault never holds $QF$ directly.
    #[ink::trait_definition]
    pub trait FortressInterface {
        /// Transfer `amount` $QF$ base units from the Fortress to `to`.
        ///
        /// The Fortress MUST verify that `self.env().caller()` is an authorised
        /// Vault contract before executing the transfer.
        #[ink(message)]
        fn transfer(&mut self, to: Address, amount: U256) -> Result<(), Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52Vault {
        // ---------------------------------------------------------------------
        // Constructor
        // ---------------------------------------------------------------------

        /// Deploy a new vesting vault.
        ///
        /// # Parameters
        /// - `team_wallet`  — H160 address of the team's receiving multisig.
        /// - `fortress`     — H160 address of the Fortress Ledger contract.
        /// - `total_supply` — Total $QF$ supply in base units (U256).
        ///
        /// # Allocation maths
        /// `total_team_allocation = total_supply × 1207 / 10_000`
        ///
        /// Integer division is used throughout; any residual dust (at most
        /// 51 base units) is recovered on the final (52nd) tranche claim.
        #[ink(constructor)]
        pub fn new(
            team_wallet: Address,
            fortress: Address,
            total_supply: U256,
        ) -> Result<Self, Error> {
            let zero = Address::from([0u8; 20]);

            if team_wallet == zero {
                return Err(Error::InvalidTeamWallet);
            }
            if fortress == zero {
                return Err(Error::InvalidFortress);
            }

            let caller = Self::env().caller();
            let current_block = Self::env().block_number();

            // ── Allocation calculation ────────────────────────────────────
            // U256 × u128: promote BPS constant to U256 for the multiply.
            let bps = U256::from(TEAM_ALLOCATION_BPS);
            let denom = U256::from(BPS_DENOMINATOR);

            let total_allocation = total_supply
                .checked_mul(bps)
                .ok_or(Error::Overflow)?
                .checked_div(denom)
                .ok_or(Error::Overflow)?;

            // Base tranche size (dust intentionally ignored here; cleared on
            // the final claim).
            let base_tranche_size = total_allocation
                .checked_div(U256::from(TOTAL_TRANCHES))
                .ok_or(Error::Overflow)?;

            let contract = Self {
                owner: caller,
                team_wallet,
                fortress,
                start_block: current_block,
                last_claimed_tranche: 0,
                total_team_allocation: total_allocation,
                claimed_amount: U256::zero(),
            };

            contract.env().emit_event(VestingScheduleInitialised {
                start_block: current_block,
                total_allocation,
                base_tranche_size,
                total_tranches: TOTAL_TRANCHES,
            });

            Ok(contract)
        }

        // ---------------------------------------------------------------------
        // Team Vesting Claim
        // ---------------------------------------------------------------------

        /// Claim all available vesting tranches for the team wallet.
        ///
        /// **Dust-clearing logic:** On the 52nd (final) tranche, the claimable
        /// amount is calculated as `total_team_allocation − claimed_amount`
        /// rather than `per_tranche × n`. This guarantees that integer-division
        /// dust (at most `TOTAL_TRANCHES − 1` base units) is fully swept and the
        /// Fortress reaches a zero balance for the team allocation.
        ///
        /// **Reentrancy:** State is updated *before* the cross-contract call to
        /// the Fortress, following the checks-effects-interactions pattern.
        ///
        /// # Errors
        /// - [`Error::NotAuthorised`]        — caller is not `team_wallet`.
        /// - [`Error::NoTranchesAvailable`]  — no new tranches have unlocked.
        /// - [`Error::FullyVested`]          — all 52 tranches already claimed.
        /// - [`Error::FortressTransferFailed`] — Fortress rejected the transfer.
        #[ink(message)]
        pub fn claim_team_vesting(&mut self) -> Result<U256, Error> {
            let caller = self.env().caller();

            if caller != self.team_wallet {
                return Err(Error::NotAuthorised);
            }

            if self.last_claimed_tranche >= TOTAL_TRANCHES {
                return Err(Error::FullyVested);
            }

            let current_block = self.env().block_number();
            let eligible_tranches = self.calculate_eligible_tranches(current_block)?;

            if eligible_tranches == 0 {
                return Err(Error::NoTranchesAvailable);
            }

            // Determine whether this claim reaches or passes the final tranche.
            let claim_end_tranche = self
                .last_claimed_tranche
                .saturating_add(eligible_tranches);
            let is_final_claim = claim_end_tranche >= TOTAL_TRANCHES;

            // ── Claimable amount ─────────────────────────────────────────
            let claimable_amount = if is_final_claim {
                // Dust-clearing: take the exact remainder so the Fortress
                // reaches zero for this allocation.
                self.total_team_allocation
                    .saturating_sub(self.claimed_amount)
            } else {
                let per_tranche = self
                    .total_team_allocation
                    .checked_div(U256::from(TOTAL_TRANCHES))
                    .ok_or(Error::Overflow)?;

                per_tranche
                    .checked_mul(U256::from(eligible_tranches))
                    .ok_or(Error::Overflow)?
            };

            if claimable_amount.is_zero() {
                return Err(Error::NoTranchesAvailable);
            }

            // ── State updates (before external call) ─────────────────────
            self.last_claimed_tranche = if is_final_claim {
                TOTAL_TRANCHES // cap at 52
            } else {
                claim_end_tranche
            };

            self.claimed_amount = self
                .claimed_amount
                .checked_add(claimable_amount)
                .ok_or(Error::Overflow)?;

            // Defensive invariant: claimed amount must never exceed allocation.
            if self.claimed_amount > self.total_team_allocation {
                return Err(Error::Overflow);
            }

            // ── Cross-contract transfer (interactions last) ───────────────
            self.request_fortress_transfer(self.team_wallet, claimable_amount)?;

            let remaining_tranches = TOTAL_TRANCHES
                .saturating_sub(self.last_claimed_tranche);

            self.env().emit_event(TeamVestingClaimed {
                tranches_claimed: eligible_tranches,
                amount: claimable_amount,
                new_total_claimed: self.claimed_amount,
                remaining_tranches,
                block: current_block,
                is_final_tranche: is_final_claim,
            });

            Ok(claimable_amount)
        }

        // ---------------------------------------------------------------------
        // Internal helpers
        // ---------------------------------------------------------------------

        /// Calculate how many unclaimed tranches are currently eligible.
        ///
        /// **Gas note:** The calculation is O(1) — a single division followed
        /// by a saturating subtraction — so it remains constant-cost regardless
        /// of how many blocks have elapsed since the last claim.
        fn calculate_eligible_tranches(&self, current_block: u32) -> Result<u32, Error> {
            if self.last_claimed_tranche >= TOTAL_TRANCHES {
                return Ok(0);
            }

            let blocks_elapsed = current_block.saturating_sub(self.start_block);

            // Integer division: number of complete intervals that have passed.
            let intervals_passed = blocks_elapsed / TRANCHE_INTERVAL;

            // Cap at TOTAL_TRANCHES so we never return more than 52.
            let max_eligible = intervals_passed.min(TOTAL_TRANCHES);

            // Subtract already-claimed tranches.
            let available = max_eligible.saturating_sub(self.last_claimed_tranche);

            Ok(available)
        }

        /// Request a token transfer from the Fortress Ledger via cross-contract
        /// call (ink! v6 / PolkaVM calling convention).
        ///
        /// This Vault does *not* hold $QF$; it only instructs the Fortress.
        fn request_fortress_transfer(
            &self,
            to: Address,
            amount: U256,
        ) -> Result<(), Error> {
            // ink! v6: build_call uses the DefaultEnvironment; the call target
            // is the Fortress H160 address.
            let result: Result<Result<(), Error>, ink::env::Error> = build_call::<
                ink::env::DefaultEnvironment,
            >()
            .call(self.fortress)
            .exec_input(
                ExecutionInput::new(Selector::new(ink::selector_bytes!("transfer")))
                    .push_arg(&to)
                    .push_arg(&amount),
            )
            .returns::<Result<(), Error>>()
            .try_invoke();

            match result {
                Ok(Ok(_)) => Ok(()),
                _ => Err(Error::FortressTransferFailed),
            }
        }

        // ---------------------------------------------------------------------
        // Admin functions
        // ---------------------------------------------------------------------

        /// Update the team receiving wallet.  Only the owner may call this.
        ///
        /// Emits [`TeamWalletUpdated`].
        #[ink(message)]
        pub fn set_team_wallet(&mut self, new_wallet: Address) -> Result<(), Error> {
            self.only_owner()?;
            if new_wallet == Address::from([0u8; 20]) {
                return Err(Error::InvalidTeamWallet);
            }
            let previous = self.team_wallet;
            self.team_wallet = new_wallet;
            self.env().emit_event(TeamWalletUpdated {
                previous_wallet: previous,
                new_wallet,
            });
            Ok(())
        }

        /// Update the Fortress Ledger address.  Only the owner may call this.
        ///
        /// Emits [`FortressUpdated`].
        #[ink(message)]
        pub fn set_fortress(&mut self, address: Address) -> Result<(), Error> {
            self.only_owner()?;
            if address == Address::from([0u8; 20]) {
                return Err(Error::InvalidFortress);
            }
            self.fortress = address;
            self.env().emit_event(FortressUpdated {
                new_fortress: address,
            });
            Ok(())
        }

        // No emergency overrides, manual resets, or "God Mode" bypass functions.
        // All vesting state is immutable except through the normal claim pathway.

        // ---------------------------------------------------------------------
        // View functions
        // ---------------------------------------------------------------------

        #[ink(message)]
        pub fn get_owner(&self) -> Address {
            self.owner
        }

        #[ink(message)]
        pub fn get_team_wallet(&self) -> Address {
            self.team_wallet
        }

        #[ink(message)]
        pub fn get_fortress(&self) -> Address {
            self.fortress
        }

        #[ink(message)]
        pub fn get_start_block(&self) -> u32 {
            self.start_block
        }

        #[ink(message)]
        pub fn get_last_claimed_tranche(&self) -> u32 {
            self.last_claimed_tranche
        }

        #[ink(message)]
        pub fn get_claimed_amount(&self) -> U256 {
            self.claimed_amount
        }

        #[ink(message)]
        pub fn get_total_allocation(&self) -> U256 {
            self.total_team_allocation
        }

        /// Return a complete snapshot of the current vesting state.
        ///
        /// Returns:
        /// `(claimed_tranches, available_tranches, remaining_tranches,
        ///   claimed_amount, remaining_amount, is_fully_vested)`
        #[ink(message)]
        pub fn get_vesting_status(&self) -> (u32, u32, u32, U256, U256, bool) {
            let current_block = self.env().block_number();
            let available = self
                .calculate_eligible_tranches(current_block)
                .unwrap_or(0);
            let remaining_tranches = TOTAL_TRANCHES
                .saturating_sub(self.last_claimed_tranche);
            let remaining_amount = self
                .total_team_allocation
                .saturating_sub(self.claimed_amount);
            let is_fully_vested = self.last_claimed_tranche >= TOTAL_TRANCHES;

            (
                self.last_claimed_tranche,
                available,
                remaining_tranches,
                self.claimed_amount,
                remaining_amount,
                is_fully_vested,
            )
        }

        /// Return the block number at which the *next* tranche will unlock.
        #[ink(message)]
        pub fn get_next_vesting_block(&self) -> u32 {
            let current_block = self.env().block_number();
            let blocks_elapsed = current_block.saturating_sub(self.start_block);
            let intervals_passed = blocks_elapsed / TRANCHE_INTERVAL;
            let next_interval = intervals_passed.saturating_add(1);
            self.start_block
                .saturating_add(next_interval.saturating_mul(TRANCHE_INTERVAL))
        }

        /// Preview what a `claim_team_vesting` call would return right now,
        /// without mutating state.
        ///
        /// Returns `(eligible_tranches, claimable_amount, is_final_tranche)`.
        /// All fields are zero / false when no tranches are currently available.
        #[ink(message)]
        pub fn preview_claim(&self) -> Result<(u32, U256, bool), Error> {
            let current_block = self.env().block_number();
            let eligible = self.calculate_eligible_tranches(current_block)?;

            if eligible == 0 {
                return Ok((0, U256::zero(), false));
            }

            let claim_end = self
                .last_claimed_tranche
                .saturating_add(eligible);
            let is_final = claim_end >= TOTAL_TRANCHES;

            let amount = if is_final {
                self.total_team_allocation
                    .saturating_sub(self.claimed_amount)
            } else {
                let per_tranche = self
                    .total_team_allocation
                    .checked_div(U256::from(TOTAL_TRANCHES))
                    .ok_or(Error::Overflow)?;
                per_tranche
                    .checked_mul(U256::from(eligible))
                    .ok_or(Error::Overflow)?
            };

            Ok((eligible, amount, is_final))
        }

        // ---------------------------------------------------------------------
        // Access control
        // ---------------------------------------------------------------------

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotAuthorised);
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

        // Helper: default test accounts (H160 in ink! v6).
        fn accounts() -> test::DefaultAccounts<DefaultEnvironment> {
            test::default_accounts::<DefaultEnvironment>()
        }

        fn set_caller(account: Address) {
            test::set_caller::<DefaultEnvironment>(account);
        }

        fn set_block(block: u32) {
            test::set_block_number::<DefaultEnvironment>(block);
        }

        // ── Helpers ───────────────────────────────────────────────────────────

        /// Construct a vault with a 1 000-token supply (18 decimals).
        fn make_vault() -> Project52Vault {
            let accs = accounts();
            set_caller(accs.alice);
            set_block(0);

            let total_supply = U256::from(1_000_000_000_000_000_000_000_u128); // 1 000 tokens

            Project52Vault::new(accs.bob, accs.charlie, total_supply)
                .expect("constructor should succeed")
        }

        // ── Constructor tests ─────────────────────────────────────────────────

        #[ink::test]
        fn constructor_sets_correct_allocation() {
            let vault = make_vault();
            let total_supply = U256::from(1_000_000_000_000_000_000_000_u128);
            let expected = total_supply * U256::from(1_207_u128) / U256::from(10_000_u128);
            assert_eq!(vault.get_total_allocation(), expected);
        }

        #[ink::test]
        fn constructor_rejects_zero_team_wallet() {
            let accs = accounts();
            set_caller(accs.alice);
            let result = Project52Vault::new(
                Address::from([0u8; 20]),
                accs.charlie,
                U256::from(1_000_u64),
            );
            assert_eq!(result, Err(Error::InvalidTeamWallet));
        }

        #[ink::test]
        fn constructor_rejects_zero_fortress() {
            let accs = accounts();
            set_caller(accs.alice);
            let result = Project52Vault::new(
                accs.bob,
                Address::from([0u8; 20]),
                U256::from(1_000_u64),
            );
            assert_eq!(result, Err(Error::InvalidFortress));
        }

        // ── Tranche eligibility ───────────────────────────────────────────────

        #[ink::test]
        fn no_tranches_at_genesis() {
            let vault = make_vault();
            // Still at block 0 — no intervals have elapsed.
            let (eligible, amount, is_final) = vault.preview_claim().unwrap();
            assert_eq!(eligible, 0);
            assert_eq!(amount, U256::zero());
            assert!(!is_final);
        }

        #[ink::test]
        fn one_tranche_after_one_interval() {
            let mut vault = make_vault();
            set_block(TRANCHE_INTERVAL); // exactly one interval
            let (eligible, _, is_final) = vault.preview_claim().unwrap();
            assert_eq!(eligible, 1);
            assert!(!is_final);
        }

        #[ink::test]
        fn all_52_tranches_available_after_full_schedule() {
            let vault = make_vault();
            set_block(52 * TRANCHE_INTERVAL);
            let (eligible, _, is_final) = vault.preview_claim().unwrap();
            assert_eq!(eligible, 52);
            assert!(is_final);
        }

        // ── Dust-clearing (the 52nd-tranche fix) ─────────────────────────────

        #[ink::test]
        fn final_tranche_clears_dust() {
            let vault = make_vault();
            set_block(52 * TRANCHE_INTERVAL);

            let allocation = vault.get_total_allocation();
            let per_tranche = allocation / U256::from(TOTAL_TRANCHES);
            let dust = allocation - (per_tranche * U256::from(TOTAL_TRANCHES));

            let (_, claim_amount, is_final) = vault.preview_claim().unwrap();

            assert!(is_final, "should flag as final tranche");
            // The remainder path must return the full allocation (none yet claimed).
            assert_eq!(claim_amount, allocation);
            // Sanity: dust would be non-zero for most real supply values.
            let _ = dust; // used in documentation / logging if needed
        }

        // ── Authorisation ─────────────────────────────────────────────────────

        #[ink::test]
        fn claim_rejected_for_non_team_wallet() {
            let accs = accounts();
            let mut vault = make_vault();
            set_block(TRANCHE_INTERVAL);
            // alice is owner, not team_wallet (bob).
            set_caller(accs.alice);
            let result = vault.claim_team_vesting();
            assert_eq!(result, Err(Error::NotAuthorised));
        }

        #[ink::test]
        fn set_team_wallet_rejected_for_non_owner() {
            let accs = accounts();
            let mut vault = make_vault();
            set_caller(accs.bob); // bob is team_wallet, not owner
            let result = vault.set_team_wallet(accs.django);
            assert_eq!(result, Err(Error::NotAuthorised));
        }

        #[ink::test]
        fn set_team_wallet_rejects_zero_address() {
            let accs = accounts();
            let mut vault = make_vault();
            set_caller(accs.alice); // alice is owner
            let result = vault.set_team_wallet(Address::from([0u8; 20]));
            assert_eq!(result, Err(Error::InvalidTeamWallet));
        }

        // ── View helpers ──────────────────────────────────────────────────────

        #[ink::test]
        fn get_vesting_status_reflects_initial_state() {
            let vault = make_vault();
            let (claimed, available, remaining, claimed_amt, remaining_amt, is_vested) =
                vault.get_vesting_status();
            assert_eq!(claimed, 0);
            assert_eq!(available, 0);
            assert_eq!(remaining, TOTAL_TRANCHES);
            assert_eq!(claimed_amt, U256::zero());
            assert_eq!(remaining_amt, vault.get_total_allocation());
            assert!(!is_vested);
        }

        #[ink::test]
        fn next_vesting_block_is_one_interval_from_start() {
            let vault = make_vault();
            // At block 0, next unlock should be at TRANCHE_INTERVAL.
            assert_eq!(vault.get_next_vesting_block(), TRANCHE_INTERVAL);
        }
    }
}
