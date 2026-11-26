# AI 额度看板插件设计

日期：2026-11-26

## 目标

做一个 inkbuddy 插件，在墨水屏页面上显示本机 Codex CLI 账号的套餐和额度用量。先看 5 小时和周两个窗口，用进度条展示。插件不收集账号密码，不弹登录窗口，数据来自 macOS 上已安装的 Codex CLI。

## 已确认的范围

- 套餐：显示 `Plus`、`Pro`、`Business` 等可读名称；未知值显示后端返回的原始值。
- 额度：5 小时窗口和周窗口各一条进度条，显示 `usedPercent` 和距 `resetsAt` 的剩余时间。
- 重置券：在顶部一行显示 `rateLimitResetCredits.availableCount`。
- 月度额度：本期不做。本机 app-server 的 `account/rateLimits/read` 只返回两个窗口，没有月度百分比。
- 账号邮箱：不显示。

## 数据来源

Codex CLI 在本机提供一个 app-server。插件和它之间走 JSON-RPC，行分隔 JSON，走 stdio。

```sh
codex app-server --listen stdio://
```

握手：先发 `initialize`，`params.clientInfo` 填插件名和版本；收到响应后发 `initialized` 通知。

读取：

- `account/read`：取 `account.type`、`account.planType`。实测返回 `chatgpt` / `plus`。
- `account/rateLimits/read`：取 `rateLimits.primary`、`rateLimits.secondary` 和 `rateLimitResetCredits.availableCount`。实测本机 `primary.windowDurationMins = 300`，`secondary.windowDurationMins = 10080`。

通知：app-server 协议定义了 `account/rateLimits/updated` 和 `account/updated`。实测空闲 20 秒只收到 `account/updated` 和 `remoteControl/status/changed`，没有收到额度更新通知。因此插件同时保留 60 秒主动读取。收到通知时把可用字段合并进当前快照，字段为空时沿用旧值。

可执行文件查找顺序：`CODEX_BIN` 环境变量、PATH 中的 `codex`、`~/.local/bin/codex`。插件不读 `auth.json`，不保存令牌。

## 进程模型

宿主把插件作为长驻子进程启动，多次 `render` 复用同一进程。插件内部再启动一个长驻 app-server 子进程。

```
inkbuddy 宿主
  └─ 插件进程
       ├─ app-server 子进程（stdio JSON-RPC）
       └─ 60 秒定时读取
```

- 插件启动后连接 app-server，握手，取账号和额度，存入内存快照。
- 后台任务持续读 app-server 的 stdout：带 `id` 的消息按 id 匹配请求，不带 `id` 的消息按通知处理。
- 60 秒定时器先调 `account/read` 再调 `account/rateLimits/read`。先读账号是为了在运行中登出时立刻发现，切到「未登录」提示；只读额度的话，登出后拿到的还是旧数字。收到额度通知时也更新快照。
- 宿主的 `render` 只读快照，直接返回。首次 `render` 若快照未就绪，最多等 3 秒。
- app-server 退出后按 1 秒到 30 秒的退避间隔重连，期间保留最后一次快照。连接存活满 30 秒后，下一次重连从 1 秒重新开始，避免反复快速失败的连接把退避推高。
- `shutdown` 请求到达后先取消定时器、终止 app-server 子进程，再回响应退出。宿主读到响应就 SIGKILL 还活着的插件，收尾放在响应之后会被打断，子进程变孤儿。
- 日志写 stderr；stdout 只写协议响应。

## 页面输出

`render` 返回一个 HTML 块，三行：

1. 左侧 ChatGPT 图标和 `ChatGPT` 文字，右上角套餐名和重置券数量，例如 `Plus · 券 2`。
2. 「5 小时」进度条：百分比和「X小时Y分后重置」。
3. 「本周」进度条：百分比和重置倒计时。

图标以黑白 SVG data URI 写在 `web/ink.html` 的样式表里。插件输出的 HTML 只有一个带 class 的空 `span`。宿主没有静态文件目录，页面由 `include_str!` 编进二进制，所以图标不能引用外部文件。

进度条宽度按 5% 取整，填充层用预设 class，`.q-0` 到 `.q-100` 共 21 档，对应 `width: 0%` 到 `width: 100%`。插件 HTML 不能写内联 `style`，白名单会剥掉。样式里的尺寸乘 `--ink-scale`，并补上反色和纯黑白模式的颜色规则。

`describe` 返回的 `variants` 为空；`refresh_ms` 设 30000，让倒计时保持准确。实际取数由插件内部的 60 秒定时器驱动。

## 错误处理

- 找不到 `codex` 可执行文件：显示「未找到 Codex CLI」。
- `account/read` 返回空账号或鉴权失败：显示「Codex 未登录，请先运行 codex login」。
- 额度读取失败且已有快照：继续显示旧数据，顶部加「更新于 X 分钟前」。
- 额度读取失败且没有快照：显示「额度读取失败」。
- JSON-RPC 请求超时：放弃这次请求，保留连接或按重连流程处理。

## 文件清单

| 路径 | 改动 |
| --- | --- |
| `crates/inkbuddy-plugin-ai-quota/Cargo.toml` | 新 crate，依赖 `inkbuddy-protocol`、`tokio`、`serde_json`、`anyhow`、`chrono` |
| `crates/inkbuddy-plugin-ai-quota/src/main.rs` | 协议循环、app-server 客户端、快照、渲染 |
| `crates/inkbuddy-plugin-ai-quota/src/locate.rs` | 按 `CODEX_BIN`、`PATH`、`~/.local/bin` 查找可执行文件 |
| `crates/inkbuddy-plugin-ai-quota/src/formatting.rs` | 套餐名、百分比宽度 class、重置倒计时、HTML 转义 |
| `crates/inkbuddy-plugin-ai-quota/src/render.rs` | 把快照渲染成 HTML 块 |
| `crates/inkbuddy-plugin-ai-quota/tests/loop.rs` | 协议循环端到端测试：render、坏请求、shutdown 收尾 |
| `crates/inkbuddy-plugin-ai-quota/tests/service.rs` | 额度服务端到端测试：握手、轮询、通知、失败提示 |
| `crates/inkbuddy-plugin-ai-quota/README.md` | 插件说明：数据来源、配置方式、输出内容 |
| `Cargo.toml` | workspace members 加入新 crate |
| `plugins/ai-quota/plugin.toml` | id `ai-quota`，默认不开启，超时 8000 毫秒 |
| `web/ink.html` | ChatGPT 图标与进度条样式 |
| `scripts/install.sh` | 安装插件二进制和清单 |
| `scripts/build-macos-app.sh` | 把插件打进 app bundle |
| `README.md` | 插件索引加一行 |

## 测试

单元测试：

- `usedPercent` 到宽度 class 的映射，包括 0、100、边界取整。
- 套餐名映射和未知值回退。
- `resetsAt` 到倒计时文字的格式，包括已过期。
- JSON-RPC 行解析：请求响应匹配、通知合并、非法行忽略。
- 渲染文本中的特殊字符转义。

集成测试用假的 app-server 脚本，验证握手、两次读取、`shutdown` 收尾。测试不读取真实令牌，也不访问 OpenAI 服务。

## 验收

```sh
cargo test -p inkbuddy-plugin-ai-quota
cargo build --release
./scripts/install.sh
```

启动宿主后通过 `GET /api/plugins` 看到 `ai-quota`，用 `?plugins=ai-quota` 打开页面。页面上的两个百分比与本机 `account/rateLimits/read` 的读数一致；断开网络或退出 Codex 登录后，页面显示对应的提示文字。
