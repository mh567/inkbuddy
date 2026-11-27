//! 本地假 HTTP server 驱动的取数集成测试；不访问 api.commandcode.ai。

use inkbuddy_plugin_command_code::api::{build_client, fetch_snapshot, FetchError};
use inkbuddy_plugin_command_code::model::{QuotaSnapshot, QuotaWindow, UsageSummary};
use reqwest::redirect::Policy;
use reqwest::Client;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

type Responder = Arc<dyn Fn(&str) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

struct FakeServer {
    base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    handle: JoinHandle<()>,
}

impl FakeServer {
    async fn start(responder: Responder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = requests.clone();
        let handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let responder = responder.clone();
                let sink = sink.clone();
                tokio::spawn(async move {
                    serve_one(&mut socket, responder, sink).await;
                });
            }
        });
        FakeServer {
            base: format!("http://{addr}"),
            requests,
            handle,
        }
    }

    /// 接受连接但永不回包，用于触发客户端超时。
    async fn start_silent() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        FakeServer {
            base: format!("http://{addr}"),
            requests: Arc::new(Mutex::new(Vec::new())),
            handle,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve_one(
    socket: &mut tokio::net::TcpStream,
    responder: Responder,
    sink: Arc<Mutex<Vec<Recorded>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        match socket.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 64 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    sink.lock().unwrap().push(Recorded {
        method,
        path: path.clone(),
        headers,
    });

    let (status, extra, body) = responder(&path);
    let mut response = format!(
        "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.as_bytes().len()
    );
    for (key, value) in extra {
        response.push_str(&format!("{key}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&body);
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

fn static_routes() -> Responder {
    Arc::new(|path| {
        let route = path.split('?').next().unwrap_or(path);
        match route {
            "/alpha/whoami" => (200, vec![], json!({"organizationId": "org_1"}).to_string()),
            "/alpha/billing/subscriptions" => (
                200,
                vec![],
                json!([{
                    "planId": "GOAT",
                    "status": "active",
                    "currentPeriodStart": 1700000000,
                    "currentPeriodEnd": 1702592000
                }])
                .to_string(),
            ),
            "/alpha/billing/credits" => (
                200,
                vec![],
                json!({
                    "credits": {"monthlyCredits": 50, "purchasedCredits": 10, "freeCredits": 2.4},
                    "windowLimits": {
                        "fiveHour": {"used": 3, "cap": 14, "resetAt": 1700007200},
                        "weekly": {"used": 20, "cap": 70, "resetAt": 1700000000}
                    }
                })
                .to_string(),
            ),
            "/alpha/usage/summary" => (
                200,
                vec![],
                json!({"summary": {"totalCount": 18, "totalCost": 7.6, "totalTokens": 42000}})
                    .to_string(),
            ),
            _ => (404, vec![], "{}".to_string()),
        }
    })
}

#[tokio::test]
async fn fetch_requests_four_read_only_gets_with_headers_and_maps_fields() {
    let server = FakeServer::start(static_routes()).await;
    let client = build_client();
    let snapshot = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/alpha/whoami");
    assert_eq!(requests[3].path, "/alpha/usage/summary?since=1700000000");
    // 第 2、3 步并发发出，只要求都排在 whoami 之后、usage 之前。
    let middle: Vec<&str> = requests[1..3]
        .iter()
        .map(|r| r.path.split('?').next().unwrap_or(&r.path))
        .collect();
    assert!(middle.contains(&"/alpha/billing/subscriptions"));
    assert!(middle.contains(&"/alpha/billing/credits"));
    assert!(requests[1].path.contains("orgId=org_1"));
    assert!(requests[2].path.contains("orgId=org_1"));
    for request in &requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("authorization"), Some("Bearer test-key"));
        assert_eq!(request.header("x-command-code-version"), Some("1.88.5"));
        assert_eq!(request.header("x-cli-environment"), Some("production"));
    }

    assert_eq!(snapshot.plan.as_deref(), Some("GOAT"));
    let five = snapshot.five_hour.unwrap();
    assert_eq!(five.used_percent, Some(21));
    assert_eq!(five.resets_at, Some(1700007200));
    let weekly = snapshot.weekly.unwrap();
    assert_eq!(weekly.used_percent, Some(29));
    let monthly = snapshot.monthly.unwrap();
    assert_eq!(monthly.resets_at, Some(1702592000));
    // 月行只算套餐剩余 50：GOAT 池 70，(70-50)/70 ≈ 29%，额外 credit 只进顶部总额。
    assert_eq!(monthly.used_percent, Some(29));
    assert_eq!(snapshot.monthly_credits_remaining, Some(50.0));
    // 顶部剩余 credit 仍是三项之和，口径与月行分离。
    assert!((snapshot.credits_remaining.unwrap() - 62.4).abs() < 1e-9);
    assert!((snapshot.credits_extra.unwrap() - 12.4).abs() < 1e-9);
    assert_eq!(
        snapshot.usage.unwrap(),
        UsageSummary {
            total_count: Some(18),
            total_cost: Some(7.6),
            total_tokens: Some(42000),
        }
    );
    assert!(snapshot.updated_at.is_some());
    assert_eq!(snapshot.failure, None);
    server.handle.abort();
}

#[tokio::test]
async fn monthly_percent_uses_plan_credits_and_excludes_extras() {
    // GOAT 池 70；monthlyCredits 50，purchased 10，free 2.4。
    // 月行按套餐单项算 (70-50)/70 ≈ 29%，额外 credit 只进顶部总额。
    let server = FakeServer::start(static_routes()).await;
    let client = build_client();
    let snapshot = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap();

    let monthly = snapshot.monthly.unwrap();
    assert_eq!(monthly.used_percent, Some(29));
    assert_eq!(monthly.resets_at, Some(1702592000));
    assert_eq!(snapshot.monthly_credits_remaining, Some(50.0));
    assert!((snapshot.credits_remaining.unwrap() - 62.4).abs() < 1e-9);
    assert!((snapshot.credits_extra.unwrap() - 12.4).abs() < 1e-9);
    server.handle.abort();
}

#[tokio::test]
async fn monthly_percent_degrades_to_none_when_plan_or_credits_missing() {
    // 未知套餐：内置池查不到，算不出百分比，写 None 而不是假的 0%。
    let unknown_plan = FakeServer::start(Arc::new(|path| {
        let route = path.split('?').next().unwrap_or(path);
        match route {
            "/alpha/whoami" => (200, vec![], "{}".to_string()),
            "/alpha/billing/subscriptions" => (
                200,
                vec![],
                json!([{"planId": "mystery-plan", "status": "active", "currentPeriodEnd": 1702592000}])
                    .to_string(),
            ),
            "/alpha/billing/credits" => (
                200,
                vec![],
                json!({"credits": {"monthlyCredits": 50, "purchasedCredits": 10, "freeCredits": 2.4}})
                    .to_string(),
            ),
            _ => (404, vec![], "{}".to_string()),
        }
    }))
    .await;
    let client = build_client();
    let snapshot = fetch_snapshot(
        &client,
        &unknown_plan.base,
        "test-key",
        &QuotaSnapshot::default(),
    )
    .await
    .unwrap();
    assert_eq!(snapshot.plan.as_deref(), Some("mystery-plan"));
    let monthly = snapshot.monthly.unwrap();
    assert_eq!(monthly.resets_at, Some(1702592000));
    assert_eq!(monthly.used_percent, None);
    unknown_plan.handle.abort();

    // 已知套餐但 credits 字段缺失：剩余 credit 查不到，同样写 None。
    let missing_credits = FakeServer::start(Arc::new(|path| {
        let route = path.split('?').next().unwrap_or(path);
        match route {
            "/alpha/whoami" => (200, vec![], "{}".to_string()),
            "/alpha/billing/subscriptions" => (
                200,
                vec![],
                json!([{"planId": "GOAT", "status": "active", "currentPeriodEnd": 1702592000}])
                    .to_string(),
            ),
            "/alpha/billing/credits" => (200, vec![], "{}".to_string()),
            _ => (404, vec![], "{}".to_string()),
        }
    }))
    .await;
    let client = build_client();
    let snapshot = fetch_snapshot(
        &client,
        &missing_credits.base,
        "test-key",
        &QuotaSnapshot::default(),
    )
    .await
    .unwrap();
    let monthly = snapshot.monthly.unwrap();
    assert_eq!(monthly.used_percent, None);
    missing_credits.handle.abort();
}

#[tokio::test]
async fn auth_failure_stops_before_billing_requests() {
    let server = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/whoami") {
            (401, vec![], "{}".to_string())
        } else {
            (200, vec![], "{}".to_string())
        }
    }))
    .await;
    let client = build_client();
    let error = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::Auth);
    assert_eq!(server.requests().len(), 1);
    server.handle.abort();
}

#[tokio::test]
async fn rate_limited_statuses_are_classified_without_backoff_here() {
    let server = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/billing/subscriptions") {
            (429, vec![], "{}".to_string())
        } else if path.starts_with("/alpha/billing/credits") {
            (503, vec![], "{}".to_string())
        } else {
            static_routes()(path)
        }
    }))
    .await;
    let client = build_client();
    let error = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::RateLimited(429));
    server.handle.abort();

    let server = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/billing/credits") {
            (500, vec![], "{}".to_string())
        } else {
            static_routes()(path)
        }
    }))
    .await;
    let client = build_client();
    let error = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::RateLimited(500));
    server.handle.abort();
}

#[tokio::test]
async fn timeout_is_classified_as_transport() {
    let server = FakeServer::start_silent().await;
    let client = Client::builder()
        .timeout(Duration::from_millis(300))
        .redirect(Policy::none())
        .build()
        .unwrap();
    let error = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::Transport);
    server.handle.abort();
}

#[tokio::test]
async fn organization_account_adds_org_id_to_billing_requests() {
    let server = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/whoami") {
            (200, vec![], json!({"orgId": "org_42"}).to_string())
        } else {
            static_routes()(path)
        }
    }))
    .await;
    let client = build_client();
    fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap();
    let requests = server.requests();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/alpha/billing/subscriptions?orgId=org_42"));
    assert!(paths.contains(&"/alpha/billing/credits?orgId=org_42"));
    assert!(paths.contains(&"/alpha/usage/summary?since=1700000000"));
    server.handle.abort();
}

#[tokio::test]
async fn nested_organization_id_and_personal_account_are_handled() {
    let nested = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/whoami") {
            (
                200,
                vec![],
                json!({"organization": {"id": "org_nested"}}).to_string(),
            )
        } else {
            static_routes()(path)
        }
    }))
    .await;
    let client = build_client();
    fetch_snapshot(&client, &nested.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap();
    let paths: Vec<String> = nested.requests().into_iter().map(|r| r.path).collect();
    assert!(paths
        .iter()
        .any(|p| p == "/alpha/billing/subscriptions?orgId=org_nested"));
    nested.handle.abort();

    let personal = FakeServer::start(static_routes()).await;
    let client = build_client();
    fetch_snapshot(
        &client,
        &personal.base,
        "test-key",
        &QuotaSnapshot::default(),
    )
    .await
    .unwrap();
    let paths: Vec<String> = personal.requests().into_iter().map(|r| r.path).collect();
    assert!(paths
        .iter()
        .any(|p| p == "/alpha/billing/subscriptions?orgId=org_1"));
    personal.handle.abort();
}

#[tokio::test]
async fn missing_fields_degrade_only_their_own_lines() {
    let server = FakeServer::start(Arc::new(|path| {
        let route = path.split('?').next().unwrap_or(path);
        match route {
            "/alpha/whoami" => (200, vec![], json!({"orgId": "org_1"}).to_string()),
            "/alpha/billing/subscriptions" => (
                200,
                vec![],
                json!([{"planId": "GOAT", "status": "active", "currentPeriodStart": 1700000000}])
                    .to_string(),
            ),
            "/alpha/billing/credits" => (
                200,
                vec![],
                json!({
                    "credits": {"monthlyCredits": 50, "purchasedCredits": 10, "freeCredits": 2},
                    "windowLimits": {
                        "fiveHour": {"used": 1, "cap": 10},
                        "weekly": {"used": 2}
                    }
                })
                .to_string(),
            ),
            "/alpha/usage/summary" => (
                200,
                vec![],
                json!({"summary": {"totalCount": 7}}).to_string(),
            ),
            _ => (404, vec![], "{}".to_string()),
        }
    }))
    .await;
    let client = build_client();
    let snapshot = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap();

    assert_eq!(snapshot.plan.as_deref(), Some("GOAT"));
    assert_eq!(snapshot.monthly, None);
    assert_eq!(
        snapshot.five_hour.unwrap(),
        QuotaWindow {
            used_percent: Some(10),
            resets_at: None
        }
    );
    assert_eq!(snapshot.weekly, None);
    assert_eq!(snapshot.credits_remaining, Some(62.0));
    assert_eq!(snapshot.credits_extra, Some(12.0));
    assert_eq!(
        snapshot.usage.unwrap(),
        UsageSummary {
            total_count: Some(7),
            total_cost: None,
            total_tokens: None,
        }
    );
    server.handle.abort();
}

#[tokio::test]
async fn previous_reset_at_survives_window_rollover() {
    let previous = QuotaSnapshot {
        five_hour: Some(QuotaWindow {
            used_percent: None,
            resets_at: Some(1700009999),
        }),
        monthly: Some(QuotaWindow {
            used_percent: None,
            resets_at: Some(1702592999),
        }),
        ..QuotaSnapshot::default()
    };
    let server = FakeServer::start(Arc::new(|path| {
        let route = path.split('?').next().unwrap_or(path);
        match route {
            "/alpha/whoami" => (200, vec![], "{}".to_string()),
            "/alpha/billing/subscriptions" => (
                200,
                vec![],
                json!([{"planId": "GOAT", "status": "active", "currentPeriodStart": 1700000000}])
                    .to_string(),
            ),
            "/alpha/billing/credits" => (
                200,
                vec![],
                json!({"windowLimits": {"fiveHour": {"used": 1, "cap": 10}}}).to_string(),
            ),
            _ => static_routes()(path),
        }
    }))
    .await;
    let client = build_client();
    let snapshot = fetch_snapshot(&client, &server.base, "test-key", &previous)
        .await
        .unwrap();
    assert_eq!(snapshot.five_hour.unwrap().resets_at, Some(1700009999));
    assert_eq!(snapshot.monthly.unwrap().resets_at, Some(1702592999));
    server.handle.abort();
}

#[tokio::test]
async fn usage_failure_keeps_quota_snapshot() {
    let server = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/usage/summary") {
            (500, vec![], "{}".to_string())
        } else {
            static_routes()(path)
        }
    }))
    .await;
    let client = build_client();
    let snapshot = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap();
    assert_eq!(snapshot.plan.as_deref(), Some("GOAT"));
    assert_eq!(snapshot.credits_remaining, Some(62.4));
    assert_eq!(snapshot.usage, None);
    server.handle.abort();
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let server = FakeServer::start(Arc::new(|path| {
        if path.starts_with("/alpha/whoami") {
            (
                302,
                vec![("Location".to_string(), "/alpha/evil".to_string())],
                "".to_string(),
            )
        } else {
            (200, vec![], "{}".to_string())
        }
    }))
    .await;
    let client = build_client();
    let error = fetch_snapshot(&client, &server.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::Transport);
    assert_eq!(server.requests().len(), 1);
    server.handle.abort();
}

#[tokio::test]
async fn redirect_following_client_cannot_take_key_out_of_origin() {
    // 目标 server 在另一个本地端口记录全部请求，用于验证 key 不出域。
    let target = FakeServer::start(Arc::new(|_| {
        (200, vec![], json!({"injected": true}).to_string())
    }))
    .await;
    let location = format!("{}/alpha/evil", target.base);
    // 源 server 的 whoami 返回 302，指向目标端口。
    let source = FakeServer::start(Arc::new(move |path| {
        if path.starts_with("/alpha/whoami") {
            (
                302,
                vec![("Location".to_string(), location.clone())],
                "".to_string(),
            )
        } else {
            (200, vec![], "{}".to_string())
        }
    }))
    .await;
    // 调用方传入「跟随重定向」的 client，而不是 build_client()。
    let client = Client::builder()
        .redirect(Policy::limited(5))
        .build()
        .unwrap();
    let error = fetch_snapshot(&client, &source.base, "test-key", &QuotaSnapshot::default())
        .await
        .unwrap_err();
    assert_eq!(error, FetchError::Transport);
    // 插件自身只对源 server 发一次请求，不读取 Location 再发第二个请求。
    assert_eq!(source.requests().len(), 1);
    // 即使 reqwest 在客户端层跟随了重定向，跨端口时也会剥掉 Authorization，
    // key 绝不能出现在另一个端口的请求里（key 不出域）。
    assert!(
        target
            .requests()
            .iter()
            .all(|request| request.header("authorization") != Some("Bearer test-key")),
        "apiKey 被带到了另一个端口"
    );
    source.handle.abort();
    target.handle.abort();
}
