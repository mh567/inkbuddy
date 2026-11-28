//! 从本机语言服务器取额度：先试候选端口，成功后解析成分组快照。

use std::time::Duration;

use serde_json::Value;

use crate::discovery::{self, Candidate};
use crate::formatting::parse_rfc3339;
use crate::http;
use crate::model::{Bucket, Snapshot};

/// 额度接口前缀；两个方法都在同一个 Connect 服务上。
const RPC_PREFIX: &str = "/exa.language_server_pb.LanguageServerService/";
/// 分组额度（5 小时与周两个窗口）在这条方法上。
const METHOD_QUOTA_SUMMARY: &str = "RetrieveUserQuotaSummary";
/// 套餐名在这条方法上。
const METHOD_USER_STATUS: &str = "GetUserStatus";

/// 额度请求的体；服务端只要求一个空对象。
const QUOTA_SUMMARY_BODY: &str = "{}";

/// 单个端口的探测上限；回环上端口关着会立刻拒绝，这个上限只防进程卡住。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
/// 读套餐名的上限；这条响应带账号信息和头像，体量在 100 KiB 量级。
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
/// 一轮取数的总预算。宿主首帧只等 3 秒，端口多的时候不能一路等满。
const COLLECT_BUDGET: Duration = Duration::from_millis(2500);
/// 剩余预算低于这个值就不再试下一个端口，免得开头就超时。
const MIN_PROBE_BUDGET: Duration = Duration::from_millis(100);

/// 没装反重力。
pub const FAILURE_NOT_FOUND: &str = "未找到 Antigravity";
/// 装了但语言服务器没在跑。
pub const FAILURE_NOT_RUNNING: &str = "反重力未运行";
/// 找到进程但读不出来（令牌过期、版本改了接口等）。
pub const FAILURE_READ: &str = "额度读取失败";

/// 一次取数失败：给用户看的原因，加上每个候选端口的细节。
///
/// 细节不直接打日志：反重力关着时每 15 秒重试一次，逐端口打会把宿主日志刷满，
/// 由服务在「失败原因变了」时打一次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub reason: &'static str,
    pub details: Vec<String>,
    /// 是否有端口应答过。应答过说明反重力在跑，只是读不出来（命令行版就是这种情况），
    /// 值得再走一次命令行版的 `/usage`。
    pub answered: bool,
}

impl Failure {
    fn new(reason: &'static str, details: Vec<String>, answered: bool) -> Self {
        Self {
            reason,
            details,
            answered,
        }
    }
}

/// 取一次额度；失败时返回给用户看的文案。
///
/// 先按语言服务器读；有端口应答却读不出来，说明是命令行版（令牌留在进程内），
/// 改走 `agy -p "/usage"`。反重力整个没开时端口一个都不应答，但额度照样会
/// 变（重置发生在服务端），本机有 `agy` 时也走一次命令行版；没有才报「未运行」。
pub async fn collect() -> Result<Snapshot, Failure> {
    let failure = match collect_candidates(discovery::candidates().await).await {
        Ok(snapshot) => return Ok(snapshot),
        Err(failure) => failure,
    };
    if !failure.answered && crate::cli::binary().is_none() {
        return Err(failure);
    }

    match crate::cli::collect().await {
        Ok(snapshot) => Ok(snapshot),
        Err(detail) => {
            let mut details = failure.details;
            details.push(detail);
            Err(Failure::new(failure.reason, details, failure.answered))
        }
    }
}

/// 用给定候选端口取一次额度。候选为空时按「装没装」区分提示。
pub async fn collect_candidates(candidates: Vec<Candidate>) -> Result<Snapshot, Failure> {
    let mut answered = false;
    let mut details = Vec::new();
    let deadline = tokio::time::Instant::now() + COLLECT_BUDGET;

    'candidates: for candidate in &candidates {
        for port in &candidate.ports {
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if budget < MIN_PROBE_BUDGET {
                details.push("本轮取数预算用尽，跳过剩余的候选端口".to_string());
                break 'candidates;
            }
            let summary = match call(
                *port,
                candidate.csrf.as_deref(),
                METHOD_QUOTA_SUMMARY,
                QUOTA_SUMMARY_BODY,
                PROBE_TIMEOUT.min(budget),
            )
            .await
            {
                Ok(summary) => summary,
                Err(err) => {
                    if matches!(err, CallError::Rejected(_)) {
                        answered = true;
                    }
                    details.push(format!("端口 {port}: {}", err.message()));
                    continue;
                }
            };

            let mut snapshot = Snapshot::default();
            apply_quota_summary(&mut snapshot, &summary);
            if snapshot.gemini.is_empty() && snapshot.third_party.is_empty() {
                // 端口通了但不是额度接口，换下一个候选。
                answered = true;
                details.push(format!("端口 {port}: 响应里没有额度分组"));
                continue;
            }

            // 套餐名读不到不影响额度显示，按尽力而为处理。
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(status) = call(
                *port,
                candidate.csrf.as_deref(),
                METHOD_USER_STATUS,
                user_status_body(),
                STATUS_TIMEOUT.min(budget.max(MIN_PROBE_BUDGET)),
            )
            .await
            {
                snapshot.plan = parse_plan(&status);
            }
            return Ok(snapshot);
        }
    }

    Err(Failure::new(
        failure_for(answered, discovery::app_installed()),
        details,
        answered,
    ))
}

/// 一次都没读到时的提示：有人应答过（哪怕回的是 401）说明进程在跑，
/// 一个端口都连不上则说明反重力根本没开；连 app 都没装另说。
fn failure_for(answered: bool, installed: bool) -> &'static str {
    if !installed {
        FAILURE_NOT_FOUND
    } else if answered {
        FAILURE_READ
    } else {
        FAILURE_NOT_RUNNING
    }
}

/// 一次 RPC 调用的失败原因。
enum CallError {
    /// 连不上或超时：端口上没有服务在听。
    Unreachable(String),
    /// 有 HTTP 响应但用不了：状态码不对或响应不是 JSON。
    Rejected(String),
}

impl CallError {
    fn message(&self) -> &str {
        match self {
            Self::Unreachable(message) | Self::Rejected(message) => message,
        }
    }
}

/// `GetUserStatus` 的请求体；服务端按这些字段识别调用方。
fn user_status_body() -> &'static str {
    r#"{"metadata":{"ideName":"antigravity","extensionName":"antigravity","locale":"en"}}"#
}

async fn call(
    port: u16,
    csrf: Option<&str>,
    method: &str,
    body: &str,
    timeout: Duration,
) -> Result<Value, CallError> {
    let path = format!("{RPC_PREFIX}{method}");
    let response = http::post_json(port, &path, csrf, body, timeout)
        .await
        .map_err(CallError::Unreachable)?;
    if response.status != 200 {
        return Err(CallError::Rejected(format!(
            "{method} 返回 HTTP {}",
            response.status
        )));
    }
    let value: Value = serde_json::from_str(&response.body)
        .map_err(|err| CallError::Rejected(format!("{method} 响应不是 JSON: {err}")))?;
    Ok(value.get("response").cloned().unwrap_or(value))
}

/// 把 `RetrieveUserQuotaSummary` 的分组写进快照。
pub fn apply_quota_summary(snapshot: &mut Snapshot, value: &Value) {
    let Some(groups) = value.get("groups").and_then(Value::as_array) else {
        return;
    };
    for group in groups {
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else {
            continue;
        };
        let display_name = group
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let group_name = display_name.unwrap_or("").to_ascii_lowercase();

        for bucket in buckets {
            let Some((target, is_weekly)) = classify_bucket(bucket, &group_name) else {
                continue;
            };
            let Some(parsed) = parse_bucket(bucket) else {
                continue;
            };
            let group_slot = match target {
                Target::Gemini => &mut snapshot.gemini,
                Target::ThirdParty => &mut snapshot.third_party,
            };
            // 组名用接口原文，页面标题跟着接口走；接口没给时渲染层退到内置短标题。
            if let Some(name) = display_name {
                group_slot.name = Some(name.to_string());
            }
            let slot = if is_weekly {
                &mut group_slot.weekly
            } else {
                &mut group_slot.five_hour
            };
            *slot = Some(parsed);
        }
    }
}

#[derive(Clone, Copy)]
enum Target {
    Gemini,
    ThirdParty,
}

/// 认出桶属于哪一组、是周窗口还是 5 小时窗口。
fn classify_bucket(bucket: &Value, group_name: &str) -> Option<(Target, bool)> {
    let id = bucket
        .get("bucketId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let window = bucket
        .get("window")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    let target = if id.starts_with("gemini") || group_name.contains("gemini") {
        Target::Gemini
    } else if id.starts_with("3p") || group_name.contains("claude") {
        Target::ThirdParty
    } else {
        return None;
    };

    let is_weekly = id.ends_with("weekly") || window == "weekly";
    let is_five_hour = id.ends_with("5h") || window == "5h";
    if is_weekly {
        Some((target, true))
    } else if is_five_hour {
        Some((target, false))
    } else {
        None
    }
}

/// 一个桶的剩余比例换成已用百分比。
///
/// 接口用 protobuf 的默认值省略规则：`remainingFraction` 为 0 时字段不出现，
/// 所以缺字段表示额度用尽；只在给了 `remainingAmount`（没有百分比）时跳过。
fn parse_bucket(bucket: &Value) -> Option<Bucket> {
    let fraction = match bucket.get("remainingFraction").and_then(Value::as_f64) {
        Some(fraction) => fraction,
        None => {
            if bucket.get("remainingAmount").is_some() {
                return None;
            }
            0.0
        }
    };
    let fraction = fraction.clamp(0.0, 1.0);
    let used = ((1.0 - fraction) * 100.0).round().clamp(0.0, 100.0) as u32;
    let resets_at = bucket
        .get("resetTime")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    Some(Bucket {
        used_percent: used,
        resets_at,
    })
}

/// 套餐名；`userStatus` 可能被包一层，两种都认。
///
/// 接口把套餐放在 `planStatus.planInfo` 里，不同版本用过 `planName` 和
/// `planDisplayName`，也见过少一层的写法，这里按顺序都试一遍。
pub fn parse_plan(value: &Value) -> Option<String> {
    let status = value.get("userStatus").unwrap_or(value);
    let plan_info = status.get("planStatus").and_then(|plan| plan.get("planInfo"));
    let candidates = [
        plan_info.and_then(|info| info.get("planDisplayName")),
        plan_info.and_then(|info| info.get("planName")),
        status.get("planInfo").and_then(|info| info.get("planName")),
        status.get("planStatus").and_then(|plan| plan.get("planName")),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|plan| !plan.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary() -> Value {
        json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "description": "Models within this group: ...",
                    "buckets": [
                        {
                            "bucketId": "gemini-weekly",
                            "displayName": "Weekly Limit Remaining",
                            "window": "weekly",
                            "remainingFraction": 0.88726187,
                            "resetTime": "2026-12-03T11:18:49Z"
                        },
                        {
                            "bucketId": "gemini-5h",
                            "window": "5h",
                            "remainingFraction": 0.7641715,
                            "resetTime": "2026-11-27T18:39:20Z"
                        }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {"bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 1},
                        {"bucketId": "3p-5h", "window": "5h", "remainingFraction": 1}
                    ]
                }
            ]
        })
    }

    #[test]
    fn maps_groups_and_converts_remaining_to_used() {
        let mut snapshot = Snapshot::default();
        apply_quota_summary(&mut snapshot, &summary());

        let weekly = snapshot.gemini.weekly.expect("gemini weekly");
        assert_eq!(weekly.used_percent, 11);
        assert_eq!(weekly.resets_at, parse_rfc3339("2026-12-03T11:18:49Z"));

        let five_hour = snapshot.gemini.five_hour.expect("gemini 5h");
        assert_eq!(five_hour.used_percent, 24);
        // 组名照接口原文留下，页面标题用它
        assert_eq!(snapshot.gemini.name.as_deref(), Some("Gemini Models"));

        let third = snapshot.third_party.clone();
        assert_eq!(
            third.name.as_deref(),
            Some("Claude and GPT models"),
            "3p group name"
        );
        assert_eq!(third.weekly.expect("3p weekly").used_percent, 0);
        let third_five_hour = third.five_hour.expect("3p 5h");
        assert_eq!(third_five_hour.used_percent, 0);
        // 额度满且没用过时接口不返回 resetTime
        assert_eq!(third_five_hour.resets_at, None);
    }

    #[test]
    fn missing_fraction_means_exhausted() {
        let mut snapshot = Snapshot::default();
        apply_quota_summary(
            &mut snapshot,
            &json!({
                "groups": [{
                    "displayName": "Gemini Models",
                    "buckets": [{"bucketId": "gemini-5h", "window": "5h"}]
                }]
            }),
        );
        assert_eq!(snapshot.gemini.five_hour.expect("5h").used_percent, 100);
    }

    #[test]
    fn skips_buckets_with_only_amount() {
        let mut snapshot = Snapshot::default();
        apply_quota_summary(
            &mut snapshot,
            &json!({
                "groups": [{
                    "displayName": "Gemini Models",
                    "buckets": [{"bucketId": "gemini-5h", "window": "5h", "remainingAmount": 3}]
                }]
            }),
        );
        assert!(snapshot.gemini.is_empty());
    }

    #[test]
    fn ignores_unknown_groups_and_windows() {
        let mut snapshot = Snapshot::default();
        apply_quota_summary(
            &mut snapshot,
            &json!({
                "groups": [
                    {"displayName": "Flow models", "buckets": [{"bucketId": "flow-5h", "window": "5h", "remainingFraction": 0.5}]},
                    {"displayName": "Gemini Models", "buckets": [{"bucketId": "gemini-daily", "window": "daily", "remainingFraction": 0.5}]}
                ]
            }),
        );
        assert!(snapshot.gemini.is_empty());
        assert!(snapshot.third_party.is_empty());
    }

    #[test]
    fn falls_back_to_group_name_when_bucket_id_missing() {
        let mut snapshot = Snapshot::default();
        apply_quota_summary(
            &mut snapshot,
            &json!({
                "groups": [{
                    "displayName": "Claude and GPT models",
                    "buckets": [{"window": "weekly", "remainingFraction": 0.5}]
                }]
            }),
        );
        assert_eq!(snapshot.third_party.weekly.expect("weekly").used_percent, 50);
    }

    #[test]
    fn reads_plan_from_both_shapes() {
        let wrapped = json!({"userStatus": {"planStatus": {"planInfo": {"planName": "Pro"}}}});
        let bare = json!({"planStatus": {"planInfo": {"planName": "Ultra"}}});
        assert_eq!(parse_plan(&wrapped).as_deref(), Some("Pro"));
        assert_eq!(parse_plan(&bare).as_deref(), Some("Ultra"));
        assert_eq!(parse_plan(&json!({"userStatus": {"planStatus": {}}})), None);
        assert_eq!(parse_plan(&json!({"userStatus": {"planStatus": {"planInfo": {"planName": "  "}}}})), None);
    }

    #[test]
    fn reads_plan_from_other_field_names() {
        // 带显示名的写法优先，值本身就是给人看的（如 "Google AI Pro"）
        let display = json!({
            "userStatus": {"planStatus": {"planInfo": {"planName": "Pro", "planDisplayName": "Google AI Pro"}}}
        });
        assert_eq!(parse_plan(&display).as_deref(), Some("Google AI Pro"));

        // 少一层的写法也认
        let flat = json!({"planInfo": {"planName": "Pro"}});
        assert_eq!(parse_plan(&flat).as_deref(), Some("Pro"));
        let flat_status = json!({"planStatus": {"planName": "Ultra"}});
        assert_eq!(parse_plan(&flat_status).as_deref(), Some("Ultra"));

        // 只有空白值时当作没有
        let blank_display = json!({
            "userStatus": {"planStatus": {"planInfo": {"planDisplayName": " ", "planName": "Pro"}}}
        });
        assert_eq!(parse_plan(&blank_display).as_deref(), Some("Pro"));
    }

    #[test]
    fn failure_reason_follows_install_and_answer_state() {
        assert_eq!(failure_for(false, false), FAILURE_NOT_FOUND);
        assert_eq!(failure_for(true, false), FAILURE_NOT_FOUND);
        assert_eq!(failure_for(false, true), FAILURE_NOT_RUNNING);
        assert_eq!(failure_for(true, true), FAILURE_READ);
    }

    #[test]
    fn unparseable_reset_time_leaves_none() {        let mut snapshot = Snapshot::default();
        apply_quota_summary(
            &mut snapshot,
            &json!({
                "groups": [{
                    "displayName": "Gemini Models",
                    "buckets": [{"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.5, "resetTime": "soon"}]
                }]
            }),
        );
        let bucket = snapshot.gemini.five_hour.expect("5h");
        assert_eq!(bucket.used_percent, 50);
        assert_eq!(bucket.resets_at, None);
    }
}
