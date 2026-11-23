//! Minimal second plugin: current date and time.
//!
//! Useful on its own (`?plugins=clock`) or combined with lyrics. Also serves
//! as the shortest complete example of the inkbuddy plugin protocol.

use anyhow::Result;
use chrono::{Datelike, Local};
use inkbuddy_protocol::{
    Block, Emphasis, KeyValue, Line, Method, PluginInfo, RenderResult, Request, Response,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "clock";

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
            Method::Ping => Response::empty_ok(req.id),
            Method::Describe => Response::success(
                req.id,
                &PluginInfo {
                    id: PLUGIN_ID.to_string(),
                    name: "时钟".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    description: "当前日期与时间".to_string(),
                    refresh_ms: 30_000,
                    protocol: PROTOCOL_VERSION,
                },
            ),
            Method::Render => {
                let now = Local::now();
                let weekday = match now.weekday() {
                    chrono::Weekday::Mon => "星期一",
                    chrono::Weekday::Tue => "星期二",
                    chrono::Weekday::Wed => "星期三",
                    chrono::Weekday::Thu => "星期四",
                    chrono::Weekday::Fri => "星期五",
                    chrono::Weekday::Sat => "星期六",
                    chrono::Weekday::Sun => "星期日",
                };
                let result = RenderResult {
                    blocks: vec![
                        Block::Lines {
                            lines: vec![Line::new(
                                now.format("%H:%M").to_string(),
                                Emphasis::Primary,
                            )],
                        },
                        Block::KeyValue {
                            items: vec![
                                KeyValue {
                                    key: "日期".to_string(),
                                    value: now.format("%Y-%m-%d").to_string(),
                                },
                                KeyValue {
                                    key: "星期".to_string(),
                                    value: weekday.to_string(),
                                },
                            ],
                        },
                    ],
                    refresh_ms: Some(30_000),
                };
                Response::success(req.id, &result)
            }
            Method::Configure => Response::empty_ok(req.id),
            Method::Shutdown => {
                let out = serde_json::to_string(&Response::empty_ok(req.id))?;
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
