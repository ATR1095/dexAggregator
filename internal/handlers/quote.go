package handlers

import (
	"net/http"

	"github.com/dexAggregator/APIGateway/internal/grpc"
	"github.com/dexAggregator/APIGateway/proto/sor"
	"github.com/gin-gonic/gin"
	"github.com/go-playground/validator/v10"
)

type QuoteRequest struct {
	InputToken  string `json:"inputToken" validate:"required"`
	OutputToken string `json:"outputToken" validate:"required"`
	Amount      string `json:"amount" validate:"required"`
}

// QuoteHandler godoc
// @Summary Get a token swap quote
// @Description Find the best route and output amount for a swap
// @Tags swap
// @Accept json
// @Produce json
// @Param quote body QuoteRequest true "Quote Request"
// @Success 200 {object} sor.QuoteResponse
// @Failure 400 {object} map[string]string
// @Failure 500 {object} map[string]string
// @Router /v1/quote [post]
func QuoteHandler(client *grpc.Client) gin.HandlerFunc {
	validate := validator.New()
	return func(c *gin.Context) {
		var req QuoteRequest
		if err := c.ShouldBindJSON(&req); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}

		if err := validate.Struct(req); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}

		grpcReq := &sor.QuoteRequest{
			InputToken:  req.InputToken,
			OutputToken: req.OutputToken,
			Amount:      req.Amount,
		}

		resp, err := client.Quote(c.Request.Context(), grpcReq)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{
				"error":   "failed to get quote from SOR",
				"details": err.Error(),
			})
			return
		}

		c.JSON(http.StatusOK, gin.H{
			"input_token":         resp.InputToken,
			"output_token":        resp.OutputToken,
			"input_amount":        resp.InputAmount,
			"output_amount":       resp.OutputAmount,
			"human_input_amount":  resp.HumanInputAmount,
			"human_output_amount": resp.HumanOutputAmount,
			"path":                resp.Path,
			"token_path":          resp.TokenPath,
			"price_impact":        resp.PriceImpact,
		})
	}
}
