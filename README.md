# InkBuddy

把闲置的墨水屏阅读器变成 macOS 的副屏。

墨水屏保持原样：在自带浏览器里输入一个局域网地址，就进入展示页面。内容由 macOS 上的 InkBuddy 主程序生成。

核心功能插件是**同步歌词**（粤语歌带粤拼）。「视图」系统把插件摆到屏幕的任意区域，所见即所得地编辑。

## 工作方式

```
墨水屏浏览器  ──HTTP──>  InkBuddy 主程序  ──JSON/stdio──>  插件进程（可多个）
 http://ip:9012/          Rust + axum               独立可执行文件
                              │
                              └─ 统一渲染成墨水屏页面
```

插件返回结构化内容（文本行、键值对、段落），主程序负责排版。按视图把插件放进屏幕区域（或 URL 参数组合显示）时，样式始终一致，插件之间互不依赖。

## 下载 app（推荐）

在 [Releases](https://github.com/mh567/inkbuddy/releases) 下载 `InkBuddy-macOS-arm64.dmg`（Apple Silicon），打开后把 InkBuddy 拖进 Applications 即可。app 内嵌 host 与插件，双击即用；菜单栏提供墨水屏实时预览与视图切换，设置窗口管理视图、扩展安装/卸载，并支持自动检查更新和一键升级。

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
| `/api/plugins` | 已安装插件及运行状态 |
| `/api/views`、`/api/views/current` | 视图列表读写、应用视图 |
| `/api/preview` | 插件内容的结构化预览（POST），控制端编辑器与预览使用 |
| `/api/display` | 显示模式（gray/bw/text）与反色（POST） |
| `/api/health` | 健康检查，含设备最近上报的屏幕宽高比 |

选择单个或组合显示，直接在 URL 上给参数，顺序即页面从上到下的顺序：

```
http://<ip>:9012/?plugins=lyrics            # 只显示歌词
http://<ip>:9012/?plugins=clock             # 只显示时钟
http://<ip>:9012/?plugins=lyrics,clock      # 组合，歌词在上、时钟在下
```

不带参数时显示所有 `default_on = true` 的插件（默认只有歌词）。显示内容优先跟随当前视图（见下文「视图」）；没有选中视图时退到 URL 参数，再退到 `default_on`。选择放在 URL 里，浏览器无需保存状态。

视图生效时每个插件渲染在自己的区域里，字号按区域大小自动缩放（宽度占比与高度装得下内容两个约束取小），缩放用的宽高比来自设备浏览器每次轮询上报的真实视口尺寸。

扩展目录在运行时会被重新扫描，新放进目录的扩展不用重启主程序就能生效。

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
- 旧格式（`plugins` 列表）加载时自动迁移为竖排等分区域。
- 接口：`GET/POST /api/views`（读写整个列表）、`POST /api/views/current`（应用视图，`id` 传 `null` 只清指针）。
- `/api/frame` 返回每区域的几何、文本行与字号缩放，供墨水屏页面取帧；控制端的编辑器预览与菜单栏预览走 `/api/preview`，取同一套结构化内容后按相同规则原生排版。

macOS 菜单栏 app 的视图编辑窗口：左侧从已安装扩展添加区域（自动避让已有区域），画布上拖拽移动、四角缩放、相邻区域共享边同步拖动；右侧数值精调，下方内嵌实时预览（随拖拽实时渲染，排版与墨水屏页面同源）。画布比例自动同步设备实测宽高比，选择会持久化。「保存」落盘，「应用」直接生效且编辑窗口保持打开，方便对着墨水屏持续调整。

### 扩展安装与卸载

- 扩展目录里被扫描到的扩展就是「已安装」。
- 「卸载」只是本地标记（`INKBUDDY_HOME/uninstalled.json`）：渲染时跳过、视图里引用它的区域不显示、子进程停掉；扩展文件仍保留在磁盘上。
- 「安装」即移除卸载标记，下次渲染恢复。
- 接口：`POST /api/plugins/<id>/installed`，body `{"installed": true|false}`。

### macOS app

`./scripts/build-macos-app.sh [版本号]` 会生成 `build-app/InkBuddy.app`。菜单栏 app 内嵌 Rust host，启动即服务；墨水屏仍然只需打开浏览器地址。

- 菜单栏下拉：墨水屏内容的原生实时预览（不跑 WebKit），左右箭头在已保存视图间循环应用。
- 设置窗口：视图管理与编辑、扩展安装/卸载（卸载只是本地标记，文件保留，随时可重装）、检查更新与一键升级、开机启动。
- app 打开编辑器时自动同步设备实测宽高比；比例选择与视图数据在重启、升级后都保留。

从 Finder 双击启动时，进程拿到的 `PATH` 通常不含 Homebrew 的 `/opt/homebrew/bin`，app 包在 `Info.plist` 里通过 `LSEnvironment` 显式补上。各插件的运行依赖见各自的说明文件。

停止：`pkill -x InkBuddy`（host 是内嵌子进程，会随之退出）。

## 自带插件

每个插件在自己的目录里维护说明文件，写输出内容、依赖、配置和工作原理。

| 插件 | 用途 | 说明 |
| --- | --- | --- |
| `lyrics` | 正在播放的同步歌词，粤语歌带粤拼 | [crates/inkbuddy-plugin-lyrics/README.md](crates/inkbuddy-plugin-lyrics/README.md) |
| `clock` | 当前日期与时间 | [crates/inkbuddy-plugin-clock/README.md](crates/inkbuddy-plugin-clock/README.md) |

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
- 页面带版本号自愈：host 升级换了展示页后，已打开的旧页面会自动刷新一次，无需手动处理。

## 许可

各 crate 为 MIT。QQ QRC 解密涉及的 DES 算法与 S-box 数值来自公开的 FIPS 46-3 标准。
