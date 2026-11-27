# Command Code 额度看板插件设计

日期：2026-11-27

## 目标

做一个 inkbuddy 插件，在墨水屏页面上显示本机 Command Code 账号的套餐和额度。页面结构和现有的 AI 额度看板一致：顶部一行品牌与套餐，下面三行进度条。数据来自本机 `cmd` CLI 已保存的登录态，插件不弹登录窗口，不向用户要密码。

## 已确认的范围

- 套餐：显示 Go、GOAT、Pro、Max、Ultra、Teams Pro 等可读名称；未知 `planId` 保留后端返回的原始值。
- 额度：5 小时、本周、本月各一条进度条，显示已用百分比和重置时间。
- 月度额度：Command Code 的月额度是一个 credit 池子，额外购买和赠送的 credit 不受窗口限制。顶部显示剩余 credit，月行显示套餐额度的已用百分比和本期结束倒计时。
- 请求用量：本计费周期的请求数、花费和 token 数有接口返回，本期只取数存进快照，页面暂不展示。
- 账号邮箱：不显示。
- 取数方式：只向 `api.commandcode.ai` 的计费类接口发只读 GET，不调用任何模型推理接口。查额度本身不消耗额度。

## 为什么不走官方 CLI 命令

`cmd status --json` 实测只有 7 个字段：`authenticated`、`version`、`user`、`provider`、`model`、`context_window`，没有套餐和额度。官方 `/usage` 斜杠命令能看到全部字段，但它是交互式 TUI，插件要通过伪终端启动会话、处理目录信任提示、输入命令、解析带 ANSI 转义的屏幕文字，一次取数 4–8 秒，还会写会话记录。墨水屏看板每分钟刷新，这条路径太脆，TUI 布局一改就失效。

社区工具（OpenUsage、cmduse、DSH 额度面板）统一走 CLI 自带的 `/alpha/*` 计费接口。这些接口和 `/usage` 同源，字段最全，命令式取数，刷新可以做到分钟级。没有封号实例指向只读额度工具；封号案例集中在把订阅额度转成推理服务对外供给（`/alpha/generate` 反代转售）和批量注册薅补贴。本插件只发只读 GET。

## 数据来源

### 凭据

读取顺序：环境变量 `COMMAND_CODE_API_KEY` → `~/.commandcode/auth.json` 的 `apiKey` 字段。实测本机文件顶层有 `apiKey`，插件只读这一个字段。文件里还有 `codex.access`、`codex.refresh` 等令牌，插件不读取、不保存、不发送。

环境变量优先，方便 CI 和无 CLI 的场景。key 只在插件进程内存里使用，不写回文件、不发给宿主和页面。

### 接口

base URL `https://api.commandcode.ai`，认证头 `Authorization: Bearer <key>`，另外带两个 CLI 标识头 `x-command-code-version: 1.88.5`、`x-cli-environment: production`。版本头沿用社区验证过的值，接口拿它判断请求来源；本机 CLI 实际版本是 1.93.3，两个值都能通过。

每轮取数按依赖顺序发 4 个 GET：

1. `GET /alpha/whoami` — 账号与组织 id。失败就直接判为鉴权失败，不再请求后面的接口。
2. `GET /alpha/billing/subscriptions` — `planId`、`status`、`currentPeriodStart`、`currentPeriodEnd`。组织账号带 `?orgId=`。
3. `GET /alpha/billing/credits` — `credits.monthlyCredits`、`purchasedCredits`、`freeCredits`，以及 `windowLimits.fiveHour` / `windowLimits.weekly`。组织账号带 `?orgId=`。
4. `GET /alpha/usage/summary?since=<currentPeriodStart>` — 本期请求数、花费、token 数。失败只影响用量统计，不影响额度显示。

第 2、3 步并发发。第 4 步要等第 2 步拿到 `currentPeriodStart`。

### 字段映射

| 接口字段 | 页面用途 |
| --- | --- |
| `subscriptions.planId` | 套餐档位；内置表识别，未知值显示原文 |
| `subscriptions.currentPeriodEnd` | 本月额度的重置时间 |
| `credits.monthlyCredits + purchasedCredits + freeCredits` | 剩余 credit；`purchased + free` 单独作为「额外」 |
| `windowLimits.fiveHour.used / cap / resetAt` | 5 小时窗口的进度与重置时间 |
| `windowLimits.weekly.used / cap / resetAt` | 周窗口的进度与重置时间 |
| `summary.totalCount / totalCost / totalTokens` | 本期用量，存入快照 |

套餐档位的 credit 池内置一份，用于算月度百分比：`Go 10`、`GOAT 70`、`Pro 30`、`Max 150`、`Ultra 300`、`Teams Pro 40`。`planId` 带 `_v1` 后缀或版本尾巴时先归一化再查表。5 小时与周窗口的上限只取接口返回的 `cap`；`cap` 或 `used` 缺失时该行百分比写 `None`，页面显示 `--`，不从百分比反推上限，也不按套餐补一个固定窗口值。

三个 `resetAt` 形态都要处理：毫秒时间戳、秒时间戳、ISO 8601 字符串。窗口滚动的一瞬间接口可能省略整个窗口、`cap`、`used` 或 `resetAt`；此时沿用旧快照的重置时间，百分比写 `None`，让倒计时继续走。连重置时间也拿不到时该行才留空。

进度条百分比：`5小时 = used / cap * 100`、`本周 = used / cap * 100`、`本月 = (池子 - monthlyCredits) / 池子 * 100`，四舍五入到最近的 5%，钳在 0–100。月行只算套餐额度，顶部显示的剩余 credit 是 monthlyCredits、purchasedCredits、freeCredits 三项之和，额外购买和赠送的 credit 不进月度百分比。Command Code 的 credit 不等于美元，对全额额度模型 1 credit 约等于 1 美元用量，高额度模型按倍率折算，所以页面只写 credit 和百分比，不换算成美元。

## 进程模型

宿主把插件作为长驻子进程启动，多次 `render` 复用同一进程。插件内部起一个后台任务定时取数，不需要再启动 CLI 子进程。

```
inkbuddy 宿主
  └─ 插件进程
       ├─ 后台 HTTP 取数（60 秒一轮）
       └─ 内存快照
```

- 启动时读凭据，起后台任务。首轮立刻取数，之后按 60 秒一轮。
- 遇到 429、5xx 或网络失败按 2×、4×、8× 退避，上限 30 分钟；成功一次就重置回 60 秒。额度耗尽的 `limited` 状态不算错误，按正常间隔继续查重置时间。
- `render` 只读快照，直接返回。首次 `render` 若快照未就绪，最多等 3 秒。
- `shutdown` 到达后先取消后台任务，再回响应退出。宿主读到响应就 SIGKILL 还活着的插件，收尾放在响应之后会被打断。
- 日志写 stderr；stdout 只写协议响应。日志里不打印 key 和 token。

## 页面输出

复用现有 `.quota-*` 样式和结构，返回 HTML 块，四行：

1. 左侧 Command Code 图标和 `Command Code` 文字，右上角套餐名和剩余 credit，例如 `GOAT · 余 62.4`。
2. 「5 小时」进度条：百分比和「X小时Y分后重置」。
3. 「本周」进度条：百分比和重置倒计时。
4. 「本月」进度条：百分比和「X天后重置」。

图标以黑白 SVG data URI 写在 `web/ink.html` 的样式表里，用一个新 class（例如 `.quota-mark-cc`），和 ChatGPT 图标分开。宿主没有静态文件目录，页面由 `include_str!` 编进二进制，图标不能引用外部文件。

进度条宽度按 5% 取整，填充层用预设 class `.q-0` 到 `.q-100`。这些 class 和 `--ink-scale`、反色、纯黑白模式的规则已经在样式表里，新插件不改颜色规则。插件 HTML 不能写内联 `style`，白名单会剥掉。

`describe` 返回的 `variants` 为空；`refresh_ms` 设 30000，让倒计时保持准确。实际取数由插件内部的 60 秒定时器驱动。

## 错误处理

- 没有 `auth.json` 或 `apiKey`：显示「Command Code 未登录，请先运行 cmd login」。这是终态，旧百分比会误导用户，整块换成提示。
- `/alpha/whoami` 返回 401：同上，凭据过期。
- 取数失败且已有快照：继续显示旧数据，底部加「更新于 X 分钟前」。
- 取数失败且没有快照：显示「额度读取失败」。
- 429 或 5xx：退避重试，保留旧快照，日志记状态码。
- 单项字段缺失：那一行显示 `--` 和「暂无数据」，其余行照常。套餐识别不出来就显示原始 `planId`。
- 请求超时：放弃这次请求，保留旧快照，按失败处理。

## 风控边界

设计上把请求限制在四条只读 GET：

- 只访问 `api.commandcode.ai`，不访问其他域名，不跟随重定向到第三方。请求发出前校验目标地址：生产域名，或测试钩子指向的本机 loopback；响应的地址与路径也要落在允许集合里。key 只随请求发往这些地址。
- 不发 `/alpha/generate` 或任何模型推理请求，不消耗订阅额度。
- 不读 `auth.json` 里的 OAuth access/refresh token，只用 `apiKey`。
- 60 秒基础间隔加指数退避；快照没变化时也照常取，不做高频轮询。
- 页面刷新（`refresh_ms`）只在本地重渲染倒计时，不触发网络请求。

已知边界：`/alpha/*` 是 CLI 内部接口，没有公开稳定性承诺，接口改了插件要跟着改。字段缺失一律降级显示，不报错、不填假数字。

## 文件清单

| 路径 | 改动 |
| --- | --- |
| `crates/inkbuddy-plugin-command-code/Cargo.toml` | 新 crate，依赖 `inkbuddy-protocol`、`reqwest`（rustls-tls）、`serde`、`serde_json`、`anyhow`、`tokio`、`chrono` |
| `crates/inkbuddy-plugin-command-code/src/main.rs` | 协议循环、快照、渲染 |
| `crates/inkbuddy-plugin-command-code/src/lib.rs` | 模块导出 |
| `crates/inkbuddy-plugin-command-code/src/auth.rs` | 环境变量与 `auth.json` 的 key 读取 |
| `crates/inkbuddy-plugin-command-code/src/api.rs` | 四个接口的请求、响应解析、退避 |
| `crates/inkbuddy-plugin-command-code/src/model.rs` | 快照结构、套餐表 |
| `crates/inkbuddy-plugin-command-code/src/formatting.rs` | 百分比换算、倒计时、HTML 转义 |
| `crates/inkbuddy-plugin-command-code/src/render.rs` | 把快照渲染成 HTML 块 |
| `crates/inkbuddy-plugin-command-code/tests/loop.rs` | 协议循环端到端测试：render、坏请求、shutdown 收尾 |
| `crates/inkbuddy-plugin-command-code/tests/api.rs` | 取数测试：假 HTTP server、字段缺失、退避、失败提示 |
| `crates/inkbuddy-plugin-command-code/README.md` | 插件说明：数据来源、配置方式、输出内容、风控边界 |
| `Cargo.toml` | workspace members 加入新 crate |
| `plugins/command-code/plugin.toml` | id `command-code`，默认按本机是否留有 `cmd` 登录态决定，超时 8000 毫秒 |
| `web/ink.html` | Command Code 图标样式 |
| `scripts/install.sh` | 安装插件二进制和清单；按 `command -v cmd` 决定 `default_on` |
| `scripts/build-macos-app.sh` | 把插件打进 app bundle；bundle 清单先写 `default_on = false` |
| `app/Sources/HostService.swift` | app 启动时按 `~/.commandcode/auth.json` 改写 `default_on`；`plugin.toml` 只在首次安装时从 bundle 复制 |
| `README.md` | 插件索引加一行 |

## 测试

单元测试：

- `planId` 映射和未知值回退，带版本后缀的归一化。
- `used / cap` 到宽度 class 的换算，包括 0、100、超出钳位、cap 或 used 缺失时写 `None`。
- 剩余 credit 到月度百分比的换算。
- 三种 `resetAt` 格式的解析，以及空值沿用旧快照。
- 倒计时文字格式，包括已过期和整天。
- 退避间隔计算：429、5xx、成功重置、上限 30 分钟。
- 渲染文本中的特殊字符转义。
- 凭据读取顺序：环境变量优先、`auth.json` 回退、两者都没有。

集成测试用本地假 HTTP server 返回固定的 `/alpha/*` JSON，验证四个请求的顺序与依赖、并发取数、订阅失败时的降级、401 提示、退避、shutdown 收尾。另外覆盖地址白名单、计费响应全空时的终态、非法凭据、窗口缺失时重置时间沿用。测试用假 key，不读真实令牌，不访问 `api.commandcode.ai`。

## 验收

```sh
cargo test -p inkbuddy-plugin-command-code
cargo build --release
./scripts/install.sh
```

启动宿主后通过 `GET /api/plugins` 看到 `command-code`，用 `?plugins=command-code` 打开页面。页面上的套餐、5 小时、周百分比与本机 `cmd` 的 `/usage` 输出一致；改坏凭据文件的 key 后，页面显示「未登录」提示。

## 参考

- `commandcode.ai/docs/resources/usage-limits`：官方用量窗口说明。
- `geminicli.com` 之外的社区实现：`openusage.sh/docs/providers/command-code`（接口与字段映射）、`github.com/miko-cn/dsh-commandcode-quota`（真实请求头、缓存与降级）、`lib.rs/crates/cmd-usage`（本地日志与 API 的分工）。
- `gitmemories.com/decolua/9router/issues/1528`：官方对反代转售的态度，区分只读额度与对外供给。
