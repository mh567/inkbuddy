# 开发自己的插件

插件是一个独立的可执行程序。inkbuddy 主程序启动它，通过 **stdin/stdout 上的行分隔 JSON** 通信：一行一个 JSON 对象，每行上限 1 MiB。插件可以用任何语言写（Rust、Python、Go、Shell 都行），和主程序之间没有共享运行时。

这个设计带来几个直接结果：

- 插件进程崩溃后，下一次请求会自动把它拉起来。
- 插件独立部署，没有 ABI 兼容问题。
- 插件只返回结构化内容，页面样式由主程序统一提供。

## 插件说明文件

每个插件在仓库里的 crate 根目录维护一份 `README.md`，写这个插件的输出内容、外部依赖、配置方式和实现原理。主 README 只放插件索引和链接，不写单个插件的细节。新增插件时一并补上这份说明。

仓库里的两个示例：[歌词插件](../crates/inkbuddy-plugin-lyrics/README.md)、[时钟插件](../crates/inkbuddy-plugin-clock/README.md)。

## 安装位置

在 `~/.inkbuddy/plugins/<任意目录名>/` 下放两个文件：

```
~/.inkbuddy/plugins/my-clock/
├── plugin.toml
└── my-clock          # 可执行程序
```

`plugin.toml`：

```toml
[plugin]
id = "demo"
name = "时钟"
version = "0.1.0"
description = "显示当前时间"
command = "./my-clock"        # 相对路径按 plugin.toml 所在目录解析

[runtime]
enabled = true
default_on = false            # 访问页面时不带参数，默认不显示
timeout_ms = 2000
args = []
```

`id` 必须唯一，且只允许 `[A-Za-z0-9_-]`（字母、数字、下划线、连字符）；它会进入 URL 参数和内部缓存键，不能带空格、斜杠或点号。主程序运行时扫描目录里的 `plugin.toml`，每 2 秒重扫一次，放进目录即可生效。

也可以用 `--plugins-dir <目录>` 指定额外扫描目录，或用 `INKBUDDY_HOME` 改掉整个运行时目录。

## 通信协议

主程序向插件 stdin 写一行：

```json
{"id":1,"method":"describe","params":null}
{"id":2,"method":"render","params":{"width":600,"height":800,"theme":"ink"}}
{"id":3,"method":"configure","params":{}}
{"id":4,"method":"shutdown","params":null}
```

插件必须在 stdout 写**一行**对应的响应，`id` 要原样带回：

```json
{"id":1,"ok":true,"result":{"id":"demo","name":"示例","version":"0.1.0","description":"最小插件","protocol":1}}
{"id":2,"ok":true,"result":{"blocks":[{"type":"key_value","items":[{"key":"时间","value":"20:30"}]}],"refresh_ms":15000}}
```

失败时：

```json
{"id":2,"ok":false,"error":"something went wrong"}
```

要点：

- **stdout 只能写协议响应**，日志和调试信息一律写 stderr。往 stdout 打印任何非响应内容都会破坏协议流。
- **一请求一行响应**：主程序写入一行请求后，会等 `id` 相同、恰好一行响应。解析失败、未知 method、空行都必须回一行失败响应，否则主程序会一直等到超时。
- **单行上限 1 MiB**：请求和响应都是单行 JSON，任何一方读到超过 1 MiB 的行都会按协议错误处理。
- `id` 要原样带回；它是 `u64`，但应小于 2^53，避免其它语言的 JSON 实现丢精度。
- 每次写完响应要 flush，主程序在等这一行。
- `shutdown` 回完响应就可以退出；主程序也会在关闭时终止子进程。

### method 说明

| method | 用途 | result |
| --- | --- | --- |
| `ping` | 健康检查 | `null` |
| `describe` | 返回插件信息 | `PluginInfo` |
| `render` | 产出当前内容 | `RenderResult` |
| `configure` | 接收配置（任意 JSON） | `null` |
| `shutdown` | 通知退出 | `null` |

`render` 的 `params` 里有 `width`、`height`（设备已知时）、`theme`（目前固定 `ink`）。

`refresh_ms` 只属于 `RenderResult`，表示主程序隔多久再向这个插件取一次内容；`PluginInfo` 里没有这个字段。上面的两个 `refresh_ms` 都是 render 响应，不是 describe 响应。

## 内容模型

插件返回 `blocks` 数组，每个 block 是一种内容块。主程序把这些块按插件顺序拼到同一个页面上，并套用统一的墨水屏样式。

`type` 的合法字面量只有四个：`lines`、`key_value`、`text`、`html`。其中 `key_value` 另接受别名 `kv` 和 `keyvalue`。未知 `type` 会被主程序忽略（只跳过该块，不影响同一帧里的其它块），方便协议向前兼容。

### 文本行

最常用的一种，歌词插件就用它：

```json
{
  "type": "lines",
  "lines": [
    {"text": "上一句", "emphasis": "muted"},
    {"text": "当前句", "emphasis": "primary"},
    {"text": "下一句", "emphasis": "muted"}
  ]
}
```

`emphasis` 取值：`primary`（最大、加粗）、`normal`、`muted`（较小、灰色）、`reading`（较小、灰色，单独一行的读音）。

读音单独占一行，和汉字行一样居中排版，不做逐字对齐。粤语歌的粤拼就是这么显示的：

```json
{
  "type": "lines",
  "lines": [
    {"text": "上一句", "emphasis": "muted"},
    {"text": "我会与你", "emphasis": "primary"},
    {"text": "o wui jyu nei", "emphasis": "reading"},
    {"text": "下一句", "emphasis": "muted"}
  ]
}
```

### 键值对

```json
{"type":"key_value","items":[{"key":"CPU","value":"12%"},{"key":"内存","value":"8.1 GB"}]}
```

### 段落

```json
{"type":"text","text":"一段说明文字"}
```

### 自定义 HTML

```json
{"type":"html","html":"<div>...</div>"}
```

这是自定义标记的入口。`html` 插入页面前会经过白名单净化：只保留 `div/span/p/b/i/em/strong/br/ul/li/code/pre/h1-h3` 这类有限标签，属性里只允许 `class`，剥离所有 `on*` 事件属性以及 `script`、`iframe`、`javascript:`，白名单外的标签转义成普通文本。自定义脚本和内联样式无法透传，只在确实需要自定义标记时使用。

## 选择组合或单个显示

访问的 URL 上带 `plugins` 参数，用逗号分隔，顺序就是页面从上到下的顺序：

```
http://<mac-ip>:9012/?plugins=lyrics
http://<mac-ip>:9012/?plugins=lyrics,clock
```

不带参数时使用所有 `default_on = true` 的插件。管理页 `http://127.0.0.1:9012/manage` 可以勾选并生成对应地址。选择放在 URL 里，浏览器无需保存状态，同一台主机可以给不同墨水屏设备配不同组合。

## 最小可用插件（Python）

```python
#!/usr/bin/env python3
import sys, json

for line in sys.stdin:
    try:
        req = json.loads(line)
    except Exception:
        continue
    rid, method = req["id"], req["method"]

    if method == "describe":
        result = {"id":"demo","name":"示例","version":"0.1.0",
                  "description":"最小插件","protocol":1}
    elif method == "render":
        result = {"blocks":[{"type":"text","text":"Hello from a plugin"}],
                  "refresh_ms":15000}
    elif method == "shutdown":
        print(json.dumps({"id":rid,"ok":True,"result":None}), flush=True)
        break
    else:
        result = None

    print(json.dumps({"id":rid,"ok":True,"result":result}), flush=True)
```

存为可执行文件，`plugin.toml` 的 `command` 指向它。主程序每 2 秒重扫一次插件目录，保存后最多等 2 秒就会自动发现，在 `/manage` 里即可看到。

## Rust 插件

可以直接依赖 `inkbuddy-protocol` crate，里面有 `Request`、`Response`、`Block`、`Line`、`Emphasis` 等类型和 serde 定义，省去手写 JSON 结构：

```toml
[dependencies]
inkbuddy-protocol = { path = "/path/to/inkbuddy/crates/inkbuddy-protocol" }
serde_json = "1"
```

仓库里的 `crates/inkbuddy-plugin-lyrics` 是完整参考实现。
