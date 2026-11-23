//! Lyric sources: QQ Music first (simplified Chinese, matches the desktop
//! player), LRCLIB as fallback. Results are cached as JSON files on disk.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

use crate::qrc::{self, RomaLine};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedSong {
    pub artist: String,
    pub title: String,
    pub source: String,
    /// (seconds, text)
    pub lines: Vec<(f64, String)>,
    /// Per-character Jyutping keyed by line start time.
    pub roma: Vec<RomaLineCache>,
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

impl LyricsSource {
    pub fn new(cache_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(8))
            .build()?;
        Ok(Self { http, cache_dir })
    }

    fn cache_path(&self, artist: &str, title: &str) -> PathBuf {
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| match c {
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                    _ => c,
                })
                .collect()
        };
        self.cache_dir
            .join(format!("{} - {}.json", safe(artist), safe(title)))
    }

    fn read_cache(&self, path: &Path) -> Option<CachedSong> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
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

    /// Fetch lyrics, using the on-disk cache when present.
    /// A cache hit still triggers a background Jyutping fetch if the cached
    /// entry has no roma, so early listeners upgrade automatically.
    pub async fn get(&self, artist: &str, title: &str) -> Option<CachedSong> {
        let path = self.cache_path(artist, title);
        if let Some(mut cached) = self.read_cache(&path) {
            debug!(artist, title, "lyrics cache hit");
            if cached.roma.is_empty() {
                if let Some(roma) = self.fetch_roma(artist, title).await {
                    if !roma.is_empty() {
                        cached.roma = roma.iter().map(RomaLineCache::from).collect();
                        self.write_cache(&path, &cached);
                    }
                }
            }
            return Some(cached);
        }

        let song = self.fetch_online(artist, title).await;
        if let Some(ref s) = song {
            if !s.lines.is_empty() {
                self.write_cache(&path, s);
            }
        }
        song
    }

    async fn fetch_online(&self, artist: &str, title: &str) -> Option<CachedSong> {
        // QQ first: its Mandarin songs are simplified and its Cantonese
        // versions match the desktop player. LRCLIB often returns traditional
        // Chinese uploads, so it is only a fallback.
        if let Some(s) = self.fetch_qq(artist, title).await {
            if !s.lines.is_empty() {
                debug!(artist, title, "using QQ lyrics");
                return Some(s);
            }
        }
        if let Some(s) = self.fetch_lrclib(artist, title).await {
            if !s.lines.is_empty() {
                debug!(artist, title, "using LRCLIB lyrics");
                return Some(s);
            }
        }
        None
    }

    // -- QQ Music -----------------------------------------------------------

    async fn qq_search(&self, artist: &str, title: &str) -> Option<(i64, String)> {
        let keyword = format!("{artist} {title}");
        let resp = self
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
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .ok()?;
        let v: serde_json::Value = resp.json().await.ok()?;
        let list = v.pointer("/data/song/list")?.as_array()?;
        let first = list.first()?;
        let id = first.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
        let mid = first.get("mid")?.as_str()?.to_string();
        if id == 0 || mid.is_empty() {
            return None;
        }
        Some((id, mid))
    }

    async fn fetch_qq(&self, artist: &str, title: &str) -> Option<CachedSong> {
        let (id, mid) = self.qq_search(artist, title).await?;

        let resp = self
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
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .ok()?;
        let v: serde_json::Value = resp.json().await.ok()?;
        let b64 = v.get("lyric")?.as_str()?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let lines = parse_lrc(&text);
        if lines.is_empty() {
            return None;
        }

        let roma = self
            .fetch_roma_by_id(id, &mid)
            .await
            .unwrap_or_default()
            .iter()
            .map(RomaLineCache::from)
            .collect();

        Some(CachedSong {
            artist: artist.to_string(),
            title: title.to_string(),
            source: "qq".to_string(),
            lines,
            roma,
        })
    }

    // -- Jyutping (QRC roma) ------------------------------------------------

    async fn fetch_roma(&self, artist: &str, title: &str) -> Option<Vec<RomaLine>> {
        let (id, mid) = self.qq_search(artist, title).await?;
        self.fetch_roma_by_id(id, &mid).await
    }

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
        let resp = self
            .http
            .get("https://u.y.qq.com/cgi-bin/musicu.fcg")
            .query(&[("format", "json"), ("data", body.as_str())])
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .ok()?;
        let v: serde_json::Value = resp.json().await.ok()?;
        let data = v.pointer("/lyric/data")?;
        let crypt = data.get("crypt")?.as_i64().unwrap_or(0);
        if crypt != 1 {
            return None;
        }
        let lyric_hex = data.get("lyric")?.as_str()?;
        let roma_hex = data.get("roma")?.as_str()?;
        if roma_hex.is_empty() {
            return None;
        }
        let lyric_xml = qrc::qrc_decrypt(lyric_hex).ok()?;
        let roma_xml = qrc::qrc_decrypt(roma_hex).ok()?;
        let lines = qrc::align_roma(&lyric_xml, &roma_xml);
        debug!(song_id, lines = lines.len(), "QRC roma parsed");
        if lines.is_empty() {
            None
        } else {
            Some(lines)
        }
    }

    // -- LRCLIB -------------------------------------------------------------

    async fn fetch_lrclib(&self, artist: &str, title: &str) -> Option<CachedSong> {
        // The exact-match endpoint is much less likely to return a wrong
        // traditional-Chinese upload than the search endpoint.
        let url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}",
            urlencoding::encode(artist),
            urlencoding::encode(title)
        );
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        let text = v
            .get("syncedLyrics")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("plainLyrics").and_then(|x| x.as_str()))?;
        let lines = parse_lrc(text);
        Some(CachedSong {
            artist: artist.to_string(),
            title: title.to_string(),
            source: "lrclib".to_string(),
            lines,
            roma: Vec::new(),
        })
    }
}

/// Parse LRC into sorted (seconds, text) pairs. Metadata tags and translation
/// lines (containing `/`) are skipped, matching the desktop project's rules.
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let metadata = ["ti", "ar", "al", "by", "offset", "length", "re", "ve"];
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('[') {
            continue;
        }
        let Some(close) = line.find(']') else {
            continue;
        };
        let tag = &line[1..close];
        if metadata.iter().any(|m| tag.starts_with(m)) {
            continue;
        }
        let Some((m, s)) = tag.split_once(':') else {
            continue;
        };
        let Ok(minutes) = m.trim().parse::<f64>() else {
            continue;
        };
        let Ok(seconds) = s.trim().parse::<f64>() else {
            continue;
        };
        let text = unescape_html(&line[close + 1..].trim());
        if text.is_empty() || text.contains('/') {
            continue;
        }
        out.push((minutes * 60.0 + seconds, text));
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn unescape_html(s: &str) -> String {
    s.replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Find the line index for a playback position using the same boundary rule
/// as the display: the current line is the last one whose start <= t.
/// Using `end` here would leave gaps during instrumental breaks and make the
/// ruby annotation disappear while the line is still on screen.
pub fn line_index_at(lines: &[(f64, String)], t: f64) -> Option<usize> {
    if lines.is_empty() {
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

    #[test]
    fn lrc_parsing_skips_metadata_and_translation() {
        let lrc = "[ti:Song]\n[ar:Artist]\n[00:12.30]第一句\n[00:15.00]a / b\n[00:18.00]第二句";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1, "第一句");
        assert!((lines[1].0 - 18.0).abs() < 1e-6);
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
}
