#![cfg_attr(not(feature = "std"), no_std)]

/// PROJECT 52F — Token52F.rs
/// Phase 1: Core Token & Eligibility Engine
/// Formal Specification v3.0 (February 2026)
/// Target: QF Network (PolkaVM / ink! v6)

pub mod constants {
    pub const SCALING_FACTOR: u128 = 1_000_000_000_000_000_000u128;
    pub const TOTAL_SUPPLY: u128   = 80_658_175_170 * SCALING_FACTOR;
    
    // Transcendental Taxes
    pub const E_BUY_TAX: u128      = 27_182_818_284_590_452; // e = 2.718%
    pub const PI_SELL_TAX: u128    = 31_415_926_535_897_932; // pi = 3.141%
    
    // Team Sustainability (0.75% both ways)
    pub const TEAM_TAX_BPS: u128   = 7_500_000_000_000_000; 
    pub const SELL_TAX_LIQUIDITY: u128 = 10_000_000_000_000_000; // 1%
    
    // Identity Gates
    pub const HOLDING_GATE_MIN: u128 = 5_200_000 * SCALING_FACTOR; 
    pub const MAX_WALLET_PERFECT: u128 = 600; // 6% (Smallest Perfect Number)
    pub const BPS: u128 = 10_000;
}

#[ink::contract]
mod token52f {
    use crate::constants::*;
    use ink::storage::Mapping;

    #[ink(storage)]
    pub struct Token52F {
        balances: Mapping<AccountId, Balance>,
        eligible_wallets: Mapping<AccountId, bool>,
        tax_exempt: Mapping<AccountId, bool>,
        accumulated_prize_tax: Balance,
        total_supply: Balance,
        owner: AccountId,
        paradox_engine: Option<AccountId>,
        spin_swap_router: Option<AccountId>,
        team_address: AccountId,
    }

    #[ink(event)]
    pub struct EligibilityChanged {
        #[ink(topic)] account: AccountId,
        eligible: bool,
    }

    impl Token52F {
        #[ink(constructor)]
        pub fn new(team: AccountId) -> Self {
            let caller = self.env().caller();
            let mut balances = Mapping::default();
            balances.insert(caller, &TOTAL_SUPPLY);
            
            let mut tax_exempt = Mapping::default();
            tax_exempt.insert(caller, &true);
            tax_exempt.insert(team, &true);

            Self {
                balances,
                eligible_wallets: Mapping::default(),
                tax_exempt,
                accumulated_prize_tax: 0,
                total_supply: TOTAL_SUPPLY,
                owner: caller,
                paradox_engine: None,
                spin_swap_router: None,
                team_address: team,
            }
        }

        #[ink(message)]
        pub fn is_eligible(&self, account: AccountId) -> bool {
            self.eligible_wallets.get(account).unwrap_or(false)
        }

        #[ink(message)]
        pub fn transfer(&mut self, to: AccountId, value: Balance) -> Result<(), Error> {
            let from = self.env().caller();
            self.process_transfer(from, to, value)
        }

        fn process_transfer(&mut self, from: AccountId, to: AccountId, value: Balance) -> Result<(), Error> {
            let from_bal = self.balances.get(from).unwrap_or(0);
            if from_bal < value { return Err(Error::InsufficientBalance); }

            let is_buy = self.spin_swap_router == Some(from);
            let is_sell = self.spin_swap_router == Some(to);
            let net_value = if self.tax_exempt.get(from).unwrap_or(false) || self.tax_exempt.get(to).unwrap_or(false) {
                value
            } else if is_buy {
                self.calculate_buy_tax(value)
            } else if is_sell {
                self.calculate_sell_tax(value)
            } else {
                value
            };

            // Execute Balances
            self.balances.insert(from, &(from_bal - value));
            let to_bal = self.balances.get(to).unwrap_or(0);
            self.balances.insert(to, &(to_bal + net_value));

            // Update Eligibility Flags (Asynchronous to BP Engine)
            self.update_flag(from);
            self.update_flag(to);

            Ok(())
        }

        fn update_flag(&mut self, account: AccountId) {
            let bal = self.balances.get(account).unwrap_or(0);
            let eligible = bal >= HOLDING_GATE_MIN;
            if self.eligible_wallets.get(account).unwrap_or(false) != eligible {
                self.eligible_wallets.insert(account, &eligible);
                self.env().emit_event(EligibilityChanged { account, eligible });
            }
        }

        fn calculate_buy_tax(&mut self, value: Balance) -> Balance {
            let team_share = (value * TEAM_TAX_BPS) / SCALING_FACTOR;
            let total_tax = (value * E_BUY_TAX) / SCALING_FACTOR;
            self.accumulated_prize_tax += total_tax - team_share;
            // Send team_share to team_address...
            value - total_tax
        }

        fn calculate_sell_(&mut self, value: Balance) -> Balance {
            let team_share = (value * TEAM__BPS) / SCALING_FACTOR;
            let total_ = (value * PI_SELL_) / SCALING_FACTOR;
            self.accumulated_prize_ += total_ - team_share - ((value * SELL__LIQUIDITY) / SCALING_FACTOR);
            value - total_
        }
    }
}
