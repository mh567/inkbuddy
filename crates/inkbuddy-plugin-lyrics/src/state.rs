//! Playback clock and lyric-window state.
//!
//! macOS players (notably QQ Music) report elapsed time as an occasional
//! snapshot that freezes between events. We anchor on each changed report and
//! advance locally in between. Seeking to 0 is a legitimate change and must
//! re-anchor too — filtering it out used to leave lyrics stuck mid-song.

use crate::nowplaying::{NowPlayingBackend, Playback};
use crate::source::{line_index_at, CachedSong, LyricsSource};
use inkbuddy_protocol::{Block, Emphasis, Line, RenderResult, Ruby};
use std::time::Instant;
use tracing::{debug, info};

/// Positive = lyrics appear earlier, to offset E-Ink refresh latency.
const LATENCY_COMPENSATION: f64 = 1.5;

pub struct PluginState {
    backend: Box<dyn NowPlayingBackend>,
    source: LyricsSource,

    last_track_id: Option<String>,
    last_reported: Option<f64>,
    position_base: f64,
    position_base_at: Instant,
    last_playing: bool,

    cached_track: Option<String>,
    cached_song: Option<CachedSong>,
    /// Set when a fetch failed, to avoid hammering the network every 500ms.
    fetch_deadline: Option<Instant>,
}

impl PluginState {
    pub fn new(backend: Box<dyn NowPlayingBackend>, source: LyricsSource) -> Self {
        Self {
            backend,
            source,
            last_track_id: None,
            last_reported: None,
            position_base: 0.0,
            position_base_at: Instant::now(),
            last_playing: false,
            cached_track: None,
            cached_song: None,
            fetch_deadline: None,
        }
    }

    /// Current playback position with latency compensation applied.
    fn position(&self, pb: &Playback) -> f64 {
        let base = if pb.playing {
            self.position_base + self.position_base_at.elapsed().as_secs_f64()
        } else {
            self.position_base
        };
        base + LATENCY_COMPENSATION
    }

    fn update_clock(&mut self, pb: &Playback) {
        let track_id = pb.track_id();
        let now = Instant::now();

        if self.last_track_id.as_deref() != Some(track_id.as_str()) {
            self.last_track_id = Some(track_id);
            self.last_reported = Some(pb.elapsed);
            self.position_base = pb.elapsed;
            self.position_base_at = now;
            self.last_playing = pb.playing;
            return;
        }

        // Re-anchor on ANY changed report, including a jump to 0.
        if self
            .last_reported
            .map(|v| (v - pb.elapsed).abs() > 0.01)
            .unwrap_or(true)
        {
            debug!(from = ?self.last_reported, to = pb.elapsed, "clock re-anchored");
            self.last_reported = Some(pb.elapsed);
            if pb.playing {
                self.position_base = pb.elapsed;
                self.position_base_at = now;
            }
        }

        if pb.playing && !self.last_playing {
            self.position_base_at = now; // resume: restart the local clock
        } else if !pb.playing && self.last_playing {
            self.position_base += self.position_base_at.elapsed().as_secs_f64();
            self.position_base_at = now; // pause: freeze
        }
        self.last_playing = pb.playing;
    }

    /// Build the three-line focused view for the current position.
    pub async fn render(&mut self) -> RenderResult {
        let pb = match self.backend.read() {
            Some(v) if v.has_track() => v,
            _ => return idle_frame(),
        };
        self.update_clock(&pb);

        let track_id = pb.track_id();
        if self.cached_track.as_deref() != Some(track_id.as_str()) {
            let allowed = self
                .fetch_deadline
                .map(|d| Instant::now() >= d)
                .unwrap_or(true);
            if allowed {
                match self.source.get(&pb.artist, &pb.title).await {
                    Some(song) => {
                        info!(title = %pb.title, source = %song.source, lines = song.lines.len(),
                              "lyrics loaded");
                        self.cached_track = Some(track_id.clone());
                        self.cached_song = Some(song);
                        self.fetch_deadline = None;
                    }
                    None => {
                        debug!(title = %pb.title, "no lyrics found, backing off");
                        self.fetch_deadline =
                            Some(Instant::now() + std::time::Duration::from_secs(10));
                    }
                }
            }
        }

        let Some(song) = self.cached_song.as_ref() else {
            return one_line_frame(&pb.title, "歌词加载中…");
        };

        let t = self.position(&pb);
        let Some(idx) = line_index_at(&song.lines, t) else {
            return one_line_frame(&pb.title, "♪");
        };

        let prev = idx.checked_sub(1).map(|i| song.lines[i].1.clone());
        let cur = song.lines[idx].1.clone();
        let next = song.lines.get(idx + 1).map(|l| l.1.clone());

        // Jyutping for the current line. Use the same start-based boundary as
        // the lyric line so the annotation never vanishes during a gap.
        let ruby = find_roma(&song.roma, &song.lines, idx);

        let mut lines = Vec::new();
        lines.push(Line::new(prev.unwrap_or_default(), Emphasis::Muted));

        let mut current = Line::new(cur, Emphasis::Primary);
        if !ruby.is_empty() {
            current.ruby = ruby
                .into_iter()
                .map(|(word, reading)| Ruby { word, reading })
                .collect();
        }
        lines.push(current);
        lines.push(Line::new(next.unwrap_or_default(), Emphasis::Muted));

        let meta = if pb.artist.is_empty() {
            pb.title.clone()
        } else {
            format!("{} — {}", pb.title, pb.artist)
        };

        RenderResult {
            blocks: vec![Block::Lines { lines }, Block::Text { text: meta }],
            refresh_ms: Some(500),
        }
    }
}

/// Match the roma line by the current line's start time, falling back to a
/// nearest-start match within 0.3s (QRC and LRC timestamps can differ slightly).
fn find_roma(
    roma: &[crate::source::RomaLineCache],
    lines: &[(f64, String)],
    idx: usize,
) -> Vec<(String, String)> {
    if roma.is_empty() {
        return Vec::new();
    }
    let start = lines[idx].0;
    let mut best: Option<(f64, &crate::source::RomaLineCache)> = None;
    for r in roma {
        // Never let a later line's annotation leak onto the previous line.
        if r.start > start + 0.3 {
            continue;
        }
        let d = (r.start - start).abs();
        if d <= 0.3 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, r));
        }
    }
    best.map(|(_, r)| r.words.clone()).unwrap_or_default()
}

fn idle_frame() -> RenderResult {
    RenderResult {
        blocks: vec![Block::Lines {
            lines: vec![Line::new("等待播放…", Emphasis::Normal)],
        }],
        refresh_ms: Some(1000),
    }
}

fn one_line_frame(title: &str, message: &str) -> RenderResult {
    RenderResult {
        blocks: vec![
            Block::Lines {
                lines: vec![Line::new(message, Emphasis::Normal)],
            },
            Block::Text {
                text: title.to_string(),
            },
        ],
        refresh_ms: Some(1000),
    }
}
