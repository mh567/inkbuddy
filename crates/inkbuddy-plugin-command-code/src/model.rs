//! 额度快照的数据结构与内置套餐 credit 池。

/// 一个额度窗口（5 小时、本周或本月）；百分比与重置时间共用结构。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaWindow {
    /// 已用百分比，0..=100；无法可信换算时为 `None`，由展示层降级显示，不填假数字。
    pub used_percent: Option<u32>,
    /// 重置时间（Unix 秒）。
    pub resets_at: Option<i64>,
}

/// 本计费周期的用量统计；接口字段缺失时用 `None`，只存快照不展示。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSummary {
    /// 请求数。
    pub total_count: Option<u64>,
    /// 花费，单位 credit。
    pub total_cost: Option<f64>,
    /// token 数。
    pub total_tokens: Option<u64>,
}

/// 一次取数的完整快照；字段缺失用 `None`，失败原因写入 `failure`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaSnapshot {
    /// 套餐档位，保留接口原始值。
    pub plan: Option<String>,
    pub five_hour: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
    pub monthly: Option<QuotaWindow>,
    /// 剩余 credit 总额：monthlyCredits + purchasedCredits + freeCredits，供顶部展示。
    pub credits_remaining: Option<f64>,
    /// 套餐 credit 剩余：仅取接口的 credits.monthlyCredits 单项，供月行百分比使用；额外购买与赠送 credit 归 credits_extra。
    pub monthly_credits_remaining: Option<f64>,
    /// 额外 credit：purchasedCredits + freeCredits。
    pub credits_extra: Option<f64>,
    /// 本期请求数、花费与 token 数。
    pub usage: Option<UsageSummary>,
    pub updated_at: Option<i64>,
    pub failure: Option<String>,
}

/// 归一化 planId：拆开驼峰、去掉版本尾巴、统一分隔符并转小写。
/// 结果只用于查内置池，页面显示仍用接口原始值。
pub fn normalize_plan(raw: &str) -> String {
    let trimmed = raw.trim();

    // 在大小写交界处插空格，让 TeamsPro 与 Teams Pro 归一到同一个查表键。
    let mut spaced = String::with_capacity(trimmed.len());
    let mut prev_is_lower_or_digit = false;
    for ch in trimmed.chars() {
        if ch.is_uppercase() && prev_is_lower_or_digit {
            spaced.push(' ');
        }
        spaced.push(ch);
        prev_is_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
    }

    let lower = spaced.to_ascii_lowercase();
    let without_version = strip_version_tail(&lower);

    // 下划线、短横线和空白统一成单个空格。
    let mut out = String::with_capacity(without_version.len());
    let mut prev_was_sep = false;
    for ch in without_version.chars() {
        if ch == '_' || ch == '-' || ch.is_whitespace() {
            if !prev_was_sep {
                out.push(' ');
            }
            prev_was_sep = true;
        } else {
            out.push(ch);
            prev_was_sep = false;
        }
    }
    out.trim().to_string()
}

/// 去掉结尾的 `_vN` / `-vN` / ` vN` 版本尾巴；`v` 前必须有分隔符。
fn strip_version_tail(lower: &str) -> &str {
    let bytes = lower.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    if end == bytes.len() || end == 0 || bytes[end - 1] != b'v' {
        return lower;
    }
    let v_index = end - 1;
    if v_index == 0 {
        return lower;
    }
    let before = bytes[v_index - 1];
    if before == b'_' || before == b'-' || before == b' ' {
        // 切点位于 ASCII 分隔符之后，字节切片边界安全。
        return lower[..v_index - 1].trim_end();
    }
    lower
}

/// 内置套餐 credit 池，用于月度百分比换算。
const PLAN_CREDIT_POOLS: [(&str, f64); 6] = [
    ("Go", 10.0),
    ("GOAT", 70.0),
    ("Pro", 30.0),
    ("Max", 150.0),
    ("Ultra", 300.0),
    ("Teams Pro", 40.0),
];

/// 按归一化后的 planId 查内置 credit 池上限；未收录的套餐返回 `None`。
pub fn plan_credit_pool(plan: Option<&str>) -> Option<f64> {
    let normalized = normalize_plan(plan?);
    PLAN_CREDIT_POOLS
        .iter()
        .find(|(name, _)| normalize_plan(name) == normalized)
        .map(|(_, cap)| *cap)
}

/// 凭据缺失时的终态提示；整块页面换成这条提示。
pub const LOGIN_REQUIRED: &str = "Command Code 未登录，请先运行 cmd login";
/// 取数失败且没有旧快照时的提示。
pub const FAILURE_READ: &str = "额度读取失败";
/// 接口没有返回可用额度数据时的提示。
pub const FAILURE_NOT_FOUND: &str = "额度数据未找到";
/// apiKey 含非法字符、无法构造认证头时的终态提示。
pub const INVALID_KEY: &str = "Command Code 凭据格式不对，请检查 COMMAND_CODE_API_KEY";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_plan_strips_version_tails_and_unifies_separators() {
        assert_eq!(normalize_plan("go_v1"), "go");
        assert_eq!(normalize_plan("GOAT-v2"), "goat");
        assert_eq!(normalize_plan(" pro v3 "), "pro");
        assert_eq!(normalize_plan("teams_pro_v1"), "teams pro");
        // 驼峰写法也归一到同一个查表键。
        assert_eq!(normalize_plan("TeamsPro"), "teams pro");
        assert_eq!(normalize_plan("Max"), "max");
        assert_eq!(normalize_plan(""), "");
        assert_eq!(normalize_plan("   "), "");
    }

    #[test]
    fn normalize_plan_strips_version_tail_after_multibyte_chars() {
        // 版本尾巴前的字节是 ASCII 分隔符；多字节字符的 UTF-8 尾字节落在 ASCII
        // 范围但切点位于字符边界，字节切片不会 panic。
        assert_eq!(normalize_plan("中文_v1"), "中文");
        assert_eq!(normalize_plan("😀_v2"), "😀");
    }

    #[test]
    fn plan_credit_pool_looks_up_all_builtin_tiers() {
        let cases = [
            ("go", 10.0),
            ("GOAT", 70.0),
            ("pro_v1", 30.0),
            ("Max-v2", 150.0),
            ("ultra v3", 300.0),
            ("Teams Pro", 40.0),
            ("teams_pro_v1", 40.0),
            ("TeamsPro", 40.0),
        ];
        for (raw, expected) in cases {
            assert_eq!(plan_credit_pool(Some(raw)), Some(expected), "plan {raw}");
        }
        assert_eq!(plan_credit_pool(Some("mystery-plan")), None);
        assert_eq!(plan_credit_pool(None), None);
    }

    #[test]
    fn snapshot_types_default_to_empty() {
        let window = QuotaWindow::default();
        assert_eq!(window.used_percent, None);
        assert_eq!(window.resets_at, None);

        let usage = UsageSummary::default();
        assert_eq!(usage.total_count, None);
        assert_eq!(usage.total_cost, None);
        assert_eq!(usage.total_tokens, None);

        let snapshot = QuotaSnapshot::default();
        assert_eq!(snapshot.plan, None);
        assert_eq!(snapshot.five_hour, None);
        assert_eq!(snapshot.weekly, None);
        assert_eq!(snapshot.monthly, None);
        assert_eq!(snapshot.credits_remaining, None);
        assert_eq!(snapshot.monthly_credits_remaining, None);
        assert_eq!(snapshot.credits_extra, None);
        assert_eq!(snapshot.usage, None);
        assert_eq!(snapshot.updated_at, None);
        assert_eq!(snapshot.failure, None);
    }

    #[test]
    fn snapshot_types_support_clone_and_equality() {
        let window = QuotaWindow {
            used_percent: Some(35),
            resets_at: Some(1_700_000_000),
        };
        let snapshot = QuotaSnapshot {
            plan: Some("GOAT".to_string()),
            five_hour: Some(window.clone()),
            credits_remaining: Some(62.4),
            monthly_credits_remaining: Some(50.0),
            credits_extra: Some(12.4),
            usage: Some(UsageSummary {
                total_count: Some(18),
                total_cost: Some(7.6),
                total_tokens: Some(42_000),
            }),
            ..QuotaSnapshot::default()
        };

        let copy = snapshot.clone();
        assert_eq!(snapshot, copy);

        let mut changed = copy;
        changed.credits_remaining = Some(60.0);
        assert_ne!(snapshot, changed);
    }

    #[test]
    fn failure_constants_match_page_copy() {
        assert_eq!(LOGIN_REQUIRED, "Command Code 未登录，请先运行 cmd login");
        assert_eq!(FAILURE_READ, "额度读取失败");
        assert_eq!(FAILURE_NOT_FOUND, "额度数据未找到");
        assert_eq!(
            INVALID_KEY,
            "Command Code 凭据格式不对，请检查 COMMAND_CODE_API_KEY"
        );
    }
}
