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
            Method::Describe => {
                // 用 JSON 构造 PluginInfo：提交态协议没有 variants 字段也没有
                // Default，工作区协议两者都有，这样写对两种版本都能编译。
                // TODO: 协议 variants/Default 合并进提交态后，改回结构体字面量。
                match serde_json::from_value::<PluginInfo>(serde_json::json!({
                    "id": PLUGIN_ID,
                    "name": "AI 额度",
                    "version": env!("CARGO_PKG_VERSION"),
                    "description": "Codex 套餐与 5 小时、周额度用量",
                    "protocol": PROTOCOL_VERSION,
                })) {
                    Ok(info) => Response::success(req.id, &info),
                    // 不因构造失败杀进程，回一条失败响应让宿主继续。
                    Err(e) => Response::failure(req.id, format!("describe: {e}")),
                }
            }
            _ => Response::empty_ok(req.id),
        };

        let out = serde_json::to_string(&resp)?;
        stdout.write_all(format!("{out}\n").as_bytes()).await?;
        stdout.flush().await?;

        // 回完 shutdown 响应就退出，避免把长驻子进程留在后台。
        if req.method == Method::Shutdown {
            break;
        }
    }
    Ok(())
}
