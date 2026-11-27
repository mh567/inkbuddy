//! 额度快照的数据结构。
//!
//! 反重力的额度按「模型分组 × 时间窗口」组织：Gemini 一组，Claude 与 GPT 一组，
//! 每组各有 5 小时与周两个窗口，两个窗口的重置时间互不相同。

use serde::{Deserialize, Serialize};

/// 一个额度窗口。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// 已用百分比，0..=100。
    pub used_percent: u32,
    /// 重置时间（Unix 秒）；额度满且没用过时接口不返回这个字段。
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// 一组模型的 5 小时与周窗口。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Group {
    pub five_hour: Option<Bucket>,
    pub weekly: Option<Bucket>,
}

impl Group {
    /// 是否已经读到过任意一个窗口。
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.weekly.is_none()
    }
}

/// 一次取数的完整快照；字段缺失用 `None`，失败原因写入 `failure`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// 套餐名，接口原样返回的英文名（`Pro`、`Ultra` 等）。
    pub plan: Option<String>,
    /// Gemini 模型组。
    pub gemini: Group,
    /// Claude 与 GPT 模型组。
    pub third_party: Group,
    /// 最近一次成功取数的时刻（Unix 秒）。
    pub updated_at: Option<i64>,
    /// 最近一次取数的失败原因；有旧数据时保留旧数据。
    pub failure: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_is_empty_until_a_window_arrives() {
        assert!(Group::default().is_empty());

        let with_window = Group {
            five_hour: Some(Bucket {
                used_percent: 20,
                resets_at: None,
            }),
            weekly: None,
        };
        assert!(!with_window.is_empty());
    }

    #[test]
    fn snapshot_defaults_have_nothing_to_show() {
        let snapshot = Snapshot::default();
        assert!(snapshot.gemini.is_empty());
        assert!(snapshot.third_party.is_empty());
        assert_eq!(snapshot.updated_at, None);
        assert_eq!(snapshot.failure, None);
    }
}
