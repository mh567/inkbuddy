//! 够用就好的 HTTP/1.1 客户端：只向本机回环地址发一次 POST，读完整响应。
//!
//! 只是为了不引入 TLS 与 HTTP 依赖。回环上的语言服务器用的是明文 HTTP，
//! 请求带 `Connection: close`，所以读到 EOF 就是响应结束。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 单次响应的上限，正常额度响应在 100 KiB 以内。
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// 向 `127.0.0.1:port` 发一条 JSON POST，返回状态码和响应体。
pub async fn post_json(
    port: u16,
    path: &str,
    csrf: Option<&str>,
    body: &str,
    timeout: Duration,
) -> Result<Response, String> {
    match tokio::time::timeout(timeout, send(port, path, csrf, body)).await {
        Ok(result) => result,
        Err(_) => Err(format!("请求 {path} 超时")),
    }
}

async fn send(
    port: u16,
    path: &str,
    csrf: Option<&str>,
    body: &str,
) -> Result<Response, String> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|err| format!("连接 {addr} 失败: {err}"))?;

    let mut request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Accept: application/json\r\n\
         Accept-Encoding: identity\r\n\
         Content-Type: application/json\r\n\
         Connect-Protocol-Version: 1\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(token) = csrf {
        request.push_str(&format!("X-Codeium-Csrf-Token: {token}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| format!("发送请求失败: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("发送请求失败: {err}"))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = stream
            .read(&mut buf)
            .await
            .map_err(|err| format!("读取响应失败: {err}"))?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err("响应过大".to_string());
        }
    }

    parse_response(&raw)
}

/// 拆出状态码、响应头和响应体，处理 chunked 编码。
fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "响应缺少头部结束标记".to_string())?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("无法解析状态行: {status_line}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-length" => content_length = value.parse::<usize>().ok(),
            _ => {}
        }
    }

    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(length) = content_length {
        body.get(..length).unwrap_or(body).to_vec()
    } else {
        body.to_vec()
    };

    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// 解开 chunked 传输编码，每一块前面是十六进制长度。
fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| "chunked 块长度缺失".to_string())?;
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("chunked 块长度非法: {size_text}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        let chunk = data
            .get(..size)
            .ok_or_else(|| "chunked 块长度超出响应".to_string())?;
        out.extend_from_slice(chunk);
        // 每块后面跟一个 CRLF；最后一块可能是 0 长度直接结束。
        data = data.get(size + 2..).unwrap_or(&[]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        let response = parse_response(raw).expect("should parse");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":true}");
    }

    #[test]
    fn parses_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    7\r\n{\"a\":1,\r\n4\r\n\"b\":\r\n2\r\n2}\r\n0\r\n\r\n";
        let response = parse_response(raw).expect("should parse");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn keeps_error_status() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 4\r\n\r\nnope";
        let response = parse_response(raw).expect("should parse");
        assert_eq!(response.status, 401);
        assert_eq!(response.body, "nope");
    }

    #[test]
    fn rejects_malformed_response() {
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"HTTP/1.1 abc OK\r\n\r\n").is_err());
    }
}
