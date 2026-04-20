package main

import (
	"context"
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
	key := fmt.Sprintf("pool:%s", req.PoolId)
	data, err := s.Redis.HGetAll(ctx, key).Result()
	if err != nil {
		return nil, fmt.Errorf("failed to get pool data from redis: %v", err)
	}

	if len(data) == 0 {
		return nil, fmt.Errorf("pool not found: %s", req.PoolId)
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

	return &pb.PoolUpdate{
		PoolId:    req.PoolId,
		TokenA:    tokenA,
		TokenB:    tokenB,
		SymbolA:   symA,
		SymbolB:   symB,
		ReserveA:  resA,
		ReserveB:  resB,
		DecimalsA: uint32(decA),
		DecimalsB: uint32(decB),
		DexLabel:  dexLabel,
	}, nil
}

func (s *OracleServer) GetMonitoredPools(ctx context.Context, req *pb.Empty) (*pb.PoolList, error) {
	pools := s.WorkerPool.GetPoolList()
	
	// Hardened Fallback: Always include essential pools to ensure pathfinding works
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
