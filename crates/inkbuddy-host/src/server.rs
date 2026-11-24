use crate::config::{self, Config};
use crate::plugin::PluginSet;
use crate::render::render_frame;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use inkbuddy_protocol::{RenderParams, RenderResult};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Upper bound on the `?plugins=` value, so the host allocates at most
/// MAX_PLUGIN_IDS strings for a query.
const MAX_QUERY_BYTES: usize = 4096;
const MAX_PLUGIN_IDS: usize = 64;

const INK_PAGE: &str = include_str!("../../../web/ink.html");

/// 可由控制端在运行时修改的显示设置，墨水屏每次取帧都会读到最新值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayState {
    pub mode: String,
    pub invert: bool,
}

/// 控制端选择的启用插件。None 表示尚未设置，跟随各插件的 default_on；
/// Some(list) 表示显式选择（可为空列表，表示全部关闭）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveState {
    ids: Option<Vec<String>>,
}

fn active_path() -> Option<std::path::PathBuf> {
    config::home_dir().ok().map(|p| p.join("active.json"))
}

pub fn load_active() -> Option<Vec<String>> {
    let path = active_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let state: ActiveState = serde_json::from_str(&text).ok()?;
    state.ids
}

fn save_active(ids: &Option<Vec<String>>) {
    let Some(path) = active_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let state = ActiveState { ids: ids.clone() };
    if let Ok(text) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(path, text);
    }
}

pub struct AppState {
    pub config: Config,
    pub display: tokio::sync::RwLock<DisplayState>,
    /// 控制端设置的启用插件。取帧时优先于 URL 参数，保证设备侧无需改地址。
    pub active: tokio::sync::RwLock<Option<Vec<String>>>,
    pub plugins: tokio::sync::RwLock<PluginSet>,
    pub plugin_dirs: Vec<std::path::PathBuf>,
    /// Throttles directory rescans so 500ms polling stays cheap.
    pub last_scan: tokio::sync::Mutex<Instant>,
    /// Last frame per plugin: (time of last render, result, its refresh_ms).
    /// Lets a minute-level plugin skip the 500ms render calls that a fast
    /// plugin in the same combined view would otherwise force on it.
    pub render_cache: tokio::sync::Mutex<HashMap<String, (Instant, RenderResult, u64)>>,
}

pub type Shared = Arc<AppState>;

/// Re-scan plugin directories at most once every 2 seconds. New plugins
/// (or removed ones) take effect without restarting the host.
async fn refresh_plugins(state: &Shared) {
    let mut last = state.last_scan.lock().await;
    if last.elapsed() < std::time::Duration::from_secs(2) {
        return;
    }
    *last = std::time::Instant::now();
    drop(last);

    let dirs = state.plugin_dirs.clone();
    let found = match tokio::task::spawn_blocking(move || crate::plugin::scan(&dirs)).await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "plugin directory scan failed");
            return;
        }
    };
    let retired = {
        let mut set = state.plugins.write().await;
        let changed = set.reconcile(found);
        if changed {
            tracing::info!("plugin list changed: {}", set.order.join(", "));
        }
        std::mem::take(&mut set.pending_shutdown)
    };
    // Shut down replaced/removed processes after releasing the write lock.
    for p in retired {
        p.shutdown().await;
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct FrameQuery {
    /// Comma-separated plugin ids; empty means "use defaults".
    pub plugins: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Serialize)]
struct FrameResponse {
    html: String,
    signature: String,
    refresh_ms: u64,
    active: Vec<String>,
    mode: String,
    invert: bool,
}

#[derive(Serialize)]
struct PluginSummary {
    id: String,
    name: String,
    version: String,
    description: String,
    enabled: bool,
    default_on: bool,
    running: bool,
}

/// Resolve the plugin ids to render. Control-side selection wins when set;
/// otherwise fall back to the URL parameter, then to each plugin's default_on.
/// Unknown and disabled ones are dropped.
fn resolve_ids(set: &PluginSet, query: &Option<String>, active: &Option<Vec<String>>) -> Vec<String> {
    let requested: Option<Vec<String>> = if let Some(ids) = active {
        Some(ids.clone())
    } else {
        query.as_ref().and_then(|raw| {
            if raw.len() > MAX_QUERY_BYTES {
                warn!(len = raw.len(), "plugins query too long, using defaults");
                return None;
            }
            let mut seen = HashSet::new();
            let ids: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter(|s| seen.insert((*s).to_string()))
                .take(MAX_PLUGIN_IDS)
                .map(|s| s.to_string())
                .collect();
            if ids.is_empty() { None } else { Some(ids) }
        })
    };
    let ids = requested.unwrap_or_else(|| set.default_ids());
    ids.into_iter()
        .filter(|id| {
            set.get(id)
                .map(|p| p.manifest.runtime.enabled)
                .unwrap_or(false)
        })
        .collect()
}

async fn get_frame(State(state): State<Shared>, Query(q): Query<FrameQuery>) -> impl IntoResponse {
    refresh_plugins(&state).await;
    let params = RenderParams {
        width: q.width.unwrap_or(0),
        height: q.height.unwrap_or(0),
        theme: "ink".to_string(),
    };
    let fallback_ms = state.config.display.refresh_ms;

    // 先快照选中的插件，再释放读锁，之后才做子进程 IO；慢插件不会阻塞
    // 目录重扫。
    let (ids, selected) = {
        let guard = state.plugins.read().await;
        let active = state.active.read().await.clone();
        let ids = resolve_ids(&guard, &q.plugins, &active);
        let selected: Vec<(String, Arc<crate::plugin::PluginProcess>)> = ids
            .iter()
            .filter_map(|id| guard.get(id).map(|p| (id.clone(), p)))
            .collect();
        (ids, selected)
    };

    // 判断哪些插件需要重新渲染。每个插件用自己的刷新间隔：和歌词组合显示时，
    // 30 秒刷新的时钟不会每 500 毫秒重渲染一次。
    let (to_render, mut resolved) = {
        let cache = state.render_cache.lock().await;
        let mut to_render = Vec::new();
        let mut resolved: HashMap<String, RenderResult> = HashMap::new();
        let now = Instant::now();
        for (id, p) in &selected {
            let fresh = cache
                .get(id)
                .map(|(at, _, interval)| now.duration_since(*at) < Duration::from_millis(*interval))
                .unwrap_or(false);
            if fresh {
                if let Some((_, result, _)) = cache.get(id) {
                    resolved.insert(id.clone(), result.clone());
                }
            } else {
                to_render.push((id.clone(), Arc::clone(p), params.clone()));
            }
        }
        (to_render, resolved)
    };

    let mut futures = Vec::new();
    for (id, p, params) in to_render {
        futures.push(async move {
            let result = p.render(&params).await;
            (id, result)
        });
    }
    let results = futures::future::join_all(futures).await;

    // 失败隔离。插件本次渲染失败时，继续显示它的最后一帧有效内容。
    let mut stale_fallback: HashMap<String, RenderResult> = {
        let cache = state.render_cache.lock().await;
        cache
            .iter()
            .map(|(id, (_, r, _))| (id.clone(), r.clone()))
            .collect()
    };
    let mut updates = Vec::new();
    for (id, result) in results {
        match result {
            Ok(r) => {
                let interval = r.refresh_ms.unwrap_or(fallback_ms).clamp(250, 30_000);
                resolved.insert(id.clone(), r.clone());
                updates.push((id, r, interval));
            }
            Err(e) => {
                warn!(plugin = %id, error = %e, "render failed, using last frame if available");
                if let Some(r) = stale_fallback.remove(&id) {
                    resolved.insert(id, r);
                }
            }
        }
    }
    if !updates.is_empty() {
        let mut cache = state.render_cache.lock().await;
        for (id, r, interval) in updates {
            cache.insert(id, (Instant::now(), r, interval));
        }
    }

    // Preserve the order requested in the URL.
    let mut parts = Vec::new();
    for id in &ids {
        if let Some(r) = resolved.remove(id) {
            parts.push((id.clone(), r));
        }
    }

    let frame = render_frame(&parts, fallback_ms);
    debug!(active = ?frame.active, refresh_ms = frame.refresh_ms, "frame built");
    let display = state.display.read().await.clone();
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(FrameResponse {
            html: frame.html,
            signature: frame.signature,
            refresh_ms: frame.refresh_ms,
            active: frame.active,
            mode: display.mode,
            invert: display.invert,
        }),
    )
}

/// 控制端推送显示设置。mode 只接受 gray/bw/text，写入后下一次取帧即生效。
async fn set_display(
    State(state): State<Shared>,
    Json(update): Json<DisplayState>,
) -> impl IntoResponse {
    let mode = if update.mode == "bw" || update.mode == "text" {
        update.mode
    } else {
        "gray".to_string()
    };
    let mut guard = state.display.write().await;
    *guard = DisplayState { mode, invert: update.invert };
    debug!(mode = %guard.mode, invert = guard.invert, "display settings updated");
    ([(header::CACHE_CONTROL, "no-store")], StatusCode::OK)
}

async fn get_plugins(State(state): State<Shared>) -> impl IntoResponse {
    refresh_plugins(&state).await;
    let active = state.active.read().await.clone();
    // Clone the handles and drop the read lock before the child-process
    // round trips; otherwise a slow describe blocks every rescan and frame.
    let entries: Vec<(String, Arc<crate::plugin::PluginProcess>)> = {
        let guard = state.plugins.read().await;
        guard
            .order
            .iter()
            .filter_map(|id| guard.get(id).map(|p| (id.clone(), p)))
            .collect()
    };
    let mut futures = Vec::new();
    for (id, p) in entries {
        futures.push(async move {
            let info = p.describe().await;
            (id, p, info)
        });
    }
    let results = futures::future::join_all(futures).await;
    let mut out = Vec::new();
    for (id, p, info) in results {
        let m = &p.manifest;
        let (running, i) = match info {
            Ok(v) => (true, Some(v)),
            Err(e) => {
                debug!(plugin = %id, error = %e, "describe failed");
                (false, None)
            }
        };
        let enabled = active
            .as_ref()
            .map(|ids| ids.contains(&id))
            .unwrap_or(m.runtime.default_on);
        out.push(PluginSummary {
            id,
            enabled,
            name: i
                .as_ref()
                .map(|v| v.name.clone())
                .unwrap_or_else(|| m.plugin.name.clone()),
            version: i
                .as_ref()
                .map(|v| v.version.clone())
                .unwrap_or_else(|| m.plugin.version.clone()),
            description: i
                .as_ref()
                .map(|v| v.description.clone())
                .unwrap_or_else(|| m.plugin.description.clone()),
            default_on: m.runtime.default_on,
            running,
        });
    }
    ([(header::CACHE_CONTROL, "no-store")], Json(out))
}

async fn get_health(State(state): State<Shared>) -> impl IntoResponse {
    let count = state.plugins.read().await.order.len();
    let body = serde_json::json!({
        "status": "ok",
        "plugins": count,
        "protocol": inkbuddy_protocol::PROTOCOL_VERSION,
    });
    ([(header::CACHE_CONTROL, "no-store")], Json(body))
}

/// The E-Ink entry point. Both `/` and `/ink` land here; the device only needs
/// the server address, with no extra path.
async fn get_ink(State(state): State<Shared>) -> Response {
    let page = INK_PAGE.replace("__TITLE__", &html_escape(&state.config.display.title));
    (
        [(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")],
        Html(page),
    )
        .into_response()
}

/// 控制端读取/设置启用插件。写入后立即生效，并持久化到 active.json。
async fn get_active(State(state): State<Shared>) -> impl IntoResponse {
    let ids = state.active.read().await.clone();
    ([(header::CACHE_CONTROL, "no-store")], Json(ActiveState { ids }))
}

#[derive(Deserialize)]
struct ActiveUpdate {
    ids: Vec<String>,
}

async fn set_active(
    State(state): State<Shared>,
    Json(update): Json<ActiveUpdate>,
) -> impl IntoResponse {
    let mut ids = update.ids;
    if ids.len() > MAX_PLUGIN_IDS {
        ids.truncate(MAX_PLUGIN_IDS);
    }
    *state.active.write().await = Some(ids.clone());
    save_active(&Some(ids.clone()));
    debug!(ids = ?ids, "active plugins updated");
    ([(header::CACHE_CONTROL, "no-store")], StatusCode::OK)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [(header::CACHE_CONTROL, "no-store")],
        "not found",
    )
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(get_ink))
        .route("/ink", get(get_ink))
        .route("/api/frame", get(get_frame))
        .route("/api/display", axum::routing::post(set_display))
        .route("/api/plugins", get(get_plugins))
        .route("/api/plugins/active", get(get_active).post(set_active))
        .route("/api/health", get(get_health))
        .fallback(not_found)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkbuddy_protocol::{Manifest, PluginManifest, RuntimeManifest};
    use std::path::PathBuf;

    fn set_with(ids: &[&str], default_on: bool) -> PluginSet {
        let mut set = PluginSet {
            plugins: HashMap::new(),
            order: Vec::new(),
            pending_shutdown: Vec::new(),
        };
        found_for(&mut set, ids, default_on);
        set
    }

    fn found_for(set: &mut PluginSet, ids: &[&str], default_on: bool) {
        let found: Vec<_> = ids
            .iter()
            .map(|id| {
                let m = Manifest {
                    plugin: PluginManifest {
                        id: (*id).to_string(),
                        name: (*id).to_string(),
                        version: "0.1.0".to_string(),
                        description: String::new(),
                        command: "./plugin".to_string(),
                    },
                    runtime: RuntimeManifest {
                        enabled: true,
                        default_on,
                        ..RuntimeManifest::default()
                    },
                };
                (m, PathBuf::from("/tmp/inkbuddy-review-test").join(id))
            })
            .collect();
        set.reconcile(found);
    }

    #[test]
    fn empty_query_falls_back_to_defaults() {
        // Both plugins are default_on, so the default set has both entries.
        // What matters is that an explicitly empty query resolves the same
        // way as no query at all, so the page stays populated.
        let set = set_with(&["lyrics", "clock"], true);
        let expected = vec!["lyrics".to_string(), "clock".to_string()];
        assert_eq!(resolve_ids(&set, &None, &None), expected);
        assert_eq!(resolve_ids(&set, &Some(String::new()), &None), expected);
        assert_eq!(resolve_ids(&set, &Some(",, ,".to_string()), &None), expected);
    }

    #[test]
    fn duplicate_ids_are_deduplicated_in_order() {
        let set = set_with(&["lyrics", "clock"], false);
        let got = resolve_ids(&set, &Some("clock,lyrics,clock".to_string()), &None);
        assert_eq!(got, vec!["clock", "lyrics"]);
    }

    #[test]
    fn unknown_ids_are_ignored_and_long_query_is_capped() {
        let set = set_with(&["lyrics"], true);
        assert!(resolve_ids(&set, &Some("ghost".to_string()), &None).is_empty());
        let long = "lyrics,".repeat(2000);
        assert_eq!(resolve_ids(&set, &Some(long), &None), vec!["lyrics"]);
    }

    #[test]
    fn active_selection_overrides_query() {
        let set = set_with(&["lyrics", "clock"], true);
        let active = Some(vec!["clock".to_string()]);
        // 控制端只启用 clock，即便 URL 请求两个也应只渲染 clock
        let got = resolve_ids(&set, &Some("lyrics,clock".to_string()), &active);
        assert_eq!(got, vec!["clock"]);
        // 控制端清空列表表示全部关闭
        let got = resolve_ids(&set, &Some("lyrics".to_string()), &Some(vec![]));
        assert!(got.is_empty());
    }

    #[test]
    fn title_injection_is_escaped() {
        let escaped = html_escape("</title><script>alert(1)</script>");
        assert!(!escaped.contains("<script>"));
        assert!(escaped.contains("&lt;/title&gt;"));
    }
}
