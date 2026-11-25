# nowplaying-cli
nowplaying-cli is a macOS command-line utility for retrieving currently playing media, and simulating media actions.

Use nowplaying-cli to get song information and play/pause your media through an easy to use CLI!

**Disclaimer:** nowplaying-cli uses private frameworks, which may cause it to break with future macOS software updates.

**Tested and working on:** 
- Ventura 13.1, 13.2, 13.3, 13.6
- Sonoma 14.4
- Sequoia 15.7
- Tahoe 26.3

## Installation

### Homebrew

You can install nowplaying-cli using [Homebrew](https://brew.sh/).
```bash
brew install nowplaying-cli
```

### Build from source

Clone the repository and run `make` to build the binary. If you want to install it system-wide, use `make install` so the helper script and dylib are installed alongside it.
```bash
sudo make install
```

## Usage
`nowplaying-cli <cmd>`
| command | description |
| --- | --- |
| get [propName1 propName2 ... ] | get now playing media properties | 
| get-raw | get all non-nil now playing media properties in dictionary format |
| play | play the currently playing media regardless of current state |
| pause | pause the currently playing media regardless of current state |
| togglePlayPause | toggle play/pause based on current state |
| seek <seconds> | seek to a specific time in the currently playing media |
| next | skip to next track | 
| previous | go to previous track |

## Examples
![screenshot of examples in terminal](screenshots/examples.png)

## Available properties
| native  |  nowplaying-cli |
|---|---|
| kMRMediaRemoteNowPlayingInfoTotalDiscCount | totalDiscCount |
|  kMRMediaRemoteNowPlayingInfoShuffleMode | shuffleMode
|  kMRMediaRemoteNowPlayingInfoTrackNumber | trackNumber
|  kMRMediaRemoteNowPlayingInfoDuration | duration
|  kMRMediaRemoteNowPlayingInfoRepeatMode | repeatMode
|  kMRMediaRemoteNowPlayingInfoTitle | title
|  kMRMediaRemoteNowPlayingInfoPlaybackRate | playbackRate | 
|  kMRMediaRemoteNowPlayingInfoArtworkData | artworkData |
|  kMRMediaRemoteNowPlayingInfoArtworkDataWidth | artworkDataWidth |
|  kMRMediaRemoteNowPlayingInfoArtworkDataHeight | artworkDataHeight |
|  kMRMediaRemoteNowPlayingInfoAlbum | album |
|  kMRMediaRemoteNowPlayingInfoTotalQueueCount | totalQueueCount | 
|  kMRMediaRemoteNowPlayingInfoArtworkMIMEType | artworkMIMEType
|  kMRMediaRemoteNowPlayingInfoMediaType | mediaType |
|  kMRMediaRemoteNowPlayingInfoDiscNumber | discNumber |
|  kMRMediaRemoteNowPlayingInfoTimestamp | timestamp |
|  kMRMediaRemoteNowPlayingInfoGenre | genre |
|  kMRMediaRemoteNowPlayingInfoQueueIndex | queueIndex |
|  kMRMediaRemoteNowPlayingInfoArtist | artist |
|  kMRMediaRemoteNowPlayingInfoDefaultPlaybackRate | defaultPlaybackRate |
|  kMRMediaRemoteNowPlayingInfoElapsedTime | elapsedTime |
|  kMRMediaRemoteNowPlayingInfoTotalTrackCount | totalTrackCount |
|  kMRMediaRemoteNowPlayingInfoIsMusicApp | isMusicApp |
|  kMRMediaRemoteNowPlayingInfoUniqueIdentifier | uniqueIdentifier |

## Acknowledgments

Thank you [@ungive](https://github.com/ungive) for their [mediaremote-adapter](https://github.com/ungive/mediaremote-adapter) project, which restores support for macOS 15, 26. The files under `src/mediaremote-mini/` are derived from that project and are licensed under the BSD 3-Clause License.

---

## InkBuddy 随附说明

InkBuddy 的 macOS app 随包分发本项目的两个文件，供歌词插件读取 Now Playing 信息：

| 文件 | SHA-256 |
| --- | --- |
| `MediaRemoteMini.dylib` | `5c0ca30f89af625f13c0315723be9535831197e4f74e7ab39889446def748ea2` |
| `mediaremote-mini.pl` | `0a3276dcbf2bddbbca669be0ef00d0dc8365aca745abc273f2bea63f5af03843` |

- 组件版本：nowplaying-cli 2.1.0（Homebrew `homebrew/core` stable）。两个文件与该版本安装包中的原文件一致。
- 上游源码：https://github.com/kirtan-shah/nowplaying-cli 。`src/mediaremote-mini/` 下的实现源自 https://github.com/ungive/mediaremote-adapter 。对应源码可从上游仓库按该版本取得。
- 许可证：GNU GPL-3.0，全文见同目录 `LICENSE`；其中 `src/mediaremote-mini/` 派生部分按 BSD 3-Clause，许可文本附在 `LICENSE` 末尾。
- app 包内的位置：两个文件在 `Contents/Resources/plugins/lyrics/`，许可证与本文档在 `Contents/Resources/licenses/nowplaying-cli/`。仓库副本在 `vendor/nowplaying-cli/`。
