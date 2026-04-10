package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"sync"
)

type TokenMetadata struct {
	Symbol   string `json:"symbol"`
	Name     string `json:"name"`
	Decimals int    `json:"decimals"`
}

var (
	tokenCache = make(map[string]TokenMetadata)
	cacheMutex sync.RWMutex
	fallbackSymbols = map[string]TokenMetadata{
		"So11111111111111111111111111111111111111112": {Symbol: "SOL", Decimals: 9},
		"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": {Symbol: "USDC", Decimals: 6},
		"Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": {Symbol: "USDT", Decimals: 6}, // Main USDT
		"Es9vMFrzaDCSTMdUiJv865tPzXWty3XDsot628au7tvH": {Symbol: "USDT", Decimals: 6}, // Alternate USDT
		"mSoLzYSa7mSrib6Pqz9shqZ57n79m1Sjg7rBe39626S":  {Symbol: "mSOL", Decimals: 9},
		"JUPyiwrS9fR9S9oiSgYpXG88K6zB42289cTSpmXkSBy":  {Symbol: "JUP", Decimals: 6},
		"4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R": {Symbol: "RAY", Decimals: 6},
		"DezXAZ8z7PnrnRJjz3wXBoRgixqc6HG8J6YW7GZ68m7G": {Symbol: "BONK", Decimals: 5},
		"3bRTivrVsitbmCTGtqwp7hxXPsybkjn4XLNtPsHqa3zR": {Symbol: "LIKE", Decimals: 9},
		"HZ1JovNiHvGr2UsFvSxH9N8gJHeK9NBeS4hR3mK9A5X4": {Symbol: "WETH", Decimals: 8},
		"7dHb9SybS8n8VqxYEtVunBsS2S2unsqTdqXwt7edWVv5": {Symbol: "stSOL", Decimals: 9},
	}
)

func GetTokenSymbol(mint string) string {
	if meta, ok := fallbackSymbols[mint]; ok {
		return meta.Symbol
	}
	return "UNKNOWN"
}

func GetTokenDecimals(mint string) uint32 {
	if meta, ok := fallbackSymbols[mint]; ok {
		return uint32(meta.Decimals)
	}
	return 0
}

// RefreshTokenMetadata refreshes the cache from multiple reliable sources
func RefreshTokenMetadata() error {
	fmt.Println("Metadata: Refreshing token list from all available sources...")
	
	// Try Raydium V3 API (fresh Raydium tokens)
	refreshFromRaydium()
	
	// Try GitHub (thousands of tokens, but slightly older)
	refreshFromGitHub()

	return nil
}

func refreshFromRaydium() error {
	url := "https://api-v3.raydium.io/mint/list"
	fmt.Printf("Metadata: Fetching from Raydium: %s\n", url)
	resp, err := http.Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	var result struct {
		Data struct {
			MintList []struct {
				Address  string `json:"address"`
				Symbol   string `json:"symbol"`
				Name     string `json:"name"`
				Decimals int    `json:"decimals"`
			} `json:"mintList"`
		} `json:"data"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return err
	}

	cacheMutex.Lock()
	defer cacheMutex.Unlock()
	for _, t := range result.Data.MintList {
		tokenCache[t.Address] = TokenMetadata{
			Symbol:   t.Symbol,
			Name:     t.Name,
			Decimals: t.Decimals,
		}
	}
	fmt.Printf("Metadata: Added %d tokens from Raydium\n", len(result.Data.MintList))
	return nil
}

func refreshFromGitHub() error {
	url := "https://raw.githubusercontent.com/solana-labs/token-list/main/src/tokens/solana.tokenlist.json"
	fmt.Printf("Metadata: Fetching from GitHub: %s\n", url)
	resp, err := http.Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	var list struct {
		Tokens []struct {
			Address  string `json:"address"`
			Symbol   string `json:"symbol"`
			Name     string `json:"name"`
			Decimals int    `json:"decimals"`
		} `json:"tokens"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&list); err != nil {
		return err
	}

	cacheMutex.Lock()
	defer cacheMutex.Unlock()
	for _, t := range list.Tokens {
		tokenCache[t.Address] = TokenMetadata{
			Symbol:   t.Symbol,
			Name:     t.Name,
			Decimals: t.Decimals,
		}
	}
	fmt.Printf("Metadata: Added %d tokens from GitHub\n", len(list.Tokens))
	return nil
}

func GetSymbolFromCache(mint string) string {
	cacheMutex.RLock()
	defer cacheMutex.RUnlock()
	if t, ok := tokenCache[mint]; ok {
		return t.Symbol
	}
	return GetTokenSymbol(mint)
}

func GetDecimalsFromCache(mint string) uint32 {
	cacheMutex.RLock()
	defer cacheMutex.RUnlock()
	if t, ok := tokenCache[mint]; ok {
		return uint32(t.Decimals)
	}
	return GetTokenDecimals(mint)
}
