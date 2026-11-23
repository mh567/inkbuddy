//! 播放时钟与歌词窗口状态。
//!
//! QQ 音乐在 macOS 上把 `ElapsedTime` 全程冻结在 0（helper 和 get-raw 都是如此）。
//! 上报值发生变化时重设锚点，其余时间本地时钟自行推进；值保持不变视为没有跳转，
//! 拖到 0 属于变化，需要重设锚点。

use crate::nowplaying::{NowPlayingBackend, Playback};
use crate::player::{ClockMode, PlayerKind};
use crate::source::{line_index_at, CachedSong, LyricsSource};
use inkbuddy_protocol::{Block, Emphasis, Line, RenderResult};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

/// 上报值变化超过此秒数才算跳转。按整秒上报时的小幅步进不影响本地时钟。
const SEEK_EPS: f64 = 1.5;
/// Refuse a snapshot timestamp older than a day when aligning the clock.
const MAX_ANCHOR_AGE: f64 = 86_400.0;
/// 最后一行之后再过这段缓冲时间显示休止符（♪）。
const TAIL_GRACE: f64 = 9.0;

pub struct PluginState {
    backend: Box<dyn NowPlayingBackend>,
    source: LyricsSource,

    last_track_id: Option<String>,
    /// 播放器上一次上报的 ElapsedTime。该值长期冻结属正常情况，不参与本地时钟
    /// 比较；比较会把进度反复拨回 0，歌词在开头几行来回跳。
    last_reported: f64,
    position_base: f64,
    position_base_at: Instant,
    last_playing: bool,
    /// 提前量（秒），按播放器分别保存：QQ 音乐用 INKBUDDY_LATENCY_MS，
    /// 其它播放器用 INKBUDDY_LATENCY_OTHER_MS。正值表示歌词提前出现，
    /// 抵消墨水屏刷新延迟；也可通过 Method::Configure 分别调整。
    qq_latency: f64,
    other_latency: f64,

    cached_track: Option<String>,
    cached_song: Option<CachedSong>,
    /// Set when a fetch failed, to avoid hammering the network every 500ms.
    fetch_deadline: Option<Instant>,
}

fn latency_from_env(name: &str) -> f64 {
    std::env::var(name)
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
            last_reported: 0.0,
            position_base: 0.0,
            position_base_at: Instant::now(),
            last_playing: false,
            qq_latency: latency_from_env("INKBUDDY_LATENCY_MS"),
            other_latency: latency_from_env("INKBUDDY_LATENCY_OTHER_MS"),
            cached_track: None,
            cached_song: None,
            fetch_deadline: None,
        }
    }

    /// 设置 QQ 音乐的提前量（毫秒）。保留原方法名，兼容既有 Configure 调用。
    pub fn set_latency_ms(&mut self, ms: f64) {
        self.qq_latency = ms / 1000.0;
    }

    /// 设置其它播放器的提前量（毫秒）。
    pub fn set_other_latency_ms(&mut self, ms: f64) {
        self.other_latency = ms / 1000.0;
    }

    fn player(pb: &Playback) -> PlayerKind {
        PlayerKind::from_bundle_id(pb.bundle_id.as_deref())
    }

    /// Current playback position with latency compensation applied.
    fn position(&self, pb: &Playback) -> f64 {
        let player = Self::player(pb);
        let latency = match player {
            PlayerKind::QqMusic => self.qq_latency,
            _ => self.other_latency,
        };
        let base = match player.clock_mode() {
            // QQ 冻结上报值：用本地时钟推进。
            ClockMode::FrozenLocal => {
                if pb.playing {
                    self.position_base + self.position_base_at.elapsed().as_secs_f64()
                } else {
                    self.position_base
                }
            }
            // 其它播放器上报值可用：直接用它，不本地推进。
            ClockMode::ReportElapsed => pb.elapsed,
        };
        let base = if pb.playing { base + latency } else { base };
        // Never run past the end of the track (the local clock would otherwise
        // keep ticking forever and leave lyrics stuck on the last line).
        if pb.duration > 0.0 {
            base.min(pb.duration)
        } else {
            base
        }
    }

    /// Position at this instant, derived from the player's snapshot.
    ///
    /// MediaRemote publishes ElapsedTime together with the moment the snapshot
    /// was taken (`timestamp`), and often leaves both frozen for the whole
    /// track. The real position is `elapsed + (now - timestamp)`; wrapping at
    /// the duration also covers single-track repeat. Paused playback and
    /// INKBUDDY_TIMESTAMP_ANCHOR=0 fall back to the raw reported value.
    fn aligned_position(pb: &Playback) -> f64 {
        if !pb.playing {
            return pb.elapsed;
        }
        if std::env::var("INKBUDDY_TIMESTAMP_ANCHOR")
            .map(|v| v == "0")
            .unwrap_or(false)
        {
            return pb.elapsed;
        }
        let Some(ts) = pb.reported_at else {
            return pb.elapsed;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let age = now - ts;
        if age <= 0.0 || age > MAX_ANCHOR_AGE {
            return pb.elapsed;
        }
        let pos = pb.elapsed + age;
        let aligned = if pb.duration > 0.0 {
            pos % pb.duration
        } else {
            pos
        };
        info!(
            reported = pb.elapsed,
            snapshot_age = age,
            aligned = aligned,
            "aligned playback position from snapshot timestamp"
        );
        aligned
    }

    fn update_clock(&mut self, pb: &Playback) {
        let track_id = pb.track_id();
        let now = Instant::now();
        let new_track = self.last_track_id.as_deref() != Some(track_id.as_str());
        let previous_reported = self.last_reported;
        self.last_track_id = Some(track_id);
        self.last_reported = pb.elapsed;

        // 其它播放器直接用上报进度，本地时钟不参与。
        if Self::player(pb).clock_mode() == ClockMode::ReportElapsed {
            self.last_playing = pb.playing;
            return;
        }

        // QQ 音乐：上报值长期冻结，用快照时间对齐起点并本地推进。
        if new_track {
            self.position_base = Self::aligned_position(pb);
            self.position_base_at = now;
            self.last_playing = pb.playing;
            return;
        }

        // 只有上报值发生变化才算跳转。把冻结的上报值与本地时钟比较会反复重设
        // 锚点到 0，歌词在开头几行来回跳。
        if (pb.elapsed - previous_reported).abs() > SEEK_EPS {
            debug!(
                reported = pb.elapsed,
                previous = previous_reported,
                "clock re-anchored after seek"
            );
            self.position_base = Self::aligned_position(pb);
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
        let player = Self::player(&pb);
        debug!(player = player.as_str(), "now playing source");
        self.update_clock(&pb);

        let track_id = pb.track_id();
        if self.cached_track.as_deref() != Some(track_id.as_str()) {
            let allowed = self
                .fetch_deadline
                .map(|d| Instant::now() >= d)
                .unwrap_or(true);
            if allowed {
                let strategy = player.lyric_strategy();
                match self
                    .source
                    .get(strategy, &pb.artist, &pb.title, &pb.album, pb.duration)
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
        // 超过最后一行后显示休止符；曲目时长可能长于歌词。
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

        // 粤拼按行索引（roma_lines）与 LRC 绑定。LRC 与 QRC 来自不同接口，
        // 时间戳可能整体错开，因此不按时间软匹配。
        let jyutping: Vec<String> = song
            .roma_lines
            .get(idx)
            .and_then(|x| x.as_ref())
            .map(|r| r.words.iter().map(|(_, reading)| reading.clone()).collect())
            .unwrap_or_default();

        let mut lines = Vec::new();
        lines.push(Line::new(prev.unwrap_or_default(), Emphasis::Muted));
        lines.push(Line::new(cur, Emphasis::Primary));
        if !jyutping.is_empty() {
            lines.push(Line::new(jyutping.join(" "), Emphasis::Reading));
        }
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
            bundle_id: Some("com.tencent.QQMusicMac".into()),
            duration: 300.0,
            elapsed,
            playing,
            reported_at: None,
        }
    }

    fn state_with(pb: Option<Playback>) -> PluginState {
        let dir =
            std::env::temp_dir().join(format!("inkbuddy-lyrics-state-test-{}", std::process::id()));
        let source = LyricsSource::new(dir).expect("test LyricsSource");
        PluginState::new(Box::new(FakeBackend(pb)), source)
    }

    /// A state already anchored on `pb`'s track, with a known clock base and
    /// the last value the player reported.
    fn primed(pb: &Playback, base: f64, reported: f64, playing: bool) -> PluginState {
        let mut s = state_with(None);
        s.last_track_id = Some(pb.track_id());
        s.last_reported = reported;
        s.position_base = base;
        s.position_base_at = Instant::now();
        s.last_playing = playing;
        s
    }

    fn now_epoch() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs_f64()
    }

    #[tokio::test]
    async fn jyutping_renders_as_a_standalone_line() {
        let pb = make_pb(5.0, true);
        let mut s = primed(&pb, 5.0, 5.0, true);
        s.cached_track = Some(pb.track_id());
        s.backend = Box::new(FakeBackend(Some(pb)));
        s.cached_song = Some(CachedSong {
            lines: vec![
                (0.0, "上一句".into()),
                (4.0, "我会与你".into()),
                (9.0, "下一句".into()),
            ],
            roma_lines: vec![
                None,
                Some(crate::source::RomaLineCache {
                    start: 4.0,
                    end: 9.0,
                    words: vec![
                        ("我".into(), "o".into()),
                        ("会".into(), "wui".into()),
                        ("与".into(), "jyu".into()),
                        ("你".into(), "nei".into()),
                    ],
                }),
                None,
            ],
            roma_checked: true,
            ..Default::default()
        });

        let frame = s.render().await;
        let Block::Lines { lines } = &frame.blocks[0] else {
            panic!("first block should be lines");
        };
        assert_eq!(lines.len(), 4, "prev + current + jyutping + next");
        assert_eq!(lines[1].text, "我会与你");
        assert_eq!(lines[1].emphasis, Emphasis::Primary);
        assert_eq!(lines[2].text, "o wui jyu nei");
        assert_eq!(lines[2].emphasis, Emphasis::Reading);
        assert_eq!(lines[3].text, "下一句");
    }

    #[tokio::test]
    async fn seek_to_zero_reanchors() {
        let pb = make_pb(0.0, true);
        let mut s = primed(&pb, 100.0, 100.0, true);
        s.update_clock(&pb);
        assert!(s.position_base.abs() < 1e-6, "seek to 0 must re-anchor");
    }

    #[tokio::test]
    async fn seek_while_paused_reanchors() {
        // 重设锚点不受 `pb.playing` 限制：暂停时拖到 0 也要重设，否则恢复播放
        // 后进度错误。
        let pb = make_pb(0.0, false);
        let mut s = primed(&pb, 100.0, 100.0, false);
        s.update_clock(&pb);
        assert!(
            s.position_base.abs() < 1e-6,
            "paused seek to 0 must re-anchor"
        );
    }

    #[tokio::test]
    async fn frozen_report_never_rewinds_the_clock() {
        // QQ 音乐整首歌的 ElapsedTime 都是 0。本地时钟持续推进，上报值不变时
        // 重设锚点会把进度拨回 0，歌词在开头几行来回跳。
        let pb = make_pb(0.0, true);
        let mut s = primed(&pb, 0.0, 0.0, true);
        s.position_base_at = Instant::now() - Duration::from_secs(10);
        s.update_clock(&pb);
        assert!(
            s.position_base.abs() < 1e-6,
            "a frozen report is not a seek"
        );
        let pos = s.position(&pb);
        assert!(pos >= 9.9, "local clock must keep advancing: {pos}");

        s.position_base_at = Instant::now() - Duration::from_secs(20);
        s.update_clock(&pb);
        assert!(
            s.position_base.abs() < 1e-6,
            "still frozen after 20 seconds"
        );
    }

    #[tokio::test]
    async fn normal_integer_reports_do_not_reanchor() {
        let pb = make_pb(13.0, true);
        let mut s = primed(&pb, 12.0, 12.0, true);
        s.position_base_at = Instant::now() - Duration::from_secs(2);
        s.update_clock(&pb);
        assert!((s.position_base - 12.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn large_report_change_reanchors() {
        let pb = make_pb(12.0, true);
        let mut s = primed(&pb, 12.0, 12.0, true);
        s.position_base_at = Instant::now() - Duration::from_secs(2);
        let seek = make_pb(20.0, true);
        s.update_clock(&seek);
        assert!(
            (s.position_base - 20.0).abs() < 1e-6,
            "a real seek must re-anchor"
        );
    }

    #[tokio::test]
    async fn position_clamped_to_duration() {
        let pb = make_pb(0.0, true);
        let mut s = primed(&pb, 1000.0, 0.0, true);
        s.set_latency_ms(0.0);
        assert!(
            (s.position(&pb) - 300.0).abs() < 1e-6,
            "position must never exceed duration"
        );
    }

    #[tokio::test]
    async fn latency_configuration_takes_effect() {
        let pb_play = make_pb(0.0, true);
        let mut s = primed(&pb_play, 10.0, 0.0, true);
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
    async fn other_player_position_uses_reported_elapsed_not_local_clock() {
        let mut pb = make_pb(42.0, true);
        pb.bundle_id = Some("com.spotify.client".into());
        let mut s = primed(&pb, 0.0, 0.0, true);
        s.set_other_latency_ms(0.0);
        // 本地锚点是 0，但 Spotify 上报 42 秒，应直接采用上报值。
        assert!((s.position(&pb) - 42.0).abs() < 0.05);
    }

    #[tokio::test]
    async fn other_player_seek_needs_no_local_reanchor() {
        let mut pb = make_pb(5.0, true);
        pb.bundle_id = Some("com.spotify.client".into());
        let mut s = primed(&pb, 0.0, 0.0, true);
        s.update_clock(&pb);
        // 其它播放器不维护本地锚点，position_base 保持原值。
        assert!((s.position_base - 0.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn latency_is_separate_per_player() {
        let mut pb = make_pb(10.0, true);
        let mut s = primed(&pb, 10.0, 10.0, true);
        s.set_latency_ms(300.0);
        s.set_other_latency_ms(0.0);
        assert!((s.position(&pb) - 10.3).abs() < 0.05, "QQ uses 300ms");

        pb.bundle_id = Some("com.spotify.client".into());
        assert!((s.position(&pb) - 10.0).abs() < 0.05, "Spotify uses 0ms");
    }

    #[tokio::test]
    async fn after_last_line_shows_idle_note() {
        let pb = make_pb(100.0, true);
        let mut s = state_with(Some(pb.clone()));
        s.last_track_id = Some(pb.track_id());
        s.last_reported = 100.0;
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

    #[tokio::test]
    async fn snapshot_timestamp_aligns_when_elapsed_is_frozen() {
        let mut pb = make_pb(0.0, true);
        pb.reported_at = Some(now_epoch() - 42.0);
        let start = PluginState::aligned_position(&pb);
        assert!(
            (start - 42.0).abs() < 0.2,
            "snapshot 42s old should start at 42s: {start}"
        );
    }

    #[tokio::test]
    async fn snapshot_timestamp_wraps_at_duration_for_repeat() {
        let mut pb = make_pb(0.0, true);
        pb.duration = 190.0;
        pb.reported_at = Some(now_epoch() - 220.0);
        let start = PluginState::aligned_position(&pb);
        assert!(
            (start - 30.0).abs() < 0.2,
            "single-track repeat wraps at duration: {start}"
        );
    }

    #[tokio::test]
    async fn real_elapsed_is_combined_with_snapshot_age() {
        let mut pb = make_pb(75.0, true);
        pb.reported_at = Some(now_epoch() - 200.0);
        let pos = PluginState::aligned_position(&pb);
        assert!(
            (pos - 275.0).abs() < 0.2,
            "elapsed 75 plus 200s snapshot age = 275s: {pos}"
        );
    }

    #[tokio::test]
    async fn paused_track_does_not_use_snapshot_timestamp() {
        let mut pb = make_pb(0.0, false);
        pb.reported_at = Some(now_epoch() - 42.0);
        assert!(PluginState::aligned_position(&pb).abs() < 1e-6);
    }
}
