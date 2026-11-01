//! Live check of the curated non-EVM keys through `RpcProxy::request_json`.
//!
//! cargo run --release --example non_evm_check -- [base64-signed-devnet-tx]
use serde_json::{Value, json};
use singlerpc::{DEFAULT_CONFIG_JSON, RpcProxy, load_config_from_str};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let signed_tx = std::env::args().nth(1);
    let config = load_config_from_str(DEFAULT_CONFIG_JSON)?;
    // verbose=1 prints one "-> Hitting endpoint" line per upstream attempt.
    let proxy = RpcProxy::with_timeout(config, 1, Duration::from_secs(5), None);

    let mut calls: Vec<(&str, Value)> = vec![
        (
            "solana:devnet",
            json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash"}),
        ),
        (
            "solana:mainnet",
            json!({"jsonrpc":"2.0","id":2,"method":"getSlot"}),
        ),
        (
            "solana:mainnet",
            json!({"jsonrpc":"2.0","id":3,"method":"getLatestBlockhash","params":[{"commitment":"confirmed"}]}),
        ),
        (
            "solana:mainnet",
            json!({"jsonrpc":"2.0","id":4,"method":"getSignatureStatuses","params":[["1111111111111111111111111111111111111111111111111111111111111111"],{"searchTransactionHistory":false}]}),
        ),
        (
            "solana:devnet",
            json!({"jsonrpc":"2.0","id":5,"method":"sendTransaction","params":["AQ==",{"encoding":"base64"}]}),
        ),
        (
            "tron:mainnet",
            json!({"jsonrpc":"2.0","id":6,"method":"eth_blockNumber","params":[]}),
        ),
    ];
    if let Some(tx) = &signed_tx {
        calls.push(("solana:devnet", json!({"jsonrpc":"2.0","id":7,"method":"simulateTransaction","params":[tx,{"encoding":"base64"}]})));
        calls.push(("solana:devnet", json!({"jsonrpc":"2.0","id":8,"method":"sendTransaction","params":[tx,{"encoding":"base64"}]})));
    }
    for (key, payload) in calls {
        let method = payload["method"].clone();
        println!("=== {key} {method}");
        let started = Instant::now();
        let result = proxy.request_json(key, payload).await;
        let text = match result {
            Ok(v) => v.to_string(),
            Err(e) => format!("ERR {e}"),
        };
        println!(
            "RESULT {key} {method} ({} ms): {}",
            started.elapsed().as_millis(),
            &text[..text.len().min(260)]
        );
    }
    Ok(())
}
