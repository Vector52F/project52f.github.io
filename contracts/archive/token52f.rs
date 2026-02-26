#![cfg_attr(not(feature = "std"), no_std, no_main)]

#[ink::contract]
mod project52f {
    use ink::prelude::string::String;
    use ink::storage::Mapping;
    use ink::primitives::{Address, U256};
    use ink::env::hash::H256;

    // =========================================================================
    // CONSTANTS — PROTOCOL MATHEMATICS
    // =========================================================================
    pub const BPS_DENOMINATOR: U256 = U256::from(10_000);
    pub const E_BUY_TAX_BPS: U256 = U256::from(272);      // Euler's Number
    pub const PI_SELL_TAX_BPS: U256 = U256::from(314);    // Pi
    
    pub const TAX_TEAM_BPS: U256 = U256::from(75);
    pub const TAX_DAMPENER_BPS: U256 = U256::from(100);
    
    pub const EPOCH_SIZE: u32 = 52;                       // 52-Transaction Epoch
    pub const TEAM_PUSH_INTERVAL: u32 = 520_000;          // Block-based push
    
    pub const MIN_TRANSACTION_THRESHOLD: U256 = U256::from(1_000_000_000_000_000_000u128); // 1 QF Floor

    // =========================================================================
    // STORAGE
    // =========================================================================
    #[ink(storage)]
    pub struct Project52F {
        total_supply: U256,
        balances: Mapping<Address, U256>,
        allowances: Mapping<(Address, Address), U256>,
        name: String,
        symbol: String,
        decimals: u8,
        owner: Address,
        
        // Tax Accumulators
        team_tax_accumulated: U256,
        prize_pot_accumulated: U256,
        dampener_tax_accumulated: U256,
        
        // Sequencer State
        sequencer_address: Option<Address>,
        dampener_address: Option<Address>,
        current_epoch_counter: u32,
        last_team_push_block: u32,
        
        paused: bool,
    }

    #[ink(event)]
    pub struct EpochReady {
        #[ink(topic)]
        epoch_id: u32,
        prize_pool: U256,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        NotOwner,
        InsufficientBalance,
        TransactionTooSmall,
        NotSequencerContract,
        NotDampenerContract,
        ContractPaused,
        Overflow,
        TransferFailed,
    }

    impl Project52F {
        #[ink(constructor)]
        pub fn new(initial_supply: U256, name: String, symbol: String) -> Self {
            let caller = Self::env().caller();
            let mut balances = Mapping::new();
            balances.insert(caller, &initial_supply);
            
            Self {
                total_supply: initial_supply,
                balances,
                allowances: Mapping::new(),
                name,
                symbol,
                decimals: 18,
                owner: caller,
                team_tax_accumulated: U256::ZERO,
                prize_pot_accumulated: U256::ZERO,
                dampener_tax_accumulated: U256::ZERO,
                sequencer_address: None,
                dampener_address: None,
                current_epoch_counter: 0,
                last_team_push_block: Self::env().block_number(),
                paused: false,
            }
        }

        // =================================================================
        // CORE TRANSACTION LOGIC (The Gatekeeper)
        // =================================================================

        #[ink(message, payable)]
        pub fn buy(&mut self) -> Result<(), Error> {
            let amount = self.env().transferred_value();
            if amount < MIN_TRANSACTION_THRESHOLD { return Err(Error::TransactionTooSmall); }

            let total_tax = amount.checked_mul(E_BUY_TAX_BPS).unwrap() / BPS_DENOMINATOR;
            let team_share = amount.checked_mul(TAX_TEAM_BPS).unwrap() / BPS_DENOMINATOR;
            let prize_share = total_tax.saturating_sub(team_share);

            self.team_tax_accumulated += team_share;
            self.prize_pot_accumulated += prize_share;
            
            self.increment_sequencer()?;
            Ok(())
        }

        fn increment_sequencer(&mut self) -> Result<(), Error> {
            self.current_epoch_counter += 1;
            if self.current_epoch_counter >= EPOCH_SIZE {
                self.env().emit_event(EpochReady {
                    epoch_id: 1, // Logic for ID incrementing goes here
                    prize_pool: self.prize_pot_accumulated,
                });
                self.current_epoch_counter = 0;
            }
            Ok(())
        }

        // =================================================================
        // YIELD DISTRIBUTION (90/10 SPLIT)
        // =================================================================

        #[ink(message)]
        pub fn pull_prize_tax(&mut self) -> Result<U256, Error> {
            if self.paused { return Err(Error::ContractPaused); }
            let caller = self.env().caller();
            
            if Some(caller) != self.sequencer_address {
                return Err(Error::NotSequencerContract);
            }

            let total_prize = self.prize_pot_accumulated;
            let burn_amount = total_prize / U256::from(10); // 10% Burn
            let yield_amount = total_prize.saturating_sub(burn_amount); // 90% Yield

            // Execute Burn to Dead Address
            let dead_address = Address::from([0x0; 20]); // Standard 0x0...dEaD
            self.prize_pot_accumulated = U256::ZERO;
            
            // In ink! v6, transfers return Result
            self.env().transfer(dead_address, burn_amount).map_err(|_| Error::TransferFailed)?;
            self.env().transfer(caller, yield_amount).map_err(|_| Error::TransferFailed)?;

            Ok(yield_amount)
        }

        // =================================================================
        // ADMIN & SAFETY
        // =================================================================

        #[ink(message)]
        pub fn set_sequencer(&mut self, addr: Address) -> Result<(), Error> {
            if self.env().caller() != self.owner { return Err(Error::NotOwner); }
            self.sequencer_address = Some(addr);
            Ok(())
        }

        #[ink(message)]
        pub fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
            if self.env().caller() != self.owner { return Err(Error::NotOwner); }
            self.paused = paused;
            Ok(())
        }
    }
}
