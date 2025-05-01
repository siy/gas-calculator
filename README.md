# Gas Estimation Service

## Overview
`gas-calculator` is a Rust-based web service for estimating 
Ethereum gas usage locally. It provides an HTTP API for clients 
to submit transaction details and receive gas estimates without 
needing to query an external Ethereum client.

## Features
- Local gas estimation for Ethereum transactions
- Metrics endpoint for monitoring request statistics

## Prerequisites
- Rust 1.86.0 or higher

## Usage

### API Endpoints

- POST `/estimate`

  Accepts a JSON payload describing the Ethereum transaction for which gas is to be estimated:

  ```json lines
  {
    "from": "0x...",          // Sender address in hex string
    "to": "0x...",            // Recipient address in hex string
    "value": "0x...",         // Amount in Wei as hex string
    "data": "0x..."           // Hex-encoded calldata or contract bytecode
  }
  ```

  Returns a JSON response with the gas estimate:

  ```json lines
  {
    "gas_estimate": "0x..."   // Gas estimate as hex string
  }
  ```

- GET `/metrics`

  Returns current server metrics like total requests, successes, and failures. Useful for monitoring.

### Command Line Examples:

#### Gas Estimation
```shell
curl -X POST http://localhost:3030/estimate \
-H "Content-Type: application/json" \
-d '{ "from": "0x742d35Cc6634C0532925a3b844Bc454e4438f44e", "to": "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984", "value": "0x2386f26fc10000", "data": "0x0" }'

```
#### Metrics
```shell
curl http://localhost:3030/metrics
```
