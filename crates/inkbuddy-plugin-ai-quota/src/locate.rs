//! 查找 Codex CLI 可执行文件。
//!
//! 查找顺序：显式配置、`PATH` 各目录、`~/.local/bin`。参数全部由调用方从环境
//! 读出后传入，函数本身不读环境变量，便于测试。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// 依次查显式配置、`PATH` 各目录、`~/.local/bin`，返回第一个存在的普通文件。
///
/// `explicit` 为 `Some` 时只看它：存在且是普通文件就返回，否则返回 `None`。
/// 用户显式配了路径就不静默回落到 `PATH`。
pub fn resolve_codex(
    explicit: Option<PathBuf>,
    path_dirs: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    fn is_file(path: &Path) -> bool {
        path.is_file()
    }

    if let Some(explicit) = explicit {
        return if is_file(&explicit) {
            Some(explicit)
        } else {
            None
        };
    }

    if let Some(path_dirs) = path_dirs {
        for dir in std::env::split_paths(&path_dirs) {
            let candidate = dir.join("codex");
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }

    if let Some(home) = home {
        let candidate = home.join(".local").join("bin").join("codex");
        if is_file(&candidate) {
            return Some(candidate);
        }
    }

    None
}

/// 从环境变量读出 `CODEX_BIN`、`PATH`、`HOME` 后调用 [`resolve_codex`]。
pub fn codex_from_env() -> Option<PathBuf> {
    let explicit = std::env::var_os("CODEX_BIN").map(PathBuf::from);
    if let Some(path) = explicit.as_deref() {
        if !path.is_file() {
            // 显式配置不回落是约定行为，但得让人知道为什么最后没找到。
            eprintln!(
                "ai-quota: CODEX_BIN={} 不存在，按约定不回落到 PATH",
                path.display()
            );
        }
    }
    resolve_codex(
        explicit,
        std::env::var_os("PATH"),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 唯一临时目录，测试结束删除。
    struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = format!(
                "inkbuddy-ai-quota-locate-{}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            );
            let dir = std::env::temp_dir().join(unique);
            fs::create_dir_all(&dir).expect("create temp dir");
            Self { dir }
        }

        fn path(&self) -> &Path {
            &self.dir
        }

        /// 在目录下创建可执行文件占位（普通文件即可，不需要执行位）。
        fn touch(&self, relative: &str) -> PathBuf {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, b"#!/bin/sh\n").expect("write file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// 把若干目录拼成 PATH 形式。
    fn path_value(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).expect("join paths")
    }

    #[test]
    fn explicit_existing_file_wins() {
        let temp = TempDir::new();
        let explicit = temp.touch("bin/codex");

        let found = resolve_codex(Some(explicit.clone()), None, None);
        assert_eq!(found, Some(explicit));
    }

    #[test]
    fn explicit_missing_file_does_not_fall_back_to_path() {
        let temp = TempDir::new();
        // PATH 里确实有一个可用的 codex，但显式配置缺文件时不能回落到它。
        let path_dir = temp.path().join("path-dir");
        fs::create_dir_all(&path_dir).expect("create path dir");
        let on_path = temp.touch("path-dir/codex");
        let missing = temp.path().join("missing/codex");

        let found = resolve_codex(
            Some(missing),
            Some(path_value(&[&path_dir])),
            Some(temp.path().to_path_buf()),
        );
        assert_eq!(found, None);
        // 确认测试环境本身是有效的：直接查 PATH 能找到那个文件。
        assert!(on_path.is_file());
    }

    #[test]
    fn returns_second_path_directory_when_first_misses() {
        let temp = TempDir::new();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first).expect("create first");
        let expected = temp.touch("second/codex");

        let found = resolve_codex(
            None,
            Some(path_value(&[&first, &second])),
            Some(temp.path().to_path_buf()),
        );
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn falls_back_to_home_local_bin() {
        let temp = TempDir::new();
        let empty = temp.path().join("empty");
        fs::create_dir_all(&empty).expect("create empty dir");
        let expected = temp.touch(".local/bin/codex");

        let found = resolve_codex(
            None,
            Some(path_value(&[&empty])),
            Some(temp.path().to_path_buf()),
        );
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn returns_none_when_nothing_exists() {
        let temp = TempDir::new();
        let empty = temp.path().join("empty");
        fs::create_dir_all(&empty).expect("create empty dir");

        let found = resolve_codex(
            None,
            Some(path_value(&[&empty])),
            Some(temp.path().to_path_buf()),
        );
        assert_eq!(found, None);
    }
}
