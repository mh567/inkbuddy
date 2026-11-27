//! 四个只读计费接口的取数、字段解析与错误分类。

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Url};
use serde_json::Value;

use crate::formatting::{month_percent, parse_reset_at, used_cap_percent};
use crate::model::{QuotaSnapshot, QuotaWindow, UsageSummary};

/// 取数 base URL；测试注入本地假 server，不会访问真实域名。
pub const BASE_URL: &str = "https://api.commandcode.ai";
/// 社区验证过的 CLI 标识头值；不读取本机 CLI 版本。
const API_VERSION: &str = "1.88.5";
const CLI_ENVIRONMENT: &str = "production";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_BASE: Duration = Duration::from_secs(60);
const BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

const WHOAMI_PATH: &str = "/alpha/whoami";
const SUBSCRIPTIONS_PATH: &str = "/alpha/billing/subscriptions";
const CREDITS_PATH: &str = "/alpha/billing/credits";
const USAGE_PATH: &str = "/alpha/usage/summary";

/// 一次取数的失败原因；429/5xx 的退避由长驻服务处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 401/403：凭据缺失或过期。
    Auth,
    /// 429/5xx：限流或服务端临时错误，携带原始状态码。
    RateLimited(u16),
    /// 超时、网络错误、重定向或响应无法解析。
    Transport,
}

/// 构造客户端：禁用重定向，避免 apiKey 被带到其他域名。
pub fn build_client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("构造 HTTP 客户端失败")
}

/// 构造三个请求头：认证、CLI 版本、运行环境。
pub fn build_headers(key: &str) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(3);
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {key}")) {
        headers.insert("authorization", value);
    }
    if let Ok(value) = HeaderValue::from_str(API_VERSION) {
        headers.insert("x-command-code-version", value);
    }
    if let Ok(value) = HeaderValue::from_str(CLI_ENVIRONMENT) {
        headers.insert("x-cli-environment", value);
    }
    headers
}

/// 把 HTTP 状态码归类为取数错误；成功或普通 4xx 返回 `None`。
pub fn classify_status(code: u16) -> Option<FetchError> {
    match code {
        401 | 403 => Some(FetchError::Auth),
        429 => Some(FetchError::RateLimited(code)),
        500..=599 => Some(FetchError::RateLimited(code)),
        // 客户端禁用重定向，收到 3xx 表示没有拿到数据。
        300..=399 => Some(FetchError::Transport),
        _ => None,
    }
}

/// 下一次退避间隔：从 60 秒起翻倍，上限 30 分钟。成功后的重置由调用方处理。
pub fn next_backoff(current: Duration) -> Duration {
    current
        .checked_mul(2)
        .unwrap_or(BACKOFF_MAX)
        .clamp(BACKOFF_BASE, BACKOFF_MAX)
}

/// 从 whoami 响应提取组织 id，兼容 `organizationId` / `orgId` / `organization.id`。
pub fn extract_org_id(value: &Value) -> Option<String> {
    value
        .get("organizationId")
        .and_then(string_value)
        .or_else(|| value.get("orgId").and_then(string_value))
        .or_else(|| {
            value
                .get("organization")
                .and_then(|org| org.get("id"))
                .and_then(string_value)
        })
}

/// 提取计费周期起点，供 usage 接口的 `since` 参数使用。
pub fn current_period_start(value: &Value) -> Option<i64> {
    pick_subscription(value)?
        .get("currentPeriodStart")
        .and_then(parse_reset_at)
}

/// 订阅字段写入快照；重置时间缺失时沿用旧快照。
pub fn apply_subscriptions(snapshot: &mut QuotaSnapshot, value: &Value, previous: &QuotaSnapshot) {
    let Some(subscription) = pick_subscription(value) else {
        return;
    };
    snapshot.plan = subscription.get("planId").and_then(string_value);
    let resets_at = subscription
        .get("currentPeriodEnd")
        .and_then(parse_reset_at)
        .or_else(|| {
            previous
                .monthly
                .as_ref()
                .and_then(|window| window.resets_at)
        });
    if let Some(resets_at) = resets_at {
        snapshot.monthly = Some(QuotaWindow {
            used_percent: None,
            resets_at: Some(resets_at),
        });
    }
}

/// credit 与两个滚动窗口写入快照；窗口字段不足时留 `None`，不推算上限。
pub fn apply_credits(snapshot: &mut QuotaSnapshot, value: &Value, previous: &QuotaSnapshot) {
    if let Some(credits) = value.get("credits") {
        let monthly = number(credits.get("monthlyCredits"));
        let purchased = number(credits.get("purchasedCredits"));
        let free = number(credits.get("freeCredits"));
        // 套餐单项单独留存，月行百分比只用这一项，额外 credit 只进顶部总额。
        snapshot.monthly_credits_remaining = monthly;
        if let (Some(monthly), Some(purchased), Some(free)) = (monthly, purchased, free) {
            snapshot.credits_remaining = Some(monthly + purchased + free);
        }
        if let (Some(purchased), Some(free)) = (purchased, free) {
            snapshot.credits_extra = Some(purchased + free);
        }
    }
    let limits = value.get("windowLimits");
    snapshot.five_hour = parse_window(
        limits.and_then(|limits| limits.get("fiveHour")),
        previous.five_hour.as_ref(),
    );
    snapshot.weekly = parse_window(
        limits.and_then(|limits| limits.get("weekly")),
        previous.weekly.as_ref(),
    );
}

/// 用量字段写入快照；字段缺失只影响用量，不改动额度。
pub fn apply_usage(snapshot: &mut QuotaSnapshot, value: &Value) {
    let summary = value
        .get("summary")
        .filter(|summary| summary.is_object())
        .unwrap_or(value);
    let usage = UsageSummary {
        total_count: count(summary.get("totalCount")),
        total_cost: number(summary.get("totalCost")),
        total_tokens: count(summary.get("totalTokens")),
    };
    if usage.total_count.is_some() || usage.total_cost.is_some() || usage.total_tokens.is_some() {
        snapshot.usage = Some(usage);
    }
}

/// 取一轮快照：whoami → 订阅与 credit 并发 → 用量。
/// 只向 `base` 发四个只读 GET；任何一步鉴权/限流/传输失败都返回错误，由调用方保留旧快照。
/// `base` 传 origin（scheme://host[:port]）即可；`Url::join` 用绝对路径替换 base 的已有路径，路径前缀会被覆盖。
pub async fn fetch_snapshot(
    client: &Client,
    base: &str,
    key: &str,
    previous: &QuotaSnapshot,
) -> Result<QuotaSnapshot, FetchError> {
    let headers = build_headers(key);

    let whoami_url = endpoint(base, WHOAMI_PATH, None, None)?;
    let whoami = get_json(client, &whoami_url, &headers).await?;
    let org_id = extract_org_id(&whoami);

    let subscriptions_url = endpoint(base, SUBSCRIPTIONS_PATH, org_id.as_deref(), None)?;
    let credits_url = endpoint(base, CREDITS_PATH, org_id.as_deref(), None)?;
    let (subscriptions, credits) = tokio::join!(
        get_json(client, &subscriptions_url, &headers),
        get_json(client, &credits_url, &headers),
    );
    let subscriptions = subscriptions?;
    let credits = credits?;

    let mut snapshot = QuotaSnapshot::default();
    apply_subscriptions(&mut snapshot, &subscriptions, previous);
    apply_credits(&mut snapshot, &credits, previous);
    if let Some(monthly) = snapshot.monthly.as_mut() {
        // 算不出百分比就保留 None，由展示层渲染成 -- 或空条；月行只算套餐 credit。
        monthly.used_percent =
            month_percent(snapshot.plan.as_deref(), snapshot.monthly_credits_remaining);
    }
    snapshot.updated_at = Some(chrono::Utc::now().timestamp());

    if let Some(since) = current_period_start(&subscriptions) {
        let usage_url = endpoint(base, USAGE_PATH, None, Some(since))?;
        if let Ok(value) = get_json(client, &usage_url, &headers).await {
            apply_usage(&mut snapshot, &value);
        } else {
            eprintln!("command-code: 用量读取失败");
        }
    }
    Ok(snapshot)
}

fn endpoint(
    base: &str,
    path: &str,
    org_id: Option<&str>,
    since: Option<i64>,
) -> Result<Url, FetchError> {
    let mut url = Url::parse(base)
        .and_then(|base| base.join(path))
        .map_err(|_| FetchError::Transport)?;
    if org_id.is_some() || since.is_some() {
        let mut query = url.query_pairs_mut();
        if let Some(org_id) = org_id {
            query.append_pair("orgId", org_id);
        }
        if let Some(since) = since {
            query.append_pair("since", &since.to_string());
        }
    }
    Ok(url)
}

async fn get_json(client: &Client, url: &Url, headers: &HeaderMap) -> Result<Value, FetchError> {
    let response = client
        .get(url.clone())
        .headers(headers.clone())
        .send()
        .await
        .map_err(|_| FetchError::Transport)?;
    // 即使调用方传入跟随重定向的客户端，也拒绝跨源响应。
    if !same_origin(url, &response.url()) {
        return Err(FetchError::Transport);
    }
    if let Some(error) = classify_status(response.status().as_u16()) {
        return Err(error);
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| FetchError::Transport)
}

fn same_origin(requested: &Url, actual: &Url) -> bool {
    requested.scheme() == actual.scheme()
        && requested.host() == actual.host()
        && requested.port_or_known_default() == actual.port_or_known_default()
}

fn pick_subscription(value: &Value) -> Option<&Value> {
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .find(|item| {
                item.get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status.eq_ignore_ascii_case("active"))
            })
            .or_else(|| items.first());
    }
    value.is_object().then_some(value)
}

fn parse_window(value: Option<&Value>, previous: Option<&QuotaWindow>) -> Option<QuotaWindow> {
    let value = value?;
    let used_percent = used_cap_percent(number(value.get("used")), number(value.get("cap")))?;
    let resets_at = value
        .get("resetAt")
        .and_then(parse_reset_at)
        .or_else(|| previous.and_then(|window| window.resets_at));
    Some(QuotaWindow {
        used_percent: Some(used_percent),
        resets_at,
    })
}

fn string_value(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    value
        .as_i64()
        .map(|n| n.to_string())
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

fn number(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|n| n.is_finite())
}

fn count(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    let n = number(Some(value))?;
    // u64::MAX as f64 舍入到 2^64；严格小于才在安全范围内，否则 `as u64`
    // 会饱和成 u64::MAX 制造假数据，故降级为 None。
    (n >= 0.0 && n < u64::MAX as f64).then_some(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn classify_status_splits_auth_rate_limit_redirect_and_success() {
        assert_eq!(classify_status(200), None);
        assert_eq!(classify_status(204), None);
        assert_eq!(classify_status(404), None);
        assert_eq!(classify_status(401), Some(FetchError::Auth));
        assert_eq!(classify_status(403), Some(FetchError::Auth));
        assert_eq!(classify_status(429), Some(FetchError::RateLimited(429)));
        assert_eq!(classify_status(500), Some(FetchError::RateLimited(500)));
        assert_eq!(classify_status(503), Some(FetchError::RateLimited(503)));
        // 客户端禁用重定向，收到 3xx 表示没有拿到数据。
        assert_eq!(classify_status(302), Some(FetchError::Transport));
    }

    #[test]
    fn next_backoff_doubles_from_one_minute_and_caps_at_thirty_minutes() {
        let min = Duration::from_secs(60);
        assert_eq!(next_backoff(min), Duration::from_secs(120));
        assert_eq!(
            next_backoff(Duration::from_secs(120)),
            Duration::from_secs(240)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(240)),
            Duration::from_secs(480)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(30 * 60)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(3600)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(next_backoff(Duration::ZERO), min);
        assert_eq!(next_backoff(Duration::MAX), Duration::from_secs(30 * 60));
    }

    #[test]
    fn build_headers_sets_auth_and_cli_identifiers() {
        let headers = build_headers("test-key");
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer test-key"
        );
        assert_eq!(
            headers
                .get("x-command-code-version")
                .unwrap()
                .to_str()
                .unwrap(),
            "1.88.5"
        );
        assert_eq!(
            headers.get("x-cli-environment").unwrap().to_str().unwrap(),
            "production"
        );
        assert_eq!(headers.len(), 3);
    }

    #[test]
    fn extract_org_id_accepts_all_three_shapes() {
        assert_eq!(
            extract_org_id(&json!({"organizationId": "org_a"})).as_deref(),
            Some("org_a")
        );
        assert_eq!(
            extract_org_id(&json!({"orgId": "org_b"})).as_deref(),
            Some("org_b")
        );
        assert_eq!(
            extract_org_id(&json!({"organization": {"id": "org_c"}})).as_deref(),
            Some("org_c")
        );
        assert_eq!(
            extract_org_id(&json!({"organizationId": 123})).as_deref(),
            Some("123")
        );
        assert_eq!(
            extract_org_id(&json!({"organizationId": "  org_d  "})).as_deref(),
            Some("org_d")
        );
    }

    #[test]
    fn extract_org_id_prefers_first_field_and_skips_empty_values() {
        let value = json!({
            "organizationId": "first",
            "orgId": "second",
            "organization": {"id": "third"}
        });
        assert_eq!(extract_org_id(&value).as_deref(), Some("first"));
        assert_eq!(
            extract_org_id(&json!({"organizationId": "", "orgId": "org_b"})).as_deref(),
            Some("org_b")
        );
        assert_eq!(
            extract_org_id(&json!({"organizationId": null, "organization": {"id": "org_c"}}))
                .as_deref(),
            Some("org_c")
        );
        assert_eq!(extract_org_id(&json!({"user": "x"})), None);
    }

    #[test]
    fn apply_subscriptions_accepts_array_and_single_object() {
        let value = json!([
            {"planId": "GOAT", "status": "active", "currentPeriodStart": 1700000000, "currentPeriodEnd": 1702592000},
            {"planId": "old", "status": "canceled", "currentPeriodEnd": 1600000000}
        ]);
        let mut snapshot = QuotaSnapshot::default();
        apply_subscriptions(&mut snapshot, &value, &QuotaSnapshot::default());
        assert_eq!(snapshot.plan.as_deref(), Some("GOAT"));
        assert_eq!(
            snapshot.monthly.as_ref().unwrap().resets_at,
            Some(1702592000)
        );
        assert_eq!(current_period_start(&value), Some(1700000000));

        let single = json!({"planId": "pro_v1", "currentPeriodEnd": "2023-11-15T00:00:00Z"});
        let mut snapshot = QuotaSnapshot::default();
        apply_subscriptions(&mut snapshot, &single, &QuotaSnapshot::default());
        assert_eq!(snapshot.plan.as_deref(), Some("pro_v1"));
        assert!(snapshot.monthly.as_ref().unwrap().resets_at.is_some());
    }

    #[test]
    fn apply_subscriptions_keeps_previous_reset_at_and_degrades_missing_plan() {
        let previous = QuotaSnapshot {
            monthly: Some(QuotaWindow {
                used_percent: None,
                resets_at: Some(111),
            }),
            ..QuotaSnapshot::default()
        };
        let mut snapshot = QuotaSnapshot::default();
        apply_subscriptions(&mut snapshot, &json!({"planId": "Go"}), &previous);
        assert_eq!(snapshot.plan.as_deref(), Some("Go"));
        assert_eq!(snapshot.monthly.unwrap().resets_at, Some(111));

        let mut snapshot = QuotaSnapshot::default();
        apply_subscriptions(
            &mut snapshot,
            &json!({"status": "active"}),
            &QuotaSnapshot::default(),
        );
        assert_eq!(snapshot.plan, None);
        assert_eq!(snapshot.monthly, None);
    }

    #[test]
    fn apply_credits_maps_credit_sum_and_both_windows() {
        let value = json!({
            "credits": {"monthlyCredits": 50.5, "purchasedCredits": 10, "freeCredits": 2.4},
            "windowLimits": {
                "fiveHour": {"used": 3, "cap": 14, "resetAt": 1700007200},
                "weekly": {"used": 20, "cap": 70, "resetAt": "2023-11-20T00:00:00Z"}
            }
        });
        let mut snapshot = QuotaSnapshot::default();
        apply_credits(&mut snapshot, &value, &QuotaSnapshot::default());
        assert!((snapshot.credits_remaining.unwrap() - 62.9).abs() < 1e-9);
        assert!((snapshot.credits_extra.unwrap() - 12.4).abs() < 1e-9);
        assert!((snapshot.monthly_credits_remaining.unwrap() - 50.5).abs() < 1e-9);
        let five = snapshot.five_hour.unwrap();
        assert_eq!(five.used_percent, Some(21));
        assert_eq!(five.resets_at, Some(1700007200));
        let weekly = snapshot.weekly.unwrap();
        assert_eq!(weekly.used_percent, Some(29));
        assert!(weekly.resets_at.is_some());
    }

    #[test]
    fn apply_credits_keeps_monthly_credits_separate_from_extras() {
        let value = json!({
            "credits": {"monthlyCredits": 50, "purchasedCredits": 10, "freeCredits": 2.4}
        });
        let mut snapshot = QuotaSnapshot::default();
        apply_credits(&mut snapshot, &value, &QuotaSnapshot::default());
        assert_eq!(snapshot.monthly_credits_remaining, Some(50.0));
        assert!((snapshot.credits_remaining.unwrap() - 62.4).abs() < 1e-9);
        assert!((snapshot.credits_extra.unwrap() - 12.4).abs() < 1e-9);
        // 三项语义各不相同：套餐单项、三项之和、额外两项之和。
        assert_ne!(
            snapshot.monthly_credits_remaining,
            snapshot.credits_remaining
        );
        assert_ne!(snapshot.monthly_credits_remaining, snapshot.credits_extra);
        assert_ne!(snapshot.credits_extra, snapshot.credits_remaining);
    }

    #[test]
    fn apply_credits_keeps_old_reset_at_when_windows_omit_it() {
        let previous = QuotaSnapshot {
            five_hour: Some(QuotaWindow {
                used_percent: Some(10),
                resets_at: Some(1000),
            }),
            weekly: Some(QuotaWindow {
                used_percent: Some(5),
                resets_at: Some(2000),
            }),
            ..QuotaSnapshot::default()
        };
        let value = json!({"windowLimits": {
            "fiveHour": {"used": 3, "cap": 14},
            "weekly": {"used": 20, "cap": 70, "resetAt": null}
        }});
        let mut snapshot = QuotaSnapshot::default();
        apply_credits(&mut snapshot, &value, &previous);
        assert_eq!(snapshot.five_hour.unwrap().resets_at, Some(1000));
        assert_eq!(snapshot.weekly.unwrap().resets_at, Some(2000));
    }

    #[test]
    fn apply_credits_leaves_window_none_when_cap_or_used_missing() {
        let value = json!({"windowLimits": {
            "fiveHour": {"used": 3, "cap": null, "resetAt": 1700007200},
            "weekly": {"used": 20, "resetAt": 1700007200}
        }});
        let mut snapshot = QuotaSnapshot::default();
        apply_credits(&mut snapshot, &value, &QuotaSnapshot::default());
        // 没有可信的内置窗口上限，不从百分比反推，也不编数字。
        assert_eq!(snapshot.five_hour, None);
        assert_eq!(snapshot.weekly, None);
    }

    #[test]
    fn apply_credits_degrades_partial_credit_fields() {
        let mut snapshot = QuotaSnapshot::default();
        apply_credits(
            &mut snapshot,
            &json!({"credits": {"monthlyCredits": 50, "purchasedCredits": 10}}),
            &QuotaSnapshot::default(),
        );
        assert_eq!(snapshot.credits_remaining, None);
        assert_eq!(snapshot.credits_extra, None);
        // 套餐单项独立于三项之和：即使 free 缺失，单项仍可读。
        assert_eq!(snapshot.monthly_credits_remaining, Some(50.0));

        let mut snapshot = QuotaSnapshot::default();
        apply_credits(
            &mut snapshot,
            &json!({"credits": {"purchasedCredits": 10, "freeCredits": 2}}),
            &QuotaSnapshot::default(),
        );
        assert_eq!(snapshot.credits_remaining, None);
        assert_eq!(snapshot.credits_extra, Some(12.0));
        assert_eq!(snapshot.monthly_credits_remaining, None);

        let mut snapshot = QuotaSnapshot::default();
        apply_credits(&mut snapshot, &json!({}), &QuotaSnapshot::default());
        assert_eq!(snapshot.credits_remaining, None);
        assert_eq!(snapshot.credits_extra, None);
        assert_eq!(snapshot.monthly_credits_remaining, None);
    }

    #[test]
    fn count_rejects_finite_f64_above_u64_max() {
        // 有限 f64 超过 u64::MAX 时 `as u64` 会饱和成 u64::MAX，制造假数据。
        // u64::MAX as f64 舍入到 2^64，严格小于才算安全范围。
        assert_eq!(count(Some(&json!(1e30f64))), None);
        assert_eq!(count(Some(&json!(u64::MAX as f64))), None);
        // 范围内的浮点与 u64 整数正常转换。
        assert_eq!(count(Some(&json!(42.0f64))), Some(42));
        assert_eq!(count(Some(&json!(0.5f64))), Some(0));
        assert_eq!(count(Some(&json!(42u64))), Some(42));
        // 负数仍按原逻辑降级。
        assert_eq!(count(Some(&json!(-1.0f64))), None);
    }

    #[test]
    fn apply_usage_accepts_nested_and_root_fields() {
        let mut snapshot = QuotaSnapshot::default();
        apply_usage(
            &mut snapshot,
            &json!({"summary": {"totalCount": 18, "totalCost": 7.6, "totalTokens": 42000}}),
        );
        let usage = snapshot.usage.unwrap();
        assert_eq!(usage.total_count, Some(18));
        assert_eq!(usage.total_cost, Some(7.6));
        assert_eq!(usage.total_tokens, Some(42000));

        let mut snapshot = QuotaSnapshot::default();
        apply_usage(
            &mut snapshot,
            &json!({"totalCount": 3, "totalCost": 1.0, "totalTokens": 900}),
        );
        let usage = snapshot.usage.unwrap();
        assert_eq!(usage.total_count, Some(3));
        assert_eq!(usage.total_tokens, Some(900));

        let mut snapshot = QuotaSnapshot::default();
        apply_usage(&mut snapshot, &json!({"summary": {"totalCount": 5}}));
        let usage = snapshot.usage.unwrap();
        assert_eq!(usage.total_count, Some(5));
        assert_eq!(usage.total_cost, None);
        assert_eq!(usage.total_tokens, None);

        let mut snapshot = QuotaSnapshot::default();
        apply_usage(&mut snapshot, &json!({"error": "boom"}));
        assert_eq!(snapshot.usage, None);
    }
}
