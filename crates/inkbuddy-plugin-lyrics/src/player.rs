//! 按播放器选择歌词来源、进度时钟和提前量。
//!
//! 播放器身份来自 MediaRemote 的 bundle id。识别结果决定三件事：歌词怎么取、
//! 进度怎么算、提前量用哪个配置。新增播放器时在这里加映射即可。

/// 识别到的播放器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerKind {
    QqMusic,
    Netease,
    Spotify,
    Other,
}

/// 歌词来源策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricStrategy {
    /// QQ 曲库优先，取不到再用 LRCLIB。QQ 音乐专用。
    QqFirst,
    /// QQ 曲库与 LRCLIB 同时发请求，先用先返回的有效结果。
    Race,
}

/// 进度时钟策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMode {
    /// 上报的 ElapsedTime 长期冻结：变化时重设锚点，其余时间本地推进。
    FrozenLocal,
    /// 直接用播放器上报的 ElapsedTime。
    ReportElapsed,
}

impl PlayerKind {
    /// 从 bundle id 识别播放器；缺失或未知时归为 Other。
    /// 用小写包含匹配，兼容同一播放器不同版本的 id 写法。
    pub fn from_bundle_id(id: Option<&str>) -> Self {
        let Some(id) = id else {
            return Self::Other;
        };
        let lower = id.to_ascii_lowercase();
        if lower.contains("qqmusic") {
            Self::QqMusic
        } else if lower.contains("cloudmusic") || lower.contains("netease") {
            Self::Netease
        } else if lower.contains("spotify") {
            Self::Spotify
        } else {
            Self::Other
        }
    }

    pub fn lyric_strategy(self) -> LyricStrategy {
        match self {
            Self::QqMusic => LyricStrategy::QqFirst,
            _ => LyricStrategy::Race,
        }
    }

    pub fn clock_mode(self) -> ClockMode {
        match self {
            Self::QqMusic => ClockMode::FrozenLocal,
            _ => ClockMode::ReportElapsed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Spotify => "spotify",
            Self::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_known_players() {
        assert_eq!(
            PlayerKind::from_bundle_id(Some("com.tencent.QQMusicMac")),
            PlayerKind::QqMusic
        );
        assert_eq!(
            PlayerKind::from_bundle_id(Some("com.netease.cloudmusic")),
            PlayerKind::Netease
        );
        assert_eq!(
            PlayerKind::from_bundle_id(Some("com.spotify.client")),
            PlayerKind::Spotify
        );
    }

    #[test]
    fn unknown_and_missing_ids_fall_back_to_other() {
        assert_eq!(PlayerKind::from_bundle_id(None), PlayerKind::Other);
        assert_eq!(PlayerKind::from_bundle_id(Some("")), PlayerKind::Other);
        assert_eq!(
            PlayerKind::from_bundle_id(Some("com.apple.Music")),
            PlayerKind::Other
        );
    }

    #[test]
    fn qq_keeps_current_strategies_others_use_race() {
        let qq = PlayerKind::QqMusic;
        assert_eq!(qq.lyric_strategy(), LyricStrategy::QqFirst);
        assert_eq!(qq.clock_mode(), ClockMode::FrozenLocal);

        for kind in [PlayerKind::Netease, PlayerKind::Spotify, PlayerKind::Other] {
            assert_eq!(kind.lyric_strategy(), LyricStrategy::Race);
            assert_eq!(kind.clock_mode(), ClockMode::ReportElapsed);
        }
    }

}
