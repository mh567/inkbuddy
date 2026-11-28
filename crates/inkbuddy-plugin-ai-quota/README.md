# ChatGPT 额度插件（inkbuddy-plugin-ai-quota）

在墨水屏上显示本机 Codex CLI 账号的套餐和额度用量。显示名为「ChatGPT 额度」，插件 id 为 `ai-quota`，安装后的清单在 `plugins/ai-quota/plugin.toml`。`scripts/install.sh` 与 `scripts/build-macos-app.sh` 会检测本机有没有 `codex`：检测到就写 `default_on = true`，随展示页一起显示；没检测到就写 `false`。两种情况下都可以用 `?plugins=ai-quota` 参数，或把 `default_on` 改成 `true` 来启用。

## 显示内容

- 顶部一行：ChatGPT 图标和名称，右侧是套餐名与重置券数量，例如 `Plus · 券 2`。
- 第二行：「5 小时」窗口的已用百分比进度条，以及距重置的倒计时。
- 第三行：「本周」窗口的已用百分比进度条，以及距重置的倒计时。

进度条按 5% 取整，填充宽度由 `q-0` 到 `q-100` 共 21 档预设 class 控制。插件输出一个 HTML 块，样式与图标写在 `web/ink.html`，随宿主一起分发。

倒计时不足 1 小时按分钟写，不超过 24 小时按「X 小时 X 分」写，超过 24 小时按「X 天 X 小时」写，整天只写「X 天」。

## 数据来源

数据来自本机已安装的 Codex CLI。插件启动 `codex app-server --listen stdio://` 作为子进程，通过行分隔 JSON 的 JSON-RPC 取数：

- 握手：发 `initialize`，收到响应后发 `initialized` 通知。
- `account/read`：读账号类型和套餐名。
- `account/rateLimits/read`：读 5 小时与周两个窗口的已用百分比、重置时间，以及重置券数量。

连上之后每 60 秒主动读一次账号和额度；收到 `account/rateLimits/updated`、`account/updated` 通知时把可用字段合并进当前快照。每次成功读到都同步落盘到 `$INKBUDDY_HOME/cache/ai-quota/last.json`。app-server 断线后按 1 秒到 30 秒的退避间隔重连，重连期间页面沿用旧数据；连续 5 轮会话都没读到数据时转入浅眠。

## 浅眠

Codex 没在使用时，额度只在服务端重置时变化，持续保活 app-server 和每 60 秒轮询都在白读一遍。所以本插件在本地无活动后进入浅眠：

- 判定：本地活动痕迹（`~/.codex/sessions` 会话文件、`~/.codex/log/codex-tui.log`、`~/.codex/auth.json` 的修改时间）连续 10 分钟无变化。使用中的会话文件持续写入，静默时钟不断归零，监控不会中断。
- 进入浅眠后 app-server 子进程被回收，只留插件本体每 10 秒看一次痕迹修改时间；页面继续显示上次的额度数字，下方附「更新于 X 分钟前」。
- 唤醒条件：活动痕迹变化（开始使用 Codex、重新登录）立即恢复完整监控；每 30 分钟定时醒一次，读一轮账号与额度再睡——额度重置发生在服务端，本地没活动也要周期性读，否则数字停在旧值。
- 登出状态同样归入浅眠：登录会改写 `auth.json`，痕迹变化会把监控拉起来。

宿主的 `render` 读内存快照并直接返回；首次 `render` 时快照尚未就绪，最多等 3 秒，服务启动时已先把落盘快照放进内存，重启后第一帧直接有数字。插件返回的 `refresh_ms` 是 30000，让倒计时保持准确，实际取数由内部的定时器驱动。

## 配置

可执行文件查找顺序：

1. 环境变量 `CODEX_BIN`（显式配置时只认这个路径）。
2. `PATH` 各目录下的 `codex`。
3. `~/.local/bin/codex`。

插件配置项就是清单里的 `plugin.toml`：

```toml
[runtime]
enabled = true
default_on = true
timeout_ms = 8000
args = []
```

`default_on` 决定不带 `?plugins=` 参数时这块出不出现，仓库里的清单写 `true`，安装与打包脚本按本机有没有 `codex` 覆盖它。`timeout_ms` 建议 8000：首次启动 app-server 并读取约需 2 秒，留出余量。`args` 保持为空。令牌和登录状态由 Codex CLI 自己管理，插件只读 app-server 返回的账号与额度字段。

## 失败提示

| 情况 | 页面显示 |
| --- | --- |
| 找不到 Codex CLI | 「未找到 Codex CLI」 |
| Codex 未登录 | 「Codex 未登录，请先运行 codex login」 |
| 读取失败但已有旧数据 | 看板照旧，看板下方按距上次成功更新显示「刚刚更新」「更新于 X 分钟前」或「更新于 X 小时前」 |
| 读取失败且没有旧数据 | 「额度读取失败」 |
| 浅眠中（无失败原因） | 看板照旧，下方附「更新于 X 分钟前」，提示监控已暂停 |
| 数据太旧（超过 10 分钟，含重启后首帧的缓存） | 看板照旧，下方附「更新于 X 分钟前」 |

登出和找不到 CLI 时，整块看板换成对应的提示文字，旧百分比不再显示，避免旧数字误导用户。

## 单独运行

编译后可以直接用协议行验证插件输出：

```sh
printf '{"id":1,"method":"describe"}\n{"id":2,"method":"render","params":{}}\n{"id":3,"method":"shutdown"}\n' \
  | ./target/release/inkbuddy-plugin-ai-quota
```

`render` 的响应里，`result.blocks[0].html` 就是看板页面。

## 源码结构

| 文件 | 内容 |
| --- | --- |
| `src/main.rs` | 协议循环，持有额度服务，`render` 输出 HTML 块 |
| `src/locate.rs` | 按 `CODEX_BIN`、`PATH`、`~/.local/bin` 查找可执行文件 |
| `src/app_server.rs` | 长驻 app-server 客户端：握手、轮询、退避重连、浅眠状态机、发布快照 |
| `src/activity.rs` | 本地活动痕迹：会话文件与日志的最新修改时间，浅眠唤醒的依据 |
| `src/store.rs` | 快照落盘与读回（`$INKBUDDY_HOME/cache/ai-quota/last.json`） |
| `src/rpc.rs` | JSON-RPC 行解析与账号/额度字段合并 |
| `src/model.rs` | 快照数据结构 |
| `src/formatting.rs` | 套餐名、百分比宽度 class、重置倒计时、HTML 转义 |
| `src/render.rs` | 把快照渲染成 HTML 块 |
