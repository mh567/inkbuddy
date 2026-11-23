mod config;
mod plugin;
mod render;
mod server;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use server::AppState;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "inkbuddy",
    version,
    about = "Idle E-Ink reader as a macOS companion display"
)]
struct Cli {
    /// Override listen address (e.g. 127.0.0.1).
    #[arg(long)]
    bind: Option<String>,

    /// Override listen port.
    #[arg(long)]
    port: Option<u16>,

    /// Additional plugin directory; scanned first.
    #[arg(long, value_name = "DIR")]
    plugins_dir: Option<PathBuf>,

    /// Write a default config.toml if missing, print the path, and exit.
    #[arg(long)]
    init_config: bool,

    /// List discovered plugins and exit without starting the server.
    #[arg(long)]
    list_plugins: bool,

    /// Log level (trace/debug/info/warn/error).
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.clone().into()),
        )
        .with_target(false)
        .init();

    if cli.init_config {
        let path = config::ensure_default_file()?;
        println!("{}", path.display());
        return Ok(());
    }

    let mut cfg: Config = config::load()?;
    if let Some(b) = cli.bind {
        cfg.server.bind = b;
    }
    if let Some(p) = cli.port {
        cfg.server.port = p;
    }

    let dirs = config::plugin_dirs(cli.plugins_dir.as_deref());
    let plugins = plugin::discover(&dirs)?;

    if cli.list_plugins {
        for id in &plugins.order {
            let p = plugins.get(id).unwrap();
            println!(
                "{:<16} {:<12} enabled={:<5} default_on={:<5} {}",
                id,
                p.manifest.plugin.version,
                p.manifest.runtime.enabled,
                p.manifest.runtime.default_on,
                p.command.display()
            );
        }
        return Ok(());
    }

    if plugins.order.is_empty() {
        warn!("no plugins found in {:?}; the page will be empty", dirs);
    } else {
        info!(
            "discovered {} plugin(s): {}",
            plugins.order.len(),
            plugins.order.join(", ")
        );
    }

    // Resolve the address before moving cfg into shared state.
    let bind = cfg.server.bind.clone();
    let port = cfg.server.port;
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("invalid listen address {bind}:{port}"))?;

    let state = std::sync::Arc::new(AppState {
        config: cfg,
        plugins: tokio::sync::RwLock::new(plugins),
        plugin_dirs: dirs,
        last_scan: tokio::sync::Mutex::new(std::time::Instant::now()),
    });
    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    // Print every usable LAN address so the URL can be typed into the reader.
    let lan = local_ip_addresses();
    info!("InkBuddy listening on {addr}");
    info!("  E-Ink page : http://127.0.0.1:{port}/");
    if let Some((first, rest)) = lan.split_first() {
        info!(
            "  LAN        : http://{}:{port}/   (推荐，输入到墨水屏浏览器)",
            first
        );
        for ip in rest {
            info!("  LAN 备选   : http://{}:{port}/", ip);
        }
    } else {
        warn!("  LAN        : 没检测到可用局域网地址，可用 127.0.0.1 本机验证");
    }
    info!("  Manage     : http://127.0.0.1:{port}/manage");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state))
        .await
        .context("http server error")?;

    Ok(())
}

async fn shutdown_signal(state: std::sync::Arc<AppState>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutting down, stopping plugins");
    state.plugins.read().await.shutdown_all().await;
}

/// Candidate IPv4 addresses the E-Ink reader can reach, best one first.
///
/// A UDP socket "connected" to a public address is not a reliable probe on
/// macOS with a VPN or TUN route: the kernel reports the virtual egress IP
/// (for example 198.18.0.1), which a device on the real LAN cannot reach.
/// We therefore enumerate active interfaces and put RFC1918 addresses at the
/// front, then append any other non-loopback address as a fallback candidate.
fn local_ip_addresses() -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    // Parse ifconfig per interface block, so the order of `inet` and
    // `status: active` inside a block does not matter.
    if let Ok(output) = std::process::Command::new("/sbin/ifconfig").output() {
        let text = String::from_utf8_lossy(&output.stdout);
        let mut block_inets: Vec<String> = Vec::new();
        let mut block_active = false;
        let flush = |inets: &mut Vec<String>, active: bool, out: &mut Vec<String>| {
            if active {
                for ip in inets.drain(..) {
                    if usable(&ip) {
                        out.push(ip);
                    }
                }
            } else {
                inets.clear();
            }
        };
        for line in text.lines() {
            let is_header = !line.starts_with(' ') && !line.starts_with('\t') && !line.is_empty();
            if is_header {
                flush(&mut block_inets, block_active, &mut candidates);
                block_active = false;
                continue;
            }
            let t = line.trim();
            if t.contains("status: active") {
                block_active = true;
            }
            if let Some(rest) = t.strip_prefix("inet ") {
                if let Some(ip) = rest.split_whitespace().next() {
                    block_inets.push(ip.to_string());
                }
            }
        }
        flush(&mut block_inets, block_active, &mut candidates);
    }

    // `ipconfig getifaddr` is the authoritative primary address on macOS;
    // include en0..en9 in case the ifconfig block parse missed one.
    for i in 0..=9 {
        let name = format!("en{i}");
        if let Ok(output) = std::process::Command::new("/usr/sbin/ipconfig")
            .args(["getifaddr", &name])
            .output()
        {
            let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if usable(&ip) {
                candidates.push(ip);
            }
        }
    }

    candidates.sort_by(|a, b| lan_rank(a).cmp(&lan_rank(b)).then(a.cmp(b)));
    candidates.dedup();
    candidates
}

fn usable(ip: &str) -> bool {
    !ip.is_empty() && !ip.starts_with("127.") && !ip.starts_with("169.254.") && ip != "0.0.0.0"
}

/// Home-router LAN addresses first, other private ranges next, virtual
/// egress addresses (VPN/TUN, CGNAT) last.
fn lan_rank(ip: &str) -> u8 {
    let parts: Vec<u8> = ip.split('.').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 4 {
        return 4;
    }
    match (parts[0], parts[1]) {
        (192, 168) => 0,
        (10, _) => 1,
        (172, 16..=31) => 2,
        _ => 3,
    }
}
