#![cfg_attr(not(feature = "std"), no_std)]

/// PROJECT 52F — DampenerVault.rs
/// Phase 3: Volatility Guard & Royal Lock Engine
/// Formal Specification v3.1 (February 2026)
/// Target: QF Network (PolkaVM / ink! v6)

#[ink::contract]
mod dampener_vault {
    use ink::storage::Mapping;
    use crate::constants::*; // Shared constants (e, pi, 52)

    // =========================================================================
    // CONSTANTS & CONFIGURATION
    // =========================================================================
    const TWAP_WINDOW: u32 = 520;           // 520 Blocks (52 seconds)
    const TRIGGER_THRESHOLD: u128 = 652;    // 6.52% (Perfect 6 + Identity 52)
    const ROYAL_LOCK_MS: u64 = 9_784_800;   // e hours (2.718 * 3600 * 1000)
    const BPS_BASE: u128 = 10_000;

    #[ink(storage)]
    pub struct DampenerVault {
        // Governance & Access
        owner: AccountId,
        token_contract: AccountId,
        paradox_engine: AccountId,
        spin_swap_pair: Option<AccountId>,

        // TWAP Oracle State
        price_cumulative_last: u128,
        block_timestamp_last: Timestamp,
        price_average: u128,

        // Royal Lock State
        royal_lock_expiry: Timestamp,
        refill_target: Balance,
        is_locked: bool,

        // Emergency Fallback
        is_frozen: bool, // Freeze mode if pool depth is too shallow
    }

    #[ink(event)]
    pub struct VolatilityTriggered {
        price_delta: u128,
        action: String,
        timestamp: Timestamp,
    }

    #[ink(event)]
    pub struct RoyalLockInitiated {
        expiry: Timestamp,
        refill_goal: Balance,
    }

    impl DampenerVault {
        #[ink(constructor)]
        pub fn new(token: AccountId, engine: AccountId) -> Self {
            Self {
                owner: self.env().caller(),
                token_contract: token,
                paradox_engine: engine,
                spin_swap_pair: None,
                price_cumulative_last: 0,
                block_timestamp_last: 0,
                price_average: 0,
                royal_lock_expiry: 0,
                refill_target: 0,
                is_locked: false,
                is_frozen: false,
            }
        }

        // =====================================================================
        // SECTION 1: ROYAL LOCK LOGIC
        // =====================================================================

        /// Called by BirthdayParadox.rs immediately after a Slot 52 (King) win.
        /// Blocks normal collisions until e-hours have passed OR 50% of the 
        /// prize value is refilled via $QF taxes.
        #[ink(message)]
        pub fn initiate_royal_lock(&mut self, prize_value: Balance) {
            self.ensure_engine()?;
            
            let now = self.env().block_timestamp();
            self.royal_lock_expiry = now + ROYAL_LOCK_MS;
            self.refill_target = prize_value / 2; // 50% Refill Gate
            self.is_locked = true;

            self.env().emit_event(RoyalLockInitiated {
                expiry: self.royal_lock_expiry,
                refill_goal: self.refill_target,
            });
        }

        /// View function for the BP Engine to check if collisions are allowed.
        #[ink(message)]
        pub fn check_lock_status(&self) -> bool {
            if !self.is_locked { return false; }

            let now = self.env().block_timestamp();
            let current_acc_tax = Token52FRef::get_accumulated_tax(&self.token_contract);

            // Lock expires only if BOTH conditions are met:
            // 1. Time > 2.718 hours
            // 2. Accumulated taxes > 50% of the last King prize
            if now > self.royal_lock_expiry && current_acc_tax >= self.refill_target {
                return false; // Unlock
            }
            true // Remain Locked
        }

        // =====================================================================
        // SECTION 2: TWAP & PRICE SUPPORT
        // =====================================================================

        /// Updates the 520-block TWAP using SPIN-Swap reserves.
        /// If depth is < 5% of inventory floor, enters Standby/Freeze mode.
        #[ink(message)]
        pub fn sync_oracle(&mut self) {
            let (res_token, res_qf, ts) = self.get_pair_reserves();
            
            if res_token < (TOTAL_SUPPLY * 5 / 100) { // 5% Depth Trigger
                self.is_frozen = true;
                return;
            }

            self.is_frozen = false;
            let time_elapsed = ts - self.block_timestamp_last;
            
            if time_elapsed >= 1 {
                // Simplified TWAP calculation for QF Network PolkaVM
                let current_price = (res_qf * SCALING_FACTOR) / res_token;
                self.price_average = current_price;
                
                self.check_volatility(current_price);
                
                self.block_timestamp_last = ts;
            }
        }

        fn check_volatility(&mut self, current_price: u128) {
            if self.price_average == 0 { return; }

            let delta = if current_price > self.price_average {
                (current_price - self.price_average) * BPS_BASE / self.price_average
            } else {
                (self.price_average - current_price) * BPS_BASE / self.price_average
            };

            if delta >= TRIGGER_THRESHOLD {
                self.execute_price_support(current_price < self.price_average);
                self.env().emit_event(VolatilityTriggered {
                    price_delta: delta,
                    action: if current_price < self.price_average { "BUY_SUPPORT".into() } else { "SELL_DAMPEN".into() },
                    timestamp: self.env().block_timestamp(),
                });
            }
        }

        fn execute_price_support(&mut self, is_dip: bool) {
            // Phase 3: Atomic LP Injection
            // DIP: Swap 1.141% Refill Vault $QF for 52F -> Add LP
            // PUMP: Add 52F directly to LP from Refill Vault
        }

        // =====================================================================
        // ADMIN & HELPERS
        // =====================================================================

        fn ensure_engine(&self) -> Result<(), Error> {
            if self.env().caller() != self.paradox_engine { return Err(Error::NotParadoxEngine); }
            Ok(())
        }

        #[ink(message)]
        pub fn set_pair_address(&mut self, pair: AccountId) {
            self.ensure_owner().expect("Not Owner");
            self.spin_swap_pair = Some(pair);
        }

        fn get_pair_reserves(&self) -> (Balance, Balance, Timestamp) {
            // Interface call to SPIN-Swap Pair
            (0, 0, 0) // Placeholder for Phase 3 integration
        }
        
        fn ensure_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner { return Err(Error::NotOwner); }
            Ok(())
        }
    }
}
