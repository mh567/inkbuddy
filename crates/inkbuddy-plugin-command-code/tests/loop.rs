//! 端到端验证协议循环：describe、render、坏请求、非法输入与 shutdown。
//!
//! 成功渲染用本地假 HTTP server 提供四个只读接口的响应；测试不访问真实域名。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
// 插件首轮最多等 3 秒，这里给响应留足余量，避免 CI 负载高时刚好卡在 3 秒边界。
const FIRST_RENDER_TIMEOUT: Duration = Duration::from_secs(5);
const EMPTY_HOME: &str = "/nonexistent-home-for-command-code-tests";

/// 起一个线程逐行读 stdout，用 channel 把行交回主线程。
fn spawn_line_reader(stdout: ChildStdout) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// 在超时内读一条响应并解析成 JSON；超时或提前 EOF 都直接让测试失败。
fn recv_reply(replies: &mpsc::Receiver<String>, timeout: Duration) -> serde_json::Value {
    match replies.recv_timeout(timeout) {
        Ok(line) => serde_json::from_str(line.trim()).expect("response is valid JSON"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("等待响应超时：插件可能不回响应"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("插件在回响应前关闭了 stdout"),
    }
}

/// 起插件进程；默认清空凭据与 base 覆盖，避免读到本机登录态或访问外部域名。
fn spawn_plugin(envs: &[(&str, &str)]) -> (Child, ChildStdin, mpsc::Receiver<String>) {
    let bin = env!("CARGO_BIN_EXE_inkbuddy-plugin-command-code");
    let mut command = Command::new(bin);
    command
        .env_remove("COMMAND_CODE_API_KEY")
        .env_remove("COMMAND_CODE_BASE_URL")
        .env("HOME", EMPTY_HOME)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn plugin");
    let stdin = child.stdin.take().expect("stdin pipe");
    let replies = spawn_line_reader(child.stdout.take().expect("stdout pipe"));
    (child, stdin, replies)
}

/// 发一行请求并读回一条响应。
fn roundtrip(
    stdin: &mut ChildStdin,
    replies: &mpsc::Receiver<String>,
    line: &str,
) -> serde_json::Value {
    writeln!(stdin, "{line}").expect("write request");
    stdin.flush().expect("flush");
    recv_reply(replies, REPLY_TIMEOUT)
}

/// 发 shutdown 并等进程退出，断言正常退出。
fn shutdown_plugin(mut child: Child, stdin: &mut ChildStdin, replies: &mpsc::Receiver<String>) {
    let reply = roundtrip(stdin, replies, r#"{"id":99,"method":"shutdown"}"#);
    assert_eq!(reply["ok"].as_bool(), Some(true));

    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "插件应正常退出");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("shutdown 后插件未退出");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// 本地假 API：只服务四个只读 GET，base 由测试环境变量注入插件。
struct FakeApi {
    base: String,
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FakeApi {
    fn new() -> Self {
        Self::with_auth_failure(false)
    }

    /// 让 whoami 返回 401，用于验证鉴权失败的终态提示。
    fn with_auth_failure(auth_failure: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake api");
        let addr = listener.local_addr().expect("local addr");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        // 保持阻塞式 accept：非阻塞监听器接受的 socket 在某些平台上也是非阻塞的，
        // 请求头尚未到达时读一行会提前返回，导致连接被误关。
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // 订阅与 credit 两路请求并发到达，逐连接处理避免互相阻塞。
                        thread::spawn(move || serve_connection(stream, auth_failure));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base: format!("http://{addr}"),
            addr,
            stop,
            worker: Some(worker),
        }
    }

    fn base(&self) -> &str {
        &self.base
    }
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 连一下监听器，把阻塞中的 accept 唤醒。
        let _ = std::net::TcpStream::connect(self.addr);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 按路径返回固定的接口响应；未知路径回空对象，便于发现路径拼接错误。
fn serve_connection(mut stream: std::net::TcpStream, auth_failure: bool) {
    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut header = String::new();
    loop {
        header.clear();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    drop(reader);
    let body = if path.starts_with("/alpha/whoami") && auth_failure {
        // 401 由下面统一的状态码处理，body 仅占位。
        "{}"
    } else if path.starts_with("/alpha/whoami") {
        r#"{"user":{"id":"test-user"}}"#
    } else if path.starts_with("/alpha/billing/subscriptions") {
        r#"[{"planId":"GOAT","status":"active","currentPeriodStart":1700000000,"currentPeriodEnd":4102444800}]"#
    } else if path.starts_with("/alpha/billing/credits") {
        r#"{"credits":{"monthlyCredits":50,"purchasedCredits":10,"freeCredits":2.4},"windowLimits":{"fiveHour":{"used":7,"cap":14,"resetAt":4102444800},"weekly":{"used":14,"cap":70,"resetAt":4102444800}}}"#
    } else if path.starts_with("/alpha/usage/summary") {
        r#"{"summary":{"totalCount":18,"totalCost":7.6,"totalTokens":42000}}"#
    } else {
        "{}"
    };
    let status = if auth_failure && path.starts_with("/alpha/whoami") {
        "401 Unauthorized"
    } else {
        "200 OK"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[test]
fn describe_returns_plugin_info() {
    let (child, mut stdin, replies) = spawn_plugin(&[]);
    let reply = roundtrip(&mut stdin, &replies, r#"{"id":1,"method":"describe"}"#);
    assert_eq!(reply["id"].as_u64(), Some(1));
    assert_eq!(reply["ok"].as_bool(), Some(true));
    assert_eq!(reply["result"]["id"].as_str(), Some("command-code"));
    assert_eq!(reply["result"]["name"].as_str(), Some("Command Code 额度"));
    assert_eq!(reply["result"]["protocol"].as_u64(), Some(1));
    assert!(!reply["result"]["version"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    let variants = reply["result"]["variants"]
        .as_array()
        .expect("variants array");
    assert!(variants.is_empty(), "variants 应为空");

    shutdown_plugin(child, &mut stdin, &replies);
}

#[test]
fn render_reports_login_required_without_key() {
    let (child, mut stdin, replies) = spawn_plugin(&[]);
    let started = Instant::now();
    let reply = roundtrip(
        &mut stdin,
        &replies,
        r#"{"id":1,"method":"render","params":{}}"#,
    );
    assert!(
        started.elapsed() < FIRST_RENDER_TIMEOUT,
        "未登录时首轮 render 应立即返回"
    );
    assert_eq!(reply["ok"].as_bool(), Some(true));
    let blocks = reply["result"]["blocks"].as_array().expect("blocks array");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"].as_str(), Some("html"));
    let html = blocks[0]["html"].as_str().expect("html string");
    assert!(html.contains("quota-notice"), "{html}");
    assert!(html.contains("Command Code 未登录"), "{html}");
    assert!(!html.contains("quota-row"), "{html}");
    assert_eq!(reply["result"]["refresh_ms"].as_u64(), Some(30000));

    shutdown_plugin(child, &mut stdin, &replies);
}

#[test]
fn render_returns_board_from_local_fake_api() {
    let fake = FakeApi::new();
    let (child, mut stdin, replies) = spawn_plugin(&[
        ("COMMAND_CODE_API_KEY", "test-key"),
        ("COMMAND_CODE_BASE_URL", fake.base()),
    ]);

    let started = Instant::now();
    writeln!(stdin, r#"{{"id":1,"method":"render","params":{{}}}}"#).expect("write render");
    stdin.flush().expect("flush");
    let reply = recv_reply(&replies, FIRST_RENDER_TIMEOUT);
    assert!(
        started.elapsed() < FIRST_RENDER_TIMEOUT,
        "首轮 render 应在 5 秒内返回"
    );
    assert_eq!(reply["ok"].as_bool(), Some(true));
    let blocks = reply["result"]["blocks"].as_array().expect("blocks array");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"].as_str(), Some("html"));
    let html = blocks[0]["html"].as_str().expect("html string");
    assert!(html.contains("<div class=\"quota\" data-ink-height="), "{html}");
    assert!(html.contains("GOAT · 余 62.4"), "{html}");
    assert_eq!(html.matches("quota-row").count(), 3, "{html}");
    // 月行只算套餐 credit：池子 70、monthlyCredits 50，得 29%；额外 credit 只进顶部总额。
    assert!(html.contains("50%"), "{html}");
    assert!(html.contains("20%"), "{html}");
    assert!(html.contains("29%"), "{html}");
    assert!(!html.contains("quota-notice"), "{html}");
    assert_eq!(reply["result"]["refresh_ms"].as_u64(), Some(30000));

    shutdown_plugin(child, &mut stdin, &replies);
}

#[test]
fn auth_failure_renders_login_notice() {
    let fake = FakeApi::with_auth_failure(true);
    let (child, mut stdin, replies) = spawn_plugin(&[
        ("COMMAND_CODE_API_KEY", "test-key"),
        ("COMMAND_CODE_BASE_URL", fake.base()),
    ]);

    let reply = roundtrip(
        &mut stdin,
        &replies,
        r#"{"id":1,"method":"render","params":{}}"#,
    );
    assert_eq!(reply["ok"].as_bool(), Some(true));
    let html = reply["result"]["blocks"][0]["html"]
        .as_str()
        .expect("html string");
    assert!(html.contains("quota-notice"), "{html}");
    assert!(html.contains("Command Code 未登录"), "{html}");
    assert!(!html.contains("quota-row"), "{html}");

    shutdown_plugin(child, &mut stdin, &replies);
}

#[test]
fn unknown_method_and_malformed_lines_do_not_block_the_loop() {
    let (mut child, mut stdin, replies) = spawn_plugin(&[]);

    let reply = roundtrip(&mut stdin, &replies, r#"{"id":9,"method":"future_method"}"#);
    assert_eq!(reply["id"].as_u64(), Some(9));
    assert_eq!(reply["ok"].as_bool(), Some(false));
    assert!(
        reply["error"]
            .as_str()
            .unwrap_or_default()
            .contains("future_method"),
        "失败文案应带原始 method"
    );
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "未知方法不应让插件退出"
    );

    writeln!(stdin, "not json").expect("write malformed");
    writeln!(stdin, r#"{{"method":"describe"}}"#).expect("write missing id");
    stdin.flush().expect("flush");
    thread::sleep(Duration::from_millis(100));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "非法输入不应让插件退出"
    );

    let reply = roundtrip(&mut stdin, &replies, r#"{"id":10,"method":"describe"}"#);
    assert_eq!(reply["id"].as_u64(), Some(10));
    assert_eq!(reply["ok"].as_bool(), Some(true));

    shutdown_plugin(child, &mut stdin, &replies);
}

#[test]
fn shutdown_replies_before_exit_with_background_service() {
    let fake = FakeApi::new();
    let (child, mut stdin, replies) = spawn_plugin(&[
        ("COMMAND_CODE_API_KEY", "test-key"),
        ("COMMAND_CODE_BASE_URL", fake.base()),
    ]);

    let reply = roundtrip(
        &mut stdin,
        &replies,
        r#"{"id":1,"method":"render","params":{}}"#,
    );
    assert_eq!(reply["ok"].as_bool(), Some(true));

    shutdown_plugin(child, &mut stdin, &replies);
}
