package main

import (
	"encoding/binary"
	"fmt"

	"github.com/mr-tron/base58/base58"
)

// RaydiumAmmInfo layout based on Raydium V4 AMM
type RaydiumAmmInfo struct {
	Status                 uint64
	Nonce                  uint64
	MaxOrder               uint64
	Depth                  uint64
	BaseDecimal            uint64
	QuoteDecimal           uint64
	State                  uint64
	ResetFlag              uint64
	MinSize                uint64
	VolMaxCutRatio         uint64
	AmountWaveRatio        uint64
	BaseLotSize            uint64
	QuoteLotSize           uint64
	MinPriceMultiplier     uint64
	MaxPriceMultiplier     uint64
	SystemDecimalValue     uint64
	MinSeparateNumerator   uint64
	MinSeparateDenominator uint64
	TradeFeeNumerator      uint64
	TradeFeeDenominator    uint64
	PnlNumerator           uint64
	PnlDenominator         uint64
	SwapFeeNumerator       uint64
	SwapFeeDenominator     uint64
	BaseNeedTakePnl        uint64
	QuoteNeedTakePnl       uint64
	QuoteTotalPnl          uint64
	BaseTotalPnl           uint64
	PoolOpenTime           uint64
	PunishPcAmount         uint64
	PunishCoinAmount       uint64
	OrderbookToInitTime    uint64
	SwapBaseInAmount       [16]byte // uint128
	SwapQuoteOutAmount     [16]byte // uint128
	SwapBase2QuoteOutAmount [16]byte // uint128
	SwapQuoteInAmount      [16]byte // uint128
	SwapBaseOutAmount      [16]byte // uint128
	SwapQuote2BaseOutAmount [16]byte // uint128
	BaseVault              [32]byte
	QuoteVault             [32]byte
	BaseMint               [32]byte
	QuoteMint              [32]byte
	LpMint                 [32]byte
	OpenOrders             [32]byte
	MarketId               [32]byte
	MarketProgramId        [32]byte
	TargetOrders           [32]byte
	WithdrawQueue          [32]byte
	LpVault                [32]byte
	AmmOwner               [32]byte
	LpReserve              uint64
	Padding                [24]uint64
}

// PoolData represents the normalized pool data we push to Redis
type PoolData struct {
	Address         string
	TokenA          string
	TokenB          string
	VaultA          string
	VaultB          string
	ReservesA       uint64
	ReservesB       uint64
	LpSupply        uint64
	LastUpdatedSlot uint64
	DexType         string
}

const (
	RaydiumProgramID        = "675kPX9MHTjS2zt1qxy1nvNDY7XJ76uA6fG7s1Uf"
	RaydiumProgramID2       = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
	TokenProgramID          = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
	TokenProgramID_Standard = "TokenkegQfeZyiNwAJbVDRk64udw67nV8pxE89s2YZ8"
	OrcaProgramID           = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
	RaydiumCPMMProgramID    = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"
)

// Decoder handles stateful decoding for the Solana program accounts
type Decoder struct{}

// Decode identifies the DEX and decodes the raw byte data
func (d *Decoder) Decode(poolAddr, programID string, data []byte) (*PoolData, error) {
	// Slot is no longer passed to the decoder directly; it's handled in the worker logic
	return DecodePoolUpdate(programID, data, 0, poolAddr)
}

// DecodePoolUpdate identifies the DEX and decodes the raw byte data
// func DecodePoolUpdate(programID string, data []byte, slot uint64, poolAddr string) (*PoolData, error) {
// 	switch programID {
// 	case RaydiumProgramID, RaydiumProgramID2:
// 		// Raydium V4 AMM accounts are 752 bytes of fixed layout.
// 		if len(data) < 752 {
// 			return nil, fmt.Errorf("raydium data too short: %d < 752", len(data))
// 		}


// 		// Raydium V4 Offsets:
// 		// baseMint: 400, quoteMint: 432
// 		// baseVault: 448, quoteVault: 480
// 		// lpSupply: 552
		
// 		lpSupply := binary.LittleEndian.Uint64(data[552:560])

// 		tokenA := base58.Encode(data[400:432])
// 		tokenB := base58.Encode(data[432:464])
// 		vaultA := base58.Encode(data[448:480])
// 		vaultB := base58.Encode(data[480:512])

// 		// Reserves will be populated by vault balance monitoring (handleVaultUpdate)
// 		reservesA := uint64(0)
// 		reservesB := uint64(0)

// 		return &PoolData{
// 			Address:         poolAddr,
// 			TokenA:          tokenA,
// 			TokenB:          tokenB,
// 			VaultA:          vaultA,
// 			VaultB:          vaultB,
// 			ReservesA:       reservesA,
// 			ReservesB:       reservesB,
// 			LpSupply:        lpSupply,
// 			LastUpdatedSlot: slot,
// 			DexType:         "raydium",
// 		}, nil

// 	case RaydiumCPMMProgramID:
// 		// Raydium CPMM PoolState layout:
// 		// mint_a: 72, mint_b: 104
// 		// vault_a: 168, vault_b: 200
// 		if len(data) < 232 {
// 			return nil, fmt.Errorf("raydium cpmm data too short: %d < 232", len(data))
// 		}

// 		tokenA := base58.Encode(data[168:200])
// 		tokenB := base58.Encode(data[200:232])
// 		vaultA := base58.Encode(data[72:104])
// 		vaultB := base58.Encode(data[104:136])

// 		return &PoolData{
// 			Address:         poolAddr,
// 			TokenA:          tokenA,
// 			TokenB:          tokenB,
// 			VaultA:          vaultA,
// 			VaultB:          vaultB,
// 			ReservesA:       0,
// 			ReservesB:       0,
// 			LpSupply:        0, 
// 			LastUpdatedSlot: slot,
// 			DexType:         "raydium_cpmm",
// 		}, nil

// 	case OrcaProgramID:
// 		// Whirlpool account layout:
// 		// sqrtPrice (u128) at offset 32
// 		// liquidity (u128) at offset 16
// 		// tokenVaultA (32 bytes) at offset 101
// 		// tokenVaultB (32 bytes) at offset 133
// 		if len(data) < 165 {
// 			return nil, fmt.Errorf("orca whirlpool data too short: %d < 165", len(data))
// 		}
		
// 		// Reading lower 64 bits for now, though technically u128
// 		liquidity := binary.LittleEndian.Uint64(data[16:24]) 
// 		// For CLMM, sqrtPrice is crucial. We read the first 8 bytes.
// 		// Note: Proper CLMM support would require the full u128.
// 		sqrtPrice := binary.LittleEndian.Uint64(data[32:40])
		
// 		vaultA := base58.Encode(data[101:133])
// 		vaultB := base58.Encode(data[133:165])

// 		return &PoolData{
// 			Address:         poolAddr,
// 			VaultA:          vaultA,
// 			VaultB:          vaultB,
// 			ReservesA:       liquidity,
// 			ReservesB:       sqrtPrice,
// 			LpSupply:        0, 
// 			LastUpdatedSlot: slot,
// 			DexType:         "orca",
// 		}, nil

// 	default:
// 		return nil, fmt.Errorf("unsupported program ID: %s", programID)
// 	}
// }

func DecodePoolUpdate(programID string, data []byte, slot uint64, poolAddr string) (*PoolData, error) {
    switch programID {
    case RaydiumProgramID, RaydiumProgramID2:
        if len(data) < 752 { return nil, fmt.Errorf("data too short") }

        return &PoolData{
            Address:   poolAddr,
            TokenA:    base58.Encode(data[400:432]), // baseMint
            TokenB:    base58.Encode(data[432:464]), // quoteMint
            VaultA:    base58.Encode(data[336:368]), // baseVault
            VaultB:    base58.Encode(data[368:400]), // quoteVault
            LpSupply:  binary.LittleEndian.Uint64(data[552:560]),
            DexType:   "raydium",
        }, nil

    case RaydiumCPMMProgramID:
        if len(data) < 232 {
            return nil, fmt.Errorf("cpmm data too short")
        }

        // CORRECT CPMM OFFSETS:
        // Mints are at 72 and 104
        // Vaults are at 168 and 200
        tokenA := base58.Encode(data[72:104])   // This MUST be the Mint
        tokenB := base58.Encode(data[104:136])  // This MUST be the Mint
        vaultA := base58.Encode(data[168:200])  // This is the Vault
        vaultB := base58.Encode(data[200:232])  // This is the Vault

        return &PoolData{
            Address:   poolAddr,
            TokenA:    tokenA,
            TokenB:    tokenB,
            VaultA:    vaultA,
            VaultB:    vaultB,
            DexType:   "raydium_cpmm",
        }, nil

    case OrcaProgramID:
        // Need at least 245 bytes to reach Token Vault B
        if len(data) < 245 {
            return nil, fmt.Errorf("orca data too short")
        }
        
        // CORRECT ORCA WHIRLPOOL OFFSETS:
        tokenA := base58.Encode(data[101:133]) // baseMint
        vaultA := base58.Encode(data[133:165]) // baseVault
        tokenB := base58.Encode(data[181:213]) // quoteMint
        vaultB := base58.Encode(data[213:245]) // quoteVault
        
        return &PoolData{
            Address: poolAddr,
            TokenA:  tokenA,
            TokenB:  tokenB,
            VaultA:  vaultA,
            VaultB:  vaultB,
            DexType: "orca",
        }, nil
    }
    return nil, fmt.Errorf("unsupported program")
}
