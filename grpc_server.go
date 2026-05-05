package main

import (
	"context"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"log"
	"net"
	"strconv"

	pb "priceOracleService/proto"

	"github.com/redis/go-redis/v9"
	"google.golang.org/grpc"
)

type OracleServer struct {
	pb.UnimplementedPriceOracleServer
	WorkerPool *WorkerPool
	Redis      *redis.Client
}

func (s *OracleServer) GetPoolReserves(ctx context.Context, req *pb.PoolRequest) (*pb.PoolUpdate, error) {
	return s.getPoolUpdate(ctx, req.PoolId)
}

func (s *OracleServer) getPoolUpdate(ctx context.Context, poolID string) (*pb.PoolUpdate, error) {
	key := fmt.Sprintf("pool:%s", poolID)
	data, err := s.Redis.HGetAll(ctx, key).Result()
	if err != nil {
		return nil, fmt.Errorf("failed to get pool data from redis: %v", err)
	}

	if len(data) == 0 {
		return nil, fmt.Errorf("pool not found: %s", poolID)
	}

	resA, _ := strconv.ParseUint(data["reserves_a"], 10, 64)
	resB, _ := strconv.ParseUint(data["reserves_b"], 10, 64)
	decA, _ := strconv.ParseUint(data["decimals_a"], 10, 32)
	decB, _ := strconv.ParseUint(data["decimals_b"], 10, 32)
	tokenA := data["token_a"]
	tokenB := data["token_b"]
	symA := data["symbol_a"]
	symB := data["symbol_b"]
	dexLabel := data["dex_type"]

	var extraData []byte
	ammType := uint32(0)
	if dexLabel == "orca" {
		ammType = 1
		sqrtStr := data["sqrt_price"]
		liqStr := data["liquidity"]
		tickStr := data["current_tick"]

		if sqrtStr != "" && liqStr != "" {
			sqrtBytes, _ := hex.DecodeString(sqrtStr)
			liqBytes, _ := hex.DecodeString(liqStr)
			tickVal, _ := strconv.ParseInt(tickStr, 10, 32)
			tickBytes := make([]byte, 4)
			binary.LittleEndian.PutUint32(tickBytes, uint32(tickVal))

			spacingVal, _ := strconv.ParseUint(data["tick_spacing"], 10, 16)
			spacingBytes := make([]byte, 2)
			binary.LittleEndian.PutUint16(spacingBytes, uint16(spacingVal))

			extraData = append(extraData, sqrtBytes...)
			extraData = append(extraData, liqBytes...)
			extraData = append(extraData, tickBytes...)
			extraData = append(extraData, spacingBytes...)
		}
	} else if dexLabel == "meteora" {
		ammType = 2
		tickStr := data["current_tick"]
		stepStr := data["tick_spacing"]

		tickVal, _ := strconv.ParseInt(tickStr, 10, 32)
		stepVal, _ := strconv.ParseUint(stepStr, 10, 16)
		baseVal, _ := strconv.ParseUint(data["base_factor"], 10, 16)

		tickBytes := make([]byte, 4)
		binary.LittleEndian.PutUint32(tickBytes, uint32(tickVal))

		stepBytes := make([]byte, 2)
		binary.LittleEndian.PutUint16(stepBytes, uint16(stepVal))

		baseBytes := make([]byte, 2)
		binary.LittleEndian.PutUint16(baseBytes, uint16(baseVal))

		extraData = append(extraData, tickBytes...)
		extraData = append(extraData, stepBytes...)
		extraData = append(extraData, baseBytes...)
	}

	accounts := make(map[string]string)
	for k, v := range data {
		if len(k) > 4 && k[:4] == "acc:" {
			accounts[k[4:]] = v
		}
	}
	if progA, ok := data["token_program_a"]; ok {
		accounts["token_program_a"] = progA
	}
	if progB, ok := data["token_program_b"]; ok {
		accounts["token_program_b"] = progB
	}

	return &pb.PoolUpdate{
		PoolId:    poolID,
		TokenA:    tokenA,
		TokenB:    tokenB,
		SymbolA:   symA,
		SymbolB:   symB,
		ReserveA:  resA,
		ReserveB:  resB,
		DecimalsA: uint32(decA),
		DecimalsB: uint32(decB),
		DexLabel:  dexLabel,
		AmmType:   ammType,
		ExtraData: extraData,
		Accounts:  accounts,
	}, nil
}

func (s *OracleServer) GetMonitoredPools(ctx context.Context, req *pb.Empty) (*pb.PoolList, error) {
	pools := s.WorkerPool.GetPoolList()
	
	essentials := []string{
		"58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2", // SOL/USDC
		"GmaDNMWsTYWjaXVBjJTHNmCWAKU6cn5hhtWWYEZt4odo", // LIKE/USDC
		"9pxP44otrjS7o3mxwsF9z4PBzC7o7dePbvDwnLidS8No", // mSOL/SOL
	}

	poolMap := make(map[string]bool)
	for _, p := range pools {
		poolMap[p] = true
	}
	
	finalPools := pools
	for _, e := range essentials {
		if !poolMap[e] {
			finalPools = append(finalPools, e)
			poolMap[e] = true
		}
	}

	return &pb.PoolList{PoolIds: finalPools}, nil
}

func (s *OracleServer) GetAllPoolUpdates(ctx context.Context, req *pb.Empty) (*pb.PoolUpdateList, error) {
	poolsResp, err := s.GetMonitoredPools(ctx, req)
	if err != nil {
		return nil, err
	}

	updates := make([]*pb.PoolUpdate, 0, len(poolsResp.PoolIds))
	for _, poolID := range poolsResp.PoolIds {
		update, err := s.getPoolUpdate(ctx, poolID)
		if err != nil {
			log.Printf("Warning: failed to get reserves for pool %s: %v", poolID, err)
			continue
		}
		updates = append(updates, update)
	}

	return &pb.PoolUpdateList{Updates: updates}, nil
}

func StartGRPCServer(port string, wp *WorkerPool, redisClient *redis.Client) {
	lis, err := net.Listen("tcp", ":"+port)
	if err != nil {
		log.Fatalf("failed to listen: %v", err)
	}
	s := grpc.NewServer()
	pb.RegisterPriceOracleServer(s, &OracleServer{
		WorkerPool: wp,
		Redis:      redisClient,
	})
	log.Printf("gRPC server listening at %v", lis.Addr())
	go func() {
		if err := s.Serve(lis); err != nil {
			log.Fatalf("failed to serve: %v", err)
		}
	}()
}
