package main

import (
	"context"
	"encoding/binary"
	"fmt"
	"log"
	"strconv"
	"sync"
	"time"

	"github.com/redis/go-redis/v9"
)

type WorkerPool struct {
	Redis           *redis.Client
	NumWorkers      int
	UpdateChan      chan *RawUpdate
	BufferSize      int
	MinLiquidityUSD float64
	BufferPool      *sync.Pool
	MonitoredPools  map[string]bool
	PoolList        []string
	PoolMutex       sync.RWMutex
	Subscribers     []chan *WorkerUpdate
	SubMutex        sync.RWMutex
	Registry        *PoolRegistry
	VaultToPool     map[string]string // vaultAddress -> poolAddress:A or poolAddress:B
	VaultMutex      sync.RWMutex
}

type WorkerUpdate struct {
	PoolId   string
	TokenA   string
	TokenB   string
	ReserveA uint64
	ReserveB uint64
	AmmType  uint32
	DexType  string
}

type RawUpdate struct {
	ProgramID string
	Data      []byte
	Slot      uint64
	PoolAddr  string
}

func NewWorkerPool(redisClient *redis.Client, numWorkers, bufferSize int) *WorkerPool {
	return &WorkerPool{
		Redis:           redisClient,
		NumWorkers:      numWorkers,
		BufferSize:      bufferSize,
		MinLiquidityUSD: 0.0,
		UpdateChan:      make(chan *RawUpdate, bufferSize),
		BufferPool: &sync.Pool{
			New: func() interface{} {
				return make([]byte, 1024)
			},
		},
		MonitoredPools: make(map[string]bool),
		VaultToPool:    make(map[string]string),
		Subscribers:    make([]chan *WorkerUpdate, 0),
		Registry:       nil,
	}
}

func (wp *WorkerPool) Subscribe() chan *WorkerUpdate {
	ch := make(chan *WorkerUpdate, 100)
	wp.SubMutex.Lock()
	wp.Subscribers = append(wp.Subscribers, ch)
	wp.SubMutex.Unlock()
	return ch
}

func (wp *WorkerPool) Unsubscribe(ch chan *WorkerUpdate) {
	wp.SubMutex.Lock()
	defer wp.SubMutex.Unlock()
	for i, sub := range wp.Subscribers {
		if sub == ch {
			wp.Subscribers = append(wp.Subscribers[:i], wp.Subscribers[i+1:]...)
			close(ch)
			break
		}
	}
}

func (wp *WorkerPool) Broadcast(update *WorkerUpdate) {
	wp.SubMutex.RLock()
	defer wp.SubMutex.RUnlock()
	for _, sub := range wp.Subscribers {
		select {
		case sub <- update:
		default:
		}
	}
}

func (wp *WorkerPool) SetMonitoredPools(pools []string) {
	newMap := make(map[string]bool, len(pools))
	for _, p := range pools {
		newMap[p] = true
	}
	wp.PoolMutex.Lock()
	wp.MonitoredPools = newMap
	wp.PoolList = pools
	wp.PoolMutex.Unlock()
	log.Printf("WorkerPool: Updated monitored pools list (%d pools)", len(pools))
}

func (wp *WorkerPool) GetPoolList() []string {
	wp.PoolMutex.RLock()
	defer wp.PoolMutex.RUnlock()
	cp := make([]string, len(wp.PoolList))
	copy(cp, wp.PoolList)
	return cp
}

func (wp *WorkerPool) IsMonitored(addr string) bool {
	wp.PoolMutex.RLock()
	defer wp.PoolMutex.RUnlock()
	return wp.MonitoredPools[addr]
}

func (wp *WorkerPool) Start(ctx context.Context) {
	for i := 0; i < wp.NumWorkers; i++ {
		go wp.WorkerRoutine(ctx, i)
	}
}

func (wp *WorkerPool) PushUpdate(update *RawUpdate) {
	wp.UpdateChan <- update
}

func (wp *WorkerPool) WorkerRoutine(ctx context.Context, id int) {
	log.Printf("Worker %d started", id)
	d := &Decoder{}

	for {
		select {
		case <-ctx.Done():
			return
		case update, ok := <-wp.UpdateChan:
			if !ok {
				return
			}
			log.Printf("Worker %d: Received update for %s (Program: %s)", id, update.PoolAddr, update.ProgramID)

			// Handle SPL Token account updates (Vaults)
			// Log every received update type for debugging
			// log.Printf("[Worker] Received update for %s (Program: %s, Data: %d bytes)", update.PoolAddr, update.ProgramID, len(update.Data))

			if update.ProgramID == TokenProgramID || update.ProgramID == TokenProgramID_Standard {
				if len(update.Data) >= 72 {
					amount := binary.LittleEndian.Uint64(update.Data[64:72])
					wp.handleVaultUpdate(ctx, update.PoolAddr, amount)
				} else {
					log.Printf("[Worker] WARNING: Token account %s data too short: %d", update.PoolAddr, len(update.Data))
				}
				continue
			}

			// Add Token2022 support just in case
			const Token2022ProgramID = "TokenzQ9kh2QKGjY99v9AAtZAuC2Yyyo6G7oYj9E9sU"
			if update.ProgramID == Token2022ProgramID {
				if len(update.Data) >= 72 {
					amount := binary.LittleEndian.Uint64(update.Data[64:72])
					wp.handleVaultUpdate(ctx, update.PoolAddr, amount)
				}
				continue
			}

			// If it's not a known token program and not a known pool program, log it
			log.Printf("[Worker] UNKNOWN Program %s for account %s (Data length: %d)", update.ProgramID, update.PoolAddr, len(update.Data))

			if !wp.IsMonitored(update.PoolAddr) {
				continue
			}

			poolData, err := d.Decode(update.PoolAddr, update.ProgramID, update.Data)
			if err != nil {
				continue
			}

			// Register Vaults for this pool
			if poolData.VaultA != "" {
				wp.VaultMutex.Lock()
				wp.VaultToPool[poolData.VaultA] = poolData.Address + ":A"
				wp.VaultToPool[poolData.VaultB] = poolData.Address + ":B"
				wp.VaultMutex.Unlock()
				log.Printf("[Worker] SUCCESSFULLY REGISTERED VAULTS for pool %s: A=%s, B=%s", poolData.Address, poolData.VaultA, poolData.VaultB)
			}

			// Always update metadata and available reserves
			wp.updatePoolReserves(ctx, poolData)
		}
	}
}

func (wp *WorkerPool) RegisterPoolVaults(poolAddr, vaultA, vaultB string) {
	wp.VaultMutex.Lock()
	defer wp.VaultMutex.Unlock()
	wp.VaultToPool[vaultA] = poolAddr + ":A"
	wp.VaultToPool[vaultB] = poolAddr + ":B"
	log.Printf("[Worker] MANUALLY REGISTERED VAULTS for pool %s: A=%s, B=%s", poolAddr, vaultA, vaultB)
}

func (wp *WorkerPool) handleVaultUpdate(ctx context.Context, vaultAddr string, amount uint64) {
	wp.VaultMutex.RLock()
	mapping, ok := wp.VaultToPool[vaultAddr]
	wp.VaultMutex.RUnlock()

	if !ok {
		return
	}

	var poolAddr, side string
	for i, c := range mapping {
		if c == ':' {
			poolAddr = mapping[:i]
			side = mapping[i+1:]
			break
		}
	}

	field := "reserves_a"
	if side == "B" {
		field = "reserves_b"
	}

	key := fmt.Sprintf("pool:%s", poolAddr)
	err := wp.Redis.HSet(ctx, key, field, fmt.Sprintf("%d", amount)).Err()
	if err != nil {
		log.Printf("[Worker] Failed to update %s for %s: %v", field, poolAddr, err)
		return
	} else if amount > 0 {
		log.Printf("[Worker] Updated %s for pool %s: %v", field, poolAddr, amount)
	} else {
		// Log 0 updates only in debug/verbose contexts to avoid spam, but keep for now
		log.Printf("[Worker] SYNC: vault %s reported 0 balance for pool %s", vaultAddr, poolAddr)
	}
	data, err := wp.Redis.HGetAll(ctx, key).Result()
	if err != nil || len(data) == 0 {
		return
	}

	resA, _ := strconv.ParseUint(data["reserves_a"], 10, 64)
	resB, _ := strconv.ParseUint(data["reserves_b"], 10, 64)

	wp.Broadcast(&WorkerUpdate{
		PoolId:   poolAddr,
		TokenA:   data["token_a"],
		TokenB:   data["token_b"],
		ReserveA: resA,
		ReserveB: resB,
		DexType:  data["dex_type"],
	})
	
	// Terminal logging for visibility
	now := float64(time.Now().UnixNano()) / 1e9
	fmt.Printf("%.6f [VaultUpdate] %s side %s updated to %d\n", now, poolAddr, side, amount)
}

func (wp *WorkerPool) updatePoolReserves(ctx context.Context, poolData *PoolData) {
	key := fmt.Sprintf("pool:%s", poolData.Address)
	symA := GetSymbolFromCache(poolData.TokenA)
	symB := GetSymbolFromCache(poolData.TokenB)

	// Simple update
	wp.Redis.HSet(ctx, key, map[string]interface{}{
		"token_a":    poolData.TokenA,
		"token_b":    poolData.TokenB,
		"symbol_a":   symA,
		"symbol_b":   symB,
		"decimals_a": GetDecimalsFromCache(poolData.TokenA),
		"decimals_b": GetDecimalsFromCache(poolData.TokenB),
		"dex_type":   poolData.DexType,
	})

	if poolData.ReservesA > 0 || poolData.ReservesB > 0 {
		wp.Redis.HSet(ctx, key, map[string]interface{}{
			"reserves_a": poolData.ReservesA,
			"reserves_b": poolData.ReservesB,
		})
	}

	if wp.Registry != nil {
		wp.Registry.UpdatePoolMetadata(ctx, poolData.Address, poolData.TokenA, poolData.TokenB, symA, symB)
	}
}
