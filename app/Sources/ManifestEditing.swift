import Foundation

/// 清单文本的布尔开关读写。app 每次启动用 bundle 清单打底，把用户改过的
/// 运行时开关写回去，其余字段随 bundle 更新。
enum ManifestEditing {
    /// 解析一行布尔赋值。只认 `key = true|false`，注释行与同前缀的其他键跳过。
    /// 返回赋值前缀、原值、行尾；行内注释和 CRLF 的 \r 都留在行尾。
    static func boolLine(_ line: String, key: String) -> (String, Bool, String)? {
        let escaped = NSRegularExpression.escapedPattern(for: key)
        let pattern = "^([ \\t]*\(escaped)[ \\t]*=[ \\t]*)(true|false)(\\b)(.*)$"
        guard let regex = try? NSRegularExpression(
            pattern: pattern, options: [.dotMatchesLineSeparators]
        ),
            let match = regex.firstMatch(in: line, range: NSRange(line.startIndex..., in: line)),
            let head = Range(match.range(at: 1), in: line),
            let value = Range(match.range(at: 2), in: line),
            let tail = Range(match.range(at: 4), in: line)
        else {
            return nil
        }
        return (String(line[head]), String(line[value]) == "true", String(line[tail]))
    }

    /// 取 `[runtime]` 段里某个布尔键的值。
    static func boolValue(_ text: String, key: String) -> Bool? {
        var inRuntime = false
        for line in text.components(separatedBy: "\n") {
            let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
            if let section = sectionName(trimmed) {
                inRuntime = section == "runtime"
                continue
            }
            guard inRuntime, let (_, value, _) = boolLine(line, key: key) else { continue }
            return value
        }
        return nil
    }

    /// 把 `[runtime]` 段里某个布尔键换成给定值，保留缩进、行内注释和行尾符。
    static func replacingBool(_ text: String, key: String, value: Bool) -> String {
        var lines = text.components(separatedBy: "\n")
        var inRuntime = false
        for index in lines.indices {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
            if let section = sectionName(trimmed) {
                inRuntime = section == "runtime"
                continue
            }
            guard inRuntime, let (head, _, tail) = boolLine(line, key: key) else { continue }
            lines[index] = head + (value ? "true" : "false") + tail
        }
        return lines.joined(separator: "\n")
    }

    /// 读表头名字，接受 `[runtime]` 与 `[runtime] # 注释` 两种写法。
    private static func sectionName(_ trimmed: String) -> String? {
        guard trimmed.hasPrefix("["),
              let close = trimmed.firstIndex(of: "]") else { return nil }
        return String(trimmed[trimmed.index(after: trimmed.startIndex)..<close])
    }

    /// 用 bundle 清单打底，把目标清单里的运行时开关写回去。
    static func merged(bundle: String, existing: String?, runtimeKeys: [String]) -> String {
        var text = bundle
        if let existing {
            for key in runtimeKeys {
                if let value = boolValue(existing, key: key) {
                    text = replacingBool(text, key: key, value: value)
                }
            }
        }
        return text
    }

    /// 生成写入运行时目录的清单文本。目标清单存在时保留用户开关，
    /// command-code 首次落地时按本机凭据判定 default_on。
    static func resolved(
        bundle: String, existing: String?, plugin: String,
        hasCredentials: Bool, runtimeKeys: [String]
    ) -> String {
        var text = merged(bundle: bundle, existing: existing, runtimeKeys: runtimeKeys)
        if existing == nil && plugin == "command-code" {
            text = replacingBool(text, key: "default_on", value: hasCredentials)
        }
        return text
    }

    /// 拼出 auth.json 的路径，口径与插件 auth.rs 一致：优先用 HOME 环境变量。
    static func commandCodeAuthPath(env: [String: String], homeDirectory: URL) -> URL {
        let base = env["HOME"].map { URL(fileURLWithPath: $0, isDirectory: true) }
            ?? homeDirectory
        return base
            .appendingPathComponent(".commandcode", isDirectory: true)
            .appendingPathComponent("auth.json")
    }

    /// 判定本机是否有 Command Code 凭据，口径与插件 auth.rs 一致：
    /// 环境变量 COMMAND_CODE_API_KEY 有非空白值，或 auth.json 顶层能解析出非空白的字符串 apiKey。
    static func hasCommandCodeCredentials(env: [String: String], authJSON: Data?) -> Bool {
        if let key = env["COMMAND_CODE_API_KEY"],
           !key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return true
        }
        guard let data = authJSON,
              let object = try? JSONSerialization.jsonObject(with: data),
              let fields = object as? [String: Any],
              let apiKey = fields["apiKey"] as? String,
              !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            return false
        }
        return true
    }
}
