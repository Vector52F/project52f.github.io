#![cfg_attr(not(feature = "std"), no_std)]

/// PROJECT 52F — Token52F.rs
/// Phase 1: Core Token & Eligibility Engine
/// Formal Specification v4.0 (February 2026)
/// Target: QF Network (PolkaVM / ink! v6)

pub mod constants {
    pub const SCALING_FACTOR: u128 = 1_000_000_000_000_000_000u128;
    pub const TOTAL_SUPPLY: u128 = 80_658_175_170 * SCALING_FACTOR;
    
    // Transcendental Taxes (all in QF)
    pub const E_BUY_TAX_BPS: u128 = 271; // 2.718% = 2718 BPS, but we use QF direct
    pub const E_BUY_TAX_QF: u128 = 2_718_000_000_000_000_000; // 2.718% with 18 decimals
    
    pub const PI_SELL_TAX_QF: u128 = 3_141_000_000_000_000_000; // 3.141%
    
    // Team Sustainability (0.75% both ways)
    pub const TEAM_TAX_BPS: u128 = 75; // 0.75% = 75 BPS
    pub const TEAM_TAX_QF: u128 = 750_000_000_000_000_000;
    
    // Liquidity tax (1% to DampenerVault)
    pub const LIQUIDITY_TAX_QF: u128 = 1_000_000_000_000_000_000;
    
    // Prize pool tax (remainder)
    pub const PRIZE_TAX_BUY_QF: u128 = 968_000_000_000_000_000; // 2.718% - 0.75% - 1% = 0.968%
    pub const PRIZE_TAX_SELL_QF: u128 = 1_391_000_000_000_000_000; // 3.141% - 0.75% - 1% = 1.391%
    
    // Identity Gates
    pub const HOLDING_GATE_MIN: u128 = 5_200_000 * SCALING_FACTOR; // 5.2M 52F
    pub const MAX_WALLET_PERFECT: u128 = 600; // 6% (Smallest Perfect Number)
    pub const BPS: u128 = 10_000;
    
    // Team sweep interval: 520,000 blocks (~14.4 hours)
    pub const TEAM_SWEEP_INTERVAL: u32 = 520_000;
}

use ink::contract_ref;
use ink::prelude::vec::Vec;

#[ink::contract]
mod token52f {
    use crate::constants::*;
    use ink::storage::Mapping;

    #[ink(storage)]
    pub struct Token52F {
        balances: Mapping<AccountId, Balance>,
        allowances: Mapping<(AccountId, AccountId), Balance>,
        eligible_wallets: Mapping<AccountId, bool>,
        tax_exempt: Mapping<AccountId, bool>,
        total_supply: Balance,
        owner: AccountId,
        team_address: AccountId,
        birthday_paradox: Option<AccountId>,
        dampener_vault: Option<AccountId>,
        team_accumulated_qf: Balance,
        last_team_sweep: BlockNumber,
        prize_pool_balance: Balance, // 5% of supply held by contract
    }

    #[ink(event)]
    pub struct Transfer {
        #[ink(topic)] from: Option<AccountId>,
        #[ink(topic)] to: Option<AccountId>,
        value: Balance,
    }

    #[ink(event)]
    pub struct Approval {
        #[ink(topic)] owner: AccountId,
        #[ink(topic)] spender: AccountId,
        value: Balance,
    }

    #[ink(event)]
    pub struct EligibilityChanged {
        #[ink(topic)] account: AccountId,
        eligible: bool,
    }

    #[ink(event)]
    pub struct TaxCollected {
        #[ink(topic)] from: AccountId,
        qf_amount: Balance,
        tax_type: u8, // 0=buy, 1=sell
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        InsufficientBalance,
        InsufficientAllowance,
        ZeroTransfer,
        NotOwner,
        NotEligible,
        InvalidAddress,
    }

    impl Token52F {
        #[ink(constructor)]
        pub fn new(team: AccountId) -> Self {
            let caller = Self::env().caller();
            let mut balances = Mapping::default();
            let total = TOTAL_SUPPLY;
            
            // 5% to prize pool (contract holds it)
            let prize_pool = (total * 5) / 100;
            // 95% to deployer
            let deployer_amount = total - prize_pool;
            
            balances.insert(caller, &deployer_amount);
            balances.insert(Self::env().account_id(), &prize_pool);
            
            let mut tax_exempt = Mapping::default();
            tax_exempt.insert(caller, &true);
            tax_exempt.insert(team, &true);

            Self {
                balances,
                allowances: Mapping::default(),
                eligible_wallets: Mapping::default(),
                tax_exempt,
                total_supply: total,
                owner: caller,
                team_address: team,
                birthday_paradox: None,
                dampener_vault: None,
                team_accumulated_qf: 0,
                last_team_sweep: Self::env().block_number(),
                prize_pool_balance: prize_pool,
            }
        }

        // ERC-20 Standard Functions
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
        pub fn approve(&mut self, spender: AccountId, value: Balance) -> Result<(), Error> {
            let owner = self.env().caller();
            self.allowances.insert((owner, spender), &value);
            self.env().emit_event(Approval { owner, spender, value });
            Ok(())
        }

        #[ink(message)]
        pub fn transfer_from(&mut self, from: AccountId, to: AccountId, value: Balance) -> Result<(), Error> {
            let caller = self.env().caller();
            let allowance = self.allowance(from, caller);
            
            if allowance < value {
                return Err(Error::InsufficientAllowance);
            }
            
            self.allowances.insert((from, caller), &(allowance - value));
            self.process_transfer(from, to, value)
        }

        #[ink(message)]
        pub fn transfer(&mut self, to: AccountId, value: Balance) -> Result<(), Error> {
            let from = self.env().caller();
            self.process_transfer(from, to, value)
        }

        // Core transfer logic
        fn process_transfer(&mut self, from: AccountId, to: AccountId, value: Balance) -> Result<(), Error> {
            if value == 0 {
                return Err(Error::ZeroTransfer);
            }

            let from_bal = self.balances.get(from).unwrap_or(0);
            if from_bal < value {
                return Err(Error::InsufficientBalance);
            }

            // Determine if this is buy or sell via router
            let is_buy = self.birthday_paradox == Some(from) || self.dampener_vault == Some(from);
            let is_sell = self.birthday_paradox == Some(to) || self.dampener_vault == Some(to);
            
            // Check tax exemption
            let tax_exempt = self.tax_exempt.get(from).unwrap_or(false) || self.tax_exempt.get(to).unwrap_or(false);
            
            let net_value = if tax_exempt {
                value
            } else if is_buy {
                self.process_buy_tax(from, value)?
            } else if is_sell {
                self.process_sell_tax(from, value)?
            } else {
                value
            };

            // Execute transfer
            self.balances.insert(from, &(from_bal - value));
            let to_bal = self.balances.get(to).unwrap_or(0);
            self.balances.insert(to, &(to_bal + net_value));

            // Update eligibility
            self.update_flag(from);
            self.update_flag(to);
            
            // Check team sweep
            self.check_team_sweep();

            self.env().emit_event(Transfer { from: Some(from), to: Some(to), value: net_value });
            Ok(())
        }

        fn process_buy_tax(&mut self, from: AccountId, value: Balance) -> Result<Balance, Error> {
            // Calculate QF tax (this would be passed as parameter in real integration)
            // For now, assume tax is deducted from separate QF transfer
            let qf_value = value; // Placeholder: actual QF amount
            
            let team_share = (qf_value * TEAM_TAX_BPS) / BPS;
            let liquidity_share = (qf_value * 100) / BPS; // 1%
            let prize_share = qf_value - team_share - liquidity_share - ((qf_value * E_BUY_TAX_BPS) / BPS);
            
            // Accumulate team QF
            self.team_accumulated_qf += team_share;
            
            // Send to DampenerVault (liquidity)
            if let Some(vault) = self.dampener_vault {
                self.env().transfer(vault, liquidity_share).map_err(|_| Error::InvalidAddress)?;
            }
            
            // Send to BirthdayParadox (prize)
            if let Some(paradox) = self.birthday_paradox {
                self.env().transfer(paradox, prize_share).map_err(|_| Error::InvalidAddress)?;
            }
            
            self.env().emit_event(TaxCollected { from, qf_amount: team_share + liquidity_share + prize_share, tax_type: 0 });
            
            // Return net 52F to buyer
            Ok(value - ((value * E_BUY_TAX_BPS) / BPS))
        }

        fn process_sell_tax(&mut self, from: AccountId, value: Balance) -> Result<Balance, Error> {
            let qf_value = value; // Placeholder
            
            let team_share = (qf_value * TEAM_TAX_BPS) / BPS;
            let liquidity_share = (qf_value * 100) / BPS; // 1%
            let prize_share = qf_value - team_share - liquidity_share - ((qf_value * 314) / BPS); // 3.14%
            
            self.team_accumulated_qf += team_share;
            
            if let Some(vault) = self.dampener_vault {
                self.env().transfer(vault, liquidity_share).map_err(|_| Error::InvalidAddress)?;
            }
            
            if let Some(paradox) = self.birthday_paradox {
                self.env().transfer(paradox, prize_share).map_err(|_| Error::InvalidAddress)?;
            }
            
            self.env().emit_event(TaxCollected { from, qf_amount: team_share + liquidity_share + prize_share, tax_type: 1 });
            
            Ok(value - ((value * 314) / BPS))
        }

        fn check_team_sweep(&mut self) {
            let current_block = self.env().block_number();
            if current_block - self.last_team_sweep >= TEAM_SWEEP_INTERVAL as u32 {
                self.sweep_team_qf();
            }
        }

        fn sweep_team_qf(&mut self) {
            if self.team_accumulated_qf > 0 {
                let _ = self.env().transfer(self.team_address, self.team_accumulated_qf);
                self.team_accumulated_qf = 0;
                self.last_team_sweep = self.env().block_number();
            }
        }

        fn update_flag(&mut self, account: AccountId) {
            let bal = self.balances.get(account).unwrap_or(0);
            let eligible = bal >= HOLDING_GATE_MIN;
            let current = self.eligible_wallets.get(account).unwrap_or(false);
            
            if current != eligible {
                self.eligible_wallets.insert(account, &eligible);
                self.env().emit_event(EligibilityChanged { account, eligible });
            }
        }

        // Admin functions
        #[ink(message)]
        pub fn set_birthday_paradox(&mut self, address: AccountId) -> Result<(), Error> {
            self.ensure_owner()?;
            self.birthday_paradox = Some(address);
            Ok(())
        }

        #[ink(message)]
        pub fn set_dampener_vault(&mut self, address: AccountId) -> Result<(), Error> {
            self.ensure_owner()?;
            self.dampener_vault = Some(address);
            Ok(())
        }

        #[ink(message)]
        pub fn set_tax_exempt(&mut self, account: AccountId, exempt: bool) -> Result<(), Error> {
            self.ensure_owner()?;
            self.tax_exempt.insert(account, &exempt);
            Ok(())
        }

        fn ensure_owner(&self) -> Result<(), Error> {
            if self.env().caller() != self.owner {
                return Err(Error::NotOwner);
            }
            Ok(())
        }

        // View functions
        #[ink(message)]
        pub fn is_eligible(&self, account: AccountId) -> bool {
            self.eligible_wallets.get(account).unwrap_or(false)
        }

        #[ink(message)]
        pub fn get_prize_pool_balance(&self) -> Balance {
            self.prize_pool_balance
        }

        #[ink(message)]
        pub fn transfer_from_prize_pool(&mut self, to: AccountId, amount: Balance) -> Result<(), Error> {
            // Only BirthdayParadox can call
            if Some(self.env().caller()) != self.birthday_paradox {
                return Err(Error::NotOwner);
            }
            
            let contract_bal = self.balances.get(self.env().account_id()).unwrap_or(0);
            if contract_bal < amount {
                return Err(Error::InsufficientBalance);
            }
            
            self.balances.insert(self.env().account_id(), &(contract_bal - amount));
            let to_bal = self.balances.get(to).unwrap_or(0);
            self.balances.insert(to, &(to_bal + amount));
            self.prize_pool_balance -= amount;
            
            Ok(())
        }
    }
}
