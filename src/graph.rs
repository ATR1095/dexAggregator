use petgraph::graph::{NodeIndex, UnGraph};
use petgraph::visit::EdgeRef;
use crate::cache::GlobalPoolCache;
use log::{info, debug};
use std::collections::{HashMap, VecDeque, HashSet};

pub struct TokenGraph {
    pub graph: UnGraph<String, String>, // Node: Token Mint, Edge: Pool ID
    pub nodes: HashMap<String, NodeIndex>,
}

impl TokenGraph {
    pub fn new() -> Self {
        Self {
            graph: UnGraph::new_undirected(),
            nodes: HashMap::new(),
        }
    }

    pub fn build(&mut self, cache: &GlobalPoolCache) {
        for pool in cache.pools.iter() {
            // Only include pools that the instruction builder (quoter.rs) supports.
            // Currently supported: Orca Whirlpool and Meteora DLMM.
            // Raydium and others are skipped to prevent "InstructionError 101" or missing account errors.
            if pool.dex_label != "orca" && pool.dex_label != "meteora" {
                debug!("Graph: Skipping unsupported DEX pool {} ({})", pool.id, pool.dex_label);
                continue;
            }
            
            // Ensure pool has necessary accounts for building instructions
            if pool.dex_label == "orca" && (!pool.accounts.contains_key("pool_vault_a") || !pool.accounts.contains_key("pool_vault_b")) {
                continue;
            }
            if pool.dex_label == "meteora" && (!pool.accounts.contains_key("reserve_x") || !pool.accounts.contains_key("reserve_y")) {
                continue;
            }

            let node_a = *self.nodes.entry(pool.token_a.clone()).or_insert_with(|| {
                self.graph.add_node(pool.token_a.clone())
            });
            let node_b = *self.nodes.entry(pool.token_b.clone()).or_insert_with(|| {
                self.graph.add_node(pool.token_b.clone())
            });
            debug!("Graph: Adding edge {} - {} (Pool: {})", pool.token_a, pool.token_b, pool.id);
            self.graph.add_edge(node_a, node_b, pool.id.clone());
        }
        info!("Graph: Successfully built graph with {} edges", cache.pools.len());
    }

    pub fn find_routes(
        &self,
        token_in: &str,
        token_out: &str,
        max_hops: usize,
    ) -> Vec<Vec<String>> {
        let mut routes = Vec::new();
        let start_node = match self.nodes.get(token_in) {
            Some(n) => *n,
            None => {
                debug!("Graph: Start token {} not found in nodes", token_in);
                return routes;
            }
        };

        info!("Graph: Searching routes from {} to {} (nodes: {})", token_in, token_out, self.nodes.len());

        // Queue carries: (current_node, pool_path, visited_token_mints)
        // visited_tokens tracks every intermediate token we have passed through,
        // so we can reject paths that loop back to a previously seen token.
        let mut initial_visited = HashSet::new();
        initial_visited.insert(token_in.to_string());
        let mut queue: VecDeque<(NodeIndex, Vec<String>, HashSet<String>)> = VecDeque::new();
        queue.push_back((start_node, vec![], initial_visited));

        while let Some((current_node, path, visited_tokens)) = queue.pop_front() {
            if path.len() >= max_hops {
                continue;
            }

            for edge in self.graph.edges(current_node) {
                let neighbor = if edge.source() == current_node { edge.target() } else { edge.source() };
                let pool_id = edge.weight();
                let neighbor_token = &self.graph[neighbor];

                // 1. Avoid pool cycles: the same pool used twice in one path.
                if path.contains(pool_id) {
                    continue;
                }

                // 2. Avoid token revisits: if we have already passed through this
                //    token as an intermediate step, skip it.
                //    This blocks circular paths like SOL → MEW → SOL → USDC.
                //    token_out is exempt — reaching it is the whole point.
                if neighbor_token != token_out && visited_tokens.contains(neighbor_token) {
                    continue;
                }

                let mut new_path = path.clone();
                new_path.push(pool_id.clone());

                if neighbor_token == token_out {
                    routes.push(new_path);
                } else {
                    let mut new_visited = visited_tokens.clone();
                    new_visited.insert(neighbor_token.clone());
                    queue.push_back((neighbor, new_path, new_visited));
                }
            }
        }
        info!("Graph: Search complete. Found {} routes", routes.len());
        routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::cache::{GlobalPoolCache, PoolState, PoolType};

    fn make_pool(id: &str, token_a: &str, token_b: &str) -> PoolState {
        PoolState {
            id: id.to_string(),
            token_a: token_a.to_string(),
            token_b: token_b.to_string(),
            symbol_a: token_a.to_string(),
            symbol_b: token_b.to_string(),
            decimals_a: 9,
            decimals_b: 6,
            reserve_a: 1_000_000,
            reserve_b: 1_000_000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            dex_label: "orca".to_string(),
            clmm_data: None,
            lb_bin_data: None,
            accounts: {
                let mut h = HashMap::new();
                h.insert("pool_vault_a".to_string(), "vault_a".to_string());
                h.insert("pool_vault_b".to_string(), "vault_b".to_string());
                h
            },
        }
    }

    #[test]
    fn test_graph_and_routes() {
        let cache = GlobalPoolCache::new();
        cache.update_pool("pool1".to_string(), make_pool("pool1", "SOL", "USDC"));
        cache.update_pool("pool2".to_string(), make_pool("pool2", "USDC", "USDT"));

        let mut graph = TokenGraph::new();
        graph.build(&cache);

        let routes = graph.find_routes("SOL", "USDT", 3);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], vec!["pool1", "pool2"]);
    }

    #[test]
    fn test_no_token_revisit() {
        // SOL-MEW pool + MEW-SOL pool (a different pool address) + SOL-USDC pool.
        // Without the fix this would produce SOL -> MEW -> SOL -> USDC.
        // With the fix, that circular path must be blocked.
        let cache = GlobalPoolCache::new();
        cache.update_pool("sol_mew".to_string(),  make_pool("sol_mew",  "SOL", "MEW"));
        cache.update_pool("mew_sol".to_string(),  make_pool("mew_sol",  "MEW", "SOL"));
        cache.update_pool("sol_usdc".to_string(), make_pool("sol_usdc", "SOL", "USDC"));

        let mut graph = TokenGraph::new();
        graph.build(&cache);

        let routes = graph.find_routes("SOL", "USDC", 3);

        // Only the direct SOL -> USDC hop should survive.
        assert_eq!(routes.len(), 1, "Circular token path SOL->MEW->SOL->USDC must be filtered");
        assert_eq!(routes[0], vec!["sol_usdc"]);
    }

    #[test]
    fn test_multi_hop_valid() {
        // SOL -> BONK -> USDC (valid 2-hop, no token revisit) plus a direct hop.
        let cache = GlobalPoolCache::new();
        cache.update_pool("sol_bonk".to_string(),  make_pool("sol_bonk",  "SOL",  "BONK"));
        cache.update_pool("bonk_usdc".to_string(), make_pool("bonk_usdc", "BONK", "USDC"));
        cache.update_pool("sol_usdc".to_string(),  make_pool("sol_usdc",  "SOL",  "USDC"));

        let mut graph = TokenGraph::new();
        graph.build(&cache);

        let routes = graph.find_routes("SOL", "USDC", 3);

        // Both the direct hop and the 2-hop path must be found.
        assert_eq!(routes.len(), 2, "Should find both SOL->USDC and SOL->BONK->USDC");
    }
}
