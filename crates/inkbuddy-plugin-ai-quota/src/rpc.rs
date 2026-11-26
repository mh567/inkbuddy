//! app-server 的 JSON-RPC 消息解析，以及把账号/额度数据合并进快照。

use serde_json::Value;

use crate::model::{QuotaSnapshot, QuotaWindow};

/// 未登录时给用户看的文案。
pub const LOGIN_REQUIRED: &str = "Codex 未登录，请先运行 codex login";

/// 一行 app-server 输出解析后的结果。
#[derive(Debug, PartialEq)]
pub enum RpcMessage {
    /// 带 id 的响应；`Err` 是错误对象/字符串里的可读文本。
    Response {
        id: u64,
        result: Result<Value, String>,
    },
    /// 不带 id 的通知。
    Notification { method: String, params: Value },
    /// 非法 JSON、缺字段或无法识别的消息。
    Unknown,
}

pub fn parse_message(line: &str) -> RpcMessage {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return RpcMessage::Unknown;
    };
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        if let Some(result) = value.get("result") {
            return RpcMessage::Response {
                id,
                result: Ok(result.clone()),
            };
        }
        if let Some(error) = value.get("error") {
            return RpcMessage::Response {
                id,
                result: Err(error_text(error)),
            };
        }
        return RpcMessage::Unknown;
    }
    match value.get("method").and_then(Value::as_str) {
        Some(method) => RpcMessage::Notification {
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        },
        None => RpcMessage::Unknown,
    }
}

/// `error` 可能是字符串或 `{message: "..."}` 对象，取其中的可读文本。
fn error_text(error: &Value) -> String {
    match error {
        Value::String(text) => text.clone(),
        _ => error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "unknown error".to_string()),
    }
}

/// 合并 `account/read` 结果或 `account/updated` 通知。
///
/// 账号为 null 时标记未登录；有可用 `planType` 时原样保存套餐名。
pub fn apply_account(snapshot: &mut QuotaSnapshot, value: &Value, now: i64) {
    match value.get("account") {
        Some(Value::Null) => {
            snapshot.plan = None;
            snapshot.failure = Some(LOGIN_REQUIRED.to_string());
        }
        Some(account) => {
            if let Some(plan) = account.get("planType").and_then(Value::as_str) {
                snapshot.plan = Some(plan.to_string());
            }
            snapshot.updated_at = Some(now);
            snapshot.failure = None;
        }
        // `account/updated` 通知：nullable 字段不清除旧值。
        None => {
            if let Some(plan) = value.get("planType").and_then(Value::as_str) {
                snapshot.plan = Some(plan.to_string());
                snapshot.updated_at = Some(now);
                snapshot.failure = None;
            }
        }
    }
}

/// 把一个额度窗口归类到 5 小时或周。
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowKind {
    FiveHour,
    Weekly,
}

fn classify_window(duration_mins: i64) -> Option<WindowKind> {
    if duration_mins <= 360 {
        Some(WindowKind::FiveHour)
    } else if duration_mins >= 1440 {
        Some(WindowKind::Weekly)
    } else {
        None
    }
}

fn window_from_value(value: &Value) -> Option<QuotaWindow> {
    // 没有 usedPercent 的窗口视为无效，不覆盖已有数据。
    let used_percent = value.get("usedPercent")?.as_i64()?;
    Some(QuotaWindow {
        used_percent: used_percent.clamp(0, 100) as u32,
        resets_at: value.get("resetsAt").and_then(Value::as_i64),
        window_duration_mins: value.get("windowDurationMins").and_then(Value::as_i64),
    })
}

fn assign_window(snapshot: &mut QuotaSnapshot, kind: WindowKind, window: QuotaWindow) {
    match kind {
        WindowKind::FiveHour => snapshot.five_hour = Some(window),
        WindowKind::Weekly => snapshot.weekly = Some(window),
    }
}

/// 合并额度读取结果或 `account/rateLimits/updated` 通知。
///
/// 窗口优先按 `windowDurationMins` 归类；两个窗口都无法区分类别时，回退到
/// primary→5 小时、secondary→周的位置约定。
pub fn apply_rate_limits(snapshot: &mut QuotaSnapshot, value: &Value, now: i64) {
    let Some(limits) = value.get("rateLimits").and_then(Value::as_object) else {
        return;
    };
    let primary = limits.get("primary").and_then(window_from_value);
    let secondary = limits.get("secondary").and_then(window_from_value);

    let primary_kind = primary
        .as_ref()
        .and_then(|window| window.window_duration_mins)
        .and_then(classify_window);
    let secondary_kind = secondary
        .as_ref()
        .and_then(|window| window.window_duration_mins)
        .and_then(classify_window);

    match (primary, secondary) {
        (Some(primary), Some(secondary)) => {
            match (primary_kind, secondary_kind) {
                (Some(primary_kind), Some(secondary_kind)) if primary_kind != secondary_kind => {
                    assign_window(snapshot, primary_kind, primary);
                    assign_window(snapshot, secondary_kind, secondary);
                }
                // 两个窗口无法区分类别时回退到位置约定。
                _ => {
                    assign_window(snapshot, WindowKind::FiveHour, primary);
                    assign_window(snapshot, WindowKind::Weekly, secondary);
                }
            }
        }
        // 只有一个窗口时仍按它自己的时长归类，无法归类再按位置。
        (Some(primary), None) => {
            let kind = primary_kind.unwrap_or(WindowKind::FiveHour);
            assign_window(snapshot, kind, primary);
        }
        (None, Some(secondary)) => {
            let kind = secondary_kind.unwrap_or(WindowKind::Weekly);
            assign_window(snapshot, kind, secondary);
        }
        (None, None) => {}
    }

    if let Some(count) = value
        .get("rateLimitResetCredits")
        .and_then(|credits| credits.get("availableCount"))
        .and_then(Value::as_u64)
    {
        snapshot.reset_credits = Some(count);
    }
    if snapshot.plan.is_none() {
        if let Some(plan) = limits.get("planType").and_then(Value::as_str) {
            snapshot.plan = Some(plan.to_string());
        }
    }

    snapshot.updated_at = Some(now);
    snapshot.failure = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::QuotaWindow;
    use serde_json::json;

    #[test]
    fn parse_message_reads_response_result() {
        match parse_message(r#"{"id":7,"result":{"account":null}}"#) {
            RpcMessage::Response { id, result } => {
                assert_eq!(id, 7);
                assert_eq!(result, Ok(json!({"account": null})));
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_reads_error_object_and_string() {
        match parse_message(r#"{"id":3,"error":{"code":-32602,"message":"bad params"}}"#) {
            RpcMessage::Response { id, result } => {
                assert_eq!(id, 3);
                assert_eq!(result, Err("bad params".to_string()));
            }
            other => panic!("expected response, got {other:?}"),
        }

        match parse_message(r#"{"id":4,"error":"boom"}"#) {
            RpcMessage::Response { id, result } => {
                assert_eq!(id, 4);
                assert_eq!(result, Err("boom".to_string()));
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_reads_notification() {
        match parse_message(r#"{"method":"account/updated","params":{"planType":"plus"}}"#) {
            RpcMessage::Notification { method, params } => {
                assert_eq!(method, "account/updated");
                assert_eq!(params, json!({"planType": "plus"}));
            }
            other => panic!("expected notification, got {other:?}"),
        }

        // 缺 params 的通知也要能识别。
        match parse_message(r#"{"method":"remoteControl/status/changed"}"#) {
            RpcMessage::Notification { method, params } => {
                assert_eq!(method, "remoteControl/status/changed");
                assert_eq!(params, Value::Null);
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_rejects_unknown_shapes() {
        for line in [
            "not json",
            "",
            "{}",
            r#"{"id":1}"#,
            r#"{"params":{}}"#,
            r#"{"id":"1","result":null}"#,
        ] {
            assert_eq!(parse_message(line), RpcMessage::Unknown, "line: {line}");
        }
    }

    #[test]
    fn apply_account_read_sets_plan_and_clears_failure() {
        let mut snapshot = QuotaSnapshot {
            failure: Some("旧错误".to_string()),
            ..QuotaSnapshot::default()
        };
        let value = json!({
            "account": {"type": "chatgpt", "email": "a@b", "planType": "plus"},
            "requiresOpenaiAuth": false
        });

        apply_account(&mut snapshot, &value, 1_700_000_000);

        assert_eq!(snapshot.plan.as_deref(), Some("plus"));
        assert_eq!(snapshot.updated_at, Some(1_700_000_000));
        assert_eq!(snapshot.failure, None);
    }

    #[test]
    fn apply_account_notification_updates_plan() {
        let mut snapshot = QuotaSnapshot::default();
        let value = json!({"authMode": "chatgpt", "planType": "pro", "reason": null});

        apply_account(&mut snapshot, &value, 42);

        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(snapshot.updated_at, Some(42));
        assert_eq!(snapshot.failure, None);
    }

    #[test]
    fn apply_account_null_marks_unauthenticated() {
        let mut snapshot = QuotaSnapshot {
            plan: Some("plus".to_string()),
            ..QuotaSnapshot::default()
        };

        apply_account(
            &mut snapshot,
            &json!({"account": null, "requiresOpenaiAuth": true}),
            7,
        );

        assert_eq!(
            snapshot.failure.as_deref(),
            Some("Codex 未登录，请先运行 codex login")
        );
        assert_eq!(snapshot.plan, None);
    }

    #[test]
    fn apply_rate_limits_maps_windows_by_duration() {
        let mut snapshot = QuotaSnapshot {
            failure: Some("旧错误".to_string()),
            ..QuotaSnapshot::default()
        };
        let value = json!({
            "rateLimits": {
                "planType": "plus",
                "primary": {"usedPercent": 37, "windowDurationMins": 300, "resetsAt": 1_700_000_100},
                "secondary": {"usedPercent": 51, "windowDurationMins": 10080, "resetsAt": 1_700_500_000}
            },
            "rateLimitResetCredits": {"availableCount": 2}
        });

        apply_rate_limits(&mut snapshot, &value, 99);

        assert_eq!(
            snapshot.five_hour,
            Some(QuotaWindow {
                used_percent: 37,
                resets_at: Some(1_700_000_100),
                window_duration_mins: Some(300),
            })
        );
        assert_eq!(
            snapshot.weekly,
            Some(QuotaWindow {
                used_percent: 51,
                resets_at: Some(1_700_500_000),
                window_duration_mins: Some(10080),
            })
        );
        assert_eq!(snapshot.reset_credits, Some(2));
        assert_eq!(snapshot.plan.as_deref(), Some("plus"));
        assert_eq!(snapshot.updated_at, Some(99));
        assert_eq!(snapshot.failure, None);
    }

    #[test]
    fn apply_rate_limits_classifies_swapped_windows_by_duration() {
        let mut snapshot = QuotaSnapshot::default();
        let value = json!({
            "rateLimits": {
                "primary": {"usedPercent": 51, "windowDurationMins": 10080},
                "secondary": {"usedPercent": 37, "windowDurationMins": 300}
            }
        });

        apply_rate_limits(&mut snapshot, &value, 1);

        assert_eq!(snapshot.five_hour.map(|w| w.used_percent), Some(37));
        assert_eq!(snapshot.weekly.map(|w| w.used_percent), Some(51));
    }

    #[test]
    fn apply_rate_limits_allows_missing_secondary() {
        let mut snapshot = QuotaSnapshot {
            weekly: Some(QuotaWindow {
                used_percent: 70,
                ..QuotaWindow::default()
            }),
            ..QuotaSnapshot::default()
        };
        let value = json!({
            "rateLimits": {
                "primary": {"usedPercent": 12, "windowDurationMins": 300},
                "secondary": null
            }
        });

        apply_rate_limits(&mut snapshot, &value, 5);

        assert_eq!(snapshot.five_hour.map(|w| w.used_percent), Some(12));
        // 缺失窗口沿用旧值，不被清空。
        assert_eq!(snapshot.weekly.map(|w| w.used_percent), Some(70));
    }

    #[test]
    fn apply_rate_limits_classifies_the_only_window_by_duration() {
        let mut snapshot = QuotaSnapshot::default();
        apply_rate_limits(
            &mut snapshot,
            &json!({"rateLimits": {"primary": {"usedPercent": 9, "windowDurationMins": 10080}}}),
            1,
        );
        assert_eq!(snapshot.five_hour, None);
        assert_eq!(snapshot.weekly.map(|w| w.used_percent), Some(9));

        let mut snapshot = QuotaSnapshot::default();
        apply_rate_limits(
            &mut snapshot,
            &json!({"rateLimits": {"secondary": {"usedPercent": 8, "windowDurationMins": 300}}}),
            1,
        );
        assert_eq!(snapshot.five_hour.map(|w| w.used_percent), Some(8));
        assert_eq!(snapshot.weekly, None);
    }

    #[test]
    fn apply_rate_limits_skips_window_without_used_percent() {
        let mut snapshot = QuotaSnapshot::default();
        let value = json!({
            "rateLimits": {
                "primary": {"windowDurationMins": 300},
                "secondary": {"usedPercent": 51, "windowDurationMins": 10080}
            }
        });

        apply_rate_limits(&mut snapshot, &value, 5);

        assert_eq!(snapshot.five_hour, None);
        assert_eq!(snapshot.weekly.map(|w| w.used_percent), Some(51));
    }

    #[test]
    fn apply_rate_limits_keeps_old_credits_when_missing() {
        let mut snapshot = QuotaSnapshot {
            reset_credits: Some(5),
            ..QuotaSnapshot::default()
        };
        let value = json!({
            "rateLimits": {"primary": {"usedPercent": 1, "windowDurationMins": 300}}
        });

        apply_rate_limits(&mut snapshot, &value, 5);

        assert_eq!(snapshot.reset_credits, Some(5));
    }

    #[test]
    fn apply_rate_limits_fills_plan_only_when_empty() {
        let mut snapshot = QuotaSnapshot {
            plan: None,
            ..QuotaSnapshot::default()
        };
        apply_rate_limits(
            &mut snapshot,
            &json!({"rateLimits": {"planType": "pro", "primary": {"usedPercent": 1}}}),
            5,
        );
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));

        // 已有套餐时不覆盖。
        apply_rate_limits(
            &mut snapshot,
            &json!({"rateLimits": {"planType": "plus", "primary": {"usedPercent": 1}}}),
            6,
        );
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
    }

    #[test]
    fn apply_rate_limits_ignores_payload_without_rate_limits() {
        let before = QuotaSnapshot {
            plan: Some("plus".to_string()),
            reset_credits: Some(1),
            updated_at: Some(10),
            ..QuotaSnapshot::default()
        };
        let mut snapshot = before.clone();

        apply_rate_limits(&mut snapshot, &json!({"availableCount": 3}), 20);

        assert_eq!(snapshot, before);
    }

    #[test]
    fn apply_rate_limits_clamps_percent_into_range() {
        let mut snapshot = QuotaSnapshot::default();
        let value = json!({
            "rateLimits": {
                "primary": {"usedPercent": -8, "windowDurationMins": 300},
                "secondary": {"usedPercent": 150, "windowDurationMins": 10080}
            }
        });

        apply_rate_limits(&mut snapshot, &value, 1);

        assert_eq!(snapshot.five_hour.map(|w| w.used_percent), Some(0));
        assert_eq!(snapshot.weekly.map(|w| w.used_percent), Some(100));
    }
}
