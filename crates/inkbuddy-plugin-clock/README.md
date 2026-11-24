# 时钟插件（inkbuddy-plugin-clock）

显示当前时间和日期的插件。它同时是最短的完整插件示例，演示如何用 `inkbuddy-protocol` 返回内容块。

插件 id 为 `clock`，安装后的清单在 `plugins/clock/plugin.toml`。默认不随展示页开启，用 `?plugins=clock` 参数、视图（在 macOS app 里把时钟加进视图）或 `default_on = true` 启用。

## 输出内容

- 一行 `HH:MM` 时间，作为主要文本行显示。
- 一组键值对：日期（`YYYY-MM-DD`）和星期（中文）。
- `refresh_ms` 设为 15000，主程序每 15 秒取一次内容。

## 依赖

只依赖 `inkbuddy-protocol`、`chrono`、`tokio`、`serde_json` 和 `anyhow`。不读取系统信息，不需要额外安装命令行工具。

## 源码结构

整个插件只有 `src/main.rs`：读取 stdin 上的行分隔 JSON 请求，按 method 返回响应。各 method 的实现可直接对照 [docs/plugin-dev.md](../../docs/plugin-dev.md)。
