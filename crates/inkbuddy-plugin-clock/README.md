# 时钟插件（inkbuddy-plugin-clock）

显示当前时间和日期的插件。它同时是最短的完整插件示例，演示如何用 `inkbuddy-protocol` 返回内容块。

插件 id 为 `clock`，安装后的清单在 `plugins/clock/plugin.toml`。默认不随展示页开启，用 `?plugins=clock` 参数、视图（在 macOS app 里把时钟加进视图）或 `default_on = true` 启用。

## 样式

插件声明三种子样式，`describe` 会把它们报给宿主，供 macOS app 的界面列出：

| id | 名称 | 显示 |
| --- | --- | --- |
| `classic` | 经典 | 一行 `HH:MM` 主文本，下方日期、星期键值对。 |
| `dot` | 点阵 | 点阵字形的时间，下方日期与星期。像素字体内嵌在展示页里，设备断网也能显示。 |
| `calendar` | 台历 | 顶部年份月份与圆形「今日」印记，中间一个大「日」，底部星期和时间。 |

样式在视图里选：macOS app 的视图编辑器把左侧「时钟」展开成三个子样式按钮，点某个就把带该样式的时钟加进画布。样式存进视图的区域里，宿主渲染时放进 `RenderParams.variant` 传给插件；选中区域后也可以在右侧「样式」下拉里改。区域没写 variant 时用上面表格的第一项 `classic`，传来的 id 不在表格里时也用 `classic`。

`dot` 和 `calendar` 用 `Block::Html` 输出带 class 的标记，具体尺寸、字体和灰阶写在 `web/ink.html`。插件只给元素命名和填时间。两种样式都只用正常文档流和 flex 排布，不用相对整屏的绝对定位，缩到小区域也不会被裁掉。区域布局下宿主会把缩放系数写成 CSS 变量 `--ink-scale` 下发，尺寸都乘这个变量。

HTML 块另外带两个属性，宿主读取后用于布局计算，清洗页面时会剥掉，不会出现在墨水屏上：

| 属性 | 位置 | 用途 |
| --- | --- | --- |
| `data-ink-height="31.2"` | 根元素 | 内容高度，单位是「正方形屏幕下的 vh」。宿主据此算 `--ink-scale`，短区域才会缩小内容。数值按 `web/ink.html` 里对应样式的 vw 尺寸累加。 |
| `data-ink-role="primary"` | 叶子元素 | 该行在结构化文本预览里的视觉权重，可取 `primary`、`normal`、`muted`、`reading`。 |

宿主只读这两个属性，不认识插件的 class 名，所以新增样式或换插件不需要改宿主渲染代码。

macOS app 的编辑器预览走宿主的 `/api/preview/html`：宿主用同一套渲染代码生成完整页面，控制端用 WebKit 显示，所以预览与墨水屏是同一份 HTML 和 CSS。

## 输出内容

- `classic`：一行 `HH:MM` 主文本 + 日期（`YYYY-MM-DD`）、星期（中文）键值对。
- `dot` / `calendar`：一个 HTML 块，包含时间、日期、星期；原生预览会从块里提取文字行。
- `refresh_ms` 设为 15000，主程序每 15 秒取一次内容。样式由 `RenderParams.variant` 决定，不走 `configure` 接口。

## 依赖

只依赖 `inkbuddy-protocol`、`chrono`、`tokio`、`serde_json` 和 `anyhow`。不读取系统信息，不需要额外安装命令行工具。

## 源码结构

整个插件只有 `src/main.rs`：读取 stdin 上的行分隔 JSON 请求，按 method 返回响应。各 method 的实现可直接对照 [docs/plugin-dev.md](../../docs/plugin-dev.md)。
