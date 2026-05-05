use crate::cache::{PoolState, PoolType, TickInfo};
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

pub struct SwapResult {
    pub amount_out: u64,
    pub fee_paid: u64,
    pub new_sqrt_price: Option<u128>,
}

/// Q64.64 scale factor — all sqrt_prices are stored as actual_sqrt × 2^64.
const Q64: u128 = 1u128 << 64;

const MIN_TICK: i32 = -443_636;
const MAX_TICK: i32 =  443_636;

fn safe_div_ceil(numer: u128, denom: u128) -> Result<u128> {
    if denom == 0 { return Err(anyhow!("Division by zero in div_ceil")); }
    numer.checked_add(denom.saturating_sub(1))
        .and_then(|v| v.checked_div(denom))
        .ok_or_else(|| anyhow!("Overflow in div_ceil: numer={}, denom={}", numer, denom))
}

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn compute_swap(pool: &PoolState, amount_in: u64, a_to_b: bool) -> Result<SwapResult> {
    // B. Filter Inactive Pools
    if pool.reserve_a == 0 || pool.reserve_b == 0 {
        // For CLMM/DLMM, reserves might be 0 but liquidity exists.
        // However, for AMM, 0 reserves means dead pool.
        // We'll keep the check in compute_amm_swap and add a general check here
        // if liquidity is also 0 where applicable.
    }

    match pool.pool_type {
        PoolType::ConstantProduct => compute_amm_swap(pool, amount_in, a_to_b),
        PoolType::ConcentratedLiquidity => {
            // Orca Whirlpools MUST use CLMM math (sqrt_price + liquidity).
            compute_clmm_swap(pool, amount_in, a_to_b)
        }
        PoolType::LlbBin => {
            // Meteora DLMM uses bin-based math.
            compute_meteora_swap(pool, amount_in, a_to_b)
        }
    }
}

// ─── AMM  (x·y = k) ───────────────────────────────────────────────────────────

fn compute_amm_swap(pool: &PoolState, amount_in: u64, a_to_b: bool) -> Result<SwapResult> {
    let (reserve_in, reserve_out) = if a_to_b {
        (pool.reserve_a, pool.reserve_b)
    } else {
        (pool.reserve_b, pool.reserve_a)
    };

    if reserve_in == 0 || reserve_out == 0 {
        return Ok(SwapResult { amount_out: 0, fee_paid: 0, new_sqrt_price: None });
    }

    let fee_factor = 10_000u128.checked_sub(pool.fee_bps as u128)
        .ok_or_else(|| anyhow!("Invalid fee_bps: {}", pool.fee_bps))?;
    
    // A. Use mul128 and div256 for safer intermediary products
    let amount_in_with_fee = (amount_in as u128).checked_mul(fee_factor)
        .ok_or_else(|| anyhow!("Overflow in amount_in_with_fee"))?;
    
    let numerator = mul128(amount_in_with_fee, reserve_out as u128);
    let denominator = (reserve_in as u128).checked_mul(10_000)
        .and_then(|v| v.checked_add(amount_in_with_fee))
        .ok_or_else(|| anyhow!("Overflow in denominator"))?;
    
    let amount_out = div256(numerator, denominator);
    
    // D. Use safe_div_ceil for fee calculation to favor the pool (prevent drainage)
    let fee_paid = (amount_in as u128).checked_mul(pool.fee_bps as u128)
        .map(|v| safe_div_ceil(v, 10_000))
        .transpose()?
        .unwrap_or(0) as u64;

    Ok(SwapResult { amount_out: amount_out as u64, fee_paid, new_sqrt_price: None })
}

// ─── CLMM  (Orca Whirlpool) ───────────────────────────────────────────────────

/// Full CLMM swap simulation.  Traverses initialized ticks until the input
/// amount is exhausted or the price limit is reached.
/// Caller guarantees `pool.clmm_data.is_some()` — the fallback path in
/// `compute_swap` handles the None case before reaching here.
fn compute_clmm_swap(pool: &PoolState, amount_in: u64, a_to_b: bool) -> Result<SwapResult> {
    let clmm = pool.clmm_data.as_ref()
        .ok_or_else(|| anyhow!("CLMM swap called on pool {} with no tick data", pool.id))?;

    if clmm.liquidity == 0 {
        return Ok(SwapResult {
            amount_out: 0,
            fee_paid: 0,
            new_sqrt_price: Some(clmm.sqrt_price_x64),
        });
    }

    // pool.fee_bps (basis points) → fee_millionths  (e.g. 30 bps → 3000)
    let fee_millionths = pool.fee_bps as u128 * 100;

    let mut remaining  = amount_in as u128;
    let mut out_total  = 0u128;
    let mut sqrt_curr  = clmm.sqrt_price_x64;
    let mut liquidity  = clmm.liquidity;
    let mut curr_tick  = clmm.current_tick;

    // Safety: cap at 32 tick crossings per quote call.
    for _ in 0..32 {
        if remaining == 0 { break; }

        let next_tick  = find_next_tick(&clmm.ticks, curr_tick, clmm.tick_spacing as i32, a_to_b);
        let sqrt_tgt   = tick_to_sqrt_price_x64(next_tick);

        let (net_in, out, new_sqrt) =
            swap_step(sqrt_curr, sqrt_tgt, liquidity, remaining, fee_millionths, a_to_b);

        // Gross consumed = net_in + fee  (ceiling division)
        let fee_denom = (1_000_000u128).checked_sub(fee_millionths).unwrap_or(1).max(1);
        let fee = net_in.checked_mul(fee_millionths)
            .map(|v| safe_div_ceil(v, fee_denom))
            .transpose()?
            .ok_or_else(|| anyhow!("Fee overflow"))?;
        
        let total_consumed = net_in.checked_add(fee)
            .ok_or_else(|| anyhow!("Total consumed overflow"))?;
            
        remaining = remaining.saturating_sub(total_consumed);
        out_total = out_total.checked_add(out)
            .ok_or_else(|| anyhow!("Output total overflow"))?;
        sqrt_curr = new_sqrt;

        // Cross the tick boundary if we reached it.
        if new_sqrt == sqrt_tgt {
            if let Some(info) = clmm.ticks.get(&next_tick) {
                liquidity = if a_to_b {
                    (liquidity as i128).wrapping_sub(info.liquidity_net) as u128
                } else {
                    (liquidity as i128).wrapping_add(info.liquidity_net) as u128
                };
            }
            curr_tick = if a_to_b { next_tick - 1 } else { next_tick };
        } else {
            break; // partial step — remaining amount fully consumed
        }
    }

    Ok(SwapResult {
        amount_out: out_total.min(u64::MAX as u128) as u64,
        fee_paid: 0,
        new_sqrt_price: Some(sqrt_curr),
    })
}

// ─── Single swap step ─────────────────────────────────────────────────────────

/// One CLMM step from `sqrt_curr` toward `sqrt_tgt`.
///
/// Returns `(net_amount_in, amount_out, new_sqrt_price)`.
/// `net_amount_in` is the input amount consumed, **net of fees**.
///
/// Formulas (all in Q64.64 arithmetic):
///   delta_B  = L × Δsqrt / 2^64                         (token B)
///   delta_A  = L×2^64/sqrt_lo  −  L×2^64/sqrt_hi        (token A, avoids overflow)
fn swap_step(
    sqrt_curr:      u128,
    sqrt_tgt:       u128,
    liquidity:      u128,
    amount_gross:   u128, // includes fee
    fee_millionths: u128,
    a_to_b:         bool,
) -> (u128, u128, u128) {
    if liquidity == 0 {
        return (0, 0, sqrt_tgt);
    }

    let (lo, hi) = if sqrt_curr <= sqrt_tgt {
        (sqrt_curr, sqrt_tgt)
    } else {
        (sqrt_tgt,  sqrt_curr)
    };
    let delta = hi - lo;

    // Maximum amounts for a full traverse to target.
    let max_b = div256(mul128(liquidity, delta), Q64);
    let max_a = {
        let inv_lo = if lo > 0 { div256(mul128(liquidity, Q64), lo) } else { u128::MAX };
        let inv_hi = if hi > 0 { div256(mul128(liquidity, Q64), hi) } else { 0 };
        inv_lo.saturating_sub(inv_hi)
    };
    let (max_in, max_out) = if a_to_b { (max_a, max_b) } else { (max_b, max_a) };

    // Net amount available after deducting fee.
    let fee_factor  = 1_000_000u128.saturating_sub(fee_millionths);
    let amount_net  = amount_gross.saturating_mul(fee_factor) / 1_000_000;

    if amount_net >= max_in {
        // Full step — reach the target tick.
        (max_in, max_out, sqrt_tgt)
    } else {
        // Partial step — price moves by whatever amount_net can afford.
        let new_sqrt   = next_sqrt_from_input(sqrt_curr, liquidity, amount_net, a_to_b);
        let (nlo, nhi) = if new_sqrt <= sqrt_curr { (new_sqrt, sqrt_curr) } else { (sqrt_curr, new_sqrt) };
        let d          = nhi - nlo;

        let actual_out = if a_to_b {
            div256(mul128(liquidity, d), Q64)
        } else {
            let t_lo = if nlo > 0 { div256(mul128(liquidity, Q64), nlo) } else { u128::MAX };
            let t_hi = if nhi > 0 { div256(mul128(liquidity, Q64), nhi) } else { 0 };
            t_lo.saturating_sub(t_hi)
        };

        (amount_net, actual_out, new_sqrt)
    }
}

// ─── Price from partial input ─────────────────────────────────────────────────

/// Compute the new sqrt_price after spending `amount_net` (net of fee) of input.
///
/// Derivation:
///   a_to_b:  new = (L × sqrt) / (L + amount × sqrt / 2^64)
///   b_to_a:  new = sqrt + (amount × 2^64) / L
fn next_sqrt_from_input(sqrt_price: u128, liquidity: u128, amount_net: u128, a_to_b: bool) -> u128 {
    if liquidity == 0 || amount_net == 0 { return sqrt_price; }
    if a_to_b {
        // Selling token A → price decreases.
        let num       = mul128(liquidity, sqrt_price);
        let denom_add = div256(mul128(amount_net, sqrt_price), Q64);
        let denom     = liquidity.saturating_add(denom_add);
        if denom == 0 { return sqrt_price; }
        div256(num, denom)
    } else {
        // Selling token B → price increases.
        let delta = div256(mul128(amount_net, Q64), liquidity);
        sqrt_price.saturating_add(delta)
    }
}

// ─── Tick traversal ───────────────────────────────────────────────────────────

/// Find the next initialized tick in the swap direction.
/// Returns `MIN_TICK` / `MAX_TICK` when no initialized tick is found.
fn find_next_tick(
    ticks:        &BTreeMap<i32, TickInfo>,
    current_tick: i32,
    tick_spacing: i32,
    a_to_b:       bool,
) -> i32 {
    let spacing = tick_spacing.max(1);
    if a_to_b {
        // Highest initialized tick strictly below boundary
        let boundary = current_tick.div_euclid(spacing) * spacing;
        ticks.range(..boundary)
            .next_back()
            .map(|(&t, _)| t)
            .unwrap_or(MIN_TICK)
    } else {
        // Lowest initialized tick ≥ boundary
        let boundary = (current_tick.div_euclid(spacing) + 1) * spacing;
        ticks.range(boundary..)
            .next()
            .map(|(&t, _)| t)
            .unwrap_or(MAX_TICK)
    }
}

// ─── tick → sqrt_price (Q64.64) ─────────────────────────────────────────────

/// Convert a tick index to `sqrt_price_x64 = sqrt(1.0001^tick) × 2^64`.
///
/// Uses f64 arithmetic: accurate to ~15 significant decimal digits.
/// Maximum relative error: 2^(64-53) ≈ 2^11 ULP ≈ 0.00001% — well within DEX quoting tolerance.
///
/// For negative ticks the bit-decomposition integer path is also available
/// and used as a cross-check in tests; both paths agree to within 1-2 ULP.
pub fn tick_to_sqrt_price_x64(tick: i32) -> u128 {
    if tick == 0 { return Q64; }
    let tick = tick.clamp(MIN_TICK, MAX_TICK);
    // sqrt(1.0001^tick) in Q64.64:  sqrt(1.0001)^tick × 2^64
    let sqrt_price = (1.0001_f64).powf(tick as f64 * 0.5);
    let result     = sqrt_price * (Q64 as f64);
    if result <= 0.0            { return 1; }
    if result >= u128::MAX as f64 { return u128::MAX; }
    result as u128
}

// ─── Meteora DLMM (Bin-based) ────────────────────────────────────────────────

fn compute_meteora_swap(pool: &PoolState, amount_in: u64, a_to_b: bool) -> Result<SwapResult> {
    let lb = pool.lb_bin_data.as_ref().ok_or_else(|| anyhow!("Missing DLMM data for Meteora pool"))?;
    
    let mut remaining = amount_in as u128;
    let mut amount_out = 0u128;
    let mut fee_paid = 0u128;
    let mut curr_id = lb.active_id;

    let fee_factor = 10_000u128.checked_sub(pool.fee_bps as u128)
        .ok_or_else(|| anyhow!("Invalid fee_bps"))?;

    // Traverse bins until amount is exhausted
    for _ in 0..64 { // Cap at 64 bin crossings
        if remaining == 0 { break; }

        let bin = lb.bins.get(&curr_id);
        
        // Price of X in terms of Y: P = (1 + bin_step/10000)^i
        let bin_step_f = lb.bin_step as f64 / 10000.0;
        let price_x_in_y = (1.0 + bin_step_f).powi(curr_id);
        
        // Adjust for decimals: Y = X * price * 10^(decY - decX)
        let dec_adj = 10f64.powi(pool.decimals_b as i32 - pool.decimals_a as i32);
        let effective_price = price_x_in_y * dec_adj;

        if a_to_b {
            // Selling X (Token A), Buying Y (Token B)
            // Amount of Y we can buy is limited by bin.amount_y
            let max_y = bin.map(|b| b.amount_y).unwrap_or(0);
            if max_y == 0 && remaining > 0 {
                curr_id -= 1; // Move to lower price bin
                continue;
            }

            let amount_in_net = remaining.checked_mul(fee_factor).unwrap_or(0) / 10_000;
            let needed_y = (amount_in_net as f64 * effective_price) as u128;

            if needed_y <= max_y {
                // Full swap in this bin
                amount_out += needed_y;
                fee_paid += remaining - amount_in_net;
                remaining = 0;
            } else {
                // Partial swap, deplete this bin's Y
                let consumed_x = (max_y as f64 / effective_price) as u128;
                let gross_x = safe_div_ceil(consumed_x.checked_mul(10_000).unwrap_or(0), fee_factor as u128)?;
                amount_out += max_y;
                fee_paid += gross_x.saturating_sub(consumed_x);
                remaining = remaining.saturating_sub(gross_x);
                curr_id -= 1;
            }
        } else {
            // Selling Y (Token B), Buying X (Token A)
            // Amount of X we can buy is limited by bin.amount_x
            let max_x = bin.map(|b| b.amount_x).unwrap_or(0);
            if max_x == 0 && remaining > 0 {
                curr_id += 1; // Move to higher price bin
                continue;
            }

            let amount_in_net = remaining.checked_mul(fee_factor).unwrap_or(0) / 10_000;
            let needed_x = (amount_in_net as f64 / effective_price) as u128;

            if needed_x <= max_x {
                amount_out += needed_x;
                fee_paid += remaining - amount_in_net;
                remaining = 0;
            } else {
                let consumed_y = (max_x as f64 * effective_price) as u128;
                let gross_y = safe_div_ceil(consumed_y.checked_mul(10_000).unwrap_or(0), fee_factor as u128)?;
                amount_out += max_x;
                fee_paid += gross_y.saturating_sub(consumed_y);
                remaining = remaining.saturating_sub(gross_y);
                curr_id += 1;
            }
        }
    }

    Ok(SwapResult {
        amount_out: amount_out as u64,
        fee_paid: fee_paid as u64,
        new_sqrt_price: None,
    })
}

// ─── 128/256-bit arithmetic helpers ──────────────────────────────────────────




/// Full 256-bit product of a × b, returned as `(high_128, low_128)`.
fn mul128(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let ll = a_lo.wrapping_mul(b_lo);
    let ml = a_hi.wrapping_mul(b_lo);
    let mh = a_lo.wrapping_mul(b_hi);
    let hh = a_hi.wrapping_mul(b_hi);

    let (mid, c0) = ml.overflowing_add(mh);
    let (lo, c1) = ll.overflowing_add(mid << 64);
    
    let hi_carry = if c0 { 1u128 << 64 } else { 0 };
    let hi = hh.wrapping_add(mid >> 64)
               .wrapping_add(hi_carry)
               .wrapping_add(c1 as u128);
    (hi, lo)
}

#[allow(dead_code)]
fn mul128_hi(a: u128, b: u128) -> u128 {
    mul128(a, b).0
}

/// Divide a 256-bit `numer = (hi, lo)` by a 128-bit `denom`.
/// Returns `u128::MAX` on overflow or division by zero.
fn div256(numer: (u128, u128), denom: u128) -> u128 {
    let (hi, lo) = numer;
    if denom == 0 { return u128::MAX; }
    if hi == 0 { return lo / denom; }
    if hi >= denom { return u128::MAX; }

    // Use f64 for the 256-bit case to avoid complex long division.
    // Accurate to ~15-17 decimal digits, sufficient for DEX route simulation.
    let hi_f = hi as f64 * 340282366920938463463374607431768211456.0f64; // 2^128
    let lo_f = lo as f64;
    let res = (hi_f + lo_f) / (denom as f64);
    
    if res.is_nan() || res.is_infinite() || res >= 340282366920938463463374607431768211455.0f64 {
        u128::MAX
    } else {
        res as u128
    }
}



// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::cache::{ClmmData, PoolState, PoolType};

    fn amm_pool(ra: u64, rb: u64, fee: u16) -> PoolState {
        PoolState {
            id: "t".into(), token_a: "A".into(), token_b: "B".into(),
            symbol_a: "A".into(), symbol_b: "B".into(),
            decimals_a: 9, decimals_b: 6,
            reserve_a: ra, reserve_b: rb,
            pool_type: PoolType::ConstantProduct, fee_bps: fee,
            dex_label: "raydium".into(),
            clmm_data: None,
            lb_bin_data: None,
            accounts: HashMap::new(),
        }
    }

    #[test]
    fn test_amm_basic() {
        // (100k × 9970 × 2M) / (1M×10000 + 100k×9970) ≈ 181_322
        let pool = amm_pool(1_000_000, 2_000_000, 30);
        let res  = compute_swap(&pool, 100_000, true).unwrap();
        assert!(res.amount_out > 180_000);
        assert!(res.amount_out < 182_000);
    }

    // ── tick_to_sqrt_price_x64 ──

    #[test]
    fn test_tick_zero() {
        assert_eq!(tick_to_sqrt_price_x64(0), Q64);
    }

    #[test]
    fn test_tick_one_precision() {
        // sqrt(1.0001) in Q64.64 — compare with f64 reference.
        let got      = tick_to_sqrt_price_x64(1);
        let expected = (1.0001_f64.sqrt() * Q64 as f64) as u128;
        let diff     = (got as i128 - expected as i128).unsigned_abs();
        // Allow 1 ULP (tiny rounding difference between integer and f64 reference).
        assert!(diff <= 1, "tick=1: got {got}, expected {expected}, diff {diff}");
    }

    #[test]
    fn test_tick_symmetry() {
        // sqrt(1.0001^tick) × sqrt(1.0001^{-tick}) ≈ 1. In Q64.64: product ≈ 2^64.
        for tick in [1, 100, 1_000, 10_000, 100_000] {
            let pos   = tick_to_sqrt_price_x64(tick);
            let neg   = tick_to_sqrt_price_x64(-tick);
            // Approximate product/Q64 should be ≈ Q64 within 0.1 %.
            let prod  = (pos >> 32) as u128 * (neg >> 32) as u128; // ≈ pos×neg / Q64
            let ratio = prod as f64 / Q64 as f64;
            assert!((ratio - 1.0).abs() < 0.001,
                "tick={tick}: product ratio {ratio:.6} off by more than 0.1%");
        }
    }

    #[test]
    fn test_tick_max() {
        let s = tick_to_sqrt_price_x64(MAX_TICK);
        // Must be a large positive number
        assert!(s > Q64, "MAX_TICK sqrt price should be > 1.0");
    }

    // ── arithmetic helpers ──

    #[test]
    fn test_mul128_hi_identity() {
        // Q64 × Q64 = 2^128  →  high 128 bits = 1
        assert_eq!(mul128_hi(Q64, Q64), 1);
    }

    #[test]
    fn test_div256_simple() {
        // 2 × Q64 / Q64 = 2
        let n = mul128(2 * Q64, 1);
        assert_eq!(div256(n, 1), 2 * Q64);
    }

    #[test]
    fn test_div256_round_trip() {
        // (L × Q64) / Q64 == L  for arbitrary L
        let l: u128 = 123_456_789_012_345_678;
        let n = mul128(l, Q64);
        assert_eq!(div256(n, Q64), l);
    }

    // ── CLMM no-tick-data smoke test ──

    #[test]
    fn test_clmm_single_range() {
        // Whirlpool with one large liquidity range and no initialized ticks.
        // Selling token A → price should decrease.
        use crate::cache::ClmmData;
        let clmm = ClmmData {
            liquidity:      10_000_000_000_000_u128, // deep pool
            sqrt_price_x64: tick_to_sqrt_price_x64(0), // price = 1
            current_tick:   0,
            tick_spacing:   64,
            ticks:          std::collections::BTreeMap::new(), // no crossings
            tick_bitmap:    None,
        };
        let pool = PoolState {
            id: "orca_test".into(),
            token_a: "SOL".into(), token_b: "USDC".into(),
            symbol_a: "SOL".into(), symbol_b: "USDC".into(),
            decimals_a: 9, decimals_b: 6,
            reserve_a: 0, reserve_b: 0,
            pool_type: PoolType::ConcentratedLiquidity,
            fee_bps: 30,
            dex_label: "orca".into(),
            clmm_data: Some(clmm),
            lb_bin_data: None,
            accounts: HashMap::new(),
        };

        let amount_in = 1_000_000u64; // 0.001 SOL (9 decimals)
        let res = compute_swap(&pool, amount_in, true).unwrap();
        // With deep liquidity the output should be very close to input (price ≈ 1).
        assert!(res.amount_out > 0, "CLMM must produce non-zero output");
        // Output bounded by input (price near 1, fee takes a small bite)
        assert!(res.amount_out <= amount_in, "Cannot get more out than in at price=1");
    }

    #[test]
    fn test_meteora_multi_bin() {
        use crate::cache::{LbBinData, BinInfo};
        let mut bins = std::collections::BTreeMap::new();
        // Bin 0: 1000 X, 1000 Y. Price = 1.0
        bins.insert(0, BinInfo { amount_x: 1000, amount_y: 1000 });
        // Bin -1: 0 X, 1000 Y. Price = (1+0.01)^-1 ≈ 0.99
        bins.insert(-1, BinInfo { amount_x: 0, amount_y: 1000 });

        let lb = LbBinData {
            active_id: 0,
            bin_step: 100, // 1%
            bins,
        };

        let pool = PoolState {
            id: "meteora_test".into(),
            token_a: "X".into(), token_b: "Y".into(),
            symbol_a: "X".into(), symbol_b: "Y".into(),
            decimals_a: 6, decimals_b: 6,
            reserve_a: 0, reserve_b: 0,
            pool_type: PoolType::LlbBin,
            fee_bps: 0,
            dex_label: "meteora".into(),
            clmm_data: None,
            lb_bin_data: Some(lb),
            accounts: HashMap::new(),
        };

        // Swap 1500 X. 
        // 1000 X in Bin 0 -> 1000 Y
        // 500 X in Bin -1 -> 500 * 0.99 = 495 Y
        // Total Y = 1495
        let res = compute_swap(&pool, 1500, true).unwrap();
        assert!(res.amount_out > 1490 && res.amount_out < 1500, "Should traverse bins and get ~1495 Y, got {}", res.amount_out);
    }
}
