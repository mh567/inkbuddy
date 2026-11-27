# Command Code 额度插件（inkbuddy-plugin-command-code）

在墨水屏上显示本机 Command Code 账号的套餐和额度用量。插件 id 为 `command-code`，安装后的清单在 `plugins/command-code/plugin.toml`。`scripts/install.sh` 按安装机器上有没有 `cmd` 写 `default_on`：检测到就写 `true`，随展示页一起显示；没检测到就写 `false`。app bundle 里的清单先写 `false`，app 启动时按本机 `~/.commandcode/auth.json` 是否存在改写；清单只在首次安装时从 bundle 复制，之后保留用户的修改。两种情况下都可以用 `?plugins=command-code` 参数，或把 `default_on` 改成 `true` 来启用。

## 显示内容

看板是四行，输出一个 HTML 块，样式与图标写在 `web/ink.html`，随宿主一起分发：

- 顶部一行：Command Code 图标和名称，右侧是套餐名与剩余 credit，例如 `GOAT · 余 62.4`。
- 第二行：「5 小时」窗口的已用百分比进度条，以及距重置的倒计时。
- 第三行：「本周」窗口的已用百分比进度条，以及距重置的倒计时。
- 第四行：「本月」窗口的已用百分比进度条，以及本期结束倒计时。

套餐名识别 Go、GOAT、Pro、Max、Ultra、Teams Pro；接口返回其他 `planId` 时直接显示原始值。进度条按 5% 取整，填充宽度由 `q-0` 到 `q-100` 共 21 档预设 class 控制。

月度行只按套餐自带的 credit 池计算百分比。顶部显示的剩余 credit 是套餐剩余 credit、购买 credit 和赠送 credit 三项之和，额外购买和赠送的部分只进入顶部总额，算作套餐外的余量。5 小时与周窗口的上限取接口返回的 `cap`，`cap` 或 `used` 缺失时该行百分比显示 `--`；重置时间已知时倒计时照常。倒计时不足 1 小时按分钟写，不超过 24 小时按「X 小时 X 分」写，超过 24 小时按「X 天 X 小时」写，整天只写「X 天」。

## 数据来源

凭据来自本机 cmd CLI 已保存的登录态：优先读环境变量 `COMMAND_CODE_API_KEY`，为空时读 `~/.commandcode/auth.json` 顶层的 `apiKey` 字段。文件里的其他令牌由 CLI 自己管理，插件只取 `apiKey` 这一项。key 只在插件进程内用于请求鉴权，插件进程退出后不落盘。

插件向 `api.commandcode.ai` 发四个只读 GET，认证头是 `Authorization: Bearer <key>`，另带 CLI 标识头 `x-command-code-version` 和 `x-cli-environment`：

1. `GET /alpha/whoami` 取账号与组织 id。
2. `GET /alpha/billing/subscriptions` 取套餐 id 与本期起止时间。
3. `GET /alpha/billing/credits` 取剩余 credit 与 5 小时、周窗口的用量和重置时间。
4. `GET /alpha/usage/summary` 取本期请求数、花费与 token 数，存进快照供后续使用。

第 2、3 个请求并发发出，第 4 个请求等第 2 个拿到本期起始时间后再发。首轮取数立即执行，之后每 60 秒一轮。遇到限流、服务端错误或网络失败按 2 倍、4 倍、8 倍退避，上限 30 分钟；成功一次回到 60 秒。

宿主的 `render` 读内存快照并直接返回；首次 `render` 时快照尚未就绪，最多等 3 秒。插件返回的 `refresh_ms` 是 30000，让倒计时保持准确，实际取数由内部的 60 秒定时器驱动。

## 配置

`COMMAND_CODE_API_KEY` 的优先级高于 `~/.commandcode/auth.json`，设置后沿用这个 key，适合无 CLI 的场景。凭据由本机 CLI 登录态提供，插件进程启动时读取一次，运行时没有登录窗口和密码输入。本机先装好 cmd CLI 并登录一次，登录态会写入 `~/.commandcode/auth.json`，插件据此取数。命令行重新登录后要重启插件进程，新的登录态才会被读到。

`COMMAND_CODE_BASE_URL` 是测试用的接口地址覆盖项：测试进程把它指向本地假 HTTP server，走的是假 key，请求留在本机。这个值只接受 `http://127.0.0.1`、`http://localhost` 或 `http://[::1]` 加端口；写成其他地址时插件改用生产地址。日常使用保持未设置。

插件配置项就是清单里的 `plugin.toml`：

```toml
[runtime]
enabled = true
default_on = true
timeout_ms = 8000
args = []
```

`default_on` 决定不带 `?plugins=` 参数时这块出不出现，仓库里的清单写 `true`，安装脚本按本机有没有 `cmd` 覆盖它。app 首次安装时按本机登录态改写，之后保留用户修改。`timeout_ms` 建议 8000：首轮取数含四个请求，留出余量。`args` 保持为空。

## 失败提示

| 情况 | 页面显示 |
| --- | --- |
| 本机没有登录态或 key | 「Command Code 未登录，请先运行 cmd login」 |
| 计费接口没有额度数据 | 「额度数据未找到」 |
| 读取失败但已有旧数据 | 看板照旧，看板下方按距上次成功更新显示「刚刚更新」「更新于 X 分钟前」或「更新于 X 小时前」 |
| 读取失败且没有旧数据 | 「额度读取失败」 |
| 快照尚未就绪 | 「正在读取额度…」 |

未登录与找不到额度数据是终态，整块看板换成对应提示文字。单项字段缺失时那一行显示 `--` 和「暂无数据」，其余行照常；窗口重置时间已知时倒计时继续走。

## 请求边界

插件的请求限于四个 `/alpha/*` 计费类只读 GET，额度从余额和用量记录读取。请求的目标地址固定在 `api.commandcode.ai`；测试钩子 `COMMAND_CODE_BASE_URL` 只能指向本机 loopback。客户端关闭重定向，响应的地址与路径都要落在允许集合里。key 留在插件进程内存里，日志只记录状态码。

## 单独运行

编译后可以直接用协议行验证插件输出：

```sh
printf '{"id":1,"method":"describe"}\n{"id":2,"method":"render","params":{}}\n{"id":3,"method":"shutdown"}\n' \
  | ./target/release/inkbuddy-plugin-command-code
```

`render` 的响应里，`result.blocks[0].html` 就是看板页面。

## 源码结构

| 文件 | 内容 |
| --- | --- |
| `src/main.rs` | 协议循环，持有额度服务，`render` 输出 HTML 块 |
| `src/lib.rs` | 模块导出 |
| `src/auth.rs` | 环境变量与本机 `auth.json` 的 key 读取顺序 |
| `src/api.rs` | 四个计费接口的请求、响应解析与错误分类 |
| `src/model.rs` | 快照结构与套餐 credit 池 |
| `src/formatting.rs` | 套餐名、百分比宽度 class、重置倒计时、HTML 转义 |
| `src/render.rs` | 把快照渲染成 HTML 块 |
| `src/service.rs` | 长驻取数服务：定时轮询、退避、快照发布 |
| `tests/api.rs` | 假 HTTP server 下的取数与降级测试 |
| `tests/loop.rs` | 协议循环端到端测试 |
