#[ink(message)]
pub fn trigger_victory_lap(&mut self) -> Result<(), Error> {
    let current_block = self.env().block_number();
    let current_timestamp = self.env().block_timestamp();
    
    // TRIPLE-CHECK CONDITIONS [...]
    let (cooldown_met, liquidity_healthy, excess_sufficient, _, _, excess_amount) = 
        self.check_conditions()?;
    
    if !cooldown_met { return Err(Error::CooldownNotComplete); }
    if !liquidity_healthy { return Err(Error::LiquidityUnhealthy); }
    if !excess_sufficient || excess_amount == 0 { return Err(Error::InsufficientExcess); }
    
    // EXECUTE: Market buy 52f with excess QF
    let tokens_bought = self.execute_market_buy(excess_amount)?;
    
    // EXECUTE: Burn the purchased tokens to 0x0
    self.execute_burn(tokens_bought)?; // <-- FIXED VARIABLE NAME
    
    // Update state
    self.last_victory_lap_block = current_block;
    
    let solvency_ratio = excess_amount
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(Error::Overflow)?
        / self.get_liquidity_value()?; // Simplified for example
    
    self.env().emit_event(VictoryLapExecuted {
        block: current_block,
        excess_qf_used: excess_amount,
        tokens_burned: tokens_bought, // <-- CONSISTENT NAMING
        timestamp: current_timestamp,
        solvency_ratio,
    });
    
    Ok(())
}
