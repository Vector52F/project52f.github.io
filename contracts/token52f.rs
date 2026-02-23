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
    
    /// Basis Points denominator: 10,000 = 100%
    pub const BPS_DENOMINATOR: u128 = 10_000;
    
    /// Buy tax: e ≈ 2.72% = 272 BPS
    /// Split: 75 BPS Team, 197 BPS Prize Pot
    pub const E_BUY_TAX_BPS: u128 = 272;
    pub const BUY_TAX_TEAM_BPS: u128 = 75;
    pub const BUY_TAX_PRIZE_BPS: u128 = 197;
    
    /// Sell tax: π ≈ 3.14% = 314 BPS  
    /// Split: 75 BPS Team, 100 BPS Dampener, 139 BPS Prize Pot
    pub const PI_SELL_TAX_BPS: u128 = 314;
    pub const SELL_TAX_TEAM_BPS: u128 = 75;
    pub const SELL_TAX_DAMPENER_BPS: u128 = 100;
    pub const SELL_TAX_PRIZE_BPS: u128 = 139;
    
    /// Team tax push interval: 520,000 blocks (~14.4 hours @ 0.1s/block)
    pub const TEAM_PUSH_INTERVAL: u32 = 520_000;
    
    /// Throne shield duration: 1 hour = 36,000 blocks @ 0.1s/block
    pub const THRONE_SHIELD_BLOCKS: u32 = 36_000;
    
    /// π/e dethroning multiplier: 15.57% = 1,557 BPS (applied to current king's buy)
    /// New buy must be 115.57% of current king's buy (100% + 15.57%)
    pub const DETHRONE_MULTIPLIER_BPS: u128 = 11_557; // 115.57%
    
    /// GMT 00:00 in milliseconds (for timestamp calculations)
    /// Used for passive throne reset check
    pub const MS_PER_DAY: u64 = 86_400_000;
    
    /// 24-hour timelock for bridge operations (in milliseconds)
    pub const TIMELOCK_DURATION: u64 = 86_400_000;

    // =========================================================================
    // STORAGE
    // =========================================================================

    #[ink(storage)]
    pub struct Project52F {
        // === PSP22 Standard ===
        /// Total token supply
        total_supply: Balance,
        /// Token balances
        balances: Mapping<AccountId, Balance>,
        /// Allowances for spender
        allowances: Mapping<(AccountId, AccountId), Balance>,
        /// Token metadata
        name: String,
        symbol: String,
        decimals: u8,
        
        // === Ownership ===
        /// Contract owner
        owner: AccountId,
        
        // === Tax Accounting (all in $QF) ===
        /// Accumulated team tax (pushed every 520k blocks)
        team_tax_accumulated: Balance,
        /// Accumulated prize pot tax (pulled by BirthdayParadox)
        prize_pot_accumulated: Balance,
        /// Accumulated dampener tax (pulled by DampenerVault)
        dampener_tax_accumulated: Balance,
        /// Last team push block
        last_team_push_block: u32,
        
        // === Throne State ===
        /// Current King address
        king: Option<AccountId>,
        /// Current King's buy amount (in $QF)
        king_buy_amount: Balance,
        /// Block when King was crowned
        king_crowned_block: u32,
        /// Timestamp when King was crowned (for GMT calculations)
        king_crowned_timestamp: u64,
        /// Whether manual reset is enabled (DEVNET only)
        manual_reset_enabled: bool,
        /// Last throne reset timestamp (GMT 00:00)
        last_throne_reset_timestamp: u64,
        
        // === Bridge (Satellite Upgrade) ===
        /// BirthdayParadox game contract address
        game_address: Option<AccountId>,
        /// DampenerVault contract address
        dampener_address: Option<AccountId>,
        /// VictoryLap contract address
        victory_lap_address: Option<AccountId>,
        /// Timelock: proposed game address
        pending_game_address: Option<AccountId>,
        /// Timelock: when proposal can be executed
        game_address_timelock: u64,
        
        // === DEX Placeholder ===
        /// DEX router address (placeholder for QF Network integration)
        dex_router: Option<AccountId>,
        /// WQF (wrapped QF) address for routing
        wqf_address: Option<AccountId>,
    }

    // =========================================================================
    // EVENTS
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
    pub struct ManualResetToggled {
        enabled: bool,
    }

    // === PULL INTERFACE EVENTS ===
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
        /// Caller is not the owner
        NotOwner,
        /// Insufficient balance
        InsufficientBalance,
        /// Insufficient allowance
        InsufficientAllowance,
        /// Transfer to zero address
        TransferToZeroAddress,
        /// Approve to zero address
        ApproveToZeroAddress,
        /// Math overflow
        Overflow,
        /// Throne shield active
        ThroneShieldActive,
        /// Buy amount insufficient to dethrone
        InsufficientDethroneAmount,
        /// Timelock not expired
        TimelockNotExpired,
        /// No pending game address
        NoPendingGameAddress,
        /// Game address already set
        GameAddressAlreadySet,
        /// Invalid DEX router
        InvalidDexRouter,
        /// Swap failed (placeholder)
        SwapFailed,
        /// Manual reset not enabled
        ManualResetNotEnabled,
        /// Caller is not authorized game contract
        NotGameContract,
        /// Caller is not authorized dampener contract
        NotDampenerContract,
        /// No tax available to pull
        NoTaxToPull,
    }

    // =========================================================================
    // DEX PLACEHOLDER TRAIT
    // =========================================================================

    /// Placeholder for QF Network DEX Router integration
    /// TODO: Replace with actual SPIN-Swap interface when available
    pub trait QfRouter {
        /// Get expected output amount for swap
        fn get_amounts_out(&self, amount_in: Balance, path: Vec<AccountId>) -> Result<Vec<Balance>, Error>;
        
        /// Execute swap: exact tokens for tokens
        fn swap_exact_tokens_for_tokens(
            &mut self,
            amount_in: Balance,
            amount_out_min: Balance,
            path: Vec<AccountId>,
            to: AccountId,
            deadline: u64,
        ) -> Result<Vec<Balance>, Error>;
    }

    // =========================================================================
    // IMPLEMENTATION
    // =========================================================================

    impl Project52F {
        /// Constructor
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
                manual_reset_enabled: true, // Enabled for DEVNET
                last_throne_reset_timestamp: Self::align_to_gmt_midnight(now),
                game_address: None,
                dampener_address: None,
                victory_lap_address: None,
                pending_game_address: None,
                game_address_timelock: 0,
                dex_router: None,
                wqf_address: None,
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
        // TAXATION LOGIC — ALL IN $QF (DUST-FREE CALCULATION)
        // =================================================================

        /// Buy function: User sends $QF, receives $52f minus e-tax
        /// Tax calculated BEFORE swap, taken from input $QF
        /// 
        /// DUST-FIX: Calculate total and team with BPS, then prize = total - team
        #[ink(message, payable)]
        pub fn buy(&mut self, min_tokens_out: Balance) -> Result<Balance, Error> {
            let caller = self.env().caller();
            let qf_sent = self.env().transferred_value();
            
            if qf_sent == 0 {
                return Err(Error::InsufficientBalance);
            }
            
            // Calculate total e-tax (272 BPS) on $QF input
            let total_tax = qf_sent
                .checked_mul(E_BUY_TAX_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // Calculate team portion (75 BPS)
            let team_portion = qf_sent
                .checked_mul(BUY_TAX_TEAM_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // DUST-FIX: Prize gets the remainder to ensure 100% allocation
            let prize_portion = total_tax.saturating_sub(team_portion);
            
            // Accumulate taxes (all in $QF)
            self.team_tax_accumulated = self.team_tax_accumulated
                .checked_add(team_portion)
                .ok_or(Error::Overflow)?;
            
            self.prize_pot_accumulated = self.prize_pot_accumulated
                .checked_add(prize_portion)
                .ok_or(Error::Overflow)?;
            
            // Check throne eligibility (before swap, using $QF amount)
            self.process_throne_logic(caller, qf_sent)?;
            
            // Execute DEX swap: remaining $QF -> $52f
            let qf_for_swap = qf_sent - total_tax;
            let tokens_out = self.execute_dex_swap_qf_to_token(caller, qf_for_swap, min_tokens_out)?;
            
            // Emit tax event
            self.env().emit_event(TaxCollected {
                from: caller,
                amount: total_tax,
                tax_type: TaxType::Buy,
                team_portion,
                prize_portion,
                dampener_portion: 0,
            });
            
            // Check and push team tax if interval reached
            self.check_team_push()?;
            
            Ok(tokens_out)
        }

        /// Sell function: User sends $52f, receives $QF minus π-tax
        /// Tax calculated AFTER swap, taken from output $QF
        /// 
        /// DUST-FIX: Calculate total, team, dampener with BPS, then prize = total - team - dampener
        #[ink(message)]
        pub fn sell(&mut self, tokens_in: Balance, min_qf_out: Balance) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            // Transfer $52f from user to contract
            self.transfer_from_to(caller, self.env().account_id(), tokens_in)?;
            
            // Execute DEX swap: $52f -> $QF
            let qf_out = self.execute_dex_swap_token_to_qf(caller, tokens_in, min_qf_out)?;
            
            // Calculate total π-tax (314 BPS) on $QF output
            let total_tax = qf_out
                .checked_mul(PI_SELL_TAX_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // Calculate team portion (75 BPS)
            let team_portion = qf_out
                .checked_mul(SELL_TAX_TEAM_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // Calculate dampener portion (100 BPS)
            let dampener_portion = qf_out
                .checked_mul(SELL_TAX_DAMPENER_BPS)
                .ok_or(Error::Overflow)?
                / BPS_DENOMINATOR;
            
            // DUST-FIX: Prize gets the remainder to ensure 100% allocation
            let prize_portion = total_tax
                .saturating_sub(team_portion)
                .saturating_sub(dampener_portion);
            
            // Accumulate taxes (all in $QF)
            self.team_tax_accumulated = self.team_tax_accumulated
                .checked_add(team_portion)
                .ok_or(Error::Overflow)?;
            
            self.dampener_tax_accumulated = self.dampener_tax_accumulated
                .checked_add(dampener_portion)
                .ok_or(Error::Overflow)?;
            
            self.prize_pot_accumulated = self.prize_pot_accumulated
                .checked_add(prize_portion)
                .ok_or(Error::Overflow)?;
            
            // Send remaining $QF to user
            let qf_to_user = qf_out - total_tax;
            self.env().transfer(caller, qf_to_user).map_err(|_| Error::SwapFailed)?;
            
            // Emit tax event
            self.env().emit_event(TaxCollected {
                from: caller,
                amount: total_tax,
                tax_type: TaxType::Sell,
                team_portion,
                prize_portion,
                dampener_portion,
            });
            
            // Check and push team tax if interval reached
            self.check_team_push()?;
            
            Ok(qf_to_user)
        }

        // =================================================================
        // PULL INTERFACE — SATELLITE WITHDRAWAL FUNCTIONS
        // =================================================================

        /// Pull accumulated prize tax to the game contract (BirthdayParadox)
        /// Only callable by the registered game_address
        #[ink(message)]
        pub fn pull_prize_tax(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            // Verify caller is the authorized game contract
            let game_addr = self.game_address.ok_or(Error::NotGameContract)?;
            if caller != game_addr {
                return Err(Error::NotGameContract);
            }
            
            let amount = self.prize_pot_accumulated;
            if amount == 0 {
                return Err(Error::NoTaxToPull);
            }
            
            // Reset storage before transfer (reentrancy protection)
            self.prize_pot_accumulated = 0;
            
            // Transfer native $QF to game contract
            self.env().transfer(caller, amount).map_err(|_| Error::SwapFailed)?;
            
            self.env().emit_event(PrizeTaxPulled {
                game_address: caller,
                amount,
                timestamp: self.env().block_timestamp(),
            });
            
            Ok(amount)
        }

        /// Pull accumulated dampener tax to the dampener vault
        /// Only callable by the registered dampener_address
        #[ink(message)]
        pub fn pull_dampener_tax(&mut self) -> Result<Balance, Error> {
            let caller = self.env().caller();
            
            // Verify caller is the authorized dampener contract
            let dampener_addr = self.dampener_address.ok_or(Error::NotDampenerContract)?;
            if caller != dampener_addr {
                return Err(Error::NotDampenerContract);
            }
            
            let amount = self.dampener_tax_accumulated;
            if amount == 0 {
                return Err(Error::NoTaxToPull);
            }
            
            // Reset storage before transfer (reentrancy protection)
            self.dampener_tax_accumulated = 0;
            
            // Transfer native $QF to dampener contract
            self.env().transfer(caller, amount).map_err(|_| Error::SwapFailed)?;
            
            self.env().emit_event(DampenerTaxPulled {
                dampener_address: caller,
                amount,
                timestamp: self.env().block_timestamp(),
            });
            
            Ok(amount)
        }

        // =================================================================
        // THRONE LOGIC
        // =================================================================

        fn process_throne_logic(&mut self, buyer: AccountId, buy_amount: Balance) -> Result<(), Error> {
            self.check_throne_reset()?;
            
            let current_block = self.env().block_number();
            
            match self.king {
                None => {
                    // Empty throne — claim it
                    self.crown_king(buyer, buy_amount, current_block)?;
                }
                Some(current_king) => {
                    if current_king == buyer {
                        // Same buyer — update amount if larger (no shield check)
                        if buy_amount > self.king_buy_amount {
                            self.king_buy_amount = buy_amount;
                        }
                    } else {
                        // Different buyer — check shield and dethrone logic
                        let blocks_since_crown = current_block - self.king_crowned_block;
                        
                        if blocks_since_crown < THRONE_SHIELD_BLOCKS {
                            return Err(Error::ThroneShieldActive);
                        }
                        
                        // Check π/e multiplier: new buy must be 115.57% of current
                        let required_amount = self.king_buy_amount
                            .checked_mul(DETHRONE_MULTIPLIER_BPS)
                            .ok_or(Error::Overflow)?
                            / BPS_DENOMINATOR;
                        
                        if buy_amount < required_amount {
                            return Err(Error::InsufficientDethroneAmount);
                        }
                        
                        // Dethrone and crown new king
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

        /// Check if throne should reset (passive, on first transaction after 00:00 GMT)
        fn check_throne_reset(&mut self) -> Result<(), Error> {
            let now = self.env().block_timestamp();
            let current_gmt_day = now / MS_PER_DAY;
            let last_reset_day = self.last_throne_reset_timestamp / MS_PER_DAY;
            
            if current_gmt_day > last_reset_day {
                // Reset throne
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

        /// Manual throne reset (DEVNET only, requires manual_reset_enabled)
        #[ink(message)]
        pub fn manual_throne_reset(&mut self) -> Result<(), Error> {
            self.only_owner()?;
            
            if !self.manual_reset_enabled {
                return Err(Error::ManualResetNotEnabled);
            }
            
            let previous_king = self.king;
            let now = self.env().block_timestamp();
            
            self.king = None;
            self.king_buy_amount = 0;
            self.king_crowned_block = 0;
            self.king_crowned_timestamp = 0;
            self.last_throne_reset_timestamp = self.align_to_gmt_midnight(now);
            
            self.env().emit_event(ThroneReset {
                previous_king,
                timestamp: now,
                is_manual: true,
            });
            
            Ok(())
        }

        #[ink(message)]
        pub fn toggle_manual_reset(&mut self, enabled: bool) -> Result<(), Error> {
            self.only_owner()?;
            self.manual_reset_enabled = enabled;
            self.env().emit_event(ManualResetToggled { enabled });
            Ok(())
        }

        fn align_to_gmt_midnight(&self, timestamp: u64) -> u64 {
            (timestamp / MS_PER_DAY) * MS_PER_DAY
        }

        // =================================================================
        // TEAM TAX PUSH
        // =================================================================

        fn check_team_push(&mut self) -> Result<(), Error> {
            let current_block = self.env().block_number();
            
            if current_block - self.last_team_push_block >= TEAM_PUSH_INTERVAL {
                if self.team_tax_accumulated > 0 {
                    let amount = self.team_tax_accumulated;
                    self.team_tax_accumulated = 0;
                    self.last_team_push_block = current_block;
                    
                    // Transfer to owner (team wallet)
                    self.env().transfer(self.owner, amount).map_err(|_| Error::SwapFailed)?;
                    
                    self.env().emit_event(TeamTaxPushed {
                        amount,
                        block: current_block,
                    });
                }
            }
            
            Ok(())
        }

        // =================================================================
        // BRIDGE — 24-HOUR TIMELOCK
        // =================================================================

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

        // =================================================================
        // DEX PLACEHOLDER FUNCTIONS
        // =================================================================

        /// TODO: QF Network Integration — Execute swap $QF -> $52f
        /// 
        /// PRODUCTION IMPLEMENTATION NOTES:
        /// 1. Call DEX router to swap exact QF for tokens
        /// 2. The router will return the amount of $52f tokens received
        /// 3. MINT or TRANSFER the received $52f tokens to the `to` address
        ///    - If minting: update total_supply and balances[to]
        ///    - If transferring: ensure contract holds sufficient $52f liquidity
        /// 4. Route the input $QF (qf_amount) to the Liquidity Pool via the router
        /// 5. Handle slippage protection using min_tokens_out
        /// 6. Emit appropriate Transfer event for the $52f tokens
        fn execute_dex_swap_qf_to_token(
            &self,
            to: AccountId,
            qf_amount: Balance,
            min_out: Balance,
        ) -> Result<Balance, Error> {
            // DEVNET: Return mock value (1:1 for testing)
            // TODO: Replace with actual DEX router call when QF Network contracts available
            
            if self.dex_router.is_none() {
                // Mock: return qf_amount as token amount (1:1)
                // In production, this would mint/transfer actual $52f tokens to `to`
                return Ok(qf_amount);
            }
            
            // Real implementation would:
            // 1. Call router.get_amounts_out() to quote
            // 2. Call router.swap_exact_tokens_for_tokens()
            // 3. Handle the PSP22 token transfer/minting to `to` address
            // 4. Route $QF to LP
            // 5. Return actual tokens received
            
            Err(Error::SwapFailed)
        }

        /// TODO: QF Network Integration — Execute swap $52f -> $QF
        /// 
        /// PRODUCTION IMPLEMENTATION NOTES:
        /// 1. Call DEX router to swap exact $52f tokens for QF
        /// 2. The router will transfer $52f from this contract to the LP
        /// 3. The router will return native $QF to this contract
        /// 4. Ensure the contract receives the $QF output before tax calculation
        /// 5. Handle slippage protection using min_qf_out
        /// 6. The calling function (sell) will then calculate taxes on the received QF
        fn execute_dex_swap_token_to_qf(
            &self,
            to: AccountId,
            token_amount: Balance,
            min_qf_out: Balance,
        ) -> Result<Balance, Error> {
            // DEVNET: Return mock value (1:1 for testing)
            
            if self.dex_router.is_none() {
                // Mock: return token_amount as QF amount (1:1)
                // In production, this would:
                // - Transfer $52f tokens from contract to DEX LP
                // - Receive native $QF from the LP to this contract
                // - Return the received QF amount for tax calculation
                return Ok(token_amount);
            }
            
            Err(Error::SwapFailed)
        }

        #[ink(message)]
        pub fn set_dex_router(&mut self, router: AccountId, wqf: AccountId) -> Result<(), Error> {
            self.only_owner()?;
            self.dex_router = Some(router);
            self.wqf_address = Some(wqf);
            Ok(())
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
        pub fn get_king_crowned_block(&self) -> u32 {
            self.king_crowned_block
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

        #[ink(message)]
        pub fn get_pending_game_address(&self) -> Option<AccountId> {
            self.pending_game_address
        }

        #[ink(message)]
        pub fn get_game_address_timelock(&self) -> u64 {
            self.game_address_timelock
        }

        #[ink(message)]
        pub fn calculate_dethrone_threshold(&self) -> Balance {
            self.king_buy_amount
                .saturating_mul(DETHRONE_MULTIPLIER_BPS)
                / BPS_DENOMINATOR
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
            
            let contract = Project52F::new(
                1_000_000_000_000_000_000,
                "Project52F".to_string(),
                "52F".to_string(),
                18,
            );
            
            assert_eq!(contract.total_supply(), 1_000_000_000_000_000_000);
            assert_eq!(contract.balance_of(accounts.alice), 1_000_000_000_000_000_000);
            assert_eq!(contract.name(), "Project52F");
            assert_eq!(contract.symbol(), "52F");
            assert_eq!(contract.decimals(), 18);
        }

        #[ink::test]
        fn tax_calculation_buy_dust_free() {
            // 100 QF buy, 2.72% tax = 2.72 QF
            // Team: 0.75 QF, Prize: 2.72 - 0.75 = 1.97 QF (remainder)
            let qf_sent: Balance = 100_000_000_000_000_000_000; // 100 QF
            
            let total_tax = qf_sent * E_BUY_TAX_BPS / BPS_DENOMINATOR;
            let team = qf_sent * BUY_TAX_TEAM_BPS / BPS_DENOMINATOR;
            let prize = total_tax.saturating_sub(team); // DUST-FIX
            
            assert_eq!(total_tax, 2_720_000_000_000_000_000); // 2.72 QF
            assert_eq!(team, 750_000_000_000_000_000); // 0.75 QF
            assert_eq!(prize, 1_970_000_000_000_000_000); // 1.97 QF (exact remainder)
            
            // Verify no dust: team + prize == total_tax
            assert_eq!(team + prize, total_tax);
        }

        #[ink::test]
        fn tax_calculation_sell_dust_free() {
            // 100 QF output, 3.14% tax = 3.14 QF
            // Team: 0.75 QF, Dampener: 1.00 QF, Prize: 3.14 - 0.75 - 1.00 = 1.39 QF
            let qf_out: Balance = 100_000_000_000_000_000_000;
            
            let total_tax = qf_out * PI_SELL_TAX_BPS / BPS_DENOMINATOR;
            let team = qf_out * SELL_TAX_TEAM_BPS / BPS_DENOMINATOR;
            let dampener = qf_out * SELL_TAX_DAMPENER_BPS / BPS_DENOMINATOR;
            let prize = total_tax.saturating_sub(team).saturating_sub(dampener); // DUST-FIX
            
            assert_eq!(total_tax, 3_140_000_000_000_000_000); // 3.14 QF
            assert_eq!(team, 750_000_000_000_000_000); // 0.75 QF
            assert_eq!(dampener, 1_000_000_000_000_000_000); // 1.00 QF
            assert_eq!(prize, 1_390_000_000_000_000_000); // 1.39 QF (exact remainder)
            
            // Verify no dust: team + dampener + prize == total_tax
            assert_eq!(team + dampener + prize, total_tax);
        }

        #[ink::test]
        fn throne_claim_and_dethrone() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let mut contract = Project52F::new(
                1_000_000_000_000_000_000,
                "Project52F".to_string(),
                "52F".to_string(),
                18,
            );
            
            set_timestamp(1_000_000); // Some GMT time
            set_block_number(1);
            
            // Alice claims throne with 100 QF buy
            contract.process_throne_logic(accounts.alice, 100_000_000_000_000_000_000).unwrap();
            assert_eq!(contract.get_king(), Some(accounts.alice));
            assert_eq!(contract.get_king_buy_amount(), 100_000_000_000_000_000_000);
            
            // Bob tries to dethrone immediately — fails (shield active)
            set_block_number(2); // Only 0.1s later
            let result = contract.process_throne_logic(accounts.bob, 200_000_000_000_000_000_000);
            assert_eq!(result, Err(Error::ThroneShieldActive));
            
            // After 1 hour (36,000 blocks), Bob can dethrone
            set_block_number(36_001);
            let dethrone_amount = contract.calculate_dethrone_threshold();
            // 100 QF * 115.57% = 115.57 QF required
            assert_eq!(dethrone_amount, 115_570_000_000_000_000_000);
            
            // Bob tries with insufficient amount
            let result = contract.process_throne_logic(accounts.bob, 115_000_000_000_000_000_000);
            assert_eq!(result, Err(Error::InsufficientDethroneAmount));
            
            // Bob succeeds with sufficient amount
            contract.process_throne_logic(accounts.bob, 116_000_000_000_000_000_000).unwrap();
            assert_eq!(contract.get_king(), Some(accounts.bob));
        }

        #[ink::test]
        fn timelock_mechanism() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let mut contract = Project52F::new(
                1_000_000_000_000_000_000,
                "Project52F".to_string(),
                "52F".to_string(),
                18,
            );
            
            // Propose new game address
            set_timestamp(0);
            contract.propose_game_address(accounts.bob).unwrap();
            assert_eq!(contract.get_pending_game_address(), Some(accounts.bob));
            assert_eq!(contract.get_game_address_timelock(), 86_400_000); // 24 hours
            
            // Try to execute immediately — fails
            let result = contract.execute_set_game_address();
            assert_eq!(result, Err(Error::TimelockNotExpired));
            
            // After 24 hours — succeeds
            set_timestamp(86_400_000);
            contract.execute_set_game_address().unwrap();
        }

        #[ink::test]
        fn pull_prize_tax_unauthorized() {
            let accounts = default_accounts();
            set_caller(accounts.alice);
            
            let mut contract = Project52F::new(
                1_000_000_000_000_000_000,
                "Project52F".to_string(),
                "52F".to_string(),
                18,
            );
            
            // Try to pull without game address set
            set_caller(accounts.bob);
            let result = contract.pull_prize_tax();
            assert_eq!(result, Err(Error::NotGameContract));
            
            // Set game address
            set_caller(accounts.alice);
            contract.propose_game_address(accounts.bob).unwrap();
            set_timestamp(86_400_000);
            contract.execute_set_game_address().unwrap();
            
            // Now bob can pull (but there's no tax accumulated)
            set_caller(accounts.bob);
            let result = contract.pull_prize_tax();
            assert_eq!(result, Err(Error::NoTaxToPull));
        }
    }
}
