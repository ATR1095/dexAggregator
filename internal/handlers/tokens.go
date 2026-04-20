package handlers

import (
	"net/http"

	"github.com/dexAggregator/APIGateway/internal/grpc"
	"github.com/dexAggregator/APIGateway/proto/sor"
	"github.com/gin-gonic/gin"
)

// TokensHandler godoc
// @Summary List all tokens
// @Description Returns all known tokens grouped by their position (token_a and token_b) across liquidity pools
// @Tags tokens
// @Produce json
// @Success 200 {object} sor.ListTokensResponse
// @Failure 500 {object} map[string]string
// @Router /v1/tokens [get]
func TokensHandler(client *grpc.Client) gin.HandlerFunc {
	return func(c *gin.Context) {
		resp, err := client.ListTokens(c.Request.Context(), &sor.ListTokensRequest{})
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{
				"error":   "failed to list tokens from SOR",
				"details": err.Error(),
			})
			return
		}

		c.JSON(http.StatusOK, gin.H{
			"token_a": resp.TokenA,
			"token_b": resp.TokenB,
		})
	}
}
