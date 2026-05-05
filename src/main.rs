use crate::cache::{GlobalPoolCache, PoolState, PoolType};
use crate::quoter::Quoter;
use crate::sor::{sor_service_server::{SorService, SorServiceServer}, QuoteRequest, QuoteResponse, SwapRequest, SwapResponse, ListTokensRequest, ListTokensResponse, TokenInfo, DetailedRoute};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tonic::{transport::Server, Request, Response, Status};
use log::{info, warn};

pub mod sor {
    tonic::include_proto!("sor");
}

pub mod oracle {
    tonic::include_proto!("oracle");
}

pub mod cache;
pub mod math;
pub mod quoter;
pub mod graph;

pub struct MySOR {
    quoter: Arc<Quoter>,
}

impl MySOR {
    fn resolve_token(&self, token: &str) -> String {
        // If it looks like a mint (length > 30), return as is
        if token.len() > 30 {
            return token.to_string();
        }
        
        let token_upper = token.to_uppercase();

        // Look up in the symbols cache (populated dynamically by the Oracle)
        if let Some(mint) = self.quoter.cache.symbols.get(&token_upper) {
            return mint.value().clone();
        }
        
        token.to_string()
    }

    fn atomic_to_human(&self, amount: u128, decimals: u32) -> String {
        if decimals == 0 {
            return amount.to_string();
        }
        // Limit decimals to 38 (max power of 10 that fits in u128)
        let safe_decimals = decimals.min(38);
        let divisor = 10u128.checked_pow(safe_decimals).unwrap_or(u128::MAX);
        
        let integer = amount / divisor;
        let fractional = amount % divisor;
        if fractional == 0 {
            return integer.to_string();
        }
        format!("{}.{:0width$}", integer, fractional, width = safe_decimals as usize).trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[tonic::async_trait]
impl SorService for MySOR {
    async fn quote(&self, request: Request<QuoteRequest>) -> Result<Response<QuoteResponse>, Status> {
        let req = request.into_inner();
        let amount_in = req.amount.parse::<u64>().map_err(|_| Status::invalid_argument("Invalid amount"))?;
        
        let input_mint = self.resolve_token(&req.input_token);
        let output_mint = self.resolve_token(&req.output_token);

        // get_quote is synchronous and takes slices (default to 16)
        let quote = self.quoter.get_quote(&input_mint, &output_mint, amount_in, 16)
            .map_err(|e| Status::not_found(format!("No route found: {}", e)))?;
        
        let input_decimals = self.quoter.cache.get_decimals(&input_mint);
        let output_decimals = self.quoter.cache.get_decimals(&output_mint);
        
        // Using the first candidate route for the main path response
        let best_route = quote.split_routes.get(0).ok_or_else(|| Status::not_found("No split routes found"))?;

        Ok(Response::new(QuoteResponse {
            input_token: req.input_token,
            output_token: req.output_token,
            input_amount: req.amount,
            output_amount: quote.amount_out.to_string(),
            path: best_route.pool_ids.clone(),
            price_impact: best_route.price_impact,
            token_path: best_route.token_path.iter().map(|m| self.quoter.cache.get_symbol_by_mint(m)).collect(),
            human_input_amount: self.atomic_to_human(amount_in as u128, input_decimals),
            human_output_amount: self.atomic_to_human(quote.amount_out as u128, output_decimals),
        }))
    }

    async fn swap(&self, request: Request<SwapRequest>) -> Result<Response<SwapResponse>, Status> {
        let req = request.into_inner();
        let amount_in = req.amount.parse::<u64>().map_err(|_| Status::invalid_argument("Invalid amount"))?;

        let input_mint = self.resolve_token(&req.input_token);
        let output_mint = self.resolve_token(&req.output_token);

        // C. Request Validation: Validate recent_blockhash
        if !req.recent_blockhash.is_empty() {
            use std::str::FromStr;
            if solana_sdk::hash::Hash::from_str(&req.recent_blockhash).is_err() {
                 return Err(Status::invalid_argument("Invalid recent_blockhash (must be 32-byte Base58)"));
            }
        }

        // 1. Get the quote first to get the routes
        let quote = self.quoter.get_quote(&input_mint, &output_mint, amount_in, 16)
            .map_err(|e| Status::not_found(format!("No route found for swap: {}", e)))?;

        // 2. Build the transaction using build_route (synchronous)
        let tx_bytes = self.quoter.build_route(
            &quote.split_routes,
            &req.user_address,
            &req.recent_blockhash,
            req.prioritization_fee_lamports,
            req.slippage_bps,
        );

        if tx_bytes.is_empty() {
            warn!("SOR: build_route returned empty bytes for {} -> {} (amount: {})", req.input_token, req.output_token, req.amount);
            return Err(Status::internal("Failed to build transaction (empty bytes)"));
        }

        // 3. Build detailed routes for the response
        let input_decimals = self.quoter.cache.get_decimals(&input_mint);
        let output_decimals = self.quoter.cache.get_decimals(&output_mint);

        let detailed_routes = quote.split_routes.iter().map(|r| {
            DetailedRoute {
                token_path: r.token_path.iter().map(|m| self.quoter.cache.get_symbol_by_mint(m)).collect(),
                pool_ids: r.pool_ids.clone(),
                amount_in: r.amount_in.to_string(),
                amount_out: r.amount_out.to_string(),
                human_amount_in: self.atomic_to_human(r.amount_in as u128, input_decimals),
                human_amount_out: self.atomic_to_human(r.amount_out as u128, output_decimals),
                price_impact: r.price_impact,
                dex_labels: r.pool_ids.iter().map(|id| {
                    self.quoter.cache.get_pool(id).map(|p| p.dex_label.clone()).unwrap_or_default()
                }).collect(),
            }
        }).collect();

        Ok(Response::new(SwapResponse {
            status: "success".to_string(),
            message: "Transaction built successfully".to_string(),
            route: quote.split_routes.get(0).map(|r| {
                r.token_path.iter().map(|m| self.quoter.cache.get_symbol_by_mint(m)).collect()
            }).unwrap_or_default(),
            output_amount: quote.amount_out.to_string(),
            human_output_amount: self.atomic_to_human(quote.amount_out as u128, output_decimals),
            routes: detailed_routes,
            transaction: tx_bytes,
        }))
    }

    async fn list_tokens(&self, _request: Request<ListTokensRequest>) -> Result<Response<ListTokensResponse>, Status> {
        let mut seen_a = std::collections::HashSet::new();
        let mut seen_b = std::collections::HashSet::new();
        let mut token_a = Vec::new();
        let mut token_b = Vec::new();
        
        for entry in self.quoter.cache.pools.iter() {
            let pool = entry.value();
            if seen_a.insert(pool.token_a.clone()) {
                token_a.push(TokenInfo {
                    mint: pool.token_a.clone(),
                    symbol: pool.symbol_a.clone(),
                    decimals: pool.decimals_a,
                });
            }
            if seen_b.insert(pool.token_b.clone()) {
                token_b.push(TokenInfo {
                    mint: pool.token_b.clone(),
                    symbol: pool.symbol_b.clone(),
                    decimals: pool.decimals_b,
                });
            }
        }
        
        Ok(Response::new(ListTokensResponse {
            token_a,
            token_b,
        }))
    }
}

async fn refresh_pools(cache: Arc<GlobalPoolCache>, oracle_addr: String, mut shutdown: tokio::sync::oneshot::Receiver<()>) {
    loop {
        // Use tokio::select to wait for either the sleep or the shutdown signal
        tokio::select! {
            _ = sleep(Duration::from_secs(5)) => {
                // Continue with the refresh logic
            }
            _ = &mut shutdown => {
                info!("SOR: Shutdown signal received, stopping refresh loop");
                break;
            }
        }
        
        use oracle::price_oracle_client::PriceOracleClient;
        let mut client = match PriceOracleClient::connect(oracle_addr.clone()).await {
            Ok(c) => c,
            Err(e) => {
                warn!("SOR: Failed to connect to Oracle: {}", e);
                continue;
            }
        };

        if let Ok(res) = client.get_all_pool_updates(oracle::Empty {}).await {
            let updates = res.into_inner().updates;
            
            info!("SOR: Received {} updates from Oracle", updates.len());
            for update in updates {
                let mut fee_bps = 30;
                let mut clmm_data = None;
                let mut lb_bin_data = None;

                if update.amm_type == 1 || update.dex_label == "orca" {
                    // Orca Whirlpool: SqrtPrice(16) + Liquidity(16) + Tick(4) + Spacing(2) = 38 bytes
                    let d = &update.extra_data;
                    if d.len() >= 36 {
                        let sqrt_price_x64 = d.get(0..16).and_then(|b| b.try_into().ok()).map(u128::from_le_bytes).unwrap_or(0);
                        let liquidity = d.get(16..32).and_then(|b| b.try_into().ok()).map(u128::from_le_bytes).unwrap_or(0);
                        let current_tick = d.get(32..36).and_then(|b| b.try_into().ok()).map(i32::from_le_bytes).unwrap_or(0);
                        let tick_spacing = d.get(36..38).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes).unwrap_or(64);

                        if sqrt_price_x64 > 0 {
                            clmm_data = Some(crate::cache::ClmmData {
                                tick_spacing,
                                current_tick,
                                sqrt_price_x64,
                                liquidity,
                                ticks: std::collections::BTreeMap::new(),
                                tick_bitmap: None,
                            });
                        }
                    }
                } else if update.amm_type == 2 || update.dex_label == "meteora" {
                    // Meteora DLMM: ActiveId(4) + BinStep(2) + BaseFactor(2) = 8 bytes
                    let d = &update.extra_data;
                    if d.len() >= 8 {
                        let active_id = d.get(0..4).and_then(|b| b.try_into().ok()).map(i32::from_le_bytes).unwrap_or(0);
                        let bin_step = d.get(4..6).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes).unwrap_or(0);
                        let base_factor = d.get(6..8).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes).unwrap_or(0);

                        let fee_rate = (base_factor as u64).saturating_mul(bin_step as u64).saturating_mul(10);
                        fee_bps = (fee_rate / 100000).max(1) as u32;

                        lb_bin_data = Some(crate::cache::LbBinData { 
                            active_id, 
                            bin_step,
                            bins: std::collections::BTreeMap::new(),
                        });
                    }
                }

                cache.update_pool(update.pool_id.clone(), PoolState {
                    id: update.pool_id,
                    token_a: update.token_a,
                    token_b: update.token_b,
                    symbol_a: update.symbol_a,
                    symbol_b: update.symbol_b,
                    decimals_a: update.decimals_a,
                    decimals_b: update.decimals_b,
                    reserve_a: update.reserve_a,
                    reserve_b: update.reserve_b,
                    pool_type: if clmm_data.is_some() {
                        PoolType::ConcentratedLiquidity
                    } else if lb_bin_data.is_some() {
                        PoolType::LlbBin
                    } else {
                        PoolType::ConstantProduct
                    },
                    fee_bps: fee_bps as u16,
                    dex_label: update.dex_label,
                    clmm_data,
                    lb_bin_data,
                    accounts: update.accounts,
                });
            }
            info!("SOR: Pool cache refresh complete ({} pools in cache)", cache.pools.len());
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let cache = Arc::new(GlobalPoolCache::new());
    cache.seed_common_tokens();
    let quoter = Arc::new(Quoter::new(cache.clone()));

    let oracle_addr = "http://127.0.0.1:50051".to_string();
    
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let refresh_cache = cache.clone();
    let refresh_addr = oracle_addr.clone();
    tokio::spawn(async move {
        refresh_pools(refresh_cache, refresh_addr, shutdown_rx).await;
    });

    let sor_service = MySOR { quoter: quoter.clone() };
    let addr = "127.0.0.1:50052".parse()?;

    info!("SOR gRPC Server listening on {}", addr);

    Server::builder()
        .add_service(SorServiceServer::new(sor_service))
        .serve_with_shutdown(addr, async move {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install CTRL+C handler");
            info!("SOR: Ctrl+C pressed, starting graceful shutdown...");
            let _ = shutdown_tx.send(());
        })
        .await?;

    info!("SOR: Service shut down completely.");
    Ok(())
}
