//! 把额度数据转成宿主页面可直接使用的字符串，以及时间解析。

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

/// 套餐名；缺失或空白时返回 `None`，页面上不占位。
///
/// 命令行版 `/usage` 的输出里没有套餐名，只有语言服务器能给出；
/// 读不到时宁可不显示，也不写一个占位词。
pub fn plan_text(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 距重置时间的倒计时文字；分钟数向上取整。
///
/// 不足 1 小时只写分钟，超过 24 小时换成天并略去分钟，
/// 免得周窗口显示成「37小时29分」这种要心算的长度。
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

/// 解析接口返回的 RFC 3339 时间，形如 `2026-12-03T11:18:49Z`。
///
/// 只处理到秒，小数秒直接丢掉；时区支持 `Z`、`+08:00` 与 `-05:00`，
/// 不带时区时按 UTC 处理。解析不出来返回 `None`，由调用方显示「重置时间未知」。
pub fn parse_rfc3339(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (date, rest) = raw.split_once('T')?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let (time, offset) = split_offset(rest)?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second_text = time_parts.next()?;
    if time_parts.next().is_some() {
        return None;
    }
    let second: i64 = second_text.split('.').next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(seconds - offset)
}

/// 把时间部分拆成「时分秒」和时区偏移秒数。
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix('Z') {
        return Some((time, 0));
    }
    // 时区符号出现在「时分秒」之后，从第 9 个字节起找（"11:18:49+08:00"）。
    let bytes = rest.as_bytes();
    let sign_at = bytes
        .iter()
        .enumerate()
        .skip(8)
        .find(|(_, byte)| **byte == b'+' || **byte == b'-')
        .map(|(index, _)| index)?;
    let (time, zone) = rest.split_at(sign_at);
    let (sign, zone) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        _ => (-1, &zone[1..]),
    };
    let (hours, minutes) = zone.split_once(':')?;
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    Some((time, sign * (hours * 3_600 + minutes * 60)))
}

/// 把公历年月日换成自 1970-01-01 起的天数（Howard Hinnant 的算法）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shifted = (month + 9) % 12;
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_class_clamps_and_rounds_to_five_percent_buckets() {
        assert_eq!(width_class(0), "q-0");
        assert_eq!(width_class(101), "q-100");
        assert_eq!(width_class(1), "q-0");
        assert_eq!(width_class(3), "q-5");
        assert_eq!(width_class(8), "q-10");
        assert_eq!(width_class(50), "q-50");
        assert_eq!(width_class(97), "q-95");
        assert_eq!(width_class(98), "q-100");
    }

    #[test]
    fn plan_text_trims_and_drops_empty() {
        assert_eq!(plan_text(Some("Pro")).as_deref(), Some("Pro"));
        assert_eq!(plan_text(Some(" Pro ")).as_deref(), Some("Pro"));
        assert_eq!(plan_text(Some("Ultra")).as_deref(), Some("Ultra"));
        assert_eq!(plan_text(Some("")), None);
        assert_eq!(plan_text(Some("   ")), None);
        assert_eq!(plan_text(None), None);
    }

    #[test]
    fn reset_countdown_rounds_minutes_up_and_handles_missing_or_expired() {
        let now = 1_700_000_000_i64;
        assert_eq!(
            reset_countdown(Some(now + 4 * 3600 + 12 * 60), now),
            "4小时12分后重置"
        );
        assert_eq!(reset_countdown(Some(now + 60), now), "1分钟后重置");
        assert_eq!(
            reset_countdown(Some(now + 37 * 3600 + 29 * 60), now),
            "1天13小时后重置"
        );
        assert_eq!(reset_countdown(Some(now + 24 * 3600), now), "1天后重置");
        assert_eq!(reset_countdown(Some(now + 59), now), "即将重置");
        assert_eq!(reset_countdown(None, now), "重置时间未知");
    }

    #[test]
    fn escape_html_replaces_reserved_characters_once() {
        assert_eq!(
            escape_html("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &#39; f"
        );
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
        assert_eq!(escape_html("Gemini 5 小时"), "Gemini 5 小时");
    }

    #[test]
    fn parse_rfc3339_handles_offset_and_fraction() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-27T18:39:20Z"), Some(1_795_804_760));
        // 带时区的同一时刻应换算成相等的 Unix 秒
        assert_eq!(
            parse_rfc3339("2026-11-28T02:39:20+08:00"),
            parse_rfc3339("2026-11-27T18:39:20Z")
        );
        assert_eq!(
            parse_rfc3339("2026-11-27T13:39:20-05:00"),
            parse_rfc3339("2026-11-27T18:39:20Z")
        );
        // 小数秒丢掉
        assert_eq!(
            parse_rfc3339("2026-11-27T18:39:20.123456Z"),
            parse_rfc3339("2026-11-27T18:39:20Z")
        );
    }

    #[test]
    fn parse_rfc3339_rejects_garbage() {
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-11-27 18:39:20"), None);
        assert_eq!(parse_rfc3339("2026-11-27T25:00:00Z"), None);
    }
}
