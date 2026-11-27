# 反重力额度插件（inkbuddy-plugin-antigravity-quota）

在墨水屏上显示本机反重力（Antigravity）账号的额度。插件 id 为 `antigravity-quota`，安装后的清单在 `plugins/antigravity-quota/plugin.toml`。`scripts/install.sh` 与 `scripts/build-macos-app.sh` 会检测 `/Applications/Antigravity.app` 以及 `~/.gemini/antigravity`、`~/.gemini/antigravity-cli` 两个数据目录：有任意一个就写 `default_on = true`，都没有就写 `false`。两种情况下都可以用 `?plugins=antigravity-quota` 参数，或把 `default_on` 改成 `true` 来启用。

## 显示内容

- 顶部一行：`Antigravity` 和套餐名，例如 `Pro`。
- `Gemini` 分组标题，下面两行：「5 小时」与「本周」两个窗口的已用百分比进度条和重置倒计时。
- `Claude、GPT` 分组标题，同样两行。

两个分组是接口里分开计费的额度池：Gemini 系列模型共用一份，Claude 与 GPT 系列共用另一份。额度满且从没用过时接口不返回重置时间，这一行写「未使用」。进度条按 5% 取整，填充宽度由 `q-0` 到 `q-100` 共 21 档预设 class 控制，样式写在 `web/ink.html`，随宿主一起分发。

反重力关着的时候，页面沿用最后一次读到的数字，底部写「反重力未运行 · 更新于 X 小时前」。如果某个窗口的重置时间在这段时间里已经过去，那个窗口的百分比不再可信，这一行改成「已重置」，不再显示旧数字。

## 数据来源

额度优先从本机正在运行的反重力语言服务器读，请求只发往回环地址；语言服务器自己会按需向 Google 取额度，和在 IDE 里看额度是同一回事，插件不额外登录、也不碰账号令牌。

1. 端口和 CSRF 令牌：用 `/bin/ps -A -ww -o pid=,command=` 找 `language_server` 进程，从命令行里取 `--csrf_token`、`--extension_server_port`、`--http_server_port`、`--https_server_port`。端口是随机分配的，所以每轮都要重新读。
2. 额度：`POST http://127.0.0.1:<port>/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary`，返回 `groups[].buckets[]`，每个桶带 `bucketId`（`gemini-5h`、`gemini-weekly`、`3p-5h`、`3p-weekly`）、`window`、`remainingFraction`、`resetTime`。
3. 套餐名：`POST .../GetUserStatus`，取 `userStatus.planStatus.planInfo.planName`。这条读不到不影响额度显示。

`ps` 没找到进程时，退到日志里记录的端口：`~/Library/Logs/Antigravity/language_server.log`、`~/.gemini/antigravity/log/`、`~/.gemini/antigravity-cli/log/` 里最新的那份，只读末尾 64 KiB。

没有 TLS 依赖：请求带 `Connection: close`，明文 HTTP 读到 EOF 就算响应结束。只监听 HTTPS 的版本会读不到，目前没遇到过。

### 命令行版

命令行版（`agy`）把语言服务器的 CSRF 令牌留在进程内，命令行、日志、缓存里都没有，从外面读不到。端口应答 401 时，插件改用它自己的命令：

```sh
agy -p "/usage" --log-file <临时文件>
```

输出四行，制表符分列：分组、窗口、剩余百分比、重置时间。这条命令会自己起一个语言服务器，一次约 6 秒，所以只在「有端口应答却读不出」时才走，轮询间隔也改成 5 分钟。`--log-file` 把日志指到临时文件，`~/.gemini/antigravity-cli/log/` 不会每轮多出一份。这条路的输出里没有套餐名，顶部写「未知套餐」。

## 反重力开着或关着

反重力不常开、插件一直开着，两边谁在跑都不一定，所以额度快照会落盘，插件重启后接着用：

| 情况 | 页面 | 什么时候变 |
| --- | --- | --- |
| 反重力开着（IDE），插件也在跑 | 实时数字 | 每 60 秒读一次，读到就落盘 |
| 反重力开着（命令行版 `agy`） | 实时数字 | 每 5 分钟跑一次 `agy -p "/usage"` |
| 反重力关掉，插件还在跑 | 上次的数字 + 「反重力未运行 · 更新于 X」 | 待机：每 10 秒看一次日志与会话目录的修改时间，不跑 `ps`、不连端口 |
| 插件刚启动，反重力开着 | 新数字在一轮取数后出现 | 启动后第一次读取就在首帧之前跑起来 |
| 插件刚启动，反重力关着 | 立刻是落盘的数字 + 提示 | 不用等满 3 秒 |
| 插件没在跑（没进视图、宿主退出） | 不更新 | 下次启动的第一件事就是读一次 |

宿主的 `render` 读内存快照并直接返回；首次 `render` 会等第一轮读取结束（成功或失败都算），最多等 3 秒。插件返回的 `refresh_ms` 平时是 30000，让倒计时保持准确；还没有读到任何数字时返回 2000，页面会早点换上新数字。

反重力关着的时候服务进入待机，只做几件事：每 10 秒 `stat` 一次 `~/Library/Logs/Antigravity/` 和 `~/.gemini/antigravity*/` 下日志与会话目录的最新修改时间。用户打开 IDE、起一个 `agy` 会话、跑一轮任务，这些文件会立刻被改写，服务在 10 秒内醒来跑一次完整取数，读到数就回到正常轮询。待机期间每 10 分钟还有一次兜底全量探测，防止痕迹判定漏掉。插件自己调 `agy -p "/usage"` 时日志指到临时目录，不会把自己叫醒。

落盘文件是 `$INKBUDDY_HOME/cache/antigravity-quota/last.json`，没设 `INKBUDDY_HOME` 时是 `~/.inkbuddy/cache/antigravity-quota/last.json`。内容是带 `updated_at` 的快照，先写临时文件再改名，进程中途被杀不会留下半份 JSON；`version` 对不上或文件损坏时当作没有记录。

## 配置

可执行文件从清单里的 `command` 启动，配置项就是 `plugin.toml`：

```toml
[runtime]
enabled = true
default_on = true
timeout_ms = 8000
args = []
```

`default_on` 决定不带 `?plugins=` 参数时这块出不出现。`timeout_ms` 建议 8000：首次读端口并取数通常不到 1 秒，留出余量。

调试时可以跳过发现流程，直接用指定的端口：

```sh
ANTIGRAVITY_QUOTA_PORT=61235 ./target/release/inkbuddy-plugin-antigravity-quota
# 多端口用逗号分隔；需要令牌时再加 ANTIGRAVITY_QUOTA_CSRF=<token>
# 命令行版的位置不在默认路径时：ANTIGRAVITY_QUOTA_AGY=/path/to/agy
```

## 失败提示

| 情况 | 页面显示 |
| --- | --- |
| 本机没有反重力（既没有 app，也没有 `~/.gemini/antigravity*` 数据目录） | 「未找到 Antigravity」 |
| 装了但语言服务器没在跑，也没有可用的命令行版 | 「反重力未运行」 |
| 端口连上了但读不出额度，命令行版也读不到（没登录、接口变了） | 「额度读取失败」 |
| 读取失败但已有旧数据 | 看板照旧，下方按距上次成功更新显示「刚刚更新」「更新于 X 分钟前」或「更新于 X 小时前」 |
| 读取失败且没有旧数据 | 「额度读取失败」 |
| 旧数据里某个窗口的重置时间已经过去 | 那一行显示「已重置」，不再显示过期百分比 |

插件不直接启动语言服务器：它要 IDE 通过 stdin 发一段握手才会开始监听。命令行版那句 `agy -p "/usage"` 由插件调用，进程随命令结束，不留后台常驻。

## 单独运行

编译后可以直接用协议行验证插件输出：

```sh
printf '{"id":1,"method":"describe"}\n{"id":2,"method":"render","params":{}}\n{"id":3,"method":"shutdown"}\n' \
  | ./target/release/inkbuddy-plugin-antigravity-quota
```

`render` 的响应里，`result.blocks[0].html` 就是看板页面。stderr 里会有每个探测失败的端口和原因，排查时先看它。

## 源码结构

| 文件 | 内容 |
| --- | --- |
| `src/main.rs` | 协议循环，持有额度服务，`render` 输出 HTML 块 |
| `src/discovery.rs` | 从 `ps` 和日志里找端口与 CSRF 令牌 |
| `src/cli.rs` | 命令行版兜底：找 `agy`，跑 `/usage` 并解析四行输出 |
| `src/http.rs` | 最小 HTTP/1.1 客户端，只发一次 JSON POST |
| `src/quota.rs` | 候选端口轮询、两组额度的解析、失败原因判定 |
| `src/model.rs` | 快照数据结构 |
| `src/formatting.rs` | 百分比宽度 class、重置倒计时、RFC 3339 解析、HTML 转义 |
| `src/render.rs` | 把快照渲染成 HTML 块 |
| `src/service.rs` | 后台定时取数，成功写盘，失败保留旧数据 |
| `src/store.rs` | 最近一次快照的落盘与读取 |
