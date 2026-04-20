package handlers

import (
	"net/http"

	"github.com/dexAggregator/APIGateway/internal/grpc"
	"github.com/dexAggregator/APIGateway/proto/sor"
	"github.com/gin-gonic/gin"
	"github.com/go-playground/validator/v10"
)

type SwapRequest struct {
	InputToken   string  `json:"inputToken" validate:"required"`
	OutputToken  string  `json:"outputToken" validate:"required"`
	Amount       string  `json:"amount" validate:"required"`
	UserAddress  string  `json:"userAddress" validate:"required"`
	SlippageBps  float64 `json:"slippageBps" validate:"min=0,max=1000"` // 0 to 10%
}

// SwapHandler godoc
// @Summary Execute a token swap
// @Description Initiate a swap transaction via the SOR
// @Tags swap
// @Accept json
// @Produce json
// @Param swap body SwapRequest true "Swap Request"
// @Success 200 {object} sor.SwapResponse
// @Failure 400 {object} map[string]string
// @Failure 500 {object} map[string]string
// @Router /v1/swap [post]
func SwapHandler(client *grpc.Client) gin.HandlerFunc {
	validate := validator.New()
	return func(c *gin.Context) {
		var req SwapRequest
		if err := c.ShouldBindJSON(&req); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}

		if err := validate.Struct(req); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}

		grpcReq := &sor.SwapRequest{
			InputToken:  req.InputToken,
			OutputToken: req.OutputToken,
			Amount:      req.Amount,
			UserAddress: req.UserAddress,
			SlippageBps: req.SlippageBps,
		}

		resp, err := client.Swap(c.Request.Context(), grpcReq)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{
				"error":   "failed to perform swap via SOR",
				"details": err.Error(),
			})
			return
		}

		routes := make([]gin.H, len(resp.Routes))
		for i, r := range resp.Routes {
			routes[i] = gin.H{
				"token_path":       r.TokenPath,
				"pool_ids":         r.PoolIds,
				"amount_in":        r.AmountIn,
				"amount_out":       r.AmountOut,
				"human_amount_in":  r.HumanAmountIn,
				"human_amount_out": r.HumanAmountOut,
				"price_impact":    r.PriceImpact,
				"dex_labels":      r.DexLabels,
			}
		}

		c.JSON(http.StatusOK, gin.H{
			"tx_hash":             resp.TxHash,
			"status":              resp.Status,
			"message":             resp.Message,
			"route":               resp.Route,
			"output_amount":       resp.OutputAmount,
			"human_output_amount": resp.HumanOutputAmount,
			"routes":              routes,
		})
	}
}
