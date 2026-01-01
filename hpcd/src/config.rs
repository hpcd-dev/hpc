// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

const APP_DIR_NAME: &str = "hpcd";
const CONFIG_FILE_NAME: &str = "hpcd.toml";
const DATABASE_FILE_NAME: &str = "hpcd.sqlite";
const DEFAULT_JOB_CHECK_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    database_path: Option<String>,
    job_check_interval_secs: Option<u64>,
}

#[derive(Debug)]
pub struct Config {
    pub database_path: PathBuf,
    pub job_check_interval_secs: u64,
    pub config_path: PathBuf,
}

pub fn load(config_path_override: Option<PathBuf>) -> Result<Config> {
    let (config_path, required) = match config_path_override {
        Some(path) => (expand_path(path), true),
        None => (default_config_path()?, false),
    };

    let file_config = read_config_file(&config_path, required)?;
    let database_path = match file_config.database_path {
        Some(raw) => resolve_path(&raw, config_path.parent()),
        None => default_database_path()?,
    };

    Ok(Config {
        database_path,
        job_check_interval_secs: file_config
            .job_check_interval_secs
            .unwrap_or(DEFAULT_JOB_CHECK_INTERVAL_SECS),
        config_path,
    })
}

pub fn ensure_database_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create database directory {}",
                parent.display()
            )
        })?;
    }
    Ok(())
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

fn resolve_path(raw: &str, base_dir: Option<&Path>) -> PathBuf {
    let expanded = shellexpand::tilde(raw);
    let path = PathBuf::from(expanded.as_ref());
    if path.is_absolute() {
        return path;
    }
    match base_dir {
        Some(dir) => dir.join(path),
        None => path,
    }
}

fn expand_path(path: PathBuf) -> PathBuf {
    let path_string = path.to_string_lossy().to_string();
    let expanded = shellexpand::tilde(&path_string);
    PathBuf::from(expanded.as_ref())
}

fn default_config_path() -> Result<PathBuf> {
    Ok(default_config_dir()?.join(CONFIG_FILE_NAME))
}

fn default_database_path() -> Result<PathBuf> {
    Ok(default_data_dir()?.join(DATABASE_FILE_NAME))
}

fn default_config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("failed to resolve config directory")?;
    Ok(base.join(APP_DIR_NAME))
}

fn default_data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("failed to resolve data directory")?;
    Ok(base.join(APP_DIR_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolves_relative_database_path_from_config_dir() {
        let dir = TempDir::new().unwrap();
        let config_dir = dir.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("hpcd.toml");
        fs::write(
            &config_path,
            "database_path = \"db/hpcd.sqlite\"\njob_check_interval_secs = 9\n",
        )
        .unwrap();

        let config = load(Some(config_path)).unwrap();
        assert_eq!(
            config.database_path,
            config_dir.join("db").join("hpcd.sqlite")
        );
        assert_eq!(config.job_check_interval_secs, 9);
    }
}
