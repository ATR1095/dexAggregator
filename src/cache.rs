use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PoolType {
    ConstantProduct,
    ConcentratedLiquidity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolState {
    pub id: String,
    pub token_a: String,
    pub token_b: String,
    pub symbol_a: String,
    pub symbol_b: String,
    pub decimals_a: u32,
    pub decimals_b: u32,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub pool_type: PoolType,
    pub fee_bps: u16,
    pub dex_label: String,
    pub clmm_data: Option<ClmmData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClmmData {
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub current_tick: i32,
    pub tick_spacing: u16,
    pub ticks: DashMap<i32, TickInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickInfo {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
}

pub struct GlobalPoolCache {
    pub pools: DashMap<String, PoolState>,
    pub symbols: DashMap<String, String>,  // Symbol -> Mint
    pub mints: DashMap<String, String>,    // Mint -> Symbol
    pub decimals: DashMap<String, u32>,    // Mint -> Decimals
}

impl GlobalPoolCache {
    pub fn new() -> Self {
        Self {
            pools: DashMap::new(),
            symbols: DashMap::new(),
            mints: DashMap::new(),
            decimals: DashMap::new(),
        }
    }

    // pub fn update_pool(&self, id: String, state: PoolState) {
    //     self.symbols.insert(state.symbol_a.to_uppercase(), state.token_a.clone());
    //     self.symbols.insert(state.symbol_b.to_uppercase(), state.token_b.clone());
    //     self.mints.insert(state.token_a.clone(), state.symbol_a.clone());
    //     self.mints.insert(state.token_b.clone(), state.symbol_b.clone());
    //     self.decimals.insert(state.token_a.clone(), state.decimals_a);
    //     self.decimals.insert(state.token_b.clone(), state.decimals_b);
    //     self.pools.insert(id, state);
    // }

    pub fn update_pool(&self, id: String, state: PoolState) {
    let sym_a = state.symbol_a.to_uppercase();
    let sym_b = state.symbol_b.to_uppercase();

    // 1. Only insert symbols if they aren't "UNKNOWN" 
    // 2. Only insert if they don't already exist (prevent overwriting good data with bad)
    if sym_a != "UNKNOWN" && !self.symbols.contains_key(&sym_a) {
        self.symbols.insert(sym_a, state.token_a.clone());
    }
    if sym_b != "UNKNOWN" && !self.symbols.contains_key(&sym_b) {
        self.symbols.insert(sym_b, state.token_b.clone());
    }

    // Always keep mint-to-symbol and decimals updated 
    // but consider adding a check to ensure state.token_a is actually a valid mint length
    if state.token_a.len() > 30 {
        self.mints.insert(state.token_a.clone(), state.symbol_a.clone());
        self.decimals.insert(state.token_a.clone(), state.decimals_a);
    }
    
    if state.token_b.len() > 30 {
        self.mints.insert(state.token_b.clone(), state.symbol_b.clone());
        self.decimals.insert(state.token_b.clone(), state.decimals_b);
    }

    // Update the actual pool state
    self.pools.insert(id, state);
}

    pub fn seed_common_tokens(&self) {
        let tokens = vec![
            ("So11111111111111111111111111111111111111112", "SOL", 9),
            ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6),
            ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "USDT", 6),
            ("mSoLzYSa7mSrib6Pqz9shqZ57n79m1Sjg7rBe39626S", "mSOL", 9),
            ("JUPyiwrS9fR9S9oiSgYpXG88K6zB42289cTSpmXkSBy", "JUP", 6),
            ("DezXAZ8z7PnrnRJjz3wXBoRgixqc6HG8J6YW7GZ68m7G", "BONK", 5),
            ("HZ1JovNiHvGr2UsFvSxH9N8gJHeK9NBeS4hR3mK9A5X4", "WETH", 8),
        ];

        for (mint, symbol, dec) in tokens {
            self.symbols.insert(symbol.to_string(), mint.to_string());
            self.mints.insert(mint.to_string(), symbol.to_string());
            self.decimals.insert(mint.to_string(), dec);
        }
    }

    pub fn get_decimals(&self, mint: &str) -> u32 {
        self.decimals.get(mint).map(|d| *d.value()).unwrap_or(0)
    }

    pub fn get_pool(&self, id: &str) -> Option<PoolState> {
        self.pools.get(id).map(|p| p.clone())
    }

    pub fn get_symbol_by_mint(&self, mint: &str) -> String {
        let symbol = self.mints.get(mint)
            .map(|s| s.value().clone())
            .unwrap_or_default();

        if symbol.is_empty() || symbol == "UNKNOWN" {
            // Return truncated mint if symbol not found or is restricted
            if mint.len() > 8 {
                format!("{}...", &mint[..8])
            } else {
                mint.to_string()
            }
        } else {
            symbol
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_normalization() {
        let cache = GlobalPoolCache::new();
        let state = PoolState {
            id: "pool1".to_string(),
            token_a: "mintA".to_string(),
            token_b: "mintB".to_string(),
            symbol_a: "AXSet".to_string(),
            symbol_b: "usdc".to_string(),
            decimals_a: 9,
            decimals_b: 6,
            reserve_a: 100,
            reserve_b: 100,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            dex_label: "test".to_string(),
            clmm_data: None,
        };

        cache.update_pool("pool1".to_string(), state);

        // Test mixed case lookup
        assert_eq!(cache.symbols.get("axset").map(|m| m.value().clone()), None); // Mixed case lookup in DashMap is case-sensitive
        // But our resolve_token logic takes care of upper-casing
        
        // Let's verify our manual resolution logic in main.rs would work
        let lookup_key = "AXSet".to_uppercase();
        assert_eq!(cache.symbols.get(&lookup_key).map(|m| m.value().clone()), Some("mintA".to_string()));
        
        let lookup_key_2 = "USDC".to_uppercase();
        assert_eq!(cache.symbols.get(&lookup_key_2).map(|m| m.value().clone()), Some("mintB".to_string()));
    }
}
