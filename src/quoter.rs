use crate::math::compute_swap;
use anyhow::{Result, anyhow};
use log::{info, warn, debug};
use std::sync::Arc;
use std::collections::HashSet;
use crate::cache::{GlobalPoolCache, PoolType};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::{v0::Message, VersionedMessage},
    pubkey::Pubkey,
    transaction::VersionedTransaction,
    hash::Hash,
};
use std::str::FromStr;
// HashMap is no longer needed here

#[derive(Clone)]
pub struct Quote {
    pub amount_out: u64,
    pub split_routes: Vec<RoutePlan>,
    pub candidate_routes: Vec<RoutePlan>,
}

#[derive(Clone)]
pub struct RoutePlan {
    pub pool_ids: Vec<String>,
    pub token_path: Vec<String>,
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
            let mut best_path_idx = None;

            for (idx, path) in paths.iter().enumerate() {
                let current_alloc = allocations[idx];
                let out_before = self.simulate_path(path, token_in, current_alloc)?;
                let out_after = self.simulate_path(path, token_in, current_alloc + slice_amount)?;
                let marginal = out_after.saturating_sub(out_before);

                if marginal == 0 { continue; }

                // Apply a hop penalty to prioritize shorter paths
                let hop_penalty_bps = (path.len() as u64) * 20; 
                let effective_marginal = (marginal as u128 * (10000 - hop_penalty_bps) as u128 / 10000) as u64;

                if effective_marginal > best_marginal_out {
                    best_marginal_out = effective_marginal;
                    best_path_idx = Some(idx);
                }
            }

            if let Some(idx) = best_path_idx {
                allocations[idx] += slice_amount;
                let marginal_out = self.simulate_path(&paths[idx], token_in, allocations[idx])?.saturating_sub(self.simulate_path(&paths[idx], token_in, allocations[idx] - slice_amount)?);
                total_out = total_out.saturating_add(marginal_out);
            }
        }

        if total_out == 0 {
            return Err(anyhow!("No liquid route found (simulated 0 output)"));
        }

        // Build route plans
        let mut split_routes = Vec::new();
        let mut used_path_indices = HashSet::new();

        // 1. Add paths with actual allocation (The "Split" route)
        for (idx, &alloc) in allocations.iter().enumerate() {
            if alloc > 0 {
                let path = &paths[idx];
                let mut token_path = vec![token_in.to_string()];
                let mut current_token = token_in.to_string();
                for pool_id in path {
                    if let Some(pool) = self.cache.get_pool(pool_id) {
                        current_token = if pool.token_a == current_token { pool.token_b.clone() } else { pool.token_a.clone() };
                        token_path.push(current_token.clone());
                    }
                }

                split_routes.push(RoutePlan {
                    pool_ids: path.clone(),
                    token_path,
                    amount_in: alloc,
                    amount_out: self.simulate_path(path, token_in, alloc)?,
                    price_impact: self.calculate_price_impact(path, token_in, alloc)?,
                });
                used_path_indices.insert(idx);
            }
        }

        // 2. Add top 3 non-allocated direct candidate routes for visibility
        let mut candidate_routes = Vec::new();
        let mut candidates = Vec::new();
        for (idx, path) in paths.iter().enumerate() {
            if !used_path_indices.contains(&idx) {
                if let Ok(out) = self.simulate_path(path, token_in, amount_in) {
                    if out > 0 {
                        candidates.push((idx, out));
                    }
                }
            }
        }
        
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        for (idx, out) in candidates.into_iter().take(3) {
            let path = &paths[idx];
            let mut token_path = vec![token_in.to_string()];
            let mut current_token = token_in.to_string();
            for pool_id in path {
                if let Some(pool) = self.cache.get_pool(pool_id) {
                    current_token = if pool.token_a == current_token { pool.token_b.clone() } else { pool.token_a.clone() };
                    token_path.push(current_token.clone());
                }
            }

            candidate_routes.push(RoutePlan {
                pool_ids: path.clone(),
                token_path,
                amount_in: amount_in,
                amount_out: out,
                price_impact: self.calculate_price_impact(path, token_in, amount_in)?,
            });
        }

        Ok(Quote {
            amount_out: total_out,
            split_routes,
            candidate_routes,
        })
    }

    fn find_best_paths(&self, token_in: &str, token_out: &str) -> Result<Vec<Vec<String>>> {
        let mut graph = crate::graph::TokenGraph::new();
        graph.build(&self.cache);
        let routes = graph.find_routes(token_in, token_out, 2);
        info!("TokenGraph: Found {} candidate paths for {} -> {}", routes.len(), token_in, token_out);
        for (i, path) in routes.iter().enumerate() {
            info!("  Path {}: {:?}", i, path);
        }
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
            current_token = if a_to_b { pool.token_b.clone() } else { pool.token_a.clone() };
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
            let mid_price = match pool.pool_type {
                PoolType::ConcentratedLiquidity => {
                    if let Some(clmm) = &pool.clmm_data {
                        let p = clmm.sqrt_price_x64 as f64 / (1u128 << 64) as f64;
                        let raw_price = p * p;
                        if pool.token_a == current_token { raw_price } else { 1.0 / raw_price }
                    } else {
                        reserve_out as f64 / reserve_in as f64
                    }
                },
                PoolType::LlbBin => {
                    if let Some(bin) = &pool.lb_bin_data {
                        let bin_step_f = bin.bin_step as f64 / 10000.0;
                        let raw_price = (1.0 + bin_step_f).powi(bin.active_id);
                        if pool.token_a == current_token { raw_price } else { 1.0 / raw_price }
                    } else {
                        reserve_out as f64 / reserve_in as f64
                    }
                },
                _ => reserve_out as f64 / reserve_in as f64,
            };
            
            // Adjust for decimals
            let dec_adj = 10f64.powi(pool.decimals_a as i32 - pool.decimals_b as i32);
            let adjusted_mid_price = if pool.token_a == current_token {
                mid_price * dec_adj
            } else {
                mid_price / dec_adj
            };

            ideal_out *= adjusted_mid_price;

            // Advance current token
            current_token = if pool.token_a == current_token { pool.token_b.clone() } else { pool.token_a.clone() };
        }

        let actual_out = self.simulate_path(path, token_in, amount_in)? as f64;
        if ideal_out <= 0.0 { return Ok(0.0); }

        let impact = (1.0 - (actual_out / ideal_out)) * 100.0;
        Ok(impact.max(0.0))
    }

    pub fn build_route(&self, routes: &[RoutePlan], user_pubkey: &str, recent_blockhash: &str, priority_fee: u64, slippage_bps: f64) -> Vec<u8> {
        let user_key = match Pubkey::from_str(user_pubkey) {
            Ok(p) => p,
            Err(e) => {
                warn!("Quoter: Invalid user_pubkey {}: {}", user_pubkey, e);
                return Vec::new();
            }
        };

        let blockhash = if recent_blockhash.is_empty() {
            Hash::default()
        } else {
            match Hash::from_str(recent_blockhash) {
                Ok(h) => h,
                Err(e) => {
                    warn!("Quoter: Invalid blockhash provided: {}, {}", recent_blockhash, e);
                    Hash::default()
                }
            }
        };

        let mut instructions = Vec::new();

        if priority_fee > 0 {
            instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee));
        }
        
        // Increase compute unit limit for complex multi-hop swaps (600k is safe for 2 hops x 2 routes)
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(600_000));

        // Collect unique mints that need ATAs
        let mut unique_mints = HashSet::new();
        for plan in routes {
            for token in &plan.token_path {
                if let Ok(mint) = Pubkey::from_str(token) {
                    unique_mints.insert(mint);
                }
            }
        }

        // Add idempotent ATA creation for each unique mint
        let wsol_mint = "So11111111111111111111111111111111111111112";
        for mint in unique_mints {
            let mint_str = mint.to_string();
            let token_program_str = self.cache.token_programs.get(&mint_str)
                .map(|p| p.value().clone())
                .unwrap_or_else(|| "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string());
            
            let token_program = match Pubkey::from_str(&token_program_str) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Quoter: Invalid token program ID for mint {}: {} (error: {})", mint_str, token_program_str, e);
                    return Vec::new();
                }
            };

            instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &user_key,
                &user_key,
                &mint,
                &token_program,
            ));
            
            // If it's WSOL, we might need SyncNative (though typically handled by client for SOL -> Token)
            if mint_str == wsol_mint {
                let wsol_ata = spl_associated_token_account::get_associated_token_address(&user_key, &mint);
                let sync_ins = spl_token::instruction::sync_native(&token_program, &wsol_ata);
                if let Err(e) = sync_ins {
                    warn!("Quoter: Failed to create sync_native instruction: {}", e);
                    return Vec::new();
                }
                instructions.push(sync_ins.unwrap());
            }
        }

        for plan in routes {
            let mut current_hop_amount = plan.amount_in;
            
            // If the first hop starts with SOL, we need to transfer it to the WSOL ATA first
            if let Some(first_token) = plan.token_path.first() {
                if first_token == wsol_mint {
                    let wsol_mint_pub = match Pubkey::from_str(wsol_mint) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!("Quoter: Invalid wsol mint string: {}", e);
                            return Vec::new();
                        }
                    };
                    let wsol_ata = spl_associated_token_account::get_associated_token_address(&user_key, &wsol_mint_pub);
                    instructions.push(solana_sdk::system_instruction::transfer(
                        &user_key,
                        &wsol_ata,
                        plan.amount_in,
                    ));
                    // Re-sync after transfer
                    let tp_str = self.cache.token_programs.get(wsol_mint).map(|p| p.value().clone()).unwrap_or_else(|| "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string());
                    let token_program_id = match Pubkey::from_str(&tp_str) {
                        Ok(p) => p,
                        Err(_) => return Vec::new(),
                    };
                    if let Ok(ix) = spl_token::instruction::sync_native(&token_program_id, &wsol_ata) {
                        instructions.push(ix);
                    } else {
                        return Vec::new();
                    }
                }
            }

            for (i, pool_id) in plan.pool_ids.iter().enumerate() {
                if i + 1 >= plan.token_path.len() { break; }
                let token_in_hop = &plan.token_path[i];
                let token_out_hop = &plan.token_path[i+1];

                if let Some(pool) = self.cache.get_pool(pool_id) {
                    // Simulate this hop to get the expected amount_out
                    if let Ok(res) = crate::math::compute_swap(&pool, current_hop_amount, pool.token_a == *token_in_hop) {
                        // The guaranteed minimum output we will receive after slippage
                        let slippage_factor = if slippage_bps > 0.0 { 1.0 - (slippage_bps / 10000.0) } else { 0.99 };
                        let min_amount_out = (res.amount_out as f64 * slippage_factor) as u64;
                        
                        if let Some(ix) = self.create_swap_instruction(&pool, user_key, current_hop_amount, min_amount_out, token_in_hop, token_out_hop) {
                            instructions.push(ix);
                            current_hop_amount = min_amount_out;
                        } else {
                            warn!("Quoter: Failed to create swap instruction for pool {} ({})", pool_id, pool.dex_label);
                            return Vec::new();
                        }
                    } else {
                        warn!("Quoter: Simulation failed for pool {} during transaction building", pool_id);
                        return Vec::new();
                    }
                } else {
                    warn!("Quoter: Pool {} not found in cache during transaction building", pool_id);
                    return Vec::new();
                }
            }
        }

        if instructions.is_empty() {
            warn!("Quoter: No instructions generated for routes");
            return Vec::new();
        }

        // Build a VersionedTransaction (V0)
        let message = match Message::try_compile(
            &user_key,
            &instructions,
            &[],
            blockhash, 
        ) {
            Ok(m) => m,
            Err(e) => {
                warn!("Quoter: Failed to compile transaction message: {}", e);
                return Vec::new();
            }
        };

        let tx = VersionedTransaction {
            signatures: vec![solana_sdk::signature::Signature::default()],
            message: VersionedMessage::V0(message),
        };

        match bincode::serialize(&tx) {
            Ok(bytes) => bytes,
            Err(_) => Vec::new(),
        }
    }

    fn derive_tick_array_pda(&self, whirlpool: &Pubkey, tick: i32, tick_spacing: u16) -> Pubkey {
        let ticks_in_array = 88i32;
        let array_size = (tick_spacing as i32).checked_mul(ticks_in_array).unwrap_or(i32::MAX);
        
        // Correct flooring for negative ticks
        let start_tick = if tick >= 0 {
            (tick / array_size.max(1)) * array_size
        } else {
            ((tick - array_size + 1) / array_size.max(1)) * array_size
        };
        
        let (pda, _) = Pubkey::find_program_address(
            &[
                b"tick_array",
                whirlpool.as_ref(),
                &start_tick.to_string().as_bytes(),
            ],
            &Pubkey::from_str("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc").unwrap(),
        );
        pda
    }

    fn create_swap_instruction(&self, pool: &crate::cache::PoolState, user_key: Pubkey, amount_in: u64, min_amount_out: u64, token_in: &str, token_out: &str) -> Option<Instruction> {
        let mint_in = Pubkey::from_str(token_in).ok().or_else(|| { warn!("Quoter: Invalid mint_in {}", token_in); None })?;
        let mint_out = Pubkey::from_str(token_out).ok().or_else(|| { warn!("Quoter: Invalid mint_out {}", token_out); None })?;
        let user_ata_in = spl_associated_token_account::get_associated_token_address(&user_key, &mint_in);
        let user_ata_out = spl_associated_token_account::get_associated_token_address(&user_key, &mint_out);

        match pool.dex_label.as_str() {
            "raydium" | "raydium_cpmm" => {
                log::warn!("Raydium routes are disabled in the instruction builder due to missing market accounts");
                None
            },
            "orca" => {
                let program_id = Pubkey::from_str(pool.accounts.get("program_id").unwrap_or(&"whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".to_string())).unwrap_or_else(|_| Pubkey::from_str("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc").unwrap());
                let whirlpool = match Pubkey::from_str(&pool.id) {
                    Ok(p) => p,
                    Err(e) => { warn!("Quoter: Invalid whirlpool address {}: {}", pool.id, e); return None; }
                };

                let vault_a = match pool.accounts.get("pool_vault_a") {
                    Some(v) => match Pubkey::from_str(v) {
                        Ok(p) => p,
                        Err(e) => { warn!("Quoter: Invalid vault_a {}: {}", v, e); return None; }
                    },
                    None => { warn!("Quoter: Orca pool {} missing pool_vault_a", pool.id); return None; }
                };
                let vault_b = match pool.accounts.get("pool_vault_b") {
                    Some(v) => match Pubkey::from_str(v) {
                        Ok(p) => p,
                        Err(e) => { warn!("Quoter: Invalid vault_b {}: {}", v, e); return None; }
                    },
                    None => { warn!("Quoter: Orca pool {} missing pool_vault_b", pool.id); return None; }
                };

                // Determine if token_in is token_a
                let a_to_b = token_in == pool.token_a;
                let (user_ata_a, user_ata_b) = if a_to_b { (user_ata_in, user_ata_out) } else { (user_ata_out, user_ata_in) };

                // Resolve specific token program for token_a (needed for Whirlpool v1 swap accounts)
                let program_a_str = self.cache.token_programs.get(&pool.token_a)
                    .map(|p| p.value().clone())
                    .unwrap_or_else(|| "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string());
                let program_a = match Pubkey::from_str(&program_a_str) {
                    Ok(p) => p,
                    Err(_) => { warn!("Quoter: Invalid program_a {}", program_a_str); return None; }
                };

                // Derive 3 TickArrays to cover the range (prevents 6036)
                let (ta0, ta1, ta2) = if let Some(clmm) = &pool.clmm_data {
                    let array_size = clmm.tick_spacing as i32 * 88;
                    let current_ta = self.derive_tick_array_pda(&whirlpool, clmm.current_tick, clmm.tick_spacing);
                    
                    if a_to_b {
                        (
                            current_ta,
                            self.derive_tick_array_pda(&whirlpool, clmm.current_tick - array_size, clmm.tick_spacing),
                            self.derive_tick_array_pda(&whirlpool, clmm.current_tick - 2 * array_size, clmm.tick_spacing),
                        )
                    } else {
                        (
                            current_ta,
                            self.derive_tick_array_pda(&whirlpool, clmm.current_tick + array_size, clmm.tick_spacing),
                            self.derive_tick_array_pda(&whirlpool, clmm.current_tick + 2 * array_size, clmm.tick_spacing),
                        )
                    }
                } else {
                    (whirlpool, whirlpool, whirlpool) // Fallback for basic connectivity
                };

                let (oracle, _) = Pubkey::find_program_address(&[b"oracle", whirlpool.as_ref()], &program_id);

                let accounts = vec![
                    AccountMeta::new_readonly(program_a, false), // token_program
                    AccountMeta::new(user_key, true),            // token_authority (Signer)
                    AccountMeta::new(whirlpool, false),
                    AccountMeta::new(user_ata_a, false),         // token_owner_account_a
                    AccountMeta::new(vault_a, false),            // token_vault_a
                    AccountMeta::new(user_ata_b, false),         // token_owner_account_b
                    AccountMeta::new(vault_b, false),            // token_vault_b
                    AccountMeta::new(ta0, false),
                    AccountMeta::new(ta1, false),
                    AccountMeta::new(ta2, false),
                    AccountMeta::new_readonly(oracle, false),
                ];

                let mut data = vec![0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];
                data.extend_from_slice(&amount_in.to_le_bytes());
                data.extend_from_slice(&min_amount_out.to_le_bytes());
                
                let sqrt_price_limit = if let Some(clmm) = &pool.clmm_data {
                    let array_size = (clmm.tick_spacing as i32).saturating_mul(88);
                    let array_idx = if clmm.current_tick >= 0 { clmm.current_tick / array_size.max(1) } else { (clmm.current_tick - array_size + 1) / array_size.max(1) };
                    if a_to_b {
                        let limit_tick = array_idx.saturating_sub(2).saturating_mul(array_size);
                        crate::math::tick_to_sqrt_price_x64(limit_tick).saturating_add(1)
                    } else {
                        let limit_tick = array_idx.saturating_add(3).saturating_mul(array_size).saturating_sub(1);
                        crate::math::tick_to_sqrt_price_x64(limit_tick).saturating_sub(1)
                    }
                } else {
                    if a_to_b { 4295048016_u128 + 1 } else { 79228162514264337593543950335_u128 - 1 }
                };

                data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
                data.push(1u8); // amount_specified_is_input = true
                data.push(if a_to_b { 1u8 } else { 0u8 }); // a_to_b

                Some(Instruction { program_id, accounts, data })
            },
            "meteora" => {
                let program_id = match Pubkey::from_str("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo") {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid program ID"); return None; }
                };
                let lb_pair = match Pubkey::from_str(&pool.id) {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid pool ID {}", pool.id); return None; }
                };
                
                let res_x_str = pool.accounts.get("reserve_x").cloned().or_else(|| { log::error!("Meteora: Missing reserve_x for pool {}", pool.id); None })?;
                let reserve_x = match Pubkey::from_str(&res_x_str) {
                    Ok(p) => p,
                    Err(_) => { warn!("Quoter: Meteora: Invalid reserve_x {} for pool {}", res_x_str, pool.id); return None; }
                };
                let res_y_str = pool.accounts.get("reserve_y").cloned().or_else(|| { warn!("Quoter: Meteora: Missing reserve_y for pool {}", pool.id); None })?;
                let reserve_y = match Pubkey::from_str(&res_y_str) {
                    Ok(p) => p,
                    Err(_) => { warn!("Quoter: Meteora: Invalid reserve_y {} for pool {}", res_y_str, pool.id); return None; }
                };
                
                let active_id = if let Some(bin) = &pool.lb_bin_data {
                    bin.active_id
                } else {
                    log::error!("Meteora: Missing lb_bin_data for pool {}", pool.id);
                    0
                };

                let bin_array_idx = if active_id >= 0 {
                    active_id / 64
                } else {
                    (active_id - 63) / 64
                };
                
                let a_to_b = token_in == pool.token_a;
                let (ba0, ba1, ba2) = if a_to_b {
                    (bin_array_idx, bin_array_idx - 1, bin_array_idx - 2)
                } else {
                    (bin_array_idx, bin_array_idx + 1, bin_array_idx + 2)
                };

                let program_in_str = self.cache.token_programs.get(token_in)
                    .map(|p| p.value().clone())
                    .unwrap_or_else(|| "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string());
                let program_out_str = self.cache.token_programs.get(token_out)
                    .map(|p| p.value().clone())
                    .unwrap_or_else(|| "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string());
                
                let program_in = match Pubkey::from_str(&program_in_str) {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid program_in {}", program_in_str); return None; }
                };
                let program_out = match Pubkey::from_str(&program_out_str) {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid program_out {}", program_out_str); return None; }
                };

                let (ba0_pub, _) = Pubkey::find_program_address(&[b"bin_array", lb_pair.as_ref(), &(ba0 as i32).to_le_bytes()], &program_id);
                let (ba1_pub, _) = Pubkey::find_program_address(&[b"bin_array", lb_pair.as_ref(), &(ba1 as i32).to_le_bytes()], &program_id);
                let (ba2_pub, _) = Pubkey::find_program_address(&[b"bin_array", lb_pair.as_ref(), &(ba2 as i32).to_le_bytes()], &program_id);
                let (bitmap_ext, _) = Pubkey::find_program_address(&[b"bitmap", lb_pair.as_ref()], &program_id);
                let (oracle, _) = Pubkey::find_program_address(&[b"oracle", lb_pair.as_ref()], &program_id);
                let event_authority = match Pubkey::from_str("EVSAoyjP54s7K9Wv7Jt2eB55S3t481S15P9f77fM5T9") {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid event_authority"); return None; }
                };

                let token_a_pub = match Pubkey::from_str(&pool.token_a) {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid token_a {}", pool.token_a); return None; }
                };
                let token_b_pub = match Pubkey::from_str(&pool.token_b) {
                    Ok(p) => p,
                    Err(_) => { log::error!("Meteora: Invalid token_b {}", pool.token_b); return None; }
                };

                let accounts = vec![
                    AccountMeta::new(lb_pair, false),
                    AccountMeta::new_readonly(bitmap_ext, false),
                    AccountMeta::new(reserve_x, false),
                    AccountMeta::new(reserve_y, false),
                    AccountMeta::new(user_ata_in, false),
                    AccountMeta::new(user_ata_out, false),
                    AccountMeta::new_readonly(token_a_pub, false),
                    AccountMeta::new_readonly(token_b_pub, false),
                    AccountMeta::new_readonly(oracle, false),
                    AccountMeta::new_readonly(program_id, false), // host_fee_in (placeholder)
                    AccountMeta::new(user_key, true),
                    AccountMeta::new_readonly(program_in, false),
                    AccountMeta::new_readonly(program_out, false),
                    AccountMeta::new_readonly(event_authority, false),
                    AccountMeta::new_readonly(program_id, false),
                    AccountMeta::new(ba0_pub, false),
                    AccountMeta::new(ba1_pub, false),
                    AccountMeta::new(ba2_pub, false),
                ];

                // Anchor discriminator for "swap": [248, 198, 158, 145, 225, 117, 135, 200]
                let mut data = vec![0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];
                data.extend_from_slice(&amount_in.to_le_bytes());
                data.extend_from_slice(&min_amount_out.to_le_bytes());

                Some(Instruction {
                    program_id,
                    accounts,
                    data,
                })
            },
            _ => None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
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
            dex_label: "orca".to_string(),
            clmm_data: None,
            lb_bin_data: None,
            accounts: {
                let mut h = HashMap::new();
                h.insert("pool_vault_a".to_string(), "v_a".to_string());
                h.insert("pool_vault_b".to_string(), "v_b".to_string());
                h
            },
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
            dex_label: "orca".to_string(),
            clmm_data: None,
            lb_bin_data: None,
            accounts: {
                let mut h = HashMap::new();
                h.insert("pool_vault_a".to_string(), "v_a".to_string());
                h.insert("pool_vault_b".to_string(), "v_b".to_string());
                h
            },
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
            dex_label: "orca".to_string(),
            clmm_data: None,
            lb_bin_data: None,
            accounts: {
                let mut h = HashMap::new();
                h.insert("pool_vault_a".to_string(), "v_a".to_string());
                h.insert("pool_vault_b".to_string(), "v_b".to_string());
                h
            },
        });

        let quoter = Quoter::new(cache);
        
        // Swap 50k Token A
        // Direct path (100k/100k) will have massive slippage (~50%).
        // Multi-hop path (10M/10M) will have almost zero slippage.
        let quote = quoter.get_quote("A", "C", 50_000, 10).unwrap();

        // Verify that the multi-hop path was prioritized
        let hop_route = quote.split_routes.iter().find(|r| r.pool_ids.len() == 2);
        assert!(hop_route.is_some(), "Aggregator should have used the multi-hop path");
        
        let direct_route = quote.split_routes.iter().find(|r| r.pool_ids.len() == 1);
        if let Some(direct) = direct_route {
            assert!(direct.amount_in < hop_route.unwrap().amount_in, "Multi-hop path should have taken more volume");
        }
    }
}
