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
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

const PLUGIN_ID: &str = "lyrics";
const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "ignoring malformed request line");
                continue;
            }
        };

        let resp = match req.method {
            Method::Ping => Response::empty_ok(req.id),
            Method::Describe => Response::success(
                req.id,
                &PluginInfo {
                    id: PLUGIN_ID.to_string(),
                    name: "歌词".to_string(),
                    version: VERSION.to_string(),
                    description: "正在播放的同步歌词，粤语歌带逐字粤拼".to_string(),
                    refresh_ms: 500,
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
                        id: req.id,
                        ok: true,
                        result: Some(v),
                        error: None,
                    },
                    Err(e) => Response::failure(req.id, e.to_string()),
                }
            }
            Method::Configure => Response::empty_ok(req.id),
            Method::Shutdown => {
                let resp = Response::empty_ok(req.id);
                let out = serde_json::to_string(&resp)?;
                stdout.write_all(format!("{out}\n").as_bytes()).await?;
                stdout.flush().await?;
                break;
            }
        };

        let out = serde_json::to_string(&resp)?;
        stdout.write_all(format!("{out}\n").as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
