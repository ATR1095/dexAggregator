package main

import (
	"context"
	"fmt"

	"github.com/jackc/pgx/v5/pgxpool"
)

// PoolRegistry handles persistent storage of pool addresses
type PoolRegistry struct {
	Pool *pgxpool.Pool
}

// NewPoolRegistry initializes the Postgres connection pool and table
func NewPoolRegistry(ctx context.Context, connStr string) (*PoolRegistry, error) {
	config, err := pgxpool.ParseConfig(connStr)
	if err != nil {
		return nil, fmt.Errorf("failed to parse PG config: %w", err)
	}

	pool, err := pgxpool.NewWithConfig(ctx, config)
	if err != nil {
		return nil, fmt.Errorf("failed to connect to Postgres: %w", err)
	}

	// Initialize tables if they don't exist
	initSQL := `
	CREATE TABLE IF NOT EXISTS monitored_pools (
		address TEXT PRIMARY KEY,
		dex_type TEXT NOT NULL,
		token_a_mint TEXT,
		token_b_mint TEXT,
		token_a_symbol TEXT,
		token_b_symbol TEXT,
		is_active BOOLEAN DEFAULT TRUE,
		last_discovered TIMESTAMP DEFAULT CURRENT_TIMESTAMP, 
		liquidity_usd DOUBLE PRECISION
	);
	CREATE INDEX IF NOT EXISTS idx_monitored_pools_active ON monitored_pools(is_active) WHERE is_active = TRUE;
	`
	fmt.Println("Postgres: Initializing schema...")
	if _, err := pool.Exec(ctx, initSQL); err != nil {
		fmt.Printf("Postgres: CRITICAL error during schema initialization: %v\n", err)
		return nil, fmt.Errorf("failed to initialize schema: %w", err)
	}
	fmt.Println("Postgres: Schema initialization complete.")

	return &PoolRegistry{Pool: pool}, nil
}

// UpsertPool adds or updates a pool in the registry with token metadata
func (pr *PoolRegistry) UpsertPool(ctx context.Context, addr, dexType, mintA, mintB, symA, symB string, usd float64) error {
	query := `
	INSERT INTO monitored_pools (address, dex_type, token_a_mint, token_b_mint, token_a_symbol, token_b_symbol, liquidity_usd)
	VALUES ($1, $2, $3, $4, $5, $6, $7)
	ON CONFLICT (address) DO UPDATE 
	SET 
		token_a_mint = EXCLUDED.token_a_mint,
		token_b_mint = EXCLUDED.token_b_mint,
		token_a_symbol = EXCLUDED.token_a_symbol,
		token_b_symbol = EXCLUDED.token_b_symbol,
		liquidity_usd = EXCLUDED.liquidity_usd, 
		last_discovered = CURRENT_TIMESTAMP;
	`
	_, err := pr.Pool.Exec(ctx, query, addr, dexType, mintA, mintB, symA, symB, usd)
	return err
}

// UpdatePoolMetadata specifically updates token mints and symbols for an existing pool record
func (pr *PoolRegistry) UpdatePoolMetadata(ctx context.Context, addr, mintA, mintB, symA, symB string) error {
	query := `
	UPDATE monitored_pools 
	SET 
		token_a_mint = $2,
		token_b_mint = $3,
		token_a_symbol = $4,
		token_b_symbol = $5,
		last_discovered = CURRENT_TIMESTAMP
	WHERE address = $1 AND (
		token_a_mint IS NULL OR token_a_mint = '' OR 
		token_a_symbol IS NULL OR token_a_symbol = '' OR token_a_symbol = 'UNKNOWN' OR
		token_b_symbol IS NULL OR token_b_symbol = '' OR token_b_symbol = 'UNKNOWN'
	);
	`
	_, err := pr.Pool.Exec(ctx, query, addr, mintA, mintB, symA, symB)
	return err
}

// GetActivePools returns a list of all active pool addresses, sorted by liquidity for prioritization
func (pr *PoolRegistry) GetActivePools(ctx context.Context) ([]string, error) {
	query := "SELECT address FROM monitored_pools WHERE is_active = TRUE ORDER BY liquidity_usd DESC"
	rows, err := pr.Pool.Query(ctx, query)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var addresses []string
	for rows.Next() {
		var addr string
		if err := rows.Scan(&addr); err != nil {
			return nil, err
		}
		addresses = append(addresses, addr)
	}
	return addresses, nil
}

type PoolMetadataRow struct {
	Address string
	MintA   string
	MintB   string
}

// GetPoolsWithUnknownSymbols returns a list of pools that have 'UNKNOWN' or missing symbols
func (pr *PoolRegistry) GetPoolsWithUnknownSymbols(ctx context.Context) ([]PoolMetadataRow, error) {
	query := `
	SELECT address, token_a_mint, token_b_mint 
	FROM monitored_pools 
	WHERE token_a_symbol = 'UNKNOWN' OR token_b_symbol = 'UNKNOWN' 
	   OR token_a_symbol IS NULL OR token_b_symbol IS NULL
	   OR token_a_symbol = '' OR token_b_symbol = '';
	`
	rows, err := pr.Pool.Query(ctx, query)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var pools []PoolMetadataRow
	for rows.Next() {
		var p PoolMetadataRow
		if err := rows.Scan(&p.Address, &p.MintA, &p.MintB); err != nil {
			return nil, err
		}
		pools = append(pools, p)
	}
	return pools, nil
}
