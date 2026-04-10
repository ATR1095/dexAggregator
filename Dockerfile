# Use the official Golang image for building
FROM golang:1.22-alpine AS builder

# Set the working directory
WORKDIR /app

# Copy the rest of the source code
COPY . .

# Ensure go.mod is tidy with the full source code
RUN go mod tidy

# Build the application
RUN go build -o price-oracle .

# Use a minimal alpine image for the final stage
FROM alpine:latest
WORKDIR /root/

# Copy the binary from the builder stage
COPY --from=builder /app/price-oracle .
COPY .env .

# Run the binary
CMD ["./price-oracle"]
