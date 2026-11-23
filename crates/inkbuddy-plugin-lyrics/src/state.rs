//! Playback clock and lyric-window state.
//!
//! macOS players (notably QQ Music) report elapsed time as an occasional
//! snapshot that freezes between events. We anchor on each changed report and
//! advance locally in between. Seeking to 0 is a legitimate change and must
//! re-anchor too — filtering it out used to leave lyrics stuck mid-song.

use crate::nowplaying::{NowPlayingBackend, Playback};
use crate::source::{line_index_at, CachedSong, LyricsSource};
use inkbuddy_protocol::{Block, Emphasis, Line, RenderResult, Ruby};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// A seek is only recognised when the player's report diverges from our
/// locally extrapolated clock by more than this many seconds. Normal playback
/// keeps the two in lock-step, so the local clock stays authoritative.
const SEEK_EPS: f64 = 1.5;
/// After the last lyric line plus this grace period, show an idle note (♪)
/// instead of freezing on the final line forever.
const TAIL_GRACE: f64 = 9.0;

pub struct PluginState {
    backend: Box<dyn NowPlayingBackend>,
    source: LyricsSource,

    last_track_id: Option<String>,
    position_base: f64,
    position_base_at: Instant,
    last_playing: bool,
    /// Positive = lyrics appear earlier, to offset E-Ink refresh latency.
    /// Default 0.0 (matches the Python prototype); configurable via
    /// INKBUDDY_LATENCY_MS (milliseconds) or Method::Configure.
    latency: f64,

    cached_track: Option<String>,
    cached_song: Option<CachedSong>,
    /// Set when a fetch failed, to avoid hammering the network every 500ms.
    fetch_deadline: Option<Instant>,
}

fn latency_from_env() -> f64 {
    std::env::var("INKBUDDY_LATENCY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|ms| ms / 1000.0)
        .unwrap_or(0.0)
}

impl PluginState {
    pub fn new(backend: Box<dyn NowPlayingBackend>, source: LyricsSource) -> Self {
        Self {
            backend,
            source,
            last_track_id: None,
            position_base: 0.0,
            position_base_at: Instant::now(),
            last_playing: false,
            latency: latency_from_env(),
            cached_track: None,
            cached_song: None,
            fetch_deadline: None,
        }
    }

    /// Write back a latency configured by the host, in milliseconds.
    pub fn set_latency_ms(&mut self, ms: f64) {
        self.latency = ms / 1000.0;
    }

    /// Current playback position with latency compensation applied.
    fn position(&self, pb: &Playback) -> f64 {
        let base = if pb.playing {
            self.position_base + self.position_base_at.elapsed().as_secs_f64() + self.latency
        } else {
            // Paused: freeze the clock and do NOT add latency compensation.
            self.position_base
        };
        // Never run past the end of the track (the local clock would otherwise
        // keep ticking forever and leave lyrics stuck on the last line).
        if pb.duration > 0.0 {
            base.min(pb.duration)
        } else {
            base
        }
    }

    fn update_clock(&mut self, pb: &Playback) {
        let track_id = pb.track_id();
        let now = Instant::now();

        if self.last_track_id.as_deref() != Some(track_id.as_str()) {
            self.last_track_id = Some(track_id);
            self.position_base = pb.elapsed;
            self.position_base_at = now;
            self.last_playing = pb.playing;
            return;
        }

        // Compare the player's report against what our local clock predicts.
        // While playing both advance together, so a normal frame is a no-op;
        // only a real seek (forward or back to 0, in ANY play state) exceeds
        // SEEK_EPS and re-anchors. Re-anchoring is NOT gated on `playing`.
        let local = self.position_base + self.position_base_at.elapsed().as_secs_f64();
        if (pb.elapsed - local).abs() > SEEK_EPS {
            debug!(
                reported = pb.elapsed,
                local = local,
                "clock re-anchored after seek"
            );
            self.position_base = pb.elapsed;
            self.position_base_at = now;
        }

        // pause/resume only toggles the local clock; it never corrects position.
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
                match self
                    .source
                    .get(&pb.artist, &pb.title, &pb.album, pb.duration)
                    .await
                {
                    Some(song) => {
                        info!(title = %pb.title, source = %song.source, lines = song.lines.len(),
                              "lyrics loaded");
                        self.cached_track = Some(track_id.clone());
                        self.cached_song = Some(song);
                        self.fetch_deadline = None;
                    }
                    None => {
                        debug!(title = %pb.title, "no lyrics found, backing off");
                        self.fetch_deadline = Some(Instant::now() + Duration::from_secs(10));
                    }
                }
            }
        }

        let Some(song) = self.cached_song.as_ref() else {
            return one_line_frame(&pb.title, "歌词加载中…");
        };

        let t = self.position(&pb);
        // Once we are well past the last line, show an idle note rather than
        // freezing on the final line (duration alone may exceed the lyrics).
        if song
            .lines
            .last()
            .map(|last| t > last.0 + TAIL_GRACE)
            .unwrap_or(false)
        {
            return one_line_frame(&pb.title, "♪");
        }

        let Some(idx) = line_index_at(&song.lines, t) else {
            return one_line_frame(&pb.title, "♪");
        };

        let prev = idx.checked_sub(1).map(|i| song.lines[i].1.clone());
        let cur = song.lines[idx].1.clone();
        let next = song.lines.get(idx + 1).map(|l| l.1.clone());

        // Jyutping is bound to LRC lines by index (roma_lines), not by a
        // timestamp soft-match: LRC and QRC come from different endpoints and
        // their timestamps may be shifted as a whole.
        let ruby: Vec<(String, String)> = song
            .roma_lines
            .get(idx)
            .and_then(|x| x.as_ref())
            .map(|r| r.words.clone())
            .unwrap_or_default();

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

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBackend(Option<Playback>);
    impl NowPlayingBackend for FakeBackend {
        fn read(&self) -> Option<Playback> {
            self.0.clone()
        }
    }

    fn make_pb(elapsed: f64, playing: bool) -> Playback {
        Playback {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: 300.0,
            elapsed,
            playing,
        }
    }

    fn state_with(pb: Option<Playback>) -> PluginState {
        let dir =
            std::env::temp_dir().join(format!("inkbuddy-lyrics-state-test-{}", std::process::id()));
        let source = LyricsSource::new(dir).expect("test LyricsSource");
        PluginState::new(Box::new(FakeBackend(pb)), source)
    }

    /// A state already anchored on `pb`'s track, with a known clock base.
    fn primed(pb: &Playback, base: f64, playing: bool) -> PluginState {
        let mut s = state_with(None);
        s.last_track_id = Some(pb.track_id());
        s.position_base = base;
        s.position_base_at = Instant::now();
        s.last_playing = playing;
        s
    }

    #[tokio::test]
    async fn seek_to_zero_reanchors() {
        let pb = make_pb(0.0, true);
        let mut s = primed(&pb, 100.0, true);
        s.update_clock(&pb);
        assert!(s.position_base.abs() < 1e-6, "seek to 0 must re-anchor");
    }

    #[tokio::test]
    async fn seek_while_paused_reanchors() {
        // Re-anchoring must NOT be gated on `pb.playing`: a paused seek to 0
        // used to be swallowed, leaving the wrong position after resume.
        let pb = make_pb(0.0, false);
        let mut s = primed(&pb, 100.0, false);
        s.update_clock(&pb);
        assert!(
            s.position_base.abs() < 1e-6,
            "paused seek to 0 must re-anchor"
        );
    }

    #[tokio::test]
    async fn local_extrapolation_reanchor_threshold() {
        let mut s = state_with(None);
        let pb = make_pb(12.0, true);
        s.last_track_id = Some(pb.track_id());
        s.position_base = 10.0;
        // Local clock has advanced ~2s, so local ~= 12 == reported 12.
        s.position_base_at = Instant::now() - Duration::from_secs(2);
        s.last_playing = true;

        s.update_clock(&pb);
        assert!(
            (s.position_base - 10.0).abs() < 1e-6,
            "small drift must NOT re-anchor (local clock stays authoritative)"
        );

        let seek = make_pb(20.0, true);
        s.update_clock(&seek);
        assert!(
            (s.position_base - 20.0).abs() < 1e-6,
            "divergence > SEEK_EPS must re-anchor"
        );
    }

    #[tokio::test]
    async fn position_clamped_to_duration() {
        let pb = make_pb(0.0, true);
        let mut s = primed(&pb, 1000.0, true);
        s.set_latency_ms(0.0);
        assert!(
            (s.position(&pb) - 300.0).abs() < 1e-6,
            "position must never exceed duration"
        );
    }

    #[tokio::test]
    async fn latency_configuration_takes_effect() {
        let pb_play = make_pb(0.0, true);
        let mut s = primed(&pb_play, 10.0, true);
        s.set_latency_ms(1500.0);
        assert!(
            (s.position(&pb_play) - 11.5).abs() < 0.05,
            "latency added while playing"
        );

        let pb_pause = make_pb(0.0, false);
        assert!(
            (s.position(&pb_pause) - 10.0).abs() < 0.05,
            "no latency compensation while paused"
        );
    }

    #[tokio::test]
    async fn after_last_line_shows_idle_note() {
        let pb = make_pb(100.0, true);
        let mut s = state_with(Some(pb.clone()));
        s.last_track_id = Some(pb.track_id());
        // Inject a loaded song so render() skips the network entirely.
        s.cached_track = Some(pb.track_id());
        s.cached_song = Some(CachedSong {
            lines: vec![(0.0, "line1".into()), (10.0, "line2".into())],
            ..Default::default()
        });
        s.position_base = 100.0;
        s.position_base_at = Instant::now();
        s.last_playing = true;
        s.set_latency_ms(0.0);

        let frame = s.render().await;
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains("♪"),
            "past last line + grace should show the idle note: {json}"
        );
    }
}
