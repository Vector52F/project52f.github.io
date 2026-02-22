#![cfg_attr(not(feature = "std"), no_std)]

pub type QFBalance = U256;
pub type Balance = u128;

pub mod constants {
    // √2 × π × e = 12.070346... % of supply
    pub const TEAM_ALLOCATION_BPS: u128 = 1207; // 12.07%
    pub const TOTAL_SUPPLY: Balance = 80_658_175_170_000_000_000_000_000_000u128;
    pub const TEAM_ALLOCATION: Balance = (TOTAL_SUPPLY * 1207) / 10000; // 9,735,419,742.19 52F
    
    pub const RELEASE_COUNT: u32 = 50;
    pub const RELEASE_PERCENTAGE_BPS: u128 = 200; // 2% per release
    pub const RELEASE_INTERVAL: u32 = 5_200_000; // ~6 days at 0.1s blocks
    
    // Derived: 50 releases × 2% = 100% of allocation
    pub const RELEASE_AMOUNT: Balance = TEAM_ALLOCATION / 50; // ~194,708,394.84 52F per release
}

use crate::constants::*;

#[ink::contract]
mod team_treasury {
    use super::*;

    #[ink(storage)]
    pub struct TeamTreasury {
        owner: AccountId,
        token52f: AccountId,
        team_wallet: AccountId,
        
        total_allocated: Balance,
        total_released: Balance,
        release_count: u32,
        last_release_block: BlockNumber,
        
        // Vesting schedule: 50 releases, each 2% of allocation
        releases: [Release; 50],
    }

    #[derive(scale::Encode, scale::Decode, Clone, Copy)]
    pub struct Release {
        amount: Balance,
        block: BlockNumber,
        claimed: bool,
    }

    #[ink(event)]
    pub struct ReleaseClaimed {
        release_number: u32,
        amount: Balance,
        block: BlockNumber,
    }

    #[ink(event)]
    pub struct VestingComplete {
        total_released: Balance,
        final_block: BlockNumber,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotOwner,
        NotTeamWallet,
        ReleaseNotReady,
        AlreadyClaimed,
        VestingComplete,
        TransferFailed,
    }

    impl TeamTreasury {
        #[ink(constructor)]
        pub fn new(token52f: AccountId, team_wallet: AccountId) -> Self {
            let start_block = Self::env().block_number();
            let mut releases = [Release { amount: 0, block: 0, claimed: false }; 50];
            
            // Initialize 50 releases, each ~6 days apart
            for i in 0..50 {
                releases[i] = Release {
                    amount: RELEASE_AMOUNT,
                    block: start_block + (i as u32 * RELEASE_INTERVAL),
                    claimed: false,
                };
            }

            Self {
                owner: Self::env().caller(),
                token52f,
                team_wallet,
                total_allocated: TEAM_ALLOCATION,
                total_released: 0,
                release_count: 0,
                last_release_block: start_block,
                releases,
            }
        }

        // Claim next available release
        #[ink(message)]
        pub fn claim_release(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            if caller != self.team_wallet {
                return Err(Error::NotTeamWallet);
            }

            let current_block = self.env().block_number();
            
            // Find next unclaimed release that's due
            for i in 0..50 {
                if !self.releases[i].claimed && current_block >= self.releases[i].block {
                    let amount = self.releases[i].amount;
                    
                    // Transfer from this contract to team wallet
                    Token52FRef::transfer(&self.token52f, caller, amount)
                        .map_err(|_| Error::TransferFailed)?;
                    
                    self.releases[i].claimed = true;
                    self.total_released += amount;
                    self.release_count += 1;
                    self.last_release_block = current_block;
                    
                    self.env().emit_event(ReleaseClaimed {
                        release_number: (i as u32) + 1,
                        amount,
                        block: current_block,
                    });
                    
                    // Check if complete
                    if self.release_count >= 50 {
                        self.env().emit_event(VestingComplete {
                            total_released: self.total_released,
                            final_block: current_block,
                        });
                    }
                    
                    return Ok(amount);
                }
            }
            
            Err(Error::ReleaseNotReady)
        }

        // View functions
        #[ink(message)]
        pub fn get_next_release(&self) -> (u32, Balance, BlockNumber, bool) {
            let current_block = self.env().block_number();
            
            for i in 0..50 {
                if !self.releases[i].claimed {
                    let ready = current_block >= self.releases[i].block;
                    return ((i as u32) + 1, self.releases[i].amount, self.releases[i].block, ready);
                }
            }
            (0, 0, 0, false) // All claimed
        }

        #[ink(message)]
        pub fn get_vesting_status(&self) -> (Balance, Balance, u32, u32) {
            (
                self.total_allocated,
                self.total_released,
                self.release_count,
                50, // Total releases
            )
        }

        #[ink(message)]
        pub fn get_release_schedule(&self) -> [(Balance, BlockNumber, bool); 50] {
            let mut schedule = [(0, 0, false); 50];
            for i in 0..50 {
                schedule[i] = (self.releases[i].amount, self.releases[i].block, self.releases[i].claimed);
            }
            schedule
        }
    }
}
