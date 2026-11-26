//! 端到端验证协议循环：describe、未知方法回失败响应、shutdown 后退出。

use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// 起一个线程逐行读 stdout，用 channel 把行交回主线程。
///
/// 主线程据此可以用 `recv_timeout` 等待；插件"活着但不回"时测试会失败，
/// 而不是像直接阻塞 `read_line` 那样把 CI 挂住。
fn spawn_line_reader(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// 在超时内读一条响应并解析成 JSON；超时或提前 EOF 都直接让测试失败。
fn recv_reply(replies: &Receiver<String>) -> serde_json::Value {
    match replies.recv_timeout(REPLY_TIMEOUT) {
        Ok(line) => serde_json::from_str(line.trim()).expect("response is valid JSON"),
        Err(RecvTimeoutError::Timeout) => panic!("等待响应超时：插件可能不回响应"),
        Err(RecvTimeoutError::Disconnected) => panic!("插件在回响应前关闭了 stdout"),
    }
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
    let replies = spawn_line_reader(child.stdout.take().expect("stdout pipe"));

    // describe：带回 id，并返回插件 id 与名称。
    writeln!(stdin, r#"{{"id":1,"method":"describe"}}"#).expect("write describe");
    stdin.flush().expect("flush");
    let reply = recv_reply(&replies);
    assert_eq!(reply["id"].as_u64(), Some(1));
    assert_eq!(reply["ok"].as_bool(), Some(true));
    assert_eq!(reply["result"]["id"].as_str(), Some("ai-quota"));
    assert_eq!(reply["result"]["name"].as_str(), Some("AI 额度"));

    // 未知方法：id 原样带回、ok=false，且进程不退出。
    writeln!(stdin, r#"{{"id":9,"method":"future_method"}}"#).expect("write unknown");
    stdin.flush().expect("flush");
    let reply = recv_reply(&replies);
    assert_eq!(reply["id"].as_u64(), Some(9));
    assert_eq!(reply["ok"].as_bool(), Some(false));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "未知方法不应让插件退出"
    );

    // 纯非法 JSON 与缺 id：只写 stderr，进程不能退出，后续请求仍要正常。
    writeln!(stdin, "not json").expect("write malformed");
    stdin.flush().expect("flush");
    writeln!(stdin, r#"{{"method":"describe"}}"#).expect("write missing id");
    stdin.flush().expect("flush");
    thread::sleep(Duration::from_millis(100));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "非法输入不应让插件退出"
    );

    writeln!(stdin, r#"{{"id":10,"method":"describe"}}"#).expect("write describe after malformed");
    stdin.flush().expect("flush");
    let reply = recv_reply(&replies);
    assert_eq!(reply["id"].as_u64(), Some(10));
    assert_eq!(reply["ok"].as_bool(), Some(true));

    // shutdown：回 ok 后进程退出。
    writeln!(stdin, r#"{{"id":2,"method":"shutdown"}}"#).expect("write shutdown");
    stdin.flush().expect("flush");
    let reply = recv_reply(&replies);
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
        thread::sleep(Duration::from_millis(20));
    }

    // shutdown 之后 stdout 应到 EOF：读取线程结束、channel 断开，不再有数据。
    match replies.recv_timeout(REPLY_TIMEOUT) {
        Err(RecvTimeoutError::Disconnected) => {}
        Ok(extra) => panic!("shutdown 后不应再有输出: {extra:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("shutdown 后 stdout 未关闭"),
    }
}
