# InkBuddy

把闲置的墨水屏阅读器变成 macOS 的副屏。

第一版采用无侵入设计：不改造设备、不装客户端、不越狱。在墨水屏自带的浏览器里输入一个局域网地址，就进入被控制的展示页面。内容由 macOS 上的 InkBuddy 主程序生成。

第一版的核心功能插件是**同步歌词**（粤语歌带逐字粤拼），用 Rust 原生重写，不依赖 Python 服务。

## 工作方式

```
墨水屏浏览器  ──HTTP──>  InkBuddy 主程序  ──JSON/stdio──>  插件进程（可多个）
 http://ip:9012/          Rust + axum               独立可执行文件
                              │
                              └─ 统一渲染成墨水屏页面
```

插件返回结构化内容（文本行、键值对、逐字注音），主程序负责排版。这样选择单个插件或把几个插件组合显示时，样式始终一致，插件之间也不需要互相知道对方存在。

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

根路径就是展示页，不需要输入后面的路径。其他入口：

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

不带参数时显示所有 `default_on = true` 的插件（默认只有歌词）。选择放在 URL 里，不写浏览器存储。

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

`./scripts/build-app.sh` 会生成 `dist/InkBuddy.app`。它是后台运行、没有 Dock 图标的 app 包，双击即启动服务；墨水屏仍然只需打开浏览器地址。第一版没有菜单栏 UI，插件选择通过 `/manage` 页面完成。

停止：`pkill -f InkBuddy.app/Contents/MacOS/inkbuddy`。

## 歌词插件

`crates/inkbuddy-plugin-lyrics` 是第一个自带插件：

- 读取 macOS 正在播放的曲目、进度和播放/暂停状态
- QQ 音乐歌词优先（简体、和桌面客户端版本一致），LRCLIB 兜底
- 三行聚焦视图：上一句 / 当前句 / 下一句，当前句最大
- 粤语歌在当前句每个字上方标逐字粤拼，读音和汉字分两套 grid 排版
- 进度用本地时钟推进：上报值变化（包括拖回开头）时重设锚点，暂停时冻结
- 歌词和粤拼缓存到 `~/.inkbuddy/cache/lyrics/*.json`

播放信息读取有两个后端。`crates/inkbuddy-plugin-lyrics/src/mediaremote.rs` 是原生 MediaRemote 框架的预留接口，第一版未启用；启动时 `create_backend()` 会先尝试它，失败后回退到 `nowplaying-cli`。实际生效的是后者，需要本机装过 `nowplaying-cli`（`brew install nowplaying-cli`）：

- `nowplaying-cli get-raw` 取标题、歌手、时长和进度。`get elapsedTime` 在部分播放器上恒为 0，所以走原始字段 `kMRMediaRemoteNowPlayingInfoElapsedTime`。
- `mediaremote-mini.pl adapter_get_env` 读权威的 `playing` 状态。QQ 音乐暂停时 `PlaybackRate` 仍可能是 1，只有这里的状态可靠。

这条链路不调用 AppleScript，因此不会把 Music.app 自动拉起。

QQ 的逐字粤拼来自 QRC 接口，返回的是 hex + 自定义 3DES + zlib 加密数据。`crates/inkbuddy-plugin-lyrics/src/qrc.rs` 用 Rust 实现了这个解密与逐字对齐，DES 表来自公开的 FIPS 46-3 标准。

## 写自己的插件

插件是独立可执行程序，用 stdin/stdout 上的行分隔 JSON 和主程序通信，可以用任何语言写，崩溃不影响主程序。完整协议、内容模型、Python 最小示例见 [docs/plugin-dev.md](docs/plugin-dev.md)。

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

## 与 inkbuddy 之外

本仓库是独立项目。此前的 `ink-lyrics` 是基于 SyncLyrics（Python/Quart）打补丁的原型，本仓库把核心链路用 Rust 重写，并补上插件体系。

## 说明

- HTTP 服务没有认证，默认监听 `0.0.0.0` 供局域网设备访问。只在可信局域网使用；需要隔离时把 `bind` 改成 `127.0.0.1` 并自行做转发。
- 墨水屏刷新慢且有残影，页面只在内容真正变化时才改 DOM，不做逐帧动画。
- 墨水屏浏览器若缓存了旧页面，在地址后加 `?v=2` 破一次缓存；页面本身已返回 `no-store`。

## 许可

各 crate 为 MIT。QQ QRC 解密涉及的 DES 算法与 S-box 数值来自公开的 FIPS 46-3 标准；实现为独立重写。
