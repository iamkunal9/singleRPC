mod banner;
mod config;
mod proxy;
mod server;

use crate::banner::print_banner;
use crate::config::load_config;
use crate::proxy::RpcProxy;
use crate::server::run_server;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_CONFIG_PATH: &str = env!("GENERATED_CONFIG_PATH");

#[derive(Parser, Debug)]
#[command(name = "singlerpc", author = "iamkunal9", version, about = None, long_about = None)]
struct Cli {
    #[arg(
        short = 'c',
        long = "config",
        value_name = "FILE",
        default_value = DEFAULT_CONFIG_PATH,
        help = "Path to config.json (defaults to the generated Chainlist snapshot)"
    )]
    config: PathBuf,

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    print_banner();
    let args = Cli::parse();

    let chains = load_config(&args.config)?;
    let proxy = Arc::new(RpcProxy::with_timeout(
        chains,
        args.verbose as u8,
        std::time::Duration::from_secs(args.timeout_secs),
    ));
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    println!("RPC Proxy Server running on port {}", args.port);
    run_server(proxy, addr).await?;
    Ok(())
}
