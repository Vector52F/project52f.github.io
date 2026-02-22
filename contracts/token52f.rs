#![cfg_attr(not(feature = "std"), no_std)]

use ink::prelude::vec::Vec;
use ink::storage::Mapping;

pub type QFBalance = U256;
pub type Balance = u128;

pub mod constants {
    pub const SCALING_FACTOR: u128 = 1_000_000_000_000_000_000u128;
    pub const TOTAL_SUPPLY: u128 = 80_658_175_170 * SCALING_FACTOR;
    
    pub const E_BUY_TAX_BPS: u128 = 2718;
    pub const PI_SELL_TAX_BPS: u128 = 3141;
    pub const TEAM_TAX_BPS: u128 = 75;
    pub const LIQUIDITY_TAX_BPS: u128 = 100;
    
    pub const GATE_ENTRY: Balance = 5_720_000 * SCALING_FACTOR;
    pub const GATE_EXIT: Balance = 4_680_000 * SCALING_FACTOR;
    pub const GATE_CENTER: Balance = 5_200_000 * SCALING_FACTOR;
    
    pub const BPS: u128 = 10_000;
    pub const TEAM_SWEEP_INTERVAL: u32 = 520_000;
    
    pub const BURN_ADDRESS: [u8; 32] = [0u8; 32];
}

use crate::constants::*;

#[ink::contract]
mod token52f {
    use super::*;

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
        victory_lap_satellite: Option<AccountId>,
        last_team_sweep: BlockNumber,
        prize_pool_balance: Balance,
        total_burned: Balance,
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
        balance: Balance,
    }

    #[ink(event)]
    pub struct TaxCollected {
        #[ink(topic)] from: AccountId,
        qf_amount: QFBalance,
        tax_type: u8,
    }

    #[ink(event)]
    pub struct TeamSweep {
        amount: QFBalance,
        block: BlockNumber,
    }

    #[ink(event)]
    pub struct Burn {
        #[ink(topic)] from: AccountId,
        amount: Balance,
        new_total_supply: Balance,
        total_burned: Balance,
    }

    #[derive(Debug, PartialEq, Eq, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        InsufficientBalance,
        InsufficientAllowance,
        InsufficientQF,
        ZeroTransfer,
        NotOwner,
        NotEligible,
        InvalidAddress,
        MathsError,
        TransferFailed,
        NotAuthorized,
    }

    impl Token52F {
        #[ink(constructor, payable)]
        pub fn new(team: AccountId) -> Self {
            let caller = Self::env().caller();
            let mut balances = Mapping::default();
            let total = TOTAL_SUPPLY;
            
            let prize_pool = (total * 5) / 100;
            let deployer_amount = total - prize_pool;
            
            balances.insert(caller, &deployer_amount);
            balances.insert(Self::env().account_id(), &prize_pool);

            let mut tax_exempt = Mapping::default();
            tax_exempt.insert(caller, &true);
            tax_exempt.insert(team, &true);

            let _initial_qf = Self::env().transferred_value();

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
                victory_lap_satellite: None,
                last_team_sweep: Self::env().block_number(),
                prize_pool_balance: prize_pool,
                total_burned: 0,
            }
        }

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

        #[ink(message, payable)]
        pub fn buy(&mut self, to: AccountId, value: Balance) -> Result<(), Error> {
            let from = self.env().caller();
            let qf_received = self.env().transferred_value();
            
            let expected_tax = self.calculate_buy_tax_qf(value);
            if qf_received < expected_tax {
                return Err(Error::InsufficientQF);
            }
            
            self.distribute_buy_tax(qf_received)?;
            self.process_transfer(from, to, value)?;
            self.check_team_sweep();
            
            Ok(())
        }

        #[ink(message, payable)]
        pub fn sell(&mut self, value: Balance) -> Result<QFBalance, Error> {
            let from = self.env().caller();
            
            let from_bal = self.balances.get(from).unwrap_or(0);
            if from_bal < value {
                return Err(Error::InsufficientBalance);
            }
            
            let qf_output = self.calculate_52f_to_qf(value);
            let tax = (qf_output * QFBalance::from(PI_SELL_TAX_BPS)) / QFBalance::from(BPS);
            let net_qf = qf_output.checked_sub(tax).ok_or(Error::MathsError)?;
            
            self.distribute_sell_tax(tax)?;
            self.balances.insert(from, &(from_bal - value));
            self.env().transfer(from, net_qf).map_err(|_| Error::TransferFailed)?;
            
            self.update_eligibility(from);
            self.check_team_sweep();
            
            Ok(net_qf)
        }

        fn process_transfer(&mut self, from: AccountId, to: AccountId, value: Balance) -> Result<(), Error> {
            if value == 0 {
                return Err(Error::ZeroTransfer);
            }

            let from_bal = self.balances.get(from).unwrap_or(0);
            if from_bal < value {
                return Err(Error::InsufficientBalance);
            }

            let tax_exempt = self.tax_exempt.get(from).unwrap_or(false) || self.tax_exempt.get(to).unwrap_or(false);
            let net_value = if tax_exempt { value } else { value };

            self.balances.insert(from, &(from_bal - value));
            let to_bal = self.balances.get(to).unwrap_or(0);
            self.balances.insert(to, &(to_bal + net_value));

            self.update_eligibility(from);
            self.update_eligibility(to);

            self.env().emit_event(Transfer { from: Some(from), to: Some(to), value: net_value });
            Ok(())
        }

        fn update_eligibility(&mut self, account: AccountId) {
            let bal = self.balances.get(account).unwrap_or(0);
            let is_currently_eligible = self.eligible_wallets.get(account).unwrap_or(false);
            
            let should_be_eligible = if !is_currently_eligible {
                bal >= GATE_ENTRY
            } else {
                bal >= GATE_EXIT
            };
            
            if is_currently_eligible != should_be_eligible {
                self.eligible_wallets.insert(account, &should_be_eligible);
                self.env().emit_event(EligibilityChanged { 
                    account, 
                    eligible: should_be_eligible,
                    balance: bal,
                });
            }
        }

        fn calculate_buy_tax_qf(&self, value: Balance) -> QFBalance {
            let value_u256 = QFBalance::from(value);
            (value_u256 * QFBalance::from(E_BUY_TAX_BPS)) / QFBalance::from(BPS)
        }

        fn calculate_52f_to_qf(&self, value: Balance) -> QFBalance {
            QFBalance::from(value)
        }

        fn distribute_buy_tax(&mut self, total_tax: QFBalance) -> Result<(), Error> {
            let team_share = (total_tax * QFBalance::from(TEAM_TAX_BPS)) / QFBalance::from(BPS);
            let liquidity_share = (total_tax * QFBalance::from(LIQUIDITY_TAX_BPS)) / QFBalance::from(BPS);
            let prize_share = total_tax
                .checked_sub(team_share)
                .and_then(|r| r.checked_sub(liquidity_share))
                .ok_or(Error::MathsError)?;

            if let Some(vault) = self.dampener_vault {
                self.env().transfer(vault, liquidity_share).map_err(|_| Error::TransferFailed)?;
            }

            if let Some(paradox) = self.birthday_paradox {
                self.env().transfer(paradox, prize_share).map_err(|_| Error::TransferFailed)?;
            }

            self.env().emit_event(TaxCollected { 
                from: self.env().caller(), 
                qf_amount: total_tax, 
                tax_type: 0 
            });

            Ok(())
        }

        fn distribute_sell_tax(&mut self, total_tax: QFBalance) -> Result<(), Error> {
            let team_share = (total_tax * QFBalance::from(TEAM_TAX_BPS)) / QFBalance::from(BPS);
            let liquidity_share = (total_tax * QFBalance::from(LIQUIDITY_TAX_BPS)) / QFBalance::from(BPS);
            let prize_share = total_tax
                .checked_sub(team_share)
                .and_then(|r| r.checked_sub(liquidity_share))
                .ok_or(Error::MathsError)?;

            if let Some(vault) = self.dampener_vault {
                self.env().transfer(vault, liquidity_share).map_err(|_| Error::TransferFailed)?;
            }

            if let Some(paradox) = self.birthday_paradox {
                self.env().transfer(paradox, prize_share).map_err(|_| Error::TransferFailed)?;
            }

            self.env().emit_event(TaxCollected { 
                from: self.env().caller(), 
                qf_amount: total_tax, 
                tax_type: 1 
            });

            Ok(())
        }

        fn check_team_sweep(&mut self) {
            let current_block = self.env().block_number();
            if current_block - self.last_team_sweep >= TEAM_SWEEP_INTERVAL {
                self.sweep_team_qf();
            }
        }

        fn sweep_team_qf(&mut self) {
            let contract_balance = self.env().balance();
            let reserve = QFBalance::from(1_000_000_000_000_000_000u128);
            
            if contract_balance > reserve {
                let sweep_amount = contract_balance - reserve;
                let _ = self.env().transfer(self.team_address, sweep_amount);
                self.last_team_sweep = self.env().block_number();
                
                self.env().emit_event(TeamSweep { 
                    amount: sweep_amount, 
                    block: self.env().block_number() 
                });
            }
        }

        // VICTORY LAP: Burn function for satellite
        #[ink(message)]
        pub fn burn_from_satellite(&mut self, amount: Balance) -> Result<(), Error> {
            // Only VictoryLapSatellite can call
            if Some(self.env().caller()) != self.victory_lap_satellite {
                return Err(Error::NotAuthorized);
            }
            
            // Burn from contract's prize pool balance
            let contract_addr = self.env().account_id();
            let contract_bal = self.balances.get(contract_addr).unwrap_or(0);
            
            if contract_bal < amount {
                return Err(Error::InsufficientBalance);
            }
            
            // True burn: reduce supply, don't credit anywhere
            self.balances.insert(contract_addr, &(contract_bal - amount));
            self.total_supply -= amount;
            self.total_burned += amount;
            
            self.env().emit_event(Burn {
                from: contract_addr,
                amount,
                new_total_supply: self.total_supply,
                total_burned: self.total_burned,
            });
            
            Ok(())
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
        pub fn set_victory_lap_satellite(&mut self, address: AccountId) -> Result<(), Error> {
            self.ensure_owner()?;
            self.victory_lap_satellite = Some(address);
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
        pub fn get_eligibility_thresholds(&self) -> (Balance, Balance, Balance) {
            (GATE_EXIT, GATE_CENTER, GATE_ENTRY)
        }

        #[ink(message)]
        pub fn get_prize_pool_balance(&self) -> Balance {
            self.prize_pool_balance
        }

        #[ink(message)]
        pub fn transfer_from_prize_pool(&mut self, to: AccountId, amount: Balance) -> Result<(), Error> {
            if Some(self.env().caller()) != self.birthday_paradox {
                return Err(Error::NotOwner);
            }
            
            let contract_addr = self.env().account_id();
            let contract_bal = self.balances.get(contract_addr).unwrap_or(0);
            if contract_bal < amount {
                return Err(Error::InsufficientBalance);
            }
            
            self.balances.insert(contract_addr, &(contract_bal - amount));
            let to_bal = self.balances.get(to).unwrap_or(0);
            self.balances.insert(to, &(to_bal + amount));
            self.prize_pool_balance -= amount;
            
            Ok(())
        }

        #[ink(message)]
        pub fn get_contract_qf_balance(&self) -> QFBalance {
            self.env().balance()
        }

        #[ink(message)]
        pub fn get_burn_stats(&self) -> (Balance, Balance) {
            (self.total_burned, self.total_supply)
        }
    }
}
