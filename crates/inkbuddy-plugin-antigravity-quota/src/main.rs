//! 反重力额度插件的协议入口。
//!
//! 启动时拉起一个长驻取数服务，`render` 从服务快照读数据渲染 HTML 块；语言
//! 服务器没在跑时返回提示块。日志写 stderr，stdout 只写协议响应。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use inkbuddy_plugin_antigravity_quota::model::Snapshot;
use inkbuddy_plugin_antigravity_quota::render::render_html;
use inkbuddy_plugin_antigravity_quota::service::QuotaService;
use inkbuddy_plugin_antigravity_quota::store;
use inkbuddy_protocol::{
    Block, Method, PluginInfo, RenderResult, Request, Response, PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "antigravity-quota";

/// 后台读取额度的间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// 首次 render 等待快照的上限。
const FIRST_RENDER_WAIT: Duration = Duration::from_secs(3);
/// 页面刷新间隔，只用来让倒计时保持准确；取数由服务定时器驱动。
const REFRESH_MS: u64 = 30_000;

/// 插件运行状态：额度服务与「是否已经等过首轮」。
struct Board {
    service: QuotaService,
    served_first: bool,
}

impl Board {
    fn new() -> Self {
        Self {
            service: QuotaService::start(POLL_INTERVAL, store::default_path()),
            served_first: false,
        }
    }

    /// 首次 render 等首轮数据，之后只读快照。
    async fn render(&mut self) -> String {
        let snapshot: Snapshot = if self.served_first {
            self.service.snapshot().borrow().clone()
        } else {
            self.served_first = true;
            self.service.wait_first(FIRST_RENDER_WAIT).await
        };
        render_html(&snapshot, unix_now())
    }
}

/// 一行输入解析后的结果。
#[derive(Debug)]
enum ParsedRequest {
    /// 合法请求，走正常处理分支。
    Request(Request),
    /// 拿得到 id 但 method 未识别或字段非法，需要回一条失败响应。
    Bad { id: u64, error: String },
    /// 连 id 都取不到（非法 JSON 或缺 id），无法回响应，只能跳过。
    Ignore,
}

/// 解析宿主写入的一行请求。区分非法 JSON、缺 id 和可回失败响应的坏请求，
/// 避免宿主一直等到超时。
fn parse_request_line(line: &str) -> ParsedRequest {
    let value = match serde_json::from_str::<serde_json::Value>(line.trim()) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("忽略无法解析的请求行");
            return ParsedRequest::Ignore;
        }
    };
    let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) else {
        eprintln!("忽略缺少 id 的请求行");
        return ParsedRequest::Ignore;
    };
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing>")
        .to_string();
    match serde_json::from_value::<Request>(value) {
        Ok(req) => ParsedRequest::Request(req),
        Err(_) => ParsedRequest::Bad {
            id,
            error: format!("unsupported method or invalid request: {method}"),
        },
    }
}

/// 处理一条合法请求，产出对应响应。
async fn handle_request(req: Request, board: &mut Board) -> Response {
    match req.method {
        Method::Describe => {
            // 用 JSON 构造 PluginInfo，兼容提交态与工作区两种协议定义。
            match serde_json::from_value::<PluginInfo>(serde_json::json!({
                "id": PLUGIN_ID,
                "name": "反重力额度",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "Antigravity 套餐与 5 小时、周额度用量",
                "protocol": PROTOCOL_VERSION,
            })) {
                Ok(info) => Response::success(req.id, &info),
                Err(e) => Response::failure(req.id, format!("describe: {e}")),
            }
        }
        Method::Render => {
            let html = board.render().await;
            let result = RenderResult {
                blocks: vec![Block::Html { html }],
                refresh_ms: Some(REFRESH_MS),
            };
            Response::success(req.id, &result)
        }
        Method::Ping | Method::Configure | Method::Shutdown => Response::empty_ok(req.id),
    }
}

/// 把一条响应写成一行 JSON 并 flush。
async fn write_response<W: AsyncWrite + Unpin>(stdout: &mut W, resp: &Response) -> Result<()> {
    let out = serde_json::to_string(resp)?;
    stdout.write_all(format!("{out}\n").as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut board = Board::new();

    while let Some(line) = reader.next_line().await? {
        let (resp, shutdown) = match parse_request_line(&line) {
            ParsedRequest::Request(req) => {
                let shutdown = req.method == Method::Shutdown;
                (handle_request(req, &mut board).await, shutdown)
            }
            ParsedRequest::Bad { id, error } => (Response::failure(id, error), false),
            ParsedRequest::Ignore => continue,
        };
        write_response(&mut stdout, &resp).await?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_line_handles_valid_unknown_and_invalid() {
        match parse_request_line(r#"{"id":1,"method":"render"}"#) {
            ParsedRequest::Request(req) => {
                assert_eq!(req.id, 1);
                assert_eq!(req.method, Method::Render);
            }
            other => panic!("expected Request, got {other:?}"),
        }

        match parse_request_line(r#"{"id":9,"method":"future_method"}"#) {
            ParsedRequest::Bad { id, error } => {
                assert_eq!(id, 9);
                assert_eq!(
                    error,
                    "unsupported method or invalid request: future_method"
                );
            }
            other => panic!("expected Bad, got {other:?}"),
        }

        assert!(matches!(
            parse_request_line("not json"),
            ParsedRequest::Ignore
        ));
        assert!(matches!(
            parse_request_line(r#"{"method":"render"}"#),
            ParsedRequest::Ignore
        ));
    }

    #[test]
    fn plugin_id_matches_manifest() {
        let manifest = include_str!("../../../plugins/antigravity-quota/plugin.toml");
        assert!(
            manifest.contains(&format!("id = \"{PLUGIN_ID}\"")),
            "PLUGIN_ID 必须与 plugins/antigravity-quota/plugin.toml 的 id 一致"
        );
    }
}
