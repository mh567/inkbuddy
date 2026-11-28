mod config;
mod plugin;
mod render;
mod server;
mod views;

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

    let initial_display = server::DisplayState {
        mode: cfg.display.mode.clone(),
        invert: cfg.display.invert,
    };
    let state = std::sync::Arc::new(AppState {
        config: cfg,
        display: tokio::sync::RwLock::new(initial_display),
        disabled: tokio::sync::RwLock::new(server::load_disabled(&plugins)),
        views: tokio::sync::RwLock::new(views::load_or_seed(&plugins)),
        plugins: tokio::sync::RwLock::new(plugins),
        plugin_dirs: dirs,
        last_scan: tokio::sync::Mutex::new(std::time::Instant::now()),
        render_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        devices: tokio::sync::RwLock::new(std::collections::HashMap::new()),
    });
    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    // 打印可输入的局域网地址。RFC1918 地址标为推荐；VPN/TUN/CGNAT 地址单列，
    // 真实局域网里的设备通常访问不到。
    let lan = local_ip_addresses();
    info!("InkBuddy listening on {addr}");
    info!("  E-Ink page : http://127.0.0.1:{port}/");
    let recommended: Vec<&String> = lan.iter().filter(|ip| lan_rank(ip) <= 2).collect();
    if let Some(first) = recommended.first() {
        info!(
            "  LAN        : http://{}:{port}/   (推荐，输入到墨水屏浏览器)",
            first
        );
        for ip in recommended.iter().skip(1) {
            info!("  LAN 备选   : http://{}:{port}/", ip);
        }
    } else {
        warn!("  LAN        : 没检测到私有局域网地址，可用 127.0.0.1 本机验证");
    }
    for ip in lan.iter().filter(|ip| lan_rank(ip) > 2) {
        info!(
            "  LAN 其他   : http://{}:{port}/   (VPN/虚拟网卡，设备可能无法访问)",
            ip
        );
    }

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
    // 先克隆句柄并释放读锁，再等待子进程退出，避免关闭流程阻塞并发的重扫。
    let handles: Vec<_> = {
        let guard = state.plugins.read().await;
        guard.order.iter().filter_map(|id| guard.get(id)).collect()
    };
    for p in handles {
        p.shutdown().await;
    }
}

/// Candidate IPv4 addresses the E-Ink reader can reach, best one first.
///
/// 在带 VPN/TUN 路由的 macOS 上，连接公网地址的 UDP socket 探测不可靠：内核
/// 返回虚拟出口 IP（例如 198.18.0.1），真实局域网设备访问不到。这里枚举活跃
/// 网卡，把 RFC1918 地址排在前面，其它非回环地址作为候选附在后面。
fn local_ip_addresses() -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    // 按网卡块解析 ifconfig，`inet` 与 `status: active` 的先后顺序不影响结果。
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

    candidates.retain(|ip| parse_ipv4(ip).is_some());
    candidates.sort_by(|a, b| {
        lan_rank(a)
            .cmp(&lan_rank(b))
            .then_with(|| parse_ipv4(a).cmp(&parse_ipv4(b)))
    });
    candidates.dedup();
    candidates
}

fn parse_ipv4(ip: &str) -> Option<[u8; 4]> {
    let parts: Vec<u8> = ip
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    parts.as_slice().try_into().ok()
}

fn usable(ip: &str) -> bool {
    let Some(o) = parse_ipv4(ip) else {
        return false;
    };
    !(o[0] == 127 || (o[0] == 169 && o[1] == 254) || o == [0, 0, 0, 0])
}

/// Home-router LAN addresses first, other private ranges next, virtual
/// egress addresses (VPN/TUN, CGNAT) last. Invalid addresses sort to the end.
fn lan_rank(ip: &str) -> u8 {
    let Some(o) = parse_ipv4(ip) else {
        return 4;
    };
    match (o[0], o[1]) {
        (192, 168) => 0,
        (10, _) => 1,
        (172, 16..=31) => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_lan_addresses_rank_first() {
        assert_eq!(lan_rank("192.168.28.21"), 0);
        assert_eq!(lan_rank("10.0.0.5"), 1);
        assert_eq!(lan_rank("172.20.1.1"), 2);
        assert_eq!(lan_rank("198.18.0.1"), 3);
        assert_eq!(lan_rank("999.1.1.1"), 4);
    }

    #[test]
    fn unusable_addresses_are_filtered() {
        assert!(!usable("127.0.0.1"));
        assert!(!usable("169.254.1.1"));
        assert!(!usable("0.0.0.0"));
        assert!(!usable("not-an-ip"));
        assert!(usable("192.168.28.21"));
    }

    #[test]
    fn ipv4_sorting_is_numeric_not_lexical() {
        let mut v = vec![
            "192.168.1.10".to_string(),
            "192.168.1.9".to_string(),
            "10.0.0.2".to_string(),
        ];
        v.sort_by(|a, b| {
            lan_rank(a)
                .cmp(&lan_rank(b))
                .then_with(|| parse_ipv4(a).cmp(&parse_ipv4(b)))
        });
        assert_eq!(v, vec!["192.168.1.9", "192.168.1.10", "10.0.0.2"]);
    }
}
