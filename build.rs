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
/// Curated endpoints for non-EVM keys (Chainlist is EVM-only), keyed CAIP-2-ish:
/// `solana:mainnet`, `solana:devnet`, `tron:mainnet`.
const NON_EVM_FILE: &str = "non_evm_rpcs.json";

#[derive(Deserialize)]
struct ChainEntry {
    #[serde(rename = "chainId")]
    chain_id: u64,
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
    println!("cargo:rerun-if-changed={NON_EVM_FILE}");

    let source_url = env::var(SOURCE_URL_ENV).unwrap_or_else(|_| DEFAULT_SOURCE_URL.to_string());
    // Offline or Chainlist down: still build, with only the curated non-EVM keys.
    let mut config = match fetch_chainlist(&source_url) {
        Ok(chains) => build_config(chains),
        Err(err) => {
            println!(
                "cargo:warning=Chainlist fetch from {source_url} failed ({err}); \
                 embedding only the curated non-EVM chains"
            );
            BTreeMap::new()
        }
    };

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let non_evm: BTreeMap<String, Vec<String>> =
        serde_json::from_str(&fs::read_to_string(manifest_dir.join(NON_EVM_FILE))?)?;
    for (key, urls) in non_evm {
        config.insert(key, urls);
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let output_path = out_dir.join(OUTPUT_FILE_NAME);
    let serialized = serde_json::to_string_pretty(&config)?;
    fs::write(&output_path, serialized)?;

    println!(
        "cargo:warning=Generated RPC config with {} chains from {}",
        config.len(),
        source_url
    );

    Ok(())
}

fn fetch_chainlist(source_url: &str) -> Result<Vec<ChainEntry>, Box<dyn Error>> {
    let response = ureq::get(source_url)
        .timeout(Duration::from_secs(30))
        .call()?;
    Ok(response.into_json()?)
}

fn build_config(chains: Vec<ChainEntry>) -> BTreeMap<String, Vec<String>> {
    let mut config = BTreeMap::new();

    for chain in chains {
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
