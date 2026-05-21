pub mod config;
pub mod proxy;
pub mod server;

pub const DEFAULT_CONFIG_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/chains_config.json"));

pub use config::{ChainsConfig, load_config, load_config_from_str};
pub use proxy::RpcProxy;
pub use server::run_server;
