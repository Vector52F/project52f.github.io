#![cfg_attr(not(feature = "std"), no_std)]

use ink::storage::traits::SpreadAllocate;

#[ink::contract]
mod dampener_vault {
    use ink::prelude::vec::Vec;

    const PULSE_BLOCKS: u32 = 187_200; // 5.2 hours
    const CRISIS_CHECKS_REQUIRED: u8 = 3;
    const CRISIS_COOLDOWN_BLOCKS: u32 = 86_400; // 24 hours
    const MAX_CRISIS_INJECTION_BPS: u128 = 200; // 2%
    const RATIO_CRISIS_THRESHOLD: u128 = 30; // 30% drop
    const PRICE_TWAP_THRESHOLD: u128 = 80; // Price < 80% of TWAP

    #[ink(storage)]
    #[derive(SpreadAllocate)]
    pub struct DampenerVault {
        launch_block: BlockNumber,
        last_action_block: BlockNumber,
        target_ratio: u128,      // 15 = 15% Liquidity:MC
        qf_reserve_percent: u128, // 5 = 5% of LP depth for agile reserve
        spin_swap_pair: Option<AccountId>,
        price_oracle: Option<AccountId>,
        birthday_paradox: Option<AccountId>,
        
        // Crisis tracking
        crisis_count: u8,
        last_crisis_block: BlockNumber,
        last_ratio: u128,
        
        // Accumulated QF for liquidity operations
        qf_balance: Balance,
    }

    #[ink(event)]
    pub struct LiquidityInjected {
        amount: Balance,
        ratio_before: u128,
        ratio_after: u128,
    }

    #[ink(event)]
    pub struct CrisisActivated {
        ratio_drop: u128,
        injection_amount: Balance,
    }

    #[ink(event)]
    pub struct BurnExecuted {
        amount: Balance,
        ratio: u128,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotAuthorized,
        InvalidRatio,
        CrisisCooldown,
        InsufficientBalance,
    }

    impl DampenerVault {
        #[ink(constructor)]
        pub fn new(target_ratio: u128) -> Self {
            ink::env::initialize_contract(|contract: &mut Self| {
                contract.launch_block = Self::env().block_number();
                contract.last_action_block = Self::env().block_number();
                contract.target_ratio = target_ratio;
                contract.qf_reserve_percent = 5;
                contract.crisis_count = 0;
                contract.last_crisis_block = 0;
                contract.last_ratio = target_ratio;
                contract.qf_balance = 0;
            })
        }

        #[ink(message, payable)]
        pub fn receive_qf(&mut self) {
            // Accept QF transfers from Token52F
            self.qf_balance += self.env().transferred_value();
        }

        #[ink(message)]
        pub fn process_trade(&mut self, trade_value: Balance, price: Balance, twap: Balance) -> Result<(), Error> {
            // Only BirthdayParadox can call
            if Some(self.env().caller()) != self.birthday_paradox {
                return Err(Error::NotAuthorized);
            }

            let current_block = self.env().block_number();
            
            // Dust guard: ignore tiny trades (< 520 QF)
            if trade_value < 520 * 10_000_000_000_000_000_000 {
                return Ok(());
            }

            let current_ratio = self.get_liquidity_ratio()?;
            let age = current_block - self.launch_block;

            // Check 1: Normal 5.2h pulse
            if current_block - self.last_action_block >= PULSE_BLOCKS {
                self.execute_normal_logic(current_ratio, price, twap, age)?;
                self.last_action_block = current_block;
            }

            // Check 2: Crisis override (automated)
            if self.is_crisis(current_ratio, price, twap) {
                self.crisis_count += 1;
                
                if self.crisis_count >= CRISIS_CHECKS_REQUIRED 
                    && current_block - self.last_crisis_block >= CRISIS_COOLDOWN_BLOCKS {
                    
                    self.execute_crisis_injection(current_ratio)?;
                    self.crisis_count = 0;
                    self.last_crisis_block = current_block;
                }
            } else {
                self.crisis_count = 0; // Reset if conditions normalize
            }

            self.last_ratio = current_ratio;
            Ok(())
        }

        fn is_crisis(&self, current_ratio: u128, price: Balance, twap: Balance) -> bool {
            // Ratio dropped 30%+
            let ratio_drop = if self.last_ratio > current_ratio {
                ((self.last_ratio - current_ratio) * 100) / self.last_ratio
            } else {
                0
            };
            
            // Price below 80% of TWAP
            let price_vs_twap = if twap > 0 {
                (price * 100) / twap
            } else {
                100
            };
            
            ratio_drop > RATIS_CRISIS_THRESHOLD && price_vs_twap < PRICE_TWAP_THRESHOLD
        }

        fn execute_normal_logic(&mut self, ratio: u128, price: Balance, twap: Balance, age: u32) -> Result<(), Error> {
            if ratio < 10 {
                // Crisis mode: aggressive support (only if price <= TWAP)
                if price <= twap {
                    self.inject_liquidity(500)?; // 5% in BPS
                }
            } else if ratio >= 10 && ratio <= 17 {
                // Normal: gentle support
                if price <= twap {
                    self.inject_liquidity(100)?; // 1% in BPS
                }
            } else if ratio > 17 && age > PULSE_BLOCKS * 4 {
                // Prosperity: burn mode (after ~21 days, price > TWAP)
                if price > twap {
                    self.execute_burn()?;
                }
            }
            
            Ok(())
        }

        fn execute_crisis_injection(&mut self, ratio: u128) -> Result<(), Error> {
            let max_injection = (self.qf_balance * MAX_CRISIS_INJECTION_BPS) / 10_000;
            
            self.env().emit_event(CrisisActivated {
                ratio_drop: ((self.last_ratio - ratio) * 100) / self.last_ratio,
                injection_amount: max_injection,
            });
            
            self.inject_liquidity(MAX_CRISIS_INJECTION_BPS as u128)?;
            
            Ok(())
        }

        fn inject_liquidity(&mut self, bps: u128) -> Result<(), Error> {
            let amount = (self.qf_balance * bps) / 10_000;
            if amount == 0 {
                return Ok(());
            }

            // Swap half to 52F, pair with half QF, add to LP
            let half = amount / 2;
            
            // Cross-contract call to SPIN-Swap
            // swap_qf_for_52f(half);
            // add_liquidity(half, 52f_received);
            
            self.qf_balance -= amount;
            
            self.env().emit_event(LiquidityInjected {
                amount,
                ratio_before: self.last_ratio,
                ratio_after: self.get_liquidity_ratio()?,
            });
            
            Ok(())
        }

        fn execute_burn(&mut self) -> Result<(), Error> {
            let excess = self.get_excess_qf_above_ratio(17)?;
            if excess == 0 {
                return Ok(());
            }
            
            let burn_amount = (excess * 75) / 100; // 75% burn
            let retain = excess - burn_amount; // 25% stay
            
            // Swap QF for 52F, burn the 52F
            // swap_and_burn(burn_amount);
            
            self.qf_balance -= excess;
            
            self.env().emit_event(BurnExecuted {
                amount: burn_amount,
                ratio: self.get_liquidity_ratio()?,
            });
            
            Ok(())
        }

        // Real DEX integration
        fn get_liquidity_ratio(&self) -> Result<u128, Error> {
            // Query SPIN-Swap pair reserves
            // ratio = (LP_depth / Market_Cap) * 100
            // Placeholder: return mock value for compilation
            Ok(15)
        }

        fn get_excess_qf_above_ratio(&self, target: u128) -> Result<Balance, Error> {
            let current = self.get_liquidity_ratio()?;
            if current <= target {
                return Ok(0);
            }
            
            // Calculate QF amount that would bring ratio to target
            // Placeholder: return mock value
            Ok(self.qf_balance / 10)
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
    }
}
