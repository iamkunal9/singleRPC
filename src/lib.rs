pub mod capability;
pub mod config;
pub mod proxy;
pub mod server;

pub const DEFAULT_CONFIG_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/chains_config.json"));

pub use capability::{
    Coverage, STATE_CAPABILITY_TTL, StateCapability, StateCapabilityStore,
    default_state_store_path, state_store_path_for_health_file,
};
pub use config::{ChainsConfig, load_config, load_config_from_str};
pub use proxy::{RpcProxy, default_health_store_path, is_evm_chain_key};
pub use server::run_server;
