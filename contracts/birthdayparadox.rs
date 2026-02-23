#![cfg_attr(not(feature = "std"), no_std, no_main)]

#[ink::contract]
mod birthday_paradox {
    use ink::prelude::vec::Vec;
    use ink::storage::Mapping;
    use ink::primitives::AccountId;
    use ink::env::call::{build_call, ExecutionInput, Selector};

    // =========================================================================
    // CONSTANTS — MATHEMATICALLY LOCKED
    // =========================================================================
    
    /// Basis Points denominator
    pub const BPS_DENOMINATOR: u128 = 10_000;
    
    /// √2 for King Boost: 1.4142 = 14142 BPS
    pub const SQRT_2_BPS: u128 = 14_142;
    
    /// Standard split: 49% = 4900 BPS
    pub const PLAYER_SHARE_BPS: u128 = 4_900;
    
    /// Dynamic Cap 1: 110% = 11,000 BPS
    pub const REVENUE_CAP_BPS: u128 = 11_000;
    
    /// Dynamic Cap 2: 50% = 5,000 BPS
    pub const DRAIN_CAP_BPS: u128 = 5_000;
    
    /// Fibonacci collision slots: 1, 2, 3, 5, 8, 13, 21, 34
    pub const FIBONACCI_SLOTS: [u32; 8] = [1, 2, 3, 5, 8, 13, 21, 34];
    
    /// The King slot
    pub const KING_SLOT: u32 = 52;
    
    /// 1-hour cooldown in blocks (0.1s block time = 36,000 blocks)
    pub const COOLDOWN_BLOCKS: u32 = 36_000;
    
    /// 24-hour cycle in milliseconds
    pub const MS_PER_DAY: u64 = 86_400_000;
    
    /// Golden Window start: Hour 20 (20 * 3600 * 1000 ms)
    pub const GOLDEN_WINDOW_START_MS: u64 = 72_000_000;
    
    /// Target payout baseline: 10.4M $52f equivalent (represented in $QF)
    pub const TARGET_PAYOUT_QF: Balance = 10_400_000_000_000_000_000;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct BirthdayParadox {
        /// Owner address
        owner: AccountId,
        
        /// Project52F contract address (Immutable Fortress)
        fortress_address: AccountId,
        
        /// The Sparse Matrix: slot_index => occupant_address
        matrix: Mapping<u32, AccountId>,
        
        /// Total revenue accumulated since last win (in $QF)
        total_revenue_since_last_win: Balance,
        
        /// Last collision payout timestamp (block number for cooldown)
        last_collision_block: u32,
        
        /// King boost claimed flag (resets 00:00 GMT)
        king_boost_claimed_today: bool,
        
        /// Last GMT midnight timestamp (for daily resets)
        last_gmt_reset_timestamp: u64,
        
        /// Current Prize Pot balance held by this contract
        prize_pot_balance: Balance,
        
        // === NEW: Pull Pattern for Player Winnings ===
        /// Player winnings ledger: address => claimable amount
        winnings: Mapping<AccountId, Balance>,
    }

    // =========================================================================
    // EVENTS
    // =========================================================================

    #[ink(event)]
    pub struct SeatClaimed {
        #[ink(topic)]
        slot: u32,
        #[ink(topic)]
        new_occupant: AccountId,
        #[ink(topic)]
        previous_occupant: Option<AccountId>,
        is_collision: bool,
    }

    #[ink(event)]
    pub struct CollisionPayout {
        #[ink(topic)]
        slot: u32,
        #[ink(topic)]
        player_a: AccountId,
        #[ink(topic)]
        player_b: AccountId,
        amount_a: Balance,
        amount_b: Balance,
        is_king_boost: bool,
        total_payout: Balance,
    }

    #[ink(event)]
    pub struct WinningsClaimed {
        #[ink(topic)]
        player: AccountId,
        amount: Balance,
    }

    #[ink(event)]
    pub struct RevenuePulled {
        amount: Balance,
        new_total_revenue: Balance,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct KingBoostActivated {
        #[ink(topic)]
        king: AccountId,
        boost_multiplier: u128,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct DailyReset {
        timestamp: u64,
        previous_king_boost_status: bool,
    }

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        /// Caller is not the owner
        NotOwner,
        /// Cooldown period active (1 hour between payouts)
        CooldownActive,
        /// Insufficient prize pot balance
        InsufficientPrizePot,
        /// Failed to pull revenue from fortress
        PullFailed,
        /// Math overflow
        Overflow,
        /// Invalid fortress address
        InvalidFortressAddress,
        /// Payout calculation error
        PayoutCalculationError,
        /// Transfer failed
        TransferFailed,
        /// No winnings to claim
        NoWinningsToClaim,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACE
    // =========================================================================

    /// Interface to call Project52F.rs
    #[ink::trait_definition]
    pub trait Project52FInterface {
        /// Pull prize tax from the fortress
        #[ink(message)]
        fn pull_prize_tax(&mut self) -> Result<Balance, Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl BirthdayParadox {
        /// Constructor
        #[ink(constructor)]
        pub fn new(fortress_address: AccountId) -> Self {
            let caller = Self::env().caller();
            let now = Self::env().block_timestamp();
            
            Self {
                owner: caller,
                fortress_address,
                matrix: Mapping::new(),
                total_revenue_since_last_win: 0,
                last_collision_block: 0,
                king_boost_claimed_today: false,
                last_gmt_reset_timestamp: Self::align_to_gmt_midnight(now),
                prize_pot_balance: 0,
                winnings: Mapping::new(), // NEW
            }
        }

        // =================================================================
        // CORE GAME LOGIC: SEAT ENTRY & COLLISION
        // =================================================================

        /// Main entry point: Claim a seat in the matrix
        /// Calculates slot from TX hash MOD 1000, handles collisions
        /// NOTE: Uses current prize_pot_balance, does NOT pull revenue synchronously
        #[ink(message, payable)]
        pub fn enter(&mut self) -> Result<u32, Error> {
            let caller = self.env().caller();
            let block = self.env().block_number();
            let timestamp = self.env().block_timestamp();
            
            // Check and handle daily reset (passive)
            self.check_daily_reset(timestamp)?;
            
            // Calculate slot: TX hash MOD 1000
            let slot = self.calculate_slot(caller, block, timestamp);
            
            // Check if seat is occupied
            let existing = self.matrix.get(slot);
            
            match existing {
                None => {
                    // Empty seat — simple occupation
                    self.matrix.insert(slot, &caller);
                    
                    self.env().emit_event(SeatClaimed {
                        slot,
                        new_occupant: caller,
                        previous_occupant: None,
                        is_collision: false,
                    });
                    
                    Ok(slot)
                }
                Some(previous_occupant) => {
                    // Seat occupied — check for collision payout
                    let is_trigger_slot = self.is_fibonacci_or_king(slot);
                    
                    if is_trigger_slot {
                        // This is a collision mine — execute payout logic
                        self.execute_collision_payout(slot, previous_occupant, caller, block, timestamp)?;
                    }
                    
                    // Overwrite seat (steal)
                    self.matrix.insert(slot, &caller);
                    
                    self.env().emit_event(SeatClaimed {
                        slot,
                        new_occupant: caller,
                        previous_occupant: Some(previous_occupant),
                        is_collision: is_trigger_slot,
                    });
                    
                    Ok(slot)
                }
            }
        }

        /// Calculate slot index: TX hash -> decimal -> MOD 1000
        fn calculate_slot(&self, caller: AccountId, block: u32, timestamp: u64) -> u32 {
            let mut hash_input = Vec::new();
            hash_input.extend_from_slice(&caller.as_ref());
            hash_input.extend_from_slice(&block.to_le_bytes());
            hash_input.extend_from_slice(&timestamp.to_le_bytes());
            
            let mut hash: u64 = 5381;
            for byte in &hash_input {
                hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*byte as u64);
            }
            
            (hash % 1000) as u32
        }

        /// Check if slot is Fibonacci number or King slot (52)
        fn is_fibonacci_or_king(&self, slot: u32) -> bool {
            if slot == KING_SLOT {
                return true;
            }
            FIBONACCI_SLOTS.contains(&slot)
        }

        // =================================================================
        // COLLISION PAYOUT LOGIC (PULL PATTERN)
        // =================================================================

        fn execute_collision_payout(
            &mut self,
            slot: u32,
            player_a: AccountId, // Existing occupant
            player_b: AccountId, // New buyer
            current_block: u32,
            timestamp: u64,
        ) -> Result<(), Error> {
            // 1. Check cooldown (1 hour)
            if current_block - self.last_collision_block < COOLDOWN_BLOCKS {
                return Err(Error::CooldownActive);
            }
            
            // 2. Use CURRENT prize pot balance (revenue pulled asynchronously)
            // NOTE: Removed synchronous pull_revenue_from_fortress() call
            let available_pot = self.prize_pot_balance;
            if available_pot == 0 {
                return Err(Error::InsufficientPrizePot);
            }
            
            // 3. Calculate base payout (49% + 49% = 98% of available pot)
            let base_player_share = available_pot
                .checked_mul(PLAYER_SHARE_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            let mut player_a_share = base_player_share;
            let mut player_b_share = base_player_share;
            let mut is_king_boost = false;
            
            // 4. Check Golden Window and King Boost (Slot 52 only)
            if slot == KING_SLOT && self.is_golden_window(timestamp) {
                if !self.king_boost_claimed_today {
                    // Apply √2 boost to Player A (the King)
                    player_a_share = base_player_share
                        .checked_mul(SQRT_2_BPS)
                        .ok_or(Error::Overflow)?
                        / BPS_DENOMINATOR;
                    
                    self.king_boost_claimed_today = true;
                    is_king_boost = true;
                    
                    self.env().emit_event(KingBoostActivated {
                        king: player_a,
                        boost_multiplier: SQRT_2_BPS,
                        timestamp,
                    });
                }
            }
            
            // 5. Apply Solvency Guards (Dynamic Caps)
            let total_payout = player_a_share
                .checked_add(player_b_share)
                .ok_or(Error::Overflow)?;
            
            // Cap 1: 110% of revenue since last win
            let revenue_cap = self.total_revenue_since_last_win
                .checked_mul(REVENUE_CAP_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // Cap 2: 50% of total prize pot (drain guard)
            let drain_cap = available_pot
                .checked_mul(DRAIN_CAP_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // Use the lower of the two caps
            let max_allowed_payout = if revenue_cap < drain_cap { revenue_cap } else { drain_cap };
            
            let (final_a_share, final_b_share, final_total) = if total_payout > max_allowed_payout {
                // Scale down proportionally if over cap
                let scale_factor = (max_allowed_payout * BPS_DENOMINATOR) / total_payout;
                let new_a = (player_a_share * scale_factor) / BPS_DENOMINATOR;
                let new_b = (player_b_share * scale_factor) / BPS_DENOMINATOR;
                (new_a, new_b, new_a + new_b)
            } else {
                (player_a_share, player_b_share, total_payout)
            };
            
            // 6. UPDATE PULL PATTERN: Record winnings instead of direct transfer
            // Deduct from prize pot immediately
            self.prize_pot_balance = self.prize_pot_balance
                .checked_sub(final_a_share)
                .ok_or(Error::Overflow)?
                .checked_sub(final_b_share)
                .ok_or(Error::Overflow)?;
            
            // Record in winnings ledger (players claim separately)
            let current_a_winnings = self.winnings.get(player_a).unwrap_or(0);
            self.winnings.insert(player_a, &(current_a_winnings + final_a_share));
            
            let current_b_winnings = self.winnings.get(player_b).unwrap_or(0);
            self.winnings.insert(player_b, &(current_b_winnings + final_b_share));
            
            // 7. Update state
            self.last_collision_block = current_block;
            self.total_revenue_since_last_win = 0; // Reset revenue counter
            
            // 8. Emit event
            self.env().emit_event(CollisionPayout {
                slot,
                player_a,
                player_b,
                amount_a: final_a_share,
                amount_b: final_b_share,
                is_king_boost,
                total_payout: final_total,
            });
            
            Ok(())
        }

        // =================================================================
        // ASYNCHRONOUS REVENUE PULL (Permissionless)
        // =================================================================

        /// Pull revenue from Project52F Fortress
        /// Permissionless: Can be called by anyone (keepers, bots, users) at any time
        /// Does NOT affect enter() transactions if it fails
        #[ink(message)]
        pub fn pull_revenue_from_fortress(&mut self) -> Result<Balance, Error> {
            let call_result: Result<Balance, Error> = build_call::<DefaultEnvironment>()
                .call(self.fortress_address)
                .exec_input(
                    ExecutionInput::new(Selector::new(ink::selector_bytes!("pull_prize_tax")))
                )
                .returns::<Result<Balance, Error>>()
                .invoke();
            
            match call_result {
                Ok(amount) => {
                    if amount > 0 {
                        self.total_revenue_since_last_win = self.total_revenue_since_last_win
                            .checked_add(amount)
                            .ok_or(Error::Overflow)?;
                        
                        self.prize_pot_balance = self.prize_pot_balance
                            .checked_add(amount)
                            .ok_or(Error::Overflow)?;
                        
                        self.env().emit_event(RevenuePulled {
                            amount,
                            new_total_revenue: self.total_revenue_since_last_win,
                            timestamp: self.env().block_timestamp(),
                        });
                    }
                    Ok(amount)
                }
                Err(_) => Err(Error::PullFailed),
            }
        }

        // =================================================================
        // PLAYER WINNINGS CLAIM (Pull Pattern)
        // =================================================================

        /// Players claim their accumulated winnings
        /// Pull pattern: Player initiates withdrawal to avoid transfer failures
        #[ink(message)]
        pub fn claim_winnings(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            let amount = self.winnings.get(caller).unwrap_or(0);
            
            if amount == 0 {
                return Err(Error::NoWinningsToClaim);
            }
            
            // Reset before transfer (reentrancy protection)
            self.winnings.insert(caller, &0);
            
            // Execute transfer
            self.env().transfer(caller, amount)
                .map_err(|_| Error::TransferFailed)?;
            
            self.env().emit_event(WinningsClaimed {
                player: caller,
                amount,
            });
            
            Ok(amount)
        }

        // =================================================================
        // TIME & WINDOW LOGIC
        // =================================================================

        fn check_daily_reset(&mut self, current_timestamp: u64) -> Result<(), Error> {
            let current_gmt_day = current_timestamp / MS_PER_DAY;
            let last_reset_day = self.last_gmt_reset_timestamp / MS_PER_DAY;
            
            if current_gmt_day > last_reset_day {
                let previous_boost_status = self.king_boost_claimed_today;
                
                self.king_boost_claimed_today = false;
                self.last_gmt_reset_timestamp = self.align_to_gmt_midnight(current_timestamp);
                
                self.env().emit_event(DailyReset {
                    timestamp: current_timestamp,
                    previous_king_boost_status: previous_boost_status,
                });
            }
            
            Ok(())
        }

        fn is_golden_window(&self, timestamp: u64) -> bool {
            let ms_today = timestamp % MS_PER_DAY;
            ms_today >= GOLDEN_WINDOW_START_MS
        }

        fn align_to_gmt_midnight(&self, timestamp: u64) -> u64 {
            (timestamp / MS_PER_DAY) * MS_PER_DAY
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
        pub fn manual_daily_reset(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            let now = self.env().block_timestamp();
            self.king_boost_claimed_today = false;
            self.last_gmt_reset_timestamp = self.align_to_gmt_midnight(now);
            
            self.env().emit_event(DailyReset {
                timestamp: now,
                previous_king_boost_status: true,
            });
            
            Ok(())
        }

        /// Emergency withdrawal of excess funds (owner only)
        #[ink(message)]
        pub fn emergency_withdraw(&mut self, amount: Balance) -> Result<(), Error> {
            self.only_owner()?;
            if amount > self.prize_pot_balance {
                return Err(Error::InsufficientPrizePot);
            }
            
            self.prize_pot_balance -= amount;
            self.env().transfer(self.owner, amount)
                .map_err(|_| Error::TransferFailed)?;
            
            Ok(())
        }

        // =================================================================
        // VIEW FUNCTIONS
        // =================================================================

        #[ink(message)]
        pub fn get_occupant(&self, slot: u32) -> Option<AccountId> {
            self.matrix.get(slot)
        }

        #[ink(message)]
        pub fn get_total_revenue(&self) -> Balance {
            self.total_revenue_since_last_win
        }

        #[ink(message)]
        pub fn get_prize_pot_balance(&self) -> Balance {
            self.prize_pot_balance
        }

        #[ink(message)]
        pub fn get_last_collision_block(&self) -> u32 {
            self.last_collision_block
        }

        #[ink(message)]
        pub fn is_king_boost_claimed(&self) -> bool {
            self.king_boost_claimed_today
        }

        #[ink(message)]
        pub fn is_slot_trigger(&self, slot: u32) -> bool {
            self.is_fibonacci_or_king(slot)
        }

        #[ink(message)]
        pub fn calculate_max_payout(&self) -> Balance {
            let revenue_cap = self.total_revenue_since_last_win * REVENUE_CAP_BPS / BPS_DENOMINATOR;
            let drain_cap = self.prize_pot_balance * DRAIN_CAP_BPS / BPS_DENOMINATOR;
            
            if revenue_cap < drain_cap { revenue_cap } else { drain_cap }
        }

        #[ink(message)]
        pub fn get_cooldown_remaining(&self) -> u32 {
            let current_block = self.env().block_number();
            let elapsed = current_block - self.last_collision_block;
            
            if elapsed >= COOLDOWN_BLOCKS {
                0
            } else {
                COOLDOWN_BLOCKS - elapsed
            }
        }

        // === NEW: View pending winnings ===
        #[ink(message)]
        pub fn get_pending_winnings(&self, player: AccountId) -> Balance {
            self.winnings.get(player).unwrap_or(0)
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

        fn set_timestamp(timestamp: u64) {
            test::set_block_timestamp::<DefaultEnvironment>(timestamp);
        }

        #[ink::test]
        fn constructor_works() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let contract = BirthdayParadox::new(accounts.bob);
            
            assert_eq!(contract.get_total_revenue(), 0);
            assert_eq!(contract.get_prize_pot_balance(), 0);
            assert_eq!(contract.is_king_boost_claimed(), false);
        }

        #[ink::test]
        fn winnings_pull_pattern() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let mut contract = BirthdayParadox::new(accounts.bob);
            
            // Simulate winnings recorded (would happen in collision)
            // For test, manually manipulate storage or add test helper
            // Check claim fails when no winnings
            let result = contract.claim_winnings();
            assert_eq!(result, Err(Error::NoWinningsToClaim));
        }

        #[ink::test]
        fn fibonacci_detection() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let contract = BirthdayParadox::new(accounts.bob);
            
            assert!(contract.is_slot_trigger(1));
            assert!(contract.is_slot_trigger(52));
            assert!(!contract.is_slot_trigger(4));
        }
    }
}
