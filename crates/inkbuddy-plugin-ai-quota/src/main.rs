//! AI 额度插件的协议入口。
//!
//! 当前只是让 crate 可编译的最小循环：describe 返回插件信息，
//! 其余方法回空成功响应。app-server 客户端与正式协议循环在后续任务接入。

use anyhow::Result;
use inkbuddy_protocol::{Method, PluginInfo, Request, Response, PROTOCOL_VERSION};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "ai-quota";

#[tokio::main]
async fn main() -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let resp = match req.method {
            Method::Describe => Response::success(
                req.id,
                &PluginInfo {
                    id: PLUGIN_ID.to_string(),
                    name: "AI 额度".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    description: "Codex 套餐与 5 小时、周额度用量".to_string(),
                    protocol: PROTOCOL_VERSION,
                    variants: Vec::new(),
                },
            ),
            _ => Response::empty_ok(req.id),
        };

        let out = serde_json::to_string(&resp)?;
        stdout.write_all(format!("{out}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}
