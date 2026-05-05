curl -X POST http://localhost:8081/v1/swap \
-H "Content-Type: application/json" \
-d '{
  "amount": "1000000000",
  "inputToken": "SOL",
  "outputToken": "USDC",
  "slippageBps": 100,
  "userAddress": "DpNXPNWvWoHaZ9P3WtfGCb2ZdLihW8VW1w1Ph4KDH9iG",
  "recentBlockhash": "5E9A1kXfN3C5k9S9U9U9U9U9U9U9U9U9U9U9U9U9U9U9"
}'
