//! Live check: probe verified state endpoints for a chain/block, then serve a
//! proxy restricted to them (e.g. as `anvil --fork-url` upstream).
//!
//! cargo run --release --example verified_fork -- <chain> <block|latest> <state-file> [port]
use singlerpc::{DEFAULT_CONFIG_JSON, RpcProxy, load_config_from_str, run_server};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let chain = args.get(1).cloned().unwrap_or_else(|| "1".into());
    let block = match args.get(2).map(String::as_str) {
        None | Some("latest") => None,
        Some(b) => Some(b.parse::<u64>()?),
    };
    let state_file = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| "/tmp/singlerpc-state.json".into()),
    );
    let port: Option<u16> = args.get(4).map(|p| p.parse()).transpose()?;

    let config = load_config_from_str(DEFAULT_CONFIG_JSON)?;
    let total = config.get(&chain).map(Vec::len).unwrap_or(0);
    let verbose: u8 = std::env::var("VERBOSE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let prober = RpcProxy::with_stores(
        config,
        verbose,
        Duration::from_secs(5),
        None,
        None,
        Some(state_file.clone()),
    );

    for round in 1..=2 {
        let started = Instant::now();
        let before = prober.probe_request_count();
        let endpoints = prober.verified_state_endpoints(&chain, block).await;
        println!(
            "round {round}: chain {chain} block {:?}: {} of {total} endpoints verified in {:.1}s, {} probe requests",
            block,
            endpoints.len(),
            started.elapsed().as_secs_f64(),
            prober.probe_request_count() - before
        );
        if round == 2 {
            for url in &endpoints {
                let cap = prober.state_store().get(&chain, url).unwrap();
                println!(
                    "  {url}  latency={:?}ms verified_block={:?}",
                    cap.latency_ms, cap.verified_block
                );
            }
            let liars: Vec<_> = prober
                .chains
                .lock()
                .unwrap()
                .get(&chain)
                .map(|c| {
                    c.endpoints
                        .iter()
                        .map(|e| e.url.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
                .into_iter()
                .filter(|u| prober.state_store().is_liar(&chain, u))
                .collect();
            println!("  liars: {liars:?}");
            if let Some(port) = port {
                // Share the prober's store: two stores on one file would race.
                let proxy = Arc::new(RpcProxy::with_shared_state_store(
                    HashMap::from([(chain.clone(), endpoints)]),
                    1,
                    Duration::from_secs(5),
                    None,
                    None,
                    prober.state_store().clone(),
                ));
                println!("serving http://127.0.0.1:{port}/{chain}");
                run_server(proxy, SocketAddr::from(([127, 0, 0, 1], port))).await?;
            }
        }
    }
    Ok(())
}
