//! Command Code 额度插件的协议入口：长驻服务、快照渲染与协议循环。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use inkbuddy_plugin_command_code::api;
use inkbuddy_plugin_command_code::auth;
use inkbuddy_plugin_command_code::model::{QuotaSnapshot, LOGIN_REQUIRED};
use inkbuddy_plugin_command_code::render::render_html;
use inkbuddy_plugin_command_code::service::QuotaService;
use inkbuddy_protocol::{
    Block, Method, PluginInfo, RenderResult, Request, Response, PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "command-code";

/// 后台读取额度的间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// 首次 render 等待快照的上限。
const FIRST_RENDER_WAIT: Duration = Duration::from_secs(3);
/// 页面刷新间隔，只用来让倒计时保持准确；取数由服务定时器驱动。
const REFRESH_MS: u64 = 30_000;
/// 测试钩子：把取数地址覆盖到本机假 server；只允许 loopback。
const BASE_URL_ENV: &str = "COMMAND_CODE_BASE_URL";

/// 插件运行状态：额度服务与「是否已经等过首轮」。
struct Board {
    service: Option<QuotaService>,
    served_first: bool,
}

impl Board {
    fn new() -> Self {
        Self {
            service: auth::api_key()
                .map(|key| QuotaService::start_with_base(key, resolve_base(), POLL_INTERVAL)),
            served_first: false,
        }
    }

    /// 当前快照；没有服务时返回「未登录」终态。
    fn current(&self) -> QuotaSnapshot {
        match &self.service {
            Some(service) => service.snapshot().borrow().clone(),
            None => QuotaSnapshot {
                failure: Some(LOGIN_REQUIRED.to_string()),
                ..QuotaSnapshot::default()
            },
        }
    }

    /// 首次有服务时等首轮数据，之后只读快照。
    async fn render(&mut self) -> String {
        let snapshot = if !self.served_first {
            self.served_first = true;
            match &self.service {
                Some(service) => service.wait_first(FIRST_RENDER_WAIT).await,
                None => self.current(),
            }
        } else {
            self.current()
        };
        render_html(&snapshot, unix_now())
    }
}

/// 解析取数地址：环境变量只接受本机 loopback，其余情况固定走生产地址。
fn resolve_base() -> String {
    let overridden = std::env::var(BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    resolve_base_from(overridden)
}

fn resolve_base_from(overridden: Option<String>) -> String {
    match overridden {
        Some(value) if api::is_loopback_base(&value) => value,
        Some(_) => {
            eprintln!("command-code: 取数地址覆盖值不在允许范围，改用生产地址");
            api::BASE_URL.to_string()
        }
        None => api::BASE_URL.to_string(),
    }
}

/// 一行输入解析后的结果。
#[derive(Debug)]
enum ParsedRequest {
    /// 合法请求，走正常处理分支。
    Request(Request),
    /// 拿得到 id 但 method 未识别或字段非法，需要回一条失败响应。
    Bad { id: u64, error: String },
    /// 连 id 都取不到，这一行没有对应的响应，主循环跳过。
    Ignore,
}

/// 解析宿主写入的一行请求。区分非法 JSON、缺 id 和可回失败响应的坏请求；
/// 宿主按 id 匹配响应，缺 id 的行没有可回的响应。
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
    // 先取原始 method（缺失记 <missing>），失败文案带上它便于宿主定位版本差异。
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<missing>")
        .to_string();
    match serde_json::from_value::<Request>(value) {
        Ok(req) => ParsedRequest::Request(req),
        // 有 id 就能回：未知 method 或字段非法都归为这条失败响应。
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
            // 按协议结构构造 PluginInfo，variants 留空。
            match serde_json::from_value::<PluginInfo>(serde_json::json!({
                "id": PLUGIN_ID,
                "name": "Command Code 额度",
                "version": env!("CARGO_PKG_VERSION"),
                "description": "Command Code 套餐与 5 小时、周、月额度用量",
                "protocol": PROTOCOL_VERSION,
            })) {
                Ok(info) => Response::success(req.id, &info),
                // 构造失败时回一条失败响应，让宿主继续。
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
async fn write_response<W>(stdout: &mut W, resp: &Response) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
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

        // 宿主读到响应行就返回，随即 SIGKILL 还活着的插件，所以收尾必须在
        // 写响应之前做完，否则后台取数任务没有机会退出。
        if shutdown {
            if let Some(service) = board.service.take() {
                service.shutdown().await;
            }
        }
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
        match parse_request_line(r#"{"id":1,"method":"describe"}"#) {
            ParsedRequest::Request(req) => {
                assert_eq!(req.id, 1);
                assert_eq!(req.method, Method::Describe);
            }
            other => panic!("expected Request, got {other:?}"),
        }

        // 未知 method：保留 id，失败文案带上原始 method。
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

        // 缺 method 时失败文案记 <missing>。
        match parse_request_line(r#"{"id":4}"#) {
            ParsedRequest::Bad { id, error } => {
                assert_eq!(id, 4);
                assert_eq!(error, "unsupported method or invalid request: <missing>");
            }
            other => panic!("expected Bad, got {other:?}"),
        }

        // 纯非法 JSON 与缺少 id 都无法回响应，只能忽略。
        assert!(matches!(
            parse_request_line("not json"),
            ParsedRequest::Ignore
        ));
        assert!(matches!(
            parse_request_line(r#"{"method":"describe"}"#),
            ParsedRequest::Ignore
        ));
        assert!(matches!(parse_request_line(""), ParsedRequest::Ignore));
    }

    #[test]
    fn plugin_id_matches_manifest() {
        let manifest = include_str!("../../../plugins/command-code/plugin.toml");
        assert!(
            manifest.contains(&format!("id = \"{PLUGIN_ID}\"")),
            "PLUGIN_ID 必须与 plugins/command-code/plugin.toml 的 id 一致"
        );
    }

    #[test]
    fn base_override_only_accepts_loopback() {
        assert_eq!(resolve_base_from(None), api::BASE_URL);
        assert_eq!(resolve_base_from(Some(String::new())), api::BASE_URL);
        assert_eq!(
            resolve_base_from(Some("http://127.0.0.1:9012".to_string())),
            "http://127.0.0.1:9012"
        );
        assert_eq!(
            resolve_base_from(Some("http://localhost:8080".to_string())),
            "http://localhost:8080"
        );
        // 生产地址、远程域名、带 userinfo 的地址都回退到生产常量。
        for value in [
            api::BASE_URL,
            "https://attacker.example",
            "http://user:pass@127.0.0.1:9012",
            "https://api.commandcode.ai/evil",
        ] {
            assert_eq!(
                resolve_base_from(Some(value.to_string())),
                api::BASE_URL,
                "value {value}"
            );
        }
    }
}
