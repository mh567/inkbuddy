use crate::config::Config;
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

pub struct AppState {
    pub config: Config,
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

/// Resolve the requested plugin ids; unknown and disabled ones are dropped.
/// An explicitly empty value (`?plugins=` or `?plugins=,,`) uses the default set.
fn resolve_ids(set: &PluginSet, query: &Option<String>) -> Vec<String> {
    let requested: Option<Vec<String>> = query.as_ref().and_then(|raw| {
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
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    });
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
        let ids = resolve_ids(&guard, &q.plugins);
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
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(FrameResponse {
            html: frame.html,
            signature: frame.signature,
            refresh_ms: frame.refresh_ms,
            active: frame.active,
        }),
    )
}

async fn get_plugins(State(state): State<Shared>) -> impl IntoResponse {
    refresh_plugins(&state).await;
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
        out.push(PluginSummary {
            id,
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
            enabled: m.runtime.enabled,
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

/// A tiny control page for choosing which plugins (and in what order) to show.
async fn get_manage(State(state): State<Shared>) -> Response {
    refresh_plugins(&state).await;
    let guard = state.plugins.read().await;
    let mut rows = String::new();
    for id in &guard.order {
        let Some(p) = guard.get(id) else { continue };
        let checked = if p.manifest.runtime.default_on {
            " checked"
        } else {
            ""
        };
        rows.push_str(&format!(
            "<label><input type=checkbox value=\"{}\"{}><b>{}</b> <span>{}</span></label>",
            html_escape(id),
            checked,
            html_escape(&p.manifest.plugin.name),
            html_escape(&p.manifest.plugin.description),
        ));
    }
    let page = format!(
        r#"<!DOCTYPE html><html lang=zh-CN><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>InkBuddy 管理</title>
<style>
body{{margin:0;padding:32px;background:#fafafa;color:#1a1a1a;font-family:-apple-system,BlinkMacSystemFont,"PingFang SC",sans-serif;max-width:680px}}
h1{{font-size:22px;margin:0 0 4px}} p.hint{{color:#666;font-size:13px;margin:0 0 24px}}
label{{display:flex;gap:10px;align-items:baseline;padding:12px 14px;margin-bottom:8px;background:#fff;border:1px solid #e5e5e5;border-radius:8px;font-size:14px}}
label span{{color:#888;font-size:12px}}
input[type=checkbox]{{width:16px;height:16px}}
#url{{margin-top:20px;padding:14px;background:#fff;border:1px solid #e5e5e5;border-radius:8px;word-break:break-all;font-family:ui-monospace,monospace;font-size:13px}}
a{{color:#0066cc}}
</style></head><body>
<h1>InkBuddy</h1>
<p class=hint>勾选要显示的内容，顺序即页面从上到下的顺序。把生成的地址输入墨水屏浏览器即可。</p>
<div id=list>{rows}</div>
<div id=url></div>
<script>
function update(){{
  var ids=[].slice.call(document.querySelectorAll('input:checked')).map(function(x){{return x.value}});
  var base=location.origin+'/';
  document.getElementById('url').innerHTML='地址：<a href="'+(ids.length?base+'?plugins='+encodeURIComponent(ids.join(',')):base)+'">'+(ids.length?base+'?plugins='+encodeURIComponent(ids.join(',')):base)+'</a>';
}}
document.getElementById('list').addEventListener('change',update); update();
</script></body></html>"#
    );
    ([(header::CACHE_CONTROL, "no-store")], Html(page)).into_response()
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
        .route("/manage", get(get_manage))
        .route("/api/frame", get(get_frame))
        .route("/api/plugins", get(get_plugins))
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
        assert_eq!(resolve_ids(&set, &None), expected);
        assert_eq!(resolve_ids(&set, &Some(String::new())), expected);
        assert_eq!(resolve_ids(&set, &Some(",, ,".to_string())), expected);
    }

    #[test]
    fn duplicate_ids_are_deduplicated_in_order() {
        let set = set_with(&["lyrics", "clock"], false);
        let got = resolve_ids(&set, &Some("clock,lyrics,clock".to_string()));
        assert_eq!(got, vec!["clock", "lyrics"]);
    }

    #[test]
    fn unknown_ids_are_ignored_and_long_query_is_capped() {
        let set = set_with(&["lyrics"], true);
        assert!(resolve_ids(&set, &Some("ghost".to_string())).is_empty());
        let long = "lyrics,".repeat(2000);
        assert_eq!(resolve_ids(&set, &Some(long)), vec!["lyrics"]);
    }

    #[test]
    fn title_injection_is_escaped() {
        let escaped = html_escape("</title><script>alert(1)</script>");
        assert!(!escaped.contains("<script>"));
        assert!(escaped.contains("&lt;/title&gt;"));
    }
}
