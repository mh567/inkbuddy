//! 端到端验证协议循环：describe、未知方法回失败响应、render 输出、
//! shutdown 后退出。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// 起一个线程逐行读 stdout，用 channel 把行交回主线程。
///
/// 主线程据此可以用 `recv_timeout` 等待；插件"活着但不回"时测试会失败，
/// 而不是像直接阻塞 `read_line` 那样把 CI 挂住。
fn spawn_line_reader(stdout: ChildStdout) -> Receiver<String> {
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
fn recv_reply(replies: &Receiver<String>) -> serde_json::Value {
    match replies.recv_timeout(REPLY_TIMEOUT) {
        Ok(line) => serde_json::from_str(line.trim()).expect("response is valid JSON"),
        Err(RecvTimeoutError::Timeout) => panic!("等待响应超时：插件可能不回响应"),
        Err(RecvTimeoutError::Disconnected) => panic!("插件在回响应前关闭了 stdout"),
    }
}

/// 起插件进程，返回进程、stdin 和逐行读取器。
fn spawn_plugin(codex_bin: &str) -> (Child, ChildStdin, Receiver<String>) {
    let bin = env!("CARGO_BIN_EXE_inkbuddy-plugin-ai-quota");
    let mut child = Command::new(bin)
        .env("CODEX_BIN", codex_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn plugin");

    let stdin = child.stdin.take().expect("stdin pipe");
    let replies = spawn_line_reader(child.stdout.take().expect("stdout pipe"));
    (child, stdin, replies)
}

/// 发一行请求并读回一条响应。
fn roundtrip(stdin: &mut ChildStdin, replies: &Receiver<String>, line: &str) -> serde_json::Value {
    writeln!(stdin, "{line}").expect("write request");
    stdin.flush().expect("flush");
    recv_reply(replies)
}

/// 发 shutdown 并等进程退出，断言正常退出。
fn shutdown_plugin(mut child: Child, stdin: &mut ChildStdin, replies: &Receiver<String>) {
    let reply = roundtrip(stdin, replies, r#"{"id":99,"method":"shutdown"}"#);
    assert_eq!(reply["ok"].as_bool(), Some(true));

    let deadline = Instant::now() + Duration::from_secs(5);
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

/// 最小的假 app-server；只实现插件握手和两次读取。
const FAKE_APP_SERVER: &str = r#"#!/usr/bin/env python3
import json
import sys


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def window(used, duration):
    return {"usedPercent": used, "windowDurationMins": duration, "resetsAt": 4102444800}


for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        message = json.loads(raw)
    except ValueError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"id": request_id, "result": {}})
    elif method == "account/read":
        send({"id": request_id, "result": {
            "account": {"type": "chatgpt", "planType": "plus"},
            "requiresOpenaiAuth": False,
        }})
    elif method == "account/rateLimits/read":
        send({"id": request_id, "result": {
            "rateLimits": {
                "planType": "plus",
                "primary": window(0, 300),
                "secondary": window(51, 10080),
            },
            "rateLimitResetCredits": {"availableCount": 2},
        }})
"#;

/// 临时目录里的假 codex 可执行文件，测试结束删除。
struct FakeCodex {
    dir: PathBuf,
}

impl FakeCodex {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "inkbuddy-ai-quota-loop-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(&dir).expect("create temp dir");
        let bin = dir.join("codex");
        fs::write(&bin, FAKE_APP_SERVER).expect("write fake codex");
        let mut permissions = fs::metadata(&bin).expect("stat fake codex").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&bin, permissions).expect("chmod fake codex");
        Self { dir }
    }

    fn bin(&self) -> String {
        self.dir.join("codex").to_string_lossy().into_owned()
    }
}

impl Drop for FakeCodex {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn answers_unknown_method_and_exits_on_shutdown() {
    // 指向不存在的路径，避免测试真的去起本机的 Codex CLI。
    let (mut child, mut stdin, replies) = spawn_plugin("/nonexistent-codex-for-tests");

    // describe：带回 id，并返回插件 id 与名称。
    let reply = roundtrip(&mut stdin, &replies, r#"{"id":1,"method":"describe"}"#);
    assert_eq!(reply["id"].as_u64(), Some(1));
    assert_eq!(reply["ok"].as_bool(), Some(true));
    assert_eq!(reply["result"]["id"].as_str(), Some("ai-quota"));
    assert_eq!(reply["result"]["name"].as_str(), Some("AI 额度"));

    // 未知方法：id 原样带回、ok=false，且进程不退出。
    let reply = roundtrip(&mut stdin, &replies, r#"{"id":9,"method":"future_method"}"#);
    assert_eq!(reply["id"].as_u64(), Some(9));
    assert_eq!(reply["ok"].as_bool(), Some(false));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "未知方法不应让插件退出"
    );

    // 纯非法 JSON 与缺 id：只写 stderr，进程不能退出，后续请求仍要正常。
    writeln!(stdin, "not json").expect("write malformed");
    stdin.flush().expect("flush");
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
fn render_reports_missing_codex() {
    let (child, mut stdin, replies) = spawn_plugin("/nonexistent-codex-for-tests");

    let reply = roundtrip(&mut stdin, &replies, r#"{"id":1,"method":"render","params":{}}"#);
    assert_eq!(reply["ok"].as_bool(), Some(true));
    let blocks = reply["result"]["blocks"].as_array().expect("blocks array");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"].as_str(), Some("html"));
    let html = blocks[0]["html"].as_str().expect("html string");
    assert!(html.contains("quota-notice"), "{html}");
    assert!(html.contains("未找到 Codex CLI"), "{html}");
    assert_eq!(reply["result"]["refresh_ms"].as_u64(), Some(30000));

    shutdown_plugin(child, &mut stdin, &replies);
}

#[test]
fn render_returns_board_from_app_server() {
    let fake = FakeCodex::new();
    let (child, mut stdin, replies) = spawn_plugin(&fake.bin());

    let reply = roundtrip(&mut stdin, &replies, r#"{"id":1,"method":"render","params":{}}"#);
    assert_eq!(reply["ok"].as_bool(), Some(true));
    let blocks = reply["result"]["blocks"].as_array().expect("blocks array");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"].as_str(), Some("html"));
    let html = blocks[0]["html"].as_str().expect("html string");
    assert!(html.contains("ChatGPT"), "{html}");
    assert!(html.contains("Plus · 券 2"), "{html}");
    assert!(html.contains("q-0"), "{html}");
    assert!(html.contains("q-50"), "{html}");
    assert!(html.contains("51%"), "{html}");
    assert_eq!(html.matches("quota-row").count(), 2, "{html}");
    assert_eq!(reply["result"]["refresh_ms"].as_u64(), Some(30000));

    shutdown_plugin(child, &mut stdin, &replies);
}
