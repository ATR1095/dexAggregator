package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
)

type JupiterImporter struct {
	Registry *PoolRegistry
}

func NewJupiterImporter(registry *PoolRegistry) *JupiterImporter {
	return &JupiterImporter{Registry: registry}
}

type poolMetadata struct {
	MintA   string
	MintB   string
	SymbolA string
	SymbolB string
	TVL     float64
	VaultA  string
	VaultB  string
}

func (ji *JupiterImporter) ImportTopPools(ctx context.Context, wp *WorkerPool) error {
	log.Println("JupiterImporter: Fetching top liquidity pools from Raydium API...")

	// Essential pools with hardcoded metadata and VAULTS for immediate availability
	poolEntries := map[string]poolMetadata{
		"58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2": {
			MintA: "So11111111111111111111111111111111111111112", MintB: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 
			SymbolA: "SOL", SymbolB: "USDC", TVL: 7000000, 
			VaultA: "CxoLXAkNEexLHK5ukudpfTWQ7okXSSsuJyYH267W9JE3", VaultB: "GXQQoBJxXLyotFBmD6UwAzbizeT2D5UJDQwBB7HcUsHM",
		},
		"GmaDNMWsTYWjaXVBjJTHNmCWAKU6cn5hhtWWYEZt4odo": {
			MintA: "3bRTivrVsitbmCTGtqwp7hxXPsybkjn4XLNtPsHqa3zR", MintB: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 
			SymbolA: "LIKE", SymbolB: "USDC", TVL: 100000,
			VaultA: "Crn5beRFeyj4Xw13E2wdJ9YkkLLEZzKYmtTV4LFDx3MN", VaultB: "3WptgZZu34aiDrLMUiPntTYZGNZ72yT1yxHYxSdbTArX",
		},
	}

	// Fetch top 100 standard pools
	poolTypes := []string{"standard", "cpmm"}
	for _, pType := range poolTypes {
		url := fmt.Sprintf("https://api-v3.raydium.io/pools/info/list?poolType=%s&poolSortField=liquidity&sortType=desc&pageSize=50&page=1", pType)
		resp, err := http.Get(url)
		if err != nil {
			continue
		}
		defer resp.Body.Close()
		var result struct {
			Data struct {
				Data []struct {
					ID       string  `json:"id"`
					Program  string  `json:"programId"`
					MintA    struct{ Address string `json:"address"` } `json:"mintA"`
					MintB    struct{ Address string `json:"address"` } `json:"mintB"`
					SymbolA  string  `json:"symbolA"`
					SymbolB  string  `json:"symbolB"`
					TVL      float64 `json:"tvl"`
					VaultA   string  `json:"vaultA"`
					VaultB   string  `json:"vaultB"`
				} `json:"data"`
			} `json:"data"`
		}
		if err := json.NewDecoder(resp.Body).Decode(&result); err == nil {
			for _, p := range result.Data.Data {
				existing, exists := poolEntries[p.ID]
				
				meta := poolMetadata{
					MintA:   p.MintA.Address,
					MintB:   p.MintB.Address,
					SymbolA: p.SymbolA,
					SymbolB: p.SymbolB,
					TVL:     p.TVL,
					VaultA:  p.VaultA,
					VaultB:  p.VaultB,
				}
				
				// Preserve hardcoded vaults if any
				if exists && existing.VaultA != "" {
					meta.VaultA = existing.VaultA
					meta.VaultB = existing.VaultB
				}
				
				poolEntries[p.ID] = meta
			}
		}
	}

	count := 0
	var seededPools []string
	for id, m := range poolEntries {
		// 1. Persistence (Optional fallback)
		if ji.Registry != nil {
			err := ji.Registry.UpsertPool(ctx, id, "raydium", m.MintA, m.MintB, m.SymbolA, m.SymbolB, m.TVL)
			if err != nil {
				log.Printf("JupiterImporter: Persistence failed for %s: %v", id, err)
			} else {
				ji.Registry.Pool.Exec(ctx, "UPDATE monitored_pools SET is_active = TRUE WHERE address = $1", id) 
			}
		}

		// 2. Redis Seeding (CRITICAL for SOR)
		key := fmt.Sprintf("pool:%s", id)
		wp.Redis.HSet(ctx, key, map[string]interface{}{
			"token_a":    m.MintA,
			"token_b":    m.MintB,
			"symbol_a":   m.SymbolA,
			"symbol_b":   m.SymbolB,
			"decimals_a": GetDecimalsFromCache(m.MintA),
			"decimals_b": GetDecimalsFromCache(m.MintB),
			"dex_type":   "raydium",
		})

		// 2b. Final Vault Registration (Guarantee registration for hardcoded/API pools)
		if m.VaultA != "" && m.VaultB != "" {
			wp.RegisterPoolVaults(id, m.VaultA, m.VaultB)
		}
		
		seededPools = append(seededPools, id)
		count++
	}

	// 3. Update WorkerPool so these pools are immediately discoverable via gRPC
	if len(seededPools) > 0 {
		wp.SetMonitoredPools(seededPools)
	}

	log.Printf("JupiterImporter: Successfully registered and seeded %d pools.", count)
	return nil
}
