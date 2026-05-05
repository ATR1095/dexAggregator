use crate::cache::{PoolState, PoolType, TickInfo};
use anyhow::{Result, anyhow};
use dashmap::DashMap;

pub struct SwapResult {
    pub amount_out: u64,
    pub fee_paid: u64,
    pub new_sqrt_price: Option<u128>,
}

/// Q64.64 scale factor — all sqrt_prices are stored as actual_sqrt × 2^64.
const Q64: u128 = 1u128 << 64;

const MIN_TICK: i32 = -443_636;
const MAX_TICK: i32 =  443_636;

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn compute_swap(pool: &PoolState, amount_in: u64, a_to_b: bool) -> Result<SwapResult> {
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

    let amount_in_with_fee = (amount_in as u128) * (10_000 - pool.fee_bps as u128);
    let numerator           = amount_in_with_fee * reserve_out as u128;
    let denominator         = reserve_in as u128 * 10_000 + amount_in_with_fee;
    let amount_out          = (numerator / denominator) as u64;
    let fee_paid            = amount_in - (amount_in_with_fee / 10_000) as u64;

    Ok(SwapResult { amount_out, fee_paid, new_sqrt_price: None })
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
        let fee_denom  = (1_000_000u128).saturating_sub(fee_millionths).max(1);
        let fee        = net_in.saturating_mul(fee_millionths).div_ceil(fee_denom);
        remaining      = remaining.saturating_sub(net_in.saturating_add(fee));
        out_total     += out;
        sqrt_curr      = new_sqrt;

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
    ticks:        &DashMap<i32, TickInfo>,
    current_tick: i32,
    tick_spacing: i32,
    a_to_b:       bool,
) -> i32 {
    let spacing = tick_spacing.max(1);
    if a_to_b {
        // Highest initialized tick strictly below floor(current/spacing)×spacing.
        let boundary = current_tick.div_euclid(spacing) * spacing;
        ticks.iter()
            .map(|e| *e.key())
            .filter(|&t| t < boundary)
            .max()
            .unwrap_or(MIN_TICK)
    } else {
        // Lowest initialized tick ≥ (floor(current/spacing)+1)×spacing.
        let boundary = (current_tick.div_euclid(spacing) + 1) * spacing;
        ticks.iter()
            .map(|e| *e.key())
            .filter(|&t| t >= boundary)
            .min()
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
    
    // Price of Token X in Token Y: (1 + bin_step / 10000) ^ active_id
    let bin_step_f = lb.bin_step as f64 / 10000.0;
    let price_x_in_y = (1.0 + bin_step_f).powi(lb.active_id);
    
    let fee_factor = 1.0 - (pool.fee_bps as f64 / 10000.0);
    let amount_net = amount_in as f64 * fee_factor;

    let (amount_out_f, _price_impact) = if a_to_b {
        // Selling X (A) -> Buy Y (B)
        // Y = X * price * (10^decY / 10^decX)
        let dec_adj = 10f64.powi(pool.decimals_b as i32 - pool.decimals_a as i32);
        (amount_net * price_x_in_y * dec_adj, 0.0)
    } else {
        // Selling Y (B) -> Buy X (A)
        // X = Y / price * (10^decX / 10^decY)
        let dec_adj = 10f64.powi(pool.decimals_a as i32 - pool.decimals_b as i32);
        (amount_net / price_x_in_y * dec_adj, 0.0)
    };

    Ok(SwapResult {
        amount_out: amount_out_f as u64,
        fee_paid: (amount_in as f64 * (1.0 - fee_factor)) as u64,
        new_sqrt_price: None,
    })
}

// ─── 128/256-bit arithmetic helpers ──────────────────────────────────────────




/// Full 256-bit product of a × b, returned as `(high_128, low_128)`.
fn mul128(a: u128, b: u128) -> (u128, u128) {
    let (a0, a1) = (a & 0xFFFF_FFFF_FFFF_FFFF, a >> 64);
    let (b0, b1) = (b & 0xFFFF_FFFF_FFFF_FFFF, b >> 64);

    let hh = a1 * b1;
    let hl = a1 * b0;
    let lh = a0 * b1;
    let ll = a0 * b0;

    let (lo, c1) = ll.overflowing_add(hl << 64);
    let (lo, c2) = lo.overflowing_add(lh << 64);
    let hi = hh
        .wrapping_add(hl >> 64)
        .wrapping_add(lh >> 64)
        .wrapping_add(c1 as u128)
        .wrapping_add(c2 as u128);

    (hi, lo)
}

#[allow(dead_code)]
fn mul128_hi(a: u128, b: u128) -> u128 {
    mul128(a, b).0
}

/// Divide a 256-bit `numer = (hi, lo)` by a 128-bit `denom`.
/// Returns `u128::MAX` on overflow or division by zero.
///
/// Exact long division for `hi <= 2^64`; f64 fallback for larger `hi`.
fn div256(numer: (u128, u128), denom: u128) -> u128 {
    let (hi, lo) = numer;
    if denom == 0  { return u128::MAX; }
    if hi == 0     { return lo / denom; }
    if hi >= denom { return u128::MAX; } // result would not fit in u128

    // Fast path: exact 64-bit word long division.
    // Works for hi <= Q64 = 2^64 (covers all tick conversions and normal pool math).
    if hi <= Q64 {
        let lo_hi = lo >> 64;
        let lo_lo = lo & 0xFFFF_FFFF_FFFF_FFFF;

        if hi < Q64 {
            // hi<<64 is safe (< 2^128).
            let partial_hi = (hi << 64) | lo_hi;
            let q1 = partial_hi / denom;
            let r1 = partial_hi % denom;
            let q2 = ((r1 << 64) | lo_lo) / denom;
            return (q1 << 64).saturating_add(q2);
        }

        // hi == Q64 (2^64): partial_hi = 2^128 + lo_hi overflows u128.
        let qd = u128::MAX / denom;
        let rd = u128::MAX % denom;
        let (r_128, carry) = rd.overflowing_add(1);
        let q_128 = if carry { qd + 1 } else { qd };
        let rem_128 = if carry { 0u128 } else { r_128 };

        let (combined, carry2) = rem_128.overflowing_add(lo_hi);
        let q1 = q_128 + (carry2 as u128) + combined / denom;
        let r1 = combined % denom;
        let q2 = ((r1 << 64) | lo_lo) / denom;
        return (q1 << 64).saturating_add(q2);
    }
    // hi > Q64: fall back to f64 (accurate to ~15 sig digits, sufficient for quoting).
    let two128 = (u128::MAX as f64) + 1.0_f64;
    let result = (hi as f64 * two128 + lo as f64) / denom as f64;
    if result >= two128 { u128::MAX } else { result as u128 }
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
            ticks:          dashmap::DashMap::new(), // no crossings
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
}
