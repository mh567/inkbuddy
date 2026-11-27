//! 用本地假服务器端到端验证一次取数：HTTP 层、响应解析、套餐名兜底。
//!
//! 不连真实反重力，也不读写 `~/.gemini`；服务器在本进程里现起现收。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use inkbuddy_plugin_antigravity_quota::discovery::Candidate;
use inkbuddy_plugin_antigravity_quota::quota::{self, FAILURE_READ};

const SUMMARY: &str = r#"{"response":{"groups":[
{"displayName":"Gemini Models","buckets":[
  {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.5,"resetTime":"2026-12-03T11:18:49Z"},
  {"bucketId":"gemini-5h","window":"5h","remainingFraction":0.75,"resetTime":"2026-11-27T18:39:20Z"}]},
{"displayName":"Claude and GPT models","buckets":[
  {"bucketId":"3p-weekly","window":"weekly","remainingFraction":1},
  {"bucketId":"3p-5h","window":"5h","remainingFraction":1}]}]}}"#;

const STATUS: &str = r#"{"response":{"userStatus":{"planStatus":{"planInfo":{"planName":"Pro"}}}}}"#;

/// 起一个只回两次响应的假服务器：第一次额度，第二次套餐。
async fn spawn_server(status: u16) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..2 {
            // 客户端提前收手时别挂在 accept 上，最多等两秒。
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
            else {
                break;
            };
            let mut buf = vec![0u8; 16 * 1024];
            let read = socket.read(&mut buf).await.expect("read request");
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let is_summary = request.contains("RetrieveUserQuotaSummary");
            seen.push(request);

            let (code, reason, body) = if !is_summary {
                (200, "OK", STATUS)
            } else if status == 200 {
                (200, "OK", SUMMARY)
            } else {
                (401, "Unauthorized", "{\"code\":\"unauthenticated\"}")
            };
            let response = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
        seen
    });
    (port, handle)
}

#[tokio::test]
async fn collects_quota_and_plan_from_local_server() {
    let (port, server) = spawn_server(200).await;
    let snapshot = quota::collect_candidates(vec![Candidate {
        csrf: Some("token-1".to_string()),
        ports: vec![port],
    }])
    .await
    .expect("should collect");

    assert_eq!(snapshot.plan.as_deref(), Some("Pro"));
    assert_eq!(snapshot.gemini.weekly.expect("weekly").used_percent, 50);
    assert_eq!(snapshot.gemini.five_hour.expect("5h").used_percent, 25);
    assert_eq!(snapshot.third_party.weekly.expect("3p weekly").used_percent, 0);

    let requests = server.await.expect("server task");
    assert_eq!(requests.len(), 2);
    // 令牌必须带上，否则真实语言服务器回 401
    assert!(requests[0].contains("X-Codeium-Csrf-Token: token-1"), "{}", requests[0]);
    assert!(requests[0].contains("Connect-Protocol-Version: 1"), "{}", requests[0]);
    assert!(requests[1].contains("GetUserStatus"), "{}", requests[1]);
}

#[tokio::test]
async fn reports_read_failure_on_unauthorized() {
    let (port, server) = spawn_server(401).await;
    let result = quota::collect_candidates(vec![Candidate {
        csrf: None,
        ports: vec![port],
    }])
    .await;
    assert_eq!(result.err().map(|failure| failure.reason), Some(FAILURE_READ));
    // 401 之后客户端不会再发第二次请求，假服务器还挂在 accept 上，直接收掉。
    server.abort();
}

#[tokio::test]
async fn skips_closed_ports_and_reports_not_running() {
    // 拿一个刚释放的端口，连接会被拒绝
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let started = std::time::Instant::now();
    let result = quota::collect_candidates(vec![Candidate {
        csrf: None,
        ports: vec![port],
    }])
    .await;
    // 一个端口都没连上：装了反重力就说没运行，没装就说没找到，都不算读取失败
    assert!(matches!(
        result.err().map(|failure| failure.reason),
        Some(quota::FAILURE_NOT_RUNNING) | Some(quota::FAILURE_NOT_FOUND)
    ));
    // 端口关着要立刻拒绝，不能等满探测超时
    assert!(started.elapsed() < Duration::from_secs(2));
}
