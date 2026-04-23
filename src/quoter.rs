use crate::cache::GlobalPoolCache;
use crate::math::compute_swap;
use anyhow::{Result, anyhow};
use log::{info, debug};
use std::sync::Arc;

pub struct Quote {
    pub amount_out: u64,
    pub routes: Vec<RoutePlan>,
}

pub struct RoutePlan {
    pub pool_ids: Vec<String>,
    pub amount_in: u64,
    pub amount_out: u64,
    pub price_impact: f64,
}

pub struct Quoter {
    pub cache: Arc<GlobalPoolCache>,
}

impl Quoter {
    pub fn new(cache: Arc<GlobalPoolCache>) -> Self {
        Self { cache }
    }

    pub fn get_quote(
        &self,
        token_in: &str,
        token_out: &str,
        amount_in: u64,
        slices: usize,
    ) -> Result<Quote> {
        info!("Quoter: Requesting quote for {} -> {} (amount: {})", token_in, token_out, amount_in);
        let paths = self.find_best_paths(token_in, token_out)?;
        if paths.is_empty() {
            return Err(anyhow!("No paths found from {} to {}", token_in, token_out));
        }
        let slice_amount = amount_in / slices as u64;
        let mut allocations = vec![0u64; paths.len()];
        let mut total_out = 0u64;
        debug!("Quoter: Considering {} paths from {} to {}", paths.len(), token_in, token_out);

        for _ in 0..slices {
            let mut best_marginal_out = 0;
            let mut best_path_idx = 0;

            for (idx, path) in paths.iter().enumerate() {
                let current_alloc = allocations[idx];
                let out_before = self.simulate_path(path, token_in, current_alloc)?;
                let out_after = self.simulate_path(path, token_in, current_alloc + slice_amount)?;
                let marginal = out_after.saturating_sub(out_before);

                // Apply a hop penalty to prioritize shorter paths unless multi-hop is significantly better.
                // Each hop adds 20bps of "virtual cost" for comparison purposes.
                let hop_penalty_bps = (path.len() as u64) * 20; 
                let effective_marginal = (marginal as u128 * (10000 - hop_penalty_bps) as u128 / 10000) as u64;

                if effective_marginal > best_marginal_out {
                    best_marginal_out = effective_marginal;
                    best_path_idx = idx;
                }
            }

            allocations[best_path_idx] += slice_amount;
            total_out += self.simulate_path(&paths[best_path_idx], token_in, allocations[best_path_idx])?.saturating_sub(self.simulate_path(&paths[best_path_idx], token_in, allocations[best_path_idx] - slice_amount)?);
        }

        // Build route plans
        let mut route_plans = Vec::new();
        for (idx, &alloc) in allocations.iter().enumerate() {
            if alloc > 0 {
                route_plans.push(RoutePlan {
                    pool_ids: paths[idx].clone(),
                    amount_in: alloc,
                    amount_out: self.simulate_path(&paths[idx], token_in, alloc)?,
                    price_impact: self.calculate_price_impact(&paths[idx], token_in, alloc)?,
                });
            }
        }

        Ok(Quote {
            amount_out: total_out,
            routes: route_plans,
        })
    }

    fn find_best_paths(&self, token_in: &str, token_out: &str) -> Result<Vec<Vec<String>>> {
        let mut graph = crate::graph::TokenGraph::new();
        graph.build(&self.cache);
        let routes = graph.find_routes(token_in, token_out, 3);
        debug!("TokenGraph: Found {} routes for {} -> {}", routes.len(), token_in, token_out);
        if routes.is_empty() {
            return Ok(vec![]);
        }
        Ok(routes)
    }

    fn simulate_path(&self, path: &[String], token_in: &str, amount_in: u64) -> Result<u64> {
        let mut current_amount = amount_in;
        let mut current_token = token_in.to_string();
        
        for pool_id in path {
            let pool = self.cache.get_pool(pool_id).ok_or_else(|| anyhow!("Pool {} not found", pool_id))?;
            let a_to_b = pool.token_a == current_token;
            
            let res = compute_swap(&pool, current_amount, a_to_b)?;
            if res.amount_out == 0 && current_amount > 0 {
                log::warn!("Quoter: Simulating pool {} resulted in 0 output (reserves: {}/{}). Check Price Oracle sync.", pool_id, pool.reserve_a, pool.reserve_b);
            }
            current_amount = res.amount_out;
            
            // Advance current token
            current_token = if a_to_b { pool.token_b } else { pool.token_a };
        }
        Ok(current_amount)
    }

    fn calculate_price_impact(&self, path: &[String], token_in: &str, amount_in: u64) -> Result<f64> {
        let mut ideal_out = amount_in as f64;
        let mut current_token = token_in.to_string();

        for pool_id in path {
            let pool = self.cache.get_pool(pool_id).ok_or_else(|| anyhow!("Pool {} not found", pool_id))?;
            let (reserve_in, reserve_out) = if pool.token_a == current_token {
                (pool.reserve_a, pool.reserve_b)
            } else {
                (pool.reserve_b, pool.reserve_a)
            };

            if reserve_in == 0 { return Ok(0.0); }
            let mid_price = reserve_out as f64 / reserve_in as f64;
            ideal_out *= mid_price;

            // Advance current token
            current_token = if pool.token_a == current_token { pool.token_b } else { pool.token_a };
        }

        let actual_out = self.simulate_path(path, token_in, amount_in)? as f64;
        if ideal_out <= 0.0 { return Ok(0.0); }

        let impact = (1.0 - (actual_out / ideal_out)) * 100.0;
        Ok(impact.max(0.0))
    }

    pub fn build_route(&self, quote: &Quote) -> Vec<u8> {
        // Create Solana transaction instruction data
        // For each route, include: amount_in, min_amount_out, pool_keys
        let mut data = Vec::new();
        for plan in &quote.routes {
            data.push(plan.amount_in.to_le_bytes().to_vec());
            data.push(plan.amount_out.to_le_bytes().to_vec());
            for pool_id in &plan.pool_ids {
                data.push(pool_id.as_bytes().to_vec());
            }
        }
        data.concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{PoolState, PoolType};

    #[test]
    fn test_force_multi_hop_optimality() {
        let cache = Arc::new(GlobalPoolCache::new());
        
        // Token A -> Token C (Direct)
        // High slippage: 100k / 100k reserves
        cache.update_pool("direct".to_string(), PoolState {
            id: "direct".to_string(),
            token_a: "A".to_string(),
            token_b: "C".to_string(),
            symbol_a: "A".to_string(),
            symbol_b: "C".to_string(),
            decimals_a: 6,
            decimals_b: 6,
            reserve_a: 100_000,
            reserve_b: 100_000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            dex_label: "test".to_string(),
            clmm_data: None,
        });

        // Path: A -> B -> C (Multi-hop)
        // Deep liquidity: 10M / 10M reserves
        cache.update_pool("hop1".to_string(), PoolState {
            id: "hop1".to_string(),
            token_a: "A".to_string(),
            token_b: "B".to_string(),
            symbol_a: "A".to_string(),
            symbol_b: "B".to_string(),
            decimals_a: 6,
            decimals_b: 6,
            reserve_a: 10_000_000,
            reserve_b: 10_000_000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            dex_label: "test".to_string(),
            clmm_data: None,
        });

        cache.update_pool("hop2".to_string(), PoolState {
            id: "hop2".to_string(),
            token_a: "B".to_string(),
            token_b: "C".to_string(),
            symbol_a: "B".to_string(),
            symbol_b: "C".to_string(),
            decimals_a: 6,
            decimals_b: 6,
            reserve_a: 10_000_000,
            reserve_b: 10_000_000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            dex_label: "test".to_string(),
            clmm_data: None,
        });

        let quoter = Quoter::new(cache);
        
        // Swap 50k Token A
        // Direct path (100k/100k) will have massive slippage (~50%).
        // Multi-hop path (10M/10M) will have almost zero slippage.
        let quote = quoter.get_quote("A", "C", 50_000, 10).unwrap();

        info!("Quote amount out: {}", quote.amount_out);
        for route in &quote.routes {
            info!("  Path: {:?}, Amount In: {}, Amount Out: {}", route.pool_ids, route.amount_in, route.amount_out);
        }

        // Verify that the multi-hop path was prioritized
        let hop_route = quote.routes.iter().find(|r| r.pool_ids.len() == 2);
        assert!(hop_route.is_some(), "Aggregator should have used the multi-hop path");
        
        let direct_route = quote.routes.iter().find(|r| r.pool_ids.len() == 1);
        if let Some(direct) = direct_route {
            assert!(direct.amount_in < hop_route.unwrap().amount_in, "Multi-hop path should have taken more volume");
        }
    }
}
