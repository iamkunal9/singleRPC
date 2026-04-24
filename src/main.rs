mod banner;
mod config;
mod proxy;
mod server;

use crate::banner::print_banner;
use crate::config::{load_config, load_config_from_str};
use crate::proxy::RpcProxy;
use crate::server::run_server;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_CONFIG_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/chains_config.json"));

#[derive(Parser, Debug)]
#[command(name = "singlerpc", author = "iamkunal9", version, about = None, long_about = None)]
struct Cli {
    #[arg(
        short = 'c',
        long = "config",
        value_name = "FILE",
        help = "Path to config.json (defaults to the embedded Chainlist snapshot)"
    )]
    config: Option<PathBuf>,

    #[arg(
        short = 'p',
        long = "port",
        default_value_t = 3000,
        help = "Port to listen on"
    )]
    port: u16,

    #[arg(short = 'v', action = clap::ArgAction::Count, help = "Increase verbosity (-v, -vv)")]
    verbose: u8,

    #[arg(
        short = 't',
        long = "timeout",
        default_value_t = 5u64,
        help = "Per-RPC request timeout in seconds"
    )]
    timeout_secs: u64,

    #[arg(
        short = 'a',
        long = "auth",
        value_name = "TOKEN",
        help = "Require clients to provide this token (header or ?auth=) before proxying"
    )]
    auth_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    print_banner();
    let args = Cli::parse();

    let chains = match args.config.as_deref() {
        Some(path) => load_config(path)?,
        None => load_config_from_str(DEFAULT_CONFIG_JSON)?,
    };
    let proxy = Arc::new(RpcProxy::with_timeout(
        chains,
        args.verbose as u8,
        std::time::Duration::from_secs(args.timeout_secs),
        args.auth_token.clone(),
    ));
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    println!("RPC Proxy Server running on port {}", args.port);
    run_server(proxy, addr).await?;
    Ok(())
}
