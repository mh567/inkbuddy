//! 后台取数服务：先把上次落盘的快照放出来，再立刻读一次，之后按间隔轮询。
//!
//! 读到就写盘并替换快照；读不到就保留旧数据，只更新 `failure`，页面按
//! 「更新于 X 分钟前」提示。
//!
//! 反重力没开时不轮询取数：只每 10 秒看一次日志与会话目录的修改时间，
//! 有活动才跑一次完整取数并恢复监控。这样不用反重力的时候插件基本不动。

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::discovery;
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
/// 待机时多久看一眼活动痕迹。只看几个文件的修改时间，不跑 `ps`。
const IDLE_TICK: Duration = Duration::from_secs(10);
/// 待机时也定期全量探一次，防止活动痕迹判定漏掉（日志目录不可读之类）。
const IDLE_PROBE: Duration = Duration::from_secs(600);
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
    // 待机：反重力没开时只盯活动痕迹，有动静才跑一次完整取数。
    let mut idle_since: Option<tokio::time::Instant> = None;
    let mut stamp = discovery::activity_stamp();
    loop {
        // 待机时每 10 秒看一次日志和会话目录的修改时间；有变化就立刻取数，
        // 没变化就继续睡，不惊动 `ps`，也不连端口。
        if let Some(since) = idle_since {
            tokio::select! {
                _ = &mut stop => return,
                _ = tokio::time::sleep(IDLE_TICK) => {}
            }
            let now = discovery::activity_stamp();
            let woke = now != stamp;
            stamp = now;
            if !woke && since.elapsed() < IDLE_PROBE {
                continue;
            }
        }

        let started = tokio::time::Instant::now();
        let result = quota::collect().await;
        let slow = started.elapsed() >= SLOW_ROUND;
        let delay = match result {
            Ok(mut snapshot) => {
                last_reason = None;
                idle_since = None;
                // 命令行版的 `/usage` 输出里没有套餐名，这一轮就用上一次读到的，
                // 别把已经知道的套餐擦掉；语言服务器读到新值时自然覆盖。
                inherit_plan(&mut snapshot, &tx.borrow());
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

                let not_running = matches!(
                    failure.reason,
                    quota::FAILURE_NOT_RUNNING | quota::FAILURE_NOT_FOUND
                );
                if not_running {
                    // 记下当前痕迹，之后只有更新的活动才会把它叫醒。
                    idle_since = Some(tokio::time::Instant::now());
                    stamp = discovery::activity_stamp();
                    Duration::ZERO
                } else if slow {
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

        if !delay.is_zero() {
            tokio::select! {
                _ = &mut stop => return,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// 这一轮没读到套餐名时沿用上一份快照的套餐名；读到了就用新的。
///
/// 只有语言服务器能给出套餐名，命令行版没有这一项。不沿用的话，走一次
/// 命令行版就会把已知套餐写回空值。
fn inherit_plan(incoming: &mut Snapshot, previous: &Snapshot) {
    if incoming.plan.is_none() {
        incoming.plan = previous.plan.clone();
    }
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
    fn idle_ticks_stay_cheap_and_wake_quickly() {
        // 待机只是看几个文件的修改时间，几秒钟一次也不心疼
        assert!(IDLE_TICK <= Duration::from_secs(15));
        // 有活动要立刻恢复监控，不能被兜底探测的周期拖住
        assert!(IDLE_PROBE > RETRY_INTERVAL);
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

    #[test]
    fn plan_survives_a_read_that_has_no_plan() {
        let previous = Snapshot {
            plan: Some("Pro".to_string()),
            ..Snapshot::default()
        };

        // 命令行版读到额度但没有套餐名：保留上一次的套餐
        let mut from_cli = Snapshot {
            plan: None,
            source: Source::Cli,
            ..Snapshot::default()
        };
        inherit_plan(&mut from_cli, &previous);
        assert_eq!(from_cli.plan.as_deref(), Some("Pro"));

        // 语言服务器读到新套餐：以新的为准
        let mut from_local = Snapshot {
            plan: Some("Ultra".to_string()),
            ..Snapshot::default()
        };
        inherit_plan(&mut from_local, &previous);
        assert_eq!(from_local.plan.as_deref(), Some("Ultra"));

        // 上一份也没有套餐名时保持为空，不编一个值
        let mut first_read = Snapshot::default();
        inherit_plan(&mut first_read, &Snapshot::default());
        assert_eq!(first_read.plan, None);
    }
}
