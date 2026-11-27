//! 命令行版兜底：`agy -p "/usage"` 打印四行额度，按制表符分列。
//!
//! 命令行版把语言服务器的 CSRF 令牌留在进程内，命令行、日志、缓存里都没有，
//! 外部拿不到，只能让它自己算一遍。一次调用约 6 秒，所以只在语言服务器读不到
//! 时才走这条路，轮询间隔也放得更长。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::formatting::parse_rfc3339;
use crate::model::{Bucket, Snapshot, Source};

/// 触发额度输出的斜杠命令；同义命令是 `/quota`。
const USAGE_PROMPT: &str = "/usage";
/// 一次调用的上限。实测 6 秒左右，留出冷启动和网络慢的余量。
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// 找 `agy`：先看环境变量，再看常见安装位置，最后扫 PATH。
///
/// app 从 Finder 启动时 PATH 只有 `/usr/bin:/bin:/usr/sbin:/sbin`，
/// 所以固定位置要逐个试。
pub fn binary() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("ANTIGRAVITY_QUOTA_AGY") {
        let path = PathBuf::from(raw);
        if is_executable(&path) {
            return Some(path);
        }
    }

    let mut fixed: Vec<PathBuf> = vec![
        PathBuf::from("/opt/homebrew/bin/agy"),
        PathBuf::from("/usr/local/bin/agy"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        fixed.insert(0, PathBuf::from(home).join(".local/bin/agy"));
    }
    if let Some(found) = fixed.into_iter().find(|path| is_executable(path)) {
        return Some(found);
    }

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("agy"))
        .find(|path| is_executable(path))
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 跑一次 `/usage` 并解析输出；失败时把原因交回去，由服务决定怎么提示。
pub async fn collect() -> Result<Snapshot, String> {
    let Some(bin) = binary() else {
        return Err("未找到 agy 命令行".to_string());
    };

    // 命令行版每次运行都会写一份日志，指到临时目录去，别在用户的数据目录里
    // 每轮多出一个文件。
    let log = std::env::temp_dir().join("inkbuddy-agy-usage.log");
    let mut command = Command::new(&bin);
    command
        .arg("-p")
        .arg(USAGE_PROMPT)
        .arg("--log-file")
        .arg(&log)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("agy {} 超时（{} 秒）", USAGE_PROMPT, RUN_TIMEOUT.as_secs()))?
        .map_err(|err| format!("启动 {} 失败: {err}", bin.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(snapshot) = parse_usage(&stdout) {
        return Ok(snapshot);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let hint = first_meaningful_line(&stdout)
        .or_else(|| first_meaningful_line(&stderr))
        .unwrap_or_else(|| "没有输出".to_string());
    Err(format!(
        "agy {} 没有给出额度行（退出码 {}）: {hint}",
        USAGE_PROMPT,
        output.status.code().unwrap_or(-1)
    ))
}

/// 解析 `/usage` 的输出。每行四列：分组、窗口、剩余百分比、重置时间。
///
/// ```text
/// Gemini Models\tWeekly Limit Remaining\t89%\t2026-12-03T11:18:49Z
/// ```
///
/// 没用过的窗口重置时间是 1970-01-01，当作没有重置时间。
pub fn parse_usage(text: &str) -> Option<Snapshot> {
    let mut snapshot = Snapshot {
        source: Source::Cli,
        ..Snapshot::default()
    };

    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        if fields.len() < 3 {
            continue;
        }
        let Some(group) = group_of(fields[0]) else {
            continue;
        };
        let Some(window) = window_of(fields[1]) else {
            continue;
        };
        let Some(remaining) = percent_of(fields[2]) else {
            continue;
        };
        let resets_at = fields
            .get(3)
            .and_then(|raw| parse_rfc3339(raw))
            .filter(|stamp| *stamp > 0);
        let bucket = Bucket {
            used_percent: 100u32.saturating_sub(remaining.min(100)),
            resets_at,
        };
        match (group, window) {
            (Group::Gemini, Window::FiveHour) => snapshot.gemini.five_hour = Some(bucket),
            (Group::Gemini, Window::Weekly) => snapshot.gemini.weekly = Some(bucket),
            (Group::ThirdParty, Window::FiveHour) => {
                snapshot.third_party.five_hour = Some(bucket)
            }
            (Group::ThirdParty, Window::Weekly) => snapshot.third_party.weekly = Some(bucket),
        }
    }

    if snapshot.gemini.is_empty() && snapshot.third_party.is_empty() {
        None
    } else {
        Some(snapshot)
    }
}

#[derive(Clone, Copy)]
enum Group {
    Gemini,
    ThirdParty,
}

#[derive(Clone, Copy)]
enum Window {
    FiveHour,
    Weekly,
}

fn group_of(raw: &str) -> Option<Group> {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("gemini") {
        Some(Group::Gemini)
    } else if lower.contains("claude") || lower.contains("gpt") {
        Some(Group::ThirdParty)
    } else {
        None
    }
}

fn window_of(raw: &str) -> Option<Window> {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("five hour") || lower.contains("5 hour") || lower.contains("5h") {
        Some(Window::FiveHour)
    } else if lower.contains("weekly") || lower.contains("week") {
        Some(Window::Weekly)
    } else {
        None
    }
}

/// `89%` 这样的剩余百分比。
fn percent_of(raw: &str) -> Option<u32> {
    let digits = raw.trim().trim_end_matches('%').trim();
    digits.parse::<u32>().ok()
}

/// 取第一行非空文本，用于把命令行的报错塞进日志。
fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_OUTPUT: &str = "Gemini Models\tWeekly Limit Remaining\t89%\t2026-12-03T11:18:49Z\n\
Gemini Models\tFive Hour Limit Remaining\t77%\t2026-11-27T18:39:20Z\n\
Claude and GPT models\tWeekly Limit Remaining\t100%\t1970-01-01T00:00:00Z\n\
Claude and GPT models\tFive Hour Limit Remaining\t100%\t1970-01-01T00:00:00Z\n";

    #[test]
    fn parses_the_real_usage_output() {
        let snapshot = parse_usage(REAL_OUTPUT).expect("should parse");

        assert_eq!(snapshot.source, Source::Cli);
        let gemini_week = snapshot.gemini.weekly.expect("gemini weekly");
        assert_eq!(gemini_week.used_percent, 11);
        assert!(gemini_week.resets_at.is_some());

        let gemini_five = snapshot.gemini.five_hour.expect("gemini 5h");
        assert_eq!(gemini_five.used_percent, 23);

        // 没用过的窗口给的是 1970，按「没有重置时间」处理
        let third_week = snapshot.third_party.weekly.expect("3p weekly");
        assert_eq!(third_week.used_percent, 0);
        assert_eq!(third_week.resets_at, None);
    }

    #[test]
    fn ignores_noise_and_partial_lines() {
        assert!(parse_usage("").is_none());
        assert!(parse_usage("Fetching quota...\n").is_none());
        // 只有三列、缺重置时间也要能用
        let snapshot = parse_usage("Claude and GPT models\tWeekly Limit Remaining\t42%\n")
            .expect("should parse");
        let week = snapshot.third_party.weekly.expect("3p weekly");
        assert_eq!(week.used_percent, 58);
        assert_eq!(week.resets_at, None);
        assert!(snapshot.gemini.is_empty());
    }

    #[test]
    fn treats_unknown_groups_and_windows_as_absent() {
        let text = "Some Other Models\tWeekly Limit Remaining\t50%\t2026-12-03T11:18:49Z\n\
Gemini Models\tMonthly Limit Remaining\t50%\t2026-12-03T11:18:49Z\n";
        assert!(parse_usage(text).is_none());
    }

    #[test]
    fn clamps_percentages_to_the_displayable_range() {
        let snapshot = parse_usage("Gemini Models\tFive Hour Limit Remaining\t120%\n")
            .expect("should parse");
        assert_eq!(snapshot.gemini.five_hour.expect("5h").used_percent, 0);

        let snapshot = parse_usage("Gemini Models\tFive Hour Limit Remaining\t0%\n")
            .expect("should parse");
        assert_eq!(snapshot.gemini.five_hour.expect("5h").used_percent, 100);
    }

    #[test]
    fn finds_the_cli_binary_through_the_override() {
        // 指向一个必然可执行的程序，验证覆盖变量生效且校验可执行位
        let previous = std::env::var_os("ANTIGRAVITY_QUOTA_AGY");
        std::env::set_var("ANTIGRAVITY_QUOTA_AGY", "/bin/echo");
        assert_eq!(binary(), Some(PathBuf::from("/bin/echo")));

        std::env::set_var("ANTIGRAVITY_QUOTA_AGY", "/etc/hosts");
        assert_ne!(binary(), Some(PathBuf::from("/etc/hosts")));

        match previous {
            Some(value) => std::env::set_var("ANTIGRAVITY_QUOTA_AGY", value),
            None => std::env::remove_var("ANTIGRAVITY_QUOTA_AGY"),
        }
    }
}
