package main

import (
	"context"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	_ "github.com/dexAggregator/APIGateway/docs"
	"github.com/dexAggregator/APIGateway/internal/grpc"
	"github.com/dexAggregator/APIGateway/internal/handlers"
	"github.com/dexAggregator/APIGateway/internal/middleware"
	"github.com/gin-gonic/gin"
	"github.com/redis/go-redis/v9"
	swaggerFiles "github.com/swaggo/files"
	ginSwagger "github.com/swaggo/gin-swagger"
	"go.uber.org/zap"
)

// @title DEX Aggregator API Gateway
// @version 1.0
// @description API Gateway for the Solana DEX Aggregator.
// @host localhost:8081
// @BasePath /
func main() {
	// Initialize Logger
	logger, _ := zap.NewProduction()
	defer logger.Sync()

	// Redis client for rate limiting
	redisAddr := os.Getenv("REDIS_ADDR")
	if redisAddr == "" {
		redisAddr = "localhost:6379"
	}
	rdb := redis.NewClient(&redis.Options{
		Addr: redisAddr,
	})

	// SOR gRPC Client
	sorAddr := os.Getenv("SOR_ADDR")
	if sorAddr == "" {
		sorAddr = "localhost:50052"
	}
	grpcClient, err := grpc.NewClient(sorAddr)
	if err != nil {
		logger.Fatal("failed to connect to SOR", zap.Error(err))
	}
	defer grpcClient.Close()

	// API Gateway router
	r := gin.New()
	r.Use(gin.Recovery())
	r.Use(middleware.Logger(logger))
	r.Use(middleware.CORS())

	// Public v1 group
	v1 := r.Group("/v1")

	// Rate Limiting: 100 requests per minute
	rl := middleware.NewRateLimiter(rdb, 100, time.Minute)
	v1.Use(rl.Middleware())

	// Routes
	v1.POST("/quote", handlers.QuoteHandler(grpcClient))
	v1.POST("/swap", handlers.SwapHandler(grpcClient))
	v1.GET("/tokens", handlers.TokensHandler(grpcClient))

	// Health check
	r.GET("/health", func(c *gin.Context) {
		c.JSON(http.StatusOK, gin.H{"status": "ok"})
	})

	// Swagger documentation
	r.GET("/api", func(c *gin.Context) {
		c.Redirect(http.StatusMovedPermanently, "/api/index.html")
	})
	r.GET("/api/*any", ginSwagger.WrapHandler(swaggerFiles.Handler))

	// Graceful shutdown
	srv := &http.Server{
		Addr:    ":8081",
		Handler: r,
	}

	go func() {
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			logger.Fatal("listen: ", zap.Error(err))
		}
	}()

	quit := make(chan os.Signal, 1)
	signal.Notify(quit, syscall.SIGINT, syscall.SIGTERM)
	<-quit
	logger.Info("shutting down server...")

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := srv.Shutdown(ctx); err != nil {
		logger.Fatal("server forced to shutdown: ", zap.Error(err))
	}

	logger.Info("server exiting")
}
