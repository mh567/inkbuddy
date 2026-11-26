//! 用假 app-server 脚本端到端验证握手、两次读取、通知与失败提示。
//!
//! 测试只读临时目录里的脚本，不访问真实 OpenAI 服务，也不读 `~/.codex`。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use inkbuddy_plugin_ai_quota::app_server::QuotaService;

/// 假 app-server 脚本；`@MODE@` 控制 account/read 与是否推送额度通知。
const SCRIPT: &str = r#"#!/usr/bin/env python3
import json
import sys

MODE = "@MODE@"
updated_sent = False


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def window(used, duration):
    return {"usedPercent": used, "windowDurationMins": duration, "resetsAt": 4102444800}


def rate_limits(primary, secondary):
    return {
        "rateLimits": {
            "planType": "plus",
            "primary": primary,
            "secondary": secondary,
        },
        "rateLimitResetCredits": {"availableCount": 2},
    }


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
    elif method == "initialized":
        continue
    elif method == "account/read":
        if MODE == "unauthenticated":
            send({"id": request_id, "result": {"account": None, "requiresOpenaiAuth": True}})
        else:
            send({"id": request_id, "result": {
                "account": {"type": "chatgpt", "email": "user@example.com", "planType": "plus"},
                "requiresOpenaiAuth": False,
            }})
    elif method == "account/rateLimits/read":
        send({"id": request_id, "result": rate_limits(window(0, 300), window(51, 10080))})
        if MODE == "notify" and not updated_sent:
            updated_sent = True
            send({
                "method": "account/rateLimits/updated",
                "params": {"rateLimits": {
                    "primary": window(0, 300),
                    "secondary": window(77, 10080),
                }},
            })
"#;

/// 临时目录里的假可执行文件，测试结束删除。
struct FakeCodex {
    dir: PathBuf,
    bin: PathBuf,
}

impl Drop for FakeCodex {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn fake_codex(mode: &str) -> FakeCodex {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = format!(
        "inkbuddy-ai-quota-{}-{}-{}",
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
    fs::write(&bin, SCRIPT.replace("@MODE@", mode)).expect("write fake codex");
    let mut permissions = fs::metadata(&bin).expect("stat fake codex").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&bin, permissions).expect("chmod fake codex");
    FakeCodex { dir, bin }
}

#[tokio::test]
async fn service_reads_plan_windows_and_credits() {
    let fake = fake_codex("normal");
    let service = QuotaService::start(fake.bin.clone(), Duration::from_secs(30));

    let snapshot = service.wait_first(Duration::from_secs(5)).await;

    assert_eq!(snapshot.plan.as_deref(), Some("plus"));
    let five_hour = snapshot.five_hour.expect("five hour window");
    assert_eq!(five_hour.used_percent, 0);
    assert_eq!(five_hour.window_duration_mins, Some(300));
    let weekly = snapshot.weekly.expect("weekly window");
    assert_eq!(weekly.used_percent, 51);
    assert_eq!(weekly.window_duration_mins, Some(10080));
    assert_eq!(snapshot.reset_credits, Some(2));
    assert!(snapshot.updated_at.unwrap_or(0) > 0, "updated_at 应被填充");
    assert_eq!(snapshot.failure, None);

    service.shutdown().await;
}

#[tokio::test]
async fn service_applies_rate_limit_notification_without_polling() {
    let fake = fake_codex("notify");
    let service = QuotaService::start(fake.bin.clone(), Duration::from_secs(30));

    let first = service.wait_first(Duration::from_secs(5)).await;
    assert_eq!(first.weekly.as_ref().map(|w| w.used_percent), Some(51));

    let mut rx = service.snapshot();
    let updated = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rx.borrow().weekly.as_ref().map(|w| w.used_percent) == Some(77) {
                return rx.borrow().clone();
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    })
    .await
    .expect("3 秒内应通过通知看到周用量 77%");

    assert_eq!(updated.weekly.map(|w| w.used_percent), Some(77));
    service.shutdown().await;
}

#[tokio::test]
async fn service_reports_unauthenticated_account() {
    let fake = fake_codex("unauthenticated");
    let service = QuotaService::start(fake.bin.clone(), Duration::from_secs(30));

    let snapshot = service.wait_first(Duration::from_secs(3)).await;
    let failure = snapshot.failure.unwrap_or_default();
    assert!(failure.contains("未登录"), "unexpected failure: {failure}");

    service.shutdown().await;
}

#[tokio::test]
async fn service_reports_missing_binary() {
    let dir = std::env::temp_dir().join(format!(
        "inkbuddy-ai-quota-missing-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let missing = dir.join("definitely-missing-codex");

    let service = QuotaService::start(missing, Duration::from_secs(30));
    let snapshot = service.wait_first(Duration::from_secs(3)).await;

    let failure = snapshot.failure.unwrap_or_default();
    assert!(
        failure.contains("Codex") || failure.contains("额度读取失败"),
        "unexpected failure: {failure}"
    );

    service.shutdown().await;
}
