#![cfg_attr(not(feature = "std"), no_std)]

use ink::contract_ref;
use ink::prelude::vec::Vec;

#[ink::contract]
mod birthday_paradox {
    use crate::constants::*;
    use ink::storage::Mapping;

    const DAY_MS: u64 = 86_400_000;
    const WARMUP_MS: u64 = 18_720_000; // 5.2 Hours
    const MIN_REIGN_MS: u64 = 3_600_000; // 1 Hour
    const PI_OVER_E: u128 = 11_557; // 1.1557x (15.6% margin)
    const ROOT_2: u128 = 14_142; // 1.414x prize boost = 1.4142
    
    // Prize constants
    const NORMAL_PRIZE_BASE: u128 = 10_000 * SCALING_FACTOR; // 10k 52F
    const PRIZE_FLOOR: u128 = 1_000 * SCALING_FACTOR; // 1k 52F minimum
    const PRIZE_CAP_BPS: u128 = 1_000; // 10% of daily inflow max

    #[derive(scale::Encode, scale::Decode, Clone, Default)]
    pub struct ArraySlot {
        value: u16,
        sender: AccountId,
        is_active: bool,
        block_number: BlockNumber,
    }

    #[derive(scale::Encode, scale::Decode, Clone, Default)]
    pub struct DailyStats {
        qf_inflow: Balance,
        prize_outflow: Balance,
        collision_count: u32,
    }

    #[ink(storage)]
    pub struct BirthdayParadox {
        rolling_array: Mapping<u8, ArraySlot>,
        write_head: u8,
        day_start: Timestamp,
        king: AccountId,
        king_buy: Balance,
        crowned_at: Timestamp,
        last_king_win: Timestamp,
        token_contract: AccountId,
        dampener_vault: AccountId,
        qf_accumulated: Balance, // Prize pool in QF
        daily_history: Mapping<u32, DailyStats>, // 30-day rolling
        current_day: u32,
        total_qf_inflow_30d: Balance,
    }

    #[ink(event)]
    pub struct Collision {
        #[ink(topic)] winner: AccountId,
        #[ink(topic)] loser: Option<AccountId>,
        prize_amount: Balance,
        is_king_win: bool,
    }

    #[ink(event)]
    pub struct NewKing {
        #[ink(topic)] king: AccountId,
        buy_amount: Balance,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotEligible,
        InvalidHash,
        PrizePoolEmpty,
        TransferFailed,
    }

    impl BirthdayParadox {
        #[ink(constructor)]
        pub fn new(token: AccountId, dampener: AccountId) -> Self {
            Self {
                rolling_array: Mapping::default(),
                write_head: 0,
                day_start: Self::env().block_timestamp(),
                king: Self::env().caller(),
                king_buy: 0,
                crowned_at: Self::env().block_timestamp(),
                last_king_win: 0,
                token_contract: token,
                dampener_vault: dampener,
                qf_accumulated: 0,
                daily_history: Mapping::default(),
                current_day: 0,
                total_qf_inflow_30d: 0,
            }
        }

        #[ink(message, payable)]
        pub fn process_buy(&mut self, buyer: AccountId, amount: Balance) -> Result<(), Error> {
            let now = self.env().block_timestamp();
            let qf_sent = self.env().transferred_value();
            
            self.check_daily_reset(now);
            self.update_inflow(qf_sent);

            // Gate 1: Eligibility
            if !self.is_eligible(buyer) {
                return Err(Error::NotEligible);
            }

            // Gate 2: Throne Logic
            let became_king = self.handle_throne(buyer, amount, now);
            if became_king {
                self.env().emit_event(NewKing { king: buyer, buy_amount: amount });
            }

            // Gate 3: Generate hash suffix
            let suffix = self.get_hash_suffix(buyer, now);
            
            // Check king win first (self-win)
            if self.is_king_win(buyer, suffix, now) {
                let prize = self.calculate_prize(true);
                self.payout_king_jackpot(buyer, prize)?;
            } else {
                // Check collision with existing slots
                self.check_collision(buyer, suffix, now)?;
            }

            // Record in rolling array
            self.record_entry(buyer, suffix);
            
            // Trigger dampener
            self.notify_dampener(amount);
            
            Ok(())
        }

        fn handle_throne(&mut self, buyer: AccountId, amount: Balance, now: Timestamp) -> bool {
            let current_reign = now - self.crowned_at;
            let dethrone_price = (self.king_buy * PI_OVER_E) / 10_000;

            if self.king_buy == 0 || (current_reign >= MIN_REIGN_MS && amount > dethrone_price) {
                self.king = buyer;
                self.king_buy = amount;
                self.crowned_at = now;
                return true;
            }
            false
        }

        fn is_king_win(&self, buyer: AccountId, suffix: u16, now: Timestamp) -> bool {
            let elapsed = now - self.crowned_at;
            suffix == 52 
                && buyer == self.king 
                && elapsed >= WARMUP_MS 
                && elapsed < (DAY_MS - WARMUP_MS)
        }

        fn check_collision(&mut self, buyer: AccountId, suffix: u16, now: Timestamp) -> Result<(), Error> {
            let mask = self.get_tier_mask();
            
            // Only check collisions for tiered suffixes
            if (mask >> suffix) & 1 == 0 {
                return Ok(());
            }

            // Iterate all 52 slots to find matching suffix value
            for i in 0..52u8 {
                if let Some(slot) = self.rolling_array.get(i) {
                    if slot.is_active && slot.value == suffix {
                        // Collision found!
                        let prize = self.calculate_prize(false);
                        
                        // Pay both parties normal prize
                        self.payout_normal(buyer, slot.sender, prize)?;
                        
                        // Deactivate old slot to prevent triple+ collisions
                        self.rolling_array.insert(i, &ArraySlot { is_active: false, ..slot });
                        
                        return Ok(());
                    }
                }
            }
            
            Ok(())
        }

        fn payout_king_jackpot(&mut self, king: AccountId, prize: Balance) -> Result<(), Error> {
            // Pull from Token52F prize pool
            Token52FRef::transfer_from_prize_pool(&self.token_contract, king, prize)
                .map_err(|_| Error::TransferFailed)?;
            
            // Refill from QF
            self.refill_base_pool(prize);
            
            self.last_king_win = self.env().block_timestamp();
            
            self.env().emit_event(Collision {
                winner: king,
                loser: None,
                prize_amount: prize,
                is_king_win: true,
            });
            
            Ok(())
        }

        fn payout_normal(&mut self, winner: AccountId, other: AccountId, prize: Balance) -> Result<(), Error> {
            // Pay both from prize pool
            Token52FRef::transfer_from_prize_pool(&self.token_contract, winner, prize)
                .map_err(|_| Error::TransferFailed)?;
            
            Token52FRef::transfer_from_prize_pool(&self.token_contract, other, prize)
                .map_err(|_| Error::TransferFailed)?;
            
            // Refill double amount
            self.refill_base_pool(prize * 2);
            
            self.env().emit_event(Collision {
                winner,
                loser: Some(other),
                prize_amount: prize,
                is_king_win: false,
            });
            
            Ok(())
        }

        fn calculate_prize(&self, is_king: bool) -> Balance {
            let base = if is_king {
                (NORMAL_PRIZE_BASE * ROOT_2) / 10_000 // 14,140 52F
            } else {
                NORMAL_PRIZE_BASE // 10,000 52F
            };
            
            // Calculate max sustainable from 30-day average
            let daily_avg = self.total_qf_inflow_30d / 30;
            let max_daily_prize = (daily_avg * PRIZE_CAP_BPS) / BPS;
            let hourly_max = max_daily_prize / 24;
            
            // Convert QF to 52F (simplified: assume 1:1 for calculation)
            let max_prize_52f = hourly_max; // In reality: query DEX
            
            // Apply bounds
            let capped = base.min(max_prize_52f);
            capped.max(PRIZE_FLOOR)
        }

        fn refill_base_pool(&mut self, amount_52f: Balance) {
            // Swap QF to 52F and send to Token52F contract
            // Simplified: burn QF, mint/request 52F from reserve
            let qf_needed = amount_52f; // Assume 1:1 for simplicity
            
            if self.qf_accumulated >= qf_needed {
                self.qf_accumulated -= qf_needed;
                // In real implementation: swap via SPIN-Swap
                // Then send 52F to Token52F to replenish prize pool
            }
        }

        fn get_hash_suffix(&self, buyer: AccountId, timestamp: Timestamp) -> u16 {
            // Hash = keccak256(buyer + timestamp + block_number)
            let block = self.env().block_number();
            let input = (buyer, timestamp, block).encode();
            let hash = self.env().hash_bytes::<ink::env::hash::Keccak256>(&input);
            
            // Take last 2 bytes, mod 1000 for 0-999 range
            let last_two = u16::from_le_bytes([hash[30], hash[31]]);
            last_two % 1000
        }

        fn get_tier_mask(&self) -> u64 {
            // Fibonacci-ish tiers: 1, 2, 3, 5, 8, 13, 21, 34, 55, 89
            // Represented as bits: positions in 0-999 space
            // Simplified: use 0x40020212E pattern for specific tiers
            0x40020212E 
        }

        fn record_entry(&mut self, buyer: AccountId, suffix: u16) {
            let slot = ArraySlot {
                value: suffix,
                sender: buyer,
                is_active: true,
                block_number: self.env().block_number(),
            };
            
            self.rolling_array.insert(self.write_head, &slot);
            self.write_head = (self.write_head + 1) % 52;
        }

        fn check_daily_reset(&mut self, now: Timestamp) {
            if now - self.day_start >= DAY_MS {
                // Archive stats
                let stats = DailyStats {
                    qf_inflow: self.qf_accumulated,
                    prize_outflow: 0, // Track separately
                    collision_count: 0,
                };
                self.daily_history.insert(self.current_day, &stats);
                
                // Update 30-day rolling total
                self.total_qf_inflow_30d = 0;
                for i in 0..30 {
                    if let Some(day) = self.daily_history.get(self.current_day - i) {
                        self.total_qf_inflow_30d += day.qf_inflow;
                    }
                }
                
                self.current_day += 1;
                self.day_start = now;
                self.king_buy = 0; // Reset king daily
            }
        }

        fn update_inflow(&mut self, qf_amount: Balance) {
            self.qf_accumulated += qf_amount;
        }

        fn notify_dampener(&self, trade_value: Balance) {
            // Call DampenerVault.process_trade
            // Pass trade info for volume tracking
            DampenerVaultRef::process_trade(&self.dampener_vault, trade_value, 0, 0);
        }

        fn is_eligible(&self, account: AccountId) -> bool {
            Token52FRef::is_eligible(&self.token_contract, account)
        }

        // Admin: receive QF from Token52F
        #[ink(message, payable)]
        pub fn receive_qf_tax(&mut self) {
            // QF transferred to this contract accumulates
        }
    }
}
