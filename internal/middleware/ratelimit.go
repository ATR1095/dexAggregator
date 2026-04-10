package middleware

import (
	"context"
	"fmt"
	"net/http"
	"strconv"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/redis/go-redis/v9"
)

type RateLimiter struct {
	client *redis.Client
	limit  int
	window time.Duration
}

func NewRateLimiter(client *redis.Client, limit int, window time.Duration) *RateLimiter {
	return &RateLimiter{
		client: client,
		limit:  limit,
		window: window,
	}
}

func (rl *RateLimiter) Middleware() gin.HandlerFunc {
	return func(c *gin.Context) {
		key := fmt.Sprintf("ratelimit:%s", c.ClientIP())
		now := time.Now().UnixNano()
		windowStart := now - rl.window.Nanoseconds()

		pipe := rl.client.Pipeline()
		pipe.ZRemRangeByScore(context.Background(), key, "0", strconv.FormatInt(windowStart, 10))
		pipe.ZAdd(context.Background(), key, redis.Z{Score: float64(now), Member: now})
		pipe.ZCount(context.Background(), key, "-inf", "+inf")
		pipe.Expire(context.Background(), key, rl.window)

		cmds, err := pipe.Exec(context.Background())
		if err != nil {
			c.AbortWithStatusJSON(http.StatusInternalServerError, gin.H{"error": "rate limit error"})
			return
		}

		count := cmds[2].(*redis.IntCmd).Val()
		if count > int64(rl.limit) {
			c.AbortWithStatusJSON(http.StatusTooManyRequests, gin.H{"error": "too many requests"})
			return
		}

		c.Next()
	}
}
