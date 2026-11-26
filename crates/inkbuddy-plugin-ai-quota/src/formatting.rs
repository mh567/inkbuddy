//! 把额度数据转成宿主页面可直接使用的字符串。

/// 把 0..=100 的百分比四舍五入到最近的 5%，返回对应的进度条 class。
/// 超出范围的值钳到 0 或 100。
pub fn width_class(used_percent: u32) -> &'static str {
    const CLASSES: [&str; 21] = [
        "q-0", "q-5", "q-10", "q-15", "q-20", "q-25", "q-30", "q-35", "q-40", "q-45", "q-50",
        "q-55", "q-60", "q-65", "q-70", "q-75", "q-80", "q-85", "q-90", "q-95", "q-100",
    ];
    let bucket = used_percent.min(100);
    CLASSES[((bucket + 2) / 5) as usize]
}

/// 套餐名映射；按原始字符串精确匹配，未知值保留原文，缺失或空白返回「未知套餐」。
pub fn plan_label(raw: Option<&str>) -> String {
    let Some(value) = raw else {
        return "未知套餐".to_string();
    };
    // 空串或纯空白显示空白比降级文字更糟，统一回「未知套餐」。
    if value.trim().is_empty() {
        return "未知套餐".to_string();
    }
    match value {
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        "prolite" => "Pro Lite".to_string(),
        "promax" => "Pro Max".to_string(),
        "team" => "Team".to_string(),
        "business" => "Business".to_string(),
        "enterprise" => "Enterprise".to_string(),
        "edu" => "Edu".to_string(),
        "free" => "Free".to_string(),
        "go" => "Go".to_string(),
        _ => value.to_string(),
    }
}

/// 距重置时间的倒计时文字；分钟数向上取整。
pub fn reset_countdown(resets_at: Option<i64>, now: i64) -> String {
    let Some(resets_at) = resets_at else {
        return "重置时间未知".to_string();
    };
    let remaining = resets_at.saturating_sub(now);
    if remaining < 60 {
        return "即将重置".to_string();
    }
    let total_mins = remaining.saturating_add(59) / 60;
    format!("{}小时{}分后重置", total_mins / 60, total_mins % 60)
}

/// 转义 HTML 保留字符。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_class_clamps_and_rounds_to_five_percent_buckets() {
        // 超出范围钳到两端
        assert_eq!(width_class(0), "q-0");
        assert_eq!(width_class(101), "q-100");
        assert_eq!(width_class(u32::MAX), "q-100");

        // 5% 档位与边界取整（四舍五入到最近的 5）
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
        // 锁死 q-0..q-100 共 21 个 class 名这个契约，样式表据此定义宽度。
        let expected = [
            "q-0", "q-5", "q-10", "q-15", "q-20", "q-25", "q-30", "q-35", "q-40", "q-45", "q-50",
            "q-55", "q-60", "q-65", "q-70", "q-75", "q-80", "q-85", "q-90", "q-95", "q-100",
        ];
        for (i, name) in expected.iter().enumerate() {
            assert_eq!(width_class((i * 5) as u32), *name);
        }

        // 穷举 0..=100，输出必须落在既定 21 档内，不得出现意外命名。
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
    fn plan_label_maps_known_plans_and_falls_back() {
        let cases = [
            ("plus", "Plus"),
            ("pro", "Pro"),
            ("prolite", "Pro Lite"),
            ("promax", "Pro Max"),
            ("team", "Team"),
            ("business", "Business"),
            ("enterprise", "Enterprise"),
            ("edu", "Edu"),
            ("free", "Free"),
            ("go", "Go"),
        ];
        for (raw, expected) in cases {
            assert_eq!(plan_label(Some(raw)), expected);
        }

        // 大小写差异视为未知值，原样返回
        assert_eq!(plan_label(Some("PLUS")), "PLUS");
        // 带非空白内容的首尾空白也按原始字符串返回
        assert_eq!(plan_label(Some(" plus ")), " plus ");

        // 未知值保留原始文本；空串、纯空白与缺失值降级为「未知套餐」
        assert_eq!(plan_label(Some("mystery-plan")), "mystery-plan");
        assert_eq!(plan_label(Some("")), "未知套餐");
        assert_eq!(plan_label(Some("   ")), "未知套餐");
        assert_eq!(plan_label(None), "未知套餐");
    }

    #[test]
    fn reset_countdown_rounds_minutes_up_and_handles_missing_or_expired() {
        let now = 1_700_000_000_i64;

        // 4 小时 12 分 = 15120 秒
        assert_eq!(
            reset_countdown(Some(now + 4 * 3600 + 12 * 60), now),
            "4小时12分后重置"
        );
        // 分钟向上取整：4 小时 11 分 41 秒也显示 4 小时 12 分
        assert_eq!(
            reset_countdown(Some(now + 4 * 3600 + 11 * 60 + 41), now),
            "4小时12分后重置"
        );
        // 整点与不足一小时仍保留两个字段
        assert_eq!(reset_countdown(Some(now + 3600), now), "1小时0分后重置");
        assert_eq!(reset_countdown(Some(now + 60), now), "0小时1分后重置");
        // 周窗口上界：7 天 = 168 小时 0 分
        assert_eq!(
            reset_countdown(Some(now + 7 * 24 * 3600), now),
            "168小时0分后重置"
        );

        // 不足 1 分钟、恰好到期、已过期
        assert_eq!(reset_countdown(Some(now + 59), now), "即将重置");
        assert_eq!(reset_countdown(Some(now), now), "即将重置");
        assert_eq!(reset_countdown(Some(now - 1), now), "即将重置");

        // 缺失重置时间
        assert_eq!(reset_countdown(None, now), "重置时间未知");
    }

    #[test]
    fn escape_html_replaces_reserved_characters_once() {
        assert_eq!(
            escape_html("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &#39; f"
        );
        // 逐字符转义，不识别已有实体
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
        // 普通文本与非 ASCII 原样保留
        assert_eq!(escape_html("套餐 Plus 中文"), "套餐 Plus 中文");
        assert_eq!(escape_html(""), "");
    }
}
