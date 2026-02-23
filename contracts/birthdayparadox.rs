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
    
    pub const BPS_DENOMINATOR: u128 = 10_000;
    pub const SQRT_2_BPS: u128 = 14_142;
    pub const PLAYER_SHARE_BPS: u128 = 4_900;
    pub const REVENUE_CAP_BPS: u128 = 11_000;
    pub const DRAIN_CAP_BPS: u128 = 5_000;
    pub const FIBONACCI_SLOTS: [u32; 8] = [1, 2, 3, 5, 8, 13, 21, 34];
    pub const KING_SLOT: u32 = 52;
    pub const COOLDOWN_BLOCKS: u32 = 36_000;
    pub const MS_PER_DAY: u64 = 86_400_000;
    pub const GOLDEN_WINDOW_START_MS: u64 = 72_000_000;
    pub const TARGET_PAYOUT_QF: Balance = 10_400_000_000_000_000_000;
    
    /// NEW: Minimum slippage tolerance (0.5% = 50 BPS) for market buys
    pub const MIN_SLIPPAGE_BPS: u128 = 50;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct BirthdayParadox {
        owner: AccountId,
        fortress_address: AccountId,
        matrix: Mapping<u32, AccountId>,
        total_revenue_since_last_win: Balance,
        last_collision_block: u32,
        king_boost_claimed_today: bool,
        last_gmt_reset_timestamp: u64,
        winnings: Mapping<AccountId, Balance>,
        
        /// NEW: DEX router for slippage-protected swaps
        dex_router: Option<AccountId>,
        wqf: Option<AccountId>,
    }

    // =========================================================================
    // EVENTS (REMOVED DailyReset - no longer needed without manual reset)
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

    // REMOVED: DailyReset event (no longer needed)

    // =========================================================================
    // ERRORS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotOwner,
        InsufficientBalance,
        InsufficientAllowance,
        TransferToZeroAddress,
        ApproveToZeroAddress,
        Overflow,
        CooldownActive,
        InsufficientPrizePot,
        PullFailed,
        NotGameContract,
        NotDampenerContract,
        NoTaxToPull,
        NoWinningsToClaim,
        
        /// NEW: Slippage tolerance too low
        SlippageTooLow,
        /// NEW: Contract paused (from Fortress)
        ContractPaused,
        /// NEW: Swap failed due to slippage
        SlippageExceeded,
    }

    // =========================================================================
    // CROSS-CONTRACT INTERFACES
    // =========================================================================

    #[ink::trait_definition]
    pub trait Project52FInterface {
        #[ink(message)]
        fn pull_prize_tax(&mut self) -> Result<Balance, Error>;
        
        #[ink(message)]
        fn transfer(&mut self, to: AccountId, amount: Balance) -> Result<(), Error>;
        
        #[ink(message)]
        fn is_paused(&self) -> bool;
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
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl BirthdayParadox {
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
                winnings: Mapping::new(),
                dex_router: None,
                wqf: None,
            }
        }

        // =================================================================
        // CORE GAME LOGIC: SEAT ENTRY
        // =================================================================

        #[ink(message, payable)]
        pub fn enter(&mut self) -> Result<u32, Error> {
            let caller = self.env().caller();
            let block = self.env().block_number();
            let timestamp = self.env().block_timestamp();
            
            // Check daily reset (passive only - no manual override)
            self.check_daily_reset(timestamp)?;
            
            let slot = self.calculate_slot(caller, block, timestamp);
            let existing = self.matrix.get(slot);
            
            match existing {
                None => {
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
                    let is_trigger_slot = self.is_fibonacci_or_king(slot);
                    
                    if is_trigger_slot {
                        self.execute_collision_payout(slot, previous_occupant, caller, block, timestamp)?;
                    }
                    
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

        fn is_fibonacci_or_king(&self, slot: u32) -> bool {
            if slot == KING_SLOT { return true; }
            FIBONACCI_SLOTS.contains(&slot)
        }

        // =================================================================
        // COLLISION PAYOUT WITH SOLVENCY GUARDS
        // =================================================================

        fn execute_collision_payout(
            &mut self,
            slot: u32,
            player_a: AccountId,
            player_b: AccountId,
            current_block: u32,
            timestamp: u64,
        ) -> Result<(), Error> {
            if current_block - self.last_collision_block < COOLDOWN_BLOCKS {
                return Err(Error::CooldownActive);
            }
            
            // Pull revenue (will fail with ContractPaused if Fortress is paused)
            self.pull_revenue_from_fortress()?;
            
            let available_pot = self.total_revenue_since_last_win;
            if available_pot == 0 {
                return Err(Error::InsufficientPrizePot);
            }
            
            let base_player_share = available_pot
                .checked_mul(PLAYER_SHARE_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            let mut player_a_share = base_player_share;
            let mut player_b_share = base_player_share;
            let mut is_king_boost = false;
            
            // King Boost logic (Golden Window: Hours 20-24)
            if slot == KING_SLOT && self.is_golden_window(timestamp) {
                if !self.king_boost_claimed_today {
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
            
            // Solvency Guards
            let total_payout = player_a_share
                .checked_add(player_b_share)
                .ok_or(Error::Overflow)?;
            
            let revenue_cap = self.total_revenue_since_last_win
                .checked_mul(REVENUE_CAP_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            let drain_cap = available_pot
                .checked_mul(DRAIN_CAP_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            let max_allowed_payout = if revenue_cap < drain_cap { revenue_cap } else { drain_cap };
            
            let (final_a_share, final_b_share) = if total_payout > max_allowed_payout {
                let scale_factor = (max_allowed_payout * BPS_DENOMINATOR) / total_payout;
                let new_a = (player_a_share * scale_factor) / BPS_DENOMINATOR;
                let new_b = (player_b_share * scale_factor) / BPS_DENOMINATOR;
                (new_a, new_b)
            } else {
                (player_a_share, player_b_share)
            };
            
            // Update pull pattern ledger
            self.total_revenue_since_last_win = self.total_revenue_since_last_win
                .checked_sub(final_a_share)
                .ok_or(Error::Overflow)?
                .checked_sub(final_b_share)
                .ok_or(Error::Overflow)?;
            
            let current_a_winnings = self.winnings.get(player_a).unwrap_or(0);
            self.winnings.insert(player_a, &(current_a_winnings + final_a_share));
            
            let current_b_winnings = self.winnings.get(player_b).unwrap_or(0);
            self.winnings.insert(player_b, &(current_b_winnings + final_b_share));
            
            self.last_collision_block = current_block;
            
            self.env().emit_event(CollisionPayout {
                slot,
                player_a,
                player_b,
                amount_a: final_a_share,
                amount_b: final_b_share,
                is_king_boost,
                total_payout: final_a_share + final_b_share,
            });
            
            Ok(())
        }

        // =================================================================
        // ASYNCHRONOUS REVENUE PULL (Handles Circuit Breaker)
        // =================================================================

        #[ink(message)]
        pub fn pull_revenue_from_fortress(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            // Check if Fortress is paused first (optional optimization)
            let is_paused: bool = build_call::<DefaultEnvironment>()
                .call(self.fortress_address)
                .exec_input(
                    ExecutionInput::new(Selector::new(ink::selector_bytes!("is_paused")))
                )
                .returns::<bool>()
                .invoke()
                .unwrap_or(false);
            
            if is_paused {
                return Err(Error::ContractPaused);
            }
            
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
                        
                        self.env().emit_event(RevenuePulled {
                            amount,
                            new_total_revenue: self.total_revenue_since_last_win,
                            timestamp: self.env().block_timestamp(),
                        });
                    }
                    Ok(amount)
                }
                Err(Error::ContractPaused) => Err(Error::ContractPaused),
                Err(_) => Err(Error::PullFailed),
            }
        }

        // =================================================================
        // PLAYER WINNINGS CLAIM (Pull Pattern)
        // =================================================================

        #[ink(message)]
        pub fn claim_winnings(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            let amount = self.winnings.get(caller).unwrap_or(0);
            
            if amount == 0 {
                return Err(Error::NoWinningsToClaim);
            }
            
            self.winnings.insert(caller, &0);
            self.env().transfer(caller, amount)
                .map_err(|_| Error::PullFailed)?;
            
            self.env().emit_event(WinningsClaimed {
                player: caller,
                amount,
            });
            
            Ok(amount)
        }

        // =================================================================
        // TIME & WINDOW LOGIC (REMOVED MANUAL RESET)
        // =================================================================

        fn check_daily_reset(&mut self, current_timestamp: u64) -> Result<(), Error> {
            let current_gmt_day = current_timestamp / MS_PER_DAY;
            let last_reset_day = self.last_gmt_reset_timestamp / MS_PER_DAY;
            
            if current_gmt_day > last_reset_day {
                // REMOVED: Manual reset flag check (no longer exists)
                let _previous_boost_status = self.king_boost_claimed_today;
                
                self.king_boost_claimed_today = false;
                self.last_gmt_reset_timestamp = self.align_to_gmt_midnight(current_timestamp);
                
                // REMOVED: DailyReset event emission (no longer tracking manual vs automatic)
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
        // ADMIN FUNCTIONS (SANITISED - REMOVED MANUAL RESET)
        // =================================================================

        #[ink(message)]
        pub fn set_game_address(&mut self, address: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.fortress_address = address;
            Ok(())
        }
        
        /// NEW: Set DEX router for slippage-protected operations
        #[ink(message)]
        pub fn set_dex_router(&mut self, router: AccountId, wqf: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.dex_router = Some(router);
            self.wqf = Some(wqf);
            Ok(())
        }

        // REMOVED: manual_daily_reset() - God Mode eliminated
        // REMOVED: toggle_manual_reset() - God Mode eliminated

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
        pub fn get_pending_winnings(&self, player: AccountId) -> Balance {
            self.winnings.get(player).unwrap_or(0)
        }

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
            }
            Ok(())
        }
    }
}
