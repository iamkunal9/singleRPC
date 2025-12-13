use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_SOURCE_URL: &str = "http://chainlist.org/rpcs.json";
const OUTPUT_FILE_NAME: &str = "chains_config.json";
const SOURCE_URL_ENV: &str = "CHAINLIST_RPC_URL";
const GENERATED_PATH_ENV: &str = "GENERATED_CONFIG_PATH";

#[derive(Deserialize)]
struct ChainEntry {
    #[serde(rename = "chainId")]
    chain_id: u64,
    #[serde(rename = "isTestnet", default)]
    is_testnet: bool,
    #[serde(default)]
    rpc: Vec<RpcEntry>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RpcEntry {
    Simple(String),
    Detailed(RpcObject),
}

#[derive(Deserialize)]
struct RpcObject {
    url: Option<String>,
}

impl RpcEntry {
    fn into_url(self) -> Option<String> {
        match self {
            RpcEntry::Simple(url) => Some(url),
            RpcEntry::Detailed(obj) => obj.url,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={SOURCE_URL_ENV}");

    let source_url = env::var(SOURCE_URL_ENV).unwrap_or_else(|_| DEFAULT_SOURCE_URL.to_string());
    let response = ureq::get(&source_url)
        .timeout(Duration::from_secs(30))
        .call()?;

    let chains: Vec<ChainEntry> = response.into_json()?;
    let config = build_config(chains);

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let output_path = out_dir.join(OUTPUT_FILE_NAME);
    let serialized = serde_json::to_string_pretty(&config)?;
    fs::write(&output_path, serialized)?;

    println!(
        "cargo:rustc-env={GENERATED_PATH_ENV}={}",
        output_path.display()
    );
    println!(
        "cargo:warning=Generated RPC config with {} chains from {}",
        config.len(),
        source_url
    );

    Ok(())
}

fn build_config(chains: Vec<ChainEntry>) -> BTreeMap<String, Vec<String>> {
    let mut config = BTreeMap::new();

    for chain in chains {
        if chain.is_testnet {
            continue;
        }

        let mut seen = HashSet::new();
        let mut urls = Vec::new();

        for rpc in chain.rpc {
            if let Some(url) = rpc.into_url() {
                let trimmed = url.trim().to_string();
                if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                    continue;
                }
                if seen.insert(trimmed.clone()) {
                    urls.push(trimmed);
                }
            }
        }

        if !urls.is_empty() {
            config.insert(chain.chain_id.to_string(), urls);
        }
    }

    config
}
