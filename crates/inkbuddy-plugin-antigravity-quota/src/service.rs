//! 后台取数服务：先把上次落盘的快照放出来，再立刻读一次，之后按间隔轮询。
//!
//! 读到就写盘并替换快照；读不到就保留旧数据，只更新 `failure`，页面按
//! 「更新于 X 分钟前」提示。反重力没开时用更短的间隔重试，这样用户随时打开
//! IDE，最多十几秒后页面就跟上。

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::model::{Snapshot, Source};
use crate::quota;
use crate::store;

/// 正常轮询间隔下限，避免 0 或过小的值造成忙循环。
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 上一次没读到时用这个间隔重试；`ps` 一次十几毫秒，代价可以忽略。
const RETRY_INTERVAL: Duration = Duration::from_secs(15);
/// 走命令行版时的轮询间隔。一次要起一个 `agy` 进程，约 6 秒，额度变化也慢。
const CLI_POLL_INTERVAL: Duration = Duration::from_secs(300);
/// 一轮取数超过这个时长，说明走了命令行版；失败后按更长的间隔重试。
const SLOW_ROUND: Duration = Duration::from_secs(3);
/// 慢路径失败后的重试间隔，避免每 15 秒拉起一次 `agy`。
const SLOW_RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// shutdown 时等待后台任务收尾的上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// 长驻额度服务；调用方只持有 `watch` 接收端。
pub struct QuotaService {
    rx: watch::Receiver<Snapshot>,
    /// 第一轮取数是否已经结束（不管成功还是失败）。
    first_done: watch::Receiver<bool>,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl QuotaService {
    /// 读出上次落盘的快照立刻发布，再启动后台任务。
    pub fn start(poll_interval: Duration, cache: PathBuf) -> Self {
        let poll_interval = poll_interval.max(MIN_POLL_INTERVAL);
        let cached = store::load(&cache).unwrap_or_default();
        let (tx, rx) = watch::channel(cached);
        let (first_done_tx, first_done_rx) = watch::channel(false);
        let (stop, stop_rx) = oneshot::channel();
        let handle = tokio::spawn(run(poll_interval, cache, tx, first_done_tx, stop_rx));
        Self {
            rx,
            first_done: first_done_rx,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    pub fn snapshot(&self) -> watch::Receiver<Snapshot> {
        self.rx.clone()
    }

    /// 等第一轮取数结束（成功或失败都算），最多等 `timeout`。
    ///
    /// 首帧要的是「这次读过了」，不是「有数据了」：反重力关着时立刻返回
    /// 「反重力未运行」，有落盘数据时也能立刻换成刚读到的新数字。
    pub async fn wait_first(&self, timeout: Duration) -> Snapshot {
        if !*self.first_done.borrow() {
            let mut first_done = self.first_done.clone();
            let _ = tokio::time::timeout(timeout, first_done.changed()).await;
        }
        self.rx.borrow().clone()
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

async fn run(
    poll_interval: Duration,
    cache: PathBuf,
    tx: watch::Sender<Snapshot>,
    first_done: watch::Sender<bool>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut announced = false;
    // 反重力关着时会一直重试，同一个原因只打一次日志，免得每 15 秒刷一行。
    let mut last_reason: Option<&'static str> = None;
    loop {
        let started = tokio::time::Instant::now();
        let result = quota::collect().await;
        let slow = started.elapsed() >= SLOW_ROUND;
        let delay = match result {
            Ok(mut snapshot) => {
                last_reason = None;
                snapshot.updated_at = Some(unix_now());
                let from_cli = snapshot.source == Source::Cli;
                if let Err(err) = store::save(&cache, &snapshot) {
                    eprintln!("antigravity-quota: 保存额度失败: {err}");
                }
                let _ = tx.send_replace(snapshot);
                if from_cli {
                    CLI_POLL_INTERVAL
                } else {
                    poll_interval
                }
            }
            Err(failure) => {
                if last_reason != Some(failure.reason) {
                    last_reason = Some(failure.reason);
                    eprintln!(
                        "antigravity-quota: {}（{}）",
                        failure.reason,
                        failure.details.join("；")
                    );
                }
                let mut snapshot = tx.borrow().clone();
                snapshot.failure = Some(failure.reason.to_string());
                let _ = tx.send_replace(snapshot);
                if slow {
                    SLOW_RETRY_INTERVAL
                } else {
                    RETRY_INTERVAL.min(poll_interval)
                }
            }
        };

        if !announced {
            announced = true;
            let _ = first_done.send(true);
        }

        tokio::select! {
            _ = &mut stop => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_is_faster_than_the_normal_interval() {
        assert!(RETRY_INTERVAL < Duration::from_secs(60));
        // 调用方给的间隔比重试间隔还短时，重试不能把它拉长
        assert_eq!(
            RETRY_INTERVAL.min(Duration::from_secs(10)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn poll_interval_has_a_floor() {
        assert_eq!(Duration::ZERO.max(MIN_POLL_INTERVAL), MIN_POLL_INTERVAL);
        assert_eq!(
            Duration::from_secs(60).max(MIN_POLL_INTERVAL),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn cli_polls_more_slowly_than_the_local_server() {
        // 命令行版一次要起一个进程，轮询间隔必须比本地 RPC 长
        assert!(CLI_POLL_INTERVAL > Duration::from_secs(60));
        assert!(SLOW_RETRY_INTERVAL > RETRY_INTERVAL);
        // 慢路径的判定要短于一次命令行调用（约 6 秒），也要长于本地 RPC（毫秒级）
        assert!(SLOW_ROUND < Duration::from_secs(6));
        assert!(SLOW_ROUND > Duration::from_secs(1));
    }
}
