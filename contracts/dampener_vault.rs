#![cfg_attr(not(feature = "std"), no_std)]

use ink::storage::Mapping;

pub type QFBalance = U256;
pub type Balance = u128;

pub mod constants {
    pub const PULSE_BLOCKS: u32 = 187_200;
    pub const CRISIS_CHECKS_REQUIRED: u8 = 3;
    pub const CRISIS_CHECK_DELAY_BLOCKS: u32 = 52;
    pub const CRISIS_COOLDOWN_BLOCKS: u32 = 86_400;
    pub const MAX_CRISIS_INJECTION_BPS: u128 = 200;
    pub const RATIO_CRISIS_THRESHOLD: u128 = 30;
    pub const PRICE_TWAP_THRESHOLD: u128 = 80;
    pub const BPS: u128 = 10_000;
    pub const U256_100: U256 = U256::from(100u128);
    
    // Victory Lap: 6.25% injection capacity threshold
    pub const VICTORY_LAP_THRESHOLD_BPS: u128 = 625;
    pub const MIN_VICTORY_LAP_QF: QFBalance = QFBalance::from(1_000_000_000_000_000_000_000u128);
}

use crate::constants::*;

#[ink::contract]
mod dampener_vault {
    use super::*;

    #[ink(storage)]
    pub struct DampenerVault {
        launch_block: BlockNumber,
        last_action_block: BlockNumber,
        target_ratio: u128,
        qf_reserve_percent: u128,
        spin_swap_pair: Option<AccountId>,
        price_oracle: Option<AccountId>,
        birthday_paradox: Option<AccountId>,
        victory_lap_satellite: Option<AccountId>,
        
        crisis_count: u8,
        last_crisis_check_block: BlockNumber,
        last_crisis_block: BlockNumber,
        last_ratio: u128,
        
        qf_balance: QFBalance,
        
        // Stats
        total_injected: QFBalance,
        total_burned: QFBalance,
        total_victory_laps_triggered: u64,
    }

    #[ink(event)]
    pub struct LiquidityInjected {
        amount: QFBalance,
        ratio_before: u128,
        ratio_after: u128,
    }

    #[ink(event)]
    pub struct CrisisActivated {
        ratio_drop: u128,
        injection_amount: QFBalance,
    }

    #[ink(event)]
    pub struct BurnExecuted {
        amount: QFBalance,
        ratio: u128,
    }

    #[ink(event)]
    pub struct VictoryLapTriggered {
        excess_qf: QFBalance,
        threshold: QFBalance,
        lp_depth: QFBalance,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotAuthorized,
        InvalidRatio,
        CrisisCooldown,
        InsufficientBalance,
        MathsError,
        TransferFailed,
        VictoryLapNotConfigured,
        BelowVictoryThreshold,
        NotProsperous,
    }

    impl DampenerVault {
        #[ink(constructor, payable)]
        pub fn new(target_ratio: u128) -> Self {
            let launch = Self::env().block_number();
            
            Self {
                launch_block: launch,
                last_action_block: launch,
                target_ratio,
                qf_reserve_percent: 5,
                spin_swap_pair: None,
                price_oracle: None,
                birthday_paradox: None,
                victory_lap_satellite: None,
                crisis_count: 0,
                last_crisis_check_block: 0,
                last_crisis_block: 0,
                last_ratio: target_ratio,
                qf_balance: Self::env().transferred_value(),
                total_injected: QFBalance::from(0u128),
                total_burned: QFBalance::from(0u128),
                total_victory_laps_triggered: 0,
            }
        }

        #[ink(message, payable)]
        pub fn receive_qf(&mut self) {
            self.qf_balance += self.env().transferred_value();
        }

        #[ink(message)]
        pub fn process_trade(&mut self, trade_value: Balance, price: QFBalance, twap: QFBalance) -> Result<(), Error> {
            if Some(self.env().caller()) != self.birthday_paradox {
                return Err(Error::NotAuthorized);
            }

            let current_block = self.env().block_number();
            
            let dust_limit = QFBalance::from(520_000_000_000_000_000_000u128);
            if QFBalance::from(trade_value) < dust_limit {
                return Ok(());
            }

            let current_ratio = self.get_liquidity_ratio()?;
            let age = current_block - self.launch_block;

            // Normal 5.2h pulse
            if current_block - self.last_action_block >= PULSE_BLOCKS {
                self.execute_normal_logic(current_ratio, price, twap, age)?;
                self.last_action_block = current_block;
            }

            // Crisis check with delay
            if current_block - self.last_crisis_check_block >= CRISIS_CHECK_DELAY_BLOCKS {
                if self.is_crisis(current_ratio, price, twap) {
                    self.crisis_count += 1;
                    
                    if self.crisis_count >= CRISIS_CHECKS_REQUIRED 
                        && current_block - self.last_crisis_block >= CRISIS_COOLDOWN_BLOCKS {
                        
                        self.execute_crisis_injection(current_ratio)?;
                        self.crisis_count = 0;
                        self.last_crisis_block = current_block;
                    }
                } else {
                    self.crisis_count = 0;
                }
                self.last_crisis_check_block = current_block;
            }

            self.last_ratio = current_ratio;
            Ok(())
        }

        fn is_crisis(&self, current_ratio: u128, price: QFBalance, twap: QFBalance) -> bool {
            let ratio_drop = if self.last_ratio > current_ratio {
                ((self.last_ratio - current_ratio) * 100) / self.last_ratio
            } else {
                0
            };
            
            if twap.is_zero() {
                return false;
            }
            
            let price_vs_twap = (price.saturating_mul(U256_100)) / twap;
            let price_vs_twap_u128: u128 = price_vs_twap.try_into().unwrap_or(100);
            
            ratio_drop > RATIO_CRISIS_THRESHOLD && price_vs_twap_u128 < PRICE_TWAP_THRESHOLD
        }

        fn execute_normal_logic(&mut self, ratio: u128, price: QFBalance, twap: QFBalance, age: u32) -> Result<(), Error> {
            let price_lte_twap = price <= twap;
            let price_gt_twap = price > twap;
            
            if ratio < 10 {
                if price_lte_twap {
                    self.inject_liquidity(500)?;
                }
            } else if ratio >= 10 && ratio <= 17 {
                if price_lte_twap {
                    self.inject_liquidity(100)?;
                }
            } else if ratio > 17 && age > PULSE_BLOCKS * 4 {
                if price_gt_twap {
                    // PROSPERITY: Try Victory Lap first, then normal burn
                    match self.check_victory_lap_trigger() {
                        Ok(_) => {},
                        Err(_) => {
                            // Victory Lap not triggered, do normal burn
                            self.execute_burn()?;
                        }
                    }
                }
            }
            Ok(())
        }

        // VICTORY LAP: Check and trigger if conditions met
        fn check_victory_lap_trigger(&mut self) -> Result<(), Error> {
            let satellite = self.victory_lap_satellite.ok_or(Error::VictoryLapNotConfigured)?;
            
            // Get LP depth (would query DEX in real implementation)
            let lp_depth = self.get_lp_depth_qf()?;
            
            // Calculate 6.25% threshold
            let threshold = (lp_depth * QFBalance::from(VICTORY_LAP_THRESHOLD_BPS)) 
                / QFBalance::from(10_000u128);
            
            // Check if we have excess above threshold
            if self.qf_balance <= threshold {
                return Err(Error::BelowVictoryThreshold);
            }
            
            let excess = self.qf_balance - threshold;
            
            if excess < MIN_VICTORY_LAP_QF {
                return Err(Error::BelowVictoryThreshold);
            }
            
            // Send excess QF to VictoryLapSatellite with the call
            self.env().transfer(satellite, excess)
                .map_err(|_| Error::TransferFailed)?;
            
            // Trigger the party
            VictoryLapSatelliteRef::execute_victory_lap(&satellite)?;
            
            // Update our balance (keep 6.25% threshold)
            self.qf_balance = threshold;
            self.total_victory_laps_triggered += 1;
            
            self.env().emit_event(VictoryLapTriggered {
                excess_qf: excess,
                threshold,
                lp_depth,
            });
            
            Ok(())
        }

        fn execute_crisis_injection(&mut self, ratio: u128) -> Result<(), Error> {
            let max_injection = (self.qf_balance * QFBalance::from(MAX_CRISIS_INJECTION_BPS)) 
                / QFBalance::from(BPS);
            
            self.env().emit_event(CrisisActivated {
                ratio_drop: ((self.last_ratio - ratio) * 100) / self.last_ratio,
                injection_amount: max_injection,
            });
            
            self.inject_liquidity(MAX_CRISIS_INJECTION_BPS)?;
            Ok(())
        }

        fn inject_liquidity(&mut self, bps: u128) -> Result<(), Error> {
            let amount = (self.qf_balance * QFBalance::from(bps)) / QFBalance::from(BPS);
            
            if amount.is_zero() {
                return Ok(());
            }

            let half = amount / QFBalance::from(2u128);
            
            self.qf_balance = self.qf_balance.checked_sub(amount)
                .ok_or(Error::MathsError)?;
            
            self.total_injected += amount;
            
            self.env().emit_event(LiquidityInjected {
                amount,
                ratio_before: self.last_ratio,
                ratio_after: self.get_liquidity_ratio()?,
            });
            
            Ok(())
        }

        fn execute_burn(&mut self) -> Result<(), Error> {
            let excess = self.get_excess_qf_above_ratio(17)?;
            
            if excess.is_zero() {
                return Ok(());
            }
            
            let burn_amount = (excess * QFBalance::from(75u128)) / QFBalance::from(100u128);
            
            self.qf_balance = self.qf_balance.checked_sub(excess)
                .ok_or(Error::MathsError)?;
            
            self.total_burned += burn_amount;
            
            self.env().emit_event(BurnExecuted {
                amount: burn_amount,
                ratio: self.get_liquidity_ratio()?,
            });
            
            Ok(())
        }

        fn get_liquidity_ratio(&self) -> Result<u128, Error> {
            Ok(15)
        }

        fn get_excess_qf_above_ratio(&self, target: u128) -> Result<QFBalance, Error> {
            let current = self.get_liquidity_ratio()?;
            if current <= target {
                return Ok(QFBalance::from(0u128));
            }
            Ok(self.qf_balance / QFBalance::from(10u128))
        }

        fn get_lp_depth_qf(&self) -> Result<QFBalance, Error> {
            // Would query SPIN-Swap pair for actual LP depth
            // Placeholder: assume 10x our balance
            Ok(self.qf_balance * QFBalance::from(10u128))
        }

        // Admin functions
        #[ink(message)]
        pub fn set_spin_swap_pair(&mut self, pair: AccountId) -> Result<(), Error> {
            self.spin_swap_pair = Some(pair);
            Ok(())
        }

        #[ink(message)]
        pub fn set_price_oracle(&mut self, oracle: AccountId) -> Result<(), Error> {
            self.price_oracle = Some(oracle);
            Ok(())
        }

        #[ink(message)]
        pub fn set_birthday_paradox(&mut self, paradox: AccountId) -> Result<(), Error> {
            self.birthday_paradox = Some(paradox);
            Ok(())
        }

        #[ink(message)]
        pub fn set_victory_lap_satellite(&mut self, satellite: AccountId) -> Result<(), Error> {
            self.victory_lap_satellite = Some(satellite);
            Ok(())
        }

        // Views
        #[ink(message)]
        pub fn get_crisis_status(&self) -> (u8, BlockNumber, BlockNumber) {
            (self.crisis_count, self.last_crisis_check_block, self.last_crisis_block)
        }

        #[ink(message)]
        pub fn get_stats(&self) -> (QFBalance, QFBalance, u64) {
            (self.total_injected, self.total_burned, self.total_victory_laps_triggered)
        }

        #[ink(message)]
        pub fn check_victory_lap_eligibility(&self) -> (bool, QFBalance, QFBalance) {
            if let Ok(lp_depth) = self.get_lp_depth_qf() {
                let threshold = (lp_depth * QFBalance::from(VICTORY_LAP_THRESHOLD_BPS)) 
                    / QFBalance::from(10_000u128);
                let excess = if self.qf_balance > threshold { 
                    self.qf_balance - threshold 
                } else { 
                    QFBalance::from(0u128) 
                };
                return (excess >= MIN_VICTORY_LAP_QF, excess, threshold);
            }
            (false, QFBalance::from(0u128), QFBalance::from(0u128))
        }
    }
}
