use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

pub type ChainsConfig = HashMap<String, Vec<String>>;

pub fn load_config<P: AsRef<Path>>(path: P) -> Result<ChainsConfig, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let config: ChainsConfig = serde_json::from_reader(file)?;
    Ok(config)
}
