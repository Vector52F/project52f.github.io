#![cfg_attr(not(feature = "std"), no_std, no_main)]

#[ink::contract]
mod project52f {
    use ink::prelude::string::String;
    use ink::prelude::vec::Vec;
    use ink::storage::Mapping;
    use ink::env::call::{build_call, ExecutionInput, Selector};
    use ink::env::DefaultEnvironment;
    use ink::primitives::AccountId;
    use ink::env::timestamp;

    // =========================================================================
    // CONSTANTS — MATHEMATICALLY LOCKED
    // =========================================================================
    
    pub const BPS_DENOMINATOR: u128 = 10_000;
    pub const E_BUY_TAX_BPS: u128 = 272;
    pub const BUY_TAX_TEAM_BPS: u128 = 75;
    pub const PI_SELL_TAX_BPS: u128 = 314;
    pub const SELL_TAX_TEAM_BPS: u128 = 75;
    pub const SELL_TAX_DAMPENER_BPS: u128 = 100;
    pub const TEAM_PUSH_INTERVAL: u32 = 520_000;
    pub const THRONE_SHIELD_BLOCKS: u32 = 36_000;
    pub const DETHRONE_MULTIPLIER_BPS: u128 = 11_557;
    pub const MS_PER_DAY: u64 = 86_400_000;
    pub const TIMELOCK_DURATION: u64 = 86_400_000;
    
    /// NEW: Minimum transaction threshold to prevent dusting attacks
    /// Ensures each tax bucket receives at least 1 unit of QF
    /// At 18 decimals, this is 0.000000000000000001 QF minimum effective tax
    pub const MIN_TRANSACTION_THRESHOLD: Balance = 1_000_000_000_000_000_000; // 1 QF minimum for meaningful taxes

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct Project52F {
        total_supply: Balance,
        balances: Mapping<AccountId, Balance>,
        allowances: Mapping<(AccountId, AccountId), Balance>,
        name: String,
        symbol: String,
        decimals: u8,
        owner: AccountId,
        team_tax_accumulated: Balance,
        prize_pot_accumulated: Balance,
        dampener_tax_accumulated: Balance,
        last_team_push_block: u32,
        king: Option<AccountId>,
        king_buy_amount: Balance,
        king_crowned_block: u32,
        king_crowned_timestamp: u64,
        last_throne_reset_timestamp: u64,
        game_address: Option<AccountId>,
        dampener_address: Option<AccountId>,
        victory_lap_address: Option<AccountId>,
        pending_game_address: Option<AccountId>,
        game_address_timelock: u64,
        dex_router: Option<AccountId>,
        wqf_address: Option<AccountId>,
        
        /// NEW: Circuit breaker pause state
        paused: bool,
    }

    // =========================================================================
    // EVENTS (unchanged except removed ManualResetToggled)
    // =========================================================================

    #[ink(event)]
    pub struct Transfer {
        #[ink(topic)]
        from: Option<AccountId>,
        #[ink(topic)]
        to: Option<AccountId>,
        value: Balance,
    }

    #[ink(event)]
    pub struct Approval {
        #[ink(topic)]
        owner: AccountId,
        #[ink(topic)]
        spender: AccountId,
        value: Balance,
    }

    #[ink(event)]
    pub struct ThroneClaimed {
        #[ink(topic)]
        new_king: AccountId,
        #[ink(topic)]
        previous_king: Option<AccountId>,
        buy_amount: Balance,
        block: u32,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct ThroneReset {
        #[ink(topic)]
        previous_king: Option<AccountId>,
        timestamp: u64,
        is_manual: bool,
    }

    #[ink(event)]
    pub struct TaxCollected {
        #[ink(topic)]
        from: AccountId,
        amount: Balance,
        tax_type: TaxType,
        team_portion: Balance,
        prize_portion: Balance,
        dampener_portion: Balance,
    }

    #[ink(event)]
    pub struct TeamTaxPushed {
        amount: Balance,
        block: u32,
    }

    #[ink(event)]
    pub struct GameAddressProposed {
        #[ink(topic)]
        proposed_address: AccountId,
        executable_at: u64,
    }

    #[ink(event)]
    pub struct GameAddressSet {
        #[ink(topic)]
        new_address: AccountId,
        previous_address: Option<AccountId>,
    }

    #[ink(event)]
    pub struct PrizeTaxPulled {
        #[ink(topic)]
        game_address: AccountId,
        amount: Balance,
        timestamp: u64,
    }

    #[ink(event)]
    pub struct DampenerTaxPulled {
        #[ink(topic)]
        dampener_address: AccountId,
        amount: Balance,
        timestamp: u64,
    }
    
    /// NEW: Circuit breaker event
    #[ink(event)]
    pub struct CircuitBreakerToggled {
        paused: bool,
        timestamp: u64,
    }

    // =========================================================================
    // ENUMS
    // =========================================================================

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum TaxType {
        Buy,
        Sell,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotOwner,
        InsufficientBalance,
        InsufficientAllowance,
        TransferToZeroAddress,
        ApproveToZeroAddress,
        Overflow,
        ThroneShieldActive,
        InsufficientDethroneAmount,
        TimelockNotExpired,
        NoPendingGameAddress,
        GameAddressAlreadySet,
        InvalidDexRouter,
        SwapFailed,
        NotGameContract,
        NotDampenerContract,
        NoTaxToPull,
        
        /// NEW: Transaction below minimum threshold (dusting protection)
        TransactionTooSmall,
        /// NEW: Contract is paused
        ContractPaused,
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52F {
        #[ink(constructor)]
        pub fn new(
            initial_supply: Balance,
            name: String,
            symbol: String,
            decimals: u8,
        ) -> Self {
            let caller = Self::env().caller();
            let block = Self::env().block_number();
            let now = Self::env().block_timestamp();
            
            let mut balances = Mapping::new();
            balances.insert(caller, &initial_supply);
            
            Self::env().emit_event(Transfer {
                from: None,
                to: Some(caller),
                value: initial_supply,
            });
            
            Self {
                total_supply: initial_supply,
                balances,
                allowances: Mapping::new(),
                name,
                symbol,
                decimals,
                owner: caller,
                team_tax_accumulated: 0,
                prize_pot_accumulated: 0,
                dampener_tax_accumulated: 0,
                last_team_push_block: block,
                king: None,
                king_buy_amount: 0,
                king_crowned_block: 0,
                king_crowned_timestamp: 0,
                last_throne_reset_timestamp: Self::align_to_gmt_midnight(now),
                game_address: None,
                dampener_address: None,
                victory_lap_address: None,
                pending_game_address: None,
                game_address_timelock: 0,
                dex_router: None,
                wqf_address: None,
                paused: false, // NEW: Start unpaused
            }
        }

        // =================================================================
        // PSP22 STANDARD FUNCTIONS
        // =================================================================

        #[ink(message)]
        pub fn total_supply(&self) -> Balance {
            self.total_supply
        }

        #[ink(message)]
        pub fn balance_of(&self, owner: AccountId) -> Balance {
            self.balances.get(owner).unwrap_or(0)
        }

        #[ink(message)]
        pub fn allowance(&self, owner: AccountId, spender: AccountId) -> Balance {
            self.allowances.get((owner, spender)).unwrap_or(0)
        }

        #[ink(message)]
        pub fn transfer(&mut self, to: AccountId, value: Balance) -> Result<(), Error> {
            let from = self.env().caller();
            self.transfer_from_to(from, to, value)
        }

        #[ink(message)]
        pub fn approve(&mut self, spender: AccountId, value: Balance) -> Result<(), Error> {
            let owner = self.env().caller();
            if spender == AccountId::from([0x0; 32]) {
                return Err(Error::ApproveToZeroAddress);
            }
            self.allowances.insert((owner, spender), &value);
            self.env().emit_event(Approval { owner, spender, value });
            Ok(())
        }

        #[ink(message)]
        pub fn transfer_from(
            &mut self,
            from: AccountId,
            to: AccountId,
            value: Balance,
        ) -> Result<(), Error> {
            let caller = self.env().caller();
            let allowance = self.allowance(from, caller);
            if allowance < value {
                return Err(Error::InsufficientAllowance);
            }
            self.transfer_from_to(from, to, value)?;
            self.allowances.insert((from, caller), &(allowance - value));
            Ok(())
        }

        #[ink(message)]
        pub fn name(&self) -> String {
            self.name.clone()
        }

        #[ink(message)]
        pub fn symbol(&self) -> String {
            self.symbol.clone()
        }

        #[ink(message)]
        pub fn decimals(&self) -> u8 {
            self.decimals
        }

        // =================================================================
        // INTERNAL TRANSFER
        // =================================================================

        fn transfer_from_to(
            &mut self,
            from: AccountId,
            to: AccountId,
            value: Balance,
        ) -> Result<(), Error> {
            if to == AccountId::from([0x0; 32]) {
                return Err(Error::TransferToZeroAddress);
            }
            
            let from_balance = self.balance_of(from);
            if from_balance < value {
                return Err(Error::InsufficientBalance);
            }
            
            self.balances.insert(from, &(from_balance - value));
            let to_balance = self.balance_of(to);
            self.balances.insert(to, &(to_balance.checked_add(value).ok_or(Error::Overflow)?));
            
            self.env().emit_event(Transfer {
                from: Some(from),
                to: Some(to),
                value,
            });
            
            Ok(())
        }

        // =================================================================
        // TAXATION LOGIC WITH TAX FLOOR PROTECTION
        // =================================================================

        #[ink(message, payable)]
        pub fn buy(&mut self, min_tokens_out: Balance) -> Result<Balance, Error> {
            let caller = self.env().caller();
            let qf_sent = self.env().transferred_value();
            
            if qf_sent == 0 {
                return Err(Error::InsufficientBalance);
            }
            
            // NEW: Tax Floor Check - ensure transaction is large enough for meaningful taxes
            // Each tax bucket must receive at least 1 unit
            let total_tax = qf_sent * E_BUY_TAX_BPS / BPS_DENOMINATOR;
            let team_portion = qf_sent * BUY_TAX_TEAM_BPS / BPS_DENOMINATOR;
            let prize_portion = total_tax.saturating_sub(team_portion);
            
            // Verify minimum thresholds (all portions must be >= 1 after division)
            if team_portion < 1 || prize_portion < 1 || qf_sent < MIN_TRANSACTION_THRESHOLD {
                return Err(Error::TransactionTooSmall);
            }
            
            // Accumulate taxes
            self.team_tax_accumulated = self.team_tax_accumulated
                .checked_add(team_portion)
                .ok_or(Error::Overflow)?;
            
            self.prize_pot_accumulated = self.prize_pot_accumulated
                .checked_add(prize_portion)
                .ok_or(Error::Overflow)?;
            
            // Rest of buy logic...
            self.process_throne_logic(caller, qf_sent)?;
            
            let qf_for_swap = qf_sent - total_tax;
            let tokens_out = self.execute_dex_swap_qf_to_token(caller, qf_for_swap, min_tokens_out)?;
            
            self.env().emit_event(TaxCollected {
                from: caller,
                amount: total_tax,
                tax_type: TaxType::Buy,
                team_portion,
                prize_portion,
                dampener_portion: 0,
            });
            
            self.check_team_push()?;
            
            Ok(tokens_out)
        }

        #[ink(message)]
        pub fn sell(&mut self, tokens_in: Balance, min_qf_out: Balance) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            self.transfer_from_to(caller, self.env().account_id(), tokens_in)?;
            
            let qf_out = self.execute_dex_swap_token_to_qf(caller, tokens_in, min_qf_out)?;
            
            // Tax calculations
            let total_tax = qf_out * PI_SELL_TAX_BPS / BPS_DENOMINATOR;
            let team_portion = qf_out * SELL_TAX_TEAM_BPS / BPS_DENOMINATOR;
            let dampener_portion = qf_out * SELL_TAX_DAMPENER_BPS / BPS_DENOMINATOR;
            let prize_portion = total_tax.saturating_sub(team_portion).saturating_sub(dampener_portion);
            
            // NEW: Tax Floor Check - all portions must be >= 1 unit
            if team_portion < 1 || dampener_portion < 1 || prize_portion < 1 || qf_out < MIN_TRANSACTION_THRESHOLD {
                return Err(Error::TransactionTooSmall);
            }
            
            // Accumulate taxes
            self.team_tax_accumulated = self.team_tax_accumulated
                .checked_add(team_portion)
                .ok_or(Error::Overflow)?;
            
            self.dampener_tax_accumulated = self.dampener_tax_accumulated
                .checked_add(dampener_portion)
                .ok_or(Error::Overflow)?;
            
            self.prize_pot_accumulated = self.prize_pot_accumulated
                .checked_add(prize_portion)
                .ok_or(Error::Overflow)?;
            
            let qf_to_user = qf_out - total_tax;
            self.env().transfer(caller, qf_to_user).map_err(|_| Error::SwapFailed)?;
            
            self.env().emit_event(TaxCollected {
                from: caller,
                amount: total_tax,
                tax_type: TaxType::Sell,
                team_portion,
                prize_portion,
                dampener_portion,
            });
            
            self.check_team_push()?;
            
            Ok(qf_to_user)
        }

        // =================================================================
        // PULL INTERFACE WITH CIRCUIT BREAKER
        // =================================================================

        #[ink(message)]
        pub fn pull_prize_tax(&mut self) -> Result<Balance, Error> {
            // NEW: Circuit breaker check
            if self.paused {
                return Err(Error::ContractPaused);
            }
            
            let caller = self.env().caller();
            let game_addr = self.game_address.ok_or(Error::NotGameContract)?;
            if caller != game_addr {
                return Err(Error::NotGameContract);
            }
            
            let amount = self.prize_pot_accumulated;
            if amount == 0 {
                return Err(Error::NoTaxToPull);
            }
            
            self.prize_pot_accumulated = 0;
            self.env().transfer(caller, amount).map_err(|_| Error::SwapFailed)?;
            
            self.env().emit_event(PrizeTaxPulled {
                game_address: caller,
                amount,
                timestamp: self.env().block_timestamp(),
            });
            
            Ok(amount)
        }

        #[ink(message)]
        pub fn pull_dampener_tax(&mut self) -> Result<Balance, Error> {
            // NEW: Circuit breaker check
            if self.paused {
                return Err(Error::ContractPaused);
            }
            
            let caller = self.env().caller();
            let dampener_addr = self.dampener_address.ok_or(Error::NotDampenerContract)?;
            if caller != dampener_addr {
                return Err(Error::NotDampenerContract);
            }
            
            let amount = self.dampener_tax_accumulated;
            if amount == 0 {
                return Err(Error::NoTaxToPull);
            }
            
            self.dampener_tax_accumulated = 0;
            self.env().transfer(caller, amount).map_err(|_| Error::SwapFailed)?;
            
            self.env().emit_event(DampenerTaxPulled {
                dampener_address: caller,
                amount,
                timestamp: self.env().block_timestamp(),
            });
            
            Ok(amount)
        }

        // =================================================================
        // CIRCUIT BREAKER (NEW)
        // =================================================================

        /// Emergency pause/unpause function
        /// When paused, pull functions are disabled (satellites cannot withdraw)
        #[ink(message)]
        pub fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
            self.only_owner()?;
            self.paused = paused;
            
            self.env().emit_event(CircuitBreakerToggled {
                paused,
                timestamp: self.env().block_timestamp(),
            });
            
            Ok(())
        }

        #[ink(message)]
        pub fn is_paused(&self) -> bool {
            self.paused
        }

        // =================================================================
        // THRONE LOGIC (REMOVED MANUAL RESET FUNCTIONS)
        // =================================================================

        fn process_throne_logic(&mut self, buyer: AccountId, buy_amount: Balance) -> Result<(), Error> {
            self.check_throne_reset()?;
            let current_block = self.env().block_number();
            
            match self.king {
                None => {
                    self.crown_king(buyer, buy_amount, current_block)?;
                }
                Some(current_king) => {
                    if current_king == buyer {
                        if buy_amount > self.king_buy_amount {
                            self.king_buy_amount = buy_amount;
                        }
                    } else {
                        let blocks_since_crown = current_block - self.king_crowned_block;
                        if blocks_since_crown < THRONE_SHIELD_BLOCKS {
                            return Err(Error::ThroneShieldActive);
                        }
                        
                        let required_amount = self.king_buy_amount
                            .checked_mul(DETHRONE_MULTIPLIER_BPS)
                            .ok_or(Error::Overflow)?
                            / BPS_DENOMINATOR;
                        
                        if buy_amount < required_amount {
                            return Err(Error::InsufficientDethroneAmount);
                        }
                        
                        self.crown_king(buyer, buy_amount, current_block)?;
                    }
                }
            }
            Ok(())
        }

        fn crown_king(&mut self, new_king: AccountId, buy_amount: Balance, block: u32) -> Result<(), Error> {
            let previous_king = self.king;
            let now = self.env().block_timestamp();
            
            self.king = Some(new_king);
            self.king_buy_amount = buy_amount;
            self.king_crowned_block = block;
            self.king_crowned_timestamp = now;
            
            self.env().emit_event(ThroneClaimed {
                new_king,
                previous_king,
                buy_amount,
                block,
                timestamp: now,
            });
            Ok(())
        }

        fn check_throne_reset(&mut self) -> Result<(), Error> {
            let now = self.env().block_timestamp();
            let current_gmt_day = now / MS_PER_DAY;
            let last_reset_day = self.last_throne_reset_timestamp / MS_PER_DAY;
            
            if current_gmt_day > last_reset_day {
                let previous_king = self.king;
                self.king = None;
                self.king_buy_amount = 0;
                self.king_crowned_block = 0;
                self.king_crowned_timestamp = 0;
                self.last_throne_reset_timestamp = self.align_to_gmt_midnight(now);
                
                self.env().emit_event(ThroneReset {
                    previous_king,
                    timestamp: now,
                    is_manual: false,
                });
            }
            Ok(())
        }

        fn align_to_gmt_midnight(&self, timestamp: u64) -> u64 {
            (timestamp / MS_PER_DAY) * MS_PER_DAY
        }

        // =================================================================
        // TEAM TAX PUSH & BRIDGE (UNCHANGED)
        // =================================================================

        fn check_team_push(&mut self) -> Result<(), Error> {
            let current_block = self.env().block_number();
            if current_block - self.last_team_push_block >= TEAM_PUSH_INTERVAL {
                if self.team_tax_accumulated > 0 {
                    let amount = self.team_tax_accumulated;
                    self.team_tax_accumulated = 0;
                    self.last_team_push_block = current_block;
                    self.env().transfer(self.owner, amount).map_err(|_| Error::SwapFailed)?;
                    self.env().emit_event(TeamTaxPushed { amount, block: current_block });
                }
            }
            Ok(())
        }

        #[ink(message)]
        pub fn propose_game_address(&mut self, new_address: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            let now = self.env().block_timestamp();
            self.pending_game_address = Some(new_address);
            self.game_address_timelock = now + TIMELOCK_DURATION;
            self.env().emit_event(GameAddressProposed {
                proposed_address: new_address,
                executable_at: self.game_address_timelock,
            });
            Ok(())
        }

        #[ink(message)]
        pub fn execute_set_game_address(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            let now = self.env().block_timestamp();
            if now < self.game_address_timelock {
                return Err(Error::TimelockNotExpired);
            }
            let new_address = self.pending_game_address.ok_or(Error::NoPendingGameAddress)?;
            let previous_address = self.game_address;
            self.game_address = Some(new_address);
            self.pending_game_address = None;
            self.game_address_timelock = 0;
            self.env().emit_event(GameAddressSet {
                new_address,
                previous_address,
            });
            Ok(())
        }

        #[ink(message)]
        pub fn set_dampener_address(&mut self, address: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.dampener_address = Some(address);
            Ok(())
        }

        #[ink(message)]
        pub fn set_victory_lap_address(&mut self, address: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.victory_lap_address = Some(address);
            Ok(())
        }

        #[ink(message)]
        pub fn set_dex_router(&mut self, router: AccountId, wqf: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.dex_router = Some(router);
            self.wqf_address = Some(wqf);
            Ok(())
        }

        // =================================================================
        // DEX PLACEHOLDER FUNCTIONS
        // =================================================================

        fn execute_dex_swap_qf_to_token(
            &self,
            to: AccountId,
            qf_amount: Balance,
            min_out: Balance,
        ) -> Result<Balance, Error> {
            if self.dex_router.is_none() {
                return Ok(qf_amount);
            }
            Err(Error::SwapFailed)
        }

        fn execute_dex_swap_token_to_qf(
            &self,
            to: AccountId,
            token_amount: Balance,
            min_qf_out: Balance,
        ) -> Result<Balance, Error> {
            if self.dex_router.is_none() {
                return Ok(token_amount);
            }
            Err(Error::SwapFailed)
        }

        // =================================================================
        // VIEW FUNCTIONS
        // =================================================================

        #[ink(message)]
        pub fn get_king(&self) -> Option<AccountId> {
            self.king
        }

        #[ink(message)]
        pub fn get_king_buy_amount(&self) -> Balance {
            self.king_buy_amount
        }

        #[ink(message)]
        pub fn get_team_tax_accumulated(&self) -> Balance {
            self.team_tax_accumulated
        }

        #[ink(message)]
        pub fn get_prize_pot_accumulated(&self) -> Balance {
            self.prize_pot_accumulated
        }

        #[ink(message)]
        pub fn get_dampener_tax_accumulated(&self) -> Balance {
            self.dampener_tax_accumulated
        }

        fn only_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
            }
            Ok(())
        }
    }
}
