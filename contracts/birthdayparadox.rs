#![cfg_attr(not(feature = "std"), no_std)]

use ink::storage::Mapping;

pub type QFBalance = U256;
pub type Balance = u128;

pub mod constants {
    pub const PULSE_BLOCKS: u32 = 187_200; // 5.2 hours
    pub const CRISIS_CHECKS_REQUIRED: u8 = 3;
    pub const CRISIS_CHECK_DELAY_BLOCKS: u32 = 52; // 5.2 seconds between checks
    pub const CRISIS_COOLDOWN_BLOCKS: u32 = 86_400; // 24 hours
    pub const MAX_CRISIS_INJECTION_BPS: u128 = 200; // 2%
    pub const RATIO_CRISIS_THRESHOLD: u128 = 30; // 30% drop
    pub const PRICE_TWAP_THRESHOLD: u128 = 80; // price < 80% twap
    pub const BPS: u128 = 10_000;
    pub const U256_100: U256 = U256::from(100u128);
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
        
        // Crisis tracking with delay
        crisis_count: u8,
        last_crisis_check_block: BlockNumber,
        last_crisis_block: BlockNumber,
        last_ratio: u128,
        
        qf_balance: QFBalance,
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

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotAuthorized,
        InvalidRatio,
        CrisisCooldown,
        InsufficientBalance,
        MathsError,
        TransferFailed,
    }

    impl DampenerVault {
        // ink! v6: Simple constructor, no SpreadAllocate or initialize_contract
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
                crisis_count: 0,
                last_crisis_check_block: 0,
                last_crisis_block: 0,
                last_ratio: target_ratio,
                qf_balance: Self::env().transferred_value(),
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
            
            // Dust guard: < 520 QF
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

            // Crisis check with 5.2s delay between checks
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

        // U256-safe crisis check (no try_into)
        fn is_crisis(&self, current_ratio: u128, price: QFBalance, twap: QFBalance) -> bool {
            // Ratio drop check (u128 is safe for percentages)
            let ratio_drop = if self.last_ratio > current_ratio {
                ((self.last_ratio - current_ratio) * 100) / self.last_ratio
            } else {
                0
            };
            
            // Price vs TWAP check - keep in U256
            if twap.is_zero() {
                return false; // Avoid division by zero
            }
            
            // price_vs_twap = (price * 100) / twap
            let price_vs_twap = (price.saturating_mul(U256_100)) / twap;
            let threshold = U256::from(PRICE_TWAP_THRESHOLD);
            
            // Convert U256 result to u128 safely (it's a percentage, should be small)
            let price_vs_twap_u128: u128 = price_vs_twap.try_into().unwrap_or(100);
            
            ratio_drop > RATIO_CRISIS_THRESHOLD && price_vs_twap_u128 < PRICE_TWAP_THRESHOLD
        }

        fn execute_normal_logic(&mut self, ratio: u128, price: QFBalance, twap: QFBalance, age: u32) -> Result<(), Error> {
            // U256 comparison for price/twap
            let price_lte_twap = price <= twap;
            let price_gt_twap = price > twap;
            
            if ratio < 10 {
                if price_lte_twap {
                    self.inject_liquidity(500)?; // 5%
                }
            } else if ratio >= 10 && ratio <= 17 {
                if price_lte_twap {
                    self.inject_liquidity(100)?; // 1%
                }
            } else if ratio > 17 && age > PULSE_BLOCKS * 4 {
                if price_gt_twap {
                    self.execute_burn()?;
                }
            }
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
            
            // TODO: Swap half QF to 52F via SPIN-Swap
            // add_liquidity(half, 52f_received);
            
            self.qf_balance = self.qf_balance.checked_sub(amount)
                .ok_or(Error::MathsError)?;
            
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
            
            // TODO: Swap QF for 52F and burn
            
            self.qf_balance = self.qf_balance.checked_sub(excess)
                .ok_or(Error::MathsError)?;
            
            self.env().emit_event(BurnExecuted {
                amount: burn_amount,
                ratio: self.get_liquidity_ratio()?,
            });
            
            Ok(())
        }

        fn get_liquidity_ratio(&self) -> Result<u128, Error> {
            // TODO: Query SPIN-Swap pair for real ratio
            Ok(15)
        }

        fn get_excess_qf_above_ratio(&self, target: u128) -> Result<QFBalance, Error> {
            let current = self.get_liquidity_ratio()?;
            if current <= target {
                return Ok(QFBalance::from(0u128));
            }
            Ok(self.qf_balance / QFBalance::from(10u128))
        }

        // Admin functions
        #[ink(message)]
        pub fn set_spin_swap_pair(&mut self, pair: AccountId) -> Result<(), Error> {
            self.ensure_owner()?;
            self.spin_swap_pair = Some(pair);
            Ok(())
        }

        #[ink(message)]
        pub fn set_price_oracle(&mut self, oracle: AccountId) -> Result<(), Error> {
            self.ensure_owner()?;
            self.price_oracle = Some(oracle);
            Ok(())
        }

        #[ink(message)]
        pub fn set_birthday_paradox(&mut self, paradox: AccountId) -> Result<(), Error> {
            self.ensure_owner()?;
            self.birthday_paradox = Some(paradox);
            Ok(())
        }

        fn ensure_owner(&self) -> Result<(), Error> {
            // Add owner check if needed
            Ok(())
        }

        // View functions
        #[ink(message)]
        pub fn get_crisis_status(&self) -> (u8, BlockNumber, BlockNumber) {
            (self.crisis_count, self.last_crisis_check_block, self.last_crisis_block)
        }

        #[ink(message)]
        pub fn get_qf_balance(&self) -> QFBalance {
            self.qf_balance
        }
    }
}
