//! 长驻取数服务：后台按间隔读取额度，通过 watch 通道发布快照。

use std::time::Duration;

use reqwest::Client;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::api::{self, FetchError};
use crate::model::{QuotaSnapshot, FAILURE_NOT_FOUND, FAILURE_READ, LOGIN_REQUIRED};

/// shutdown 时等待后台任务收尾的上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// 后台任务的等待节奏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollState {
    /// 正常间隔。
    Normal,
    /// 限流后按当前退避间隔等待。
    Backoff(Duration),
}

impl PollState {
    fn new() -> Self {
        Self::Normal
    }

    fn on_success(&mut self) {
        *self = Self::Normal;
    }

    fn on_failure(&mut self, base: Duration) {
        let current = match *self {
            Self::Normal => base,
            Self::Backoff(delay) => delay,
        };
        *self = Self::Backoff(api::next_backoff(current));
    }

    fn delay(&self, base: Duration) -> Duration {
        match *self {
            Self::Normal => base,
            Self::Backoff(delay) => delay,
        }
    }
}

/// 长驻额度服务；调用方只持有 watch 接收端。
pub struct QuotaService {
    rx: watch::Receiver<QuotaSnapshot>,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl QuotaService {
    /// 按生产 base 启动后台任务。
    pub fn start(key: String, poll_interval: Duration) -> Self {
        Self::start_with_base(key, api::BASE_URL.to_string(), poll_interval)
    }

    /// 按指定 base 启动后台任务，供测试注入本地假 server。
    pub fn start_with_base(key: String, base: String, poll_interval: Duration) -> Self {
        let (tx, rx) = watch::channel(QuotaSnapshot::default());
        let (stop, stop_rx) = oneshot::channel();
        // 客户端统一由 api 构造，保留禁重定向与超时设置。
        let client = api::build_client();
        let handle = tokio::spawn(run(client, base, key, poll_interval, tx, stop_rx));
        Self {
            rx,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    pub fn snapshot(&self) -> watch::Receiver<QuotaSnapshot> {
        self.rx.clone()
    }

    /// 等首轮结果：任一窗口、失败原因或更新时间到手就立即返回。
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

    /// 通知后台任务退出，最多等 2 秒。
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

/// 快照是否足以让首轮 render 返回。
fn is_ready(snapshot: &QuotaSnapshot) -> bool {
    snapshot.five_hour.is_some()
        || snapshot.weekly.is_some()
        || snapshot.monthly.is_some()
        || snapshot.failure.is_some()
        || snapshot.updated_at.is_some()
}

/// 鉴权失败是终态：丢弃旧百分比，整块换成登录提示。
fn auth_snapshot() -> QuotaSnapshot {
    QuotaSnapshot {
        failure: Some(LOGIN_REQUIRED.to_string()),
        ..QuotaSnapshot::default()
    }
}

/// 取数失败：保留旧快照，仅更新提示文案。
fn failed_snapshot(previous: &QuotaSnapshot, message: String) -> QuotaSnapshot {
    let mut snapshot = previous.clone();
    // 登录提示与「额度数据未找到」都是终态，后续传输失败不改写它们。
    if matches!(
        snapshot.failure.as_deref(),
        Some(LOGIN_REQUIRED) | Some(FAILURE_NOT_FOUND)
    ) {
        return snapshot;
    }
    snapshot.failure = Some(message);
    snapshot
}

/// 取数失败时更新等待节奏：限流与传输失败都退避，凭据问题保持正常间隔。
fn on_fetch_error(state: &mut PollState, error: &FetchError, base: Duration) {
    match error {
        FetchError::RateLimited(_) | FetchError::Transport => state.on_failure(base),
        FetchError::Auth | FetchError::InvalidKey => state.on_success(),
    }
}

/// 后台取数循环。
async fn run(
    client: Client,
    base: String,
    key: String,
    poll_interval: Duration,
    tx: watch::Sender<QuotaSnapshot>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut state = PollState::new();
    loop {
        let previous = tx.borrow().clone();
        let result = tokio::select! {
            _ = &mut stop => return,
            result = api::fetch_snapshot(&client, &base, &key, &previous) => result,
        };
        match result {
            Ok(snapshot) => {
                state.on_success();
                // updated_at 由 fetch_snapshot 统一设置。
                let _ = tx.send_replace(snapshot);
            }
            Err(error) => {
                on_fetch_error(&mut state, &error, poll_interval);
                let snapshot = match &error {
                    FetchError::Auth => {
                        // 凭据在进程启动时读入，命令行重新登录后要重启插件进程才会生效。
                        eprintln!("command-code: 鉴权失败，重启插件进程后才会重新读取凭据");
                        auth_snapshot()
                    }
                    FetchError::InvalidKey => {
                        eprintln!("command-code: apiKey 含非法字符，无法构造认证头");
                        auth_snapshot()
                    }
                    FetchError::RateLimited(code) => {
                        // 日志只写状态码，凭据留在进程内存里。
                        eprintln!("command-code: 取数被限流，状态码 {code}");
                        failed_snapshot(&previous, format!("额度读取失败（HTTP {code}）"))
                    }
                    FetchError::Transport => {
                        eprintln!("command-code: 取数失败");
                        failed_snapshot(&previous, FAILURE_READ.to_string())
                    }
                };
                let _ = tx.send_replace(snapshot);
            }
        }
        let delay = state.delay(poll_interval);
        tokio::select! {
            _ = &mut stop => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_ready_accepts_any_window_failure_or_update_time() {
        assert!(!is_ready(&QuotaSnapshot::default()));

        let plan_only = QuotaSnapshot {
            plan: Some("GOAT".to_string()),
            ..QuotaSnapshot::default()
        };
        assert!(!is_ready(&plan_only));

        let updated = QuotaSnapshot {
            updated_at: Some(1_700_000_000),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&updated));

        let failed = QuotaSnapshot {
            failure: Some(FAILURE_READ.to_string()),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&failed));

        let five_hour = QuotaSnapshot {
            five_hour: Some(crate::model::QuotaWindow::default()),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&five_hour));

        let weekly = QuotaSnapshot {
            weekly: Some(crate::model::QuotaWindow::default()),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&weekly));

        let monthly = QuotaSnapshot {
            monthly: Some(crate::model::QuotaWindow::default()),
            ..QuotaSnapshot::default()
        };
        assert!(is_ready(&monthly));
    }

    #[test]
    fn poll_state_doubles_on_consecutive_failures_and_resets_on_success() {
        let base = Duration::from_secs(60);
        let mut state = PollState::new();
        assert_eq!(state.delay(base), base);

        state.on_failure(base);
        assert_eq!(state.delay(base), Duration::from_secs(120));
        state.on_failure(base);
        assert_eq!(state.delay(base), Duration::from_secs(240));
        state.on_failure(base);
        assert_eq!(state.delay(base), Duration::from_secs(480));

        state.on_success();
        assert_eq!(state, PollState::Normal);
        assert_eq!(state.delay(base), base);

        state.on_failure(base);
        assert_eq!(state.delay(base), Duration::from_secs(120));
    }

    #[test]
    fn poll_state_backoff_caps_at_thirty_minutes() {
        let base = Duration::from_secs(60);
        let mut state = PollState::new();
        for _ in 0..10 {
            state.on_failure(base);
        }
        assert_eq!(state.delay(base), Duration::from_secs(30 * 60));
    }

    #[test]
    fn auth_snapshot_drops_old_percentages() {
        let snapshot = auth_snapshot();
        assert_eq!(snapshot.failure.as_deref(), Some(LOGIN_REQUIRED));
        assert!(snapshot.five_hour.is_none());
        assert!(snapshot.weekly.is_none());
        assert!(snapshot.monthly.is_none());
        assert!(snapshot.updated_at.is_none());
        assert!(snapshot.credits_remaining.is_none());
    }

    #[test]
    fn failed_snapshot_keeps_old_data_and_login_terminal_text() {
        let old = QuotaSnapshot {
            plan: Some("GOAT".to_string()),
            five_hour: Some(crate::model::QuotaWindow {
                used_percent: Some(50),
                resets_at: Some(1_700_000_000),
            }),
            updated_at: Some(1_699_999_900),
            ..QuotaSnapshot::default()
        };
        let failed = failed_snapshot(&old, FAILURE_READ.to_string());
        assert_eq!(failed.failure.as_deref(), Some(FAILURE_READ));
        assert_eq!(failed.plan, old.plan);
        assert_eq!(failed.five_hour, old.five_hour);
        assert_eq!(failed.updated_at, old.updated_at);

        let first_round = failed_snapshot(&QuotaSnapshot::default(), FAILURE_READ.to_string());
        assert_eq!(first_round.failure.as_deref(), Some(FAILURE_READ));
        assert!(first_round.updated_at.is_none());

        // 登录终态不能被一次传输失败改回通用文案。
        let login = auth_snapshot();
        let still_login = failed_snapshot(&login, FAILURE_READ.to_string());
        assert_eq!(still_login.failure.as_deref(), Some(LOGIN_REQUIRED));

        // 额度未找到同样是终态。
        let not_found = QuotaSnapshot {
            failure: Some(FAILURE_NOT_FOUND.to_string()),
            updated_at: Some(1_700_000_000),
            ..QuotaSnapshot::default()
        };
        let still_not_found = failed_snapshot(&not_found, FAILURE_READ.to_string());
        assert_eq!(still_not_found.failure.as_deref(), Some(FAILURE_NOT_FOUND));
        assert_eq!(still_not_found.updated_at, not_found.updated_at);
    }

    #[test]
    fn on_fetch_error_backs_off_transport_and_rate_limit_only() {
        let base = Duration::from_secs(60);

        let mut state = PollState::new();
        on_fetch_error(&mut state, &FetchError::Transport, base);
        assert_eq!(state.delay(base), Duration::from_secs(120));
        on_fetch_error(&mut state, &FetchError::RateLimited(503), base);
        assert_eq!(state.delay(base), Duration::from_secs(240));

        let mut state = PollState::new();
        on_fetch_error(&mut state, &FetchError::Auth, base);
        assert_eq!(state, PollState::Normal);
        on_fetch_error(&mut state, &FetchError::InvalidKey, base);
        assert_eq!(state, PollState::Normal);
    }

    #[tokio::test]
    async fn wait_first_returns_failure_when_fetch_cannot_reach_local_server() {
        let service = QuotaService::start_with_base(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            Duration::from_secs(60),
        );
        let snapshot = service.wait_first(Duration::from_secs(2)).await;
        assert_eq!(snapshot.failure.as_deref(), Some(FAILURE_READ));
        service.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_background_task_without_panic() {
        let service = QuotaService::start_with_base(
            "test-key".to_string(),
            "http://127.0.0.1:1".to_string(),
            Duration::from_secs(60),
        );
        service.shutdown().await;
    }
}
