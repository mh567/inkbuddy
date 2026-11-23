# InkBuddy

把闲置的墨水屏阅读器变成 macOS 的副屏。

墨水屏保持原样：在自带浏览器里输入一个局域网地址，就进入展示页面。内容由 macOS 上的 InkBuddy 主程序生成。

核心功能插件是**同步歌词**（粤语歌带粤拼）。

## 工作方式

```
墨水屏浏览器  ──HTTP──>  InkBuddy 主程序  ──JSON/stdio──>  插件进程（可多个）
 http://ip:9012/          Rust + axum               独立可执行文件
                              │
                              └─ 统一渲染成墨水屏页面
```

插件返回结构化内容（文本行、键值对、段落），主程序负责排版。选择单个插件或把几个插件组合显示时，样式始终一致，插件之间互不依赖。

## 快速开始

```bash
# 需要 Rust 工具链（rustup）
./scripts/install.sh
```

脚本会编译 workspace、把歌词插件装到 `~/.inkbuddy/plugins/lyrics/`、写一份默认配置。然后启动：

```bash
./target/release/inkbuddy
```

启动时会打印推荐输入的局域网地址（有备选地址时一并列在后面）。墨水屏浏览器打开：

```
http://<mac-局域网-IP>:9012/
```

根路径就是展示页，直接输入服务器地址即可。其他入口：

| 地址 | 用途 |
| --- | --- |
| `/` 或 `/ink` | 墨水屏展示页 |
| `/manage` | 勾选插件、生成带组合参数的地址 |
| `/api/frame` | 聚合后的展示内容（JSON） |
| `/api/plugins` | 已安装插件及运行状态 |
| `/api/health` | 健康检查 |

选择单个或组合显示，直接在 URL 上给参数，顺序即页面从上到下的顺序：

```
http://<ip>:9012/?plugins=lyrics            # 只显示歌词
http://<ip>:9012/?plugins=clock             # 只显示时钟
http://<ip>:9012/?plugins=lyrics,clock      # 组合，歌词在上、时钟在下
```

不带参数时显示所有 `default_on = true` 的插件（默认只有歌词）。选择放在 URL 里，浏览器无需保存状态。

插件目录在运行时会被重新扫描，新放进目录的插件不用重启主程序就能生效。

## 配置

`~/.inkbuddy/config.toml`（首次可用 `inkbuddy --init-config` 生成）：

```toml
[server]
bind = "0.0.0.0"
port = 9012

[display]
title = "InkBuddy"
refresh_ms = 500
```

插件放在 `~/.inkbuddy/plugins/<目录>/plugin.toml`，启动时自动扫描。也可以：

```bash
inkbuddy --plugins-dir ./my-plugins --port 8080
inkbuddy --list-plugins
```

`INKBUDDY_HOME` 可改掉整个运行时目录（配置、插件、歌词缓存都跟着走）。

### macOS app

`./scripts/build-app.sh` 会生成 `dist/InkBuddy.app`。它是后台运行、没有 Dock 图标的 app 包，双击即启动服务；墨水屏仍然只需打开浏览器地址。插件选择通过 `/manage` 页面完成。

从 Finder 双击启动时，进程拿到的 `PATH` 通常不含 Homebrew 的 `/opt/homebrew/bin`，会让按 `PATH` 查找 `nowplaying-cli` 失败。app 包在 `Info.plist` 里通过 `LSEnvironment` 显式设置了包含 brew 前缀的 `PATH`；歌词插件本身也会先试 `/opt/homebrew/bin/nowplaying-cli`、`/usr/local/bin/nowplaying-cli` 等绝对路径再回退 `PATH`，两层保障。

停止：`pkill -f InkBuddy.app/Contents/MacOS/inkbuddy`。

## 歌词插件

`crates/inkbuddy-plugin-lyrics` 是第一个自带插件：

- 读取 macOS 正在播放的曲目、播放器、进度和播放/暂停状态
- 按播放器选择歌词来源、进度算法和提前量，见下表
- 三行聚焦视图：上一句 / 当前句 / 下一句，当前句最大
- 粤语歌在当前句下方单独显示一行粤拼，和中文行各自居中排版
- 歌词和粤拼缓存到 `~/.inkbuddy/cache/lyrics/*.json`，按策略分开存

播放器身份取 MediaRemote 的 bundle id（`bundleIdentifier` / `kMRMediaRemoteNowPlayingInfoClientBundleIdentifier`），映射如下：

| 播放器 | bundle id 关键字 | 歌词来源 | 进度算法 | 提前量环境变量 |
| --- | --- | --- | --- | --- |
| QQ 音乐 | `qqmusic` | QQ 曲库优先，LRCLIB 兜底 | 本地时钟 | `INKBUDDY_LATENCY_MS` |
| 网易云 | `cloudmusic` / `netease` | QQ 与 LRCLIB 并发 | 用上报进度 | `INKBUDDY_LATENCY_OTHER_MS` |
| Spotify | `spotify` | QQ 与 LRCLIB 并发 | 用上报进度 | `INKBUDDY_LATENCY_OTHER_MS` |
| 其它 | — | QQ 与 LRCLIB 并发 | 用上报进度 | `INKBUDDY_LATENCY_OTHER_MS` |

网易云和 Spotify 目前与其它播放器共用同一套策略，各自的官方音源后续接入 `src/player.rs` 和 `src/source.rs`。并发策略下两个来源同时发请求，先用先返回的有效歌词；QQ 这一路跳过粤拼解密，让两边耗时只取决于歌词本身。

提前量单位为毫秒，正值让歌词提前出现，抵消墨水屏刷新延迟；暂停时不叠加。默认都是 `0.0` 秒，也可通过 `configure` 请求调整：`latency_ms` 改 QQ 的值，`other_latency_ms` 改其它播放器的值。

QQ 音乐 Mac 把 `ElapsedTime` 一直冻结在 0，helper 和 `get-raw` 都读不到真实进度。本地时钟按「上报值是否变化」工作：值不变就自行推进，播放器拖动进度时上报值变化，歌词随之对齐。其它播放器上报值可用，进度直接取 `ElapsedTime`。

helper 返回的 `timestamp` 是这条播放信息的发布时间，`ElapsedTime` 是那个时刻的进度。QQ 音乐切歌或进度值变化时，用 `ElapsedTime + (当前时间 - timestamp)` 算出实际位置，并按歌曲时长取模，单曲循环时也能对上。这个校正只在切歌和进度值变化时做一次，之后完全靠本地时钟推进。设 `INKBUDDY_TIMESTAMP_ANCHOR=0` 可以关掉，只用上报值。

缓存文件里 `lines` 是 LRC 行；`roma_lines` 与 `lines` 等长、按行索引绑定粤拼（没有对应读音的行为空）。`roma_checked` 表示这首歌的粤拼已确认，读取时直接按行索引取，命中缓存不发网络请求。策略名称进入缓存文件名，QQ 的简体+粤拼结果与并发策略的结果不会互相覆盖。

播放信息通过 `nowplaying-cli` 读取，需要本机装过（`brew install nowplaying-cli`）：

- 优先用 `mediaremote-mini.pl adapter_get_env` 一次取全量字段（标题、歌手、专辑、时长、进度、`playing`、`playbackRate`、`timestamp`、`bundleIdentifier`）。QQ 音乐暂停时 `PlaybackRate` 仍可能是 1，播放状态取 helper 给的 `playing`；helper 偶发取不到时沿用上一次的值。
- helper 不可用或解析失败时回退到 `nowplaying-cli get-raw`，读原始字段 `kMRMediaRemoteNowPlayingInfoElapsedTime`。`get-raw` 没有 `timestamp`，这条路径直接从上报值开始本地计时。

QQ 的逐字粤拼来自 QRC 接口，返回的是 hex + 自定义 3DES + zlib 加密数据。`crates/inkbuddy-plugin-lyrics/src/qrc.rs` 用 Rust 实现了这个解密与读音对齐，DES 表来自公开的 FIPS 46-3 标准。

## 写自己的插件

插件是独立可执行程序，用 stdin/stdout 上的行分隔 JSON 和主程序通信，可以用任何语言写，插件进程崩溃后下一次请求会自动拉起。完整协议、内容模型、Python 最小示例见 [docs/plugin-dev.md](docs/plugin-dev.md)。

Rust 插件可直接依赖 `inkbuddy-protocol` crate 拿到全部类型定义。

## workspace 结构

| 路径 | 说明 |
| --- | --- |
| `crates/inkbuddy-protocol` | 共享协议与内容模型，插件作者可依赖 |
| `crates/inkbuddy-host` | 主程序：插件进程管理、HTTP 服务、页面渲染 |
| `crates/inkbuddy-plugin-lyrics` | 歌词插件（核心功能） |
| `crates/inkbuddy-plugin-clock` | 时钟插件，演示组合与最小插件写法 |
| `web/ink.html` | 墨水屏展示页（编译进主程序） |
| `plugins/*/plugin.toml` | 开发用的插件清单 |
| `scripts/install.sh` | 编译并安装 |
| `scripts/build-app.sh` | 打包成 InkBuddy.app |
| `docs/plugin-dev.md` | 插件开发文档 |

## 说明

- HTTP 服务默认监听 `0.0.0.0`，不带认证，同一局域网里的任何设备都能读取展示内容、查看插件列表并切换显示组合。只在可信局域网使用；需要隔离时把 `bind` 改成 `127.0.0.1`，再自行做转发或加一层鉴权代理。
- 墨水屏刷新慢且有残影，页面只在内容真正变化时才改 DOM。
- 墨水屏浏览器若缓存了旧页面，在地址后加 `?v=2` 破一次缓存；页面本身已返回 `no-store`。

## 许可

各 crate 为 MIT。QQ QRC 解密涉及的 DES 算法与 S-box 数值来自公开的 FIPS 46-3 标准。
