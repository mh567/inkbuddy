use crate::config::{self, Config};
use crate::plugin::PluginSet;
use crate::render::{preview_content, render_frame_with_mode, FrameRegion, PreviewBlock, PreviewLine};
use crate::views::{self, Region, ViewState};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use inkbuddy_protocol::{RenderParams, RenderResult, VariantInfo};
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
    /// manifest 里的运行时开关。默认布局要用它过滤，和 default_ids 一致。
    enabled: bool,
    default_on: bool,
    running: bool,
    /// 插件声明的子样式；控制端据此把时钟展开成可选样式。
    variants: Vec<VariantInfo>,
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

/// 预览请求的一项。老控制端只传字符串数组（没有 variant），用 untagged
/// 兼容；新控制端传 {"plugin":"clock","variant":"dot"}。
#[derive(Deserialize)]
#[serde(untagged)]
enum PreviewItem {
    Plain(String),
    WithVariant {
        plugin: String,
        #[serde(default)]
        variant: Option<String>,
    },
}

impl PreviewItem {
    fn plugin(&self) -> &str {
        match self {
            PreviewItem::Plain(p) => p,
            PreviewItem::WithVariant { plugin, .. } => plugin,
        }
    }

    fn variant(&self) -> Option<&str> {
        match self {
            PreviewItem::Plain(_) => None,
            PreviewItem::WithVariant { variant, .. } => variant.as_deref(),
        }
    }
}

#[derive(Deserialize)]
struct PreviewRequest {
    plugins: Vec<PreviewItem>,
}

/// 预览内容的 key：有 variant 时拼成 "plugin:variant"，让同一插件的
/// 不同子样式在编辑器里各占一份缓存，互不覆盖。
fn preview_key(plugin: &str, variant: Option<&str>) -> String {
    match variant {
        Some(v) if !v.is_empty() => format!("{plugin}:{v}"),
        _ => plugin.to_string(),
    }
}

/// 控制端预览用的请求：传当前正在编辑（尚未保存）的区域布局和画布宽高比。
/// host 用与墨水屏完全相同的代码生成页面，控制端只需在 WebKit 里显示。
#[derive(Deserialize)]
struct PreviewHtmlRequest {
    regions: Vec<Region>,
    #[serde(default = "default_preview_aspect")]
    aspect: f32,
}

fn default_preview_aspect() -> f32 {
    1.0
}

#[derive(Serialize)]
struct PreviewHtmlResponse {
    /// 完整独立 HTML 文档，样式与墨水屏页面同源，可直接喂给 WebView。
    html: String,
    /// 文档签名，内容不变时控制端跳过重新加载，避免预览闪烁。
    signature: String,
}

/// 把取帧得到的 HTML 包成独立文档。直接复用设备页面的 head 与 body 标记，
/// 只做四处改动：注入帧内容、写死显示模式的 body class、加上 preview class
/// （画布按整页高度算，底部条不占位）、去掉轮询脚本（预览内容由控制端
/// 每次取数提供）。区域位置、字号缩放与设备按同一套公式算；预览画布不含
/// 底部条，纵向可用的那几十像素比设备多一点，其余一致。
fn preview_document(frame_html: &str, mode: &str, invert: bool) -> String {
    let cut = INK_PAGE.find("<script>").unwrap_or(INK_PAGE.len());
    let mut doc = INK_PAGE[..cut].to_string();
    let classes = format!(
        " class=\"layout-region preview mode-{} {}\"",
        mode,
        if invert { "invert" } else { "" }
    );
    doc = doc.replacen("<body>", &format!("<body{classes}>"), 1);
    doc = doc.replacen(
        "<div id=\"frame\">载入中…</div>",
        &format!("<div id=\"frame\">{frame_html}</div>"),
        1,
    );
    doc.push_str("</body></html>");
    doc
}

/// 编辑器实时预览：按未保存的区域布局渲染一帧，返回完整 HTML 文档。
/// 与 `/api/frame` 共用 `render_frame_with_mode`，所以点阵、台历这类
/// HTML 块样式在预览里和设备上是同一份内容和同一份样式。
async fn preview_html(
    State(state): State<Shared>,
    Json(req): Json<PreviewHtmlRequest>,
) -> Response {
    refresh_plugins(&state).await;
    let display = state.display.read().await.clone();
    let fallback_ms = state.config.display.refresh_ms;

    let regions: Vec<Region> = req
        .regions
        .iter()
        .take(MAX_PLUGIN_IDS)
        .map(Region::sanitized)
        .collect();

    // 与 /api/frame 一致：同一插件只占一个区域，取第一个区域的 variant。
    let mut seen = HashSet::new();
    let selected: Vec<(String, Option<String>, Arc<crate::plugin::PluginProcess>)> = {
        let guard = state.plugins.read().await;
        let uninstalled = state.uninstalled.read().await;
        regions
            .iter()
            .filter(|r| seen.insert(r.plugin.clone()))
            .filter(|r| is_installed(&guard, &uninstalled, &r.plugin))
            .filter_map(|r| {
                guard
                    .get(&r.plugin)
                    .map(|p| (r.plugin.clone(), r.variant.clone(), Arc::clone(&p)))
            })
            .collect()
    };

    let mut futures = Vec::new();
    for (id, variant, p) in selected {
        let params = RenderParams {
            width: 0,
            height: 0,
            theme: "ink".to_string(),
            variant,
        };
        futures.push(async move { (id, p.render(&params).await) });
    }
    let results = futures::future::join_all(futures).await;
    let parts: Vec<(String, RenderResult)> = results
        .into_iter()
        .filter_map(|(id, result)| result.ok().map(|r| (id, r)))
        .collect();

    let frame = render_frame_with_mode(
        &parts,
        fallback_ms,
        Some(&regions),
        display.mode == "text",
        req.aspect.clamp(0.3, 4.0),
    );
    let doc = preview_document(&frame.html, &display.mode, display.invert);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&doc, &mut hasher);
    let signature = format!("{:016x}", std::hash::Hasher::finish(&hasher));
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(PreviewHtmlResponse {
            html: doc,
            signature,
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct PreviewContent {
    plugin: String,
    lines: Vec<PreviewLine>,
    /// 按块分组的内容：块内行紧挨着，块间留间距，控制端据此复刻页面排版。
    blocks: Vec<PreviewBlock>,
    /// 内容按基准字号需要的高度（正方形屏幕下的 vh）。客户端用它
    /// 结合自己的画布宽高比算 fit 缩放，与墨水屏的公式一致——可用高度
    /// 同样要先扣掉区域上下留白。
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
    let selected: Vec<(String, Option<String>, Arc<crate::plugin::PluginProcess>)> = {
        let guard = state.plugins.read().await;
        let uninstalled = state.uninstalled.read().await;
        let mut seen = HashSet::new();
        req.plugins
            .iter()
            .filter(|item| seen.insert(preview_key(item.plugin(), item.variant())))
            .filter(|item| is_installed(&guard, &uninstalled, item.plugin()))
            .filter_map(|item| {
                guard.get(item.plugin()).map(|p| {
                    (
                        preview_key(item.plugin(), item.variant()),
                        item.variant().map(str::to_string),
                        Arc::clone(&p),
                    )
                })
            })
            // 与 MAX_REGIONS_PER_VIEW / /api/frame 一致：一个视图最多 64 个区域，
            // 编辑器预览要能看到全部区域，不能在这里截成 8 个。
            .take(MAX_PLUGIN_IDS)
            .collect()
    };
    let mut futures = Vec::new();
    for (key, variant, p) in selected {
        let params = RenderParams {
            width: 0,
            height: 0,
            theme: "ink".to_string(),
            variant,
        };
        futures.push(async move { (key, p.render(&params).await) });
    }
    let results = futures::future::join_all(futures).await;
    let out: Vec<PreviewContent> = results
        .into_iter()
        .filter_map(|(key, result)| {
            result.ok().map(|r| {
                let preview = preview_content(&r, text_mode);
                PreviewContent {
                    plugin: key,
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
    let base_params = RenderParams {
        width: q.width.unwrap_or(0),
        height: q.height.unwrap_or(0),
        theme: "ink".to_string(),
        variant: None,
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
        // 区域的子样式随插件一起带下去，并参与渲染缓存的 key。
        // 同一个插件在视图里只占一个区域（sanitize 按 plugin 去重），
        // 所以 ids 仍按 plugin 标识，variant 单独查表。
        let variants: HashMap<String, Option<String>> = match &layout {
            Some(regions) => regions
                .iter()
                .map(|r| (r.plugin.clone(), r.variant.clone()))
                .collect(),
            None => HashMap::new(),
        };
        let selected: Vec<(String, Option<String>, Arc<crate::plugin::PluginProcess>)> = ids
            .iter()
            .filter_map(|id| {
                guard.get(id).map(|p| {
                    (id.clone(), variants.get(id).cloned().flatten(), p)
                })
            })
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
        for (id, variant, p) in &selected {
            let key = preview_key(id, variant.as_deref());
            let fresh = cache
                .get(&key)
                .map(|(at, _, interval)| now.duration_since(*at) < Duration::from_millis(*interval))
                .unwrap_or(false);
            if fresh {
                if let Some((_, result, _)) = cache.get(&key) {
                    resolved.insert(id.clone(), result.clone());
                }
            } else {
                let params = RenderParams {
                    variant: variant.clone(),
                    ..base_params.clone()
                };
                to_render.push((id.clone(), key, Arc::clone(p), params));
            }
        }
        (to_render, resolved)
    };

    let mut futures = Vec::new();
    for (id, key, p, params) in to_render {
        futures.push(async move {
            let result = p.render(&params).await;
            (id, key, result)
        });
    }
    let results = futures::future::join_all(futures).await;

    // 失败隔离。插件本次渲染失败时，继续显示它的最后一帧有效内容。
    let mut stale_fallback: HashMap<String, RenderResult> = {
        let cache = state.render_cache.lock().await;
        cache
            .iter()
            .map(|(key, (_, r, _))| (key.clone(), r.clone()))
            .collect()
    };
    let mut updates = Vec::new();
    for (id, key, result) in results {
        match result {
            Ok(r) => {
                let interval = r.refresh_ms.unwrap_or(fallback_ms).clamp(250, 30_000);
                resolved.insert(id, r.clone());
                updates.push((key, r, interval));
            }
            Err(e) => {
                warn!(plugin = %id, error = %e, "render failed, using last frame if available");
                if let Some(r) = stale_fallback.remove(&key) {
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
            enabled: m.runtime.enabled,
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
            variants: i.map(|v| v.variants).unwrap_or_default(),
        });
    }
    for (id, p) in offline {
        let m = &p.manifest;
        out.push(PluginSummary {
            id,
            installed: false,
            enabled: m.runtime.enabled,
            name: m.plugin.name.clone(),
            version: m.plugin.version.clone(),
            description: m.plugin.description.clone(),
            default_on: m.runtime.default_on,
            running: false,
            variants: Vec::new(),
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
        .route("/api/preview/html", post(preview_html))
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

    #[test]
    fn preview_item_accepts_plain_and_variant_forms() {
        let req: PreviewRequest = serde_json::from_str(
            r#"{"plugins":["lyrics",{"plugin":"clock","variant":"dot"},{"plugin":"clock"}]}"#,
        )
        .unwrap();
        assert_eq!(req.plugins.len(), 3);
        assert_eq!(req.plugins[0].plugin(), "lyrics");
        assert_eq!(req.plugins[0].variant(), None);
        assert_eq!(req.plugins[1].plugin(), "clock");
        assert_eq!(req.plugins[1].variant(), Some("dot"));
        assert_eq!(req.plugins[2].variant(), None);
    }

    #[test]
    fn preview_key_separates_variants() {
        assert_eq!(preview_key("clock", None), "clock");
        assert_eq!(preview_key("clock", Some("")), "clock");
        assert_eq!(preview_key("clock", Some("dot")), "clock:dot");
        assert_ne!(
            preview_key("clock", Some("dot")),
            preview_key("clock", Some("calendar"))
        );
    }

    #[test]
    fn preview_document_uses_the_device_css_and_layout_classes() {
        let html = "<div class=\"block\">26</div>";
        let doc = preview_document(html, "gray", false);
        // 样式表来自同一份 ink.html：点阵字体与通用块规则都在文档里
        assert!(doc.contains("font-family: 'Silkscreen'"));
        assert!(doc.contains(".block + .block"));
        // 区域布局 class、预览 class、画布标记与设备一致。
        // 底部条标记仍在文档里，预览时由 preview 规则关掉。
        assert!(doc.contains("class=\"layout-region preview mode-gray \""));
        assert!(doc.contains("id=\"reader-bar\""));
        assert!(doc.contains("--canvas-h"));
        assert!(doc.contains(html));
        // 反色和显示模式进入文档，签名会跟着变
        let inverted = preview_document(html, "bw", true);
        assert!(inverted.contains("class=\"layout-region preview mode-bw invert\""));
        assert_ne!(doc, inverted);
    }

    #[test]
    fn preview_document_drops_the_reader_bar_from_the_canvas() {
        // 预览窗口只有两三百像素高，56px 的底部条会占掉两三成高度，
        // 区域百分比随之变形。两件事必须同时成立：body 上挂了 preview，
        // 样式表里有对应的整页高度规则。只断言规则文本存在没有意义——
        // 样式表是从 ink.html 整段复制过来的，有没有 class 都在。
        let doc = preview_document("<div></div>", "gray", false);
        assert!(
            doc.contains("<body class=\"layout-region preview "),
            "preview class missing on <body>"
        );
        assert!(
            doc.contains("body.preview { --canvas-h: 100vh; }"),
            "preview class must give the canvas the full viewport height"
        );
        assert!(
            doc.contains("body.preview #reader-bar, body.preview #status { display: none; }"),
            "preview class must hide the reader bar and the status line"
        );
    }

    #[test]
    fn device_page_sizes_the_canvas_around_the_reader_bar() {
        // 设备页面：画布是视口减去底部条，底部条本身仍在（点它进全屏）。
        assert!(INK_PAGE.contains("--canvas-h: calc(100vh - var(--bar-h))"));
        assert!(INK_PAGE.contains("id=\"reader-bar\""));
        // 预览用另一套画布高度，两者不能共用同一个值。
        assert!(INK_PAGE.contains("body.preview { --canvas-h: 100vh; }"));
    }

    #[test]
    fn region_sections_carry_a_gutter() {
        // 区域布局没有 #wrap 那层留白，内容会贴到墨水屏边缘。
        // 留白写在区块规则里，数值必须与 render::REGION_PAD_* 一致，
        // 否则缩放按不含留白的内容框算，内容又压回边缘。
        let gutter = format!(
            "padding: {}vh {}vw",
            crate::render::REGION_PAD_Y,
            crate::render::REGION_PAD_X
        );
        let section = css_rule(".frame-canvas .plugin-section {");
        assert!(
            section.contains("box-sizing: border-box"),
            "padding needs border-box: {section}"
        );
        assert!(section.contains(&gutter), "region gutter missing: {section}");

        // 相邻区块那条规则更具体，会把基础排版里的 padding-top 盖回去，
        // 所以它必须自己把留白写全，否则第二个以后的区块没有上留白。
        let sibling = css_rule(".frame-canvas .plugin-section + .plugin-section {");
        assert!(sibling.contains("margin: 0"), "stacked margin should reset: {sibling}");
        assert!(sibling.contains(&gutter), "sibling keeps the same gutter: {sibling}");
    }

    /// 取出 ink.html 里某条规则的声明块（到下一个 `}` 为止）。
    fn css_rule(selector: &str) -> String {
        let start = INK_PAGE
            .find(selector)
            .unwrap_or_else(|| panic!("missing rule: {selector}"));
        let rest = &INK_PAGE[start..];
        rest[..rest.find('}').expect("rule body")].to_string()
    }
}
