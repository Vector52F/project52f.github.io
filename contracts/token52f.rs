#![cfg_attr(not(feature = "std"), no_std)]

use ink::storage::Mapping;

pub type QFBalance = U256;
pub type Balance = u128;

pub mod constants {
    pub const TOTAL_SUPPLY: Balance = 80_658_175_170_000_000_000_000_000_000u128;
    pub const TEAM_ALLOCATION_BPS: u128 = 1207; // 12.07%
    pub const TEAM_ALLOCATION: Balance = (TOTAL_SUPPLY * 1207) / 10000;
    
    pub const RELEASE_COUNT: u32 = 50;
    pub const RELEASE_INTERVAL: u32 = 5_200_000; // ~6 days at 0.1s blocks
    pub const RELEASE_AMOUNT: Balance = TEAM_ALLOCATION / 50;
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
        
        releases: Mapping<u32, Release>,
    }

    #[derive(scale::Encode, scale::Decode, Clone, Copy, Default)]
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
        InvalidRelease,
    }

    impl TeamTreasury {
        #[ink(constructor)]
        pub fn new(token52f: AccountId, team_wallet: AccountId) -> Self {
            let start_block = Self::env().block_number();
            let mut releases = Mapping::default();
            
            for i in 0..50 {
                releases.insert(i, &Release {
                    amount: RELEASE_AMOUNT,
                    block: start_block + ((i as u32 + 1) * RELEASE_INTERVAL),
                    claimed: false,
                });
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

        #[ink(message)]
        pub fn claim_release(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            if caller != self.team_wallet {
                return Err(Error::NotTeamWallet);
            }

            let current_block = self.env().block_number();
            
            if self.release_count >= 50 {
                return Err(Error::VestingComplete);
            }
            
            let release = self.releases.get(self.release_count).ok_or(Error::InvalidRelease)?;
            
            if release.claimed {
                return Err(Error::AlreadyClaimed);
            }
            
            if current_block < release.block {
                return Err(Error::ReleaseNotReady);
            }
            
            Token52FRef::transfer(&self.token52f, caller, release.amount)
                .map_err(|_| Error::TransferFailed)?;
            
            self.releases.insert(self.release_count, &Release { claimed: true, ..release });
            self.total_released += release.amount;
            self.release_count += 1;
            self.last_release_block = current_block;
            
            self.env().emit_event(ReleaseClaimed {
                release_number: self.release_count,
                amount: release.amount,
                block: current_block,
            });
            
            if self.release_count >= 50 {
                self.env().emit_event(VestingComplete {
                    total_released: self.total_released,
                    final_block: current_block,
                });
            }
            
            Ok(release.amount)
        }

        #[ink(message)]
        pub fn get_next_release(&self) -> (u32, Balance, BlockNumber, bool) {
            if self.release_count >= 50 {
                return (0, 0, 0, false);
            }
            
            let release = self.releases.get(self.release_count).unwrap_or_default();
            let current_block = self.env().block_number();
            let ready = current_block >= release.block && !release.claimed;
            
            (self.release_count + 1, release.amount, release.block, ready)
        }

        #[ink(message)]
        pub fn get_vesting_status(&self) -> (Balance, Balance, u32, u32) {
            (
                self.total_allocated,
                self.total_released,
                self.release_count,
                50,
            )
        }

        #[ink(message)]
        pub fn get_release_info(&self, index: u32) -> (Balance, BlockNumber, bool) {
            let release = self.releases.get(index).unwrap_or_default();
            (release.amount, release.block, release.claimed)
        }
    }
}
