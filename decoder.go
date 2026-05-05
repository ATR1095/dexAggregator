package main

import (
	"encoding/binary"
	"encoding/hex"
	"fmt"

	"github.com/mr-tron/base58/base58"
)

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
	// CLMM fields (stored as hex strings for precision)
	SqrtPriceX64    string
	Liquidity       string
	CurrentTick     int32
	TickSpacing     uint32
	DecimalsA       uint8
	DecimalsB       uint8
	BaseFactor      uint16
	AmmType         uint32
	ExtraData       []byte
	// Auxiliary accounts for transaction building
	Accounts        map[string]string
}

const (
	RaydiumProgramID        = "675kPX9MHTjS2zt1qxy1nvNDY7XJ76uA6fG7s1Uf"
	OrcaProgramID           = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
	MeteoraProgramID        = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"
	TokenProgramID          = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
	Token2022ProgramID      = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
)

type Decoder struct{}

func (d *Decoder) Decode(poolAddr, programID string, data []byte) (*PoolData, error) {
	switch programID {
	case OrcaProgramID:
		if len(data) < 245 {
			return nil, fmt.Errorf("orca data too short")
		}
		tickSpacing := binary.LittleEndian.Uint16(data[41:43])
		liquidity := data[49:65]
		sqrtPrice := data[65:81]
		tickIndex := int32(binary.LittleEndian.Uint32(data[81:85]))
		tokenA := base58.Encode(data[101:133])
		vaultA := base58.Encode(data[133:165])
		tokenB := base58.Encode(data[181:213])
		vaultB := base58.Encode(data[213:245])

		var extraData []byte
		extraData = append(extraData, sqrtPrice...)
		extraData = append(extraData, liquidity...)
		tickBytes := make([]byte, 4)
		binary.LittleEndian.PutUint32(tickBytes, uint32(tickIndex))
		spacingBytes := make([]byte, 2)
		binary.LittleEndian.PutUint16(spacingBytes, uint16(tickSpacing))
		extraData = append(extraData, tickBytes...)
		extraData = append(extraData, spacingBytes...)

		return &PoolData{
			Address:      poolAddr,
			TokenA:       tokenA,
			TokenB:       tokenB,
			VaultA:       vaultA,
			VaultB:       vaultB,
			SqrtPriceX64: hex.EncodeToString(sqrtPrice),
			Liquidity:    hex.EncodeToString(liquidity),
			CurrentTick:  tickIndex,
			TickSpacing:  uint32(tickSpacing),
			DexType:      "orca",
			AmmType:      1,
			ExtraData:    extraData,
			Accounts: map[string]string{
				"program_id":   programID,
				"pool_vault_a": vaultA,
				"pool_vault_b": vaultB,
			},
		}, nil

	case MeteoraProgramID:
		return DecodeMeteora(data, poolAddr)
	}
	return nil, fmt.Errorf("unsupported program: %s", programID)
}

func DecodeMeteora(data []byte, poolAddr string) (*PoolData, error) {
	if len(data) < 216 {
		return nil, fmt.Errorf("meteora data too short")
	}
	activeID := int32(binary.LittleEndian.Uint32(data[8:12]))
	binStep := binary.LittleEndian.Uint16(data[12:14])
	baseFactor := binary.LittleEndian.Uint16(data[14:16])
	decA := data[16]
	decB := data[17]
	tokenX := base58.Encode(data[88:120])
	tokenY := base58.Encode(data[120:152])
	reserveX := base58.Encode(data[152:184])
	reserveY := base58.Encode(data[184:216])

	var extraData []byte
	tickBytes := make([]byte, 4)
	binary.LittleEndian.PutUint32(tickBytes, uint32(activeID))
	stepBytes := make([]byte, 2)
	binary.LittleEndian.PutUint16(stepBytes, uint16(binStep))
	baseBytes := make([]byte, 2)
	binary.LittleEndian.PutUint16(baseBytes, uint16(baseFactor))
	extraData = append(extraData, tickBytes...)
	extraData = append(extraData, stepBytes...)
	extraData = append(extraData, baseBytes...)

	return &PoolData{
		Address:      poolAddr,
		TokenA:       tokenX,
		TokenB:       tokenY,
		VaultA:       reserveX,
		VaultB:       reserveY,
		CurrentTick:  activeID,
		TickSpacing:  uint32(binStep),
		DexType:      "meteora",
		DecimalsA:    decA,
		DecimalsB:    decB,
		BaseFactor:   baseFactor,
		AmmType:      2,
		ExtraData:    extraData,
		Accounts: map[string]string{
			"program_id": MeteoraProgramID,
			"reserve_x":  reserveX,
			"reserve_y":  reserveY,
		},
	}, nil
}
