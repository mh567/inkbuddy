//! 把额度快照渲染成宿主可用的 HTML 块。
//!
//! 宿主内容模型只接受 `html` 块，白名单标签内的属性只有 `class`，所以进度条宽度
//! 全部走 `q-0`..`q-100` 预设 class，图标写在 `web/ink.html` 的样式表里。

use crate::formatting::{escape_html, plan_label, reset_countdown, width_class};
use crate::model::{QuotaSnapshot, QuotaWindow, FAILURE_NOT_FOUND, LOGIN_REQUIRED};

/// 渲染完整的额度看板；快照未就绪或失败时降级为提示块。
pub fn render_html(snapshot: &QuotaSnapshot, now: i64) -> String {
    // 未登录与找不到额度数据是终态：旧百分比会误导用户，整块换成提示。
    if let Some(failure) = snapshot.failure.as_deref() {
        if failure == LOGIN_REQUIRED || failure == FAILURE_NOT_FOUND {
            return render_notice(failure);
        }
    }

    let Some(updated_at) = snapshot.updated_at else {
        return match snapshot.failure.as_deref() {
            Some(failure) => render_notice(failure),
            None => render_notice("正在读取额度…"),
        };
    };

    let plan = plan_label(snapshot.plan.as_deref());
    let meta = match snapshot.credits_remaining {
        Some(credits) => format!("{} · 余 {:.1}", plan, credits),
        None => plan,
    };

    let mut out = String::new();
    out.push_str("<div class=\"quota\">\n");
    out.push_str("  <div class=\"quota-head\">\n");
    out.push_str("    <span class=\"quota-mark quota-mark-cc\"></span>\n");
    out.push_str("    <span class=\"quota-brand\">Command Code</span>\n");
    out.push_str(&format!(
        "    <span class=\"quota-meta\">{}</span>\n",
        escape_html(&meta)
    ));
    out.push_str("  </div>\n");
    out.push_str(&render_row("5 小时", snapshot.five_hour.as_ref(), now));
    out.push_str(&render_row("本周", snapshot.weekly.as_ref(), now));
    out.push_str(&render_row("本月", snapshot.monthly.as_ref(), now));
    out.push_str("</div>");

    if snapshot.failure.is_some() {
        out.push_str(&format!(
            "\n<div class=\"quota-stale\">{}</div>",
            escape_html(&stale_text(updated_at, now))
        ));
    }

    out
}

/// 单行提示，用于插件失败或快照未就绪。
pub fn render_notice(message: &str) -> String {
    format!(
        "<div class=\"quota quota-notice\"><span class=\"quota-notice-text\">{}</span></div>",
        escape_html(message)
    )
}

fn render_row(label: &str, window: Option<&QuotaWindow>, now: i64) -> String {
    let (class, percent, reset) = match window.and_then(|w| w.used_percent) {
        Some(p) => (
            width_class(p),
            format!("{p}%"),
            reset_countdown(window.and_then(|w| w.resets_at), now),
        ),
        None => ("q-0", "--".to_string(), "暂无数据".to_string()),
    };
    format!(
        "  <div class=\"quota-row\">\n    <span class=\"quota-label\">{}</span>\n    <span class=\"quota-bar\"><span class=\"quota-fill {}\"></span></span>\n    <span class=\"quota-pct\">{}</span>\n    <span class=\"quota-reset\">{}</span>\n  </div>\n",
        escape_html(label),
        class,
        escape_html(&percent),
        escape_html(&reset),
    )
}

/// 有快照但最近一次读取失败时的「更新于 …」文字。
fn stale_text(updated_at: i64, now: i64) -> String {
    let age = now.saturating_sub(updated_at);
    if age < 60 {
        "刚刚更新".to_string()
    } else if age < 3600 {
        format!("更新于 {} 分钟前", age / 60)
    } else {
        format!("更新于 {} 小时前", age / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn window(percent: u32, resets_in: i64) -> QuotaWindow {
        QuotaWindow {
            used_percent: Some(percent),
            resets_at: Some(NOW + resets_in),
        }
    }

    fn sample_snapshot() -> QuotaSnapshot {
        QuotaSnapshot {
            plan: Some("GOAT".to_string()),
            five_hour: Some(window(55, 4 * 3600 + 12 * 60)),
            weekly: Some(window(51, 7 * 24 * 3600)),
            monthly: Some(window(11, 30 * 24 * 3600)),
            credits_remaining: Some(62.4),
            monthly_credits_remaining: Some(50.0),
            credits_extra: Some(12.4),
            updated_at: Some(NOW),
            failure: None,
            ..QuotaSnapshot::default()
        }
    }

    #[test]
    fn renders_full_board() {
        let html = render_html(&sample_snapshot(), NOW);
        assert!(html.contains("quota-mark-cc"), "{html}");
        assert!(html.contains("quota-brand"), "{html}");
        assert!(html.contains("Command Code"), "{html}");
        assert!(html.contains("GOAT · 余 62.4"), "{html}");
        assert_eq!(html.matches("quota-row").count(), 3, "{html}");
        assert!(html.contains("q-55"), "{html}");
        assert!(html.contains("q-50"), "{html}");
        assert!(html.contains("q-10"), "{html}");
        assert!(html.contains("55%"), "{html}");
        assert!(html.contains("51%"), "{html}");
        assert!(html.contains("11%"), "{html}");
        assert!(!html.contains("--"), "{html}");
        assert!(html.contains("4小时12分后重置"), "{html}");
        assert!(html.contains("7天后重置"), "{html}");
        assert!(html.contains("30天后重置"), "{html}");
    }

    #[test]
    fn output_only_uses_div_span_and_class_attributes() {
        let html = render_html(&sample_snapshot(), NOW);
        for (i, _) in html.match_indices('<') {
            let rest = &html[i..];
            assert!(
                ["<div", "</div", "<span", "</span"]
                    .iter()
                    .any(|tag| rest.starts_with(tag)),
                "unexpected markup at byte {i}: {html}"
            );
        }
        assert!(!html.contains("style="), "{html}");
        assert!(!html.contains(" id="), "{html}");
        assert!(!html.contains(" data-"), "{html}");
    }

    #[test]
    fn none_percent_shows_placeholder_without_fake_zero() {
        let mut snapshot = sample_snapshot();
        snapshot.weekly = Some(QuotaWindow {
            used_percent: None,
            resets_at: Some(NOW + 3 * 3600),
        });
        let html = render_html(&snapshot, NOW);
        // 本周行降级为空条与 --，不写入 0% 这类假数据。
        assert!(html.contains("本周"), "{html}");
        assert!(html.contains("--"), "{html}");
        assert!(html.contains("暂无数据"), "{html}");
        assert!(!html.contains("0%"), "{html}");
        assert_eq!(html.matches("q-0").count(), 1, "{html}");
        // 另外两行照常显示。
        assert!(html.contains("55%"), "{html}");
        assert!(html.contains("11%"), "{html}");
    }

    #[test]
    fn zero_percent_shows_zero_not_placeholder() {
        let snapshot = QuotaSnapshot {
            updated_at: Some(NOW),
            five_hour: Some(window(0, 3600)),
            ..QuotaSnapshot::default()
        };
        let html = render_html(&snapshot, NOW);
        // 全新窗口的真实 0% 不能被误判成「暂无数据」。
        assert!(html.contains("0%"), "{html}");
        // 其余 default 行自然是 --，所以只检查 5 小时这一行。
        let five_hour_row = html
            .split("  <div class=\"quota-row\">\n")
            .find(|row| row.contains("5 小时"))
            .expect("five hour row");
        assert!(five_hour_row.contains("0%"), "{five_hour_row}");
        assert!(!five_hour_row.contains("--"), "{five_hour_row}");
    }

    #[test]
    fn missing_window_shows_placeholder() {
        let mut snapshot = sample_snapshot();
        snapshot.monthly = None;
        let html = render_html(&snapshot, NOW);
        assert_eq!(html.matches("quota-row").count(), 3, "{html}");
        assert!(html.contains("本月"), "{html}");
        assert!(html.contains("--"), "{html}");
        assert!(html.contains("暂无数据"), "{html}");
        assert!(html.contains("55%"), "{html}");
    }

    #[test]
    fn month_row_uses_precomputed_percent_without_recomputing() {
        let mut snapshot = sample_snapshot();
        // api.rs 已按月池算出 35%；render 层直接展示，不再从顶部 credit 换算。
        snapshot.monthly = Some(QuotaWindow {
            used_percent: Some(35),
            resets_at: Some(NOW + 2 * 24 * 3600),
        });
        let html = render_html(&snapshot, NOW);
        assert!(html.contains("35%"), "{html}");
        assert!(html.contains("q-35"), "{html}");
        assert!(!html.contains("11%"), "{html}");
        assert!(html.contains("GOAT · 余 62.4"), "{html}");
    }

    #[test]
    fn month_row_with_missing_reset_time_falls_back_to_unknown() {
        let mut snapshot = sample_snapshot();
        snapshot.monthly = Some(QuotaWindow {
            used_percent: Some(20),
            resets_at: None,
        });
        let html = render_html(&snapshot, NOW);
        assert!(html.contains("20%"), "{html}");
        assert!(html.contains("重置时间未知"), "{html}");
    }

    #[test]
    fn login_required_replaces_board_with_notice() {
        let mut snapshot = sample_snapshot();
        snapshot.failure = Some(LOGIN_REQUIRED.to_string());
        let html = render_html(&snapshot, NOW);
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("Command Code 未登录"), "{html}");
        assert!(!html.contains("quota-row"), "{html}");
        assert!(!html.contains("55%"), "{html}");
        assert!(!html.contains("更新于"), "{html}");
    }

    #[test]
    fn not_found_replaces_board_with_notice() {
        let mut snapshot = sample_snapshot();
        snapshot.failure = Some(FAILURE_NOT_FOUND.to_string());
        let html = render_html(&snapshot, NOW);
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("额度数据未找到"), "{html}");
        assert!(!html.contains("quota-row"), "{html}");
        assert!(!html.contains("55%"), "{html}");
    }

    #[test]
    fn missing_snapshot_shows_loading_notice() {
        let html = render_html(&QuotaSnapshot::default(), NOW);
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("正在读取额度…"), "{html}");
        assert!(!html.contains("quota-row"), "{html}");
    }

    #[test]
    fn stale_snapshot_keeps_rows_and_shows_age() {
        let mut snapshot = sample_snapshot();
        snapshot.updated_at = Some(NOW - 180);
        snapshot.failure = Some("额度读取失败".to_string());
        let html = render_html(&snapshot, NOW);
        assert_eq!(html.matches("quota-row").count(), 3, "{html}");
        assert!(html.contains("quota-stale"), "{html}");
        assert!(html.contains("更新于 3 分钟前"), "{html}");
        assert!(html.contains("55%"), "{html}");
    }

    #[test]
    fn escapes_plan_markup() {
        let mut snapshot = sample_snapshot();
        snapshot.plan = Some("<script>alert(1)</script>".to_string());
        let html = render_html(&snapshot, NOW);
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn omits_credit_segment_when_missing() {
        let mut snapshot = sample_snapshot();
        snapshot.credits_remaining = None;
        let html = render_html(&snapshot, NOW);
        assert!(html.contains("GOAT"), "{html}");
        assert!(!html.contains("余 "), "{html}");
    }

    #[test]
    fn render_notice_escapes_message() {
        let html = render_notice("失败 <详情>");
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("失败 &lt;详情&gt;"), "{html}");
    }

    #[test]
    fn stylesheet_defines_command_code_icon() {
        let css = include_str!("../../../web/ink.html");
        assert!(css.contains(".quota-mark-cc"), "missing .quota-mark-cc");
        assert!(
            css.contains("body.invert .quota-mark-cc"),
            "missing body.invert .quota-mark-cc"
        );
    }
}
