package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"priceOracleService/geyser" // Local vendored Helius types

	"github.com/joho/godotenv"
	"github.com/mr-tron/base58"
	"github.com/redis/go-redis/v9"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/metadata"
)

func main() {
	if err := godotenv.Load(); err != nil {
		log.Println("Warning: .env file not found, using system environment variables")
	}

	yellowstoneURL := os.Getenv("YELLOWSTONE_GRPC_URL")
	xToken := os.Getenv("YELLOWSTONE_X_TOKEN")
	redisAddr := os.Getenv("REDIS_ADDR")
	postgresURL := os.Getenv("POSTGRES_URL")

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Initialize Postgres Pool Registry
	registry, err := NewPoolRegistry(ctx, postgresURL)
	if err != nil {
		log.Printf("Warning: Failed to initialize Postgres registry: %v. Using hardcoded pools.", err)
	}

	// Initialize Metadata
	go func() {
		log.Println("Metadata: Starting background token list fetch...")
		for {
			if err := RefreshTokenMetadata(); err == nil {
				break
			}
			time.Sleep(10 * time.Second)
		}
	}()

	redisClient := redis.NewClient(&redis.Options{Addr: redisAddr, DB: 0})
	workerPool := NewWorkerPool(redisClient, 20, 10000)
	workerPool.Registry = registry
	workerPool.Start(ctx)
	importer := NewJupiterImporter(registry)

	// Load monitored pools
	// Even if registry fails, the importer should at least seed Redis with essential pools
	if importer != nil {
		importer.ImportTopPools(ctx, workerPool)
	}

	if registry != nil {
		if pools, err := registry.GetActivePools(ctx); err == nil && len(pools) > 0 {
			workerPool.SetMonitoredPools(pools)
		} else if err != nil {
			log.Printf("Warning: Failed to load pools from DB: %v. Using fallback discovery.", err)
		}
	}

	// Double check we have SOMETHING to monitor, if not, wait and try discovery again
	if len(workerPool.GetPoolList()) == 0 {
		log.Println("Warning: No pools discovered yet. Starting sync sequence anyway...")
	}
	
	go syncInitialPools(ctx, workerPool)

	// GEYSER START
	go refreshPoolsLoop(ctx, registry, workerPool)
	go executeLiveLoop(ctx, yellowstoneURL, xToken, workerPool)
	go runRPCPooling(ctx, xToken, workerPool) // Always run fallback polling
	go healUnknownSymbolsLoop(ctx, registry)

	// Start gRPC Server for SOR
	StartGRPCServer("50051", workerPool, redisClient)

	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, syscall.SIGINT, syscall.SIGTERM)
	<-sigChan
	log.Println("Shutting down Price Oracle Service...")
	cancel()
}

func refreshPoolsLoop(ctx context.Context, registry *PoolRegistry, wp *WorkerPool) {
	if registry == nil {
		return
	}

	ticker := time.NewTicker(60 * time.Second)
	defer ticker.Stop()

	for {
		pools, err := registry.GetActivePools(ctx)
		if err != nil {
			log.Printf("Failed to refresh pools from registry: %v", err)
		} else {
			wp.SetMonitoredPools(pools)
		}

		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

func executeLiveLoop(ctx context.Context, url, token string, wp *WorkerPool) {
	for {
		select {
		case <-ctx.Done():
			return
		default:
			err := runGeyserStream(ctx, url, token, wp)
			if err != nil {
				log.Printf("Geyser stream failed/disconnected: %v. Retrying in 10s...", err)
				time.Sleep(10 * time.Second)
			}
		}
	}
}

func runGeyserStream(ctx context.Context, url, token string, wp *WorkerPool) error {
	log.Printf("GEYSER ACTIVE: Connecting to %s...", url)

	conn, err := grpc.Dial(url, grpc.WithTransportCredentials(credentials.NewTLS(&tls.Config{})))
	if err != nil {
		return fmt.Errorf("dial error: %w", err)
	}
	defer conn.Close()

	client := geyser.NewGeyserClient(conn)
	md := metadata.Pairs("x-token", token)
	streamCtx := metadata.NewOutgoingContext(ctx, md)

	stream, err := client.Subscribe(streamCtx)
	if err != nil {
		return fmt.Errorf("subscribe error: %w", err)
	}

	commitment := geyser.CommitmentLevel_PROCESSED
	req := &geyser.SubscribeRequest{
		Accounts: map[string]*geyser.SubscribeRequestFilterAccounts{
			"dex_programs": {
				Owner: []string{RaydiumProgramID, OrcaProgramID},
			},
			"vaults": {
				Owner: []string{TokenProgramID, "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"},
			},
		},
		Slots: map[string]*geyser.SubscribeRequestFilterSlots{"all": {}},
		Ping:  &geyser.SubscribeRequestPing{},
		Commitment: &commitment,
	}

	if err := stream.Send(req); err != nil {
		return fmt.Errorf("send error: %w", err)
	}

	log.Println("SUCCESS: Geyser stream established. PROGRAM-LEVEL MONITORING ACTIVE.")

	for {
		resp, err := stream.Recv()
		if err != nil {
			return err
		}

		if ping := resp.GetPing(); ping != nil {
			continue
		}

		accountUpdate := resp.GetAccount()
		if accountUpdate == nil || accountUpdate.Account == nil {
			continue
		}

		acc := accountUpdate.Account
		poolAddr := base58.Encode(acc.Pubkey)
		
		isMonitored := wp.IsMonitored(poolAddr)

		wp.VaultMutex.RLock()
		_, isVault := wp.VaultToPool[poolAddr]
		wp.VaultMutex.RUnlock()

		if !isMonitored && !isVault {
			continue
		}

		wp.PushUpdate(&RawUpdate{
			ProgramID: base58.Encode(acc.Owner),
			Data:      acc.Data,
			Slot:      accountUpdate.Slot,
			PoolAddr:  poolAddr,
		})
	}
}

func runRPCPooling(ctx context.Context, token string, wp *WorkerPool) {
	url := "https://mainnet.helius-rpc.com/?api-key=" + token
	ticker := time.NewTicker(15 * time.Second)
	defer ticker.Stop()

	// 1. FAST TRACK: Immediate sync of essential pools
	log.Println("[Polling] FAST-TRACK: Initializing essential pool reserves...")
	fetchEssentialPools(ctx, url, wp)

	// 2. Initial full fetch
	log.Println("[Polling] Initial full sync of pools and vaults...")
	fetchReservesRPC(ctx, url, wp)
	fetchVaultBalancesRPC(ctx, url, wp)

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			log.Println("[Polling] Refreshing both pool states and vault balances...")
			fetchReservesRPC(ctx, url, wp)
			fetchVaultBalancesRPC(ctx, url, wp)
		}
	}
}

func fetchEssentialPools(ctx context.Context, url string, wp *WorkerPool) {
	essentials := []string{
		"58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2", // SOL/USDC
		"GmaDNMWsTYWjaXVBjJTHNmCWAKU6cn5hhtWWYEZt4odo", // LIKE/USDC
		"9pxP44otrjS7o3mxwsF9z4PBzC7o7dePbvDwnLidS8No", // mSOL/SOL
	}
	for _, addr := range essentials {
		fetchSingleAccountRPC(ctx, url, wp, addr)
	}
}

func fetchReservesRPC(ctx context.Context, url string, wp *WorkerPool) {
	pools := wp.GetPoolList()
	if len(pools) == 0 {
		return
	}
	
	const batchSize = 25
	for i := 0; i < len(pools); i += batchSize {
		end := i + batchSize
		if end > len(pools) {
			end = len(pools)
		}
		chunk := pools[i:end]
		fetchAndPushRPC(ctx, url, wp, chunk)
		time.Sleep(50 * time.Millisecond)
	}
}

func fetchVaultBalancesRPC(ctx context.Context, url string, wp *WorkerPool) {
	wp.VaultMutex.RLock()
	var vaultAddrs []string
	for v := range wp.VaultToPool {
		vaultAddrs = append(vaultAddrs, v)
	}
	wp.VaultMutex.RUnlock()

	if len(vaultAddrs) == 0 {
		return
	}

	const batchSize = 25
	for i := 0; i < len(vaultAddrs); i += batchSize {
		end := i + batchSize
		if end > len(vaultAddrs) {
			end = len(vaultAddrs)
		}
		chunk := vaultAddrs[i:end]
		fetchAndPushRPC(ctx, url, wp, chunk)
		time.Sleep(50 * time.Millisecond)
	}
}

func fetchAndPushRPC(ctx context.Context, url string, wp *WorkerPool, addresses []string) {
	type RPCReq struct {
		JSONRPC string        `json:"jsonrpc"`
		ID      int           `json:"id"`
		Method  string        `json:"method"`
		Params  []interface{} `json:"params"`
	}

	payload := RPCReq{
		JSONRPC: "2.0",
		ID:      1,
		Method:  "getMultipleAccounts",
		Params: []interface{}{
			addresses,
			map[string]interface{}{"encoding": "base64"},
		},
	}

	body, _ := json.Marshal(payload)
	req, _ := http.NewRequestWithContext(ctx, "POST", url, bytes.NewBuffer(body))
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		log.Printf("[RPC] Request Error: %v", err)
		return
	}
	defer resp.Body.Close()

	respBody, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 200 {
		log.Printf("[RPC] HTTP Error %d: %s", resp.StatusCode, string(respBody))
		return
	}

	var result struct {
		Result struct {
			Context struct {
				Slot uint64 `json:"slot"`
			} `json:"context"`
			Value []struct {
				Data  []string `json:"data"`
				Owner string   `json:"owner"`
			} `json:"value"`
		} `json:"result"`
		Error interface{} `json:"error"`
	}

	if err := json.Unmarshal(respBody, &result); err != nil {
		log.Printf("[RPC] Decode Error: %v | Body: %s", err, string(respBody))
		return
	}

	if result.Error != nil {
		log.Printf("[RPC] Error from Helius: %v", result.Error)
		// Fallback to individual calls if batch fails
		for _, addr := range addresses {
			fetchSingleAccountRPC(ctx, url, wp, addr)
		}
		return
	}

	for idx, val := range result.Result.Value {
		if len(val.Data) == 0 {
			continue
		}
		data, _ := base64.StdEncoding.DecodeString(val.Data[0])
		wp.PushUpdate(&RawUpdate{
			ProgramID: val.Owner,
			Data:      data,
			Slot:      result.Result.Context.Slot,
			PoolAddr:  addresses[idx],
		})
	}
}

func fetchSingleAccountRPC(ctx context.Context, url string, wp *WorkerPool, addr string) {
	log.Printf("[RPC] Fetching single account: %s", addr)
	type RPCReq struct {
		JSONRPC string        `json:"jsonrpc"`
		ID      int           `json:"id"`
		Method  string        `json:"method"`
		Params  []interface{} `json:"params"`
	}

	payload := RPCReq{
		JSONRPC: "2.0",
		ID:      1,
		Method:  "getAccountInfo",
		Params: []interface{}{
			addr,
			map[string]interface{}{"encoding": "base64"},
		},
	}

	body, _ := json.Marshal(payload)
	req, _ := http.NewRequestWithContext(ctx, "POST", url, bytes.NewBuffer(body))
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return
	}
	defer resp.Body.Close()

	var result struct {
		Result struct {
			Context struct {
				Slot uint64 `json:"slot"`
			} `json:"context"`
			Value struct {
				Data  []string `json:"data"`
				Owner string   `json:"owner"`
			} `json:"value"`
		} `json:"result"`
	}

	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		log.Printf("[RPC] JSON Decode Error for %s: %v", addr, err)
		return
	}

	if result.Result.Value.Owner == "" {
		log.Printf("[RPC] WARNING: No owner found for %s. Account might not exist.", addr)
		return
	}

	if len(result.Result.Value.Data) > 0 {
		log.Printf("[RPC] Successfully fetched single account: %s (Owner: %s)", addr, result.Result.Value.Owner)
		data, _ := base64.StdEncoding.DecodeString(result.Result.Value.Data[0])
		wp.PushUpdate(&RawUpdate{
			ProgramID: result.Result.Value.Owner,
			Data:      data,
			Slot:      result.Result.Context.Slot,
			PoolAddr:  addr,
		})
	} else {
		log.Printf("[RPC] Successfully fetched account %s but it had NO DATA", addr)
	}
}

func syncInitialPools(ctx context.Context, wp *WorkerPool) {
	log.Println("[Sync] Starting initial vault discovery and sync...")
	apiKey := os.Getenv("YELLOWSTONE_X_TOKEN")
	if apiKey == "" {
		log.Println("[Sync] ABORTED: No YELLOWSTONE_X_TOKEN provided for RPC sync.")
		return
	}
	url := "https://mainnet.helius-rpc.com/?api-key=" + apiKey
	
	// 1. Force discovery by fetching pool state
	fetchReservesRPC(ctx, url, wp)

	// 2. Wait for workers to register vaults
	log.Println("[Sync] Waiting for vault registration (15s)...")
	time.Sleep(15 * time.Second)

	// 3. Fetch vault balances
	wp.VaultMutex.RLock()
	var vaultAddrs []string
	for v := range wp.VaultToPool {
		vaultAddrs = append(vaultAddrs, v)
	}
	wp.VaultMutex.RUnlock()

	if len(vaultAddrs) == 0 {
		log.Println("[Sync] WARNING: No vaults registered yet. Retrying in 10s...")
		time.Sleep(10 * time.Second)
		wp.VaultMutex.RLock()
		for v := range wp.VaultToPool {
			vaultAddrs = append(vaultAddrs, v)
		}
		wp.VaultMutex.RUnlock()
	}

	log.Printf("[Sync] Found %d vaults. Syncing balances...", len(vaultAddrs))
	if len(vaultAddrs) > 0 {
		// Batch vault fetches to avoid rate limits
		for _, addr := range vaultAddrs {
			fetchSingleAccountRPC(ctx, url, wp, addr)
			time.Sleep(20 * time.Millisecond) // Small throttle
		}
	}
	log.Println("[Sync] Initial vault sync sequence complete.")
}

func healUnknownSymbolsLoop(ctx context.Context, registry *PoolRegistry) {
	if registry == nil {
		return
	}

	ticker := time.NewTicker(60 * time.Second) // Every 1 minute
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			healUnknownSymbols(ctx, registry)
		}
	}
}

func healUnknownSymbols(ctx context.Context, registry *PoolRegistry) {
	pools, err := registry.GetPoolsWithUnknownSymbols(ctx)
	if err != nil {
		log.Printf("[Healer] Failed to fetch unknown pools: %v", err)
		return
	}

	if len(pools) == 0 {
		return
	}

	log.Printf("[Healer] Attempting to heal %d pools with missing symbols...", len(pools))
	healedCount := 0

	for _, p := range pools {
		symA := GetSymbolFromCache(p.MintA)
		symB := GetSymbolFromCache(p.MintB)

		// Only update if we actually found something better than UNKNOWN
		if (symA != "" && symA != "UNKNOWN") || (symB != "" && symB != "UNKNOWN") {
			err := registry.UpdatePoolMetadata(ctx, p.Address, p.MintA, p.MintB, symA, symB)
			if err == nil {
				healedCount++
			}
		}
	}

	if healedCount > 0 {
		log.Printf("[Healer] Successfully healed %d pools.", healedCount)
	}
}
