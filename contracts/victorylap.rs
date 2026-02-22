#![cfg_attr(not(feature = "std"), no_std)]

pub type QFBalance = U256;
pub type Balance = u128;

pub mod constants {
    // 6.25% threshold already checked by DampenerVault
    // Minimum excess enforced there too
    pub const SPLIT_QUARTERS: u8 = 4; // 25% each
    pub const BPS: u128 = 10_000;
}

use crate::constants::*;

#[ink::contract]
mod victory_lap_satellite {
    use super::*;

    #[ink(storage)]
    pub struct VictoryLapSatellite {
        owner: AccountId,
        dampener_vault: AccountId,
        token52f: AccountId,
        dex_router: AccountId,
        
        total_victory_laps: u64,
        total_qf_burned: QFBalance,
        total_qf_to_52f_burned: QFBalance,
        total_52f_burned: Balance,
        last_lap_block: BlockNumber,
        
        paused: bool,
    }

    #[ink(event)]
    pub struct VictoryLap {
        #[ink(topic)] lap_number: u64,
        #[ink(topic)] triggered_by: AccountId,
        excess_qf: QFBalance,
        qf_burned_direct: QFBalance,
        qf_swapped_to_52f: QFBalance,
        fifty_two_f_burned: Balance,
        block: BlockNumber,
    }

    #[ink(event)]
    pub struct FreeBeerForEveryone {
        message: Vec<u8>,
        block: BlockNumber,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotAuthorized,
        BelowThreshold,
        DexSwapFailed,
        BurnFailed,
        Paused,
        InvalidSplit,
    }

    impl VictoryLapSatellite {
        #[ink(constructor)]
        pub fn new(dampener: AccountId, token52f: AccountId, dex: AccountId) -> Self {
            Self {
                owner: Self::env().caller(),
                dampener_vault: dampener,
                token52f,
                dex_router: dex,
                total_victory_laps: 0,
                total_qf_burned: QFBalance::from(0u128),
                total_qf_to_52f_burned: QFBalance::from(0u128),
                total_52f_burned: 0,
                last_lap_block: 0,
                paused: false,
            }
        }

        // THE VICTORY LAP: Receives QF from DampenerVault
        #[ink(message, payable)]
        pub fn execute_victory_lap(&mut self) -> Result<(), Error> {
            if self.paused {
                return Err(Error::Paused);
            }

            // Only DampenerVault can send QF and trigger
            if self.env().caller() != self.dampener_vault {
                return Err(Error::NotAuthorized);
            }

            let qf_received = self.env().transferred_value();
            
            if qf_received.is_zero() {
                return Err(Error::BelowThreshold);
            }

            // Split into 4 quarters (25% each)
            let quarter = qf_received / QFBalance::from(SPLIT_QUARTERS);
            
            if quarter.is_zero() {
                return Err(Error::InvalidSplit);
            }

            // Q1: Burn QF directly (25%)
            self.burn_qf(quarter)?;
            
            // Q2: Burn QF directly (25%) = 50% total QF burn
            self.burn_qf(quarter)?;
            
            // Q3: Swap QF→52F, burn 52F (25%)
            let fifty_two_f_1 = self.swap_and_burn(quarter)?;
            
            // Q4: Swap QF→52F, burn 52F (25%) = 50% total for 52F buyback+burn
            let fifty_two_f_2 = self.swap_and_burn(quarter)?;

            // Update stats
            self.total_victory_laps += 1;
            self.total_qf_burned += quarter * QFBalance::from(2u128);
            self.total_qf_to_52f_burned += quarter * QFBalance::from(2u128);
            self.total_52f_burned += fifty_two_f_1 + fifty_two_f_2;
            self.last_lap_block = self.env().block_number();

            // THE PARTY
            self.env().emit_event(VictoryLap {
                lap_number: self.total_victory_laps,
                triggered_by: self.env().caller(),
                excess_qf: qf_received,
                qf_burned_direct: quarter * QFBalance::from(2u128),
                qf_swapped_to_52f: quarter * QFBalance::from(2u128),
                fifty_two_f_burned: fifty_two_f_1 + fifty_two_f_2,
                block: self.env().block_number(),
            });

            self.env().emit_event(FreeBeerForEveryone {
                message: b"52F TO THE MOON! VICTORY LAP COMPLETE! FREE BEER FOR EVERYONE!".to_vec(),
                block: self.env().block_number(),
            });

            Ok(())
        }

        fn burn_qf(&mut self, amount: QFBalance) -> Result<(), Error> {
            // Burn native QF to zero address
            let burn_address = AccountId::from([0u8; 32]);
            self.env().transfer(burn_address, amount)
                .map_err(|_| Error::BurnFailed)
        }

        fn swap_and_burn(&mut self, qf_amount: QFBalance) -> Result<Balance, Error> {
            // Execute swap: QF → 52F via SPIN-Swap
            let fifty_two_f_received = self.execute_dex_swap(qf_amount)?;
            
            // Burn received 52F via Token52F contract
            Token52FRef::burn_from_satellite(&self.token52f, fifty_two_f_received)
                .map_err(|_| Error::BurnFailed)?;
            
            Ok(fifty_two_f_received)
        }

        fn execute_dex_swap(&self, qf_in: QFBalance) -> Result<Balance, Error> {
            // Cross-contract call to SPIN-Swap router
            // RouterRef::swap_exact_native_for_tokens(
            //     qf_in,
            //     min_out,
            //     path: [QF, 52F],
            //     to: self.env().account_id(),
            //     deadline: now + 300
            // )
            
            // Placeholder: assume 1:1 rate for compilation
            let out: Balance = qf_in.try_into().unwrap_or(0);
            Ok(out)
        }

        // Admin: Manual victory lap for special occasions
        #[ink(message, payable)]
        pub fn force_victory_lap(&mut self) -> Result<(), Error> {
            self.ensure_owner()?;
            
            let qf_received = self.env().transferred_value();
            let half = qf_received / QFBalance::from(2u128);
            
            self.burn_qf(half)?;
            let fifty_two_f = self.swap_and_burn(half)?;
            
            self.total_victory_laps += 1;
            
            self.env().emit_event(VictoryLap {
                lap_number: self.total_victory_laps,
                triggered_by: self.env().caller(),
                excess_qf: qf_received,
                qf_burned_direct: half,
                qf_swapped_to_52f: half,
                fifty_two_f_burned: fifty_two_f,
                block: self.env().block_number(),
            });
            
            Ok(())
        }

        #[ink(message)]
        pub fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
            self.ensure_owner()?;
            self.paused = paused;
            Ok(())
        }

        fn ensure_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotAuthorized);
            }
            Ok(())
        }

        // Views
        #[ink(message)]
        pub fn get_stats(&self) -> (u64, QFBalance, QFBalance, Balance, BlockNumber) {
            (
                self.total_victory_laps,
                self.total_qf_burned,
                self.total_qf_to_52f_burned,
                self.total_52f_burned,
                self.last_lap_block,
            )
        }

        #[ink(message)]
        pub fn get_message(&self) -> Vec<u8> {
            b"52F TO THE MOON!".to_vec()
        }
    }
}
