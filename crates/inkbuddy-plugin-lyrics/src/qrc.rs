//! QRC 歌词解密与粤拼对齐。
//!
//! QQ 音乐的 QRC 数据经过 hex 编码、自定义 3DES 加密和 zlib 压缩。DES 表采用
//! FIPS 46-3 公开标准数值；密钥扩展使用自定义的 PC-2 表（标准 PC-2 表的
//! 0-based 形式），标准 des / openssl 实现无法解开，因此这里按该密钥扩展实现。

use flate2::read::ZlibDecoder;
use std::fmt;
use std::io::Read;

/// zlib 解压输出上限（4 MiB），防止解压炸弹耗尽内存。
const MAX_DECOMPRESSED: usize = 4 * 1024 * 1024;

// ===================== 错误类型 =====================

#[derive(Debug)]
pub enum QrcError {
    InvalidHex(String),
    InvalidBlockLength(usize),
    Decompress(String),
    InvalidUtf8(std::string::FromUtf8Error),
    PayloadTooLarge { limit: usize },
}

impl fmt::Display for QrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QrcError::InvalidHex(s) => write!(f, "无效的 hex 字符串: {}", s),
            QrcError::InvalidBlockLength(n) => write!(f, "密文长度 {} 不是 8 的倍数", n),
            QrcError::Decompress(s) => write!(f, "zlib 解压失败: {}", s),
            QrcError::InvalidUtf8(e) => write!(f, "UTF-8 解码失败: {}", e),
            QrcError::PayloadTooLarge { limit } => {
                write!(f, "解压后的数据超过上限 {} 字节", limit)
            }
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
        return Err(QrcError::InvalidHex("输入为空".into()));
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
    // take 最多读 4 MiB + 1 字节：多读 1 字节用于区分「恰好到上限」与「超出上限」，
    // 同时避免把整个解压炸弹读进内存。
    let decoder = ZlibDecoder::new(decrypted.as_slice());
    let mut limited = decoder.take(MAX_DECOMPRESSED as u64 + 1);
    let mut out = Vec::new();
    limited
        .read_to_end(&mut out)
        .map_err(|e| QrcError::Decompress(e.to_string()))?;
    if out.len() > MAX_DECOMPRESSED {
        return Err(QrcError::PayloadTooLarge {
            limit: MAX_DECOMPRESSED,
        });
    }
    String::from_utf8(out).map_err(QrcError::InvalidUtf8)
}

/// 从 QRC XML 的 LyricContent="..." 属性取出内容并做 HTML 实体反转义。
/// 取不到属性时返回空字符串，不会把整份 XML 误当歌词内容。
/// 保留这个公开入口给插件作者；插件内部走 [`align_roma`]。
#[allow(dead_code)]
pub fn extract_lyric_content(xml: &str) -> String {
    try_extract_lyric_content(xml).unwrap_or_default()
}

/// 内部版本：找不到 LyricContent 属性时返回 None，供 [`align_roma`] 区分「无内容」。
fn try_extract_lyric_content(xml: &str) -> Option<String> {
    const MARK: &str = "LyricContent=\"";
    let start = xml.find(MARK)?;
    let begin = start + MARK.len();
    let rel = xml[begin..].find('"')?;
    Some(html_unescape(&xml[begin..begin + rel]))
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

/// 把两个 QRC 文本（原文 + 罗马音）按行 start 时间就近对齐，产出带粤拼的行。
/// 仅保留能在 0.3 秒内匹配到罗马音行、字数一致且存在非空读音的原文行
/// （元数据行、无音译行会被跳过）。
pub fn align_roma(lyric_xml: &str, roma_xml: &str) -> Vec<RomaLine> {
    let Some(lyric_content) = try_extract_lyric_content(lyric_xml) else {
        tracing::warn!("align_roma: 原文 XML 缺少 LyricContent 属性，跳过粤拼对齐");
        return Vec::new();
    };
    let Some(roma_content) = try_extract_lyric_content(roma_xml) else {
        tracing::warn!("align_roma: 罗马音 XML 缺少 LyricContent 属性，跳过粤拼对齐");
        return Vec::new();
    };

    let lyric_lines = parse_lines(&lyric_content);
    let roma_lines = parse_lines(&roma_content);

    const TOLERANCE: f64 = 0.3;
    let mut unmatched = 0usize;
    let mut result = Vec::new();
    for l in &lyric_lines {
        // 在容差内取时间最接近的罗马音行，避免 1ms 抖动就让整行丢失。
        let Some(r) = roma_lines
            .iter()
            .filter(|r| (r.start - l.start).abs() <= TOLERANCE)
            .min_by(|a, b| {
                (a.start - l.start)
                    .abs()
                    .total_cmp(&(b.start - l.start).abs())
            })
        else {
            unmatched += 1;
            continue;
        };
        // 字数不等时跳过整行，避免 zip 静默截断产生错位粤拼。
        if l.words.len() != r.words.len() {
            unmatched += 1;
            continue;
        }
        let words: Vec<(String, String)> = l
            .words
            .iter()
            .zip(r.words.iter())
            .filter_map(|((lw, _), (rw, _))| {
                let pron = rw.trim();
                (!pron.is_empty()).then(|| (lw.clone(), pron.to_string()))
            })
            .collect();
        if words.is_empty() {
            unmatched += 1;
            continue;
        }
        result.push(RomaLine {
            start: l.start,
            end: l.end,
            words,
        });
    }
    if unmatched > 0 {
        tracing::debug!(unmatched, "align_roma: 未能产出粤拼的歌词行数");
    }
    result
}

// ===================== DES 常量 =====================

// ===== DES 表（FIPS 46-3 公开标准数值） =====
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

// 标准 PC-2 的 0-based 形式：48 项即标准 PC-2 各值减 1，并非自定义偏移量。
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
    for (n, slot) in data.iter_mut().enumerate() {
        let b = if n < 4 { 27 - n as i32 } else { 35 - n as i32 };
        let mut byte = 0u8;
        for k in 0..4 {
            let sh = (b - 8 * k) as u32;
            byte |= (((s1 >> sh) & 1) as u8) << (7 - 2 * k as u32);
            byte |= (((s0 >> sh) & 1) as u8) << (6 - 2 * k as u32);
        }
        *slot = byte;
    }
    data
}

/// 单块 DES：16 轮 Feistel（15 轮迭代 + 第 16 轮 + 逆置换）。
fn crypt(input: &[u8; 8], key: &[[u8; 6]; 16]) -> [u8; 8] {
    let (mut s0, mut s1) = initial_permutation(input);
    for subkey in key.iter().take(15) {
        let prev_s1 = s1;
        s1 = f(s1, subkey) ^ s0;
        s0 = prev_s1;
    }
    s0 ^= f(s1, &key[15]);
    inverse_permutation(s0, s1)
}

/// DES 密钥扩展（PC-2 表采用标准 PC-2 的 0-based 形式，即标准值减 1）。
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

    for (i, &sh) in KEY_RND_SHIFT.iter().enumerate() {
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
    for k in key.iter().take(3) {
        buf = crypt(&buf, k);
    }
    buf
}

// ===================== 工具函数 =====================

fn hex_val(b: u8) -> Result<u8, QrcError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        // 用十六进制字节值而非 `b as char`，避免非 ASCII 字节打出乱码。
        _ => Err(QrcError::InvalidHex(format!("非法字符 0x{:02x}", b))),
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
        "hellip" => Some("\u{2026}".into()),
        "mdash" => Some("\u{2014}".into()),
        "ndash" => Some("\u{2013}".into()),
        "middot" => Some("\u{00b7}".into()),
        "ldquo" => Some("\u{201c}".into()),
        "rdquo" => Some("\u{201d}".into()),
        "lsquo" => Some("\u{2018}".into()),
        "rsquo" => Some("\u{2019}".into()),
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

/// HTML 实体反转义。单遍扫描，解码结果直接写入输出且不再回扫，
/// 等价于「amp 实体最后替换」：以 amp 实体开头的组合只还原成字面实体文本，
/// 不会被二次解码（例如不会还原出小于号）。
pub(crate) fn html_unescape(s: &str) -> String {
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

/// 纯数字且长度不超过 10 位，保证一定能安全解析为 u64（不会溢出 panic）。
fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.len() <= 10 && s.bytes().all(|c| c.is_ascii_digit())
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
        // is_digits 已限制位数，这里仍用 let-else 兜底，彻底消除 unwrap panic。
        let Ok(start_ms) = a.parse::<u64>() else {
            continue;
        };
        let Ok(dur_ms) = b.parse::<u64>() else {
            continue;
        };

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
                                // 解析失败只跳过该字：此处不能 continue，
                                // 否则 while 里 i 不前进会造成死循环。
                                if let Ok(ms) = p.parse::<u64>() {
                                    words.push((text.to_string(), ms as f64 / 1000.0));
                                }
                            }
                        }
                    }
                    i = j + 1;
                    text_start = i;
                    continue;
                }
                // 裸 '('（后面没有配对的 ')'）：当作普通正文字符，
                // 不重置 text_start，避免吞掉此前积累的文字。
            } else if bb[i] == b')' {
                // 裸 ')'：同样只前进游标，不重置 text_start。
            }
            i += 1;
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

    // ===================== 失败路径与健壮性 =====================

    /// 用 QRC 的 3DES 加密明文并转成 hex，便于构造自定义密文。
    fn encrypt_to_hex(plain: &[u8]) -> String {
        let mut padded = plain.to_vec();
        padded.resize(plain.len().div_ceil(8) * 8, 0);
        let sched = tripledes_key_setup(&QRC_KEY, Mode::Encrypt);
        let mut out = Vec::with_capacity(padded.len());
        for chunk in padded.chunks_exact(8) {
            let block: [u8; 8] = chunk.try_into().unwrap();
            out.extend_from_slice(&tripledes_crypt(&block, &sched));
        }
        out.iter().map(|b| format!("{:02X}", b)).collect()
    }

    fn qrc_xml(content: &str) -> String {
        format!("<Lyric_1 LyricContent=\"{}\"/>", content)
    }

    #[test]
    fn tripledes_encrypt_decrypt_roundtrip() {
        let plain = b"roundtrip payload 123";
        let data = hex_decode(&encrypt_to_hex(plain)).unwrap();
        let sched = tripledes_key_setup(&QRC_KEY, Mode::Decrypt);
        let mut dec = Vec::new();
        for chunk in data.chunks_exact(8) {
            let block: [u8; 8] = chunk.try_into().unwrap();
            dec.extend_from_slice(&tripledes_crypt(&block, &sched));
        }
        assert_eq!(&dec[..plain.len()], &plain[..]);
    }

    #[test]
    fn qrc_decrypt_rejects_empty_input() {
        assert!(matches!(qrc_decrypt(""), Err(QrcError::InvalidHex(_))));
    }

    #[test]
    fn qrc_decrypt_rejects_odd_length_hex() {
        assert!(matches!(qrc_decrypt("ABC"), Err(QrcError::InvalidHex(_))));
    }

    #[test]
    fn qrc_decrypt_rejects_invalid_hex_chars() {
        let err = qrc_decrypt("ZZ").unwrap_err();
        assert!(err.to_string().contains("0x5a"), "实际: {err}");
        // 非 ASCII 字节必须打印成十六进制，而不是乱码字符。
        let err = qrc_decrypt("é").unwrap_err();
        assert!(err.to_string().contains("0xc3"), "实际: {err}");
    }

    #[test]
    fn qrc_decrypt_rejects_non_multiple_of_eight() {
        assert!(matches!(
            qrc_decrypt("AABBCC"),
            Err(QrcError::InvalidBlockLength(3))
        ));
    }

    #[test]
    fn qrc_decrypt_rejects_corrupt_zlib() {
        let hex = encrypt_to_hex(b"this is definitely not a zlib stream");
        assert!(
            matches!(qrc_decrypt(&hex), Err(QrcError::Decompress(_))),
            "损坏的 zlib 应返回 Decompress"
        );
    }

    #[test]
    fn qrc_decrypt_rejects_decompression_bomb() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(&vec![b'x'; MAX_DECOMPRESSED + 1024]).unwrap();
        let compressed = enc.finish().unwrap();
        let hex = encrypt_to_hex(&compressed);
        assert!(
            matches!(qrc_decrypt(&hex), Err(QrcError::PayloadTooLarge { .. })),
            "解压超过 4 MiB 应返回 PayloadTooLarge"
        );
    }

    #[test]
    fn oversized_timestamps_do_not_panic() {
        // 行头时间戳超长：整行跳过，不能触发 u64 解析 panic。
        let head_overflow = "[999999999999999999999999,1000]你(0,50)好(50,50)";
        assert!(parse_qrc_lines(head_overflow).is_empty());
        // 逐字时间戳超长：该字被跳过，但不应 panic，也不应死循环。
        let word_overflow = "[0,1000]你(99999999999999999999,50)好(50,50)";
        let lines = parse_qrc_lines(word_overflow);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "好");
    }

    #[test]
    fn bare_parenthesis_does_not_swallow_text() {
        // 裸 ')' 不应吞掉其前的正文文字。
        let lines = parse_qrc_lines("[0,1000]喂(0,50)你好)世界(100,50)");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "喂你好)世界");
    }

    #[test]
    fn align_roma_matches_nearest_timestamp_within_tolerance() {
        let lyric = qrc_xml("[1000,900]你(1000,50)好(1050,50)");
        let roma = qrc_xml("[1100,900]nei(1100,50)hou(1150,50)");
        let lines = align_roma(&lyric, &roma);
        assert_eq!(lines.len(), 1, "0.1s 错位应能就近匹配");
        assert_eq!(
            lines[0].words,
            vec![
                ("你".to_string(), "nei".to_string()),
                ("好".to_string(), "hou".to_string()),
            ]
        );
    }

    #[test]
    fn align_roma_skips_timestamps_out_of_tolerance() {
        let lyric = qrc_xml("[1000,900]你(0,50)好(50,50)");
        let roma = qrc_xml("[2000,900]nei(0,50)hou(50,50)");
        assert!(align_roma(&lyric, &roma).is_empty(), "相差 1s 应不匹配");
    }

    #[test]
    fn align_roma_skips_mismatched_word_counts() {
        let lyric = qrc_xml("[0,1000]你(0,50)好(50,50)吗(100,50)");
        let roma = qrc_xml("[0,1000]nei(0,50)hou(50,50)");
        assert!(align_roma(&lyric, &roma).is_empty(), "字数不等应跳过整行");
    }

    #[test]
    fn align_roma_returns_empty_when_lyric_content_missing() {
        let xml = "<?xml version=\"1.0\"?><Lyric_1/>";
        assert!(align_roma(xml, xml).is_empty());
        // 公开 helper 也不再把整份 XML 误当歌词内容。
        assert_eq!(extract_lyric_content(xml), "");
    }

    #[test]
    fn html_unescape_handles_named_entities_and_amp_last() {
        // 用 concat! 拼接，避免源码里直接写实体串被工具链当成 HTML 解码。
        let input = concat!(
            "a", "&", "hellip;", "b", "&", "mdash;", "c", "&", "middot;", "d", "&", "ldquo;", "e",
            "&", "rdquo;", "f"
        );
        assert_eq!(
            html_unescape(input),
            "a\u{2026}b\u{2014}c\u{00b7}d\u{201c}e\u{201d}f"
        );
        // amp 实体等价于最后替换：以 amp 开头的组合只还原成字面实体文本。
        let amp_then_lt = concat!("&", "amp;", "lt;");
        assert_eq!(html_unescape(amp_then_lt), concat!("&", "lt;"));
        assert_eq!(
            html_unescape(concat!("&", "#x4F60;", "&", "#20320;")),
            "你你"
        );
    }
}
