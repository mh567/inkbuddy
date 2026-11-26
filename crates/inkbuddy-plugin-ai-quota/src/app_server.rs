//! Codex app-server 客户端：后台握手、读取额度、失败重连，向外发布快照。
//!
//! 服务在启动时立刻 spawn 一个 tokio 任务，调用方拿到的是 `watch` 通道，
//! 不阻塞宿主。任务内部再启动 `codex app-server --listen stdio://`，失败时
//! 按 1 秒到 30 秒的退避重连，并保留最后一次成功快照。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::model::QuotaSnapshot;
use crate::rpc::{apply_account, apply_rate_limits, parse_message, RpcMessage};

/// 单次 JSON-RPC 请求的等待上限。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// shutdown 时等待后台任务收尾的上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

const CLIENT_NAME: &str = "inkbuddy-ai-quota";
const CLIENT_TITLE: &str = "InkBuddy AI Quota";

/// 找不到可执行文件时的提示。
const FAILURE_NOT_FOUND: &str = "未找到 Codex CLI";
/// 其它取数失败（含进程退出、握手失败、超时）的提示。
const FAILURE_READ: &str = "额度读取失败";

/// 下一次重连等待：从 1 秒起翻倍，最多 30 秒。
pub fn next_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        return INITIAL_BACKOFF;
    }
    current.saturating_mul(2).min(MAX_BACKOFF)
}

/// 长驻额度服务；调用方只持有 `watch` 接收端。
pub struct QuotaService {
    rx: watch::Receiver<QuotaSnapshot>,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl QuotaService {
    /// 立刻启动后台任务，不阻塞调用方。
    pub fn start(codex_bin: PathBuf, poll_interval: Duration) -> Self {
        let (tx, rx) = watch::channel(QuotaSnapshot::default());
        let (stop, stop_rx) = oneshot::channel();
        let handle = tokio::spawn(run(codex_bin, poll_interval, tx, stop_rx));
        Self {
            rx,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    pub fn snapshot(&self) -> watch::Receiver<QuotaSnapshot> {
        self.rx.clone()
    }

    /// 等首轮结果：已有数据或失败原因就立即返回，否则等到超时。
    pub async fn wait_first(&self, timeout: Duration) -> QuotaSnapshot {
        let mut rx = self.rx.clone();
        if is_ready(&rx.borrow()) {
            return rx.borrow().clone();
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {
                    let snapshot = rx.borrow_and_update().clone();
                    if is_ready(&snapshot) {
                        return snapshot;
                    }
                }
                Ok(Err(_)) | Err(_) => return rx.borrow().clone(),
            }
        }
    }

    /// 通知后台任务退出，最多等 2 秒；子进程靠 `kill_on_drop` 收掉。
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut handle) = self.handle.take() {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
            }
        }
    }
}

fn is_ready(snapshot: &QuotaSnapshot) -> bool {
    snapshot.updated_at.is_some() || snapshot.failure.is_some()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

fn publish_failure(tx: &watch::Sender<QuotaSnapshot>, message: &str) {
    let mut snapshot = tx.borrow().clone();
    snapshot.failure = Some(message.to_string());
    let _ = tx.send_replace(snapshot);
}

/// 单次请求的结局。
enum RequestOutcome {
    /// 收到匹配 id 的响应。
    Result(Result<Value, String>),
    /// 收到 stop 信号。
    Stopped,
    /// IO 失败、进程退出或超时。
    Failed,
}

/// 一次连接的结局。
enum SessionEnd {
    /// 收到 stop 信号。
    Stopped,
    /// 连接结束；`reset_backoff` 表示结束前读到过完整数据，用于重置退避。
    Ended { reset_backoff: bool },
}

async fn run(
    codex_bin: PathBuf,
    poll_interval: Duration,
    tx: watch::Sender<QuotaSnapshot>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let reset_backoff = match run_session(&codex_bin, poll_interval, &tx, &mut stop).await {
            SessionEnd::Stopped => return,
            SessionEnd::Ended { reset_backoff } => reset_backoff,
        };
        if reset_backoff {
            backoff = INITIAL_BACKOFF;
        }
        tokio::select! {
            _ = &mut stop => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = next_backoff(backoff);
    }
}

async fn run_session(
    codex_bin: &Path,
    poll_interval: Duration,
    tx: &watch::Sender<QuotaSnapshot>,
    stop: &mut oneshot::Receiver<()>,
) -> SessionEnd {
    let mut command = Command::new(codex_bin);
    command
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = if err.kind() == std::io::ErrorKind::NotFound {
                FAILURE_NOT_FOUND
            } else {
                FAILURE_READ
            };
            eprintln!("ai-quota: 启动 codex app-server 失败: {err}");
            publish_failure(tx, message);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
    };

    let Some(stdin) = child.stdin.take() else {
        eprintln!("ai-quota: app-server 缺少 stdin 管道");
        publish_failure(tx, FAILURE_READ);
        return SessionEnd::Ended {
            reset_backoff: false,
        };
    };
    let Some(stdout) = child.stdout.take() else {
        eprintln!("ai-quota: app-server 缺少 stdout 管道");
        publish_failure(tx, FAILURE_READ);
        return SessionEnd::Ended {
            reset_backoff: false,
        };
    };

    let mut session = Session {
        stdin,
        lines: BufReader::new(stdout).lines(),
        snapshot: tx.borrow().clone(),
        tx,
        // 持有 Child 以免 `kill_on_drop` 提前生效；离开作用域即回收子进程。
        _child: child,
    };

    let handshake = json!({
        "clientInfo": {
            "name": CLIENT_NAME,
            "title": CLIENT_TITLE,
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    match session.request(1, "initialize", handshake, stop).await {
        RequestOutcome::Result(Ok(_)) => {}
        RequestOutcome::Result(Err(message)) => {
            eprintln!("ai-quota: initialize 失败: {message}");
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
        RequestOutcome::Stopped => return SessionEnd::Stopped,
        RequestOutcome::Failed => {
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
    }
    if !session.send(&json!({"method": "initialized"})).await {
        publish_failure(tx, FAILURE_READ);
        return SessionEnd::Ended {
            reset_backoff: false,
        };
    }

    match session.request(2, "account/read", json!({}), stop).await {
        RequestOutcome::Result(Ok(value)) => {
            apply_account(&mut session.snapshot, &value, unix_now());
            if matches!(value.get("account"), Some(Value::Null)) {
                // 未登录时不必再读额度；发布失败快照后等用户登录再重连。
                session.publish();
                return SessionEnd::Ended {
                    reset_backoff: false,
                };
            }
        }
        RequestOutcome::Result(Err(message)) => {
            eprintln!("ai-quota: account/read 失败: {message}");
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
        RequestOutcome::Stopped => return SessionEnd::Stopped,
        RequestOutcome::Failed => {
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
    }

    let mut next_id = 4;
    match session
        .request(3, "account/rateLimits/read", json!({}), stop)
        .await
    {
        RequestOutcome::Result(Ok(value)) => {
            apply_rate_limits(&mut session.snapshot, &value, unix_now());
            // 账号与额度都到手后统一发布，避免首轮只拿到半份数据。
            session.publish();
        }
        RequestOutcome::Result(Err(message)) => {
            eprintln!("ai-quota: account/rateLimits/read 失败: {message}");
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
        RequestOutcome::Stopped => return SessionEnd::Stopped,
        RequestOutcome::Failed => {
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: false,
            };
        }
    }

    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一拍立即触发，跳过，避免刚读完就再读一次。
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = &mut *stop => return SessionEnd::Stopped,
            _ = ticker.tick() => {
                match session.request(next_id, "account/rateLimits/read", json!({}), stop).await {
                    RequestOutcome::Result(Ok(value)) => {
                        apply_rate_limits(&mut session.snapshot, &value, unix_now());
                        session.publish();
                        next_id += 1;
                    }
                    RequestOutcome::Result(Err(message)) => {
                        eprintln!("ai-quota: 轮询额度失败: {message}");
                        publish_failure(tx, FAILURE_READ);
                        // 首轮已成功过，重连退避回到 1 秒。
                        return SessionEnd::Ended { reset_backoff: true };
                    }
                    RequestOutcome::Stopped => return SessionEnd::Stopped,
                    RequestOutcome::Failed => {
                        publish_failure(tx, FAILURE_READ);
                        return SessionEnd::Ended { reset_backoff: true };
                    }
                }
            }
            line = session.lines.next_line() => {
                match line {
                    Ok(Some(line)) => match parse_message(&line) {
                        RpcMessage::Notification { method, params } => {
                            session.apply_notification(&method, &params);
                        }
                        RpcMessage::Response { .. } | RpcMessage::Unknown => {}
                    },
                    Ok(None) => {
                        eprintln!("ai-quota: app-server 已退出");
                        publish_failure(tx, FAILURE_READ);
                        return SessionEnd::Ended { reset_backoff: true };
                    }
                    Err(err) => {
                        eprintln!("ai-quota: 读取 app-server 输出失败: {err}");
                        publish_failure(tx, FAILURE_READ);
                        return SessionEnd::Ended { reset_backoff: true };
                    }
                }
            }
        }
    }
}

/// 一条已建立的连接。
struct Session<'a> {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    snapshot: QuotaSnapshot,
    tx: &'a watch::Sender<QuotaSnapshot>,
    _child: Child,
}

impl Session<'_> {
    /// 写一行 JSON；失败只记日志，由调用方按重连处理。
    async fn send(&mut self, message: &Value) -> bool {
        let mut line = match serde_json::to_string(message) {
            Ok(line) => line,
            Err(err) => {
                eprintln!("ai-quota: 序列化请求失败: {err}");
                return false;
            }
        };
        line.push('\n');
        if let Err(err) = self.stdin.write_all(line.as_bytes()).await {
            eprintln!("ai-quota: 写入 app-server 失败: {err}");
            return false;
        }
        if let Err(err) = self.stdin.flush().await {
            eprintln!("ai-quota: 刷新 app-server stdin 失败: {err}");
            return false;
        }
        true
    }

    fn publish(&self) {
        let _ = self.tx.send_replace(self.snapshot.clone());
    }

    fn apply_notification(&mut self, method: &str, params: &Value) {
        let now = unix_now();
        match method {
            "account/updated" => apply_account(&mut self.snapshot, params, now),
            "account/rateLimits/updated" => apply_rate_limits(&mut self.snapshot, params, now),
            _ => return,
        }
        self.publish();
    }

    /// 发请求并等待匹配 id 的响应；期间收到的通知先应用。
    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        stop: &mut oneshot::Receiver<()>,
    ) -> RequestOutcome {
        if !self
            .send(&json!({"id": id, "method": method, "params": params}))
            .await
        {
            return RequestOutcome::Failed;
        }

        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        loop {
            let line = tokio::select! {
                _ = &mut *stop => return RequestOutcome::Stopped,
                line = tokio::time::timeout_at(deadline, self.lines.next_line()) => {
                    match line {
                        Ok(Ok(Some(line))) => line,
                        Ok(Ok(None)) => {
                            eprintln!("ai-quota: app-server 提前关闭 stdout");
                            return RequestOutcome::Failed;
                        }
                        Ok(Err(err)) => {
                            eprintln!("ai-quota: 读取 app-server 输出失败: {err}");
                            return RequestOutcome::Failed;
                        }
                        Err(_) => {
                            eprintln!("ai-quota: 等待 {method} 响应超时");
                            return RequestOutcome::Failed;
                        }
                    }
                }
            };

            match parse_message(&line) {
                RpcMessage::Response {
                    id: response_id,
                    result,
                } if response_id == id => return RequestOutcome::Result(result),
                RpcMessage::Response { .. } | RpcMessage::Unknown => {}
                RpcMessage::Notification { method, params } => {
                    self.apply_notification(&method, &params);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_doubles_from_one_second_and_caps_at_thirty() {
        let mut current = Duration::from_secs(1);
        for expected in [2, 4, 8, 16, 30, 30, 30] {
            current = next_backoff(current);
            assert_eq!(current, Duration::from_secs(expected));
        }
    }

    #[test]
    fn next_backoff_starts_at_one_second() {
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(1));
    }
}
