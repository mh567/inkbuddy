//! First-party inkbuddy plugin: synced lyrics with per-character Jyutping.
//!
//! Standalone executable. Speaks the inkbuddy newline-delimited JSON protocol
//! on stdin/stdout; diagnostics go to stderr so they never corrupt the stream.

mod mediaremote;
mod nowplaying;
mod qrc;
mod source;
mod state;

use anyhow::Result;
use inkbuddy_protocol::{Method, PluginInfo, RenderParams, Request, Response, PROTOCOL_VERSION};
use serde_json::Value;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

const PLUGIN_ID: &str = "lyrics";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Hard cap on a single request line (protocol contract: 1 MiB).
const MAX_LINE: usize = 1024 * 1024;

#[tokio::main]
async fn main() -> Result<()> {
    // stderr only: stdout is reserved for protocol responses.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "inkbuddy_plugin_lyrics=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cache_dir = std::env::var("INKBUDDY_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_home().join(".inkbuddy"))
        .join("cache")
        .join("lyrics");

    let source = source::LyricsSource::new(cache_dir)?;
    let backend = nowplaying::create_backend();
    let mut state = state::PluginState::new(backend, source);

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF
        }
        if line.len() > MAX_LINE {
            emit(
                &mut stdout,
                &Response::failure(0, "request line exceeds 1 MiB"),
            )
            .await?;
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            emit(&mut stdout, &Response::failure(0, "empty request line")).await?;
            continue;
        }

        let (id, method, req) = match parse_line(trimmed) {
            Ok(v) => v,
            Err((id, msg)) => {
                warn!(id = id, error = %msg, "invalid request line");
                emit(&mut stdout, &Response::failure(id, msg)).await?;
                continue;
            }
        };
        debug!(id = id, method = %method, "request");

        let resp = match req.method {
            Method::Ping => Response::empty_ok(id),
            Method::Describe => Response::success(
                id,
                &PluginInfo {
                    id: PLUGIN_ID.to_string(),
                    name: "歌词".to_string(),
                    version: VERSION.to_string(),
                    description: "正在播放的同步歌词，粤语歌带逐字粤拼".to_string(),
                    protocol: PROTOCOL_VERSION,
                },
            ),
            Method::Render => {
                let params: RenderParams =
                    serde_json::from_value(req.params.clone()).unwrap_or_default();
                debug!(
                    width = params.width,
                    height = params.height,
                    "render requested"
                );
                match serde_json::to_value(state.render().await) {
                    Ok(v) => Response {
                        id,
                        ok: true,
                        result: Some(v),
                        error: None,
                    },
                    Err(e) => Response::failure(id, e.to_string()),
                }
            }
            Method::Configure => {
                if let Some(ms) = req.params.get("latency_ms").and_then(|v| v.as_f64()) {
                    state.set_latency_ms(ms);
                }
                Response::empty_ok(id)
            }
            Method::Shutdown => {
                emit(&mut stdout, &Response::empty_ok(id)).await?;
                break;
            }
        };
        emit(&mut stdout, &resp).await?;
    }

    Ok(())
}

/// Parse one request line on the `Value` path: extract the id and method
/// string first, reject unknown methods without touching the `Method` enum,
/// then rebuild a typed `Request`. Every failure carries the id we managed to
/// read, so the host always gets exactly one response per line.
fn parse_line(line: &str) -> Result<(u64, String, Request), (u64, String)> {
    let value: Value = serde_json::from_str(line).map_err(|e| (0, format!("invalid JSON: {e}")))?;
    let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !is_known_method(&method) {
        return Err((id, format!("unknown method: {method}")));
    }
    let req = serde_json::from_value(value).map_err(|e| (id, format!("invalid request: {e}")))?;
    Ok((id, method, req))
}

fn is_known_method(method: &str) -> bool {
    matches!(
        method,
        "ping" | "describe" | "render" | "configure" | "shutdown"
    )
}

async fn emit(stdout: &mut tokio::io::Stdout, resp: &Response) -> Result<()> {
    let out = serde_json::to_string(resp)?;
    stdout.write_all(format!("{out}\n").as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_extracts_id_and_method() {
        let (id, method, req) =
            parse_line(r#"{"id":7,"method":"render","params":{"width":600}}"#).unwrap();
        assert_eq!(id, 7);
        assert_eq!(method, "render");
        assert_eq!(req.id, 7);
        assert!(matches!(req.method, Method::Render));
    }

    #[test]
    fn parse_line_rejects_unknown_method_with_id() {
        let (id, msg) = parse_line(r#"{"id":42,"method":"frobnicate"}"#).unwrap_err();
        assert_eq!(id, 42);
        assert!(msg.contains("unknown method"));
    }

    #[test]
    fn parse_line_rejects_bad_json_with_zero_id() {
        let (id, msg) = parse_line("not json").unwrap_err();
        assert_eq!(id, 0);
        assert!(msg.contains("invalid JSON"));
    }

    #[test]
    fn known_method_whitelist() {
        assert!(is_known_method("ping"));
        assert!(is_known_method("shutdown"));
        assert!(!is_known_method(""));
        assert!(!is_known_method("Frobnicate"));
    }
}
