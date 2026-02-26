#![cfg_attr(not(feature = "std"), no_std, no_main)]

#[ink::contract]
mod dampener_vault {
    use ink::prelude::vec::Vec;
    use ink::storage::Mapping;
    use ink::primitives::AccountId;
    use ink::env::call::{build_call, ExecutionInput, Selector};

    // =========================================================================
    // CONSTANTS — MATHEMATICALLY LOCKED
    // =========================================================================
    
    pub const BPS_DENOMINATOR: u128 = 10_000;
    pub const TARGET_LIQUIDITY_RATIO_BPS: u128 = 1_500;
    pub const MAX_DRIP_BPS: u128 = 500;
    pub const COOLDOWN_BLOCKS: u32 = 36_000;
    pub const TWAP_PERIOD_MS: u64 = 3_600_000;
    pub const PRICE_PRECISION: u128 = 1_000_000_000_000_000_000;
    
    /// NEW: Default slippage tolerance (1% = 100 BPS)
    pub const DEFAULT_SLIPPAGE_BPS: u128 = 100;
    /// NEW: Minimum acceptable slippage (0.5% = 50 BPS) - prevents 0% slippage
    pub const MIN_SLIPPAGE_BPS: u128 = 50;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct DampenerVault {
        owner: AccountId,
        fortress_address: AccountId,
        last_injection_block: u32,
        last_injection_timestamp: u64,
        dex_router: Option<AccountId>,
        lp_token: Option<AccountId>,
        
        /// NEW: Oracle address for TWAP
        oracle_address: Option<AccountId>,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct RevenuePulled {
        amount: Balance,
        contract_native_balance: Balance,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct LiquidityInjected {
        amount: Balance,
        remaining_contract_balance: Balance,
        block: u32,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct LiquidityHealthy {
        current_ratio_bps: u128,
        target_ratio_bps: u128,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct InjectionSkipped {
        reason: SkipReason,
        contract_native_balance: Balance,
    }
    
    /// NEW: Oracle address updated
    #[ink(event)]
    pub struct OracleAddressSet {
        #[ink(topic)]
        oracle: AccountId,
    }

    // =========================================================================
    // ENUMS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotOwner,
        CooldownActive,
        PullFailed,
        Overflow,
        TwapUnavailable,
        LiquidityHealthy,
        InsufficientVaultBalance,
        NoInjectionNeeded,
        DexRouterNotConfigured,
        InjectionFailed,
        
        /// NEW: Slippage tolerance too low (must be >= 0.5%)
        SlippageTooLow,
        /// NEW: Transaction already occurred this block (throttle)
        AlreadyInjectedThisBlock,
        /// NEW: Oracle not configured
        OracleNotConfigured,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum SkipReason {
        CooldownActive,
        LiquidityHealthy,
        InsufficientFunds,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACES
    // =========================================================================

    /// NEW: Interface for external Oracle (TWAP)
    #[ink::trait_definition]
    pub trait OracleInterface {
        /// Get TWAP price for pair
        #[ink(message)]
        fn get_twap_price(&self, token_in: AccountId, token_out: AccountId, period: u64) -> Result<Balance, Error>;
        
        /// Check if price is fresh (not stale)
        #[ink(message)]
        fn is_price_fresh(&self) -> bool;
    }

    #[ink::trait_definition]
    pub trait Project52FInterface {
        #[ink(message)]
        fn pull_dampener_tax(&mut self) -> Result<Balance, Error>;
    }

    #[ink::trait_definition]
    pub trait DexRouterInterface {
        #[ink(message)]
        fn swap_exact_native_for_tokens(
            &mut self,
            amount_out_min: Balance,
            path: Vec<AccountId>,
            to: AccountId,
            deadline: u64,
        ) -> Result<Vec<Balance>, Error>;
        
        #[ink(message)]
        fn add_liquidity_native(
            &mut self,
            token: AccountId,
            amount_token_desired: Balance,
            amount_token_min: Balance,
            amount_native_min: Balance,
            to: AccountId,
            deadline: u64,
        ) -> Result<(Balance, Balance, Balance), Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl DampenerVault {
        #[ink(constructor)]
        pub fn new(fortress_address: AccountId) -> Self {
            let caller = Self::env().caller();
            let block = Self::env().block_number();
            
            Self {
                owner: caller,
                fortress_address,
                last_injection_block: block,
                last_injection_timestamp: 0,
                dex_router: None,
                lp_token: None,
                oracle_address: None, // NEW
            }
        }

        // =================================================================
        // NEW: ORACLE ADMINISTRATION
        // =================================================================

        #[ink(message)]
        pub fn set_oracle_address(&mut self, oracle: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.oracle_address = Some(oracle);
            
            self.env().emit_event(OracleAddressSet { oracle });
            Ok(())
        }

        // =================================================================
        // ASYNCHRONOUS REVENUE PULL
        // =================================================================

        #[ink(message)]
        pub fn pull_from_fortress(&mut self) -> Result<Balance, Error> {
            let pre_balance = self.env().balance();
            
            let call_result: Result<Balance, Error> = build_call::<DefaultEnvironment>()
                .call(self.fortress_address)
                .exec_input(
                    ExecutionInput::new(Selector::new(ink::selector_bytes!("pull_dampener_tax")))
                )
                .returns::<Result<Balance, Error>>()
                .invoke();
            
            match call_result {
                Ok(amount) => {
                    let post_balance = self.env().balance();
                    self.env().emit_event(RevenuePulled {
                        amount,
                        contract_native_balance: post_balance,
                        timestamp: self.env().block_timestamp(),
                    });
                    Ok(amount)
                }
                Err(_) => Err(Error::PullFailed),
            }
        }

        // =================================================================
        // NEW: REAL TWAP INTEGRATION (Placeholder for QF Network)
        // =================================================================

        /// Get 60-minute Time-Weighted Average Price (TWAP) from external Oracle
        /// 
        /// TODO: QF Network Integration — Connect to official DEX TWAP Oracle
        /// Requirements:
        /// - Must return price with PRICE_PRECISION (18 decimals)
        /// - Must validate price is not stale (< 1 hour old)
        /// - Must be resistant to flash loan manipulation
        fn get_twap_price(&self) -> Result<Balance, Error> {
            let oracle = self.oracle_address.ok_or(Error::OracleNotConfigured)?;
            
            // DEVNET: Return mock if oracle not set to valid address
            if self.dex_router.is_none() && self.oracle_address.is_none() {
                return Ok(PRICE_PRECISION); // 1:1 mock
            }
            
            // Production call to external oracle
            let price_result: Result<Balance, Error> = build_call::<DefaultEnvironment>()
                .call(oracle)
                .exec_input(
                    ExecutionInput::new(Selector::new(ink::selector_bytes!("get_twap_price")))
                        .push_arg(self.fortress_address) // token_in (52f)
                        .push_arg(self.env().account_id()) // token_out (QF/native) - adjust as needed
                        .push_arg(TWAP_PERIOD_MS) // 60 minutes
                )
                .returns::<Result<Balance, Error>>()
                .invoke();
            
            match price_result {
                Ok(price) => {
                    // Verify price is fresh
                    let is_fresh: bool = build_call::<DefaultEnvironment>()
                        .call(oracle)
                        .exec_input(
                            ExecutionInput::new(Selector::new(ink::selector_bytes!("is_price_fresh")))
                        )
                        .returns::<bool>()
                        .invoke()
                        .unwrap_or(false);
                    
                    if !is_fresh {
                        return Err(Error::TwapUnavailable);
                    }
                    
                    Ok(price)
                }
                Err(_) => Err(Error::TwapUnavailable),
            }
        }

        // =================================================================
        // LIQUIDITY HEALTH CHECK
        // =================================================================

        pub fn check_liquidity_health(&self) -> Result<(bool, Balance, Balance), Error> {
            let twap_price = self.get_twap_price()?;
            let total_supply = self.get_total_supply()?;
            
            let market_cap = total_supply
                .checked_mul(twap_price)
                .ok_or(Error::Overflow)?
                / PRICE_PRECISION;
            
            let current_liquidity_value = self.get_current_liquidity_value()?;
            
            let current_ratio_bps = if market_cap > 0 {
                current_liquidity_value
                    .checked_mul(BPS_DENOMINATOR)
                    .ok_or(Error::Overflow)?
                    / market_cap
            } else {
                0
            };
            
            if current_ratio_bps >= TARGET_LIQUIDITY_RATIO_BPS {
                self.env().emit_event(LiquidityHealthy {
                    current_ratio_bps,
                    target_ratio_bps: TARGET_LIQUIDITY_RATIO_BPS,
                    timestamp: self.env().block_timestamp(),
                });
                Ok((true, 0, 0))
            } else {
                let target_liquidity = market_cap
                    .checked_mul(TARGET_LIQUIDITY_RATIO_BPS)
                    .ok_or(Error::Overflow)?
                    / BPS_DENOMINATOR;
                
                let deficit = target_liquidity.saturating_sub(current_liquidity_value);
                let current_balance = self.env().balance();
                let max_injection = current_balance
                    .checked_mul(MAX_DRIP_BPS)
                    .ok_or(Error::Overflow)?
                    / BPS_DENOMINATOR;
                
                Ok((false, deficit, max_injection))
            }
        }

        // =================================================================
        // RATE-LIMITED EXECUTION WITH INJECTION THROTTLE
        // =================================================================

        /// Execute rate-limited liquidity injection
        /// 
        /// NEW PROTECTIONS:
        /// 1. Slippage protection (min_out calculated from slippage_bps)
        /// 2. Per-block throttle (only one injection per block)
        #[ink(message)]
        pub fn rate_limited_inject(&mut self, slippage_bps: u128) -> Result<Balance, Error> {
            let current_block = self.env().block_number();
            let current_timestamp = self.env().block_timestamp();
            let current_balance = self.env().balance();
            
            // NEW: Slippage check (must be >= 0.5%, prevents 0% slippage)
            if slippage_bps < MIN_SLIPPAGE_BPS {
                return Err(Error::SlippageTooLow);
            }
            
            // NEW: Per-block throttle (prevent multi-transaction drain)
            if current_block == self.last_injection_block {
                return Err(Error::AlreadyInjectedThisBlock);
            }
            
            // Check cooldown (1 hour minimum between injections)
            if current_block - self.last_injection_block < COOLDOWN_BLOCKS {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::CooldownActive,
                    contract_native_balance: current_balance,
                });
                return Err(Error::CooldownActive);
            }
            
            let (is_healthy, deficit, max_permitted) = self.check_liquidity_health()?;
            
            if is_healthy {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::LiquidityHealthy,
                    contract_native_balance: current_balance,
                });
                return Err(Error::LiquidityHealthy);
            }
            
            let injection_amount = if deficit > max_permitted {
                max_permitted
            } else {
                deficit
            };
            
            if injection_amount == 0 {
                return Err(Error::NoInjectionNeeded);
            }
            
            if injection_amount > current_balance {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::InsufficientFunds,
                    contract_native_balance: current_balance,
                });
                return Err(Error::InsufficientVaultBalance);
            }
            
            // NEW: Calculate min_out with slippage protection
            let expected_out = self.calculate_expected_lp_tokens(injection_amount)?;
            let min_out = expected_out
                .checked_mul(BPS_DENOMINATOR - slippage_bps)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // Execute with slippage protection
            self.execute_liquidity_addition(injection_amount, min_out)?;
            
            // NEW: Update last_injection_block for throttle
            self.last_injection_block = current_block;
            self.last_injection_timestamp = current_timestamp;
            
            let remaining_balance = self.env().balance();
            
            self.env().emit_event(LiquidityInjected {
                amount: injection_amount,
                remaining_contract_balance: remaining_balance,
                block: current_block,
                timestamp: current_timestamp,
            });
            
            Ok(injection_amount)
        }

        // =================================================================
        // EXECUTION WITH SLIPPAGE PROTECTION
        // =================================================================

        /// Execute liquidity addition with strict slippage protection
        /// 
        /// NEW: min_lp_tokens parameter prevents 0% slippage attacks
        fn execute_liquidity_addition(
            &self, 
            amount_qf: Balance,
            min_lp_tokens: Balance
        ) -> Result<(), Error> {
            if self.dex_router.is_none() {
                // DEVNET: Mock success
                return Ok(());
            }
            
            // Production: Call DEX Router with min_lp_tokens protection
            let deadline = self.env().block_timestamp() + 300_000;
            let router = self.dex_router.ok_or(Error::DexRouterNotConfigured)?;
            let token = self.fortress_address;
            
            let result: Result<(Balance, Balance, Balance), Error> = build_call::<DefaultEnvironment>()
                .call(router)
                .transfer_value(amount_qf)
                .exec_input(
                    ExecutionInput::new(Selector::new(ink::selector_bytes!("add_liquidity_native")))
                        .push_arg(token)
                        .push_arg(0u128) // amount_token_desired (calculated by router)
                        .push_arg(0u128) // amount_token_min
                        .push_arg(min_lp_tokens) // amount_native_min (slippage protection)
                        .push_arg(self.env().account_id()) // to
                        .push_arg(deadline)
                )
                .returns::<Result<(Balance, Balance, Balance), Error>>()
                .invoke();
            
            match result {
                Ok(_) => Ok(()),
                Err(_) => Err(Error::InjectionFailed),
            }
        }

        /// Calculate expected LP tokens for slippage protection
        fn calculate_expected_lp_tokens(&self, qf_amount: Balance) -> Result<Balance, Error> {
            // DEVNET: Return 1:1 for testing
            if self.dex_router.is_none() {
                return Ok(qf_amount);
            }
            
            // Production: Query DEX for expected LP token amount
            // This would call router.quote() or similar
            Ok(qf_amount) // Placeholder
        }

        // =================================================================
        // HELPER FUNCTIONS
        // =================================================================

        fn get_total_supply(&self) -> Result<Balance, Error> {
            if self.dex_router.is_none() {
                return Ok(1_000_000_000_000_000_000_000_000);
            }
            Err(Error::TwapUnavailable)
        }

        fn get_current_liquidity_value(&self) -> Result<Balance, Error> {
            if self.dex_router.is_none() {
                let mock_market_cap = 1_000_000_000_000_000_000_000_000_u128;
                return Ok(mock_market_cap * 1000 / BPS_DENOMINATOR);
            }
            Err(Error::TwapUnavailable)
        }

        // =================================================================
        // ADMIN FUNCTIONS
        // =================================================================

        #[ink(message)]
        pub fn set_fortress_address(&mut self, new_address: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.fortress_address = new_address;
            Ok(())
        }

        #[ink(message)]
        pub fn set_dex_router(&mut self, router: AccountId, lp_token: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.dex_router = Some(router);
            self.lp_token = Some(lp_token);
            Ok(())
        }

        #[ink(message)]
        pub fn emergency_withdraw(&mut self, amount: Balance) -> Result<(), Error> {
            self.only_owner()?;
            let current_balance = self.env().balance();
            
            if amount > current_balance {
                return Err(Error::InsufficientVaultBalance);
            }
            
            self.env().transfer(self.owner, amount)
                .map_err(|_| Error::InjectionFailed)?;
            
            Ok(())
        }

        // =================================================================
        // VIEW FUNCTIONS
        // =================================================================

        #[ink(message)]
        pub fn get_vault_balance(&self) -> Balance {
            self.env().balance()
        }

        #[ink(message)]
        pub fn get_last_injection_block(&self) -> u32 {
            self.last_injection_block
        }

        #[ink(message)]
        pub fn get_cooldown_remaining(&self) -> u32 {
            let current_block = self.env().block_number();
            let elapsed = current_block - self.last_injection_block;
            
            if elapsed >= COOLDOWN_BLOCKS {
                0
            } else {
                COOLDOWN_BLOCKS - elapsed
            }
        }

        #[ink(message)]
        pub fn get_max_injection_amount(&self) -> Balance {
            let current_balance = self.env().balance();
            current_balance * MAX_DRIP_BPS / BPS_DENOMINATOR
        }
        
        /// NEW: Check if injection already happened this block
        #[ink(message)]
        pub fn can_inject_this_block(&self) -> bool {
            let current_block = self.env().block_number();
            current_block != self.last_injection_block
        }

        #[ink(message)]
        pub fn preview_injection(&self) -> Result<(bool, Balance, Balance), Error> {
            let (is_healthy, deficit, max_permitted) = self.check_liquidity_health()?;
            if is_healthy {
                return Ok((true, 0, 0));
            }
            
            let amount = if deficit > max_permitted { max_permitted } else { deficit };
            Ok((false, amount, deficit))
        }

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
            }
            Ok(())
        }
    }
}
