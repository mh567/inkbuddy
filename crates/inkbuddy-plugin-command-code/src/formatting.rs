//! 把额度数据转成宿主页面可直接使用的字符串。

use crate::model::{normalize_plan, plan_credit_pool};

/// 把 0..=100 的百分比四舍五入到最近的 5%，返回对应的进度条 class。
/// 超出范围的值钳到 0 或 100。
/// 调用方拿到 `Option<u32>` 的 `None` 时渲染成 `--` 或空条；有具体百分比时才传给本函数。
pub fn width_class(used_percent: u32) -> &'static str {
    const CLASSES: [&str; 21] = [
        "q-0", "q-5", "q-10", "q-15", "q-20", "q-25", "q-30", "q-35", "q-40", "q-45", "q-50",
        "q-55", "q-60", "q-65", "q-70", "q-75", "q-80", "q-85", "q-90", "q-95", "q-100",
    ];
    let bucket = used_percent.min(100);
    CLASSES[((bucket + 2) / 5) as usize]
}

/// 转义 HTML 保留字符，供页面文本使用。
pub fn escape_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 毫秒时间戳阈值：达到这个量级按毫秒处理，否则按秒处理。
const MILLIS_THRESHOLD: i64 = 1_000_000_000_000;

/// 解析接口返回的 resetAt：毫秒时间戳、秒时间戳或 ISO 8601 字符串。
/// 无法识别时返回 `None`。
pub fn parse_reset_at(value: &serde_json::Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(timestamp_from_number(n));
    }
    if let Some(n) = value.as_u64() {
        // 超过 i64::MAX 的 u64 转型会回绕成负数（2^63 恰好回绕成 i64::MIN），
        // 这类值不是有效 Unix 时间戳，直接降级。
        if n > i64::MAX as u64 {
            return None;
        }
        return Some(timestamp_from_number(n as i64));
    }
    if let Some(f) = value.as_f64() {
        // serde_json::Number 不会持有 NaN/Infinity，is_finite 仅作防御。
        // 超出 i64 表示范围的浮点转型会饱和到 i64::MIN/MAX，产生假时间戳，故降级。
        // i64::MAX as f64 会舍入到 2^63，故用严格小于排除饱和到 i64::MAX 的值。
        if f.is_finite() && f >= i64::MIN as f64 && f < i64::MAX as f64 {
            return Some(timestamp_from_number(f as i64));
        }
        return None;
    }
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.trim().parse::<i64>() {
            return Some(timestamp_from_number(n));
        }
        return chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp());
    }
    None
}

/// 按数值大小区分毫秒与秒时间戳，不使用字符串长度判断。
fn timestamp_from_number(n: i64) -> i64 {
    // 用无符号绝对值避免 i64::MIN 取绝对值溢出（debug panic / release 回绕）。
    if n.unsigned_abs() >= MILLIS_THRESHOLD as u64 {
        n / 1000
    } else {
        n
    }
}

/// 距重置时间的倒计时文字；分钟数向上取整。
/// 不足 1 小时只写分钟，超过 24 小时换成天并略去分钟。
pub fn reset_countdown(resets_at: Option<i64>, now: i64) -> String {
    let Some(resets_at) = resets_at else {
        return "重置时间未知".to_string();
    };
    let remaining = resets_at.saturating_sub(now);
    if remaining < 60 {
        return "即将重置".to_string();
    }
    let total_mins = remaining.saturating_add(59) / 60;
    if total_mins < 60 {
        return format!("{total_mins}分钟后重置");
    }
    let (total_hours, mins) = (total_mins / 60, total_mins % 60);
    if total_hours < 24 {
        return format!("{total_hours}小时{mins}分后重置");
    }
    let (days, hours) = (total_hours / 24, total_hours % 24);
    if hours == 0 {
        return format!("{days}天后重置");
    }
    format!("{days}天{hours}小时后重置")
}

/// 用内置 credit 池上限和剩余 credit 算月度已用百分比。
/// 池子或剩余 credit 缺失、非有限时返回 `None`。
pub fn month_percent(plan: Option<&str>, credits_remaining: Option<f64>) -> Option<u32> {
    let pool = plan_credit_pool(plan)?;
    let remaining = credits_remaining?;
    if !remaining.is_finite() {
        return None;
    }
    Some(round_clamp_percent((pool - remaining) / pool * 100.0))
}

/// 计算 `used / cap * 100`，四舍五入并钳在 0..=100。
/// `used` 为 0 表示窗口全新，结果是 0%；`cap` 必须为正数，否则返回 `None`。
pub fn used_cap_percent(used: Option<f64>, cap: Option<f64>) -> Option<u32> {
    let used = used?;
    let cap = cap?;
    if !used.is_finite() || !cap.is_finite() || cap <= 0.0 {
        return None;
    }
    Some(round_clamp_percent(used / cap * 100.0))
}

/// 四舍五入到整数百分比并钳在 0..=100。
fn round_clamp_percent(value: f64) -> u32 {
    value.round().clamp(0.0, 100.0) as u32
}

/// 把 planId 映射成可读套餐名；内置档位按归一化结果不区分大小写匹配。
/// 未收录的档位保留接口原始值，缺失或空白返回「未知套餐」。
pub fn plan_label(raw: Option<&str>) -> String {
    let Some(value) = raw else {
        return "未知套餐".to_string();
    };
    if value.trim().is_empty() {
        return "未知套餐".to_string();
    }
    match normalize_plan(value).as_str() {
        "go" => "Go",
        "goat" => "GOAT",
        "pro" => "Pro",
        "max" => "Max",
        "ultra" => "Ultra",
        "teams pro" => "Teams Pro",
        _ => return value.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_class_clamps_and_rounds_to_five_percent_buckets() {
        assert_eq!(width_class(0), "q-0");
        assert_eq!(width_class(101), "q-100");
        assert_eq!(width_class(u32::MAX), "q-100");
        assert_eq!(width_class(1), "q-0");
        assert_eq!(width_class(2), "q-0");
        assert_eq!(width_class(3), "q-5");
        assert_eq!(width_class(7), "q-5");
        assert_eq!(width_class(8), "q-10");
        assert_eq!(width_class(50), "q-50");
        assert_eq!(width_class(97), "q-95");
        assert_eq!(width_class(98), "q-100");
        assert_eq!(width_class(100), "q-100");
    }

    #[test]
    fn width_class_covers_all_21_buckets() {
        // 锁死 q-0..q-100 共 21 个 class 名，样式表据此定义宽度。
        let expected = [
            "q-0", "q-5", "q-10", "q-15", "q-20", "q-25", "q-30", "q-35", "q-40", "q-45", "q-50",
            "q-55", "q-60", "q-65", "q-70", "q-75", "q-80", "q-85", "q-90", "q-95", "q-100",
        ];
        for (i, name) in expected.iter().enumerate() {
            assert_eq!(width_class((i * 5) as u32), *name);
        }

        // 穷举 0..=100，输出必须落在既定 21 档内。
        let allowed: std::collections::BTreeSet<&str> = expected.into_iter().collect();
        for p in 0..=100u32 {
            assert!(
                allowed.contains(width_class(p)),
                "unexpected width class for {p}: {}",
                width_class(p)
            );
        }

        // 单调不减：百分比增大时填充宽度不能回退。
        let bucket = |p: u32| {
            width_class(p)
                .trim_start_matches("q-")
                .parse::<u32>()
                .expect("class ends with a number")
        };
        for p in 0..100u32 {
            assert!(
                bucket(p) <= bucket(p + 1),
                "width class not monotonic at {p}"
            );
        }
    }

    #[test]
    fn escape_html_replaces_reserved_characters_once() {
        assert_eq!(
            escape_html("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &#39; f"
        );
        // 逐字符转义，不识别已有实体。
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
        // 普通文本与非 ASCII 原样保留。
        assert_eq!(escape_html("套餐 Ultra 中文"), "套餐 Ultra 中文");
        assert_eq!(escape_html(""), "");
    }

    #[test]
    fn parse_reset_at_accepts_millis_seconds_and_iso8601() {
        // 毫秒时间戳（13 位）按数值大小识别，转秒。
        assert_eq!(
            parse_reset_at(&serde_json::json!(1_700_000_000_000_i64)),
            Some(1_700_000_000)
        );
        // 秒时间戳（10 位）直接返回。
        assert_eq!(
            parse_reset_at(&serde_json::json!(1_700_000_000_i64)),
            Some(1_700_000_000)
        );
        // ISO 8601：Z、正偏移、负偏移。
        assert_eq!(
            parse_reset_at(&serde_json::json!("2023-11-14T22:13:20Z")),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_reset_at(&serde_json::json!("2023-11-14T22:13:20+00:00")),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_reset_at(&serde_json::json!("2023-11-15T06:13:20+08:00")),
            Some(1_700_000_000)
        );
        // 数字字符串同样按数值大小判断。
        assert_eq!(
            parse_reset_at(&serde_json::json!("1700000000000")),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_reset_at(&serde_json::json!("1700000000")),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn parse_reset_at_degrades_extreme_numbers_without_fake_values() {
        // i64::MIN 经 as_i64 分支：旧实现 n.abs() 在 debug 下 panic；
        // 改用 unsigned_abs 后按毫秒折算，得到稳定的秒值。
        assert_eq!(
            parse_reset_at(&serde_json::json!(i64::MIN)),
            Some(i64::MIN / 1000)
        );
        // 数字字符串分支的 i64::MIN，期望与上面一致。
        assert_eq!(
            parse_reset_at(&serde_json::json!("-9223372036854775808")),
            Some(i64::MIN / 1000)
        );
        // as_u64 分支：2^63 旧实现回绕成 i64::MIN 触发 panic，u64::MAX 返回 Some(-1) 假数据。
        // 超过 i64::MAX 的 u64 不是有效 Unix 时间戳，一律降级为 None。
        assert_eq!(
            parse_reset_at(&serde_json::json!(9_223_372_036_854_775_808u64)),
            None
        );
        assert_eq!(parse_reset_at(&serde_json::json!(u64::MAX)), None);
        // as_f64 分支：-1e30 经 f as i64 饱和到 i64::MIN，旧实现 panic；应降级为 None。
        assert_eq!(parse_reset_at(&serde_json::json!(-1e30f64)), None);
        // 正常范围内的浮点秒/毫秒按现有逻辑换算：小数部分截断。
        assert_eq!(
            parse_reset_at(&serde_json::json!(1_700_000_000.5f64)),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_reset_at(&serde_json::json!(1_700_000_000_000.5f64)),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn parse_reset_at_rejects_unparseable_values() {
        assert_eq!(parse_reset_at(&serde_json::json!(null)), None);
        assert_eq!(parse_reset_at(&serde_json::json!("not a time")), None);
        // ISO 8601 必须带时区信息。
        assert_eq!(
            parse_reset_at(&serde_json::json!("2023-11-14T22:13:20")),
            None
        );
        assert_eq!(parse_reset_at(&serde_json::json!(true)), None);
        assert_eq!(parse_reset_at(&serde_json::json!({})), None);
        assert_eq!(parse_reset_at(&serde_json::json!([])), None);
    }

    #[test]
    fn reset_countdown_rounds_minutes_up_and_handles_missing_or_expired() {
        let now = 1_700_000_000_i64;

        // 4 小时 12 分 = 15120 秒。
        assert_eq!(
            reset_countdown(Some(now + 4 * 3600 + 12 * 60), now),
            "4小时12分后重置"
        );
        // 分钟向上取整：4 小时 11 分 41 秒也显示 4 小时 12 分。
        assert_eq!(
            reset_countdown(Some(now + 4 * 3600 + 11 * 60 + 41), now),
            "4小时12分后重置"
        );
        assert_eq!(reset_countdown(Some(now + 3600), now), "1小时0分后重置");

        // 不足 1 小时只写分钟。
        assert_eq!(reset_countdown(Some(now + 60), now), "1分钟后重置");
        assert_eq!(reset_countdown(Some(now + 59 * 60), now), "59分钟后重置");

        // 超过 24 小时换成天并略去分钟。
        assert_eq!(
            reset_countdown(Some(now + 37 * 3600 + 29 * 60), now),
            "1天13小时后重置"
        );
        assert_eq!(
            reset_countdown(Some(now + 2 * 24 * 3600 + 3 * 3600), now),
            "2天3小时后重置"
        );
        // 整天边界：24 小时、7 天（周窗口上界）。
        assert_eq!(reset_countdown(Some(now + 24 * 3600), now), "1天后重置");
        assert_eq!(reset_countdown(Some(now + 7 * 24 * 3600), now), "7天后重置");
        // 23 小时 59 分仍按小时写，不进位到天。
        assert_eq!(
            reset_countdown(Some(now + 23 * 3600 + 59 * 60), now),
            "23小时59分后重置"
        );

        // 不足 1 分钟、恰好到期、已过期。
        assert_eq!(reset_countdown(Some(now + 59), now), "即将重置");
        assert_eq!(reset_countdown(Some(now), now), "即将重置");
        assert_eq!(reset_countdown(Some(now - 1), now), "即将重置");

        // 缺失重置时间。
        assert_eq!(reset_countdown(None, now), "重置时间未知");
    }

    #[test]
    fn month_percent_uses_builtin_pool_and_clamps() {
        // GOAT 池 70，剩余 62.4：7.6 / 70 * 100 ≈ 10.86 -> 11。
        assert_eq!(month_percent(Some("GOAT"), Some(62.4)), Some(11));
        assert_eq!(month_percent(Some("go"), Some(10.0)), Some(0));
        assert_eq!(month_percent(Some("pro_v1"), Some(0.0)), Some(100));
        // 剩余超出池子或为负，钳到 0..=100。
        assert_eq!(month_percent(Some("Max"), Some(200.0)), Some(0));
        assert_eq!(month_percent(Some("Max"), Some(-10.0)), Some(100));
        // 版本尾巴归一后查表：Teams Pro 池 40，剩余 20 -> 50%。
        assert_eq!(month_percent(Some("teams_pro_v1"), Some(20.0)), Some(50));
    }

    #[test]
    fn month_percent_returns_none_when_inputs_missing() {
        assert_eq!(month_percent(None, Some(10.0)), None);
        assert_eq!(month_percent(Some("GOAT"), None), None);
        assert_eq!(month_percent(Some("mystery"), Some(10.0)), None);
        assert_eq!(month_percent(Some("GOAT"), Some(f64::NAN)), None);
        assert_eq!(month_percent(Some("GOAT"), Some(f64::INFINITY)), None);
    }

    #[test]
    fn used_cap_percent_rounds_and_clamps_to_range() {
        assert_eq!(used_cap_percent(Some(3.0), Some(10.0)), Some(30));
        // 四舍五入：12.5 -> 13，87.5 -> 88。
        assert_eq!(used_cap_percent(Some(1.0), Some(8.0)), Some(13));
        assert_eq!(used_cap_percent(Some(7.0), Some(8.0)), Some(88));
        assert_eq!(used_cap_percent(Some(0.0), Some(10.0)), Some(0));
        assert_eq!(used_cap_percent(Some(10.0), Some(10.0)), Some(100));
        // 超出范围和负数钳到 0..=100。
        assert_eq!(used_cap_percent(Some(14.0), Some(10.0)), Some(100));
        assert_eq!(used_cap_percent(Some(-2.0), Some(10.0)), Some(0));
    }

    #[test]
    fn used_cap_percent_rejects_missing_and_invalid_inputs() {
        assert_eq!(used_cap_percent(None, Some(10.0)), None);
        assert_eq!(used_cap_percent(Some(1.0), None), None);
        assert_eq!(used_cap_percent(None, None), None);
        assert_eq!(used_cap_percent(Some(f64::NAN), Some(10.0)), None);
        assert_eq!(used_cap_percent(Some(1.0), Some(f64::NAN)), None);
        assert_eq!(used_cap_percent(Some(f64::INFINITY), Some(10.0)), None);
        assert_eq!(used_cap_percent(Some(1.0), Some(f64::INFINITY)), None);
        assert_eq!(used_cap_percent(Some(1.0), Some(0.0)), None);
        assert_eq!(used_cap_percent(Some(1.0), Some(-5.0)), None);
    }

    #[test]
    fn plan_label_maps_builtin_tiers_after_normalizing_version_tails() {
        let cases = [
            ("go", "Go"),
            ("GOAT", "GOAT"),
            ("pro", "Pro"),
            ("Max", "Max"),
            ("ultra", "Ultra"),
            ("Teams Pro", "Teams Pro"),
            ("go_v1", "Go"),
            ("GOAT-v2", "GOAT"),
            ("pro v3", "Pro"),
            ("teams_pro_v1", "Teams Pro"),
            ("TeamsPro", "Teams Pro"),
            (" Ultra ", "Ultra"),
        ];
        for (raw, expected) in cases {
            assert_eq!(plan_label(Some(raw)), expected, "plan {raw}");
        }
    }

    #[test]
    fn plan_label_keeps_unknown_raw_and_falls_back_for_empty() {
        // 未收录的档位保留接口原始值，包括大小写与版本尾巴。
        assert_eq!(plan_label(Some("mystery-plan")), "mystery-plan");
        assert_eq!(plan_label(Some("CUSTOM_v1")), "CUSTOM_v1");
        assert_eq!(plan_label(Some(" plus ")), " plus ");
        assert_eq!(plan_label(Some("")), "未知套餐");
        assert_eq!(plan_label(Some("   ")), "未知套餐");
        assert_eq!(plan_label(None), "未知套餐");
    }
}
