package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"sort"
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
	VaultA    string
	VaultB    string
	DexType   string
	DecimalsA uint32
	DecimalsB uint32
}

func (ji *JupiterImporter) ImportTopPools(ctx context.Context, wp *WorkerPool) error {
	log.Println("JupiterImporter: Fetching top liquidity pools from Raydium API...")

	// Essential pools with hardcoded metadata and VAULTS for immediate availability.
	// Vault addresses left empty for auto-discovery where unknown.
	poolEntries := map[string]poolMetadata{
		"58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2": {
			MintA: "So11111111111111111111111111111111111111112", MintB: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
			SymbolA: "SOL", SymbolB: "USDC", TVL: 7000000,
			VaultA: "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz", VaultB: "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz", DexType: "raydium", DecimalsA: 9, DecimalsB: 6,
		},
		// PONKE/SOL: Raydium API returns empty symbolA/symbolB for this pool, so we hardcode
		// the symbols here. Vaults are left empty and auto-discovered via RPC on startup.
		"5uTwG3y3F5cx4YkodgTjWEHDrX5HDKZ5bZZ72x8eQ6zE": {
			MintA: "5z3EqYQo9HiCEs3R84RCDMu2n7anpDMxRhdK8PSWmrRC", MintB: "So11111111111111111111111111111111111111112",
			SymbolA: "PONKE", SymbolB: "SOL", TVL: 1000000,
			VaultA: "", VaultB: "", DexType: "raydium", DecimalsA: 9, DecimalsB: 9,
		},
		"GmaDNMWsTYWjaXVBjJTHNmCWAKU6cn5hhtWWYEZt4odo": {
			MintA: "3bRTivrVsitbmCTGtqwp7hxXPsybkjn4XLNtPsHqa3zR", MintB: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
			SymbolA: "LIKE", SymbolB: "USDC", TVL: 100000,
			VaultA: "8LoHX6f6bMdQVs4mThoH2KwX2dQDSkqVFADi4ZjDQv9T", VaultB: "2Fwm8M8vuPXEXxvKz98VdawDxsK9W8uRuJyJhvtRdhid", DexType: "raydium", DecimalsA: 9, DecimalsB: 6,
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
					MintA    struct{ Address string `json:"address"`; Decimals uint32 `json:"decimals"` } `json:"mintA"`
					MintB    struct{ Address string `json:"address"`; Decimals uint32 `json:"decimals"` } `json:"mintB"`
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
					VaultA:    p.VaultA,
					VaultB:    p.VaultB,
					DexType:   "raydium",
					DecimalsA: p.MintA.Decimals,
					DecimalsB: p.MintB.Decimals,
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

	// Fetch top 50 Orca Pools
	orcaURL := "https://api.mainnet.orca.so/v1/whirlpool/list"
	log.Printf("JupiterImporter: Fetching from Orca API: %s\n", orcaURL)
	
	reqOrca, _ := http.NewRequest("GET", orcaURL, nil)
	// Orca's API blocks default Go-http-client via Cloudflare, so we use a standard browser UA
	reqOrca.Header.Set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
	
	respOrca, errOrca := http.DefaultClient.Do(reqOrca)
	if errOrca == nil {
		defer respOrca.Body.Close()
		var orcaResult struct {
			Whirlpools []struct {
				Address string  `json:"address"`
				TokenA  struct{ Mint string `json:"mint"`; Symbol string `json:"symbol"`; Decimals uint32 `json:"decimals"` } `json:"tokenA"`
				TokenB  struct{ Mint string `json:"mint"`; Symbol string `json:"symbol"`; Decimals uint32 `json:"decimals"` } `json:"tokenB"`
				TVL     float64 `json:"tvl"`
			} `json:"whirlpools"`
		}
		if err := json.NewDecoder(respOrca.Body).Decode(&orcaResult); err == nil {
			sort.Slice(orcaResult.Whirlpools, func(i, j int) bool {
				return orcaResult.Whirlpools[i].TVL > orcaResult.Whirlpools[j].TVL
			})
			limit := 50
			if len(orcaResult.Whirlpools) < limit {
				limit = len(orcaResult.Whirlpools)
			}
			for _, p := range orcaResult.Whirlpools[:limit] {
				poolEntries[p.Address] = poolMetadata{
					MintA:   p.TokenA.Mint,
					MintB:   p.TokenB.Mint,
					SymbolA: p.TokenA.Symbol,
					SymbolB: p.TokenB.Symbol,
					TVL:       p.TVL,
					VaultA:    "",
					VaultB:    "",
					DexType:   "orca",
					DecimalsA: p.TokenA.Decimals,
					DecimalsB: p.TokenB.Decimals,
				}
			}
			log.Printf("JupiterImporter: Added top %d Orca pools", limit)
		} else {
			log.Printf("JupiterImporter: Error parsing Orca response: %v", err)
		}
	} else {
		log.Printf("JupiterImporter: Error fetching Orca API: %v", errOrca)
	}

	// Fetch top 50 Meteora DLMM Pools
	meteoraURL := "https://dlmm.datapi.meteora.ag/pools?page=1&page_size=50&sort_by=tvl:desc"
	log.Printf("JupiterImporter: Fetching from Meteora API: %s\n", meteoraURL)
	
	reqMet, _ := http.NewRequest("GET", meteoraURL, nil)
	reqMet.Header.Set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
	
	respMet, errMet := http.DefaultClient.Do(reqMet)
	if errMet == nil {
		defer respMet.Body.Close()
		if respMet.StatusCode != http.StatusOK {
			log.Printf("JupiterImporter: Meteora API returned status %d", respMet.StatusCode)
		}
		var metResult struct {
			Data []struct {
				Address string  `json:"address"`
				TokenX  struct{ Address string `json:"address"`; Symbol string `json:"symbol"`; Decimals uint32 `json:"decimals"` } `json:"token_x"`
				TokenY  struct{ Address string `json:"address"`; Symbol string `json:"symbol"`; Decimals uint32 `json:"decimals"` } `json:"token_y"`
				ReserveX string `json:"reserve_x"`
				ReserveY string `json:"reserve_y"`
				TVL      float64 `json:"tvl"`
			} `json:"data"`
		}
		if err := json.NewDecoder(respMet.Body).Decode(&metResult); err == nil {
			for _, p := range metResult.Data {
				poolEntries[p.Address] = poolMetadata{
					MintA:     p.TokenX.Address,
					MintB:     p.TokenY.Address,
					SymbolA:   p.TokenX.Symbol,
					SymbolB:   p.TokenY.Symbol,
					TVL:       p.TVL,
					VaultA:    p.ReserveX,
					VaultB:    p.ReserveY,
					DexType:   "meteora",
					DecimalsA: p.TokenX.Decimals,
					DecimalsB: p.TokenY.Decimals,
				}
			}
			log.Printf("JupiterImporter: Added top %d Meteora DLMM pools", len(metResult.Data))
		} else {
			log.Printf("JupiterImporter: Error parsing Meteora response: %v", err)
		}
	} else {
		log.Printf("JupiterImporter: Error fetching Meteora API: %v", errMet)
	}

	count := 0
	var seededPools []string
	for id, m := range poolEntries {
		// 1. Persistence (Optional fallback)
		if ji.Registry != nil {
			err := ji.Registry.UpsertPool(ctx, id, m.DexType, m.MintA, m.MintB, m.SymbolA, m.SymbolB, m.TVL)
			if err != nil {
				log.Printf("JupiterImporter: Persistence failed for %s: %v", id, err)
			} else {
				ji.Registry.Pool.Exec(ctx, "UPDATE monitored_pools SET is_active = TRUE WHERE address = $1", id) 
			}
		}

		// 2. Redis Seeding (CRITICAL for downstream consumers)
		key := fmt.Sprintf("pool:%s", id)
		decA := GetDecimalsFromCache(m.MintA)
		if decA == 0 { decA = m.DecimalsA }
		decB := GetDecimalsFromCache(m.MintB)
		if decB == 0 { decB = m.DecimalsB }

		wp.Redis.HSet(ctx, key, map[string]interface{}{
			"token_a":    m.MintA,
			"token_b":    m.MintB,
			"symbol_a":   m.SymbolA,
			"symbol_b":   m.SymbolB,
			"decimals_a": decA,
			"decimals_b": decB,
			"dex_type":   m.DexType,
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
