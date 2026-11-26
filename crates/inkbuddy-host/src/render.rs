//! Server-side renderer: turns structured [`Block`]s into E-Ink friendly HTML.
//!
//! The host owns rendering so every plugin gets the same typography, the same
//! three-line focus view, and repaints only when the content changes.

use crate::views::Region;
use inkbuddy_protocol::{Block, Emphasis, Line, RenderResult};
use serde::Serialize;
use std::hash::{Hash, Hasher};
use tracing::warn;

const ALLOWED_TAGS: &[&str] = &[
    "div", "span", "p", "b", "i", "em", "strong", "br", "ul", "li", "code", "pre", "h1", "h2", "h3",
];

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn emphasis_class(e: Emphasis) -> &'static str {
    match e {
        Emphasis::Primary => "line-primary",
        Emphasis::Normal => "line-normal",
        Emphasis::Muted => "line-muted",
        Emphasis::Reading => "line-reading",
    }
}

/// Render one line of text. Reading lines use the same layout with a smaller
/// CSS class, so no line type needs per-character cells.
fn render_line(line: &Line) -> String {
    let cls = emphasis_class(line.emphasis);
    format!("<div class=\"line {cls}\">{}</div>", esc(&line.text))
}

fn render_block(block: &Block) -> String {
    match block {
        Block::Lines { lines } => {
            let inner: String = lines.iter().map(render_line).collect();
            format!("<div class=\"block block-lines\">{inner}</div>")
        }
        Block::KeyValue { items } => {
            let rows: String = items
                .iter()
                .map(|kv| {
                    format!(
                        "<div class=\"kv\"><span class=\"kv-k\">{}</span><span class=\"kv-v\">{}</span></div>",
                        esc(&kv.key),
                        esc(&kv.value)
                    )
                })
                .collect();
            format!("<div class=\"block block-kv\">{rows}</div>")
        }
        Block::Text { text } => {
            format!("<div class=\"block block-text\"><p>{}</p></div>", esc(text))
        }
        // Raw markup from a plugin is untrusted: the service has no
        // authentication and plugin directories are user-writable. Keep only
        // a small tag allowlist and drop every attribute except a safe class.
        Block::Html { html } => {
            format!(
                "<div class=\"block block-html\">{}</div>",
                sanitize_html(html)
            )
        }
        Block::Unknown => String::new(),
    }
}

/// Allowlist-based HTML filter. Tags outside [`ALLOWED_TAGS`] are escaped into
/// plain text, which keeps `<script>`, `<img onerror=...>` and event handlers
/// out of the DOM. Only `class` is kept, restricted to a safe charset.
fn sanitize_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let ch = input[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some(rel) = input[i..].find('>') else {
            // Unclosed tag: escape the rest as text.
            out.push_str(&esc(&input[i..]));
            break;
        };
        let end = i + rel;
        let inner = input[i + 1..end].trim();
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim_start();
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        if name.is_empty() || !ALLOWED_TAGS.contains(&name.as_str()) {
            out.push_str(&esc(&input[i..=end]));
            i = end + 1;
            continue;
        }
        if closing {
            out.push_str(&format!("</{name}>"));
        } else {
            let rest = &body[name.len()..];
            match safe_class(rest) {
                Some(class) => out.push_str(&format!("<{name} class=\"{class}\">")),
                None => out.push_str(&format!("<{name}>")),
            }
        }
        i = end + 1;
    }
    // 兜底：移除所有 javascript: URL。
    let lower = out.to_ascii_lowercase();
    if lower.contains("javascript:") {
        return out.replace("javascript:", "").replace("JAVASCRIPT:", "");
    }
    out
}

/// Extract a `class="..."` value if present, keeping only characters that
/// cannot break out of the attribute or introduce a new one.
fn safe_class(rest: &str) -> Option<String> {
    let lower = rest.to_ascii_lowercase();
    let at = lower.find("class")?;
    let after = rest[at + 5..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &after[1..];
    let end = value.find(quote)?;
    let filtered: String = value[..end]
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .take(128)
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

/// 预览用的结构化文本行。菜单栏 app 不跑 WebKit，直接按 role 排版。
#[derive(Debug, Clone, Serialize)]
pub struct PreviewLine {
    pub text: String,
    pub role: String,
}

/// 预览用的一块内容：块内行紧挨着排，块与块之间有间距。
/// 与墨水屏页面的 .block + .block margin-top 语义一致。
#[derive(Debug, Clone, Serialize)]
pub struct PreviewBlock {
    pub lines: Vec<PreviewLine>,
}

/// 一个屏幕区域的结构化内容：几何信息 + 该插件摊平后的文本行。
/// 控制端（菜单栏预览、编辑器画布）用它做原生渲染，与墨水屏同源。
#[derive(Debug, Clone, Serialize)]
pub struct FrameRegion {
    pub plugin: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 服务端算好的字号缩放系数。fit 已扣掉区域四周的留白，
    /// 客户端要自己复算时按同一套规则（可用宽高）来。
    pub scale: f32,
    pub lines: Vec<PreviewLine>,
}

fn preview_role(e: Emphasis) -> &'static str {
    match e {
        Emphasis::Primary => "primary",
        Emphasis::Normal => "normal",
        Emphasis::Muted => "muted",
        Emphasis::Reading => "reading",
    }
}

/// 去掉 HTML 标签，留下可见文本。只用于插件自家输出的简单标记。
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else { break };
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out
}

/// 读插件在标记里声明的属性值。属性只出现在 `<...>` 内部，值用双引号，
/// 内容限制在安全字符内，取不到就返回 None。宿主不解析插件的 class 名。
fn html_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let at = tag.find(name)?;
    let after = tag[at + name.len()..].trim_start();
    let value = after.strip_prefix("=")?.trim_start().strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

/// 结构化文本预览里允许的行角色。未知值按 muted 处理，避免插件写入
/// 任意字符串后影响控制端排版。
fn declared_role(tag: &str) -> &'static str {
    match html_attr(tag, "data-ink-role") {
        Some("primary") => "primary",
        Some("normal") => "normal",
        Some("muted") => "muted",
        Some("reading") => "reading",
        _ => "muted",
    }
}

/// HTML 块声明的内容高度，单位是「正方形屏幕下的 vh」。
/// 插件按自己的 CSS 尺寸给一个数，宿主不认识具体样式名。
fn declared_height(html: &str) -> f32 {
    html_attr(html, "data-ink-height")
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 200.0)
        .unwrap_or(0.0)
}

/// HTML 块的控制端预览：取每个不含子 div 的叶子 div 的文字。
/// 外层容器整段跳过，避免把头、正文、脚注拼成一行。
fn html_preview_lines(html: &str) -> Vec<PreviewLine> {
    let mut lines = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = html[from..].find("<div") {
        let start = from + rel;
        let Some(gt_rel) = html[start..].find('>') else { break };
        let gt = start + gt_rel;
        let Some(end_rel) = html[gt + 1..].find("</div>") else { break };
        let end = gt + 1 + end_rel;
        let tag = &html[start..=gt];
        let inner = &html[gt + 1..end];
        if !inner.contains("<div") {
            let text = strip_tags(inner).trim().to_string();
            if !text.is_empty() {
                lines.push(PreviewLine {
                    text,
                    role: declared_role(tag).to_string(),
                });
            }
        }
        from = start + 4;
    }
    lines
}

/// 把插件的结构化内容按块分组，供控制端复刻页面排版（块间距、块内行距）。
/// kv 行的 role 标为 "kv"（页面上 .kv 字号 4vw、行高 1.8，与普通行不同）。
fn collect_preview_blocks(blocks: &mut Vec<PreviewBlock>, result: &RenderResult) {
    for block in &result.blocks {
        let mut lines = Vec::new();
        match block {
            Block::Lines { lines: ls } => {
                for line in ls {
                    lines.push(PreviewLine {
                        text: line.text.clone(),
                        role: preview_role(line.emphasis).to_string(),
                    });
                }
            }
            Block::KeyValue { items } => {
                for kv in items {
                    lines.push(PreviewLine {
                        text: format!("{} {}", kv.key, kv.value),
                        role: "kv".to_string(),
                    });
                }
            }
            Block::Text { text } => {
                lines.push(PreviewLine {
                    text: text.clone(),
                    role: "text".to_string(),
                });
            }
            Block::Html { html } => {
                lines.extend(html_preview_lines(html));
            }
            Block::Unknown => {}
        }
        if !lines.is_empty() {
            blocks.push(PreviewBlock { lines });
        }
    }
}

/// 把插件的结构化内容摊平成纯文本行，供控制端原生渲染预览。
fn collect_preview_lines(lines: &mut Vec<PreviewLine>, result: &RenderResult) {
    for block in &result.blocks {
        match block {
            Block::Lines { lines: ls } => {
                for line in ls {
                    lines.push(PreviewLine {
                        text: line.text.clone(),
                        role: preview_role(line.emphasis).to_string(),
                    });
                }
            }
            Block::KeyValue { items } => {
                for kv in items {
                    lines.push(PreviewLine {
                        text: format!("{} {}", kv.key, kv.value),
                        role: "normal".to_string(),
                    });
                }
            }
            Block::Text { text } => {
                lines.push(PreviewLine {
                    text: text.clone(),
                    role: "text".to_string(),
                });
            }
            Block::Html { html } => {
                lines.extend(html_preview_lines(html));
            }
            Block::Unknown => {}
        }
    }
}

/// A frame is the complete body HTML for the selected plugins, plus a stable
/// signature the browser uses to skip DOM writes when nothing changed.
/// `layout` 存在时按区域绝对定位渲染；None 时竖排流式（无视图/URL 模式）。
pub struct Frame {
    pub html: String,
    pub signature: String,
    pub refresh_ms: u64,
    pub active: Vec<String>,
    /// 每个已渲染插件占用的屏幕区域及其文本行，供原生预览。
    pub regions: Vec<FrameRegion>,
    /// 是否按区域布局渲染（当前视图生效）。页面据此切换容器样式。
    pub layout: bool,
}

/// 区域内容四周的留白。与 web/ink.html 里 `.frame-canvas .plugin-section`
/// 的 `padding: 3vh 4vw` 对应：竖排布局有 #wrap 的留白，区域布局没有，
/// 就加在区块自己身上。单位分别是屏幕高度、宽度的百分比。
/// 缩放按扣掉留白后的可用高度与宽度算：空间不够时缩小字号，内容不压回边缘。
pub(crate) const REGION_PAD_Y: f32 = 3.0;
pub(crate) const REGION_PAD_X: f32 = 4.0;

/// 区域内容的缩放系数：宽度与高度两个约束取较小值。
/// - 宽度：字号以 vw 定义，可用宽度占屏幕的比例决定文字相对区域的填充比例。
///   可用宽度扣掉左右留白，换行位置因此与留白前一致。
/// - 高度：内容装不下时自动缩小。`content_vh` 是内容按基准字号需要的
///   高度（屏幕高度的百分比，已含宽高比换算），fit = 可用高度 / 内容高度；
///   可用高度扣掉上下留白，装得下时 fit ≥ 1 不起作用，装不下时把字号
///   压到刚好放得下。
fn region_scale(w: f32, h: f32, content_vh: f32) -> f32 {
    let usable_w = (w - REGION_PAD_X * 2.0).max(0.0);
    let width_scale = (usable_w / 100.0).clamp(0.1, 1.0);
    let usable_h = (h - REGION_PAD_Y * 2.0).max(0.0);
    let fit = if content_vh > 0.01 {
        (usable_h / content_vh).clamp(0.1, 1.0)
    } else {
        1.0
    };
    let s = width_scale.min(fit);
    (s * 100.0).round() / 100.0
}

/// 估算插件内容按基准字号渲染需要的高度，单位是「正方形屏幕下的 vh」
/// （即 1vw 字体按 1vh 计）。真实屏幕的换算在调用处乘以宽高比。
/// 数值与 ink.html 的 CSS 保持一致：行高约 1.4，primary 行有上下边距，
/// muted 行有最小高度，块与块之间有间距。
fn content_height_units(result: &RenderResult, text_mode: bool) -> f32 {
    let mut total = 0.0;
    for (block_index, block) in result.blocks.iter().enumerate() {
        if block_index > 0 {
            total += 2.0; // .block + .block 的 margin-top: 2vh
        }
        match block {
            Block::Lines { lines } => {
                for line in lines {
                    total += match line.emphasis {
                        Emphasis::Primary => 6.2 * 1.4 + 5.0, // 字号×行高 + 上下 margin 2.5vh×2
                        Emphasis::Normal => 5.0 * 1.4,
                        Emphasis::Muted => 7.0, // min-height: 7vw
                        // 4.6×行高 1.25 + margin(-2.4+1.8)vh
                        Emphasis::Reading => 4.6 * 1.25 - 0.6,
                    };
                }
            }
            Block::KeyValue { items } => {
                total += 4.0 * 1.8 * items.len() as f32; // .kv line-height: 1.8
            }
            Block::Text { .. } => {
                let title = if text_mode { 5.0 } else { 3.8 };
                total += title * 1.6;
            }
            // HTML 块也要参与高度估算，否则短区域不会缩小，内容会被裁掉。
            // 高度由插件在标记里声明，宿主按同一公式换算，不认识具体样式。
            Block::Html { html } => {
                total += declared_height(html);
            }
            Block::Unknown => {}
        }
    }
    total
}

/// /api/preview 用：单个插件当前内容的预览文本行与估算内容高度。
pub struct PluginPreview {
    pub lines: Vec<PreviewLine>,
    /// 按块分组的内容，控制端用它复刻墨水屏的块间距与行距。
    pub blocks: Vec<PreviewBlock>,
    /// 内容按基准字号需要的高度（正方形屏幕下的 vh），控制端用它算 fit。
    pub content_units: f32,
}

pub fn preview_content(result: &RenderResult, text_mode: bool) -> PluginPreview {
    let mut lines = Vec::new();
    collect_preview_lines(&mut lines, result);
    let mut blocks = Vec::new();
    collect_preview_blocks(&mut blocks, result);
    PluginPreview {
        lines,
        blocks,
        content_units: content_height_units(result, text_mode),
    }
}

/// 字号覆盖的 CSS 选择器里直接拼接插件 id，只接受受限字符集；
/// 含其它字符（引号、反斜杠、花括号等）的 id 跳过覆盖，避免破坏样式表。
fn css_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 单个区域的字号覆盖规则：基准尺寸 × 缩放系数，算成具体数值输出。
/// 这是接入规范的一部分：插件只要用标准块/行类型输出内容，
/// 缩放就自动生效，插件自身不需要处理尺寸。区域都要留白，
/// 缩放必然小于 1，所以每个区域都会有一组规则。
/// text_mode（纯文本显示模式）下标题块用更大的基准字号，与页面 CSS 一致。
fn section_overrides(plugin: &str, scale: f32, text_mode: bool) -> String {
    let m = |v: f32| fmt_pct(v * scale);
    let sel = format!(".frame-canvas section[data-plugin=\"{}\"]", esc(plugin));
    let mut css = String::new();
    // HTML 块里的自定义样式用这个变量跟着区域一起缩放。
    css.push_str(&format!("{sel} {{ --ink-scale: {}; }}", fmt_pct(scale)));
    css.push_str(&format!(
        "{sel} .line-primary {{ font-size: {}vw; margin: {}vh 0; }}",
        m(6.2),
        m(2.5)
    ));
    css.push_str(&format!("{sel} .line-normal {{ font-size: {}vw; }}", m(5.0)));
    css.push_str(&format!(
        "{sel} .line-muted {{ font-size: {}vw; min-height: {}vw; }}",
        m(5.0),
        m(7.0)
    ));
    css.push_str(&format!(
        "{sel} .line-reading {{ font-size: {}vw; margin: {}vh 0 {}vh; }}",
        m(4.6),
        m(-2.4),
        m(1.8)
    ));
    css.push_str(&format!("{sel} .kv {{ font-size: {}vw; }}", m(4.0)));
    css.push_str(&format!(
        "{sel} .block + .block {{ margin-top: {}vh; }}",
        m(2.0)
    ));
    let title = if text_mode { 5.0 } else { 3.8 };
    css.push_str(&format!(
        "{sel} .block-text p {{ font-size: {}vw; }}",
        m(title)
    ));
    css
}

fn html_for_regions(
    parts: &[(String, RenderResult)],
    layout: &[Region],
    text_mode: bool,
    aspect: f32,
) -> String {
    // 有内容的区域才生成字号覆盖；没装或没内容的区域不产生规则。
    let mut rules = String::new();
    for region in layout {
        if let Some((_, result)) = parts.iter().find(|(id, _)| id == &region.plugin) {
            // 字体是 vw，区域高度是屏幕高度的百分比：乘以宽高比才能比。
            let units = content_height_units(result, text_mode);
            let scale = region_scale(region.w, region.h, units * aspect);
            if css_safe_id(&region.plugin) {
                rules.push_str(&section_overrides(&region.plugin, scale, text_mode));
            } else {
                warn!(plugin = %region.plugin, "plugin id has characters unsafe for CSS selectors; skipping font-size overrides");
            }
        }
    }
    let mut html = String::new();
    if !rules.is_empty() {
        html.push_str(&format!("<style>{rules}</style>"));
    }
    html.push_str("<div class=\"frame-canvas\">");
    for region in layout {
        let Some((_, result)) = parts.iter().find(|(id, _)| id == &region.plugin) else {
            continue;
        };
        let inner: String = result.blocks.iter().map(render_block).collect();
        html.push_str(&format!(
            "<section class=\"plugin-section\" data-plugin=\"{}\" style=\"left:{}%;top:{}%;width:{}%;height:{}%;\">",
            esc(&region.plugin),
            fmt_pct(region.x),
            fmt_pct(region.y),
            fmt_pct(region.w),
            fmt_pct(region.h)
        ));
        html.push_str(&inner);
        html.push_str("</section>");
    }
    html.push_str("</div>");
    html
}

/// 流式竖排：无布局时的回退排版。
fn html_stacked(parts: &[(String, RenderResult)]) -> String {
    let mut html = String::new();
    for (id, result) in parts {
        html.push_str(&format!(
            "<section class=\"plugin-section\" data-plugin=\"{}\">",
            esc(id)
        ));
        for block in &result.blocks {
            html.push_str(&render_block(block));
        }
        html.push_str("</section>");
    }
    html
}

/// 去掉浮点尾零：12.0 → "12"，12.5 → "12.5"。
fn fmt_pct(v: f32) -> String {
    let v = (v * 100.0).round() / 100.0;
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// 把每个插件的内容与它的屏幕区域配对。layout 为 None 时生成竖排等分。
fn frame_regions(
    parts: &[(String, RenderResult)],
    layout: Option<&[Region]>,
    text_mode: bool,
    aspect: f32,
) -> Vec<FrameRegion> {
    let scale_of = |r: &Region, result: &RenderResult| {
        let units = content_height_units(result, text_mode);
        region_scale(r.w, r.h, units * aspect)
    };
    match layout {
        Some(regions) => regions
            .iter()
            .filter_map(|r| {
                let (_, result) = parts.iter().find(|(id, _)| id == &r.plugin)?;
                let scale = scale_of(r, result);
                let mut lines = Vec::new();
                collect_preview_lines(&mut lines, result);
                Some(FrameRegion {
                    plugin: r.plugin.clone(),
                    x: r.x,
                    y: r.y,
                    w: r.w,
                    h: r.h,
                    scale,
                    lines,
                })
            })
            .collect(),
        None => parts
            .iter()
            .enumerate()
            .map(|(i, (id, result))| {
                let mut lines = Vec::new();
                collect_preview_lines(&mut lines, result);
                let r = Region::stacked(id, i, parts.len());
                FrameRegion {
                    plugin: id.clone(),
                    x: r.x,
                    y: r.y,
                    w: r.w,
                    h: r.h,
                    // 流式布局无字号覆盖，原生预览按全尺寸渲染。
                    scale: 1.0,
                    lines,
                }
            })
            .collect(),
    }
}

/// `text_mode` 对应显示设置的纯文本模式：区域布局下标题块用更大的基准字号。
/// `aspect` 是墨水屏的宽高比（宽/高），由页面轮询时上报；未知按 1（正方形）。
pub fn render_frame_with_mode(
    parts: &[(String, RenderResult)],
    fallback_ms: u64,
    layout: Option<&[Region]>,
    text_mode: bool,
    aspect: f32,
) -> Frame {
    let mut refresh_ms = u64::MAX;
    let mut active = Vec::new();

    let html = if parts.is_empty() {
        // 空视图/无已安装扩展时的占位。标记必须与 web/ink.html 页面脚本里的
        // placeholder 完全一致：两边不一致时，浏览器会每 300ms 在两个 DOM
        // 之间来回改写，墨水屏整屏重绘、看起来在闪。
        "<div class=\"block block-text\"><p>无可用扩展</p></div>".to_string()
    } else {
        match layout {
            Some(regions) => html_for_regions(parts, regions, text_mode, aspect),
            None => html_stacked(parts),
        }
    };

    for (id, result) in parts {
        active.push(id.clone());
        if let Some(ms) = result.refresh_ms {
            if ms == 0 || ms > 30_000 {
                warn!(plugin = %id, refresh_ms = ms, "plugin reported an out-of-range refresh interval");
            }
            refresh_ms = refresh_ms.min(ms);
        }
    }

    let regions = frame_regions(parts, layout, text_mode, aspect);

    if refresh_ms == u64::MAX {
        refresh_ms = fallback_ms;
    }
    // Keep polling within a sane band for E-Ink devices.
    let refresh_ms = refresh_ms.clamp(250, 30_000);

    // 对最终 HTML 取哈希：HTML 相同则浏览器跳过重绘，插件文本里的分隔符也
    // 无法伪造出与其它帧相同的签名。
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut hasher);
    let signature = format!("{:016x}", hasher.finish());

    Frame {
        html,
        signature,
        refresh_ms,
        active,
        regions,
        layout: layout.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试便捷封装：默认灰度模式、正方形屏幕。
    fn render_frame(
        parts: &[(String, RenderResult)],
        fallback_ms: u64,
        layout: Option<&[Region]>,
    ) -> Frame {
        render_frame_with_mode(parts, fallback_ms, layout, false, 1.0)
    }

    fn text_block(text: &str) -> RenderResult {
        RenderResult {
            blocks: vec![Block::Text {
                text: text.to_string(),
            }],
            refresh_ms: Some(500),
        }
    }

    #[test]
    fn region_scale_tracks_width_and_height_fit() {
        // 宽度约束：扣掉左右留白后占屏幕的比例；极窄的区域夹到 0.1。
        let full_width = (100.0 - REGION_PAD_X * 2.0) / 100.0;
        assert_eq!(region_scale(100.0, 50.0, 10.0), full_width);
        assert_eq!(region_scale(50.0, 50.0, 10.0), 0.42);
        assert_eq!(region_scale(2.0, 50.0, 10.0), 0.1);
        // 高度装不下时缩小：时钟型内容约 20.7 vh，区域高 13.8，扣掉上下
        // 留白 6vh 后可用 7.8，压到约 0.38。
        let clock_units = 6.2 * 1.4 + 5.0 + 7.0;
        assert_eq!(region_scale(100.0, 13.8, clock_units), 0.38);
        // 装得下时高度不起作用，只剩宽度约束。
        assert_eq!(region_scale(100.0, 86.2, clock_units), full_width);
        // 区域比留白还矮：夹到下限。
        assert_eq!(region_scale(100.0, 3.0, clock_units), 0.1);
    }

    #[test]
    fn region_scale_leaves_room_for_the_gutter() {
        // 留白后的可用尺寸说了算：内容高度正好等于可用高度时，高度不再是
        // 瓶颈，缩放由宽度决定；再高一个单位，高度就压过宽度。
        let units = 10.0;
        let fits_h = units + REGION_PAD_Y * 2.0;
        let full_width = (100.0 - REGION_PAD_X * 2.0) / 100.0;
        assert_eq!(region_scale(100.0, fits_h, units), full_width);
        assert!(region_scale(100.0, fits_h - 1.0, units) < full_width);
        // 窄区域的可用宽度再扣一次留白：92% 宽的区域只剩 84%。
        assert_eq!(region_scale(92.0, 200.0, 10.0), 0.84);
        // 极矮的区域夹到下限，不会算出 0 或负数。
        assert_eq!(region_scale(100.0, REGION_PAD_Y, units), 0.1);
    }

    #[test]
    fn content_height_units_matches_css_numbers() {
        // 单行 primary：6.2×1.4 + 上下边距 5.0。
        let result = RenderResult {
            blocks: vec![Block::Lines {
                lines: vec![Line {
                    text: "12:00".into(),
                    emphasis: Emphasis::Primary,
                }],
            }],
            refresh_ms: Some(500),
        };
        assert_eq!(content_height_units(&result, false), 6.2 * 1.4 + 5.0);
    }

    #[test]
    fn content_height_units_kv_and_reading_match_css() {
        // kv 行：.kv 字号 4vw、行高 1.8。
        let kv = RenderResult {
            blocks: vec![Block::KeyValue {
                items: vec![inkbuddy_protocol::KeyValue {
                    key: "日期".into(),
                    value: "11月25日".into(),
                }],
            }],
            refresh_ms: Some(500),
        };
        assert_eq!(content_height_units(&kv, false), 4.0 * 1.8);
        // reading 行：字号 4.6vw、行高 1.25、margin -2.4vh 0 1.8vh。
        let reading = RenderResult {
            blocks: vec![Block::Lines {
                lines: vec![Line {
                    text: "goeng3 si4".into(),
                    emphasis: Emphasis::Reading,
                }],
            }],
            refresh_ms: Some(500),
        };
        assert_eq!(content_height_units(&reading, false), 4.6 * 1.25 - 0.6);
    }

    #[test]
    fn html_block_height_comes_from_plugin_declaration() {
        let mk = |html: &str| RenderResult {
            blocks: vec![Block::Html {
                html: html.to_string(),
            }],
            refresh_ms: Some(500),
        };
        // class 名可以是任意值：宿主只读 data-ink-height。
        let dot = mk("<div class=\"block any-name\" data-ink-height=\"31.2\"></div>");
        let cal = mk("<div class=\"block another-name\" data-ink-height=\"49.4\"></div>");
        let plain = mk("<div class=\"block other\"></div>");
        assert_eq!(content_height_units(&dot, false), 31.2);
        assert_eq!(content_height_units(&cal, false), 49.4);
        assert_eq!(content_height_units(&plain, false), 0.0);
        // 短区域必须缩小高内容，否则会被裁掉。
        assert!(region_scale(100.0, 20.0, 49.4) < 0.5);
        // 非法声明不能把缩放带偏。
        for bad in ["-3", "0", "abc", "99999", "NaN"] {
            let r = mk(&format!("<div data-ink-height=\"{bad}\"></div>"));
            assert_eq!(content_height_units(&r, false), 0.0, "bad value: {bad}");
        }
    }

    #[test]
    fn fullscreen_region_scales_to_its_content_box() {
        let parts = vec![("clock".into(), text_block("时间"))];
        let layout = vec![Region { plugin: "clock".into(), x: 0.0, y: 0.0, w: 100.0, h: 100.0, variant: None }];
        let frame = render_frame(&parts, 500, Some(&layout));
        // 满屏区域也要为左右留白让出宽度：缩放是可用宽度占比，不是 1。
        let expected = (100.0 - REGION_PAD_X * 2.0) / 100.0;
        assert!(frame.html.contains(&format!("--ink-scale: {expected};")));
        // 留白之外的区域（更窄）依旧按宽度占比缩小。
        let narrow = vec![Region { plugin: "clock".into(), x: 0.0, y: 0.0, w: 50.0, h: 100.0, variant: None }];
        let narrow_frame = render_frame(&parts, 500, Some(&narrow));
        assert!(narrow_frame.html.contains("--ink-scale: 0.42;"));
    }

    #[test]
    fn text_mode_uses_larger_title_baseline() {
        let parts = vec![("clock".into(), text_block("时间"))];
        let layout = vec![Region { plugin: "clock".into(), x: 0.0, y: 0.0, w: 50.0, h: 50.0, variant: None }];
        let normal = render_frame(&parts, 500, Some(&layout));
        let text = render_frame_with_mode(&parts, 500, Some(&layout), true, 1.0);
        // 50% 宽的区域扣掉左右留白是 42%：3.8vw × 0.42 ≈ 1.6vw（常规），
        // 5vw × 0.42 = 2.1vw（纯文本模式）。
        assert!(normal.html.contains(".block-text p { font-size: 1.6vw;"));
        assert!(text.html.contains(".block-text p { font-size: 2.1vw;"));
    }

    #[test]
    fn empty_parts_render_a_stable_placeholder() {
        // 空视图（regions 为空或引用的扩展都未安装）必须返回签名恒定的
        // 占位 HTML；签名一变，页面脚本就会整屏重写，墨水屏闪烁。
        let a = render_frame(&[], 500, None);
        let b = render_frame(&[], 500, Some(&[]));
        assert_eq!(a.signature, b.signature);
        assert_eq!(a.signature, render_frame(&[], 500, None).signature);
        assert!(a.html.contains("无可用扩展"));
        assert!(a.active.is_empty());
        // 与 web/ink.html 页面脚本里的 placeholder 保持同一份标记，
        // 防止两边不一致导致浏览器每轮询一次就改写一次 DOM。
        assert_eq!(
            a.html,
            "<div class=\"block block-text\"><p>无可用扩展</p></div>"
        );
    }

    #[test]
    fn signature_tracks_the_rendered_markup() {
        let a = render_frame(&[("demo".into(), text_block("第一句"))], 500, None);
        let b = render_frame(&[("demo".into(), text_block("第一句"))], 500, None);
        let c = render_frame(&[("demo".into(), text_block("第二句"))], 500, None);
        assert_eq!(a.signature, b.signature);
        assert_ne!(a.signature, c.signature);
        assert_ne!(a.html, c.html);
    }

    #[test]
    fn signature_is_independent_of_text_containing_separators() {
        // The old `id:json|` concatenation could be forged by text containing
        // `|` or `:`. Hashing the markup makes the two frames distinct.
        let a = render_frame(&[("demo".into(), text_block("a|b:c"))], 500, None);
        let b = render_frame(&[("demo".into(), text_block("a|b:c|"))], 500, None);
        assert_ne!(a.signature, b.signature);
    }

    #[test]
    fn sanitize_removes_scripts_handlers_and_unknown_tags() {
        let dirty = "<p class=\"nice\">你好</p><script>alert(1)</script>\
                     <img src=x onerror=alert(2)><div onclick=\"evil()\">文本</div>\
                     <a href=\"javascript:alert(3)\">链接</a>";
        let clean = sanitize_html(dirty);
        assert!(clean.contains("<p class=\"nice\">你好</p>"));
        assert!(clean.contains("<div>文本</div>"));
        assert!(!clean.contains("<script"));
        assert!(!clean.contains("<img"));
        assert!(!clean.contains("onclick"));
        assert!(!clean.to_ascii_lowercase().contains("javascript:"));
        // Disallowed tags become visible text, not executable markup. The
        // literal string "onerror" may appear inside that escaped text.
        assert!(clean.contains("&lt;img"));
    }

    #[test]
    fn sanitize_keeps_safe_class_but_drops_other_attributes() {
        let clean = sanitize_html("<span class=\"roma x\" id=\"y\" style=\"color:red\">o</span>");
        assert_eq!(clean, "<span class=\"roma x\">o</span>");
    }

    #[test]
    fn unknown_block_contributes_no_markup() {
        let frame = render_frame(
            &[
                (
                    "demo".into(),
                    RenderResult {
                        blocks: vec![Block::Unknown],
                        refresh_ms: None,
                    },
                ),
            ],
            500,
            None,
        );
        assert!(!frame.html.contains("block-unknown"));
        assert_eq!(frame.refresh_ms, 500);
    }

    #[test]
    fn layout_render_positions_sections_absolutely() {
        let parts = vec![("lyrics".into(), text_block("歌词")), ("clock".into(), text_block("时间"))];
        let layout = vec![
            Region { plugin: "lyrics".into(), x: 0.0, y: 0.0, w: 60.0, h: 100.0, variant: None },
            Region { plugin: "clock".into(), x: 60.0, y: 0.0, w: 40.0, h: 100.0, variant: None },
        ];
        let frame = render_frame(&parts, 500, Some(&layout));
        assert!(frame.html.contains("frame-canvas"));
        assert!(frame.html.contains("left:0%;top:0%;width:60%;height:100%"));
        assert!(frame.html.contains("left:60%;top:0%;width:40%;height:100%"));
        // 字号覆盖由服务端算好直接输出（60 宽 → 可用 52%，40 宽 → 可用 32%）：
        // 6.2vw × 0.52 = 3.22vw，6.2vw × 0.32 = 1.98vw。
        assert!(frame.html.contains(
            ".frame-canvas section[data-plugin=\"lyrics\"] .line-primary { font-size: 3.22vw;"
        ));
        assert!(frame.html.contains(
            ".frame-canvas section[data-plugin=\"clock\"] .line-primary { font-size: 1.98vw;"
        ));
        // 同一份系数也下发给 HTML 块的自定义样式
        assert!(frame.html.contains("--ink-scale: 0.52;"));
        assert!(frame.html.contains("--ink-scale: 0.32;"));
        assert_eq!(frame.regions.len(), 2);
        assert_eq!(frame.regions[0].plugin, "lyrics");
        assert_eq!(frame.regions[0].lines.len(), 1);
        // 布局里没引用的插件不出现在 regions
        let partial = render_frame(&parts, 500, Some(&layout[..1]));
        assert_eq!(partial.regions.len(), 1);
        assert!(!partial.html.contains("data-plugin=\"clock\""));
    }

    #[test]
    fn stacked_mode_emits_even_regions() {
        let parts = vec![
            ("lyrics".into(), text_block("歌词")),
            ("clock".into(), text_block("时间")),
        ];
        let frame = render_frame(&parts, 500, None);
        assert_eq!(frame.regions.len(), 2);
        assert_eq!(frame.regions[0].y, 0.0);
        assert_eq!(frame.regions[0].h, 50.0);
        assert_eq!(frame.regions[1].y, 50.0);
        // 流式 html 不含绝对定位容器
        assert!(!frame.html.contains("frame-canvas"));
    }

    #[test]
    fn reading_line_renders_as_plain_text() {
        let frame = render_frame(
            &[
                (
                    "lyrics".into(),
                    RenderResult {
                        blocks: vec![Block::Lines {
                            lines: vec![
                                Line::new("我会与你", Emphasis::Primary),
                                Line::new("o wui jyu nei", Emphasis::Reading),
                            ],
                        }],
                        refresh_ms: Some(500),
                    },
                ),
            ],
            500,
            None,
        );
        assert!(frame.html.contains("line-primary\">我会与你</div>"));
        assert!(frame.html.contains("line-reading\">o wui jyu nei</div>"));
        assert!(!frame.html.contains("ruby-grid"));
        assert!(!frame.html.contains("grid-template-columns"));
    }

    #[test]
    fn html_block_preview_uses_declared_roles() {
        // class 名不带任何插件标识，角色完全由 data-ink-role 决定。
        let html = "<div class=\"block whatever\" data-ink-height=\"31.2\">\
                      <div class=\"a\" data-ink-role=\"primary\">21:22</div>\
                      <div class=\"b\" data-ink-role=\"normal\"><span>11月26日</span></div>\
                      <div class=\"c\" data-ink-role=\"bogus\">note</div>\
                      <div class=\"d\">fallback</div>\
                    </div>";
        let result = RenderResult {
            blocks: vec![Block::Html {
                html: html.to_string(),
            }],
            refresh_ms: None,
        };
        let preview = preview_content(&result, false);
        assert_eq!(preview.lines.len(), 4);
        assert_eq!(preview.lines[0].text, "21:22");
        assert_eq!(preview.lines[0].role, "primary");
        assert_eq!(preview.lines[1].text, "11月26日");
        assert_eq!(preview.lines[1].role, "normal");
        assert_eq!(preview.lines[2].role, "muted", "unknown role falls back");
        assert_eq!(preview.lines[3].role, "muted", "missing role falls back");
        assert_eq!(preview.content_units, 31.2);
    }

    #[test]
    fn scaled_region_exposes_ink_scale_variable() {
        let parts = vec![("clock".into(), text_block("时间"))];
        let layout = vec![Region {
            plugin: "clock".into(),
            x: 0.0,
            y: 0.0,
            w: 40.0,
            h: 100.0,
            variant: None,
        }];
        let frame = render_frame(&parts, 500, Some(&layout));
        // 40% 宽的区域扣掉左右留白还剩 32%。
        assert!(frame.html.contains("--ink-scale: 0.32;"));
    }
}
