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
    
    /// Total vesting tranches: 52 (Fibonacci symmetry)
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
        /// Contract owner
        owner: AccountId,
        
        /// Authorized team wallet (recipient of vesting)
        team_wallet: AccountId,
        
        /// Project52F contract address (source of tokens)
        fortress: AccountId,
        
        /// Deployment block (start of vesting schedule)
        start_block: u32,
        
        /// Last claimed tranche index (0-52)
        last_claimed_tranche: u32,
        
        /// Total team allocation amount
        total_team_allocation: Balance,
        
        /// Amount already claimed by team
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
        /// Caller is not the authorized team wallet
        NotAuthorized,
        /// No tranches available to claim
        NoTranchesAvailable,
        /// All 52 tranches already claimed
        FullyVested,
        /// Math overflow
        Overflow,
        /// Transfer from fortress failed
        TransferFailed,
        /// Invalid team wallet
        InvalidTeamWallet,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACE
    // =========================================================================

    #[ink::trait_definition]
    pub trait FortressInterface {
        #[ink(message)]
        fn transfer(&mut self, to: AccountId, amount: Balance) -> Result<(), Error>;
        
        #[ink(message)]
        fn balance_of(&self, owner: AccountId) -> Balance;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52Vault {
        /// Constructor
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
        // TEAM VESTING CLAIM (Cumulative Pull Model)
        // =================================================================

        /// Claim available team vesting tranches
        /// 
        /// Logic: eligible_tranches = (current_block - start_block) / 5,200,000
        /// total_available = (total_allocation / 52) * min(eligible_tranches, 52)
        /// claimable = total_available - already_claimed
        /// 
        /// Cumulative Pull: If team doesn't claim for 3 intervals, they can pull 3 tranches at once
        #[ink(message)]
        pub fn claim_team_vesting(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            // Verify caller is authorized team wallet
            if caller != self.team_wallet {
                return Err(Error::NotAuthorized);
            }
            
            let current_block = self.env().block_number();
            
            // Calculate eligible tranches
            let eligible_tranches = self.calculate_eligible_tranches(current_block)?;
            
            if eligible_tranches == 0 {
                return Err(Error::NoTranchesAvailable);
            }
            
            // Calculate amount per tranche (safe math)
            let amount_per_tranche = self.total_team_allocation
                .checked_div(TOTAL_TRANCHES as Balance)
                .ok_or(Error::Overflow)?;
            
            // Calculate total claimable for all eligible tranches
            let total_for_eligible = amount_per_tranche
                .checked_mul(eligible_tranches as Balance)
                .ok_or(Error::Overflow)?;
            
            // Calculate what's actually available (eligible - already claimed)
            let claimable_amount = total_for_eligible.saturating_sub(self.claimed_amount);
            
            if claimable_amount == 0 {
                return Err(Error::NoTranchesAvailable);
            }
            
            // Update state BEFORE transfer (checks-effects-interactions)
            let tranches_being_claimed = (claimable_amount / amount_per_tranche) as u32;
            self.last_claimed_tranche += tranches_being_claimed;
            self.claimed_amount = self.claimed_amount
                .checked_add(claimable_amount)
                .ok_or(Error::Overflow)?;
            
            // Transfer from Fortress to team wallet
            self.transfer_from_fortress(caller, claimable_amount)?;
            
            self.env().emit_event(TeamVestingClaimed {
                tranches_claimed: tranches_being_claimed,
                amount: claimable_amount,
                new_total_claimed: self.claimed_amount,
                remaining_tranches: TOTAL_TRANCHES - self.last_claimed_tranche,
                block: current_block,
            });
            
            Ok(claimable_amount)
        }

        /// Calculate how many tranches are eligible based on time passed
        /// Formula: eligible = (current_block - start_block) / TRANCHE_INTERVAL
        /// Capped at TOTAL_TRANCHES (52)
        fn calculate_eligible_tranches(&self, current_block: u32) -> Result<u32, Error> {
            // If fully vested, return 0
            if self.last_claimed_tranche >= TOTAL_TRANCHES {
                return Ok(0);
            }
            
            // Calculate blocks elapsed since start
            let blocks_elapsed = current_block.saturating_sub(self.start_block);
            
            // Calculate how many intervals have passed
            let intervals_passed = blocks_elapsed / TRANCHE_INTERVAL;
            
            // Cap at 52 tranches max
            let max_eligible = if intervals_passed > TOTAL_TRANCHES {
                TOTAL_TRANCHES
            } else {
                intervals_passed
            };
            
            // Subtract already claimed to get currently available
            let available = max_eligible.saturating_sub(self.last_claimed_tranche);
            
            Ok(available)
        }

        /// Transfer tokens from Fortress to recipient
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
        /// Returns: (claimed_tranches, available_tranches, remaining_tranches, claimed_amount, remaining_amount)
        #[ink(message)]
        pub fn get_vesting_status(&self) -> (u32, u32, u32, Balance, Balance) {
            let current_block = self.env().block_number();
            let available = self.calculate_eligible_tranches(current_block).unwrap_or(0);
            let remaining = TOTAL_TRANCHES - self.last_claimed_tranche;
            let remaining_amount = self.total_team_allocation - self.claimed_amount;
            
            (
                self.last_claimed_tranche,
                available,
                remaining,
                self.claimed_amount,
                remaining_amount,
            )
        }

        /// Calculate next vesting milestone
        #[ink(message)]
        pub fn get_next_vesting_block(&self) -> u32 {
            let current_block = self.env().block_number();
            let blocks_elapsed = current_block.saturating_sub(self.start_block);
            let intervals_passed = blocks_elapsed / TRANCHE_INTERVAL;
            let next_interval = intervals_passed + 1;
            
            self.start_block + (next_interval * TRANCHE_INTERVAL)
        }

        /// Preview claim without executing
        #[ink(message)]
        pub fn preview_claim(&self) -> Result<(u32, Balance), Error> {
            let current_block = self.env().block_number();
            let eligible = self.calculate_eligible_tranches(current_block)?;
            
            if eligible == 0 {
                return Ok((0, 0));
            }
            
            let per_tranche = self.total_team_allocation / TOTAL_TRANCHES as Balance;
            let total_eligible = per_tranche * eligible as Balance;
            let claimable = total_eligible.saturating_sub(self.claimed_amount);
            
            Ok((eligible, claimable))
        }

        // =================================================================
        // ADMIN FUNCTIONS
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
        fn constructor_initializes_vesting() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            set_block_number(0);
            
            let total_supply: Balance = 1_000_000_000_000_000_000_000_000; // 1M tokens
            
            let vault = Project52Vault::new(
                accounts.bob,     // team wallet
                accounts.charlie, // fortress
                total_supply,
            );
            
            assert_eq!(vault.get_start_block(), 0);
            assert_eq!(vault.get_last_claimed_tranche(), 0);
            assert_eq!(vault.get_claimed_amount(), 0);
            
            // Verify ~12.07% allocation
            let expected = total_supply * TEAM_ALLOCATION_BPS / BPS_DENOMINATOR;
            assert_eq!(vault.get_total_allocation(), expected);
        }

        #[ink::test]
        fn cumulative_pull_accumulates() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            set_block_number(0);
            
            let total_supply = 1_000_000_000_000_000_000_000_000u128;
            let mut vault = Project52Vault::new(accounts.bob, accounts.charlie, total_supply);
            
            // At 0 blocks, nothing available
            let (claimed, available, _, _, _) = vault.get_vesting_status();
            assert_eq!(claimed, 0);
            assert_eq!(available, 0);
            
            // At 5.2M blocks, 1 tranche available
            set_block_number(5_200_000);
            let (claimed, available, remaining, _, _) = vault.get_vesting_status();
            assert_eq!(claimed, 0);
            assert_eq!(available, 1);
            assert_eq!(remaining, 52);
            
            // At 15.6M blocks (3 intervals), 3 tranches available
            set_block_number(15_600_000);
            let (_, available, _, _, _) = vault.get_vesting_status();
            assert_eq!(available, 3);
        }

        #[ink::test]
        fn unauthorized_claim_fails() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            set_block_number(5_200_000);
            
            let total_supply = 1_000_000_000_000_000_000_000_000u128;
            let mut vault = Project52Vault::new(accounts.bob, accounts.charlie, total_supply);
            
            // Try to claim as non-team wallet
            set_caller(accounts.charlie);
            let result = vault.claim_team_vesting();
            assert_eq!(result, Err(Error::NotAuthorized));
        }
    }
}
