//! Reads macOS "Now Playing" information: title, artist, elapsed time,
//! duration and the authoritative play/pause flag.
//!
//! Two backends:
//! * `MediaRemoteBackend` — a reserved native slot, disabled in v1. The
//!   private framework's C functions return empty data in a normal process;
//!   see `mediaremote.rs` for the measured result.
//! * `CliBackend` — the backend in use: prefers the MediaRemoteMini helper,
//!   which returns the full field set in one process; falls back to
//!   `nowplaying-cli get-raw` only when the helper is unavailable.

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};
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
        // Album + duration disambiguate same-title remasters / live versions
        // and a single-track loop (which must swap the cached lyrics).
        format!(
            "{}|{}|{}|{}",
            self.artist.to_lowercase(),
            self.title.to_lowercase(),
            self.album.to_lowercase(),
            self.duration,
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

/// `nowplaying-cli get-raw` output. Used only on the fallback path.
/// `PlaybackRate` is deliberately NOT parsed: QQ Music keeps it at 1 while
/// paused, so it must never be used as a play/pause flag.
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
}

/// Full field set returned by `mediaremote-mini.pl adapter_get_env` in a
/// single call: title/artist/album/duration/elapsedTime/playing/playbackRate.
/// `playing` is the only authoritative play/pause source.
#[derive(Debug, Clone, PartialEq, Default)]
struct HelperEnv {
    title: String,
    artist: String,
    album: String,
    duration: f64,
    elapsed: f64,
    playing: bool,
    playback_rate: f64,
}

/// Parse the helper's JSON. Returns `None` when the payload is not JSON or the
/// `playing` field is missing/unrecognisable, so the caller falls back to the
/// sticky value instead of guessing (never assume playback is running).
fn parse_helper_env(text: &str) -> Option<HelperEnv> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let playing = parse_playing(v.get("playing")?)?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    Some(HelperEnv {
        title: s("title"),
        artist: s("artist"),
        album: s("album"),
        duration: f("duration"),
        elapsed: f("elapsedTime"),
        playing,
        playback_rate: f("playbackRate"),
    })
}

/// Accept bool / number / string forms of the flag.
fn parse_playing(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => Some(n.as_f64().unwrap_or(0.0) != 0.0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

pub struct CliBackend {
    helper: Option<(String, String)>, // (dylib path, perl script path)
    /// Last known play/pause state. When the helper gives no answer we keep
    /// this value and never default to `true`.
    last_playing: AtomicBool,
}

impl CliBackend {
    pub fn detect() -> Self {
        Self {
            helper: find_helper(),
            last_playing: AtomicBool::new(false),
        }
    }

    /// Resolve the authoritative flag: update the sticky value when the helper
    /// reports one; otherwise keep the last known value and warn.
    fn resolve_playing(&self, helper_playing: Option<bool>) -> bool {
        match helper_playing {
            Some(p) => {
                self.last_playing.store(p, Ordering::Relaxed);
                p
            }
            None => {
                let sticky = self.last_playing.load(Ordering::Relaxed);
                warn!(
                    playing = sticky,
                    "helper gave no playing state, keeping last known value (never default true)"
                );
                sticky
            }
        }
    }

    /// One process, one instant: the helper returns the whole field set.
    fn read_helper(&self) -> Option<HelperEnv> {
        let (dylib, pl) = self.helper.as_ref()?;
        let out = run_command(
            &perl_program(),
            &[pl.as_str(), dylib.as_str(), "adapter_get_env"],
            Duration::from_secs(2),
        )?;
        if out.trim().is_empty() {
            debug!("helper returned no data");
            return None;
        }
        let env = parse_helper_env(&out);
        if env.is_none() {
            debug!("parsing helper env output failed");
        }
        env
    }
}

impl NowPlayingBackend for CliBackend {
    fn read(&self) -> Option<Playback> {
        if let Some(env) = self.read_helper() {
            let title = env.title.trim().to_string();
            let artist = env.artist.trim().to_string();
            if title.is_empty() && artist.is_empty() {
                return None;
            }
            debug!(
                playback_rate = env.playback_rate,
                elapsed = env.elapsed,
                "helper env (full field set)"
            );
            let playing = self.resolve_playing(Some(env.playing));
            return Some(Playback {
                title,
                artist,
                album: env.album,
                duration: env.duration,
                elapsed: env.elapsed,
                playing,
            });
        }

        // Helper unavailable / failed / unparsable: fall back to `get-raw`
        // for track metadata, but NEVER derive `playing` from PlaybackRate.
        let playing = self.resolve_playing(None);
        let out = run_command(
            &nowplaying_cli_program(),
            &["get-raw"],
            Duration::from_secs(2),
        )?;
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

/// `nowplaying-cli` lives in a brew prefix that is missing from the PATH of
/// an app launched from Finder. Probe the known absolute locations first.
fn nowplaying_cli_program() -> String {
    resolve_program(
        &[
            "/opt/homebrew/bin/nowplaying-cli",
            "/usr/local/bin/nowplaying-cli",
        ],
        "nowplaying-cli",
    )
}

/// system perl has a stable absolute path; prefer it over a PATH lookup.
fn perl_program() -> String {
    resolve_program(&["/usr/bin/perl"], "perl")
}

fn resolve_program(abs_candidates: &[&str], path_name: &str) -> String {
    for p in abs_candidates {
        if std::path::Path::new(p).exists() {
            return p.to_string();
        }
    }
    path_name.to_string()
}

fn find_helper() -> Option<(String, String)> {
    let roots = [
        "/opt/homebrew/Cellar/nowplaying-cli",
        "/usr/local/Cellar/nowplaying-cli",
    ];
    let mut candidates: Vec<(SystemTime, String)> = Vec::new();
    for root in roots {
        let pattern = format!("{root}/*/lib/nowplaying-cli/MediaRemoteMini.dylib");
        // One bad root (or a bad glob entry) must not abort the whole search.
        let paths = match glob::glob(&pattern) {
            Ok(p) => p,
            Err(e) => {
                debug!(root = %root, error = %e, "helper glob failed");
                continue;
            }
        };
        for entry in paths.filter_map(|e| e.ok()) {
            let mtime = std::fs::metadata(&entry)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((mtime, entry.to_string_lossy().to_string()));
        }
    }
    // Newest first (by install time), not lexicographic last(): version
    // strings like 1.9 vs 1.10 sort the wrong way lexicographically.
    candidates.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, dylib) in candidates {
        let prefix = dylib.split("/lib/nowplaying-cli/").next().unwrap_or("");
        let pl = format!("{prefix}/share/nowplaying-cli/scripts/mediaremote-mini.pl");
        if std::path::Path::new(&pl).exists() {
            return Some((dylib, pl));
        }
    }
    None
}

fn run_command(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;

    // Both `nowplaying-cli get-raw` and the helper emit a ~330 KB JSON blob
    // with artworkData. Drain stdout on a dedicated thread so the pipe never
    // fills up and deadlocks the child; send the result over a channel so the
    // caller can bound the wait with recv_timeout instead of a blocking join().
    let (out_tx, out_rx) = mpsc::channel();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.read_to_end(&mut buf);
        let _ = out_tx.send((res, buf));
    });
    let (err_tx, err_rx) = mpsc::channel();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        let _ = err_tx.send(buf);
    });

    // A hung child never closes stdout, so recv_timeout is our deadline.
    let (read_res, buf) = match out_rx.recv_timeout(timeout) {
        Ok(v) => v,
        Err(e) => {
            warn!(program = %program, error = %e, "command timed out or stdout reader disconnected");
            let _ = child.kill();
            let _ = child.wait();
            // Killing the child closes its pipes, so both readers see EOF and
            // exit instead of leaking a thread / zombie process.
            let _ = out_thread.join();
            let _ = err_thread.join();
            return None;
        }
    };

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            warn!(program = %program, error = %e, "waiting for command failed");
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            return None;
        }
    };
    let _ = out_thread.join();
    let _ = err_thread.join();

    if let Ok(err_bytes) = err_rx.try_recv() {
        let text = String::from_utf8_lossy(&err_bytes);
        if !text.trim().is_empty() {
            debug!(program = %program, stderr = %text.trim(), "command stderr");
        }
    }

    if let Err(e) = read_res {
        warn!(program = %program, error = %e, "reading command stdout failed");
        return None;
    }
    if !status.success() {
        warn!(program = %program, status = ?status.code(), "command exited with error");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Playback {
        Playback {
            artist: "Beyond".into(),
            title: "海阔天空".into(),
            album: "乐与怒".into(),
            duration: 326.0,
            elapsed: 0.0,
            playing: true,
        }
    }

    #[test]
    fn track_id_includes_album_and_duration() {
        let a = base();
        let mut b = a.clone();
        b.album = "Live 1996".into();
        assert_ne!(a.track_id(), b.track_id(), "different album must change id");
        let mut c = a.clone();
        c.duration = 300.0;
        assert_ne!(
            a.track_id(),
            c.track_id(),
            "different duration must change id"
        );
        let same = a.clone();
        assert_eq!(a.track_id(), same.track_id());
    }

    #[test]
    fn helper_env_full_field_mapping() {
        let json = r#"{
            "title":"海阔天空","artist":"Beyond","album":"乐与怒",
            "duration":326.0,"elapsedTime":12.5,
            "playing":true,"playbackRate":1.0
        }"#;
        let env = parse_helper_env(json).expect("valid helper env");
        assert_eq!(env.title, "海阔天空");
        assert_eq!(env.artist, "Beyond");
        assert_eq!(env.album, "乐与怒");
        assert!((env.duration - 326.0).abs() < 1e-6);
        assert!((env.elapsed - 12.5).abs() < 1e-6, "elapsedTime -> elapsed");
        assert!(
            (env.playback_rate - 1.0).abs() < 1e-6,
            "playbackRate -> playback_rate"
        );
        assert!(env.playing);
    }

    #[test]
    fn helper_env_accepts_flag_variants() {
        let mk = |p: &str| format!(r#"{{"title":"x","playing":{}}}"#, p);
        assert!(parse_helper_env(&mk("true")).unwrap().playing);
        assert!(!parse_helper_env(&mk("false")).unwrap().playing);
        assert!(parse_helper_env(&mk("1")).unwrap().playing);
        assert!(!parse_helper_env(&mk("0")).unwrap().playing);
        assert!(parse_helper_env(&mk("\"true\"")).unwrap().playing);
    }

    #[test]
    fn helper_env_missing_playing_is_rejected() {
        assert!(parse_helper_env(r#"{"title":"x","elapsedTime":1.0}"#).is_none());
        assert!(parse_helper_env("not json").is_none());
    }

    #[test]
    fn playing_is_sticky_when_helper_missing() {
        let backend = CliBackend {
            helper: None,
            last_playing: AtomicBool::new(true),
        };
        // helper gives no answer -> keep last known value, never default true.
        assert!(backend.resolve_playing(None));
        assert!(backend.last_playing.load(Ordering::Relaxed));

        // helper reports paused -> updates the sticky value.
        assert!(!backend.resolve_playing(Some(false)));
        // next miss must stay paused, not flip back to playing.
        assert!(!backend.resolve_playing(None));
        assert!(!backend.last_playing.load(Ordering::Relaxed));
    }
}
