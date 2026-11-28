//! 额度快照的数据结构。

use serde::{Deserialize, Serialize};

/// 一个额度窗口（5 小时或一周）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// 已用百分比，0..=100。
    pub used_percent: u32,
    /// 重置时间（Unix 秒）。
    pub resets_at: Option<i64>,
    /// 窗口长度（分钟）。
    pub window_duration_mins: Option<i64>,
}

/// 一次取数的完整快照；字段缺失用 `None`，失败原因写入 `failure`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaSnapshot {
    pub plan: Option<String>,
    pub five_hour: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
    pub reset_credits: Option<u64>,
    pub updated_at: Option<i64>,
    pub failure: Option<String>,
    /// 浅眠标记：true 时页面附加「更新于 X 分钟前」，提示监控已暂停。
    /// 运行时状态，不入盘。
    pub idle: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_types_default_to_empty() {
        let window = QuotaWindow::default();
        assert_eq!(window.used_percent, 0);
        assert_eq!(window.resets_at, None);
        assert_eq!(window.window_duration_mins, None);

        let snapshot = QuotaSnapshot::default();
        assert_eq!(snapshot.plan, None);
        assert_eq!(snapshot.five_hour, None);
        assert_eq!(snapshot.weekly, None);
        assert_eq!(snapshot.reset_credits, None);
        assert_eq!(snapshot.updated_at, None);
        assert_eq!(snapshot.failure, None);
    }

    #[test]
    fn quota_types_support_clone_and_equality() {
        let window = QuotaWindow {
            used_percent: 35,
            resets_at: Some(1_700_000_000),
            window_duration_mins: Some(300),
        };
        let snapshot = QuotaSnapshot {
            plan: Some("Plus".to_string()),
            five_hour: Some(window.clone()),
            ..QuotaSnapshot::default()
        };

        let copy = snapshot.clone();
        assert_eq!(snapshot, copy);

        let mut changed = copy;
        changed.plan = Some("Pro".to_string());
        assert_ne!(snapshot, changed);
    }
}
