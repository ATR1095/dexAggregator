use crate::cache::{PoolState, PoolType};
use anyhow::{Result, anyhow};

pub struct SwapResult {
    pub amount_out: u64,
    pub fee_paid: u64,
    pub new_sqrt_price: Option<u128>,
}

pub fn compute_swap(
    pool: &PoolState,
    amount_in: u64,
    a_to_b: bool,
) -> Result<SwapResult> {
    match pool.pool_type {
        PoolType::ConstantProduct => compute_amm_swap(pool, amount_in, a_to_b),
        PoolType::ConcentratedLiquidity => compute_clmm_swap(pool, amount_in, a_to_b),
    }
}

fn compute_amm_swap(
    pool: &PoolState,
    amount_in: u64,
    a_to_b: bool,
) -> Result<SwapResult> {
    let (reserve_in, reserve_out) = if a_to_b {
        (pool.reserve_a, pool.reserve_b)
    } else {
        (pool.reserve_b, pool.reserve_a)
    };

    if reserve_in == 0 || reserve_out == 0 {
        return Ok(SwapResult {
            amount_out: 0,
            fee_paid: 0,
            new_sqrt_price: None,
        });
    }

    let amount_in_with_fee = (amount_in as u128) * (10000 - pool.fee_bps as u128);
    let numerator = amount_in_with_fee * (reserve_out as u128);
    let denominator = (reserve_in as u128) * 10000 + amount_in_with_fee;
    
    let amount_out = (numerator / denominator) as u64;
    let fee_paid = amount_in - (amount_in_with_fee / 10000) as u64;

    Ok(SwapResult {
        amount_out,
        fee_paid,
        new_sqrt_price: None,
    })
}

fn compute_clmm_swap(
    pool: &PoolState,
    amount_in: u64,
    a_to_b: bool,
) -> Result<SwapResult> {
    let clmm = pool.clmm_data.as_ref().ok_or_else(|| anyhow!("Missing CLMM data"))?;
    let mut amount_remaining = amount_in as u128;
    let mut amount_out = 0u128;
    let mut current_sqrt_price = clmm.sqrt_price_x64;
    let mut current_liquidity = clmm.liquidity;
    let mut current_tick = clmm.current_tick;

    while amount_remaining > 0 {
        // 1. Find next initialized tick in direction of swap
        let next_tick = find_next_tick(current_tick, clmm.tick_spacing, a_to_b, &clmm.ticks)?;
        let next_sqrt_price = tick_to_price(next_tick);

        // 2. Compute swap step within current tick range
        let (num_in_step, num_out_step, new_sqrt_price) = compute_swap_step(
            current_sqrt_price,
            next_sqrt_price,
            current_liquidity,
            amount_remaining,
            a_to_b,
        )?;

        amount_remaining -= num_in_step;
        amount_out += num_out_step;
        current_sqrt_price = new_sqrt_price;

        if current_sqrt_price == next_sqrt_price {
            // 3. Crossed a tick: update liquidity and move to next tick
            if let Some(tick_info) = clmm.ticks.get(&next_tick) {
                if a_to_b {
                    current_liquidity = (current_liquidity as i128 - tick_info.liquidity_net) as u128;
                } else {
                    current_liquidity = (current_liquidity as i128 + tick_info.liquidity_net) as u128;
                }
            }
            current_tick = if a_to_b { next_tick - 1 } else { next_tick };
        } else {
            break;
        }
    }

    Ok(SwapResult {
        amount_out: amount_out as u64,
        fee_paid: 0, // Simplified
        new_sqrt_price: Some(current_sqrt_price),
    })
}

fn find_next_tick(_tick: i32, _spacing: u16, _a_to_b: bool, _ticks: &dashmap::DashMap<i32, crate::cache::TickInfo>) -> Result<i32> {
    // Logic to find next set bit in tick bitmap
    Ok(0)
}

fn tick_to_price(_tick: i32) -> u128 {
    // 1.0001^tick * 2^64
    0
}

fn compute_swap_step(
    _curr_price: u128,
    _next_price: u128,
    _liquidity: u128,
    _amount_in: u128,
    _a_to_b: bool,
) -> Result<(u128, u128, u128)> {
    // sqrt_price_diff * liquidity / sqrt_price_product
    Ok((0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{PoolState, PoolType};

    #[test]
    fn test_amm_swap() {
        let pool = PoolState {
            id: "pool1".to_string(),
            token_a: "A".to_string(),
            token_b: "B".to_string(),
            symbol_a: "A".to_string(),
            symbol_b: "B".to_string(),
            decimals_a: 9,
            decimals_b: 6,
            reserve_a: 1000_000,
            reserve_b: 2000_000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            dex_label: "test".to_string(),
            clmm_data: None,
        };

        let res = compute_swap(&pool, 100_000, true).unwrap();
        // 100k in -> (100k * 0.997 * 2M) / (1M + 100k*0.997)
        // ~ 199.4k / 1.0997M ~ 181,322
        assert!(res.amount_out > 180_000);
        assert!(res.amount_out < 182_000);
    }
}
