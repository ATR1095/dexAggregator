pub mod cache;
pub mod math;
pub mod graph;
pub mod quoter;

use crate::cache::{GlobalPoolCache, PoolState, PoolType};
use crate::quoter::Quoter;
use log::{info, warn, debug};
use oracle::price_oracle_client::PriceOracleClient;
use sor::sor_service_server::{SorService, SorServiceServer};
use std::sync::Arc;
use tonic::{transport::Server, Request, Response, Status};

pub mod oracle {
    tonic::include_proto!("oracle");
}

pub mod sor {
    tonic::include_proto!("sor");
}

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
        
        // Manual override for common tokens if cache is not yet ready
        match token_upper.as_str() {
            "SOL" => return "So11111111111111111111111111111111111111112".to_string(),
            "USDC" => return "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            "USDT" => return "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".to_string(),
            "MSOL" => return "mSoLzYSa7mSrib6Pqz9shqZ57n79m1Sjg7rBe39626S".to_string(),
            "LIKE" => return "3bRTivrVsitbmCTGtqwp7hxXPsybkjn4XLNtPsHqa3zR".to_string(),
            _ => {}
        }

        // Otherwise, look up in the symbols cache
        if let Some(mint) = self.quoter.cache.symbols.get(&token_upper) {
            let m: String = mint.value().clone();
            return m;
        }
        
        token.to_string()
    }


    fn atomic_to_human(&self, amount: u128, decimals: u32) -> String {
        if decimals == 0 {
            return amount.to_string();
        }
        let divisor = 10u128.pow(decimals);
        let integer = amount / divisor;
        let fractional = amount % divisor;
        if fractional == 0 {
            return integer.to_string();
        }
        format!("{}.{:0width$}", integer, fractional, width = decimals as usize).trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[tonic::async_trait]
impl SorService for MySOR {
    async fn quote(
        &self,
        request: Request<sor::QuoteRequest>,
    ) -> Result<Response<sor::QuoteResponse>, Status> {
        let req = request.into_inner();
        let amount_in = req.amount.parse::<u64>().map_err(|_| Status::invalid_argument("Invalid amount"))?;
        
        let input_mint = self.resolve_token(&req.input_token);
        let output_mint = self.resolve_token(&req.output_token);

        let quote = self.quoter.get_quote(&input_mint, &output_mint, amount_in, 16)
            .map_err(|e| Status::internal(format!("Quote error: {}", e)))?;

        let input_decimals = self.quoter.cache.get_decimals(&input_mint);
        let output_decimals = self.quoter.cache.get_decimals(&output_mint);
        info!("Quote: {} -> {} (amount_in={}, human_in={})", req.input_token, req.output_token, amount_in, self.atomic_to_human(amount_in as u128, input_decimals));

        let mut token_path = Vec::new();
        if let Some(route) = quote.routes.get(0) {
            token_path.push(self.quoter.cache.get_symbol_by_mint(&input_mint));
            let mut current_token = input_mint.clone();
            for pool_id in &route.pool_ids {
                if let Some(pool) = self.quoter.cache.get_pool(pool_id) {
                    if pool.token_a == current_token {
                        current_token = pool.token_b.clone();
                    } else {
                        current_token = pool.token_a.clone();
                    }
                    token_path.push(self.quoter.cache.get_symbol_by_mint(&current_token));
                }
            }
        }

        let mut response = sor::QuoteResponse {
            input_token: req.input_token,
            output_token: req.output_token,
            input_amount: req.amount.clone(),
            output_amount: quote.amount_out.to_string(),
            path: quote.routes.get(0).map(|r| r.pool_ids.clone()).unwrap_or_default(),
            price_impact: 0.0,
            token_path,
            human_input_amount: self.atomic_to_human(amount_in as u128, input_decimals),
            human_output_amount: self.atomic_to_human(quote.amount_out as u128, output_decimals),
        };

        // Populate new fields for the best route (legacy QuoteResponse)
        // Note: For now, QuoteResponse only has an overall price_impact.
        // We'll calculate it for the best route.
        if let Some(best_route) = quote.routes.first() {
            response.price_impact = best_route.price_impact;
        }

        Ok(Response::new(response))
    }

    async fn list_tokens(
        &self,
        _request: Request<sor::ListTokensRequest>,
    ) -> Result<Response<sor::ListTokensResponse>, Status> {
        let mut seen_a = std::collections::HashSet::new();
        let mut seen_b = std::collections::HashSet::new();
        let mut token_a_list = Vec::new();
        let mut token_b_list = Vec::new();

        for entry in self.quoter.cache.pools.iter() {
            let pool = entry.value();

            // Collect unique token_a entries (filter out UNKNOWN)
            if pool.symbol_a.to_uppercase() != "UNKNOWN" && seen_a.insert(pool.token_a.clone()) {
                token_a_list.push(sor::TokenInfo {
                    mint: pool.token_a.clone(),
                    symbol: pool.symbol_a.clone(),
                    decimals: pool.decimals_a,
                });
            }

            // Collect unique token_b entries (filter out UNKNOWN)
            if pool.symbol_b.to_uppercase() != "UNKNOWN" && seen_b.insert(pool.token_b.clone()) {
                token_b_list.push(sor::TokenInfo {
                    mint: pool.token_b.clone(),
                    symbol: pool.symbol_b.clone(),
                    decimals: pool.decimals_b,
                });
            }
        }

        // Sort both lists alphabetically by symbol for consistent ordering
        token_a_list.sort_by(|a, b| a.symbol.to_lowercase().cmp(&b.symbol.to_lowercase()));
        token_b_list.sort_by(|a, b| a.symbol.to_lowercase().cmp(&b.symbol.to_lowercase()));

        info!("ListTokens: returning {} token_a, {} token_b", token_a_list.len(), token_b_list.len());

        Ok(Response::new(sor::ListTokensResponse {
            token_a: token_a_list,
            token_b: token_b_list,
        }))
    }

    async fn swap(
        &self,
        request: Request<sor::SwapRequest>,
    ) -> Result<Response<sor::SwapResponse>, Status> {
        let req = request.into_inner();
        let amount_in = req.amount.parse::<u64>().map_err(|_| Status::invalid_argument("Invalid amount"))?;
        
        let input_mint = self.resolve_token(&req.input_token);
        let output_mint = self.resolve_token(&req.output_token);

        let input_decimals = self.quoter.cache.get_decimals(&input_mint);
        let output_decimals = self.quoter.cache.get_decimals(&output_mint);

        let mut quote = self.quoter.get_quote(&input_mint, &output_mint, amount_in, 16)
            .map_err(|e| Status::internal(format!("Quote error: {}", e)))?;

        // Sort routes descending by amount_out: best route (highest output) first.
        quote.routes.sort_by(|a, b| b.amount_out.cmp(&a.amount_out));

        // Build the token path for each route plan.
        let build_token_path = |pool_ids: &Vec<String>| -> Vec<String> {
            let mut path = Vec::new();
            path.push(self.quoter.cache.get_symbol_by_mint(&input_mint));
            let mut current_token = input_mint.clone();
            for pool_id in pool_ids {
                if let Some(pool) = self.quoter.cache.get_pool(pool_id) {
                    if pool.token_a == current_token {
                        current_token = pool.token_b.clone();
                    } else {
                        current_token = pool.token_a.clone();
                    }
                    path.push(self.quoter.cache.get_symbol_by_mint(&current_token));
                }
            }
            path
        };

        // Build the detailed routes (already sorted best-first).
        let detailed_routes: Vec<sor::DetailedRoute> = quote.routes.iter().map(|plan| {
            let token_path = build_token_path(&plan.pool_ids);
            sor::DetailedRoute {
                token_path,
                pool_ids: plan.pool_ids.clone(),
                amount_in: plan.amount_in.to_string(),
                amount_out: plan.amount_out.to_string(),
                human_amount_in: self.atomic_to_human(plan.amount_in as u128, input_decimals),
                human_amount_out: self.atomic_to_human(plan.amount_out as u128, output_decimals),
                price_impact: plan.price_impact,
                dex_labels: plan.pool_ids.iter().map(|id| {
                    self.quoter.cache.get_pool(id).map(|p| p.dex_label).unwrap_or_default()
                }).collect(),
            }
        }).collect();

        // Best route token path (index 0 after sort = highest amount_out = best).
        let best_token_path = quote.routes.first()
            .map(|plan| build_token_path(&plan.pool_ids))
            .unwrap_or_default();

        let route_str = best_token_path.join(" -> ");
        Ok(Response::new(sor::SwapResponse {
            tx_hash: "0x...".to_string(),
            status: "success".to_string(),
            message: format!("Swap initiated for {} to {} via [{}]", req.input_token, req.output_token, route_str),
            route: best_token_path,
            output_amount: quote.amount_out.to_string(),
            human_output_amount: self.atomic_to_human(quote.amount_out as u128, output_decimals),
            routes: detailed_routes,
        }))
    }
}

async fn refresh_pools(
    cache: Arc<GlobalPoolCache>,
    oracle_addr: String,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(15));
    loop {
        interval.tick().await;
        debug!("SOR: Starting pool refresh from Oracle...");

        let mut client = match PriceOracleClient::connect(oracle_addr.clone()).await {
            Ok(c) => c,
            Err(e) => {
                warn!("SOR: Failed to connect to Price Oracle for refresh: {}", e);
                continue;
            }
        };

        let pools_resp = client.get_monitored_pools(oracle::Empty {}).await;
        if let Ok(response) = pools_resp {
            let pools = response.into_inner().pool_ids;
            if pools.is_empty() {
                debug!("SOR: Oracle reported 0 monitored pools. Skipping update.");
                continue;
            }
            
            info!("SOR: Discovered {} pools from Oracle", pools.len());
            for pool_id in pools {
                let request = tonic::Request::new(oracle::PoolRequest {
                    pool_id: pool_id.clone(),
                });
                if let Ok(res) = client.get_pool_reserves(request).await {
                    let update = res.into_inner();
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
                        pool_type: PoolType::ConstantProduct,
                        fee_bps: 30,
                        dex_label: update.dex_label,
                        clmm_data: None,
                    });
                }
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
    
    // Spawn background refresh task
    let refresh_cache = cache.clone();
    let refresh_addr = oracle_addr.clone();
    tokio::spawn(async move {
        refresh_pools(refresh_cache, refresh_addr).await;
    });

    let sor_service = MySOR { quoter: quoter.clone() };
    let addr = "127.0.0.1:50052".parse()?;
    info!("SOR gRPC Server listening on {}", addr);

    Server::builder()
        .add_service(SorServiceServer::new(sor_service))
        .serve(addr)
        .await?;

    Ok(())
}
