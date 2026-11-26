//! 端到端验证协议循环：describe、未知方法回失败响应、shutdown 后退出。

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// 读一行响应并解析成 JSON。
fn read_reply(reader: &mut BufReader<ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response line");
    serde_json::from_str(line.trim()).expect("response is valid JSON")
}

#[test]
fn answers_unknown_method_and_exits_on_shutdown() {
    let bin = env!("CARGO_BIN_EXE_inkbuddy-plugin-ai-quota");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn plugin");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);

    // describe：带回 id，并返回插件 id 与名称。
    writeln!(stdin, r#"{{"id":1,"method":"describe"}}"#).expect("write describe");
    stdin.flush().expect("flush");
    let reply = read_reply(&mut reader);
    assert_eq!(reply["id"].as_u64(), Some(1));
    assert_eq!(reply["ok"].as_bool(), Some(true));
    assert_eq!(reply["result"]["id"].as_str(), Some("ai-quota"));
    assert_eq!(reply["result"]["name"].as_str(), Some("AI 额度"));

    // 未知方法：id 原样带回、ok=false，且进程不退出。
    writeln!(stdin, r#"{{"id":9,"method":"future_method"}}"#).expect("write unknown");
    stdin.flush().expect("flush");
    let reply = read_reply(&mut reader);
    assert_eq!(reply["id"].as_u64(), Some(9));
    assert_eq!(reply["ok"].as_bool(), Some(false));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "未知方法不应让插件退出"
    );

    // shutdown：回 ok 后进程退出。
    writeln!(stdin, r#"{{"id":2,"method":"shutdown"}}"#).expect("write shutdown");
    stdin.flush().expect("flush");
    let reply = read_reply(&mut reader);
    assert_eq!(reply["id"].as_u64(), Some(2));
    assert_eq!(reply["ok"].as_bool(), Some(true));

    // 有界轮询等待退出，回归时失败而不是挂住测试。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "插件应正常退出");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("shutdown 后插件未退出");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // shutdown 之后 stdout 应到达 EOF，没有多余输出。
    let mut rest = String::new();
    reader.read_to_string(&mut rest).expect("read to EOF");
    assert!(rest.is_empty(), "shutdown 后不应再有输出: {rest:?}");
}
