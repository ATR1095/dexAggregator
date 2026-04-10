package grpc

import (
	"context"
	"time"

	"github.com/dexAggregator/APIGateway/proto/sor"
	"github.com/sony/gobreaker"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

type Client struct {
	conn   *grpc.ClientConn
	client sor.SORServiceClient
	cb     *gobreaker.CircuitBreaker
}

func NewClient(addr string) (*Client, error) {
	conn, err := grpc.NewClient(addr, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return nil, err
	}

	cb := gobreaker.NewCircuitBreaker(gobreaker.Settings{
		Name:        "sor-client",
		MaxRequests: 3,
		Interval:    5 * time.Second,
		Timeout:     10 * time.Second,
	})

	return &Client{
		conn:   conn,
		client: sor.NewSORServiceClient(conn),
		cb:     cb,
	}, nil
}

func (c *Client) Close() error {
	return c.conn.Close()
}

func (c *Client) Quote(ctx context.Context, req *sor.QuoteRequest) (*sor.QuoteResponse, error) {
	result, err := c.cb.Execute(func() (interface{}, error) {
		ctx, cancel := context.WithTimeout(ctx, 500*time.Millisecond)
		defer cancel()
		return c.client.Quote(ctx, req)
	})
	if err != nil {
		return nil, err
	}
	return result.(*sor.QuoteResponse), nil
}

func (c *Client) Swap(ctx context.Context, req *sor.SwapRequest) (*sor.SwapResponse, error) {
	result, err := c.cb.Execute(func() (interface{}, error) {
		ctx, cancel := context.WithTimeout(ctx, 1*time.Second) // Swap might need more than 500ms
		defer cancel()
		return c.client.Swap(ctx, req)
	})
	if err != nil {
		return nil, err
	}
	return result.(*sor.SwapResponse), nil
}
