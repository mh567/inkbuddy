//! Server-side renderer: turns structured [`Block`]s into E-Ink friendly HTML.
//!
//! The host owns rendering so every plugin gets the same typography, the same
//! three-line focus view, and repaints only when the content changes.

use inkbuddy_protocol::{Block, Emphasis, Line, RenderResult};
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

/// A frame is the complete body HTML for all selected plugins, plus a stable
/// signature the browser uses to skip DOM writes when nothing changed.
pub struct Frame {
    pub html: String,
    pub signature: String,
    pub refresh_ms: u64,
    pub active: Vec<String>,
}

pub fn render_frame(parts: &[(String, RenderResult)], fallback_ms: u64) -> Frame {
    let mut html = String::new();
    let mut refresh_ms = u64::MAX;
    let mut active = Vec::new();

    if parts.is_empty() {
        // 给浏览器一个非空正文：空页面会清成纯白，签名不变时无法自行恢复。
        html.push_str(
            "<section class=\"plugin-section\"><div class=\"block block-text\"><p>无可用插件</p></div></section>",
        );
    }
    for (id, result) in parts {
        html.push_str(&format!(
            "<section class=\"plugin-section\" data-plugin=\"{}\">",
            esc(id)
        ));
        for block in &result.blocks {
            html.push_str(&render_block(block));
        }
        html.push_str("</section>");

        active.push(id.clone());
        if let Some(ms) = result.refresh_ms {
            if ms == 0 || ms > 30_000 {
                warn!(plugin = %id, refresh_ms = ms, "plugin reported an out-of-range refresh interval");
            }
            refresh_ms = refresh_ms.min(ms);
        }
    }

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> RenderResult {
        RenderResult {
            blocks: vec![Block::Text {
                text: text.to_string(),
            }],
            refresh_ms: Some(500),
        }
    }

    #[test]
    fn signature_tracks_the_rendered_markup() {
        let a = render_frame(&[("demo".into(), text_block("第一句"))], 500);
        let b = render_frame(&[("demo".into(), text_block("第一句"))], 500);
        let c = render_frame(&[("demo".into(), text_block("第二句"))], 500);
        assert_eq!(a.signature, b.signature);
        assert_ne!(a.signature, c.signature);
        assert_ne!(a.html, c.html);
    }

    #[test]
    fn signature_is_independent_of_text_containing_separators() {
        // The old `id:json|` concatenation could be forged by text containing
        // `|` or `:`. Hashing the markup makes the two frames distinct.
        let a = render_frame(&[("demo".into(), text_block("a|b:c"))], 500);
        let b = render_frame(&[("demo".into(), text_block("a|b:c|"))], 500);
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
            &[(
                "demo".into(),
                RenderResult {
                    blocks: vec![Block::Unknown],
                    refresh_ms: None,
                },
            )],
            500,
        );
        assert!(!frame.html.contains("block-unknown"));
        assert_eq!(frame.refresh_ms, 500);
    }

    #[test]
    fn reading_line_renders_as_plain_text() {
        let frame = render_frame(
            &[(
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
            )],
            500,
        );
        assert!(frame.html.contains("line-primary\">我会与你</div>"));
        assert!(frame.html.contains("line-reading\">o wui jyu nei</div>"));
        assert!(!frame.html.contains("ruby-grid"));
        assert!(!frame.html.contains("grid-template-columns"));
    }
}
