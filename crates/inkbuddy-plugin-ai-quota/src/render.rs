//! 把额度快照渲染成宿主可用的 HTML 块。
//!
//! 宿主内容模型只接受 `html` 块，白名单标签内的属性只有 `class`，所以进度条宽度
//! 全部走 `q-0`..`q-100` 预设 class，图标写在 `web/ink.html` 的样式表里。

use crate::formatting::{escape_html, plan_label, reset_countdown, width_class};
use crate::model::{QuotaSnapshot, QuotaWindow};

/// 渲染完整的额度看板；快照未就绪或失败时降级为提示块。
pub fn render_html(snapshot: &QuotaSnapshot, now: i64) -> String {
    let Some(updated_at) = snapshot.updated_at else {
        return match snapshot.failure.as_deref() {
            Some(failure) => render_notice(failure),
            None => render_notice("正在读取额度…"),
        };
    };

    let plan = plan_label(snapshot.plan.as_deref());
    let meta = match snapshot.reset_credits {
        Some(credits) => format!("{} · 券 {}", plan, credits),
        None => plan,
    };

    let mut out = String::new();
    out.push_str("<div class=\"quota\">\n");
    out.push_str("  <div class=\"quota-head\">\n");
    out.push_str("    <span class=\"quota-mark\"></span>\n");
    out.push_str("    <span class=\"quota-brand\">ChatGPT</span>\n");
    out.push_str(&format!(
        "    <span class=\"quota-meta\">{}</span>\n",
        escape_html(&meta)
    ));
    out.push_str("  </div>\n");
    out.push_str(&render_row("5 小时", snapshot.five_hour.as_ref(), now));
    out.push_str(&render_row("本周", snapshot.weekly.as_ref(), now));
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
    let (class, percent, reset) = match window {
        Some(window) => (
            width_class(window.used_percent).to_string(),
            format!("{}%", window.used_percent),
            reset_countdown(window.resets_at, now),
        ),
        None => ("q-0".to_string(), "--".to_string(), "暂无数据".to_string()),
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
    use crate::model::QuotaWindow;

    fn sample_snapshot() -> QuotaSnapshot {
        let now = 1_700_000_000_i64;
        QuotaSnapshot {
            plan: Some("plus".to_string()),
            five_hour: Some(QuotaWindow {
                used_percent: 55,
                resets_at: Some(now + 4 * 3600 + 12 * 60),
                window_duration_mins: Some(300),
            }),
            weekly: Some(QuotaWindow {
                used_percent: 51,
                resets_at: Some(now + 7 * 24 * 3600),
                window_duration_mins: Some(10080),
            }),
            reset_credits: Some(2),
            updated_at: Some(now),
            failure: None,
        }
    }

    #[test]
    fn renders_full_board() {
        let now = 1_700_000_000_i64;
        let html = render_html(&sample_snapshot(), now);
        assert!(html.contains("quota-mark"), "{html}");
        assert!(html.contains("ChatGPT"), "{html}");
        assert!(html.contains("Plus · 券 2"), "{html}");
        assert!(html.contains("q-55"), "{html}");
        assert!(html.contains("q-50"), "{html}");
        assert!(html.contains("55%"), "{html}");
        assert!(html.contains("51%"), "{html}");
        assert_eq!(html.matches("quota-row").count(), 2, "{html}");
        assert!(html.contains("4小时12分后重置"), "{html}");
        assert!(html.contains("168小时0分后重置"), "{html}");
    }

    #[test]
    fn renders_placeholder_for_missing_windows() {
        let now = 1_700_000_000_i64;
        let snapshot = QuotaSnapshot {
            plan: Some("plus".to_string()),
            updated_at: Some(now),
            ..QuotaSnapshot::default()
        };
        let html = render_html(&snapshot, now);
        assert!(html.contains("--"), "{html}");
        assert!(html.contains("q-0"), "{html}");
        assert_eq!(html.matches("暂无数据").count(), 2, "{html}");
    }

    #[test]
    fn omits_credits_when_absent() {
        let now = 1_700_000_000_i64;
        let snapshot = QuotaSnapshot {
            plan: Some("plus".to_string()),
            updated_at: Some(now),
            ..QuotaSnapshot::default()
        };
        let html = render_html(&snapshot, now);
        assert!(html.contains(">Plus<"), "{html}");
        assert!(!html.contains("券"), "{html}");
    }

    #[test]
    fn escapes_plan_markup() {
        let now = 1_700_000_000_i64;
        let snapshot = QuotaSnapshot {
            plan: Some("<script>alert(1)</script>".to_string()),
            updated_at: Some(now),
            ..QuotaSnapshot::default()
        };
        let html = render_html(&snapshot, now);
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn failure_without_snapshot_returns_escaped_notice() {
        let snapshot = QuotaSnapshot {
            failure: Some("未找到 <Codex> CLI".to_string()),
            ..QuotaSnapshot::default()
        };
        let html = render_html(&snapshot, 1_700_000_000);
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("未找到 &lt;Codex&gt; CLI"), "{html}");
        assert!(!html.contains("quota-row"), "{html}");
    }

    #[test]
    fn failure_with_stale_snapshot_keeps_rows_and_shows_age() {
        let now = 1_700_000_000_i64;
        let mut snapshot = sample_snapshot();
        snapshot.updated_at = Some(now - 180);
        snapshot.failure = Some("读取失败".to_string());
        let html = render_html(&snapshot, now);
        assert!(html.contains("quota-row"), "{html}");
        assert!(html.contains("更新于 3 分钟前"), "{html}");
        assert!(!html.contains("更新于更新于"), "{html}");
    }

    #[test]
    fn missing_snapshot_without_failure_shows_loading() {
        let html = render_html(&QuotaSnapshot::default(), 1_700_000_000);
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("正在读取额度…"), "{html}");
    }

    #[test]
    fn stylesheet_defines_quota_classes() {
        let css = include_str!("../../../web/ink.html");
        assert!(css.contains(".quota-mark"), "missing .quota-mark");
        assert!(css.contains(".quota-bar"), "missing .quota-bar");
        for p in (0..=100).step_by(5) {
            let class = format!(".q-{p}");
            assert!(css.contains(&class), "missing {class}");
        }
    }
}
