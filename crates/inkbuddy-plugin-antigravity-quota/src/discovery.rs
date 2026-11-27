//! 找到本机正在运行的反重力语言服务器：端口和 CSRF 令牌从进程命令行读，
//! 读不到时退到日志里记录的端口。
//!
//! 语言服务器监听随机端口，且只在回环地址上监听；IDE 启动它时会把
//! `--csrf_token`、`--extension_server_port`、`--http_server_port` 写进命令行，
//! `ps` 能读到同用户进程的完整命令行，所以两条信息一次拿全。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;

use tokio::process::Command;

/// 一个可探测的目标：候选端口，以及与之配套的 CSRF 令牌（可能没有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub csrf: Option<String>,
    pub ports: Vec<u16>,
}

/// 反重力 app 的安装位置；决定「没装」和「装了但没开」两种提示。
const APP_PATH: &str = "/Applications/Antigravity.app";

/// 反重力留下的数据目录（IDE 与命令行版各一个）。
const DATA_DIRS: [&str; 2] = [".gemini/antigravity", ".gemini/antigravity-cli"];

/// `ps` 的绝对路径：app 从 Finder 启动时 PATH 很窄。
const PS_PATH: &str = "/bin/ps";

/// `ps` 输出的读取上限。正常机器上几百 KiB，这里只防某个进程带着超长参数把
/// 内存顶上去。
const MAX_PS_BYTES: usize = 8 * 1024 * 1024;

/// 日志里最多取这么多个候选端口，避免在过期的旧会话端口上耗光首轮预算。
const MAX_LOG_PORTS: usize = 4;

/// 只读日志尾部这么多字节，日志可能是几 MB。
const LOG_TAIL_BYTES: u64 = 64 * 1024;

/// 本机是否用过反重力：装了 app，或者留下了 IDE / 命令行版的数据目录。
pub fn app_installed() -> bool {
    if Path::new(APP_PATH).exists() {
        return true;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    DATA_DIRS.iter().any(|dir| home.join(dir).is_dir())
}

/// 收集候选目标：先取 `ps` 给的（带 CSRF 令牌），再补日志里 `ps` 没报过的端口。
pub async fn candidates() -> Vec<Candidate> {
    if let Some(candidate) = env_candidate() {
        return vec![candidate];
    }
    let from_process = process_candidates().await;
    merge_candidates(from_process, log_ports_from_disk().await)
}

/// 把 `ps` 的结果和日志端口合成候选列表。
///
/// 日志端口没有配套令牌，单独排在后面：`ps` 那条端口失效时才试得到，令牌也
/// 不会被发给日志里出现的端口。
fn merge_candidates(mut from_process: Vec<Candidate>, log_ports: Vec<u16>) -> Vec<Candidate> {
    let known: Vec<u16> = from_process
        .iter()
        .flat_map(|item| item.ports.clone())
        .collect();
    let extra: Vec<u16> = log_ports
        .into_iter()
        .filter(|port| !known.contains(port))
        .take(MAX_LOG_PORTS)
        .collect();
    if !extra.is_empty() {
        from_process.push(Candidate {
            csrf: None,
            ports: extra,
        });
    }
    from_process
}

/// 显式指定端口时跳过发现流程，便于调试：`ANTIGRAVITY_QUOTA_PORT=61235`，
/// 多个端口用逗号分隔；令牌用 `ANTIGRAVITY_QUOTA_CSRF` 给。
fn env_candidate() -> Option<Candidate> {
    let raw = std::env::var("ANTIGRAVITY_QUOTA_PORT").ok()?;
    let ports: Vec<u16> = raw
        .split(',')
        .filter_map(|item| item.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
        .collect();
    if ports.is_empty() {
        return None;
    }
    let csrf = std::env::var("ANTIGRAVITY_QUOTA_CSRF")
        .ok()
        .filter(|token| !token.trim().is_empty());
    Some(Candidate { csrf, ports })
}

/// 从 `ps` 输出里找语言服务器进程。
async fn process_candidates() -> Vec<Candidate> {
    let Ok(output) = Command::new(PS_PATH)
        .args(["-A", "-ww", "-o", "pid=,command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let bytes = &output.stdout[..output.stdout.len().min(MAX_PS_BYTES)];
    let text = String::from_utf8_lossy(bytes);

    let mut out: Vec<Candidate> = Vec::new();
    for line in text.lines() {
        let Some(candidate) = parse_ps_line(line) else {
            continue;
        };
        // 同一份端口被多个进程报出来时只留一条；令牌优先留有的。
        if let Some(existing) = out.iter_mut().find(|item| item.ports == candidate.ports) {
            if existing.csrf.is_none() {
                existing.csrf = candidate.csrf;
            }
            continue;
        }
        out.push(candidate);
    }
    out
}

/// 解析一行 `ps -A -ww -o pid=,command=` 的输出。
pub fn parse_ps_line(line: &str) -> Option<Candidate> {
    let line = line.trim();
    let (_pid, command) = line.split_once(char::is_whitespace)?;
    let command = command.trim_start();
    let lower = command.to_ascii_lowercase();

    // 语言服务器二进制名是 language_server；命令行里带令牌的那条也收，
    // 免得二进制被改名后整条判断失效。
    let looks_like_server =
        lower.contains("language_server") || lower.contains("--csrf_token");
    if !looks_like_server {
        return None;
    }

    let csrf = flag_value(command, "--csrf_token");
    let mut ports = Vec::new();
    for flag in [
        "--extension_server_port",
        "--http_server_port",
        "--https_server_port",
    ] {
        if let Some(value) = flag_value(command, flag) {
            if let Ok(port) = value.parse::<u16>() {
                push_port(&mut ports, port);
            }
        }
    }
    if ports.is_empty() {
        return None;
    }
    Some(Candidate { csrf, ports })
}

/// 取 `--flag=value` 或 `--flag value` 里的值，跳过同名后缀不同的参数。
fn flag_value(command: &str, flag: &str) -> Option<String> {
    let mut from = 0;
    while let Some(found) = command[from..].find(flag) {
        let start = from + found;
        let rest = &command[start + flag.len()..];
        let value = match rest.chars().next() {
            Some('=') => rest[1..].split_whitespace().next(),
            Some(ch) if ch.is_whitespace() => rest.split_whitespace().next(),
            _ => None,
        };
        if let Some(value) = value {
            let value = value.trim_matches(|ch| ch == '"' || ch == '\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        from = start + flag.len();
    }
    None
}

fn push_port(ports: &mut Vec<u16>, port: u16) {
    if port != 0 && !ports.contains(&port) {
        ports.push(port);
    }
}

/// 日志里记录的端口；进程已经改名或拿不到命令行时用。
async fn log_ports_from_disk() -> Vec<u16> {
    let Some(log) = tokio::task::spawn_blocking(newest_log_path)
        .await
        .ok()
        .flatten()
    else {
        return Vec::new();
    };
    let Some(text) = tokio::task::spawn_blocking(move || read_tail(&log, LOG_TAIL_BYTES))
        .await
        .ok()
        .flatten()
    else {
        return Vec::new();
    };
    log_ports(&text)
}

/// 候选日志文件按修改时间取最新的一份。
fn newest_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    let mut files = vec![
        home.join("Library/Logs/Antigravity/language_server.log"),
        home.join(".gemini/antigravity/log"),
        home.join(".gemini/antigravity-cli/log"),
    ]
    .into_iter()
    .flat_map(|path| {
        if path.is_dir() {
            newest_in_dir(&path).into_iter().collect::<Vec<_>>()
        } else if path.is_file() {
            vec![path]
        } else {
            Vec::new()
        }
    })
    .collect::<Vec<_>>();

    files.sort_by_key(|path| modified_at(path));
    files.pop()
}

/// 目录里按修改时间最新的 `.log`。
fn newest_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|ext| ext != "log").unwrap_or(true) {
            continue;
        }
        let stamp = modified_at(&path);
        if newest.as_ref().map(|(best, _)| stamp > *best).unwrap_or(true) {
            newest = Some((stamp, path));
        }
    }
    newest.map(|(_, path)| path)
}

fn modified_at(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// 反重力的活动痕迹：日志与数据目录里最新的修改时间（Unix 秒）。
///
/// 用户开始用反重力时，这些文件会立刻被改写（IDE 的 language_server.log、
/// 命令行每次会话的日志、会话库目录）。反重力没开的时候，服务只盯这个值，
/// 有变化才去跑一次完整取数，省掉每 15 秒一次的 `ps`。
pub fn activity_stamp() -> u64 {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };

    let mut newest = 0;
    for path in [
        home.join("Library/Logs/Antigravity/language_server.log"),
        home.join("Library/Logs/Antigravity/main.log"),
    ] {
        newest = newest.max(stamp_of(&path));
    }
    for dir in [
        home.join(".gemini/antigravity-cli/log"),
        home.join(".gemini/antigravity-cli/conversations"),
        home.join(".gemini/antigravity/log"),
        home.join(".gemini/antigravity/conversations"),
    ] {
        newest = newest.max(newest_stamp_in(&dir));
    }
    newest
}

/// 单个文件或目录的修改时间；不存在按 0 处理。
fn stamp_of(path: &Path) -> u64 {
    modified_at(path)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// 目录里最新的修改时间（不递归）。
fn newest_stamp_in(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| stamp_of(&entry.path()))
        .max()
        .unwrap_or_default()
}

/// 读文件最后 `limit` 个字节，日志很大时不整份读进来。
fn read_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let skip = len.saturating_sub(limit);
    if skip > 0 {
        file.seek(SeekFrom::Start(skip)).ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 从日志文本里挑出 HTTP 端口，靠后的（更近的会话）排在前面。
pub fn log_ports(text: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    for line in text.lines() {
        for port in ports_in_line(line) {
            // 后出现的优先：旧会话的端口早就关了。
            ports.retain(|item| *item != port);
            ports.push(port);
        }
    }
    ports.reverse();
    ports.truncate(MAX_LOG_PORTS);
    ports
}

fn ports_in_line(line: &str) -> Vec<u16> {
    let mut out = Vec::new();
    // "listening on random port at 58504 for HTTP"：HTTPS 那行是 gRPC，
    // 用 HTTP 探测会得到 400，所以只要 HTTP 这一行。
    if let Some(index) = line.find("random port at ") {
        if line.contains("for HTTP") {
            let rest = &line[index + "random port at ".len()..];
            if let Some(port) = leading_port(rest) {
                out.push(port);
            }
        }
    }
    // "listening on http://127.0.0.1:64253"、不带协议头的一条，以及
    // "listening on 127.0.0.1:58535, proxying to loopback port 58533"。
    for prefix in [
        "http://127.0.0.1:",
        "https://127.0.0.1:",
        "listening on 127.0.0.1:",
    ] {
        if let Some(index) = line.find(prefix) {
            let rest = &line[index + prefix.len()..];
            if let Some(port) = leading_port(rest) {
                out.push(port);
            }
        }
    }
    if let Some(index) = line.find("proxying to loopback port ") {
        let rest = &line[index + "proxying to loopback port ".len()..];
        if let Some(port) = leading_port(rest) {
            out.push(port);
        }
    }
    out
}

fn leading_port(text: &str) -> Option<u16> {
    let digits: String = text.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let port = digits.parse::<u16>().ok()?;
    if port == 0 {
        None
    } else {
        Some(port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ide_language_server_line() {
        let line = "  1234 /Applications/Antigravity.app/Contents/Resources/bin/language_server \
                    --csrf_token=abc123 --extension_server_port=58535 --http_server_port=0 \
                    --https_server_port=0 --app_data_dir antigravity-ide";
        let candidate = parse_ps_line(line).expect("should parse");
        assert_eq!(candidate.csrf.as_deref(), Some("abc123"));
        // 0 表示随机端口，不是可用端口
        assert_eq!(candidate.ports, vec![58535]);
    }

    #[test]
    fn parses_space_separated_flags() {
        let line = "9 /x/language_server --csrf_token token-1 --extension_server_port 50001";
        let candidate = parse_ps_line(line).expect("should parse");
        assert_eq!(candidate.csrf.as_deref(), Some("token-1"));
        assert_eq!(candidate.ports, vec![50001]);
    }

    #[test]
    fn ignores_processes_without_ports() {
        assert!(parse_ps_line("  42 /usr/bin/some --csrf_token=x").is_none());
        assert!(parse_ps_line("  42 /Applications/Antigravity.app/antigravity").is_none());
        assert!(parse_ps_line("  42 /usr/bin/ps -A -ww -o pid=,command=").is_none());
        assert!(parse_ps_line("no pid here").is_none());
    }

    #[test]
    fn collects_all_distinct_ports() {
        let line = "7 /b/language_server --csrf_token=t --extension_server_port=1 \
                    --http_server_port=2 --https_server_port=2";
        let candidate = parse_ps_line(line).expect("should parse");
        assert_eq!(candidate.ports, vec![1, 2]);
    }

    #[test]
    fn ignores_flags_that_only_share_a_prefix() {
        // --csrf_token_extra 不是 --csrf_token，不能把它的值当成令牌
        let line = "7 /b/language_server --csrf_token_extra=oops --http_server_port=9";
        let candidate = parse_ps_line(line).expect("should parse");
        assert_eq!(candidate.csrf, None);
        assert_eq!(candidate.ports, vec![9]);
    }

    #[test]
    fn merge_keeps_tokens_away_from_log_ports() {
        let from_ps = vec![Candidate {
            csrf: Some("token-1".to_string()),
            ports: vec![5000],
        }];
        let merged = merge_candidates(from_ps, vec![5000, 5001, 5002]);

        assert_eq!(merged.len(), 2);
        // 第一条来自进程，带令牌
        assert_eq!(merged[0].csrf.as_deref(), Some("token-1"));
        assert_eq!(merged[0].ports, vec![5000]);
        // 日志那条不带令牌，且不重复试进程已经报过的端口
        assert_eq!(merged[1].csrf, None);
        assert_eq!(merged[1].ports, vec![5001, 5002]);
    }

    #[test]
    fn merge_without_log_ports_keeps_process_candidates() {
        let from_ps = vec![Candidate {
            csrf: None,
            ports: vec![5000],
        }];
        let merged = merge_candidates(from_ps, Vec::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].ports, vec![5000]);
    }

    #[test]
    fn merge_caps_log_ports() {
        let log: Vec<u16> = (1..=20).collect();
        let merged = merge_candidates(Vec::new(), log);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].ports.len(), MAX_LOG_PORTS);
    }

    #[test]
    fn reads_ports_from_log_lines() {
        let text = "I: listening on random port at 58503 for HTTPS (gRPC)\n\
                    I: listening on random port at 58504 for HTTP\n\
                    I: listening on http://127.0.0.1:64253\n\
                    I: listening on 127.0.0.1:58535, proxying to loopback port 58533\n";
        let ports = log_ports(text);
        // 靠后的行优先，HTTPS(gRPC) 那行不进候选
        assert_eq!(ports, vec![58533, 58535, 64253, 58504]);
        assert!(!ports.contains(&58503));
    }

    #[test]
    fn log_ports_dedupes_and_caps() {
        let mut text = String::new();
        for port in 1..=8 {
            text.push_str(&format!("listening on http://127.0.0.1:{port}\n"));
        }
        text.push_str("listening on http://127.0.0.1:8\n");
        let ports = log_ports(&text);
        assert_eq!(ports.len(), MAX_LOG_PORTS);
        assert_eq!(ports[0], 8);
    }

    #[test]
    fn missing_paths_have_no_activity_stamp() {
        assert_eq!(stamp_of(Path::new("/nonexistent/inkbuddy/never")), 0);
        assert_eq!(newest_stamp_in(Path::new("/nonexistent/inkbuddy/never")), 0);
    }

    #[test]
    fn a_fresh_file_shows_up_as_activity() {
        let dir = std::env::temp_dir().join(format!("inkbuddy-activity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("cli.log"), b"x").expect("write file");

        assert!(newest_stamp_in(&dir) > 0);
        // 目录本身也算一处痕迹
        assert!(stamp_of(&dir) > 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
