# 歌词插件（inkbuddy-plugin-lyrics）

InkBuddy 的核心插件：读取 macOS 正在播放的曲目，输出同步歌词；粤语歌在当前句下方单独显示一行粤拼。

插件 id 为 `lyrics`，安装后的清单在 `plugins/lyrics/plugin.toml`。

## 输出内容

- 三行歌词：上一句、当前句、下一句，当前句字号最大。
- 粤语歌在当前句下方单独显示一行粤拼（`emphasis: reading`）。粤拼行和中文行各自居中，不做逐字对齐。
- 当前曲目结束后显示休止符。

## 播放器策略

播放器身份取 MediaRemote 的 bundle id（`bundleIdentifier` / `kMRMediaRemoteNowPlayingInfoClientBundleIdentifier`），按包含匹配识别。识别结果决定歌词来源和进度算法：

| 播放器 | bundle id 关键字 | 歌词来源 | 进度算法 | 提前量环境变量 |
| --- | --- | --- | --- | --- |
| QQ 音乐 | `qqmusic` | QQ 曲库优先，LRCLIB 兜底 | 本地时钟 | `INKBUDDY_LATENCY_MS` |
| 网易云 | `cloudmusic` / `netease` / `163music` | 网易云优先，QQ 曲库、LRCLIB 依次兜底 | 本地时钟 | `INKBUDDY_LATENCY_OTHER_MS` |
| Spotify | `spotify` | QQ 与 LRCLIB 并发 | 用上报进度 | `INKBUDDY_LATENCY_OTHER_MS` |
| 其它 | — | QQ 与 LRCLIB 并发 | 用上报进度 | `INKBUDDY_LATENCY_OTHER_MS` |

网易云走自己的官方曲库，歌词与桌面客户端一致：先搜索匹配歌曲，再取 LRC 歌词；匹配分数过低（伴奏、翻唱、现场版）时依次退到 QQ 曲库、LRCLIB。网易云返回的歌词没有粤拼，兜底到 QQ 时也不请求粤拼，让取词尽快返回。并发策略（Spotify 与其它）下 QQ 与 LRCLIB 同时发请求，先用先返回的有效歌词；QQ 这一路跳过粤拼解密，让两边耗时只取决于歌词本身。

新增播放器在 `src/player.rs` 里加 bundle id 映射。

## 进度算法

QQ 音乐 Mac 把 `ElapsedTime` 一直冻结在 0，网易云 Mac 把它冻结在切歌时刻（实测 `elapsedTime` 与 `timestamp` 都不再更新），helper 和 `get-raw` 都读不到真实进度。这两家用本地时钟：上报值不变就自行推进，播放器拖动进度时上报值变化，歌词随之对齐。

helper 返回的 `timestamp` 是这条播放信息的发布时间，`ElapsedTime` 是那个时刻的进度。QQ 音乐与网易云切歌或进度值变化时，用 `ElapsedTime + (当前时间 - timestamp)` 算出实际位置，并按歌曲时长取模，单曲循环时也能对上。这个校正只在切歌和进度值变化时做一次，之后完全靠本地时钟推进。设 `INKBUDDY_TIMESTAMP_ANCHOR=0` 可以关掉，只用上报值。

其它播放器（Spotify 等）上报的 `ElapsedTime` 可用，进度直接取它。

提前量单位为毫秒，正值让歌词提前出现，抵消墨水屏刷新延迟；暂停时不叠加。默认 `0`，也可通过 `configure` 请求调整：`latency_ms` 改 QQ 的值，`other_latency_ms` 改其它播放器的值。

## 歌词来源与缓存

歌词来自网易云、QQ 曲库和 LRCLIB，结果缓存到 `~/.inkbuddy/cache/lyrics/*.json`（跟随 `INKBUDDY_HOME`）。缓存文件里 `lines` 是 LRC 行；`roma_lines` 与 `lines` 等长、按行索引绑定粤拼（没有对应读音的行为空）；`roma_checked` 表示这首歌的粤拼已确认，读取时直接按行索引取，命中缓存不发网络请求。策略名称进入缓存文件名，QQ 的简体+粤拼结果、网易云结果和并发策略的结果不会互相覆盖。

网易云的取词用两个公开接口：`POST/GET https://music.163.com/api/cloudsearch/pc`（关键词 `歌手 歌曲`，取 `result.songs` 里分数最高的一条）和 `GET https://music.163.com/api/song/lyric?id=<id>&lv=1&kv=1&tv=-1`（取 `lrc.lyric`，标准 LRC 文本，复用同一套解析）。搜索结果里歌手在 `ar` 数组、专辑在 `al.name`、时长 `dt` 是毫秒，打分时按歌名、歌手、专辑、时长（3 秒内）加权。

QQ 的逐字粤拼来自 QRC 接口，返回的是 hex + 自定义 3DES + zlib 加密数据。`src/qrc.rs` 用 Rust 实现了这个解密与读音对齐，DES 表来自公开的 FIPS 46-3 标准。

## 播放信息读取

播放信息通过 `nowplaying-cli` 的 helper 读取。构建 app 时把 `MediaRemoteMini.dylib` 和 `mediaremote-mini.pl` 放进插件目录（`vendor/nowplaying-cli/` 是随仓库保存的副本，含 GPL-3.0 许可证），随 app 一起分发，用户无需安装 Homebrew 或任何外部软件。

- 优先用插件自身目录里的 helper：`mediaremote-mini.pl adapter_get_env` 一次取全量字段（标题、歌手、专辑、时长、进度、`playing`、`playbackRate`、`timestamp`、`bundleIdentifier`）。QQ 音乐暂停时 `PlaybackRate` 仍可能是 1，播放状态取 helper 给的 `playing`；helper 偶发取不到时沿用上一次的值。
- 开发机上直接跑插件二进制时，若自身目录没有 helper，会回退到 Homebrew 的 `/opt/homebrew/Cellar/nowplaying-cli`、`/usr/local/Cellar/nowplaying-cli`，再回退到 `nowplaying-cli get-raw` 读原始字段 `kMRMediaRemoteNowPlayingInfoElapsedTime`。`get-raw` 没有 `timestamp`，这条路径直接从上报值开始本地计时。

## 源码结构

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | 插件入口，协议方法与 `configure` 参数 |
| `src/nowplaying.rs` | 播放信息后端：helper 与 `get-raw` 两条读取路径 |
| `src/player.rs` | 播放器识别、歌词策略与进度时钟的映射 |
| `src/state.rs` | 播放时钟、歌词窗口状态、提前量 |
| `src/source.rs` | 歌词来源、并发取词、磁盘缓存、粤拼按行绑定 |
| `src/qrc.rs` | QRC 粤拼解密与读音对齐 |

## 参考

- [FIPS 46-3: Data Encryption Standard](https://csrc.nist.gov/pubs/fips/46-3/final)
