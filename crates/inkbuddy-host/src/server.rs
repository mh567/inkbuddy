use crate::config::{self, Config};
use crate::plugin::PluginSet;
use crate::render::{preview_content, render_frame_with_mode, FrameRegion, PreviewBlock, PreviewLine};
use crate::views::{self, Region, ViewState};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use inkbuddy_protocol::{RenderParams, RenderResult};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Upper bound on the `?plugins=` value, so the host allocates at most
/// MAX_PLUGIN_IDS strings for a query.
const MAX_QUERY_BYTES: usize = 4096;
const MAX_PLUGIN_IDS: usize = 64;

const INK_PAGE: &str = include_str!("../../../web/ink.html");

/// 页面版本号：页面内容哈希。墨水屏页面每次轮询都会带上它对比，
/// host 升级换了页面后，旧页面自动刷新一次，不需要人工干预。
fn page_version() -> &'static str {
    use std::sync::OnceLock;
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(INK_PAGE, &mut hasher);
        format!("{:016x}", std::hash::Hasher::finish(&hasher))
    })
}

/// 可由控制端在运行时修改的显示设置，墨水屏每次取帧都会读到最新值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayState {
    pub mode: String,
    pub invert: bool,
}

/// 卸载标记的持久化形式。目录扫描到 = 安装来源；卸载只是本地黑名单。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct UninstalledState {
    ids: Vec<String>,
}

fn uninstalled_path() -> Option<std::path::PathBuf> {
    config::home_dir().ok().map(|p| p.join("uninstalled.json"))
}

pub fn load_uninstalled() -> Vec<String> {
    let Some(path) = uninstalled_path() else {
        return Vec::new();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<UninstalledState>(&text) {
        Ok(state) => state.ids,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "invalid uninstalled.json, ignoring");
            Vec::new()
        }
    }
}

fn save_uninstalled(ids: &[String]) {
    let Some(path) = uninstalled_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&UninstalledState {
        ids: ids.to_vec(),
    }) {
        // 临时文件 + rename，防止半截 JSON。
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub struct AppState {
    pub config: Config,
    pub display: tokio::sync::RwLock<DisplayState>,
    /// 卸载的扩展 id 列表。显示与可组合集合都排除这些扩展。
    pub uninstalled: tokio::sync::RwLock<Vec<String>>,
    /// 具名视图（区域布局）+ 当前应用的视图指针。显示完全跟随当前视图。
    pub views: tokio::sync::RwLock<ViewState>,
    pub plugins: tokio::sync::RwLock<PluginSet>,
    pub plugin_dirs: Vec<std::path::PathBuf>,
    /// Throttles directory rescans so 500ms polling stays cheap.
    pub last_scan: tokio::sync::Mutex<Instant>,
    /// Last frame per plugin: (time of last render, result, its refresh_ms).
    /// Lets a minute-level plugin skip the 500ms render calls that a fast
    /// plugin in the same combined view would otherwise force on it.
    pub render_cache: tokio::sync::Mutex<HashMap<String, (Instant, RenderResult, u64)>>,
    /// 设备最近一次上报的视口宽高比（f32 bits；0 = 未知）。墨水屏页面每次
    /// 轮询都会带 vw/vh，控制端的编辑器/预览用它对齐设备真实比例。
    pub device_aspect: std::sync::atomic::AtomicU32,
    /// 上报时刻的 Unix 秒。超过时效的上报不再暴露给控制端，
    /// 防止设备离线后陈旧比例误导编辑器。
    pub device_aspect_at: std::sync::atomic::AtomicU64,
}

pub type Shared = Arc<AppState>;

/// 扩展是否可用：被扫描到、自身声明 enabled、且未被用户卸载。
fn is_installed(set: &PluginSet, uninstalled: &[String], id: &str) -> bool {
    set.get(id)
        .map(|p| p.manifest.runtime.enabled)
        .unwrap_or(false)
        && !uninstalled.iter().any(|x| x == id)
}

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
            info!("plugin list changed: {}", set.order.join(", "));
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
    /// 页面视口尺寸：字号(vw)换算到区域高度(vh)需要宽高比。
    /// 缺失时按正方形屏幕估算。
    pub vw: Option<f32>,
    pub vh: Option<f32>,
}

#[derive(Serialize)]
struct FrameResponse {
    html: String,
    signature: String,
    refresh_ms: u64,
    active: Vec<String>,
    mode: String,
    invert: bool,
    /// 是否按区域布局渲染。
    layout: bool,
    /// 每个已渲染扩展的屏幕区域与文本行，供控制端原生渲染（预览/画布同源）。
    regions: Vec<FrameRegion>,
    /// 页面版本号：与浏览器已加载页面的版本不一致时页面自动刷新。
    page_version: String,
}

#[derive(Serialize)]
struct PluginSummary {
    id: String,
    name: String,
    version: String,
    description: String,
    installed: bool,
    default_on: bool,
    running: bool,
}

/// Resolve the plugin ids for the no-view path: the URL parameter wins when
/// set, otherwise each installed plugin's default_on. Unknown and uninstalled
/// ones are dropped.
fn resolve_ids(set: &PluginSet, query: &Option<String>, uninstalled: &[String]) -> Vec<String> {
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
        if ids.is_empty() { None } else { Some(ids) }
    });
    let ids = requested.unwrap_or_else(|| set.default_ids());
    ids.into_iter()
        .filter(|id| is_installed(set, uninstalled, id))
        .collect()
}

#[derive(Deserialize)]
struct PreviewRequest {
    plugins: Vec<String>,
}

#[derive(Serialize)]
struct PreviewContent {
    plugin: String,
    lines: Vec<PreviewLine>,
    /// 按块分组的内容：块内行紧挨着，块间留间距，控制端据此复刻页面排版。
    blocks: Vec<PreviewBlock>,
    /// 内容按基准字号需要的高度（正方形屏幕下的 vh）。客户端用它
    /// 结合自己的画布宽高比算 fit 缩放，与墨水屏的公式一致。
    #[serde(rename = "contentUnits")]
    content_units: f32,
    /// 服务端当前是否为纯文本模式（隐藏拼音行、标题加大加粗），
    /// 控制端预览据此过滤内容，与墨水屏一致。
    #[serde(rename = "textMode")]
    text_mode: bool,
}

/// 编辑器实时预览：接收当前正在编辑（尚未保存）的区域里的扩展 id，
/// 返回每个扩展当前内容的结构化文本行。不读视图状态、不写盘，
/// 不影响墨水屏显示。几何信息客户端自己有，这里只回内容。
async fn preview_frame(State(state): State<Shared>, Json(req): Json<PreviewRequest>) -> Response {
    refresh_plugins(&state).await;
    let text_mode = state.display.read().await.mode == "text";
    let params = RenderParams {
        width: 0,
        height: 0,
        theme: "ink".to_string(),
    };
    let selected: Vec<(String, Arc<crate::plugin::PluginProcess>)> = {
        let guard = state.plugins.read().await;
        let uninstalled = state.uninstalled.read().await;
        let mut seen = HashSet::new();
        req.plugins
            .iter()
            .filter(|id| seen.insert((*id).clone()))
            .filter(|id| is_installed(&guard, &uninstalled, id))
            .filter_map(|id| guard.get(id).map(|p| (id.clone(), Arc::clone(&p))))
            .take(8)
            .collect()
    };
    let mut futures = Vec::new();
    for (id, p) in selected {
        let params = params.clone();
        futures.push(async move { (id, p.render(&params).await) });
    }
    let results = futures::future::join_all(futures).await;
    let out: Vec<PreviewContent> = results
        .into_iter()
        .filter_map(|(id, result)| {
            result.ok().map(|r| {
                let preview = preview_content(&r, text_mode);
                PreviewContent {
                    plugin: id,
                    lines: preview.lines,
                    blocks: preview.blocks,
                    content_units: preview.content_units,
                    text_mode,
                }
            })
        })
        .collect();
    ([(header::CACHE_CONTROL, "no-store")], Json(out)).into_response()
}

async fn get_frame(State(state): State<Shared>, Query(q): Query<FrameQuery>) -> impl IntoResponse {
    refresh_plugins(&state).await;
    let fallback_ms = state.config.display.refresh_ms;
    let params = RenderParams {
        width: q.width.unwrap_or(0),
        height: q.height.unwrap_or(0),
        theme: "ink".to_string(),
    };

    // 先快照布局与插件句柄，再释放读锁，之后才做子进程 IO；慢插件不会阻塞
    // 目录重扫。
    let (layout, ids, selected) = {
        let guard = state.plugins.read().await;
        let uninstalled = state.uninstalled.read().await;
        let views_state = state.views.read().await;
        let layout: Option<Vec<Region>> = views_state.current_regions();
        // 当前视图存在时按区域顺序渲染；否则走 URL 参数 / default_on 流式。
        let ids: Vec<String> = match &layout {
            Some(regions) => {
                let mut seen = HashSet::new();
                regions
                    .iter()
                    .filter(|r| seen.insert(r.plugin.clone()))
                    .map(|r| r.plugin.clone())
                    .filter(|id| is_installed(&guard, &uninstalled, id))
                    .collect()
            }
            None => resolve_ids(&guard, &q.plugins, &uninstalled),
        };
        let selected: Vec<(String, Arc<crate::plugin::PluginProcess>)> = ids
            .iter()
            .filter_map(|id| guard.get(id).map(|p| (id.clone(), p)))
            .collect();
        (layout, ids, selected)
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

    // Preserve the requested order: view regions first, then URL/default order.
    let mut parts = Vec::new();
    for id in &ids {
        if let Some(r) = resolved.remove(id) {
            parts.push((id.clone(), r));
        }
    }

    let display = state.display.read().await.clone();
    // 宽高比 = 视口宽/高。墨水屏页面每次轮询都会带上；拿不到时按正方形估算。
    let aspect = match (q.vw, q.vh) {
        (Some(w), Some(h)) if w > 1.0 && h > 1.0 => {
            let a = (w / h).clamp(0.3, 4.0);
            state
                .device_aspect
                .store(a.to_bits(), std::sync::atomic::Ordering::Relaxed);
            state.device_aspect_at.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                std::sync::atomic::Ordering::Relaxed,
            );
            a
        }
        _ => 1.0,
    };
    let frame = render_frame_with_mode(
        &parts,
        fallback_ms,
        layout.as_deref(),
        display.mode == "text",
        aspect,
    );
    debug!(active = ?frame.active, refresh_ms = frame.refresh_ms, "frame built");
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(FrameResponse {
            html: frame.html,
            signature: frame.signature,
            refresh_ms: frame.refresh_ms,
            active: frame.active,
            mode: display.mode,
            invert: display.invert,
            layout: frame.layout,
            regions: frame.regions,
            page_version: page_version().to_string(),
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
    // Clone the handles and drop the read lock before the child-process
    // round trips; otherwise a slow describe blocks every rescan and frame.
    let (entries, uninstalled) = {
        let guard = state.plugins.read().await;
        let uninstalled = state.uninstalled.read().await.clone();
        let entries: Vec<(String, Arc<crate::plugin::PluginProcess>)> = guard
            .order
            .iter()
            .filter_map(|id| guard.get(id).map(|p| (id.clone(), p)))
            .collect();
        (entries, uninstalled)
    };
    let mut futures = Vec::new();
    let mut offline: Vec<(String, Arc<crate::plugin::PluginProcess>)> = Vec::new();
    for (id, p) in entries {
        // 未安装的扩展不 describe：懒启动会为一个不可用的扩展拉起子进程。
        if uninstalled.iter().any(|x| x == &id) {
            offline.push((id, p));
        } else {
            futures.push(async move {
                let info = p.describe().await;
                (id, p, info)
            });
        }
    }
    let results = futures::future::join_all(futures).await;
    let mut out = Vec::new();
    for (id, p, info) in results {
        let m = &p.manifest;
        let installed = !uninstalled.iter().any(|x| x == &id);
        let (running, i) = match info {
            Ok(v) => (true, Some(v)),
            Err(e) => {
                debug!(plugin = %id, error = %e, "describe failed");
                (false, None)
            }
        };
        out.push(PluginSummary {
            id,
            installed,
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
    for (id, p) in offline {
        let m = &p.manifest;
        out.push(PluginSummary {
            id,
            installed: false,
            name: m.plugin.name.clone(),
            version: m.plugin.version.clone(),
            description: m.plugin.description.clone(),
            default_on: m.runtime.default_on,
            running: false,
        });
    }
    ([(header::CACHE_CONTROL, "no-store")], Json(out))
}

#[derive(Deserialize)]
struct InstalledUpdate {
    installed: bool,
}

/// 安装/卸载一个扩展。卸载会把它的子进程停掉；安装只是移除黑名单标记。
async fn set_installed(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(update): Json<InstalledUpdate>,
) -> Response {
    let id = id.trim().to_string();
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-store")],
            "empty id",
        )
            .into_response();
    }
    let mut list = state.uninstalled.write().await;
    let changed = if update.installed {
        let before = list.len();
        list.retain(|x| x != &id);
        before != list.len()
    } else if !list.contains(&id) {
        list.push(id.clone());
        true
    } else {
        false
    };
    if changed {
        save_uninstalled(&list);
    }
    drop(list);
    if !update.installed {
        // 卸载后停掉子进程；再安装时按需懒启动。
        let proc = state.plugins.read().await.get(&id);
        if let Some(p) = proc {
            p.shutdown().await;
        }
    }
    debug!(plugin = %id, installed = update.installed, "extension install state updated");
    StatusCode::OK.into_response()
}

async fn get_health(State(state): State<Shared>) -> impl IntoResponse {
    let count = state.plugins.read().await.order.len();
    // 设备比例只在 60 秒内上报过才暴露：设备离线后陈旧值不应
    // 继续影响编辑器的画布方向。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bits = state.device_aspect.load(std::sync::atomic::Ordering::Relaxed);
    let at = state.device_aspect_at.load(std::sync::atomic::Ordering::Relaxed);
    let aspect = if bits != 0 && at > 0 && now.saturating_sub(at) < 60 {
        Some(f32::from_bits(bits))
    } else {
        None
    };
    let body = serde_json::json!({
        "status": "ok",
        "plugins": count,
        "protocol": inkbuddy_protocol::PROTOCOL_VERSION,
        // 设备实测宽高比（宽/高）；设备还没轮询过时为 null。
        "aspect": aspect,
    });
    ([(header::CACHE_CONTROL, "no-store")], Json(body))
}

/// The E-Ink entry point. Both `/` and `/ink` land here; the device only needs
/// the server address, with no extra path.
async fn get_ink(State(state): State<Shared>) -> Response {
    let page = INK_PAGE
        .replace("__TITLE__", &html_escape(&state.config.display.title))
        .replace("__PAGE_VERSION__", page_version());
    (
        [(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")],
        Html(page),
    )
        .into_response()
}

async fn get_views(State(state): State<Shared>) -> impl IntoResponse {
    refresh_plugins(&state).await;
    let views_state = state.views.read().await.clone();
    ([(header::CACHE_CONTROL, "no-store")], Json(views_state))
}

#[derive(Deserialize)]
struct ViewsUpdate {
    views: Vec<views::View>,
}

/// 整体替换视图列表。current 指向不存在的视图时重置为无视图。
async fn set_views(
    State(state): State<Shared>,
    Json(update): Json<ViewsUpdate>,
) -> impl IntoResponse {
    let cleaned = views::sanitize(update.views);
    let mut views_state = state.views.write().await;
    if let Some(cur) = views_state.current.clone() {
        if !cleaned.iter().any(|v| v.id == cur) {
            views_state.current = None;
        }
    }
    views_state.views = cleaned;
    views::save(&views_state);
    // 保存是丢数据排查的关键路径：记录每个视图的 id 与区域摘要（y/h），
    // 与 views.json 的 mtime 对照即可定位「保存内容与预期不符」发生在哪一环。
    info!(
        views = ?views_state
            .views
            .iter()
            .map(|v| (
                v.id.as_str(),
                v.regions
                    .iter()
                    .map(|r| (r.plugin.as_str(), r.y, r.h))
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        "views updated"
    );
    ([(header::CACHE_CONTROL, "no-store")], StatusCode::OK)
}

#[derive(Deserialize)]
struct CurrentViewUpdate {
    id: Option<String>,
}

/// 应用一个视图：显示跟随该视图的区域布局，下一次取帧即生效。
/// id 为 null 表示取消视图，回到流式布局（URL 参数 / default_on）。
async fn set_current_view(
    State(state): State<Shared>,
    Json(update): Json<CurrentViewUpdate>,
) -> Response {
    let mut views_state = state.views.write().await;
    match update.id {
        Some(id) => {
            if views_state.view(&id).is_none() {
                return (
                    StatusCode::NOT_FOUND,
                    [(header::CACHE_CONTROL, "no-store")],
                    "no such view",
                )
                    .into_response();
            }
            views_state.current = Some(id.clone());
            info!(view = %id, "view applied");
        }
        None => {
            views_state.current = None;
            info!("current view cleared");
        }
    }
    views::save(&views_state);
    StatusCode::OK.into_response()
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
        .route("/api/preview", post(preview_frame))
        .route("/api/display", axum::routing::post(set_display))
        .route("/api/plugins", get(get_plugins))
        .route("/api/plugins/{id}/installed", post(set_installed))
        .route("/api/views", get(get_views).post(set_views))
        .route("/api/views/current", post(set_current_view))
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
        let set = set_with(&["lyrics", "clock"], true);
        let expected = vec!["lyrics".to_string(), "clock".to_string()];
        assert_eq!(resolve_ids(&set, &None, &[]), expected);
        assert_eq!(resolve_ids(&set, &Some(String::new()), &[]), expected);
        assert_eq!(resolve_ids(&set, &Some(",, ,".to_string()), &[]), expected);
    }

    #[test]
    fn duplicate_ids_are_deduplicated_in_order() {
        let set = set_with(&["lyrics", "clock"], false);
        let got = resolve_ids(&set, &Some("clock,lyrics,clock".to_string()), &[]);
        assert_eq!(got, vec!["clock", "lyrics"]);
    }

    #[test]
    fn unknown_ids_are_ignored_and_long_query_is_capped() {
        let set = set_with(&["lyrics"], true);
        assert!(resolve_ids(&set, &Some("ghost".to_string()), &[]).is_empty());
        let long = "lyrics,".repeat(2000);
        assert_eq!(resolve_ids(&set, &Some(long), &[]), vec!["lyrics"]);
    }

    #[test]
    fn uninstalled_plugins_are_filtered_everywhere() {
        let set = set_with(&["lyrics", "clock"], true);
        let un = vec!["clock".to_string()];
        // URL 请求两个，卸载的 clock 被过滤
        assert_eq!(
            resolve_ids(&set, &Some("lyrics,clock".to_string()), &un),
            vec!["lyrics"]
        );
        // default_on 回退同样过滤
        assert_eq!(resolve_ids(&set, &None, &un), vec!["lyrics"]);
    }

    #[test]
    fn title_injection_is_escaped() {
        let escaped = html_escape("</title><script>alert(1)</script>");
        assert!(!escaped.contains("<script>"));
        assert!(escaped.contains("&lt;/title&gt;"));
    }
}
