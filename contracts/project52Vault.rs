#![cfg_attr(not(feature = "std"), no_std, no_main)]

#[ink::contract]
mod project52_vault {
    use ink::primitives::AccountId;
    use ink::env::call::{build_call, ExecutionInput, Selector};

    // =========================================================================
    // CONSTANTS — MATHEMATICALLY LOCKED
    // =========================================================================
    
    /// Team allocation: ~12.07% (π × e × √2) = 1207 BPS
    pub const TEAM_ALLOCATION_BPS: u128 = 1_207;
    
    /// Total vesting tranches: 52
    pub const TOTAL_TRANCHES: u32 = 52;
    
    /// Tranche interval: 5.2M blocks (~6 days @ 0.1s/block)
    pub const TRANCHE_INTERVAL: u32 = 5_200_000;
    
    /// Basis Points denominator
    pub const BPS_DENOMINATOR: u128 = 10_000;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct Project52Vault {
        owner: AccountId,
        team_wallet: AccountId,
        fortress: AccountId,
        start_block: u32,
        last_claimed_tranche: u32,
        total_team_allocation: Balance,
        claimed_amount: Balance,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct TeamVestingClaimed {
        #[ink(topic)]
        tranches_claimed: u32,
        amount: Balance,
        new_total_claimed: Balance,
        remaining_tranches: u32,
        block: u32,
        is_final_tranche: bool, // NEW: Track if this was the dust-clearing final tranche
    }

    #[ink(event)]
    pub struct VestingScheduleInitialized {
        start_block: u32,
        total_allocation: Balance,
        tranche_size: Balance,
        total_tranches: u32,
    }

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotAuthorized,
        NoTranchesAvailable,
        FullyVested,
        Overflow,
        TransferFailed,
        InvalidTeamWallet,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACE
    // =========================================================================

    #[ink::trait_definition]
    pub trait FortressInterface {
        #[ink(message)]
        fn transfer(&mut self, to: AccountId, amount: Balance) -> Result<(), Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52Vault {
        #[ink(constructor)]
        pub fn new(
            team_wallet: AccountId,
            fortress: AccountId,
            total_supply: Balance,
        ) -> Self {
            let caller = Self::env().caller();
            let current_block = Self::env().block_number();
            
            // Calculate ~12.07% allocation
            let total_allocation = total_supply
                .checked_mul(TEAM_ALLOCATION_BPS)
                .expect("Overflow in allocation calculation")
                / BPS_DENOMINATOR;
            
            // Calculate base tranche size (this will have dust remainder)
            let tranche_size = total_allocation / TOTAL_TRANCHES as Balance;
            
            let contract = Self {
                owner: caller,
                team_wallet,
                fortress,
                start_block: current_block,
                last_claimed_tranche: 0,
                total_team_allocation: total_allocation,
                claimed_amount: 0,
            };
            
            contract.env().emit_event(VestingScheduleInitialized {
                start_block: current_block,
                total_allocation,
                tranche_size,
                total_tranches: TOTAL_TRANCHES,
            });
            
            contract
        }

        // =================================================================
        // TEAM VESTING CLAIM (THE 52ND TRANCHE FIX)
        // =================================================================

        /// Claim available team vesting tranches
        /// 
        /// CRITICAL FIX: On the 52nd (final) tranche, claims exactly 
        /// total_allocation - claimed_amount to clear dust and ensure zero balance.
        #[ink(message)]
        pub fn claim_team_vesting(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            if caller != self.team_wallet {
                return Err(Error::NotAuthorized);
            }
            
            let current_block = self.env().block_number();
            
            // Calculate eligible tranches
            let eligible_tranches = self.calculate_eligible_tranches(current_block)?;
            
            if eligible_tranches == 0 {
                return Err(Error::NoTranchesAvailable);
            }
            
            // Determine if this claim includes the final (52nd) tranche
            let claim_end_tranche = self.last_claimed_tranche + eligible_tranches;
            let is_final_claim = claim_end_tranche >= TOTAL_TRANCHES;
            
            let claimable_amount = if is_final_claim {
                // THE FIX: On final claim, take remainder to ensure zero dust
                // This clears any rounding errors from integer division
                self.total_team_allocation.saturating_sub(self.claimed_amount)
            } else {
                // Standard calculation for non-final tranches
                let amount_per_tranche = self.total_team_allocation 
                    / TOTAL_TRANCHES as Balance;
                
                amount_per_tranche
                    .checked_mul(eligible_tranches as Balance)
                    .ok_or(Error::Overflow)?
            };
            
            if claimable_amount == 0 {
                return Err(Error::NoTranchesAvailable);
            }
            
            // Update state BEFORE transfer
            self.last_claimed_tranche = if is_final_claim {
                TOTAL_TRANCHES // Cap at 52
            } else {
                self.last_claimed_tranche + eligible_tranches
            };
            
            self.claimed_amount = self.claimed_amount
                .checked_add(claimable_amount)
                .ok_or(Error::Overflow)?;
            
            // Defensive: Ensure we never exceed total allocation
            if self.claimed_amount > self.total_team_allocation {
                return Err(Error::Overflow);
            }
            
            // Transfer from Fortress to team wallet
            self.transfer_from_fortress(caller, claimable_amount)?;
            
            self.env().emit_event(TeamVestingClaimed {
                tranches_claimed: eligible_tranches,
                amount: claimable_amount,
                new_total_claimed: self.claimed_amount,
                remaining_tranches: TOTAL_TRANCHES - self.last_claimed_tranche,
                block: current_block,
                is_final_tranche: is_final_claim,
            });
            
            Ok(claimable_amount)
        }

        /// Calculate eligible tranches based on time passed
        fn calculate_eligible_tranches(&self, current_block: u32) -> Result<u32, Error> {
            if self.last_claimed_tranche >= TOTAL_TRANCHES {
                return Ok(0);
            }
            
            let blocks_elapsed = current_block.saturating_sub(self.start_block);
            let intervals_passed = blocks_elapsed / TRANCHE_INTERVAL;
            
            let max_eligible = if intervals_passed > TOTAL_TRANCHES {
                TOTAL_TRANCHES
            } else {
                intervals_passed
            };
            
            let available = max_eligible.saturating_sub(self.last_claimed_tranche);
            Ok(available)
        }

        fn transfer_from_fortress(&self, to: AccountId, amount: Balance) -> Result<(), Error> {
            let result: Result<(), Error> = build_call::<DefaultEnvironment>()
                .call(self.fortress)
                .exec_input(
                    ExecutionInput::new(Selector::new(ink::selector_bytes!("transfer")))
                        .push_arg(&to)
                        .push_arg(amount)
                )
                .returns::<Result<(), Error>>()
                .invoke();
            
            match result {
                Ok(_) => Ok(()),
                Err(_) => Err(Error::TransferFailed),
            }
        }

        // =================================================================
        // ADMIN FUNCTIONS (SANITISED)
        // =================================================================

        #[ink(message)]
        pub fn set_team_wallet(&mut self, new_wallet: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            if new_wallet == AccountId::from([0x0; 32]) {
                return Err(Error::InvalidTeamWallet);
            }
            self.team_wallet = new_wallet;
            Ok(())
        }

        #[ink(message)]
        pub fn set_fortress(&mut self, address: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.fortress = address;
            Ok(())
        }
        
        // REMOVED: No manual reset functions (God Mode eliminated)
        // REMOVED: No emergency_claim or bypass functions

        // =================================================================
        // VIEW FUNCTIONS
        // =================================================================

        #[ink(message)]
        pub fn get_start_block(&self) -> u32 {
            self.start_block
        }

        #[ink(message)]
        pub fn get_last_claimed_tranche(&self) -> u32 {
            self.last_claimed_tranche
        }

        #[ink(message)]
        pub fn get_claimed_amount(&self) -> Balance {
            self.claimed_amount
        }

        #[ink(message)]
        pub fn get_total_allocation(&self) -> Balance {
            self.total_team_allocation
        }

        /// Get complete vesting status
        /// Returns: (claimed_tranches, available_tranches, remaining_tranches, claimed_amount, remaining_amount, is_fully_vested)
        #[ink(message)]
        pub fn get_vesting_status(&self) -> (u32, u32, u32, Balance, Balance, bool) {
            let current_block = self.env().block_number();
            let available = self.calculate_eligible_tranches(current_block).unwrap_or(0);
            let remaining = TOTAL_TRANCHES - self.last_claimed_tranche;
            let remaining_amount = self.total_team_allocation - self.claimed_amount;
            let is_fully_vested = self.last_claimed_tranche >= TOTAL_TRANCHES;
            
            (
                self.last_claimed_tranche,
                available,
                remaining,
                self.claimed_amount,
                remaining_amount,
                is_fully_vested,
            )
        }

        #[ink(message)]
        pub fn get_next_vesting_block(&self) -> u32 {
            let current_block = self.env().block_number();
            let blocks_elapsed = current_block.saturating_sub(self.start_block);
            let intervals_passed = blocks_elapsed / TRANCHE_INTERVAL;
            let next_interval = intervals_passed + 1;
            
            self.start_block + (next_interval * TRANCHE_INTERVAL)
        }

        /// Preview claim showing if it will be a standard or remainder (dust-clearing) claim
        #[ink(message)]
        pub fn preview_claim(&self) -> Result<(u32, Balance, bool), Error> {
            let current_block = self.env().block_number();
            let eligible = self.calculate_eligible_tranches(current_block)?;
            
            if eligible == 0 {
                return Ok((0, 0, false));
            }
            
            let claim_end = self.last_claimed_tranche + eligible;
            let is_final = claim_end >= TOTAL_TRANCHES;
            
            let amount = if is_final {
                self.total_team_allocation - self.claimed_amount
            } else {
                let per_tranche = self.total_team_allocation / TOTAL_TRANCHES as Balance;
                per_tranche * eligible as Balance
            };
            
            Ok((eligible, amount, is_final))
        }

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotAuthorized);
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

        fn default_accounts() -> test::DefaultAccounts<DefaultEnvironment> {
            test::default_accounts::<DefaultEnvironment>()
        }

        fn set_caller(account: AccountId) {
            test::set_caller::<DefaultEnvironment>(account);
        }

        fn set_block_number(block: u32) {
            test::set_block_number::<DefaultEnvironment>(block);
        }

        #[ink::test]
        fn tranche_remainder_fix() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            set_block_number(0);
            
            // Supply that creates dust when divided by 52
            // 1000 % 52 = 1000 - (52*19) = 1000 - 988 = 12 dust
            let total_supply: Balance = 1_000_000_000_000_000_000_000; // 1000 tokens
            
            let mut vault = Project52Vault::new(accounts.bob, accounts.charlie, total_supply);
            
            let allocation = vault.get_total_allocation();
            let per_tranche = allocation / 52;
            let expected_dust = allocation - (per_tranche * 52);
            
            // Fast forward to block 52 * 5.2M (all tranches eligible)
            set_block_number(52 * 5_200_000);
            
            // Claim should give exact remainder, not calculated amount
            let (_, claim_amount, is_final) = vault.preview_claim().unwrap();
            
            assert!(is_final);
            assert_eq!(claim_amount, allocation); // Should get full allocation
            assert_eq!(vault.get_claimed_amount(), 0); // Not claimed yet
            
            // After claim, should be fully vested with 0 remaining
            // (Mock transfer would fail in test, but math checks out)
        }
    }
}
