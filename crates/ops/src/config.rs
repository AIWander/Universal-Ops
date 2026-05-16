//! Configuration — resolved at runtime, never hardcoded.

use once_cell::sync::Lazy;
use std::path::PathBuf;

#[allow(dead_code)]
pub struct Config {
    pub volumes_path: String,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub log_dir: PathBuf,
    pub dashboard_port: u16,
}

static CONFIG: Lazy<Config> = Lazy::new(|| {
    let volumes = std::env::var("VOLUMES_PATH").unwrap_or_default();

    let ops_base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Ops");

    let data_dir = ops_base.join("data");
    let config_dir = ops_base.join("config");
    let backup_dir = ops_base.join("backups");
    let log_dir = ops_base.join("logs");

    for dir in [&data_dir, &config_dir, &backup_dir, &log_dir] {
        let _ = std::fs::create_dir_all(dir);
    }

    Config {
        volumes_path: volumes,
        data_dir,
        config_dir,
        backup_dir,
        log_dir,
        dashboard_port: 8001,
    }
});

pub fn get_config() -> &'static Config {
    &CONFIG
}
