//! Reads macOS "Now Playing" information: title, artist, elapsed time,
//! duration and the authoritative play/pause flag.
//!
//! Two backends:
//! * `MediaRemoteBackend` — a reserved native slot, disabled in v1. The
//!   private framework's C functions return empty data in a normal process;
//!   see `mediaremote.rs` for the measured result.
//! * `CliBackend` — the backend in use: shells out to `nowplaying-cli` and
//!   its bundled MediaRemoteMini helper. Requires `nowplaying-cli` installed.

use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, warn};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Playback {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    /// Progress reported by the player at this instant (may be a frozen
    /// snapshot; the clock in `state.rs` advances it locally).
    pub elapsed: f64,
    pub playing: bool,
}

impl Playback {
    pub fn track_id(&self) -> String {
        format!(
            "{}|{}",
            self.artist.to_lowercase(),
            self.title.to_lowercase()
        )
    }

    pub fn has_track(&self) -> bool {
        !self.title.is_empty() || !self.artist.is_empty()
    }
}

pub trait NowPlayingBackend: Send + Sync {
    fn read(&self) -> Option<Playback>;
}

// ---------------------------------------------------------------------------
// Fallback: nowplaying-cli + its MediaRemoteMini helper
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct RawNowPlaying {
    #[serde(rename = "kMRMediaRemoteNowPlayingInfoTitle")]
    title: Option<String>,
    #[serde(rename = "kMRMediaRemoteNowPlayingInfoArtist")]
    artist: Option<String>,
    #[serde(rename = "kMRMediaRemoteNowPlayingInfoAlbum")]
    album: Option<String>,
    #[serde(rename = "kMRMediaRemoteNowPlayingInfoDuration")]
    duration: Option<f64>,
    #[serde(rename = "kMRMediaRemoteNowPlayingInfoElapsedTime")]
    elapsed: Option<f64>,
    #[serde(rename = "kMRMediaRemoteNowPlayingInfoPlaybackRate")]
    playback_rate: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct HelperEnv {
    playing: Option<bool>,
}

pub struct CliBackend {
    helper: Option<(String, String)>, // (dylib path, perl script path)
}

impl CliBackend {
    pub fn detect() -> Self {
        Self {
            helper: find_helper(),
        }
    }

    fn helper_playing(&self) -> Option<bool> {
        let (dylib, pl) = self.helper.as_ref()?;
        let out = run_command(
            "perl",
            &[pl.as_str(), dylib.as_str(), "adapter_get_env"],
            Duration::from_secs(2),
        )?;
        serde_json::from_str::<HelperEnv>(&out)
            .ok()
            .and_then(|v| v.playing)
    }
}

impl NowPlayingBackend for CliBackend {
    fn read(&self) -> Option<Playback> {
        let out = run_command("nowplaying-cli", &["get-raw"], Duration::from_secs(2))?;
        let raw: RawNowPlaying = match serde_json::from_str(&out) {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "parsing get-raw output failed");
                return None;
            }
        };
        let title = raw.title.unwrap_or_default().trim().to_string();
        let artist = raw.artist.unwrap_or_default().trim().to_string();
        if title.is_empty() && artist.is_empty() {
            return None;
        }

        // PlaybackRate stays at 1 while QQ Music is paused, so the helper's
        // app-level flag wins whenever it is available.
        let mut playing = raw.playback_rate.unwrap_or(0.0) > 0.0;
        if let Some(p) = self.helper_playing() {
            playing = p;
        }

        Some(Playback {
            title,
            artist,
            album: raw.album.unwrap_or_default(),
            duration: raw.duration.unwrap_or(0.0),
            elapsed: raw.elapsed.unwrap_or(0.0),
            playing,
        })
    }
}

fn find_helper() -> Option<(String, String)> {
    let roots = [
        "/opt/homebrew/Cellar/nowplaying-cli",
        "/usr/local/Cellar/nowplaying-cli",
    ];
    for root in roots {
        let pattern = format!("{root}/*/lib/nowplaying-cli/MediaRemoteMini.dylib");
        let mut found: Vec<String> = glob::glob(&pattern)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        found.sort();
        if let Some(dylib) = found.last() {
            let prefix = dylib.split("/lib/nowplaying-cli/").next().unwrap_or("");
            let pl = format!("{prefix}/share/nowplaying-cli/scripts/mediaremote-mini.pl");
            if std::path::Path::new(&pl).exists() {
                return Some((dylib.clone(), pl));
            }
        }
    }
    None
}

fn run_command(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    // `timeout` is a GNU tool not present on macOS by default, so we implement
    // the deadline with a child + poll loop instead of an external binary.
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Both `nowplaying-cli get-raw` and the helper emit a ~330 KB JSON blob
    // with artworkData. A poll loop that never reads stdout lets the pipe
    // buffer fill up, so the child blocks on write and never exits: every
    // call would hit the timeout. Drain stdout on a separate thread while
    // the main thread watches the deadline.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    warn!(program, "command timed out");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    };
    let buf = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// Pick the backend for this version. The native MediaRemote interface is a
/// reserved slot that is disabled in v1, so the CLI backend is what runs.
pub fn create_backend() -> Box<dyn NowPlayingBackend> {
    match crate::mediaremote::MediaRemoteBackend::new() {
        Ok(b) => {
            debug!("using native MediaRemote backend");
            Box::new(b)
        }
        Err(e) => {
            debug!(error = %e, "native MediaRemote disabled, using nowplaying-cli");
            Box::new(CliBackend::detect())
        }
    }
}
