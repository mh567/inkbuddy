use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

/// Host configuration, read from `~/.inkbuddy/config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub display: DisplayConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Listen address. 0.0.0.0 so the E-Ink reader on the LAN can reach it.
    /// HTTP 服务不带认证，仅在可信局域网使用；网络共享时把 bind 设为
    /// 127.0.0.1，再按需做转发。
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    /// Title shown in the browser tab.
    pub title: String,
    /// Fallback refresh interval when no plugin reports one.
    pub refresh_ms: u64,
    /// E-Ink rendering mode: gray / bw / text. Pushed by the macOS control app.
    pub mode: String,
    /// Invert colors on the E-Ink page.
    pub invert: bool,
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
            mode: "gray".to_string(),
            invert: false,
        }
    }
}

/// Absolute path holding config, caches and installed plugins. A relative or
/// empty `INKBUDDY_HOME` resolves against the current directory, keeping config
/// and plugins in one place.
pub fn home_dir() -> Result<PathBuf> {
    let raw = std::env::var("INKBUDDY_HOME").unwrap_or_default();
    let path = if raw.trim().is_empty() {
        let base = dirs::home_dir().context("cannot resolve home directory")?;
        base.join(".inkbuddy")
    } else {
        let p = PathBuf::from(&raw);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        }
    };
    Ok(path)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cannot read config, using defaults");
            return Ok(Config::default());
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(c) => Ok(c),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "invalid config, using defaults");
            Ok(Config::default())
        }
    }
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
/// `plugins` directory so `cargo run` works straight from a checkout.
///
/// Every entry is absolute. A background app started by Finder or launchd has
/// an arbitrary cwd, so a bare relative `plugins` would silently point at the
/// a wrong directory, leaving the page with no plugins.
pub fn plugin_dirs(explicit: Option<&Path>) -> Vec<PathBuf> {
    let absolutize = |p: PathBuf| -> PathBuf {
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        }
    };
    let mut out = Vec::new();
    if let Some(p) = explicit {
        out.push(absolutize(p.to_path_buf()));
    }
    if let Ok(home) = home_dir() {
        out.push(home.join("plugins"));
    }
    // cargo places the binary at <workspace>/target/<profile>/inkbuddy, so
    // walking up three levels lands on the workspace root that owns `plugins`.
    let repo_plugins = std::env::current_exe().ok().and_then(|exe| {
        exe.parent()?
            .parent()?
            .parent()
            .map(|root| root.join("plugins"))
    });
    if let Some(p) = repo_plugins {
        out.push(p);
    } else {
        out.push(absolutize(PathBuf::from("plugins")));
    }
    out
}
