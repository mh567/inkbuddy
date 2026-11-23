//! Lyric sources: QQ Music first (simplified Chinese, matches the desktop
//! player), LRCLIB as fallback. Results are cached as JSON files on disk.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

use crate::qrc::{self, RomaLine};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)";
/// QQ search results below this score are rejected (ported from the Python
/// prototype's `MIN_CONFIDENCE_THRESHOLD`), so covers / live / wrong-language
/// versions never win over LRCLIB.
const QQ_MIN_SCORE: i32 = 65;
/// Jyutping/LRC time-proximity tolerance when both texts match exactly.
const ROMA_TIME_TOLERANCE: f64 = 0.5;
/// Minimum Dice similarity for the text-only (time-shift tolerant) match.
const ROMA_TEXT_THRESHOLD: f64 = 0.6;
/// Cap on each field folded into the cache key, guarding against pathological
/// metadata. The hashed file name itself is always fixed-length.
const CACHE_KEY_FIELD_LIMIT: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedSong {
    pub artist: String,
    pub title: String,
    pub source: String,
    /// (seconds, text)
    pub lines: Vec<(f64, String)>,
    /// Raw QRC alignment result, keyed by its own start time. Kept for
    /// backward compatibility with existing cache files.
    #[serde(default)]
    pub roma: Vec<RomaLineCache>,
    /// Jyutping bound to LRC lines BY INDEX; length == lines.len().
    /// `None` means that line has no romanisation.
    #[serde(default)]
    pub roma_lines: Vec<Option<RomaLineCache>>,
    /// Already resolved "this song has no Jyutping"; do not hit the network
    /// again on every track switch.
    #[serde(default)]
    pub roma_checked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RomaLineCache {
    pub start: f64,
    pub end: f64,
    pub words: Vec<(String, String)>,
}

impl From<&RomaLine> for RomaLineCache {
    fn from(l: &RomaLine) -> Self {
        Self {
            start: l.start,
            end: l.end,
            words: l.words.clone(),
        }
    }
}

pub struct LyricsSource {
    http: reqwest::Client,
    cache_dir: PathBuf,
}

// -- Text normalisation / Jyutping binding ---------------------------------

fn norm_lower(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Strip whitespace and common punctuation (ASCII + CJK) so that an LRC line
/// and the concatenation of a QRC line's `words` compare equal.
fn normalize_match_text(s: &str) -> String {
    const ASCII_PUNCT: &str = ",.!?;:'\"()[]{}-_/\\~…";
    const CJK_PUNCT: &str = "，。！？；：「」『』（）【】、—～·";
    s.chars()
        .filter(|c| !c.is_whitespace() && !ASCII_PUNCT.contains(*c) && !CJK_PUNCT.contains(*c))
        .collect()
}

/// Longest common subsequence length (char based).
fn lcs_len(a: &[char], b: &[char]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for &ca in a {
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = if ca == cb {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Dice coefficient over the LCS; 1.0 for identical text.
fn text_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    2.0 * lcs_len(&ac, &bc) as f64 / (ac.len() + bc.len()) as f64
}

/// Bind each QRC `RomaLine` to an LRC line by normalised text. LRC and QRC
/// come from different endpoints, so their timestamps may be shifted as a
/// whole: a 0.3s soft time-match used to drop the entire song's Jyutping.
///
/// For every LRC line we first look for an unused roma line whose text matches
/// exactly and whose start is within `ROMA_TIME_TOLERANCE`; if none, we fall
/// back to the best text-only match (time-independent). Each roma line is used
/// at most once. The returned vector always has `lines.len()` elements.
fn bind_roma_to_lines(lines: &[(f64, String)], roma: &[RomaLine]) -> Vec<Option<RomaLineCache>> {
    let roma_text: Vec<String> = roma
        .iter()
        .map(|r| {
            let joined: String = r.words.iter().map(|(w, _)| w.as_str()).collect();
            normalize_match_text(&joined)
        })
        .collect();
    let line_text: Vec<String> = lines.iter().map(|(_, t)| normalize_match_text(t)).collect();

    let mut used = vec![false; roma.len()];
    let mut out: Vec<Option<RomaLineCache>> = Vec::with_capacity(lines.len());

    for (i, (t, _)) in lines.iter().enumerate() {
        let lt = &line_text[i];
        if lt.is_empty() {
            out.push(None);
            continue;
        }

        // Pass A: exact text + close timestamp, pick the closest timestamp.
        let mut chosen: Option<usize> = None;
        let mut best_dt = f64::INFINITY;
        for (j, r) in roma.iter().enumerate() {
            if used[j] || roma_text[j].as_str() != lt.as_str() {
                continue;
            }
            let dt = (r.start - t).abs();
            if dt <= ROMA_TIME_TOLERANCE && dt < best_dt {
                best_dt = dt;
                chosen = Some(j);
            }
        }

        // Pass B: best text match regardless of time (handles a global shift).
        if chosen.is_none() {
            let mut best_sim = ROMA_TEXT_THRESHOLD;
            for j in 0..roma.len() {
                if used[j] {
                    continue;
                }
                let s = text_similarity(lt.as_str(), roma_text[j].as_str());
                if s > best_sim {
                    best_sim = s;
                    chosen = Some(j);
                }
            }
        }

        match chosen {
            Some(j) => {
                used[j] = true;
                out.push(Some(RomaLineCache::from(&roma[j])));
            }
            None => out.push(None),
        }
    }
    out
}

/// Score a QQ search hit (ported from `qq.py::_score_result`).
fn qq_score(
    song: &serde_json::Value,
    artist: &str,
    title: &str,
    album: &str,
    duration: f64,
) -> i32 {
    let song_title = song
        .get("name")
        .and_then(|v| v.as_str())
        .map(norm_lower)
        .unwrap_or_default();
    let t_title = norm_lower(title);
    let mut score = 0;

    // Title (most important).
    if !song_title.is_empty() && !t_title.is_empty() {
        if song_title == t_title {
            score += 55;
        } else if t_title.contains(song_title.as_str()) || song_title.contains(t_title.as_str()) {
            score += 45;
        }
    }

    // Artist (QQ uses a `singer` array).
    let t_artist = norm_lower(artist);
    let artist_match = song
        .get("singer")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|s| {
                let a = s
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(norm_lower)
                    .unwrap_or_default();
                !a.is_empty()
                    && !t_artist.is_empty()
                    && (t_artist.contains(a.as_str()) || a.contains(t_artist.as_str()))
            })
        })
        .unwrap_or(false);
    if artist_match {
        score += 40;
    }

    // Album (only when the player reported one).
    let t_album = norm_lower(album);
    if !t_album.is_empty() {
        let song_album = song
            .pointer("/album/name")
            .and_then(|v| v.as_str())
            .map(norm_lower)
            .unwrap_or_default();
        if !song_album.is_empty()
            && (t_album.contains(song_album.as_str()) || song_album.contains(t_album.as_str()))
        {
            score += 20;
        }
    }

    // Duration within 3 seconds.
    if duration > 0.0 {
        let song_dur = song.get("interval").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if song_dur > 0.0 && (song_dur - duration).abs() <= 3.0 {
            score += 20;
        }
    }
    score
}

impl LyricsSource {
    pub fn new(cache_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(8))
            // `connect_timeout` is a client-level setting, so it applies to
            // every request built from this client.
            .connect_timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self { http, cache_dir })
    }

    /// Hash-based file name so different songs never overwrite each other and
    /// long metadata cannot exceed the file-system name limit.
    fn cache_path(&self, artist: &str, title: &str) -> PathBuf {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let trunc = |s: &str| -> String {
            s.to_lowercase()
                .chars()
                .take(CACHE_KEY_FIELD_LIMIT)
                .collect()
        };
        // "\0" prevents ("a","bc") and ("ab","c") from hashing identically.
        let key = format!("{}\0{}", trunc(artist), trunc(title));
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        self.cache_dir.join(format!("{:016x}.json", h.finish()))
    }

    fn read_cache(&self, path: &Path) -> Option<CachedSong> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str::<CachedSong>(&text) {
            Ok(song) => Some(song),
            Err(e) => {
                warn!(error = %e, "parsing lyric cache failed");
                None
            }
        }
    }

    fn write_cache(&self, path: &Path, song: &CachedSong) {
        match serde_json::to_string_pretty(song) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    warn!(error = %e, "writing lyric cache failed");
                }
            }
            Err(e) => warn!(error = %e, "serializing lyric cache failed"),
        }
    }

    /// Fetch lyrics, using the on-disk cache when present. A cache hit never
    /// triggers a network request: old caches are upgraded locally (rebinding
    /// raw `roma` to `roma_lines`) and marked `roma_checked` so the "confirmed
    /// no Jyutping" result is persisted instead of re-fetched every switch.
    pub async fn get(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> Option<CachedSong> {
        let path = self.cache_path(artist, title);
        if let Some(mut cached) = self.read_cache(&path) {
            debug!(artist = %artist, title = %title, "lyrics cache hit");
            if !cached.roma_checked {
                // Legacy cache: rebind the raw QRC result to LRC lines locally.
                if cached.roma_lines.is_empty() && !cached.roma.is_empty() {
                    let roma: Vec<RomaLine> = cached
                        .roma
                        .iter()
                        .map(|c| RomaLine {
                            start: c.start,
                            end: c.end,
                            words: c.words.clone(),
                        })
                        .collect();
                    let bound = bind_roma_to_lines(&cached.lines, &roma);
                    cached.roma_lines = bound;
                }
                // Whether or not we found any roma, resolve it once and persist.
                cached.roma_checked = true;
                self.write_cache(&path, &cached);
            }
            return Some(cached);
        }

        // Whole online fetch is capped so 4x8s serial requests cannot collide
        // with the host's 3s render timeout.
        let song = match tokio::time::timeout(
            Duration::from_millis(2500),
            self.fetch_online(artist, title, album, duration),
        )
        .await
        {
            Ok(s) => s,
            Err(_) => {
                warn!(artist = %artist, title = %title, "online lyric fetch timed out");
                None
            }
        };
        if let Some(ref s) = song {
            if !s.lines.is_empty() {
                self.write_cache(&path, s);
            }
        }
        song
    }

    async fn fetch_online(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> Option<CachedSong> {
        // QQ first: its Mandarin songs are simplified and its Cantonese
        // versions match the desktop player. LRCLIB often returns traditional
        // Chinese uploads, so it is only a fallback.
        if let Some(s) = self.fetch_qq(artist, title, album, duration).await {
            if !s.lines.is_empty() {
                debug!(artist = %artist, title = %title, "using QQ lyrics");
                return Some(s);
            }
        }
        if let Some(s) = self.fetch_lrclib(artist, title).await {
            if !s.lines.is_empty() {
                debug!(artist = %artist, title = %title, "using LRCLIB lyrics");
                return Some(s);
            }
        }
        None
    }

    /// Send a request and decode JSON, logging the source and reason at every
    /// failure point (instead of silently swallowing it with `.ok()?`).
    async fn fetch_json(
        &self,
        builder: reqwest::RequestBuilder,
        source: &str,
    ) -> Option<serde_json::Value> {
        let resp = match builder.send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(source = %source, error = %e, "HTTP request failed");
                return None;
            }
        };
        if !resp.status().is_success() {
            warn!(source = %source, status = %resp.status(), "HTTP response not successful");
            return None;
        }
        match resp.json::<serde_json::Value>().await {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(source = %source, error = %e, "response is not valid JSON");
                None
            }
        }
    }

    // -- QQ Music -----------------------------------------------------------

    async fn qq_search(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> Option<(i64, String)> {
        let keyword = format!("{artist} {title}");
        let builder = self
            .http
            .get("https://c.y.qq.com/soso/fcgi-bin/client_search_cp")
            .query(&[
                ("w", keyword.as_str()),
                ("format", "json"),
                ("p", "1"),
                ("n", "10"),
                ("aggr", "1"),
                ("lossless", "1"),
                ("cr", "1"),
                ("new_json", "1"),
                ("platform", "yqq.json"),
            ])
            .header("Referer", "https://y.qq.com/");
        let v = self.fetch_json(builder, "qq.search").await?;
        let list = match v.pointer("/data/song/list").and_then(|x| x.as_array()) {
            Some(l) if !l.is_empty() => l,
            _ => {
                debug!(artist = %artist, title = %title, "QQ search returned no songs");
                return None;
            }
        };

        // Pick the highest-scoring hit; reject anything below the threshold so
        // a same-name cover / live / wrong-language version never wins.
        let mut best: Option<(usize, i32)> = None; // (index, score)
        for (i, song) in list.iter().enumerate() {
            let s = qq_score(song, artist, title, album, duration);
            if best.map(|(_, bs)| s > bs).unwrap_or(true) {
                best = Some((i, s));
            }
        }
        let (idx, score) = best?;
        let song = &list[idx];
        if score < QQ_MIN_SCORE {
            debug!(artist = %artist, title = %title, score = %score, "QQ best match below threshold");
            return None;
        }
        let id = song.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
        let mid = song.get("mid")?.as_str()?.to_string();
        if id == 0 || mid.is_empty() {
            return None;
        }
        debug!(artist = %artist, title = %title, score = %score, id = %id, "QQ best match selected");
        Some((id, mid))
    }

    async fn fetch_qq(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        duration: f64,
    ) -> Option<CachedSong> {
        let (id, mid) = self.qq_search(artist, title, album, duration).await?;

        let builder = self
            .http
            .get("https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg")
            .query(&[
                ("songmid", mid.as_str()),
                ("g_tk", "5381"),
                ("g_tk_new_20200303", "5381"),
                ("loginUin", "0"),
                ("hostUin", "0"),
                ("format", "json"),
                ("inCharset", "utf8"),
                ("outCharset", "utf-8"),
                ("notice", "0"),
                ("platform", "yqq.json"),
                ("needNewCode", "0"),
            ])
            .header("Referer", "https://y.qq.com/");
        let v = self.fetch_json(builder, "qq.lyric").await?;
        let b64 = match v.get("lyric").and_then(|x| x.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => {
                debug!("qq.lyric: response has no 'lyric' field");
                return None;
            }
        };
        let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
            Ok(b) => b,
            Err(e) => {
                debug!(error = %e, "qq.lyric: base64 lyric decode failed");
                return None;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => {
                debug!(error = %e, "qq.lyric: lyric is not valid UTF-8");
                return None;
            }
        };
        let lines = parse_lrc(&text);
        if lines.is_empty() {
            return None;
        }

        // Reuse the very same selected song id/mid for the QRC roma fetch:
        // a second search could pick a different (wrong) track.
        let roma = self.fetch_roma_by_id(id, &mid).await.unwrap_or_default();
        let roma_lines = bind_roma_to_lines(&lines, &roma);
        let roma_cache: Vec<RomaLineCache> = roma.iter().map(RomaLineCache::from).collect();

        Some(CachedSong {
            artist: artist.to_string(),
            title: title.to_string(),
            source: "qq".to_string(),
            lines,
            roma: roma_cache,
            roma_lines,
            // The QQ path has now attempted the QRC roma fetch; the outcome
            // (present or absent) is final, so never re-fetch on a cache hit.
            roma_checked: true,
        })
    }

    // -- Jyutping (QRC roma) ------------------------------------------------

    async fn fetch_roma_by_id(&self, song_id: i64, mid: &str) -> Option<Vec<RomaLine>> {
        let payload = serde_json::json!({
            "comm": {"ct": "19", "cv": "2111", "tmeAppID": "qqmusic", "format": "json"},
            "lyric": {
                "module": "music.musichallSong.PlayLyricInfo",
                "method": "GetPlayLyricInfo",
                "param": {
                    "songID": song_id,
                    "songMid": mid,
                    "crypt": 1, "lrc_t": 0, "qrc": 1, "qrc_t": 0,
                    "trans": 1, "trans_t": 0, "roma": 1, "roma_t": 0,
                    "type": 0, "interval": 0
                }
            }
        });
        let body = serde_json::to_string(&payload).ok()?;
        let builder = self
            .http
            .get("https://u.y.qq.com/cgi-bin/musicu.fcg")
            .query(&[("format", "json"), ("data", body.as_str())])
            .header("Referer", "https://y.qq.com/");
        let v = self.fetch_json(builder, "qq.roma").await?;
        let data = match v.pointer("/lyric/data") {
            Some(d) => d,
            None => {
                debug!(song_id = %song_id, "QRC response missing /lyric/data");
                return None;
            }
        };
        let crypt = data.get("crypt")?.as_i64().unwrap_or(0);
        if crypt != 1 {
            debug!(song_id = %song_id, crypt = %crypt, "QRC response not encrypted");
            return None;
        }
        let lyric_hex = data.get("lyric")?.as_str()?;
        let roma_hex = data.get("roma")?.as_str()?;
        if roma_hex.is_empty() {
            debug!(song_id = %song_id, "QRC response has no roma data");
            return None;
        }
        let lyric_xml = match qrc::qrc_decrypt(lyric_hex) {
            Ok(x) => x,
            Err(e) => {
                debug!(song_id = %song_id, error = %e, "QRC lyric decrypt failed");
                return None;
            }
        };
        let roma_xml = match qrc::qrc_decrypt(roma_hex) {
            Ok(x) => x,
            Err(e) => {
                debug!(song_id = %song_id, error = %e, "QRC roma decrypt failed");
                return None;
            }
        };
        let lines = qrc::align_roma(&lyric_xml, &roma_xml);
        debug!(song_id = %song_id, lines = %lines.len(), "QRC roma parsed");
        if lines.is_empty() {
            None
        } else {
            Some(lines)
        }
    }

    // -- LRCLIB -------------------------------------------------------------

    async fn fetch_lrclib(&self, artist: &str, title: &str) -> Option<CachedSong> {
        // Exact-match endpoint first; a 404 or a missing syncedLyrics falls
        // back to the search endpoint, filtering for timestamped results.
        let get_url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}",
            urlencoding::encode(artist),
            urlencoding::encode(title)
        );
        let mut synced: Option<String> = None;
        match self.http.get(&get_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => {
                        synced = v
                            .get("syncedLyrics")
                            .and_then(|x| x.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string());
                    }
                    Err(e) => debug!(error = %e, "lrclib.get: response is not valid JSON"),
                }
            }
            Ok(resp) => {
                debug!(status = %resp.status(), "lrclib.get: exact match not found, will search");
            }
            Err(e) => warn!(error = %e, "lrclib.get: request failed, will search"),
        }

        if synced.is_none() {
            let q = format!("{artist} {title}");
            let search_url = format!(
                "https://lrclib.net/api/search?q={}",
                urlencoding::encode(&q)
            );
            let resp = match self.http.get(&search_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "lrclib.search: request failed");
                    return None;
                }
            };
            if !resp.status().is_success() {
                warn!(status = %resp.status(), "lrclib.search: response not successful");
                return None;
            }
            let v: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "lrclib.search: response is not valid JSON");
                    return None;
                }
            };
            synced = v.as_array().and_then(|list| {
                list.iter().find_map(|item| {
                    item.get("syncedLyrics")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                })
            });
        }

        // Plain lyrics have no timestamps and cannot drive a synced view, so
        // the old `or_else(plainLyrics)` fallback is intentionally dropped.
        let text = synced.filter(|s| !s.is_empty())?;
        let lines = parse_lrc(&text);
        if lines.is_empty() {
            debug!(artist = %artist, title = %title, "LRCLIB returned no timestamped lines");
            return None;
        }
        Some(CachedSong {
            artist: artist.to_string(),
            title: title.to_string(),
            source: "lrclib".to_string(),
            lines,
            roma: Vec::new(),
            roma_lines: Vec::new(),
            // LRCLIB has no QRC/Jyutping source; mark as resolved so a cache
            // hit never re-queries the network.
            roma_checked: true,
        })
    }
}

/// Parse LRC into sorted (seconds, text) pairs.
///
/// Every `[mm:ss.xx]` tag on a line produces its own pair (the body follows
/// the last leading tag). A `/` in the body only introduces a translation, so
/// we keep the first segment instead of dropping the whole line. Metadata
/// tags (`ti/ar/al/by/offset/length/re/ve`) are skipped, matching the Python
/// prototype; instrumental gaps are handled by `line_index_at` (start
/// boundary), not here.
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    const METADATA: [&str; 8] = ["ti", "ar", "al", "by", "offset", "length", "re", "ve"];
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if !line.starts_with('[') {
            continue;
        }

        // Walk the run of leading [tag] groups; collect all valid timestamps.
        let mut timestamps: Vec<f64> = Vec::new();
        let mut first_tag: Option<&str> = None;
        let mut cursor = 0usize;
        while cursor < line.len() && line.as_bytes()[cursor] == b'[' {
            let Some(rel) = line[cursor + 1..].find(']') else {
                break;
            };
            let close = cursor + 1 + rel;
            let tag = &line[cursor + 1..close];
            if first_tag.is_none() {
                first_tag = Some(tag);
            }
            if let Some((m, s)) = tag.split_once(':') {
                if let (Ok(minutes), Ok(seconds)) =
                    (m.trim().parse::<f64>(), s.trim().parse::<f64>())
                {
                    let t = minutes * 60.0 + seconds;
                    if t.is_finite() {
                        timestamps.push(t);
                    }
                }
            }
            cursor = close + 1;
        }

        // Metadata is judged on the first tag, as in the Python prototype.
        if first_tag
            .map(|t| METADATA.iter().any(|m| t.starts_with(*m)))
            .unwrap_or(false)
        {
            continue;
        }

        let body = &line[cursor..];
        let unescaped = qrc::html_unescape(body.trim());
        let main = unescaped.split('/').next().unwrap_or("").trim();
        if main.is_empty() {
            continue;
        }
        for t in timestamps {
            out.push((t, main.to_string()));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Find the line index for a playback position using the same boundary rule
/// as the display: the current line is the last one whose start <= t. Using
/// `end` here would leave gaps during instrumental breaks and make the ruby
/// annotation disappear while the line is still on screen. Before the first
/// line starts (intro) there is no current line, so `None` lets the caller
/// show the idle note.
pub fn line_index_at(lines: &[(f64, String)], t: f64) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    if t < lines[0].0 {
        return None;
    }
    let mut idx = 0usize;
    for (i, (start, _)) in lines.iter().enumerate() {
        if *start <= t {
            idx = i;
        } else {
            break;
        }
    }
    Some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: updated for the new behaviour. A `/` now only strips the
    // translation segment instead of discarding the whole line, so this LRC
    // yields three lines instead of two. Metadata tags are still skipped.
    #[test]
    fn lrc_parsing_skips_metadata_and_translation() {
        let lrc = "[ti:Song]\n[ar:Artist]\n[00:12.30]第一句\n[00:15.00]a / b\n[00:18.00]第二句";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].1, "第一句");
        assert_eq!(lines[1].1, "a", "slash keeps only the pre-translation part");
        assert!((lines[2].0 - 18.0).abs() < 1e-6);
    }

    #[test]
    fn line_index_uses_start_boundary_not_end() {
        let lines = vec![
            (0.0, "a".to_string()),
            (10.0, "b".to_string()),
            (17.2, "c".to_string()),
        ];
        // t=16 falls in a gap; it must stay on line b, not become None.
        assert_eq!(line_index_at(&lines, 16.0), Some(1));
        assert_eq!(line_index_at(&lines, 17.3), Some(2));
    }

    #[test]
    fn bind_roma_to_lines_matches_by_text_and_keeps_length() {
        let lines = vec![
            (10.0, "第一句".to_string()),
            (13.0, "第二句".to_string()),
            (16.0, "间奏无对应".to_string()),
        ];
        // QRC timestamps are globally shifted +2s: a 0.5s soft time-match
        // would drop the whole song, but text binding still matches.
        let roma = vec![
            RomaLine {
                start: 12.0,
                end: 13.0,
                words: vec![
                    ("第".into(), "dai6".into()),
                    ("一".into(), "jat1".into()),
                    ("句".into(), "geoi3".into()),
                ],
            },
            RomaLine {
                start: 15.0,
                end: 16.0,
                words: vec![
                    ("第".into(), "dai6".into()),
                    ("二".into(), "ji6".into()),
                    ("句".into(), "geoi3".into()),
                ],
            },
        ];
        let bound = bind_roma_to_lines(&lines, &roma);
        assert_eq!(bound.len(), lines.len(), "length must equal LRC line count");
        assert!(bound[0].is_some(), "first line matched despite time shift");
        assert!(bound[1].is_some(), "second line matched despite time shift");
        assert!(bound[2].is_none(), "line with no roma counterpart is None");
        assert_eq!(bound[0].as_ref().unwrap().words[0].0, "第");
    }

    #[test]
    fn bind_roma_to_lines_normalizes_punctuation() {
        let lines = vec![(5.0, "你 好， 世界".to_string())];
        let roma = vec![RomaLine {
            start: 5.1,
            end: 6.0,
            words: vec![
                ("你".into(), "nei5".into()),
                ("好".into(), "hou2".into()),
                ("世界".into(), "sai3".into()),
            ],
        }];
        let bound = bind_roma_to_lines(&lines, &roma);
        assert_eq!(bound.len(), 1);
        assert!(
            bound[0].is_some(),
            "whitespace/punctuation stripped before match"
        );
    }

    #[test]
    fn parse_lrc_multiple_timestamps_slash_and_nan() {
        let lrc = concat!(
            "[ti:标题]\n",
            "[ar:艺人]\n",
            "[00:12.30][00:20.00]副歌重复\n", // one tag per timestamp
            "[00:15.00]原文 / 译文部分\n",    // slash strips translation
            "[00:NaN]坏行A\n",                // non-finite timestamp skipped
            "[00:inf]坏行B\n",                // non-finite timestamp skipped
            "[00:18.00]第二句",
        );
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], (12.30, "副歌重复".to_string()));
        assert_eq!(lines[1], (15.0, "原文".to_string()));
        assert_eq!(lines[2], (18.0, "第二句".to_string()));
        assert_eq!(lines[3], (20.0, "副歌重复".to_string()));
    }

    #[test]
    fn cache_path_is_stable_and_case_insensitive() {
        let dir =
            std::env::temp_dir().join(format!("inkbuddy-lyrics-cache-key-{}", std::process::id()));
        let source = LyricsSource::new(dir.clone()).expect("test LyricsSource");
        let p1 = source.cache_path("Artist", "Song");
        let p2 = source.cache_path("artist", "song");
        let p3 = source.cache_path("Artist", "Other Song");
        assert_eq!(p1, p2, "case-insensitive: same song, same path");
        assert_ne!(p1, p3, "different songs must not overwrite each other");
        assert_eq!(p1, source.cache_path("Artist", "Song"), "hash is stable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn line_index_before_first_line_is_none() {
        let lines = vec![(5.0, "a".to_string()), (10.0, "b".to_string())];
        assert_eq!(line_index_at(&lines, 4.99), None, "intro must return None");
        assert_eq!(line_index_at(&lines, 5.0), Some(0));
    }

    #[tokio::test]
    async fn get_with_new_signature_uses_cache_without_network() {
        let dir =
            std::env::temp_dir().join(format!("inkbuddy-lyrics-get-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = LyricsSource::new(dir.clone()).expect("test LyricsSource");
        let (artist, title) = ("Artist", "Song");
        let path = source.cache_path(artist, title);
        // Legacy cache: has raw `roma`, no `roma_lines`, not yet checked.
        let legacy = CachedSong {
            lines: vec![(1.0, "歌词".to_string())],
            roma: vec![RomaLineCache {
                start: 1.0,
                end: 2.0,
                words: vec![("歌".into(), "go1".into()), ("词".into(), "ci4".into())],
            }],
            ..Default::default()
        };
        source.write_cache(&path, &legacy);

        // 4-argument signature is callable; a cache hit never touches the net.
        let got = source
            .get(artist, title, "Album", 200.0)
            .await
            .expect("cache hit");
        assert_eq!(got.lines.len(), 1);
        assert!(got.roma_checked, "legacy cache is marked resolved");
        assert_eq!(got.roma_lines.len(), 1, "roma bound to the single LRC line");
        assert!(got.roma_lines[0].is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
