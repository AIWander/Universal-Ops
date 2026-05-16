//! BagTag — installation tracking. Single implementation (was duplicated 3x).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::get_config;

#[derive(Serialize, Deserialize, Default)]
struct BagtagConfig {
    #[serde(default)]
    install_code: String,
    #[serde(default)]
    install_date: String,
    #[serde(default)]
    machine_id: String,
    #[serde(default)]
    servers: Vec<String>,
}

fn bagtag_path() -> std::path::PathBuf {
    get_config().config_dir.join("bagtag.json")
}

fn load_bagtag() -> BagtagConfig {
    let path = bagtag_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        BagtagConfig::default()
    }
}

fn save_bagtag(cfg: &BagtagConfig) -> Result<()> {
    let path = bagtag_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

pub async fn bag_tag(_args: Value) -> Result<Value> {
    let cfg = load_bagtag();
    let ts = chrono::Local::now().format("%m%d%y%H%M%S").to_string();
    Ok(json!({ "install_code": cfg.install_code, "timestamp": ts }))
}

pub async fn bag_read(_args: Value) -> Result<Value> {
    Ok(json!(load_bagtag()))
}

pub async fn bag_clear(_args: Value) -> Result<Value> {
    save_bagtag(&BagtagConfig::default())?;
    Ok(json!({ "status": "cleared" }))
}
