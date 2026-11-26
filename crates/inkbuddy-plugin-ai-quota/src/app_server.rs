//! Codex app-server 客户端：后台握手、读取额度、失败重连，向外发布快照。
//!
//! 服务在启动时立刻 spawn 一个 tokio 任务，调用方拿到的是 `watch` 通道，
//! 不阻塞宿主。任务内部再启动 `codex app-server --listen stdio://`，失败时
//! 按 1 秒到 30 秒的退避重连，并保留最后一次成功快照。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::model::QuotaSnapshot;
use crate::rpc::{
    apply_account, apply_rate_limits, is_auth_error, parse_message, RpcMessage, LOGIN_REQUIRED,
};

/// 单次 JSON-RPC 请求的等待上限。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 首轮两个并发请求共同的等待上限；首帧预算只有 3 秒，一侧不来时不能白等 10 秒。
const PAIR_TIMEOUT: Duration = Duration::from_secs(5);
/// shutdown 时等待后台任务收尾的上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// 连接存活超过这个时长后，下次重连从最短退避重新开始。
const STABLE_SESSION: Duration = Duration::from_secs(30);
/// 轮询间隔下限，避免 0 或过小的值造成忙循环。
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);

const CLIENT_NAME: &str = "inkbuddy-ai-quota";
const CLIENT_TITLE: &str = "InkBuddy AI Quota";

/// 找不到可执行文件时的提示。
pub const FAILURE_NOT_FOUND: &str = "未找到 Codex CLI";
/// 其它取数失败（含进程退出、握手失败、超时）的提示。
const FAILURE_READ: &str = "额度读取失败";

/// 下一次重连等待：从 1 秒起翻倍，最多 30 秒。
pub fn next_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        return INITIAL_BACKOFF;
    }
    current.saturating_mul(2).min(MAX_BACKOFF)
}

/// 把轮询间隔钳到下限之上。
fn clamp_poll_interval(interval: Duration) -> Duration {
    interval.max(MIN_POLL_INTERVAL)
}

/// 连接是否已稳定存活到足以重置退避。
fn is_stable(elapsed: Duration) -> bool {
    elapsed >= STABLE_SESSION
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
        let poll_interval = clamp_poll_interval(poll_interval);
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
    // 只拿到套餐名还不算就绪：`account/updated` 通知常在额度读取返回前到达，
    // 若此时放行首轮渲染，页面会先闪一帧「暂无数据」。等到任一窗口或失败原因。
    snapshot.five_hour.is_some() || snapshot.weekly.is_some() || snapshot.failure.is_some()
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

/// `account/read` 失败时按错误文本选择给用户看的文案。
fn account_failure(message: &str) -> &'static str {
    if is_auth_error(message) {
        LOGIN_REQUIRED
    } else {
        FAILURE_READ
    }
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

/// 两个并发请求的结局。
enum PairOutcome {
    /// 两个请求都收到响应，按传入顺序返回。
    Results(Result<Value, String>, Result<Value, String>),
    /// 收到 stop 信号。
    Stopped,
    /// IO 失败、进程退出或超时。
    Failed,
}

/// 超时或 IO 失败时的收尾：一侧到手就补齐另一侧，别把已读到的结果丢掉。
fn partial_pair(
    first: Option<Result<Value, String>>,
    second: Option<Result<Value, String>>,
) -> PairOutcome {
    let timeout = || Err("timeout".to_string());
    match (first, second) {
        (Some(first), Some(second)) => PairOutcome::Results(first, second),
        (Some(first), None) => PairOutcome::Results(first, timeout()),
        (None, Some(second)) => PairOutcome::Results(timeout(), second),
        (None, None) => PairOutcome::Failed,
    }
}

/// 一次连接的结局。
enum SessionEnd {
    /// 收到 stop 信号。
    Stopped,
    /// 连接结束；`reset_backoff` 表示连接存活 ≥30 秒，用于重置退避。
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
    let started = Instant::now();

    let Some(stdin) = child.stdin.take() else {
        eprintln!("ai-quota: app-server 缺少 stdin 管道");
        publish_failure(tx, FAILURE_READ);
        return SessionEnd::Ended {
            reset_backoff: is_stable(started.elapsed()),
        };
    };
    let Some(stdout) = child.stdout.take() else {
        eprintln!("ai-quota: app-server 缺少 stdout 管道");
        publish_failure(tx, FAILURE_READ);
        return SessionEnd::Ended {
            reset_backoff: is_stable(started.elapsed()),
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
                reset_backoff: is_stable(started.elapsed()),
            };
        }
        RequestOutcome::Stopped => return SessionEnd::Stopped,
        RequestOutcome::Failed => {
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: is_stable(started.elapsed()),
            };
        }
    }
    if !session.send(&json!({"method": "initialized"})).await {
        publish_failure(tx, FAILURE_READ);
        return SessionEnd::Ended {
            reset_backoff: is_stable(started.elapsed()),
        };
    }

    // 账号与额度一起发：两次读取各要一次网络往返，串行会超过首轮等待窗口。
    let account_request = (2, "account/read", json!({}));
    let limits_request = (3, "account/rateLimits/read", json!({}));
    let (account, limits) = match session
        .request_pair(account_request, limits_request, stop)
        .await
    {
        PairOutcome::Results(account, limits) => (account, limits),
        PairOutcome::Stopped => return SessionEnd::Stopped,
        PairOutcome::Failed => {
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: is_stable(started.elapsed()),
            };
        }
    };

    match account {
        Ok(value) => {
            apply_account(&mut session.snapshot, &value, unix_now());
            if matches!(value.get("account"), Some(Value::Null)) {
                // 未登录时不必再读额度；发布失败快照后等用户登录再重连。
                session.publish();
                return SessionEnd::Ended {
                    reset_backoff: is_stable(started.elapsed()),
                };
            }
        }
        Err(message) => {
            eprintln!("ai-quota: account/read 失败: {message}");
            publish_failure(tx, account_failure(&message));
            return SessionEnd::Ended {
                reset_backoff: is_stable(started.elapsed()),
            };
        }
    }

    match limits {
        Ok(value) => {
            apply_rate_limits(&mut session.snapshot, &value, unix_now());
            // 账号与额度都到手后统一发布，避免首轮只拿到半份数据。
            session.publish();
        }
        Err(message) => {
            eprintln!("ai-quota: account/rateLimits/read 失败: {message}");
            publish_failure(tx, FAILURE_READ);
            return SessionEnd::Ended {
                reset_backoff: is_stable(started.elapsed()),
            };
        }
    }

    let mut next_id = 4;

    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一拍立即触发，跳过，避免刚读完就再读一次。
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = &mut *stop => return SessionEnd::Stopped,
            _ = ticker.tick() => {
                // 先确认账号，再读额度。运行中登出只发布未登录状态，
                // 复用同一条连接继续探测，避免每次重连都 spawn 子进程。
                match session.request(next_id, "account/read", json!({}), stop).await {
                    RequestOutcome::Result(Ok(value)) => {
                        next_id += 1;
                        apply_account(&mut session.snapshot, &value, unix_now());
                        if matches!(value.get("account"), Some(Value::Null)) {
                            session.publish();
                            continue;
                        }
                        match session
                            .request(next_id, "account/rateLimits/read", json!({}), stop)
                            .await
                        {
                            RequestOutcome::Result(Ok(value)) => {
                                next_id += 1;
                                apply_rate_limits(&mut session.snapshot, &value, unix_now());
                                session.publish();
                            }
                            RequestOutcome::Result(Err(message)) => {
                                eprintln!("ai-quota: 轮询额度失败: {message}");
                                publish_failure(tx, FAILURE_READ);
                                return SessionEnd::Ended {
                                    reset_backoff: is_stable(started.elapsed()),
                                };
                            }
                            RequestOutcome::Stopped => return SessionEnd::Stopped,
                            RequestOutcome::Failed => {
                                publish_failure(tx, FAILURE_READ);
                                return SessionEnd::Ended {
                                    reset_backoff: is_stable(started.elapsed()),
                                };
                            }
                        }
                    }
                    RequestOutcome::Result(Err(message)) => {
                        eprintln!("ai-quota: 轮询账号失败: {message}");
                        publish_failure(tx, account_failure(&message));
                        return SessionEnd::Ended {
                            reset_backoff: is_stable(started.elapsed()),
                        };
                    }
                    RequestOutcome::Stopped => return SessionEnd::Stopped,
                    RequestOutcome::Failed => {
                        publish_failure(tx, FAILURE_READ);
                        return SessionEnd::Ended {
                            reset_backoff: is_stable(started.elapsed()),
                        };
                    }
                }
            }
            line = session.lines.next_line() => {
                match line {
                    Ok(Some(line)) => match parse_message(&line) {
                        RpcMessage::Notification { method, params } => {
                            session.apply_notification(&method, &params);
                        }
                        RpcMessage::Request { id, method } => {
                            session.reject_request(id, &method).await;
                        }
                        RpcMessage::Response { .. } | RpcMessage::Unknown => {}
                    },
                    Ok(None) => {
                        eprintln!("ai-quota: app-server 已退出");
                        publish_failure(tx, FAILURE_READ);
                        return SessionEnd::Ended { reset_backoff: is_stable(started.elapsed()) };
                    }
                    Err(err) => {
                        eprintln!("ai-quota: 读取 app-server 输出失败: {err}");
                        publish_failure(tx, FAILURE_READ);
                        return SessionEnd::Ended { reset_backoff: is_stable(started.elapsed()) };
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

    /// 对 app-server 主动发来的请求回「方法不支持」，避免对方一直等待。
    async fn reject_request(&mut self, id: u64, method: &str) {
        let response = json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("method not supported: {method}"),
            }
        });
        // stdin 管道写满时不能无界等待，按单次请求超时收口。
        let _ = tokio::time::timeout(REQUEST_TIMEOUT, self.send(&response)).await;
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

    /// 同时发出两个请求并等到两条响应；期间收到的通知先应用。
    ///
    /// 首轮要读账号和额度，两次读取各走一次网络往返，串行会超出宿主给的首轮
    /// 等待窗口，所以一起发出、按 id 分别收结果。
    async fn request_pair(
        &mut self,
        first: (u64, &str, Value),
        second: (u64, &str, Value),
        stop: &mut oneshot::Receiver<()>,
    ) -> PairOutcome {
        let (first_id, first_method, first_params) = first;
        let (second_id, second_method, second_params) = second;
        // 两个 id 相同时第一条 guard 永远命中，第二个结果永不赋值，会白等满超时。
        if first_id == second_id {
            eprintln!("ai-quota: 并发请求的 id 不能相同: {first_id}");
            return PairOutcome::Failed;
        }
        if !self
            .send(&json!({"id": first_id, "method": first_method, "params": first_params}))
            .await
            || !self
                .send(&json!({"id": second_id, "method": second_method, "params": second_params}))
                .await
        {
            return PairOutcome::Failed;
        }

        let mut first_result = None;
        let mut second_result = None;
        let deadline = tokio::time::Instant::now() + PAIR_TIMEOUT;
        loop {
            // 两个都到齐才取走，避免每轮克隆整棵 Value。
            if first_result.is_some() && second_result.is_some() {
                let first = first_result.take().expect("checked above");
                let second = second_result.take().expect("checked above");
                return PairOutcome::Results(first, second);
            }
            let line = tokio::select! {
                _ = &mut *stop => return PairOutcome::Stopped,
                line = tokio::time::timeout_at(deadline, self.lines.next_line()) => {
                    match line {
                        Ok(Ok(Some(line))) => line,
                        Ok(Ok(None)) => {
                            eprintln!("ai-quota: app-server 提前关闭 stdout");
                            return partial_pair(first_result, second_result);
                        }
                        Ok(Err(err)) => {
                            eprintln!("ai-quota: 读取 app-server 输出失败: {err}");
                            return partial_pair(first_result, second_result);
                        }
                        Err(_) => {
                            eprintln!("ai-quota: 等待 {first_method} 与 {second_method} 响应超时");
                            return partial_pair(first_result, second_result);
                        }
                    }
                }
            };

            match parse_message(&line) {
                RpcMessage::Response { id, result } if id == first_id => {
                    first_result = Some(result);
                }
                RpcMessage::Response { id, result } if id == second_id => {
                    second_result = Some(result);
                }
                RpcMessage::Response { .. } | RpcMessage::Unknown => {}
                RpcMessage::Request { id, method } => {
                    self.reject_request(id, &method).await;
                }
                RpcMessage::Notification { method, params } => {
                    self.apply_notification(&method, &params);
                }
            }
        }
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
                RpcMessage::Request { id, method } => {
                    self.reject_request(id, &method).await;
                }
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

    #[test]
    fn is_ready_requires_window_or_failure() {
        // 只有套餐名和更新时间：额度通知先到时不能当作首轮就绪。
        let plan_only = QuotaSnapshot {
            plan: Some("plus".to_string()),
            updated_at: Some(1_700_000_000),
            ..QuotaSnapshot::default()
        };
        assert!(!is_ready(&plan_only));

        let with_window = QuotaSnapshot {
            five_hour: Some(crate::model::QuotaWindow::default()),
            updated_at: Some(1_700_000_000),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&with_window));

        let failed = QuotaSnapshot {
            failure: Some(FAILURE_NOT_FOUND.to_string()),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&failed));
    }

    #[test]
    fn partial_pair_keeps_the_side_already_received() {
        match partial_pair(Some(Ok(json!({"plan": "plus"}))), None) {
            PairOutcome::Results(Ok(value), Err(error)) => {
                assert_eq!(value["plan"], "plus");
                assert_eq!(error, "timeout");
            }
            _ => panic!("一侧到手时应返回 Results"),
        }

        match partial_pair(None, Some(Ok(json!({"rateLimits": {}})))) {
            PairOutcome::Results(Err(error), Ok(value)) => {
                assert_eq!(error, "timeout");
                assert!(value.get("rateLimits").is_some());
            }
            _ => panic!("一侧到手时应返回 Results"),
        }

        assert!(matches!(partial_pair(None, None), PairOutcome::Failed));
    }

    #[test]
    fn is_stable_requires_thirty_seconds() {
        assert!(!is_stable(Duration::from_secs(29)));
        assert!(is_stable(Duration::from_secs(30)));
        assert!(is_stable(Duration::from_secs(31)));
    }

    #[test]
    fn clamp_poll_interval_enforces_minimum() {
        assert_eq!(clamp_poll_interval(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            clamp_poll_interval(Duration::from_millis(500)),
            Duration::from_secs(5)
        );
        assert_eq!(
            clamp_poll_interval(Duration::from_secs(1)),
            Duration::from_secs(5)
        );
        assert_eq!(
            clamp_poll_interval(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }
}
