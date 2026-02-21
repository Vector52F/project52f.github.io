#[ink::contract]
mod birthday_paradox {
    use crate::constants::*;
    use ink::storage::Mapping;

    const DAY_MS: u64 = 86_400_000;
    const WARMUP_MS: u64 = 18_720_000; // 5.2 Hours
    const MIN_REIGN_MS: u64 = 3_600_000; // 1 Hour
    const PI_OVER_E: u128 = 11_557; // 1.1557x (15.6% margin)
    const ROOT_2: u128 = 14_142; // 1.414x prize boost

    #[derive(scale::Encode, scale::Decode, Clone, Default)]
    pub struct ArraySlot {
        value: u16,
        sender: AccountId,
        is_active: bool,
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
    }

    impl BirthdayParadox {
        #[ink(message)]
        pub fn process_buy(&mut self, buyer: AccountId, amount: Balance) {
            let now = self.env().block_timestamp();
            self.check_daily_reset(now);

            // Gate 1: Eligibility (One storage read from Token52F)
            if !Token52FRef::is_eligible(&self.token_contract, buyer) { return; }

            // Gate 2: Throne Logic
            self.handle_throne(buyer, amount, now);

            // Gate 3: Fibonacci & Collision
            let suffix = self.get_hash_suffix(buyer);
            if self.is_king_win(buyer, suffix, now) {
                self.payout(buyer, self.king, true);
            } else {
                self.check_normal_collision(buyer, suffix);
            }
        }

        fn handle_throne(&mut self, buyer: AccountId, amount: Balance, now: Timestamp) {
            let current_reign = now - self.crowned_at;
            let dethrone_price = (self.king_buy * PI_OVER_E) / 10_000;

            if (self.king_buy == 0) || (current_reign >= MIN_REIGN_MS && amount > dethrone_price) {
                self.king = buyer;
                self.king_buy = amount;
                self.crowned_at = now;
            }
        }

        fn is_king_win(&self, buyer: AccountId, suffix: u16, now: Timestamp) -> bool {
            let elapsed = now - self.crowned_at;
            suffix == 52 && buyer == self.king && elapsed >= WARMUP_MS && elapsed < (DAY_MS - WARMUP_MS)
        }

        fn check_normal_collision(&mut self, buyer: AccountId, suffix: u16) {
            let mask = self.get_tier_mask();
            if (mask >> suffix) & 1 == 1 {
                if let Some(slot) = self.rolling_array.get(suffix as u8) {
                    if slot.is_active {
                        self.payout(buyer, slot.sender, false);
                    }
                }
            }
            // Record entry to ring buffer (FIFO)
            self.rolling_array.insert(self.write_head, &ArraySlot { value: suffix, sender: buyer, is_active: true });
            self.write_head = (self.write_head + 1) % 52;
        }

        fn get_tier_mask(&self) -> u64 {
            // Logic to return 0x40020212E (Entry) down to 0x40020000E (Mature)
            0x40020212E 
        }

        fn check_daily_reset(&mut self, now: Timestamp) {
            if now - self.day_start >= DAY_MS {
                self.king_buy = 0;
                self.day_start = now;
            }
        }
    }
}
