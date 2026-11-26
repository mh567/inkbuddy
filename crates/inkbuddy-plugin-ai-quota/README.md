# AI 额度插件（inkbuddy-plugin-ai-quota）

在墨水屏上显示本机 Codex CLI 账号的套餐和额度用量。插件 id 为 `ai-quota`，安装后的清单在 `plugins/ai-quota/plugin.toml`。`scripts/install.sh` 与 `scripts/build-macos-app.sh` 会检测本机有没有 `codex`：检测到就写 `default_on = true`，随展示页一起显示；没检测到就写 `false`。两种情况下都可以用 `?plugins=ai-quota` 参数，或把 `default_on` 改成 `true` 来启用。

## 显示内容

- 顶部一行：ChatGPT 图标和名称，右侧是套餐名与重置券数量，例如 `Plus · 券 2`。
- 第二行：「5 小时」窗口的已用百分比进度条，以及距重置的倒计时。
- 第三行：「本周」窗口的已用百分比进度条，以及距重置的倒计时。

进度条按 5% 取整，填充宽度由 `q-0` 到 `q-100` 共 21 档预设 class 控制。插件输出一个 HTML 块，样式与图标写在 `web/ink.html`，随宿主一起分发。

## 数据来源

数据来自本机已安装的 Codex CLI。插件启动 `codex app-server --listen stdio://` 作为子进程，通过行分隔 JSON 的 JSON-RPC 取数：

- 握手：发 `initialize`，收到响应后发 `initialized` 通知。
- `account/read`：读账号类型和套餐名。
- `account/rateLimits/read`：读 5 小时与周两个窗口的已用百分比、重置时间，以及重置券数量。

连上之后每 60 秒主动读一次账号和额度；收到 `account/rateLimits/updated`、`account/updated` 通知时把可用字段合并进当前快照。app-server 断线后按 1 秒到 30 秒的退避间隔重连，重连期间页面沿用旧数据。

宿主的 `render` 读内存快照并直接返回；首次 `render` 时快照尚未就绪，最多等 3 秒。插件返回的 `refresh_ms` 是 30000，让倒计时保持准确，实际取数由内部的 60 秒定时器驱动。

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
| `src/app_server.rs` | 长驻 app-server 客户端，握手、轮询、退避重连、发布快照 |
| `src/rpc.rs` | JSON-RPC 行解析与账号/额度字段合并 |
| `src/model.rs` | 快照数据结构 |
| `src/formatting.rs` | 套餐名、百分比宽度 class、重置倒计时、HTML 转义 |
| `src/render.rs` | 把快照渲染成 HTML 块 |
