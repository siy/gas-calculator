use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use dotenv::dotenv;

use revm::primitives::AccountInfo;
use revm::{
    primitives::{Address, Bytes, TransactTo, U256},
    InMemoryDB, EVM,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tokio::net::TcpListener;
use tracing::{error, info};

#[derive(Deserialize)]
struct GasEstimateRequest {
    from: String,
    to: String,
    value: String,
    data: String,
}

#[derive(Serialize)]
struct GasEstimateResponse {
    gas_estimate_hex: String,
    gas_estimate_dec: String,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Invalid address format: {0}")]
    InvalidAddress(String),
    #[error("Invalid hex value: {0}")]
    InvalidHexValue(String),
    #[error("Hex string is too long")]
    TooLongHexValue(),
    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),
}

// Metrics counters
struct Metrics {
    total_requests: AtomicU64,
    successful_estimates: AtomicU64,
    failed_estimates: AtomicU64,
    total_iterations: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            successful_estimates: AtomicU64::new(0),
            failed_estimates: AtomicU64::new(0),
            total_iterations: AtomicU64::new(0),
        }
    }
}

// Application state
struct AppState {
    metrics: Arc<Metrics>,
}

// Parse hex string to Address
fn parse_address(input: &str) -> Result<Address, Error> {
    let input = sanitize_input(input);
    let bytes =
        hex::decode(input.as_bytes()).map_err(|_| Error::InvalidAddress(input.to_string()))?;

    if bytes.len() != 20 {
        return Err(Error::InvalidAddress(format!(
            "Address must be 20 bytes, got {}",
            bytes.len()
        )));
    }

    let mut address_bytes = [0u8; 20];
    address_bytes.copy_from_slice(&bytes);
    Ok(Address::from(address_bytes))
}

fn sanitize_input(input: &str) -> String {
    let data = if let Some(stripped) = input.strip_prefix("0x") {
        stripped
    } else {
        input
    };

    if data.len() % 2 != 0 {
        "0".to_string() + data
    } else {
        data.to_string()
    }
}

// Parse hex string to U256
fn parse_u256(input: &str) -> Result<U256, Error> {
    let input = sanitize_input(input);

    if input.is_empty() {
        return Ok(U256::ZERO);
    }

    let bytes = hex::decode(input.as_bytes()).map_err(|_| Error::InvalidHexValue(input))?;

    if bytes.len() > 32 {
        return Err(Error::TooLongHexValue());
    }
    // Create a fixed size array for U256::from_be_bytes
    let mut be_bytes = [0u8; 32];
    let start_idx = 32 - bytes.len();
    be_bytes[start_idx..].copy_from_slice(&bytes);

    Ok(U256::from_be_bytes(be_bytes))
}

fn parse_bytes(input: &str) -> Result<Bytes, Error> {
    let input = sanitize_input(input);

    let bytes =
        hex::decode(input.as_bytes()).map_err(|_| Error::InvalidHexValue(input.to_string()))?;

    Ok(Bytes::from(bytes))
}

// Calculate intrinsic gas cost for the transaction
fn intrinsic_gas(data: &[u8]) -> u64 {
    21_000u64
        + data
            .iter()
            .map(|&b| if b == 0 { 4u64 } else { 16u64 })
            .reduce(|acc, v| acc + v)
            .unwrap_or(0)
}

// Simulated gas estimation using local EVM
async fn estimate_gas_local(request: &GasEstimateRequest) -> Result<(u64, u64), Error> {
    // Parse request parameters
    let from = parse_address(&request.from)?;
    let to = parse_address(&request.to)?;
    let value = parse_u256(&request.value)?;
    let data = parse_bytes(&request.data)?;

    // Gas estimation using binary search
    let mut low = intrinsic_gas(&data);
    let mut high = 30_000_000u64; // Use a reasonable upper bound
    let mut iterations = 0;

    info!(
        "Starting local gas estimation with range: low={}, high={}",
        low, high
    );

    while low < high {
        iterations += 1;
        let mid = (low + high) / 2;

        let mut evm = EVM::new();
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            from,
            AccountInfo {
                balance: U256::MAX,
                ..Default::default()
            },
        );
        evm.database(db);

        evm.env.tx.caller = from;
        evm.env.tx.transact_to = TransactTo::Call(to);
        evm.env.tx.value = value;
        evm.env.tx.data = data.clone();
        evm.env.tx.gas_limit = mid;

        let result = evm.transact();

        match result {
            Ok(exec_result) => {
                // Check if the transaction was successful
                if !exec_result.result.is_success() {
                    // Transaction failed, try with more gas
                    low = mid + 1;
                } else {
                    // Transaction succeeded, try with less gas
                    high = mid;
                }
            }
            Err(_) => {
                // Error during execution, try with more gas
                low = mid + 1;
            }
        }
    }

    info!(
        "Local gas estimation completed after {} iterations: gas={}",
        iterations, low
    );
    Ok((low, iterations))
}

// Gas estimation function that chooses the appropriate estimation method
async fn gas_estimate(_state: &AppState, request: GasEstimateRequest) -> Result<(u64, u64), Error> {
    estimate_gas_local(&request).await
}

// HTTP handler for gas estimation
async fn estimate_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GasEstimateRequest>,
) -> Result<Json<GasEstimateResponse>, StatusCode> {
    state.metrics.total_requests.fetch_add(1, Ordering::Relaxed);

    let start = Instant::now();
    let result = gas_estimate(&state, req).await;
    let duration = start.elapsed();

    match result {
        Ok((gas, iterations)) => {
            info!(
                "Gas estimation took {:?} with {} iterations",
                duration, iterations
            );
            state
                .metrics
                .successful_estimates
                .fetch_add(1, Ordering::Relaxed);
            state
                .metrics
                .total_iterations
                .fetch_add(iterations, Ordering::Relaxed);
            Ok(Json(GasEstimateResponse {
                gas_estimate_hex: format!("0x{:x}", gas),
                gas_estimate_dec: format!("{}", gas),
            }))
        }
        Err(e) => {
            state
                .metrics
                .failed_estimates
                .fetch_add(1, Ordering::Relaxed);
            error!("Gas estimation error: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Metrics endpoint
async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
    let metrics = &state.metrics;

    format!(
        "# HELP gas_calculator_requests_total Total number of gas estimation requests\n\
        # TYPE gas_calculator_requests_total counter\n\
        gas_calculator_requests_total {}\n\
        # HELP gas_calculator_estimates_success_total Successful gas estimations\n\
        # TYPE gas_calculator_estimates_success_total counter\n\
        gas_calculator_estimates_success_total {}\n\
        # HELP gas_calculator_estimates_failed_total Failed gas estimations\n\
        # TYPE gas_calculator_estimates_failed_total counter\n\
        gas_calculator_estimates_failed_total {}\n\
        # HELP gas_calculator_iterations_total Total number of binary search iterations\n\
        # TYPE gas_calculator_iterations_total counter\n\
        gas_calculator_iterations_total {}\n",
        metrics.total_requests.load(Ordering::Relaxed),
        metrics.successful_estimates.load(Ordering::Relaxed),
        metrics.failed_estimates.load(Ordering::Relaxed),
        metrics.total_iterations.load(Ordering::Relaxed)
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load environment variables and initialize logging
    dotenv().ok();
    tracing_subscriber::fmt::init();

    // Create the application state
    let app_state = Arc::new(AppState {
        metrics: Arc::new(Metrics::new()),
    });

    // Build Axum application
    let app = Router::new()
        .route("/estimate", post(estimate_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(app_state);

    // Start HTTP server
    let addr = SocketAddr::from(([0, 0, 0, 0], 3030));
    info!("Starting server on {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper function for test requests
    fn create_test_request(from: &str, to: &str, value: &str, data: &str) -> GasEstimateRequest {
        GasEstimateRequest {
            from: from.to_string(),
            to: to.to_string(),
            value: value.to_string(),
            data: data.to_string(),
        }
    }

    #[tokio::test]
    async fn test_intrinsic_gas_calculation() {
        // Test empty data
        assert_eq!(intrinsic_gas(&[]), 21000);

        // Test zero bytes
        let zero_bytes = vec![0u8; 10];
        assert_eq!(
            intrinsic_gas(&zero_bytes),
            21000 + (10 * 4),
            "Zero bytes calculation incorrect"
        );

        // Test non-zero bytes
        let nonzero_bytes = vec![1u8; 10];
        assert_eq!(
            intrinsic_gas(&nonzero_bytes),
            21000 + (10 * 16),
            "Non-zero bytes calculation incorrect"
        );

        // Test mixed bytes
        let mixed_bytes = vec![0, 1, 0, 1, 0];
        assert_eq!(
            intrinsic_gas(&mixed_bytes),
            21000 + (3 * 4) + (2 * 16),
            "Mixed bytes calculation incorrect"
        );
    }

    #[tokio::test]
    async fn test_parse_address() {
        // Valid address
        let valid = "0x742d35Cc6634C0532925a3b844Bc454e4438f44e";
        assert!(parse_address(valid).is_ok());

        // Invalid address (too short)
        let invalid = "0x742d";
        assert!(parse_address(invalid).is_err());

        // Invalid hex
        let invalid = "0xZZZZ35Cc6634C0532925a3b844Bc454e4438f44e";
        assert!(parse_address(invalid).is_err());
    }

    #[tokio::test]
    async fn test_parse_u256() {
        // Zero
        assert_eq!(parse_u256("0x0").unwrap(), U256::ZERO);

        // Empty string (should be zero)
        assert_eq!(parse_u256("0x").unwrap(), U256::ZERO);

        // Some value
        assert_eq!(parse_u256("0x1234").unwrap(), U256::from(0x1234));

        // Max value
        let max = "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert_eq!(parse_u256(max).unwrap(), U256::MAX);

        // Invalid hex
        let invalid = "0xZZZZ";
        assert!(parse_u256(invalid).is_err());
    }

    #[tokio::test]
    async fn test_strip_prefix() {
        assert_eq!(sanitize_input("0x1234"), "1234");
        assert_eq!(sanitize_input("1234"), "1234");
        assert_eq!(sanitize_input(""), "");
        assert_eq!(sanitize_input("0x"), "");
    }

    #[tokio::test]
    async fn test_eth_transfer() {
        // Simple ETH transfer => 21_000 gas
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "0x1234", // Some ETH value
            "0x",     // Empty data
        );

        let (gas, iterations) = estimate_gas_local(&request).await.unwrap();
        assert_eq!(gas, 21000, "ETH transfer should cost exactly 21000 gas");
        assert!(iterations > 0, "Should perform at least one iteration");
    }

    #[tokio::test]
    async fn test_invalid_inputs() {
        // Test invalid from address
        let request = create_test_request(
            "0xinvalid",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "0x0",
            "0x",
        );
        assert!(estimate_gas_local(&request).await.is_err());

        // Test invalid to address
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0xinvalid",
            "0x0",
            "0x",
        );
        assert!(estimate_gas_local(&request).await.is_err());

        // Test invalid value
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "0xinvalid",
            "0x",
        );
        assert!(estimate_gas_local(&request).await.is_err());

        // Test invalid data
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "0x0",
            "0xinvalid",
        );
        assert!(estimate_gas_local(&request).await.is_err());
    }

    #[tokio::test]
    async fn test_contract_call_with_calldata() {
        // A minimal contract call with calldata
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "0x0",
            "0xa9059cbb000000000000000000000000d9e1ce17f2641f24ae83637ab66a2cca9c378b9f0000000000000000000000000000000000000000000000000de0b6b3a7640000", // ERC20 transfer
        );

        let (gas, iterations) = estimate_gas_local(&request).await.unwrap();
        // Using InMemoryDB without contract code, the call will "succeed" with just the intrinsic gas cost
        assert!(
            gas >= intrinsic_gas(&hex::decode(&request.data[2..]).unwrap()),
            "Gas should be at least the intrinsic cost"
        );
        assert!(iterations > 0, "Should perform at least one iteration");
    }

    #[tokio::test]
    async fn test_deploy_contract() {
        // Simple contract deployment
        let contract_bytecode =
            "0x6080604052348015600f57600080fd5b50603f80601d6000396000f3fe6080604052600080fdaa";
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0x0000000000000000000000000000000000000000",
            "0x0",
            contract_bytecode,
        );

        let bytecode_bytes = hex::decode(&contract_bytecode[2..]).unwrap();
        let (gas, iterations) = estimate_gas_local(&request).await.unwrap();

        // Contract deployment costs should be at least intrinsic + bytecode * 200
        let min_expected = intrinsic_gas(&bytecode_bytes);

        assert!(
            gas >= min_expected,
            "Contract deployment should cost at least intrinsic {min_expected} > {gas}"
        );
        assert!(iterations > 0, "Should perform at least one iteration");
    }

    #[tokio::test]
    async fn test_zero_value_transfer() {
        // Zero value transfer
        let request = create_test_request(
            "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "0x0", // Zero value
            "0x",  // Empty data
        );

        let (gas, _) = estimate_gas_local(&request).await.unwrap();
        assert_eq!(
            gas, 21000,
            "Zero value transfer should still cost 21000 gas"
        );
    }

    #[tokio::test]
    async fn test_data_size_impact() {
        // Test increasing data size impact on gas
        let small_data = "0x1234";
        let med_data = "0x1234567890abcdef1234567890abcdef";
        let large_data = "0x".to_owned() + &"1234567890abcdef".repeat(10);

        let make_request = |data: &str| {
            create_test_request(
                "0x742d35Cc6634C0532925a3b844Bc454e4438f44e",
                "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
                "0x0",
                data,
            )
        };

        let (small_gas, _) = estimate_gas_local(&make_request(small_data)).await.unwrap();
        let (med_gas, _) = estimate_gas_local(&make_request(med_data)).await.unwrap();
        let (large_gas, _) = estimate_gas_local(&make_request(&*large_data))
            .await
            .unwrap();

        assert!(small_gas < med_gas, "More data should cost more gas");
        assert!(med_gas < large_gas, "More data should cost more gas");
    }
}
