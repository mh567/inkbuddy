# 开发自己的插件

插件是一个独立的可执行程序。inkbuddy 主程序启动它，通过 **stdin/stdout 上的行分隔 JSON** 通信：一行一个 JSON 对象，没有长度前缀，没有握手。插件可以用任何语言写（Rust、Python、Go、Shell 都行），不需要和主程序用同一套编译器或运行时。

这个设计带来几个直接结果：

- 插件崩溃不会让主程序退出，下一次请求会自动把插件进程拉起来。
- 插件不需要链接任何 inkbuddy 的库，也没有 ABI 兼容问题。
- 插件不需要知道墨水屏的存在，只返回结构化内容；页面样式由主程序统一提供。

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
id = "clock"
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

`id` 必须唯一。主程序启动时扫描所有 `plugin.toml`，不做编译期注册，放进目录即可生效。

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
{"id":1,"ok":true,"result":{"id":"clock","name":"时钟","version":"0.1.0","description":"显示当前时间","refresh_ms":1000,"protocol":1}}
{"id":2,"ok":true,"result":{"blocks":[{"type":"key_value","items":[{"key":"时间","value":"20:30"}]}],"refresh_ms":1000}}
```

失败时：

```json
{"id":2,"ok":false,"error":"something went wrong"}
```

要点：

- **stdout 只能写协议响应**。日志、调试信息一律写 stderr，否则会破坏协议流。非 JSON 的行会被主程序跳过，但不建议依赖这个行为。
- 每次写完响应要 flush，主程序在等这一行。
- `shutdown` 回完响应就可以退出；主程序也会在关闭时终止子进程。

### method 说明

| method | 用途 | result |
| --- | --- | --- |
| `ping` | 健康检查 | `null` |
| `describe` | 返回插件信息 | `PluginInfo`，其中 `refresh_ms` 是建议轮询间隔 |
| `render` | 产出当前内容 | `RenderResult` |
| `configure` | 接收配置（任意 JSON） | `null` |
| `shutdown` | 通知退出 | `null` |

`render` 的 `params` 里有 `width`、`height`（设备已知时）、`theme`（目前固定 `ink`）。

## 内容模型

插件返回 `blocks` 数组，每个 block 是一种内容块。主程序把这些块按插件顺序拼到同一个页面上，并套用统一的墨水屏样式。

### 文本行

最常用的一种，歌词插件就用它：

```json
{
  "type": "lines",
  "lines": [
    {"text": "上一句", "emphasis": "muted", "ruby": []},
    {"text": "当前句", "emphasis": "primary", "ruby": []},
    {"text": "下一句", "emphasis": "muted", "ruby": []}
  ]
}
```

`emphasis` 取值：`primary`（最大、加粗）、`normal`、`muted`（较小、灰色）。

`ruby` 非空时，主程序会把这一行渲染成两行 grid：第一行是逐字读音，第二行是对应汉字，两套排版各自独立但共享列宽，所以能逐字对齐。粤语歌的粤拼就是这么显示的：

```json
{
  "text": "我会与你变爱侣",
  "emphasis": "primary",
  "ruby": [
    {"word": "我", "reading": "o"},
    {"word": "会", "reading": "wui"},
    {"word": "与", "reading": "yu"},
    {"word": "你", "reading": "nei"}
  ]
}
```

`word` 和 `reading` 分开存，读音可以是多字母罗马字，汉字仍是一个字。

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

这是逃生口，内容原样插入页面，会绕过主程序样式。只在确实需要自定义标记时使用，且内容是自己写的可信内容。

## 选择组合或单个显示

访问的 URL 上带 `plugins` 参数，用逗号分隔，顺序就是页面从上到下的顺序：

```
http://<mac-ip>:9012/?plugins=lyrics
http://<mac-ip>:9012/?plugins=lyrics,clock
```

不带参数时使用所有 `default_on = true` 的插件。管理页 `http://127.0.0.1:9012/manage` 可以勾选并生成对应地址。选择放在 URL 里，不写浏览器存储，同一台主机可以给不同墨水屏设备配不同组合。

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
                  "description":"最小插件","refresh_ms":1000,"protocol":1}
    elif method == "render":
        result = {"blocks":[{"type":"text","text":"Hello from a plugin"}],
                  "refresh_ms":1000}
    elif method == "shutdown":
        print(json.dumps({"id":rid,"ok":True,"result":None}), flush=True)
        break
    else:
        result = None

    print(json.dumps({"id":rid,"ok":True,"result":result}), flush=True)
```

存为可执行文件，`plugin.toml` 的 `command` 指向它，重启主程序即可在 `/manage` 看到。

## Rust 插件

可以直接依赖 `inkbuddy-protocol` crate，里面有 `Request`、`Response`、`Block`、`Line`、`Ruby` 等类型和 serde 定义，省去手写 JSON 结构：

```toml
[dependencies]
inkbuddy-protocol = { path = "/path/to/inkbuddy/crates/inkbuddy-protocol" }
serde_json = "1"
```

仓库里的 `crates/inkbuddy-plugin-lyrics` 是完整参考实现。
