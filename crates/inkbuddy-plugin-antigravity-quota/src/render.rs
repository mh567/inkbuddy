//! 把额度快照渲染成宿主可用的 HTML 块。
//!
//! 宿主内容模型只接受 `html` 块，白名单标签内的属性只有 `class`，所以进度条宽度
//! 全部走 `q-0`..`q-100` 预设 class，样式与图标写在 `web/ink.html`。

use crate::formatting::{escape_html, plan_text, reset_countdown, width_class};
use crate::model::{Bucket, Group, Snapshot};

/// 接口没给组名时的短标题。
const GEMINI_TITLE: &str = "Gemini";
const THIRD_PARTY_TITLE: &str = "Claude、GPT";

/// 渲染完整看板；快照未就绪或失败时降级为提示块。
pub fn render_html(snapshot: &Snapshot, now: i64) -> String {
    let Some(updated_at) = snapshot.updated_at else {
        return match snapshot.failure.as_deref() {
            Some(failure) => render_notice(failure),
            None => render_notice("正在读取额度…"),
        };
    };

    let plan = plan_text(snapshot.plan.as_deref());
    // 读失败时页面沿用上次的数据，这时窗口如果已经越过重置时间，那个百分比
    // 就肯定不准了，标成「已重置」而不是继续显示旧数字。
    let stale = snapshot.failure.is_some();

    let mut out = String::new();
    // 声明本卡按基准字号占用的高度（vw 单位），宿主据此在矮区域里缩小字号。
    out.push_str("<div class=\"quota\" data-ink-height=\"41\">\n");
    out.push_str("  <div class=\"quota-head\">\n");
    out.push_str("    <span class=\"quota-mark quota-mark-ag\"></span>\n");
    out.push_str("    <span class=\"quota-brand\">Antigravity</span>\n");
    // 套餐名只有语言服务器能给；命令行版读不到时这一格留空。
    if let Some(plan) = plan {
        out.push_str(&format!(
            "    <span class=\"quota-meta\">{}</span>\n",
            escape_html(&plan)
        ));
    }
    out.push_str("  </div>\n");
    out.push_str(&render_group(&snapshot.gemini, GEMINI_TITLE, now, stale));
    out.push_str(&render_group(
        &snapshot.third_party,
        THIRD_PARTY_TITLE,
        now,
        stale,
    ));
    out.push_str("</div>");

    if let Some(failure) = snapshot.failure.as_deref() {
        out.push_str(&format!(
            "\n<div class=\"quota-stale\">{} · {}</div>",
            escape_html(failure),
            escape_html(&stale_text(updated_at, now))
        ));
    }

    out
}

/// 单行提示，用于插件失败或快照未就绪。
pub fn render_notice(message: &str) -> String {
    format!(
        "<div class=\"quota quota-notice\" data-ink-height=\"6\"><span class=\"quota-notice-text\">{}</span></div>",
        escape_html(message)
    )
}

/// 一组模型：分组标题加 5 小时、周两行。标题优先用接口给的组名，读不到时用内置短标题。
fn render_group(group: &Group, fallback_title: &str, now: i64, stale: bool) -> String {
    let title = group.name.as_deref().unwrap_or(fallback_title);
    let mut out = format!(
        "  <div class=\"quota-group\">{}</div>\n",
        escape_html(title)
    );
    out.push_str(&render_row("5 小时", group.five_hour.as_ref(), now, stale));
    out.push_str(&render_row("本周", group.weekly.as_ref(), now, stale));
    out
}

fn render_row(label: &str, bucket: Option<&Bucket>, now: i64, stale: bool) -> String {
    let (class, percent, reset) = match bucket {
        Some(bucket) if stale && has_reset_since(bucket, now) => (
            "q-0".to_string(),
            "--".to_string(),
            "已重置".to_string(),
        ),
        Some(bucket) => (
            width_class(bucket.used_percent).to_string(),
            format!("{}%", bucket.used_percent),
            reset_text(bucket, now),
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

/// 重置列文字。额度满且从没用过时接口不给重置时间，这时写「未使用」。
fn reset_text(bucket: &Bucket, now: i64) -> String {
    if bucket.used_percent == 0 && bucket.resets_at.is_none() {
        return "未使用".to_string();
    }
    reset_countdown(bucket.resets_at, now)
}

/// 这个窗口的重置时间是否已经过去。
fn has_reset_since(bucket: &Bucket, now: i64) -> bool {
    bucket.resets_at.map(|reset| reset <= now).unwrap_or(false)
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
    use crate::model::{Bucket, Group};

    fn sample_snapshot(now: i64) -> Snapshot {
        Snapshot {
            plan: Some("Pro".to_string()),
            source: crate::model::Source::Local,
            gemini: Group {
                five_hour: Some(Bucket {
                    used_percent: 24,
                    resets_at: Some(now + 2 * 3600 + 39 * 60),
                }),
                weekly: Some(Bucket {
                    used_percent: 11,
                    resets_at: Some(now + 5 * 24 * 3600 + 11 * 3600),
                }),
                ..Group::default()
            },
            third_party: Group {
                five_hour: Some(Bucket {
                    used_percent: 0,
                    resets_at: None,
                }),
                weekly: Some(Bucket {
                    used_percent: 0,
                    resets_at: None,
                }),
                ..Group::default()
            },
            updated_at: Some(now),
            failure: None,
        }
    }

    #[test]
    fn renders_two_groups_and_four_rows() {
        let now = 1_700_000_000_i64;
        let html = render_html(&sample_snapshot(now), now);
        assert!(html.contains("Antigravity"), "{html}");
        assert!(html.contains(">Pro<"), "{html}");
        assert!(html.contains(">Gemini<"), "{html}");
        assert!(html.contains(">Claude、GPT<"), "{html}");
        assert_eq!(html.matches("quota-row").count(), 4, "{html}");
        assert!(html.contains("q-25"), "{html}");
        assert!(html.contains("24%"), "{html}");
        assert!(html.contains("11%"), "{html}");
        assert!(html.contains("2小时39分后重置"), "{html}");
        // 满额且没有重置时间
        assert_eq!(html.matches("未使用").count(), 2, "{html}");
    }

    #[test]
    fn renders_group_titles_from_api_names() {
        let now = 1_700_000_000_i64;
        let mut snapshot = sample_snapshot(now);
        snapshot.gemini.name = Some("Gemini Models".to_string());
        snapshot.third_party.name = Some("Claude and GPT models".to_string());
        let html = render_html(&snapshot, now);
        assert!(html.contains(">Gemini Models<"), "{html}");
        assert!(html.contains(">Claude and GPT models<"), "{html}");
        // 有了接口组名就不再出现内置短标题
        assert!(!html.contains(">Gemini<"), "{html}");
        assert!(!html.contains(">Claude、GPT<"), "{html}");
    }

    #[test]
    fn header_carries_the_plugin_icon() {
        let now = 1_700_000_000_i64;
        let html = render_html(&sample_snapshot(now), now);
        assert!(html.contains("quota-mark quota-mark-ag"), "{html}");
    }

    #[test]
    fn stylesheet_defines_antigravity_icon() {
        let css = include_str!("../../../web/ink.html");
        assert!(css.contains(".quota-mark-ag"), "missing .quota-mark-ag");
        assert!(
            css.contains("body.invert .quota-mark-ag"),
            "missing body.invert .quota-mark-ag"
        );
    }

    #[test]
    fn renders_placeholder_for_missing_windows() {
        let now = 1_700_000_000_i64;
        let snapshot = Snapshot {
            plan: Some("Pro".to_string()),
            updated_at: Some(now),
            ..Snapshot::default()
        };
        let html = render_html(&snapshot, now);
        assert_eq!(html.matches("暂无数据").count(), 4, "{html}");
        assert!(html.contains("--"), "{html}");
    }

    #[test]
    fn omits_plan_meta_when_name_is_unknown() {
        let now = 1_700_000_000_i64;
        let mut snapshot = sample_snapshot(now);
        snapshot.plan = None;
        let html = render_html(&snapshot, now);
        assert!(!html.contains("quota-meta"), "{html}");
        assert!(!html.contains("未知套餐"), "{html}");
        // 其余行照常
        assert_eq!(html.matches("quota-row").count(), 4, "{html}");
    }

    #[test]
    fn escapes_plan_markup() {
        let now = 1_700_000_000_i64;
        let mut snapshot = sample_snapshot(now);
        snapshot.plan = Some("<script>alert(1)</script>".to_string());
        let html = render_html(&snapshot, now);
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn failure_without_snapshot_returns_notice() {
        let snapshot = Snapshot {
            failure: Some("反重力未运行".to_string()),
            ..Snapshot::default()
        };
        let html = render_html(&snapshot, 1_700_000_000);
        assert!(html.contains("quota-notice"), "{html}");
        assert!(html.contains("反重力未运行"), "{html}");
        assert!(!html.contains("quota-row"), "{html}");
    }

    #[test]
    fn failure_with_stale_snapshot_keeps_rows_and_shows_reason() {
        let now = 1_700_000_000_i64;
        let mut snapshot = sample_snapshot(now);
        snapshot.updated_at = Some(now - 180);
        snapshot.failure = Some("反重力未运行".to_string());
        let html = render_html(&snapshot, now);
        assert_eq!(html.matches("quota-row").count(), 4, "{html}");
        assert!(html.contains("反重力未运行 · 更新于 3 分钟前"), "{html}");
    }

    #[test]
    fn empty_snapshot_shows_loading() {
        let html = render_html(&Snapshot::default(), 1_700_000_000);
        assert!(html.contains("正在读取额度…"), "{html}");
    }

    #[test]
    fn stale_window_past_its_reset_shows_reset_instead_of_old_numbers() {
        let now = 1_700_000_000_i64;
        // 8 小时前读到：5 小时窗口（重置在 2 小时 39 分后）早已越过重置，
        // 周窗口（5 天后）还没到。
        let mut snapshot = sample_snapshot(now - 8 * 3600);
        snapshot.failure = Some("反重力未运行".to_string());
        let html = render_html(&snapshot, now);

        assert_eq!(html.matches("已重置").count(), 1, "{html}");
        assert!(!html.contains("24%"), "{html}");
        // 周窗口还在有效期内，旧百分比照旧显示
        assert!(html.contains("11%"), "{html}");
    }

    #[test]
    fn fresh_snapshot_keeps_numbers_even_if_reset_looks_past() {
        let now = 1_700_000_000_i64;
        let snapshot = sample_snapshot(now - 8 * 3600);
        let html = render_html(&snapshot, now);
        assert!(!html.contains("已重置"), "{html}");
        assert!(html.contains("24%"), "{html}");
    }
}
