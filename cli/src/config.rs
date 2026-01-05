// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

const APP_DIR_NAME: &str = "hpc";
const CONFIG_FILE_NAME: &str = "hpc.toml";
const DEFAULT_PORT: u16 = 50056;
const DEFAULT_HOST: &str = "127.0.0.1";

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    port: Option<u16>,
}

#[derive(Debug)]
pub struct Config {
    pub port: u16,
    pub config_path: Option<PathBuf>,
}

pub fn load(config_path_override: Option<PathBuf>) -> Result<Config> {
    let required = config_path_override.is_some();
    let config_path = match config_path_override {
        Some(path) => Some(expand_path(path)),
        None => default_config_path().ok(),
    };

    let file_config = match config_path.as_deref() {
        Some(path) => read_config_file(path, required)?,
        None => FileConfig::default(),
    };

    let port = file_config.port.unwrap_or(DEFAULT_PORT);
    if port == 0 {
        anyhow::bail!("port must be between 1 and 65535");
    }

    Ok(Config {
        port,
        config_path,
    })
}

pub fn daemon_endpoint(config_path_override: Option<PathBuf>) -> Result<String> {
    let config = load(config_path_override)?;
    Ok(format!("http://{DEFAULT_HOST}:{}", config.port))
}

fn read_config_file(path: &Path, required: bool) -> Result<FileConfig> {
    if !path.exists() {
        if required {
            anyhow::bail!("config file not found at {}", path.display());
        }
        return Ok(FileConfig::default());
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

fn expand_path(path: PathBuf) -> PathBuf {
    let path_string = path.to_string_lossy().to_string();
    let expanded = shellexpand::tilde(&path_string);
    PathBuf::from(expanded.as_ref())
}

fn default_config_path() -> Result<PathBuf> {
    Ok(default_config_dir()?.join(CONFIG_FILE_NAME))
}

fn default_config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("failed to resolve config directory")?;
    Ok(base.join(APP_DIR_NAME))
}
