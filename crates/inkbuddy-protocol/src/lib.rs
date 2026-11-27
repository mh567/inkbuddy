//! Wire protocol shared by the inkbuddy host and its plugins.
//!
//! # Design
//!
//! A plugin is a standalone executable. The host launches it as a child
//! process and talks to it over **newline-delimited JSON** on stdin/stdout:
//! one JSON object per line. Plugins can be written in any language (Rust,
//! Python, JS, shell) since there is no shared ABI, and a plugin crash stays
//! inside that child process.
//!
//! The host sends a [`Request`] line and reads exactly one [`Response`] line
//! with the matching `id`. Non-response stdout lines are ignored; diagnostics
//! go to stderr. Every request carrying an `id` gets exactly one response
//! line, including unknown methods and invalid fields; otherwise the host
//! waits until its deadline. A line that is not valid JSON or has no `id`
//! is skipped by the plugin, since the host pairs replies by `id` and there
//! is nothing to echo. Hosts send well-formed requests with an `id`.
//!
//! Both sides cap a single line at 1 MiB. `id` is a monotonically increasing
//! `u64` and stays below 2^53 so JSON implementations in other languages can
//! echo it without precision loss.
//!
//! Plugins return **structured content**. The host owns the E-Ink page and
//! renders every plugin with the same stylesheet, so plugins can be selected
//! or combined without knowing about each other. [`Block::Html`] lets a plugin
//! supply custom markup; the host sanitizes it with a tag allowlist.
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
//! {"id":1,"ok":true,"result":{"blocks":[{"type":"lines","lines":[{"text":"Hello","emphasis":"primary"}]}],"refresh_ms":500}}
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
    /// 视图区域选中的子样式。插件用 id 匹配自己声明的 variants；
    /// None 表示用 variants 的第一项。
    pub variant: Option<String>,
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

/// 插件可选的一种子样式。id 用于匹配 render 请求的 `variant`，
/// name 是控制端展示用的名字。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VariantInfo {
    pub id: String,
    pub name: String,
}

/// Reply payload for `describe`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub protocol: u32,
    /// 该插件提供的子样式。空列表表示不可切换样式。
    /// 老插件不返回这个字段，反序列化时按空列表处理。
    pub variants: Vec<VariantInfo>,
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
    #[serde(alias = "kv", alias = "keyvalue")]
    KeyValue { items: Vec<KeyValue> },
    /// A plain text paragraph.
    Text { text: String },
    /// Escape hatch: raw HTML. The host sanitizes it with a tag allowlist
    /// before inserting it into the page.
    ///
    /// The markup can carry two optional attributes for the host's layout
    /// estimates. They are read from the raw markup and stripped by the
    /// sanitizer, so they never reach the page:
    /// - `data-ink-height="<f32>"` on the root element: content height in
    ///   "vh on a square screen" units, used to scale content into a region.
    /// - `data-ink-role="primary|normal|muted|reading"` on a leaf element:
    ///   visual weight of that line in the structured text preview.
    /// Both are optional. Any plugin can use them; the host does not know
    /// any plugin's class names.
    Html { html: String },
    /// Unknown block type from a newer or third-party plugin. The host skips
    /// that block and keeps the rest of the frame.
    #[serde(other)]
    Unknown,
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
    /// Small, grey reading line, e.g. Jyutping. Rendered on its own row.
    Reading,
}

/// An unknown emphasis string from a third-party plugin falls back to Normal,
/// keeping the rest of the `RenderResult` deserializable.
fn de_emphasis<'de, D>(de: D) -> Result<Emphasis, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(de)?;
    Ok(match raw.as_deref() {
        Some("primary") => Emphasis::Primary,
        Some("muted") => Emphasis::Muted,
        Some("reading") => Emphasis::Reading,
        _ => Emphasis::Normal,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Line {
    pub text: String,
    #[serde(deserialize_with = "de_emphasis")]
    pub emphasis: Emphasis,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            text: String::new(),
            emphasis: Emphasis::Normal,
        }
    }
}

impl Line {
    pub fn new(text: impl Into<String>, emphasis: Emphasis) -> Self {
        Self {
            text: text.into(),
            emphasis,
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

/// `plugin.toml` placed next to every plugin executable. The host discovers
/// plugins by scanning directories for these files at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub plugin: PluginManifest,
    #[serde(default)]
    pub runtime: RuntimeManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Show this plugin by default when the visitor omits `?plugins=`.
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
        assert!(!json.contains("ruby"));
    }

    #[test]
    fn response_echoes_id() {
        let r = Response::empty_ok(7);
        assert_eq!(r.id, 7);
        assert!(r.ok);
    }

    #[test]
    fn unknown_block_type_becomes_unknown() {
        let b: Block = serde_json::from_str(r#"{"type":"future_block","data":1}"#).unwrap();
        assert!(matches!(b, Block::Unknown));
    }

    #[test]
    fn keyvalue_aliases_are_accepted() {
        for tag in ["key_value", "kv", "keyvalue"] {
            let s = format!(r#"{{"type":"{tag}","items":[{{"key":"a","value":"b"}}]}}"#);
            let b: Block = serde_json::from_str(&s).unwrap();
            assert!(matches!(b, Block::KeyValue { .. }));
        }
    }

    #[test]
    fn unknown_emphasis_falls_back_to_normal() {
        let l: Line = serde_json::from_str(r#"{"text":"hi","emphasis":"blink"}"#).unwrap();
        assert_eq!(l.emphasis, Emphasis::Normal);
        let l2: Line = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert_eq!(l2.emphasis, Emphasis::Normal);
        let l3: Line = serde_json::from_str(r#"{"text":"hi","emphasis":"muted"}"#).unwrap();
        assert_eq!(l3.emphasis, Emphasis::Muted);
        let l4: Line = serde_json::from_str(r#"{"text":"o nei","emphasis":"reading"}"#).unwrap();
        assert_eq!(l4.emphasis, Emphasis::Reading);
    }

    #[test]
    fn manifest_comparison_covers_display_fields() {
        let make = |name: &str| Manifest {
            plugin: PluginManifest {
                id: "demo".to_string(),
                name: name.to_string(),
                version: "0.1.0".to_string(),
                description: String::new(),
                command: "./demo".to_string(),
            },
            runtime: RuntimeManifest::default(),
        };
        assert_eq!(make("旧名字"), make("旧名字"));
        assert_ne!(make("旧名字"), make("新名字"));
    }
}
