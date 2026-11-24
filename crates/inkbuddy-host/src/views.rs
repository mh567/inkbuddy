//! Named views. A view is a layout: a list of regions, each mapping one
//! plugin to a rectangle of the screen. Coordinates are percentages (0-100,
//! f32) of the screen, so the same view adapts to any device resolution and
//! any region can sit anywhere at any size. State lives in `views.json`
//! under `INKBUDDY_HOME`.

use crate::config::home_dir;
use crate::plugin::PluginSet;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::warn;

pub const MAX_VIEWS: usize = 64;
pub const MAX_REGIONS_PER_VIEW: usize = 64;
/// 最小区域尺寸（百分比）。低于此值基本不可见，直接视为编辑错误。
pub const MIN_REGION_SIZE: f32 = 5.0;

/// 一个区域：把某个插件放到屏幕的哪个位置、占多大。
/// x/y 是左上角，w/h 是宽高，全部为屏幕百分比（0-100）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Region {
    pub plugin: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Region {
    /// 竖排等分：第 index 个区域占满宽度、均分高度。用于旧数据迁移和
    /// 流式布局的 regions 输出。
    pub fn stacked(plugin: &str, index: usize, total: usize) -> Region {
        let total = total.max(1);
        let h = 100.0 / total as f32;
        Region {
            plugin: plugin.to_string(),
            x: 0.0,
            y: h * index as f32,
            w: 100.0,
            h,
        }
    }

    fn clamp_coord(v: f32) -> f32 {
        if v.is_nan() || v.is_infinite() {
            return 0.0;
        }
        v.clamp(0.0, 100.0)
    }

    /// 校正到合法范围：坐标 0-100，尺寸 MIN_REGION_SIZE-100，
    /// 并保证区域不出屏（x + w <= 100）。
    pub fn sanitized(&self) -> Region {
        let w = self.w.clamp(MIN_REGION_SIZE, 100.0);
        let h = self.h.clamp(MIN_REGION_SIZE, 100.0);
        let x = Self::clamp_coord(self.x).min(100.0 - w);
        let y = Self::clamp_coord(self.y).min(100.0 - h);
        Region {
            plugin: self.plugin.trim().to_string(),
            x: (x * 100.0).round() / 100.0,
            y: (y * 100.0).round() / 100.0,
            w: (w * 100.0).round() / 100.0,
            h: (h * 100.0).round() / 100.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct View {
    pub id: String,
    pub name: String,
    pub regions: Vec<Region>,
}

/// 反序列化入口：兼容两种历史格式。
/// - regions: 当前格式。
/// - plugins: 旧格式（纯插件列表），迁移为竖排等分区域。
#[derive(Deserialize)]
struct RawView {
    id: String,
    name: String,
    #[serde(default)]
    regions: Vec<Region>,
    #[serde(default)]
    plugins: Vec<String>,
}

impl From<RawView> for View {
    fn from(raw: RawView) -> Self {
        if !raw.regions.is_empty() {
            return View {
                id: raw.id,
                name: raw.name,
                regions: raw.regions,
            };
        }
        let total = raw.plugins.len();
        View {
            id: raw.id,
            name: raw.name,
            regions: raw
                .plugins
                .iter()
                .enumerate()
                .map(|(i, p)| Region::stacked(p, i, total))
                .collect(),
        }
    }
}

impl<'de> Deserialize<'de> for View {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(RawView::deserialize(d)?.into())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    /// 最近应用的视图 id。显示始终跟随当前视图的 regions。
    pub current: Option<String>,
    pub views: Vec<View>,
}

impl ViewState {
    pub fn view(&self, id: &str) -> Option<&View> {
        self.views.iter().find(|v| v.id == id)
    }

    /// 当前视图的布局；无当前视图时返回 None（回退流式布局）。
    pub fn current_regions(&self) -> Option<Vec<Region>> {
        let id = self.current.as_ref()?;
        Some(self.view(id)?.regions.clone())
    }
}

fn path() -> Option<PathBuf> {
    home_dir().ok().map(|p| p.join("views.json"))
}

/// 写临时文件后 rename，避免进程中途被杀留下半截 JSON。
/// load() 对损坏文件虽有容错，但那时 load_or_seed 不会再播种，数据就丢了。
fn write_atomic(path: &std::path::Path, text: &str) {
    let tmp = path.with_extension("tmp");
    let result = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        warn!(path = %path.display(), error = %e, "failed to write views.json");
    }
}

pub fn load() -> ViewState {
    let Some(p) = path() else {
        return ViewState::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            warn!(path = %p.display(), error = %e, "invalid views.json, starting empty");
            ViewState::default()
        }),
        Err(_) => ViewState::default(), // missing file: seeded by load_or_seed
    }
}

pub fn save(state: &ViewState) {
    let Some(p) = path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        write_atomic(&p, &text);
    }
}

/// Load the stored state; when no file exists yet, seed default views from the
/// discovered plugins: one view per plugin plus an "all" view with every
/// default_on plugin, each as a stacked full-width layout. The seeded state is
/// written back so a later edit does not re-seed over it.
pub fn load_or_seed(set: &PluginSet) -> ViewState {
    if let Some(p) = path() {
        if p.exists() {
            return load();
        }
    }

    let mut views = Vec::new();
    for (i, id) in set.order.iter().enumerate() {
        let Some(p) = set.get(id) else { continue };
        views.push(View {
            id: id.clone(),
            name: p.manifest.plugin.name.clone(),
            regions: vec![Region::stacked(id, 0, 1)],
        });
        let _ = i; // 单插件视图只占一格
    }
    let defaults = set.default_ids();
    if !defaults.is_empty()
        && !views
            .iter()
            .any(|v| v.regions.len() == defaults.len() && v.regions.iter().zip(&defaults).all(|(r, d)| &r.plugin == d))
    {
        views.push(View {
            id: "all".to_string(),
            name: "全部".to_string(),
            regions: defaults
                .iter()
                .enumerate()
                .map(|(i, id)| Region::stacked(id, i, defaults.len()))
                .collect(),
        });
    }

    let state = ViewState {
        current: None,
        views,
    };
    save(&state);
    state
}

/// Normalize a views list pushed by the control app: drop empty entries,
/// deduplicate ids, clamp region geometry, cap sizes. Returns the cleaned list.
pub fn sanitize(mut views: Vec<View>) -> Vec<View> {
    let mut seen = std::collections::HashSet::new();
    views.retain(|v| !v.id.trim().is_empty());
    for v in views.iter_mut() {
        v.id = v.id.trim().to_string();
        if v.name.trim().is_empty() {
            v.name = v.id.clone();
        } else {
            v.name = v.name.trim().to_string();
        }
        let mut plugin_seen = std::collections::HashSet::new();
        v.regions = v
            .regions
            .iter()
            .filter(|r| !r.plugin.trim().is_empty() && plugin_seen.insert(r.plugin.trim().to_string()))
            .map(|r| r.sanitized())
            .take(MAX_REGIONS_PER_VIEW)
            .collect();
    }
    views.retain(|v| seen.insert(v.id.clone()));
    views.truncate(MAX_VIEWS);
    views
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(plugin: &str, x: f32, y: f32, w: f32, h: f32) -> Region {
        Region {
            plugin: plugin.into(),
            x,
            y,
            w,
            h,
        }
    }

    #[test]
    fn sanitize_dedupes_and_caps() {
        let views = vec![
            View {
                id: "a".into(),
                name: "  ".into(),
                regions: vec![region("lyrics", 0.0, 0.0, 100.0, 60.0), region(" lyrics ", 0.0, 60.0, 100.0, 40.0)],
            },
            View {
                id: " a ".into(),
                name: "重复".into(),
                regions: vec![],
            },
            View {
                id: "".into(),
                name: "被丢弃".into(),
                regions: vec![region("x", 0.0, 0.0, 100.0, 100.0)],
            },
        ];
        let out = sanitize(views);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
        assert_eq!(out[0].name, "a"); // 空名回退为 id
        assert_eq!(out[0].regions.len(), 1);
        assert_eq!(out[0].regions[0].plugin, "lyrics");
    }

    #[test]
    fn region_geometry_is_clamped() {
        let r = region("clock", -10.0, 200.0, 250.0, 1.0).sanitized();
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 95.0); // y=200 出屏 → 拉回 100-h，贴底不出屏
        assert_eq!(r.w, 100.0);
        assert_eq!(r.h, MIN_REGION_SIZE);
        // x 被压回不出屏
        let r = region("clock", 99.0, 0.0, 50.0, 50.0).sanitized();
        assert_eq!(r.x, 50.0);
    }

    #[test]
    fn legacy_plugins_field_migrates_to_stacked_regions() {
        let text = r#"{"id":"old","name":"旧视图","plugins":["lyrics","clock"]}"#;
        let view: View = serde_json::from_str(text).unwrap();
        assert_eq!(view.regions.len(), 2);
        assert_eq!(view.regions[0].plugin, "lyrics");
        assert_eq!(view.regions[0].y, 0.0);
        assert_eq!(view.regions[1].plugin, "clock");
        assert_eq!(view.regions[1].y, 50.0);
        assert_eq!(view.regions[1].h, 50.0);
    }

    #[test]
    fn current_regions_follows_pointer() {
        let vs = ViewState {
            current: Some("clock".into()),
            views: vec![View {
                id: "clock".into(),
                name: "时钟".into(),
                regions: vec![region("clock", 0.0, 0.0, 100.0, 100.0)],
            }],
        };
        assert!(vs.current_regions().is_some());
        assert!(ViewState::default().current_regions().is_none());
    }

    #[test]
    fn view_lookup_matches_by_id() {
        let vs = ViewState {
            current: Some("clock".into()),
            views: vec![View {
                id: "clock".into(),
                name: "时钟".into(),
                regions: vec![region("clock", 0.0, 0.0, 100.0, 100.0)],
            }],
        };
        assert!(vs.view("clock").is_some());
        assert!(vs.view("ghost").is_none());
    }
}
