mod banner;

use crate::banner::print_banner;
use clap::Parser;
use singlerpc::{
    DEFAULT_CONFIG_JSON, RpcProxy, default_health_store_path, load_config, load_config_from_str,
    run_server,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const INSTALL_URL: &str = "https://raw.githubusercontent.com/iamkunal9/singleRPC/main/install.sh";

const RELEASES_LATEST_URL: &str = "https://github.com/iamkunal9/singleRPC/releases/latest";

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

    #[arg(
        long = "health-file",
        value_name = "FILE",
        help = "Persist hard-dead endpoint health in this file (default: ~/.singlerpc/health.json)"
    )]
    health_file: Option<PathBuf>,

    #[arg(
        long = "no-health-file",
        action = clap::ArgAction::SetTrue,
        help = "Keep endpoint health in memory only"
    )]
    no_health_file: bool,

    #[arg(
        short = 'u',
        long = "update",
        action = clap::ArgAction::SetTrue,
        help = "Re-download and install the latest release via install.sh, then exit"
    )]
    update: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    print_banner();
    let args = Cli::parse();

    if args.update {
        return run_update().await;
    }

    if let Some(latest) = check_latest_version().await {
        let current = env!("CARGO_PKG_VERSION");
        if is_newer(&latest, current) {
            eprintln!("[!] A newer singlerpc v{latest} is available (you are on v{current}).");
            eprintln!("[!] Run `singlerpc --update` to upgrade.");
        }
    }

    let chains = match args.config.as_deref() {
        Some(path) => load_config(path)?,
        None => load_config_from_str(DEFAULT_CONFIG_JSON)?,
    };
    let health_store_path = if args.no_health_file {
        None
    } else {
        args.health_file.or_else(default_health_store_path)
    };
    let proxy = Arc::new(RpcProxy::with_health_store(
        chains,
        args.verbose,
        Duration::from_secs(args.timeout_secs),
        args.auth_token.clone(),
        health_store_path,
    ));
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    println!("RPC Proxy Server running on port {}", args.port);
    run_server(proxy, addr).await?;
    Ok(())
}

async fn run_update() -> Result<(), Box<dyn std::error::Error>> {
    println!("[+] Updating singlerpc via {INSTALL_URL}");
    let status = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(format!("curl -fsSL {INSTALL_URL} | bash"))
        .status()
        .await?;
    if !status.success() {
        return Err(format!("install.sh exited with status {status}").into());
    }
    Ok(())
}

async fn check_latest_version() -> Option<String> {
    let fut = async {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let resp = client.get(RELEASES_LATEST_URL).send().await.ok()?;
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)?
            .to_str()
            .ok()?;
        let tag = loc.rsplit("/tag/").next()?.trim();
        if tag.is_empty() {
            None
        } else {
            Some(tag.trim_start_matches('v').to_string())
        }
    };
    tokio::time::timeout(Duration::from_secs(3), fut)
        .await
        .ok()
        .flatten()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse().ok()).collect() };
    parse(latest) > parse(current)
}
