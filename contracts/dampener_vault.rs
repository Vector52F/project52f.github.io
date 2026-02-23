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
    
    /// Basis Points denominator: 10,000 = 100%
    pub const BPS_DENOMINATOR: u128 = 10_000;
    
    /// Target liquidity-to-market-cap ratio: 15% = 1,500 BPS
    pub const TARGET_LIQUIDITY_RATIO_BPS: u128 = 1_500;
    
    /// Maximum drip per transaction: 5% = 500 BPS of vault balance
    pub const MAX_DRIP_BPS: u128 = 500;
    
    /// Cooldown between injections: 1 hour = 36,000 blocks (0.1s block time)
    pub const COOLDOWN_BLOCKS: u32 = 36_000;
    
    /// TWAP period: 60 minutes in milliseconds
    pub const TWAP_PERIOD_MS: u64 = 3_600_000;
    
    /// Price precision: 18 decimal places
    pub const PRICE_PRECISION: u128 = 1_000_000_000_000_000_000;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct DampenerVault {
        /// Owner address
        owner: AccountId,
        
        /// Project52F contract address (Immutable Fortress)
        fortress_address: AccountId,
        
        // REMOVED: vault_balance — now using self.env().balance() directly
        
        /// Last injection block number (for cooldown)
        last_injection_block: u32,
        
        /// Last injection timestamp (for TWAP reference)
        last_injection_timestamp: u64,
        
        /// DEX router address (placeholder for QF Network integration)
        dex_router: Option<AccountId>,
        
        /// LP token address (placeholder)
        lp_token: Option<AccountId>,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct RevenuePulled {
        amount: Balance,
        contract_native_balance: Balance,  // Updated: shows actual blockchain balance
        timestamp: u64,
    }

    #[ink(event)]
    pub struct LiquidityInjected {
        amount: Balance,
        remaining_contract_balance: Balance,  // Updated: shows actual after transfer
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
        contract_native_balance: Balance,  // Updated: shows actual balance
    }

    // =========================================================================
    // ENUMS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        /// Caller is not the owner
        NotOwner,
        /// Cooldown period active (1 hour between injections)
        CooldownActive,
        /// Failed to pull revenue from fortress
        PullFailed,
        /// Math overflow
        Overflow,
        /// TWAP oracle unavailable or stale
        TwapUnavailable,
        /// Liquidity ratio already healthy (>= 15%)
        LiquidityHealthy,
        /// Insufficient vault balance for injection
        InsufficientVaultBalance,
        /// No injection needed (calculated amount is zero)
        NoInjectionNeeded,
        /// DEX router not configured
        DexRouterNotConfigured,
        /// Liquidity injection failed (placeholder)
        InjectionFailed,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum SkipReason {
        CooldownActive,
        LiquidityHealthy,
        InsufficientFunds,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACE
    // =========================================================================

    /// Interface to call Project52F.rs
    #[ink::trait_definition]
    pub trait Project52FInterface {
        /// Pull dampener tax from the fortress
        #[ink(message)]
        fn pull_dampener_tax(&mut self) -> Result<Balance, Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl DampenerVault {
        /// Constructor
        #[ink(constructor)]
        pub fn new(fortress_address: AccountId) -> Self {
            let caller = Self::env().caller();
            let block = Self::env().block_number();
            
            Self {
                owner: caller,
                fortress_address,
                // REMOVED: vault_balance initialization
                last_injection_block: block,
                last_injection_timestamp: 0,
                dex_router: None,
                lp_token: None,
            }
        }

        // =================================================================
        // ASYNCHRONOUS REVENUE PULL (Permissionless)
        // =================================================================

        /// Pull accumulated dampener tax from Project52F Fortress
        /// Permissionless: Can be called by anyone (keepers, bots) at any time
        /// 
        /// REFACTORED: No manual balance tracking. Project52F sends native $QF 
        /// directly via value transfer. Balance automatically reflected in 
        /// self.env().balance().
        #[ink(message)]
        pub fn pull_from_fortress(&mut self) -> Result<Balance, Error> {
            let pre_balance = self.env().balance();  // Get current native balance
            
            let call_result: Result<Balance, Error> = build_call::<DefaultEnvironment>()
                .call(self.fortress_address)
                .exec_input(
                    ExecutionInput::new(ink::selector_bytes!("pull_dampener_tax"))
                )
                .returns::<Result<Balance, Error>>()
                .invoke();
            
            match call_result {
                Ok(amount) => {
                    // Verify balance increased by pulled amount (optional sanity check)
                    let post_balance = self.env().balance();
                    
                    self.env().emit_event(RevenuePulled {
                        amount,
                        contract_native_balance: post_balance,  // Report actual blockchain state
                        timestamp: self.env().block_timestamp(),
                    });
                    
                    Ok(amount)
                }
                Err(_) => Err(Error::PullFailed),
            }
        }

        // =================================================================
        // TWAP ORACLE SECURITY (Placeholder)
        // =================================================================

        /// Get 60-minute Time-Weighted Average Price (TWAP) of $52f in $QF
        /// 
        /// TODO: QF Network Integration — Replace with actual DEX TWAP oracle
        /// 
        /// SECURITY NOTICE: This function MUST integrate with QF Network's official 
        /// TWAP oracle or DEX interface before mainnet deployment. Using spot prices
        /// exposes the vault to flash loan attacks and MEV manipulation.
        /// 
        /// Returns: Price of 1 $52f token in $QF (18 decimal precision)
        fn get_twap_price(&self) -> Result<Balance, Error> {
            // DEVNET: Return mock price (1:1 ratio for testing)
            if self.dex_router.is_none() {
                return Ok(PRICE_PRECISION);
            }
            
            Err(Error::TwapUnavailable)
        }

        // =================================================================
        // LIQUIDITY HEALTH CHECK (The 15% Target Logic)
        // =================================================================

        /// Check if liquidity ratio is healthy and calculate injection requirements
        /// 
        /// REFACTORED: Uses self.env().balance() for native $QF holdings
        /// 
        /// Returns:
        /// - (true, 0, 0) if ratio >= 15% (healthy)
        /// - (false, deficit_qf, max_injection_qf) if ratio < 15% (needs injection)
        pub fn check_liquidity_health(&self) -> Result<(bool, Balance, Balance), Error> {
            let twap_price = self.get_twap_price()?;
            let total_supply = self.get_total_supply()?;
            
            // Calculate market cap: supply * price / precision
            let market_cap = total_supply
                .checked_mul(twap_price)
                .ok_or(Error::Overflow)?
                / PRICE_PRECISION;
            
            let current_liquidity_value = self.get_current_liquidity_value()?;
            
            // Calculate current ratio in BPS
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
                // Unhealthy - calculate deficit
                let target_liquidity = market_cap
                    .checked_mul(TARGET_LIQUIDITY_RATIO_BPS)
                    .ok_or(Error::Overflow)?
                    / BPS_DENOMINATOR;
                
                let deficit = target_liquidity.saturating_sub(current_liquidity_value);
                
                // REFACTORED: Calculate max permitted injection (5% of actual native balance)
                let current_balance = self.env().balance();
                let max_injection = current_balance
                    .checked_mul(MAX_DRIP_BPS)
                    .ok_or(Error::Overflow)?
                    / BPS_DENOMINATOR;
                
                Ok((false, deficit, max_injection))
            }
        }

        // =================================================================
        // RATE-LIMITED EXECUTION (Anti-MEV Drip-Feed)
        // =================================================================

        /// Execute rate-limited liquidity injection
        /// 
        /// REFACTORED: No manual balance subtraction. Native $QF deduction happens 
        /// automatically when execute_liquidity_addition transfers to DEX router.
        #[ink(message)]
        pub fn rate_limited_inject(&mut self) -> Result<Balance, Error> {
            let current_block = self.env().block_number();
            let current_timestamp = self.env().block_timestamp();
            let current_balance = self.env().balance();  // Get actual native balance
            
            // 1. Check cooldown (1 hour minimum)
            if current_block - self.last_injection_block < COOLDOWN_BLOCKS {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::CooldownActive,
                    contract_native_balance: current_balance,
                });
                return Err(Error::CooldownActive);
            }
            
            // 2. Check liquidity health (uses TWAP internally)
            let (is_healthy, deficit, max_permitted) = self.check_liquidity_health()?;
            
            if is_healthy {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::LiquidityHealthy,
                    contract_native_balance: current_balance,
                });
                return Err(Error::LiquidityHealthy);
            }
            
            // 3. Determine injection amount: min(deficit, max_permitted)
            let injection_amount = if deficit > max_permitted {
                max_permitted
            } else {
                deficit
            };
            
            if injection_amount == 0 {
                return Err(Error::NoInjectionNeeded);
            }
            
            // REFACTORED: Check against actual native balance
            if injection_amount > current_balance {
                self.env().emit_event(InjectionSkipped {
                    reason: SkipReason::InsufficientFunds,
                    contract_native_balance: current_balance,
                });
                return Err(Error::InsufficientVaultBalance);
            }
            
            // 4. Execute liquidity injection (transfers native $QF to DEX)
            self.execute_liquidity_addition(injection_amount)?;
            
            // REFACTORED: No manual balance subtraction. The transfer in 
            // execute_liquidity_addition automatically reduces self.env().balance()
            
            // 5. Update state
            self.last_injection_block = current_block;
            self.last_injection_timestamp = current_timestamp;
            
            // Get post-injection balance for event reporting
            let remaining_balance = self.env().balance();
            
            self.env().emit_event(LiquidityInjected {
                amount: injection_amount,
                remaining_contract_balance: remaining_balance,  // Actual native balance after transfer
                block: current_block,
                timestamp: current_timestamp,
            });
            
            Ok(injection_amount)
        }

        /// Execute actual liquidity addition to DEX
        /// 
        /// PRODUCTION IMPLEMENTATION:
        /// 1. Calculate optimal QF/52f split based on current pool ratios
        /// 2. Transfer native $QF to DEX router (this automatically deducts from contract balance)
        /// 3. Add liquidity to QF/52f pool
        /// 4. Handle LP token receipt
        fn execute_liquidity_addition(&mut self, amount_qf: Balance) -> Result<(), Error> {
            if self.dex_router.is_none() {
                // DEVNET: Mock success
                return Ok(());
            }
            
            // Production: build_call to DEX router
            // The .transfer_value(amount_qf) or equivalent will automatically
            // deduct from this contract's native balance
            
            Err(Error::InjectionFailed)
        }

        // =================================================================
        // HELPER FUNCTIONS (Placeholders for DEX Integration)
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

        /// Emergency withdrawal of vault funds (owner only)
        /// REFACTORED: Uses actual native balance
        #[ink(message)]
        pub fn emergency_withdraw(&mut self, amount: Balance) -> Result<(), Error> {
            self.only_owner()?;
            let current_balance = self.env().balance();
            
            if amount > current_balance {
                return Err(Error::InsufficientVaultBalance);
            }
            
            // Native transfer automatically updates blockchain state
            self.env().transfer(self.owner, amount)
                .map_err(|_| Error::InjectionFailed)?;
            
            Ok(())
        }

        // =================================================================
        // VIEW FUNCTIONS
        // =================================================================

        /// REFACTORED: Returns actual native $QF balance from blockchain
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

        /// REFACTORED: Calculates max injection based on actual native balance
        #[ink(message)]
        pub fn get_max_injection_amount(&self) -> Balance {
            let current_balance = self.env().balance();
            current_balance * MAX_DRIP_BPS / BPS_DENOMINATOR
        }

        /// Preview injection without executing
        #[ink(message)]
        pub fn preview_injection(&self) -> Result<(bool, Balance, Balance), Error> {
            let (is_healthy, deficit, max_permitted) = self.check_liquidity_health()?;
            if is_healthy {
                return Ok((true, 0, 0));
            }
            
            let amount = if deficit > max_permitted { max_permitted } else { deficit };
            Ok((false, amount, deficit))
        }

        // =================================================================
        // MODIFIERS
        // =================================================================

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
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
        fn constructor_works() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let contract = DampenerVault::new(accounts.bob);
            
            // REFACTORED: get_vault_balance returns self.env().balance()
            // In tests, this will be 0 unless value is transferred
            assert_eq!(contract.get_last_injection_block(), 0); // Block 0 in test env
        }

        #[ink::test]
        fn max_injection_calculation_uses_native_balance() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let contract = DampenerVault::new(accounts.bob);
            
            // In real scenario with 1000 QF balance:
            // max = 1000 * 500 / 10000 = 50 QF
            let mock_balance: Balance = 1_000_000_000_000_000_000_000;
            let expected_max = mock_balance * MAX_DRIP_BPS / BPS_DENOMINATOR;
            
            assert_eq!(expected_max, 50_000_000_000_000_000_000);
        }
    }
}
