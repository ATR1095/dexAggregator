use petgraph::graph::{NodeIndex, UnGraph};
use petgraph::visit::EdgeRef;
use crate::cache::GlobalPoolCache;
use log::{info, debug};
use std::collections::{HashMap, VecDeque};

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

        let mut queue: VecDeque<(NodeIndex, Vec<String>)> = VecDeque::new();
        queue.push_back((start_node, vec![]));
        
        while let Some((current_node, path)) = queue.pop_front() {
            if path.len() >= max_hops {
                continue;
            }

            for edge in self.graph.edges(current_node) {
                let neighbor = if edge.source() == current_node { edge.target() } else { edge.source() };
                let pool_id = edge.weight();
                
                // Avoid cycles
                if path.contains(pool_id) {
                    continue;
                }
                
                let mut new_path = path.clone();
                new_path.push(pool_id.clone());

                if self.graph[neighbor] == token_out {
                    routes.push(new_path);
                } else {
                    queue.push_back((neighbor, new_path));
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
    use crate::cache::{GlobalPoolCache, PoolState, PoolType};

    #[test]
    fn test_graph_and_routes() {
        let cache = GlobalPoolCache::new();
        cache.update_pool("pool1".to_string(), PoolState {
            id: "pool1".to_string(),
            token_a: "SOL".to_string(),
            token_b: "USDC".to_string(),
            symbol_a: "SOL".to_string(),
            symbol_b: "USDC".to_string(),
            decimals_a: 9,
            decimals_b: 6,
            reserve_a: 1000,
            reserve_b: 1000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            clmm_data: None,
        });

        cache.update_pool("pool2".to_string(), PoolState {
            id: "pool2".to_string(),
            token_a: "USDC".to_string(),
            token_b: "USDT".to_string(),
            symbol_a: "USDC".to_string(),
            symbol_b: "USDT".to_string(),
            decimals_a: 6,
            decimals_b: 6,
            reserve_a: 1000,
            reserve_b: 1000,
            pool_type: PoolType::ConstantProduct,
            fee_bps: 30,
            clmm_data: None,
        });

        let mut graph = TokenGraph::new();
        graph.build(&cache);

        let routes = graph.find_routes("SOL", "USDT", 3);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], vec!["pool1", "pool2"]);
    }
}
