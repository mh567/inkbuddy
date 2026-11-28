//! Codex 本地活动痕迹：会话文件与日志的最近修改时间。
//!
//! 用户在用 Codex 时这些文件持续被写入；没在用的时候值保持不变。
//! 浅眠中的插件只盯这个值，有变化才恢复监控，避免反复拉起 app-server。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 最新活动痕迹（Unix 秒）；没有任何痕迹时为 0。
pub fn stamp() -> u64 {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => stamp_at(&home),
        None => 0,
    }
}

/// 以指定的用户主目录计算活动痕迹，测试用。
fn stamp_at(home: &Path) -> u64 {
    let codex = home.join(".codex");
    let mut newest = stamp_of(&codex.join("log").join("codex-tui.log"));
    // 登录/登出会改写 auth.json：浅眠期间用户重新登录要能立刻唤醒。
    newest = newest.max(stamp_of(&codex.join("auth.json")));
    // 会话按 ~/.codex/sessions/YYYY/MM/DD/*.jsonl 落盘：每次会话新建文件，
    // 进行中的会话持续追加。逐层挑修改时间最新的子目录跟下去，最后取
    // 最深目录里最新一项的修改时间——目录的 mtime 在新文件创建时更新，
    // 文件的 mtime 在追加时更新，两条路都跟得住。
    if let Some(day_dir) = newest_dir(&codex.join("sessions")) {
        newest = newest.max(newest_stamp_in(&day_dir));
    }
    newest
}

/// 沿目录树逐层跟到修改时间最新的叶子目录；路径不存在时返回 `None`。
fn newest_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    while let Some(next) = newest_subdir(&dir) {
        dir = next;
    }
    if dir.exists() { Some(dir) } else { None }
}

/// 子目录里修改时间最新的一个；没有子目录时返回 `None`。
fn newest_subdir(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let stamp = stamp_of(&path);
            if newest.as_ref().map(|(at, _)| stamp > *at).unwrap_or(true) {
                newest = Some((stamp, path));
            }
        }
    }
    newest.map(|(_, path)| path)
}

/// 目录里所有条目（会话文件）的最新修改时间；目录不存在为 0。
fn newest_stamp_in(dir: &Path) -> u64 {
    let mut newest = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        newest = newest.max(stamp_of(&entry.path()));
    }
    newest
}

/// 单个文件或目录的修改时间（Unix 秒）；不存在按 0 处理。
fn stamp_of(path: &Path) -> u64 {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(UNIX_EPOCH);
    modified
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    fn temp_dir() -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        );
        let dir = std::env::temp_dir().join(format!("inkbuddy-aiq-act-{unique}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn missing_tree_stamps_zero() {
        let home = temp_dir();
        assert_eq!(stamp_at(&home), 0);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn session_file_is_seen_through_the_tree() {
        let home = temp_dir();
        let day = home.join(".codex/sessions/2026/11/28");
        fs::create_dir_all(&day).expect("mkdir");
        fs::write(day.join("rollout-1.jsonl"), "{}").expect("write");

        let stamp = stamp_at(&home);
        assert!(stamp > 0, "应能穿过年/月/日三层目录看到会话文件");

        // 追加进行中的会话：目录 mtime 不变，文件 mtime 变，痕迹要跟得上。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(day.join("rollout-1.jsonl"), "{}\n{}").expect("append");
        assert!(stamp_at(&home) > stamp, "追加写入应刷新活动痕迹");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn auth_json_counts_as_activity() {
        let home = temp_dir();
        let codex = home.join(".codex");
        fs::create_dir_all(codex.join("log")).expect("mkdir");
        fs::write(codex.join("auth.json"), "{}").expect("write");
        assert!(stamp_at(&home) > 0, "auth.json 的修改应被当作活动痕迹");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn newest_day_dir_wins() {
        let home = temp_dir();
        let sessions = home.join(".codex/sessions/2026/11");
        fs::create_dir_all(sessions.join("27")).expect("mkdir");
        fs::create_dir_all(sessions.join("28")).expect("mkdir");
        fs::write(sessions.join("27/old.jsonl"), "{}").expect("write");
        fs::write(sessions.join("28/new.jsonl"), "{}").expect("write");
        let stamp = stamp_at(&home);
        assert!(stamp > 0);
        fs::remove_dir_all(&home).ok();
    }
}
