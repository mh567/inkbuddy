# InkBuddy

把闲置的墨水屏阅读器变成 macOS 的副屏。

墨水屏保持原样：在自带浏览器里输入一个局域网地址，就进入展示页面。内容由 macOS 上的 InkBuddy 主程序生成。

核心功能插件是**同步歌词**。「视图」系统把插件摆到屏幕的任意区域，所见即所得地编辑。

## 工作方式

```
墨水屏浏览器  ──HTTP──>  InkBuddy 主程序  ──JSON/stdio──>  插件进程（可多个）
 http://ip:9012/          Rust + axum               独立可执行文件
                              │
                              └─ 统一渲染成墨水屏页面
```

插件返回结构化内容（文本行、键值对、段落），主程序负责排版。按视图把插件放进屏幕区域（或 URL 参数组合显示）时，样式始终一致，插件之间互不依赖。

## 下载 app（推荐）

在 [Releases](https://github.com/mh567/inkbuddy/releases) 下载 `InkBuddy-macOS-arm64.dmg`（Apple Silicon），打开后把 InkBuddy 拖进 Applications 即可。app 内嵌 host 与插件，双击即用；点状态栏图标弹出面板，里面有墨水屏实时预览与视图切换，设置窗口管理视图、扩展启用/停用，并支持自动检查更新和一键升级。

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
| `/api/frame` | 聚合后的展示内容（JSON），含视图区域数据与页面版本号 |
| `/api/plugins` | 扩展列表及启用、运行状态 |
| `/api/views`、`/api/views/current` | 视图列表读写、应用视图 |
| `/api/preview` | 插件内容的结构化文本预览（POST） |
| `/api/preview/html` | 按给定区域与宽高比生成的完整页面（POST），控制端编辑器与状态栏面板预览使用 |
| `/api/display` | 显示模式（gray/bw/text）与反色（POST） |
| `/api/health` | 健康检查，含 60 秒内上报过的设备清单（设备 id、实测宽高比） |

选择单个或组合显示，直接在 URL 上给参数，顺序即页面从上到下的顺序：

```
http://<ip>:9012/?plugins=lyrics            # 只显示歌词
http://<ip>:9012/?plugins=clock             # 只显示时钟
http://<ip>:9012/?plugins=lyrics,clock      # 组合，歌词在上、时钟在下
http://<ip>:9012/?plugins=ai-quota          # 只显示 ChatGPT 额度
http://<ip>:9012/?plugins=command-code       # 只显示 Command Code 额度
http://<ip>:9012/?plugins=antigravity-quota  # 只显示反重力额度
```

不带参数时显示所有 `default_on = true` 的插件（歌词，以及装了 Codex CLI 的机器上的 ChatGPT 额度、本机留有 Command Code 凭据时启用的 Command Code 额度、装了反重力的机器上的反重力额度）。显示内容优先跟随当前视图（见下文「视图」）；没有选中视图时退到 URL 参数，再退到 `default_on`。选择放在 URL 里。

视图生效时每个插件渲染在自己的区域里，字号按区域大小自动缩放（可用宽度占比与高度装得下内容两个约束取小），缩放用的宽高比来自设备浏览器每次轮询上报的真实画布尺寸。每个区域四周留白（上下 3vh、左右 4vw），内容不贴墨水屏边缘；缩放按扣掉留白后的可用宽高算，内容多时字号变小。

墨水屏页面底部是 `inkbuddy` 条，点它进入全屏展示，底部条随之隐藏。全屏下点左侧区域切到上一个视图、右侧区域切到下一个视图；点中间区域在顶部显示或隐藏两个吊牌按钮：「退出全屏」和「横屏显示/竖屏显示」。设备物理旋转屏幕后，下一次轮询会带新画布尺寸，内容按新比例自动重排；旋转按钮把内容坐标系转 90°，在横竖屏之间手动切换，选择会记住，按钮与翻页触摸区保持在物理方向上。视图切换与状态栏面板的左右箭头走同一份视图数据。

扩展目录在运行时会被重新扫描，新放进目录的扩展即时生效。

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

扩展放在 `~/.inkbuddy/plugins/<目录>/plugin.toml`，启动时自动扫描。也可以：

```bash
inkbuddy --plugins-dir ./my-plugins --port 8080
inkbuddy --list-plugins
```

`INKBUDDY_HOME` 可改掉整个运行时目录（配置、扩展、歌词缓存都跟着走）。

### 视图

视图是一份具名的**区域布局**，保存在 `INKBUDDY_HOME/views.json`。每个视图由若干区域组成，每个区域把一个扩展放到屏幕的一个矩形里：

```json
{"id": "split", "name": "左右分屏",
 "regions": [
   {"plugin": "lyrics", "x": 0,   "y": 0,   "w": 60,  "h": 100},
   {"plugin": "clock",  "x": 60,  "y": 0,   "w": 40,  "h": 100}
 ]}
```

- 坐标全部是屏幕**百分比**（0–100，浮点）：任意位置、任意大小、任意分辨率自适应。
- 显示始终跟随当前视图的布局；没有选中视图时墨水屏按流式竖排渲染（URL 参数 / `default_on`）。
- 新用户首次启动预置三个视图：时钟、歌词、ChatGPT 额度，各整屏放一个扩展；对应扩展未安装时跳过该视图。
- 旧格式（`plugins` 列表）加载时自动迁移为竖排等分区域。
- 接口：`GET/POST /api/views`（读写整个列表）、`POST /api/views/current`（应用视图，`id` 传 `null` 只清指针）。
- `/api/frame` 返回每区域的几何、文本行与字号缩放，供墨水屏页面取帧；控制端的编辑器预览与状态栏面板预览走 `/api/preview/html`，宿主用与取帧相同的代码生成完整页面，控制端用 WebKit 显示，两处是同一份 HTML 和 CSS。预览文档的画布按整页高度算、不含底部条，区域位置与墨水屏一致。

macOS 菜单栏 app 的视图编辑窗口：左侧从已启用的扩展添加区域，画布上拖拽移动、四角缩放、相邻区域共享边同步拖动；右侧数值精调，下方内嵌实时预览（用 WebKit 显示宿主生成的页面，与墨水屏页面同源）。在「画布比例」菜单中，可以跟随在线设备的实测比例，也可以选择手动预设。「保存」落盘，「应用」直接生效且编辑窗口保持打开，方便对着墨水屏持续调整。

添加区域时的自动拆分以竖屏为准，主轴是上下（左右分出来的块太窄）：1 个全屏，2 个上下对半，3 个上下三等分（行数到 3 为止）；4 个起按「第一行左右、第二行左右、第三行左右」的顺序逐行加列，每行最多 3 列（9 个时是 3×3）；超过 9 个从第一格起把格子上下对分，容量到 18。画布还是拆分方案的形状时，每次添加整盘重排成下一档、新扩展拿最后一格；手动拖拽或缩放过的布局不再自动重排，新增时找空白放，找不到就把最大的区域对半切。

### 扩展启用与停用

- 扩展目录里扫描到的扩展都可以启用，不需要时再停用。停用后子进程退出，不再取数、不再渲染，省下它占的 CPU 和内存。
- 停用只是本地记录（`INKBUDDY_HOME/disabled.json`）：渲染时跳过、视图里引用它的区域不显示；扩展文件仍保留在磁盘上，随时可以再启用。
- 第一次启动时按各插件清单里的 `default_on` 生成这份停用列表：时钟和歌词默认启用，三个额度插件按本机有没有对应的命令行工具判定（打包时写入）。之后完全由用户在设置窗口里开关。
- 墨水屏没有选中视图时，默认布局按扫描顺序排列所有启用的扩展。
- 接口：`POST /api/plugins/<id>/enabled`，body `{"enabled": true|false}`。

### macOS app

`./scripts/build-macos-app.sh [版本号]` 会生成 `build-app/InkBuddy.app`。菜单栏 app 内嵌 Rust host，启动即服务；墨水屏仍然只需打开浏览器地址。

- 状态栏弹出面板：墨水屏内容的实时预览（WebKit 渲染，与设置窗口的预览同一份页面），左右箭头在已保存视图间循环应用，底部是「设置」和「退出 inkbuddy」。面板使用编辑器「画布比例」菜单中选定的比例。面板是普通窗口，网页能正常重绘；点面板外面自动收起。
- 设置窗口：视图管理与编辑、扩展启用/停用（停用只是本地标记，文件保留，随时可以再启用）、检查更新与一键升级、开机启动。
- 手动比例和跟随目标在重启或升级后保留。跟随设备离线时保留它最后一次上报的比例，重新上线后继续跟随。

歌词插件读取播放信息所需的组件随 app 一起分发。各插件的运行依赖见各自的说明文件。

停止：`pkill -x InkBuddy`（host 是内嵌子进程，会随之退出）。

## 自带插件

每个插件在自己的目录里维护说明文件，写输出内容、依赖、配置和工作原理。

| 插件 | 用途 | 说明 |
| --- | --- | --- |
| `lyrics` | 正在播放的同步歌词 | [crates/inkbuddy-plugin-lyrics/README.md](crates/inkbuddy-plugin-lyrics/README.md) |
| `clock` | 当前日期与时间，视图里可给时钟区域选经典、点阵、台历 | [crates/inkbuddy-plugin-clock/README.md](crates/inkbuddy-plugin-clock/README.md) |
| `ai-quota` | Codex 套餐与 5 小时、周额度用量，进度条展示 | [crates/inkbuddy-plugin-ai-quota/README.md](crates/inkbuddy-plugin-ai-quota/README.md) |
| `command-code` | Command Code 套餐与 5 小时、周、月额度用量 | [crates/inkbuddy-plugin-command-code/README.md](crates/inkbuddy-plugin-command-code/README.md) |
| `antigravity-quota` | Antigravity 的 Gemini、Claude/GPT 两组 5 小时与周额度，读本机语言服务器 | [crates/inkbuddy-plugin-antigravity-quota/README.md](crates/inkbuddy-plugin-antigravity-quota/README.md) |

额度类插件在对应的 AI 没被使用时会自动休眠：额度只在服务端重置时变化，持续监控没有意义。休眠期间插件收掉 app-server 子进程（或不再探测语言服务器），只看本地活动痕迹；页面继续显示上次的额度数字并附「更新于 X 分钟前」，开始使用或定时器到期（服务端额度重置）时自动恢复监控。各插件的判定与唤醒条件见各自的说明文件。

## 写自己的插件

插件是独立可执行程序，用 stdin/stdout 上的行分隔 JSON 和主程序通信，可以用任何语言写，插件进程崩溃后下一次请求会自动拉起。完整协议、内容模型、Python 最小示例见 [docs/plugin-dev.md](docs/plugin-dev.md)。

Rust 插件可直接依赖 `inkbuddy-protocol` crate 拿到全部类型定义。

## workspace 结构

| 路径 | 说明 |
| --- | --- |
| `crates/inkbuddy-protocol` | 共享协议与内容模型，插件作者可依赖 |
| `crates/inkbuddy-host` | 主程序：插件进程管理、HTTP 服务、视图、页面渲染 |
| `crates/inkbuddy-plugin-lyrics` | 歌词插件（核心功能），[说明](crates/inkbuddy-plugin-lyrics/README.md) |
| `crates/inkbuddy-plugin-clock` | 时钟插件，演示组合与最小插件写法，[说明](crates/inkbuddy-plugin-clock/README.md) |
| `crates/inkbuddy-plugin-ai-quota` | ChatGPT 额度看板插件（显示名「ChatGPT 额度」），读本机 Codex CLI 的账号额度，[说明](crates/inkbuddy-plugin-ai-quota/README.md) |
| `crates/inkbuddy-plugin-command-code` | Command Code 额度看板插件，读本机 Command Code CLI 的账号额度，[说明](crates/inkbuddy-plugin-command-code/README.md) |
| `crates/inkbuddy-plugin-antigravity-quota` | 反重力额度看板插件，读本机反重力语言服务器的额度接口，[说明](crates/inkbuddy-plugin-antigravity-quota/README.md) |
| `web/ink.html` | 墨水屏展示页（编译进主程序） |
| `app/Sources` | macOS 菜单栏 app（Swift，编译进 InkBuddy.app） |
| `plugins/*/plugin.toml` | 开发用的插件清单 |
| `scripts/install.sh` | 编译并安装 |
| `scripts/build-macos-app.sh` | 构建 InkBuddy.app（内嵌 host、插件与界面） |
| `docs/plugin-dev.md` | 插件开发文档 |

## 说明

- 项目在掌阅 iReader Neo2 墨水屏阅读器上做过调试：设备自带浏览器打开局域网地址即可显示。其他能接入同一局域网、带浏览器的墨水屏设备用法相同。
- HTTP 服务默认监听 `0.0.0.0`，不带认证，同一局域网里的任何设备都能读取展示内容、查看插件列表并切换显示组合。只在可信局域网使用；需要隔离时把 `bind` 改成 `127.0.0.1`，再自行做转发或加一层鉴权代理。
- 墨水屏刷新慢且有残影，页面只在内容真正变化时才改 DOM。
- 页面带版本号自愈：host 升级换了展示页后，已打开的旧页面会自动刷新一次。

## 许可

各 crate 为 MIT。QQ QRC 解密涉及的 DES 算法与 S-box 数值来自公开的 FIPS 46-3 标准。

歌词插件随 app 附带 nowplaying-cli 2.1.0 的 `MediaRemoteMini.dylib` 与 `mediaremote-mini.pl`，按 GPL-3.0 分发。许可证全文与源码获取方式放在 app 包的 `Contents/Resources/licenses/nowplaying-cli/`，仓库副本在 `vendor/nowplaying-cli/`。
