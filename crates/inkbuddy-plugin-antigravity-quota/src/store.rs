//! 把最近一次成功读到的额度落盘，插件重启或反重力关着时接着用。
//!
//! 文件名固定，内容是一份带时间戳的快照；写的时候先写临时文件再改名，
//! 中途被杀不会留下半份 JSON。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Group, Snapshot};

/// 当前文件格式版本；读到别的版本直接当没有，别拿旧字段硬解。
const VERSION: u32 = 1;

/// 落盘内容。`failure` 不入盘：那是运行时状态，不是读到的额度。
#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    updated_at: i64,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    gemini: Group,
    #[serde(default)]
    third_party: Group,
}

/// 默认路径：`$INKBUDDY_HOME/cache/antigravity-quota/last.json`，
/// 与宿主和歌词插件的目录约定一致。
pub fn default_path() -> PathBuf {
    cache_file(&home_dir())
}

/// 空或只有空白的 `INKBUDDY_HOME` 按未设置处理，与宿主 `home_dir()` 的判断一致。
fn home_dir() -> PathBuf {
    match std::env::var("INKBUDDY_HOME") {
        Ok(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
        _ => home().join(".inkbuddy"),
    }
}

fn cache_file(base: &Path) -> PathBuf {
    base.join("cache")
        .join("antigravity-quota")
        .join("last.json")
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 读上次落盘的快照；文件不存在、格式对不上、内容坏了都返回 `None`。
pub fn load(path: &Path) -> Option<Snapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    let stored: Stored = serde_json::from_str(&text).ok()?;
    if stored.version != VERSION || stored.updated_at <= 0 {
        return None;
    }
    Some(Snapshot {
        plan: stored.plan,
        gemini: stored.gemini,
        third_party: stored.third_party,
        updated_at: Some(stored.updated_at),
        failure: None,
    })
}

/// 写入快照；失败只返回错误文本，由调用方记日志。
pub fn save(path: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let Some(updated_at) = snapshot.updated_at else {
        return Err("快照没有更新时间".to_string());
    };
    let stored = Stored {
        version: VERSION,
        updated_at,
        plan: snapshot.plan.clone(),
        gemini: snapshot.gemini.clone(),
        third_party: snapshot.third_party.clone(),
    };
    let text = serde_json::to_string_pretty(&stored).map_err(|err| format!("序列化失败: {err}"))?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("创建目录 {} 失败: {err}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|err| format!("写入 {} 失败: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|err| format!("改名到 {} 失败: {err}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Bucket;

    fn temp_dir() -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        );
        let dir = std::env::temp_dir().join(format!("inkbuddy-agq-{unique}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample(now: i64) -> Snapshot {
        Snapshot {
            plan: Some("Pro".to_string()),
            gemini: Group {
                five_hour: Some(Bucket {
                    used_percent: 24,
                    resets_at: Some(now + 3600),
                }),
                weekly: Some(Bucket {
                    used_percent: 11,
                    resets_at: Some(now + 86_400),
                }),
            },
            third_party: Group {
                five_hour: Some(Bucket {
                    used_percent: 0,
                    resets_at: None,
                }),
                weekly: None,
            },
            updated_at: Some(now),
            failure: None,
        }
    }

    #[test]
    fn round_trips_a_snapshot() {
        let dir = temp_dir();
        let path = dir.join("cache/antigravity-quota/last.json");
        let now = 1_795_804_760;
        save(&path, &sample(now)).expect("save");
        let loaded = load(&path).expect("load");

        assert_eq!(loaded.updated_at, Some(now));
        assert_eq!(loaded.plan.as_deref(), Some("Pro"));
        assert_eq!(loaded.gemini.five_hour.expect("5h").used_percent, 24);
        assert_eq!(loaded.third_party.weekly, None);
        assert_eq!(loaded.failure, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_or_broken_file_loads_as_none() {
        let dir = temp_dir();
        assert!(load(&dir.join("nope.json")).is_none());

        let path = dir.join("broken.json");
        std::fs::write(&path, "{not json").expect("write");
        assert!(load(&path).is_none());

        std::fs::write(&path, r#"{"version":99,"updated_at":1}"#).expect("write");
        assert!(load(&path).is_none());

        std::fs::write(&path, r#"{"version":1,"updated_at":0}"#).expect("write");
        assert!(load(&path).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_requires_an_update_time() {
        let dir = temp_dir();
        let mut snapshot = sample(1);
        snapshot.updated_at = None;
        assert!(save(&dir.join("x.json"), &snapshot).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_path_follows_inkbuddy_home() {
        // 环境变量在测试进程里是全局的，这里只断言不 panic 且路径以固定片段结尾。
        let path = default_path();
        assert!(path.ends_with("cache/antigravity-quota/last.json"), "{path:?}");
    }

    #[test]
    fn cache_file_sits_under_the_given_base() {
        let path = cache_file(Path::new("/tmp/inkbuddy-home"));
        assert_eq!(
            path,
            PathBuf::from("/tmp/inkbuddy-home/cache/antigravity-quota/last.json")
        );
    }
}
