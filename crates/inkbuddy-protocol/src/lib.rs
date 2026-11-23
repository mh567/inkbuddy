//! Wire protocol shared by the inkbuddy host and its plugins.
//!
//! # Design
//!
//! A plugin is a standalone executable. The host launches it as a child
//! process and talks to it over **newline-delimited JSON** on stdin/stdout:
//! one JSON object per line, no framing, no length prefix. This makes plugins
//! writable in any language (Rust, Python, JS, shell) with no ABI coupling,
//! and a crashing plugin cannot take the host down.
//!
//! The host sends a [`Request`] line and reads exactly one [`Response`] line
//! with the matching `id`. Anything a plugin writes to stdout that is not a
//! response line is ignored; diagnostics belong on stderr.
//!
//! Plugins return **structured content**, not HTML. The host owns the E-Ink
//! page and renders every plugin with the same stylesheet. That is what makes
//! "select one or combine several plugins" work without each plugin having to
//! know about the others. [`Block::Html`] is an escape hatch for plugins that
//! genuinely need custom markup.
//!
//! # Example request (host -> plugin, one line)
//!
//! ```json
//! {"id":1,"method":"render","params":{"width":600,"height":800,"theme":"ink"}}
//! ```
//!
//! # Example response (plugin -> host, one line)
//!
//! ```json
//! {"id":1,"ok":true,"result":{"blocks":[{"type":"lines","lines":[{"text":"Hello","emphasis":"primary","ruby":[]}]}],"refresh_ms":500}}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A single request line written by the host to a plugin's stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Monotonic id; the response must echo it back.
    pub id: u64,
    pub method: Method,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Health check. Plugin replies with `ok: true`.
    Ping,
    /// Return [`PluginInfo`].
    Describe,
    /// Produce the current content. See [`RenderParams`] / [`RenderResult`].
    Render,
    /// Push host-side settings (arbitrary JSON) into the plugin.
    Configure,
    /// Ask the plugin to exit cleanly.
    Shutdown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderParams {
    /// Display area in CSS pixels, when known.
    pub width: u32,
    pub height: u32,
    /// Active theme name, currently always `"ink"`.
    pub theme: String,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A single response line emitted by a plugin on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn success<T: Serialize>(id: u64, result: &T) -> Self {
        Self {
            id,
            ok: true,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn failure(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(message.into()),
        }
    }

    pub fn empty_ok(id: u64) -> Self {
        Self {
            id,
            ok: true,
            result: Some(Value::Null),
            error: None,
        }
    }
}

/// Reply payload for `describe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// How often the host should ask for new content, in milliseconds.
    pub refresh_ms: u64,
    pub protocol: u32,
}

/// Reply payload for `render`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderResult {
    pub blocks: Vec<Block>,
    /// Optional override of the refresh interval for this particular frame.
    pub refresh_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Content model
// ---------------------------------------------------------------------------

/// A self-contained chunk of content contributed by one plugin.
///
/// Internally-tagged so plugin authors can write plain JSON:
/// `{"type":"lines","lines":[...]}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// A group of text lines, e.g. the three-line lyrics view.
    Lines { lines: Vec<Line> },
    /// Label/value pairs, e.g. a clock or system status plugin.
    KeyValue { items: Vec<KeyValue> },
    /// A plain text paragraph.
    Text { text: String },
    /// Escape hatch: raw HTML inserted as-is. Use sparingly; it bypasses the
    /// host stylesheet and may not age well on E-Ink devices.
    Html { html: String },
}

/// Visual weight of a line. The host maps these onto the E-Ink stylesheet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Emphasis {
    /// Largest, bold, full black. The focal line.
    Primary,
    /// Normal body text.
    #[default]
    Normal,
    /// Smaller, grey. Context lines above/below the focal line.
    Muted,
}

/// Per-character reading (Cantonese Jyutping, Japanese furigana, ...).
/// `word` and `reading` are kept as separate fields so a reading can be a
/// multi-letter romanisation while `word` stays a single glyph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ruby {
    pub word: String,
    pub reading: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Line {
    pub text: String,
    pub emphasis: Emphasis,
    /// When non-empty the host renders `text` as per-character cells with the
    /// readings above them on a shared grid, instead of a plain text line.
    pub ruby: Vec<Ruby>,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            text: String::new(),
            emphasis: Emphasis::Normal,
            ruby: Vec::new(),
        }
    }
}

impl Line {
    pub fn new(text: impl Into<String>, emphasis: Emphasis) -> Self {
        Self {
            text: text.into(),
            emphasis,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// `plugin.toml` placed next to every plugin executable. The host scans a
/// plugins directory for these files; there is no compile-time registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub plugin: PluginManifest,
    #[serde(default)]
    pub runtime: RuntimeManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Executable path. Relative paths resolve against the manifest's directory.
    pub command: String,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeManifest {
    pub enabled: bool,
    /// Show this plugin by default when the visitor does not pass `?plugins=`.
    pub default_on: bool,
    /// Upper bound on a single `render` call, in milliseconds.
    pub timeout_ms: u64,
    /// Extra arguments appended to the command line.
    pub args: Vec<String>,
}

impl Default for RuntimeManifest {
    fn default() -> Self {
        Self {
            enabled: true,
            default_on: true,
            timeout_ms: 3000,
            args: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_roundtrip_is_plain_json() {
        let line = Line::new("我会与你变爱侣", Emphasis::Primary);
        let json = serde_json::to_string(&line).unwrap();
        assert!(json.contains("\"primary\""));
        assert!(json.contains("\"ruby\":[]"));
    }

    #[test]
    fn response_echoes_id() {
        let r = Response::empty_ok(7);
        assert_eq!(r.id, 7);
        assert!(r.ok);
    }
}
