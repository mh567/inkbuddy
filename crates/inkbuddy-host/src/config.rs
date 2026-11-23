use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Host configuration, read from `~/.inkbuddy/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub display: DisplayConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Listen address. 0.0.0.0 so the E-Ink reader on the LAN can reach it.
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Title shown in the browser tab.
    pub title: String,
    /// Fallback refresh interval when no plugin reports one.
    pub refresh_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            display: DisplayConfig::default(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: 9012,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            title: "InkBuddy".to_string(),
            refresh_ms: 500,
        }
    }
}

/// Directory holding config and installed plugins.
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("INKBUDDY_HOME") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::home_dir().context("cannot resolve home directory")?;
    Ok(base.join(".inkbuddy"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write a default config file if none exists, so users have something to edit.
pub fn ensure_default_file() -> Result<PathBuf> {
    let path = config_path()?;
    if path.exists() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(&Config::default())?;
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Candidate plugin directories, in discovery order.
/// An explicit CLI path wins; then the user install dir; then a repo-local
/// `./plugins` so `cargo run` works straight from a checkout.
pub fn plugin_dirs(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = explicit {
        out.push(p.to_path_buf());
    }
    if let Ok(home) = home_dir() {
        out.push(home.join("plugins"));
    }
    out.push(PathBuf::from("plugins"));
    out
}
