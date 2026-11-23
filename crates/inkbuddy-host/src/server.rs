use crate::config::Config;
use crate::plugin::PluginSet;
use crate::render::render_frame;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use inkbuddy_protocol::RenderParams;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

const INK_PAGE: &str = include_str!("../../../web/ink.html");

pub struct AppState {
    pub config: Config,
    pub plugins: tokio::sync::RwLock<PluginSet>,
    pub plugin_dirs: Vec<std::path::PathBuf>,
    /// Throttles directory rescans so 500ms polling stays cheap.
    pub last_scan: tokio::sync::Mutex<std::time::Instant>,
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

    let found = crate::plugin::scan(&state.plugin_dirs);
    let mut set = state.plugins.write().await;
    if set.reconcile(found) {
        tracing::info!("plugin list changed: {}", set.order.join(", "));
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

/// Resolve the requested plugin ids, ignoring unknown/disabled ones.
fn resolve_ids(set: &PluginSet, query: &Option<String>) -> Vec<String> {
    let requested: Option<Vec<String>> = query.as_ref().map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
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

async fn get_frame(
    State(state): State<Shared>,
    Query(q): Query<FrameQuery>,
) -> Json<FrameResponse> {
    refresh_plugins(&state).await;
    let guard = state.plugins.read().await;
    let ids = resolve_ids(&guard, &q.plugins);
    let params = RenderParams {
        width: q.width.unwrap_or(0),
        height: q.height.unwrap_or(0),
        theme: "ink".to_string(),
    };

    // Ask every selected plugin for its content. Failures are isolated: a
    // broken plugin contributes nothing instead of failing the page.
    let mut futures = Vec::new();
    for id in &ids {
        if let Some(p) = guard.get(id) {
            let params = params.clone();
            futures.push(async move {
                let result = p.render(&params).await;
                (id.clone(), result)
            });
        }
    }
    drop(guard);
    let results = futures::future::join_all(futures).await;

    // A failed plugin contributes nothing rather than an empty section, so a
    // broken optional plugin does not leave a blank band on the E-Ink page.
    let mut parts = Vec::new();
    for (id, result) in results {
        match result {
            Ok(r) => parts.push((id, r)),
            Err(e) => warn!(plugin = %id, error = %e, "render failed, section skipped"),
        }
    }

    let frame = render_frame(&parts, state.config.display.refresh_ms);
    debug!(active = ?frame.active, refresh_ms = frame.refresh_ms, "frame built");
    Json(FrameResponse {
        html: frame.html,
        signature: frame.signature,
        refresh_ms: frame.refresh_ms,
        active: frame.active,
    })
}

async fn get_plugins(State(state): State<Shared>) -> Json<Vec<PluginSummary>> {
    refresh_plugins(&state).await;
    let guard = state.plugins.read().await;
    let mut out = Vec::new();
    for id in &guard.order {
        let Some(p) = guard.get(id) else { continue };
        let m = &p.manifest;

        // Ask the plugin itself. A successful describe both proves the
        // process is alive and gives authoritative metadata (a plugin may
        // report a newer version than its manifest).
        let (running, info) = match p.describe().await {
            Ok(i) => (true, Some(i)),
            Err(e) => {
                debug!(plugin = %id, error = %e, "describe failed");
                (false, None)
            }
        };
        let i = info;
        out.push(PluginSummary {
            id: id.clone(),
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
    Json(out)
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

/// The E-Ink entry point. Both `/` and `/ink` land here; no path beyond the
/// server address is required on the device.
async fn get_ink(State(state): State<Shared>) -> Response {
    let page = INK_PAGE.replace("__TITLE__", &state.config.display.title);
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
    (StatusCode::NOT_FOUND, "not found")
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
