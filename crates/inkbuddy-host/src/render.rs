//! Server-side renderer: turns structured [`Block`]s into E-Ink friendly HTML.
//!
//! Keeping rendering here (rather than in each plugin) is the point of the
//! structured protocol: every plugin automatically gets the same typography,
//! the same three-line focus view, and the same "don't repaint unless the
//! content changed" behaviour.

use inkbuddy_protocol::{Block, Emphasis, Line, RenderResult};

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
    }
}

/// Render a single line. When `ruby` is present, use a shared-column grid:
/// the first row holds the readings, the second row the glyphs. Both rows
/// have their own CSS, so romanisation layout is independent of glyph layout.
fn render_line(line: &Line) -> String {
    let cls = emphasis_class(line.emphasis);
    if line.ruby.is_empty() {
        return format!("<div class=\"line {cls}\">{}</div>", esc(&line.text));
    }

    let n = line.ruby.len();
    let mut readings = String::new();
    let mut glyphs = String::new();
    for r in &line.ruby {
        readings.push_str(&format!("<span class=\"roma\">{}</span>", esc(&r.reading)));
        glyphs.push_str(&format!("<span class=\"han\">{}</span>", esc(&r.word)));
    }
    format!(
        "<div class=\"line {cls} ruby-grid\" style=\"grid-template-columns:repeat({n},auto)\">{readings}{glyphs}</div>"
    )
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
        // Deliberately unescaped: the escape hatch is trusted, host-installed
        // plugin output. Documented in the protocol.
        Block::Html { html } => format!("<div class=\"block block-html\">{html}</div>"),
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
    let mut signature = String::new();
    let mut refresh_ms = u64::MAX;
    let mut active = Vec::new();

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
        signature.push_str(id);
        signature.push(':');
        if let Ok(v) = serde_json::to_string(&result.blocks) {
            signature.push_str(&v);
        }
        signature.push('|');

        if let Some(ms) = result.refresh_ms {
            refresh_ms = refresh_ms.min(ms);
        }
    }

    if refresh_ms == u64::MAX {
        refresh_ms = fallback_ms;
    }
    // Keep polling within a sane band for E-Ink devices.
    let refresh_ms = refresh_ms.clamp(250, 30_000);

    Frame {
        html,
        signature,
        refresh_ms,
        active,
    }
}
