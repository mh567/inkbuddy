//! QRC 歌词解密与粤拼对齐。
//!
//! 算法参考 QQMusicApi / QQMusicDecoder 的行为，DES 表来自 FIPS 46-3。
//! DES / 3DES 的算法结构与 S-box 数值均为公开的 FIPS 标准事实；本文件用
//! Rust 独立实现，未复制参考项目的代码文本，也未引入其 GPL 许可声明。
//!
//! QQ 音乐 QRC 使用一套自定义 3DES 密钥扩展（PC-2 偏移量与标准不同），
//! 标准 des / openssl 实现无法解开，必须按参考实现的行为精确复现。

use flate2::read::ZlibDecoder;
use std::collections::HashMap;
use std::fmt;
use std::io::Read;

// ===================== 错误类型 =====================

#[derive(Debug)]
pub enum QrcError {
    InvalidHex(String),
    InvalidBlockLength(usize),
    Decompress(String),
    InvalidUtf8(std::string::FromUtf8Error),
}

impl fmt::Display for QrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QrcError::InvalidHex(s) => write!(f, "无效的 hex 字符串: {}", s),
            QrcError::InvalidBlockLength(n) => write!(f, "密文长度 {} 不是 8 的倍数", n),
            QrcError::Decompress(s) => write!(f, "zlib 解压失败: {}", s),
            QrcError::InvalidUtf8(e) => write!(f, "UTF-8 解码失败: {}", e),
        }
    }
}

impl std::error::Error for QrcError {}

// ===================== 公开数据结构 =====================

/// Exposed for plugin authors and tests; the plugin itself only needs
/// [`align_roma`], which keeps the internal `end` timestamp.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct QrcLine {
    pub start: f64,
    pub text: String,
    pub words: Vec<(String, f64)>, // (字或拼音, 绝对起始秒)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RomaLine {
    pub start: f64,
    pub end: f64,
    pub words: Vec<(String, String)>, // (字, 粤拼)
}

// ===================== 公开 API =====================

/// hex -> 自定义 3DES -> zlib -> QRC XML 文本。失败返回 Err。
pub fn qrc_decrypt(encrypted_hex: &str) -> Result<String, QrcError> {
    if encrypted_hex.is_empty() {
        return Ok(String::new());
    }
    let data = hex_decode(encrypted_hex)?;
    if data.len() % 8 != 0 {
        return Err(QrcError::InvalidBlockLength(data.len()));
    }
    let schedule = tripledes_key_setup(&QRC_KEY, Mode::Decrypt);
    let mut decrypted = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(8) {
        let block: [u8; 8] = chunk.try_into().unwrap();
        decrypted.extend_from_slice(&tripledes_crypt(&block, &schedule));
    }
    let mut decoder = ZlibDecoder::new(decrypted.as_slice());
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| QrcError::Decompress(e.to_string()))?;
    String::from_utf8(out).map_err(QrcError::InvalidUtf8)
}

/// 从 QRC XML 的 LyricContent="..." 属性取出内容并做 HTML 实体反转义。
pub fn extract_lyric_content(xml: &str) -> String {
    const MARK: &str = "LyricContent=\"";
    if let Some(start) = xml.find(MARK) {
        let begin = start + MARK.len();
        if let Some(rel) = xml[begin..].find('"') {
            return html_unescape(&xml[begin..begin + rel]);
        }
    }
    xml.to_string()
}

/// 解析 QRC 逐字行，过滤含「音译标注 / AI 工具」的声明行。
/// Exposed for plugin authors; the plugin calls [`align_roma`] directly.
#[allow(dead_code)]
pub fn parse_qrc_lines(content: &str) -> Vec<QrcLine> {
    parse_lines(content)
        .into_iter()
        .map(|p| {
            let text: String = p.words.iter().map(|(w, _)| w.as_str()).collect();
            QrcLine {
                start: p.start,
                text,
                words: p.words,
            }
        })
        .collect()
}

/// 把两个 QRC 文本（原文 + 罗马音）按行 start 时间对齐，产出带粤拼的行。
/// 仅保留能按 start 匹配到罗马音行、且逐字对齐成功的原文行
/// （元数据行、无音译行会被跳过）。
pub fn align_roma(lyric_xml: &str, roma_xml: &str) -> Vec<RomaLine> {
    let lyric_lines = parse_lines(&extract_lyric_content(lyric_xml));
    let roma_lines = parse_lines(&extract_lyric_content(roma_xml));

    let mut roma_by_start: HashMap<i64, &ParsedLine> = HashMap::new();
    for r in &roma_lines {
        let key = (r.start * 1000.0).round() as i64;
        roma_by_start.insert(key, r);
    }

    let mut result = Vec::new();
    for l in &lyric_lines {
        let key = (l.start * 1000.0).round() as i64;
        if let Some(r) = roma_by_start.get(&key) {
            let words: Vec<(String, String)> = l
                .words
                .iter()
                .zip(r.words.iter())
                .map(|((lw, _), (rw, _))| (lw.clone(), rw.trim().to_string()))
                .collect();
            if words.is_empty() {
                continue;
            }
            result.push(RomaLine {
                start: l.start,
                end: l.end,
                words,
            });
        }
    }
    result
}

// ===================== DES 常量 =====================

// ===== DES 表（FIPS 46-3 公开标准数值，按参考实现行为精确导出） =====
const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 15, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10,
        10, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

const KEY_RND_SHIFT: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

const KEY_PERM_C: [u32; 28] = [
    56, 48, 40, 32, 24, 16, 8, 0, 57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59,
    51, 43, 35,
];

const KEY_PERM_D: [u32; 28] = [
    62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 60, 52, 44, 36, 28, 20, 12, 4,
    27, 19, 11, 3,
];

const KEY_COMPRESSION: [u32; 48] = [
    13, 16, 10, 23, 0, 4, 2, 27, 14, 5, 20, 9, 22, 18, 11, 3, 25, 7, 15, 6, 26, 19, 12, 1, 40, 51,
    30, 36, 46, 54, 29, 39, 50, 44, 32, 47, 43, 48, 38, 55, 33, 52, 45, 41, 49, 35, 28, 31,
];

const QRC_KEY: [u8; 24] = *b"!@#)(*$%123ZXC!@!@#)(NHL";

// f() 末尾的 P 置换: (源位, 目标位)
const P_PERM: [(u32, u32); 32] = [
    (16, 31),
    (25, 30),
    (12, 29),
    (11, 28),
    (3, 27),
    (20, 26),
    (4, 25),
    (15, 24),
    (31, 23),
    (17, 22),
    (9, 21),
    (6, 20),
    (27, 19),
    (14, 18),
    (1, 17),
    (22, 16),
    (30, 15),
    (24, 14),
    (8, 13),
    (18, 12),
    (0, 11),
    (5, 10),
    (29, 9),
    (23, 8),
    (13, 7),
    (19, 6),
    (2, 5),
    (26, 4),
    (10, 3),
    (21, 2),
    (28, 1),
    (7, 0),
];

// ===================== DES 实现 =====================

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encrypt,
    Decrypt,
}

/// 把 6 位 S-box 输入重排成表内的线性索引（row * 16 + col）。
fn sbox_bit(a: u32) -> usize {
    ((a & 32) | ((a & 31) >> 1) | ((a & 1) << 4)) as usize
}

/// DES F 函数：扩展置换 -> 与子密钥异或 -> 8 个 S-box -> P 置换。
fn f(state: u32, key: &[u8; 6]) -> u32 {
    let t1 = ((state & 1) << 31)
        | ((state & 0xF800_0000) >> 1)
        | ((state & 0x1F80_0000) >> 3)
        | ((state & 0x01F8_0000) >> 5)
        | ((state & 0x001F_8000) >> 7);
    let t2 = ((state & 0x0001_F800) << 15)
        | ((state & 0x0000_1F80) << 13)
        | ((state & 0x0000_01F8) << 11)
        | ((state & 0x0000_001F) << 9)
        | ((state & 0x8000_0000) >> 23);

    let k0 = ((t1 >> 24) & 0xFF) ^ key[0] as u32;
    let k1 = ((t1 >> 16) & 0xFF) ^ key[1] as u32;
    let k2 = ((t1 >> 8) & 0xFF) ^ key[2] as u32;
    let k3 = ((t2 >> 24) & 0xFF) ^ key[3] as u32;
    let k4 = ((t2 >> 16) & 0xFF) ^ key[4] as u32;
    let k5 = ((t2 >> 8) & 0xFF) ^ key[5] as u32;

    let s = ((SBOX[0][sbox_bit(k0 >> 2)] as u32) << 28)
        | ((SBOX[1][sbox_bit(((k0 & 0x03) << 4) | (k1 >> 4))] as u32) << 24)
        | ((SBOX[2][sbox_bit(((k1 & 0x0F) << 2) | (k2 >> 6))] as u32) << 20)
        | ((SBOX[3][sbox_bit(k2 & 0x3F)] as u32) << 16)
        | ((SBOX[4][sbox_bit(k3 >> 2)] as u32) << 12)
        | ((SBOX[5][sbox_bit(((k3 & 0x03) << 4) | (k4 >> 4))] as u32) << 8)
        | ((SBOX[6][sbox_bit(((k4 & 0x0F) << 2) | (k5 >> 6))] as u32) << 4)
        | (SBOX[7][sbox_bit(k5 & 0x3F)] as u32);

    let mut out = 0u32;
    for &(src, dst) in &P_PERM {
        out |= ((s >> src) & 1) << dst;
    }
    out
}

/// 初始置换：8 字节 -> (L, R) 两个 32 位寄存器。
fn initial_permutation(input: &[u8; 8]) -> (u32, u32) {
    let x = u64::from_le_bytes(*input);
    let mut halves = [0u32; 2];
    for p in 0..2u32 {
        for g in 0..4u32 {
            let base = 6i32 + p as i32 - 2 * g as i32;
            for k in 0..4u32 {
                let src_v0 = (base + 8 * k as i32) as u32;
                let src_v1 = src_v0 + 32;
                let dst_hi = 31 - 8 * g - k;
                let dst_lo = 27 - 8 * g - k;
                halves[p as usize] |= (((x >> src_v1) & 1) as u32) << dst_hi;
                halves[p as usize] |= (((x >> src_v0) & 1) as u32) << dst_lo;
            }
        }
    }
    (halves[0], halves[1])
}

/// 逆初始置换：(L, R) -> 8 字节。
fn inverse_permutation(s0: u32, s1: u32) -> [u8; 8] {
    let mut data = [0u8; 8];
    for n in 0..8usize {
        let b = if n < 4 { 27 - n as i32 } else { 35 - n as i32 };
        let mut byte = 0u8;
        for k in 0..4 {
            let sh = (b - 8 * k as i32) as u32;
            byte |= (((s1 >> sh) & 1) as u8) << (7 - 2 * k as u32);
            byte |= (((s0 >> sh) & 1) as u8) << (6 - 2 * k as u32);
        }
        data[n] = byte;
    }
    data
}

/// 单块 DES：16 轮 Feistel（15 轮迭代 + 第 16 轮 + 逆置换）。
fn crypt(input: &[u8; 8], key: &[[u8; 6]; 16]) -> [u8; 8] {
    let (mut s0, mut s1) = initial_permutation(input);
    for idx in 0..15 {
        let prev_s1 = s1;
        s1 = f(s1, &key[idx]) ^ s0;
        s0 = prev_s1;
    }
    s0 = f(s1, &key[15]) ^ s0;
    inverse_permutation(s0, s1)
}

/// DES 密钥扩展（含参考实现的自定义 PC-2 偏移量）。
fn key_schedule(key: &[u8; 8], mode: Mode) -> [[u8; 6]; 16] {
    let mut schedule = [[0u8; 6]; 16];
    let v0 = u32::from_le_bytes([key[0], key[1], key[2], key[3]]);
    let v1 = u32::from_le_bytes([key[4], key[5], key[6], key[7]]);

    let mut c = 0u32;
    for (i, &b) in KEY_PERM_C.iter().enumerate() {
        let bit = if b < 32 {
            (v0 >> (31 - b)) & 1
        } else {
            (v1 >> (63 - b)) & 1
        };
        c |= bit << (31 - i as u32);
    }
    let mut d = 0u32;
    for (i, &b) in KEY_PERM_D.iter().enumerate() {
        let bit = if b < 32 {
            (v0 >> (31 - b)) & 1
        } else {
            (v1 >> (63 - b)) & 1
        };
        d |= bit << (31 - i as u32);
    }

    for i in 0..16 {
        let sh = KEY_RND_SHIFT[i];
        c = ((c << sh) | (c >> (28 - sh))) & 0xFFFF_FFF0;
        d = ((d << sh) | (d >> (28 - sh))) & 0xFFFF_FFF0;

        let togen = match mode {
            Mode::Decrypt => 15 - i,
            Mode::Encrypt => i,
        };

        for j in 0..24 {
            let bit = (c >> (31 - KEY_COMPRESSION[j])) & 1;
            schedule[togen][j / 8] |= (bit << (7 - (j % 8) as u32)) as u8;
        }
        for j in 24..48 {
            let bit = (d >> (31 - (KEY_COMPRESSION[j] - 27))) & 1;
            schedule[togen][j / 8] |= (bit << (7 - (j % 8) as u32)) as u8;
        }
    }
    schedule
}

/// 3DES 密钥设置：把 24 字节密钥按 [16..24], [8..16], [0..8] 分发到 3 组子密钥。
fn tripledes_key_setup(key: &[u8; 24], mode: Mode) -> [[[u8; 6]; 16]; 3] {
    let k0: [u8; 8] = key[16..24].try_into().unwrap();
    let k1: [u8; 8] = key[8..16].try_into().unwrap();
    let k2: [u8; 8] = key[0..8].try_into().unwrap();
    match mode {
        Mode::Decrypt => [
            key_schedule(&k0, Mode::Decrypt),
            key_schedule(&k1, Mode::Encrypt),
            key_schedule(&k2, Mode::Decrypt),
        ],
        Mode::Encrypt => [
            key_schedule(&k2, Mode::Encrypt),
            key_schedule(&k1, Mode::Decrypt),
            key_schedule(&k0, Mode::Encrypt),
        ],
    }
}

/// 3DES 单块：依次用 3 组子密钥跑 DES。
fn tripledes_crypt(data: &[u8; 8], key: &[[[u8; 6]; 16]; 3]) -> [u8; 8] {
    let mut buf = *data;
    for i in 0..3 {
        buf = crypt(&buf, &key[i]);
    }
    buf
}

// ===================== 工具函数 =====================

fn hex_val(b: u8) -> Result<u8, QrcError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(QrcError::InvalidHex(format!("非法字符 {:?}", b as char))),
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, QrcError> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(QrcError::InvalidHex("长度为奇数".into()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((hex_val(pair[0])? << 4) | hex_val(pair[1])?);
    }
    Ok(out)
}

fn decode_entity(ent: &str) -> Option<String> {
    match ent {
        "amp" => Some("&".into()),
        "lt" => Some("<".into()),
        "gt" => Some(">".into()),
        "quot" => Some("\"".into()),
        "apos" | "#39" => Some("'".into()),
        "nbsp" => Some("\u{00a0}".into()),
        _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16)
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string()),
        _ if ent.starts_with('#') => ent[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string()),
        _ => None,
    }
}

fn html_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'&' {
            if let Some(rel) = s[i + 1..].find(';') {
                let end = i + 1 + rel;
                if let Some(rep) = decode_entity(&s[i + 1..end]) {
                    out.push_str(&rep);
                    i = end + 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ===================== QRC 逐字解析 =====================

struct ParsedLine {
    start: f64,
    end: f64,
    words: Vec<(String, f64)>,
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// 手写扫描复现「文本 + 紧跟 (数字,数字)」的逐字匹配，文本段不含括号。
fn parse_lines(content: &str) -> Vec<ParsedLine> {
    let mut result = Vec::new();
    for raw in content.split('\n') {
        let line = raw.trim();
        if !line.starts_with('[') {
            continue;
        }
        let Some(close) = line.find(']') else {
            continue;
        };
        let head = &line[1..close];
        let Some((a, b)) = head.split_once(',') else {
            continue;
        };
        if !is_digits(a) || !is_digits(b) {
            continue;
        }
        let body = &line[close + 1..];
        if body.is_empty() {
            continue;
        }
        let start_ms: u64 = a.parse().unwrap();
        let dur_ms: u64 = b.parse().unwrap();

        let bb = body.as_bytes();
        let mut words: Vec<(String, f64)> = Vec::new();
        let mut i = 0usize;
        let mut text_start = 0usize;
        while i < bb.len() {
            if bb[i] == b'(' {
                if let Some(rel) = body[i + 1..].find(')') {
                    let j = i + 1 + rel;
                    let inner = &body[i + 1..j];
                    if let Some((p, q)) = inner.split_once(',') {
                        if is_digits(p) && is_digits(q) {
                            let text = body[text_start..i].trim();
                            if !text.is_empty() {
                                let ms: f64 = p.parse::<u64>().unwrap() as f64;
                                words.push((text.to_string(), ms / 1000.0));
                            }
                        }
                    }
                    i = j + 1;
                    text_start = i;
                    continue;
                } else {
                    i += 1;
                    text_start = i;
                    continue;
                }
            } else if bb[i] == b')' {
                i += 1;
                text_start = i;
                continue;
            } else {
                i += 1;
            }
        }

        if words.is_empty() {
            continue;
        }
        let text: String = words.iter().map(|(w, _)| w.as_str()).collect();
        if text.contains("音译标注") || text.contains("AI工具") || text.contains("AI 工具")
        {
            continue;
        }
        result.push(ParsedLine {
            start: start_ms as f64 / 1000.0,
            end: (start_ms + dur_ms) as f64 / 1000.0,
            words,
        });
    }
    result
}

// ===================== 单元测试 =====================

#[cfg(test)]
mod tests {
    use super::*;

    // 真实 roma 密文的前 8 字节（QQ 接口实跑），其明文应为 zlib 头 0x78 0x9c。
    const ROMA_HEAD_8_BYTES_HEX: &str = "34571D84AD813FA4";
    const SMALL_CIPHER_HEX: &str = "1EDC3E7B4186011AEAFA5191B545C9C507DA1EC4BFFBC345";
    const SMALL_PLAIN: &str = "Hello QRC 你好";

    #[test]
    fn des_single_block_known_answer() {
        let data = hex_decode(ROMA_HEAD_8_BYTES_HEX).unwrap();
        let block: [u8; 8] = data.as_slice().try_into().unwrap();
        let sched = tripledes_key_setup(&QRC_KEY, Mode::Decrypt);
        let plain = tripledes_crypt(&block, &sched);
        assert_eq!(plain[0], 0x78, "zlib 头首字节");
        assert_eq!(plain[1], 0x9c, "zlib 头次字节");
        assert_eq!(
            plain.to_vec(),
            vec![0x78, 0x9c, 0x55, 0x5a, 0x49, 0x6f, 0xa5, 0x57]
        );
    }

    #[test]
    fn qrc_decrypt_known_answer() {
        let out = qrc_decrypt(SMALL_CIPHER_HEX).unwrap();
        assert_eq!(out, SMALL_PLAIN);
    }

    #[test]
    fn extract_lyric_content_unescapes_entities() {
        let xml = "<?xml version=\"1.0\"?>\n<Lyric_1 LyricContent=\"A&amp;B &lt;tag&gt; &quot;q&quot; &#39;s&#39; &#x4F60;&#x597D; &nbsp;end\"/>";
        let c = extract_lyric_content(xml);
        assert_eq!(c, "A&B <tag> \"q\" 's' 你好 \u{00a0}end");
    }

    #[test]
    fn parse_qrc_lines_filters_declaration_and_brackets() {
        let content = concat!(
            "[0,9110]以下(0,700)音译(700,700)标注(1400,700)由(2100,700)AI(2800,700)工具(3500,700)生产(4200,700)\n",
            "[9118,3110]我(9118,180)会(9298,160)与(9458,280)你(9738,550)\n",
            "[12228,3350]半边生命-梁汉文(6259,569)Edmond(6828,569)",
        );
        let lines = parse_qrc_lines(content);
        assert_eq!(lines.len(), 2, "声明行应被过滤");
        assert_eq!(lines[0].start, 9.118);
        assert_eq!(lines[0].text, "我会与你");
        assert_eq!(
            lines[0].words,
            vec![
                ("我".to_string(), 9.118),
                ("会".to_string(), 9.298),
                ("与".to_string(), 9.458),
                ("你".to_string(), 9.738),
            ]
        );
        // 文本段不含括号, "(6259,569)" 的数字不会污染文字
        assert_eq!(lines[1].text, "半边生命-梁汉文Edmond");
    }
}
