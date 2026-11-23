//! Reserved interface for a native macOS Now Playing reader.
//!
//! First version does not enable it. `new()` returns an error and
//! nowplaying.rs falls back to the CLI backend.
//!
//! Measured on macOS 27.2.1 arm64: the private MediaRemote C functions load
//! via dlopen/dlsym, the async callback fires, but
//! MRMediaRemoteGetNowPlayingInfo hands back a nil/empty CFDictionary in a
//! normal process and MRMediaRemoteGetNowPlayingApplicationIsPlaying returns
//! false. An Objective-C probe gets the same empty result while
//! nowplaying-cli reads real track data at the same moment. The framework
//! appears to require an XPC subscription identity that mediaremoted does
//! not grant to ordinary processes. nowplaying-cli avoids these C functions
//! entirely: it loads MediaRemoteMini.dylib and uses MRNowPlayingController.
//! Keep this as an extension point rather than shipping an empty reader.

use crate::nowplaying::{NowPlayingBackend, Playback};
use anyhow::{anyhow, Result};

pub struct MediaRemoteBackend;

impl MediaRemoteBackend {
    pub fn new() -> Result<Self> {
        Err(anyhow!("native MediaRemote backend disabled in v1"))
    }
}

impl NowPlayingBackend for MediaRemoteBackend {
    fn read(&self) -> Option<Playback> {
        None
    }
}
