import Foundation

/// Swift 侧清单处理的独立验证。编译运行：
/// swiftc app/Sources/ManifestEditing.swift app/Tests/ManifestEditingTests.swift -o /tmp/inkbuddy-manifest-tests
@main
struct ManifestEditingTests {
    static func main() {
        var passed = 0
        func check(_ condition: @autoclosure () -> Bool, _ message: String) {
            guard condition() else { fatalError("FAIL: \(message)") }
            passed += 1
        }
        func data(_ text: String) -> Data { Data(text.utf8) }

        // 布尔赋值行的解析：前缀、值、行尾分开返回。
        if let (head, value, tail) = ManifestEditing.boolLine("default_on = true", key: "default_on") {
            check(head == "default_on = ", "赋值前缀")
            check(value, "读到 true")
            check(tail.isEmpty, "无行尾内容")
        } else {
            fatalError("FAIL: 基本赋值行没有解析出来")
        }
        if let (_, value, tail) = ManifestEditing.boolLine("  enabled = false # 用户改的", key: "enabled") {
            check(!value, "读到 false")
            check(tail == " # 用户改的", "行内注释留在行尾")
        } else {
            fatalError("FAIL: 带注释的赋值行没有解析出来")
        }

        // 注释行、同前缀键、非布尔值都不算赋值行。
        check(ManifestEditing.boolLine("# default_on = true", key: "default_on") == nil,
              "注释行跳过")
        check(ManifestEditing.boolLine("default_on_extra = true", key: "default_on") == nil,
              "同前缀键跳过")
        check(ManifestEditing.boolLine("default_on = trueish", key: "default_on") == nil,
              "值后紧跟单词字符时跳过")
        check(ManifestEditing.boolLine("enabled = 1", key: "enabled") == nil,
              "非布尔值跳过")

        // CRLF 的 \r 留在行尾。
        if let (_, _, tail) = ManifestEditing.boolLine("enabled = true\r", key: "enabled") {
            check(tail == "\r", "CRLF 的回车留在行尾")
        } else {
            fatalError("FAIL: CRLF 赋值行没有解析出来")
        }

        // 只认 [runtime] 段里的键。
        let twoSections = """
        [plugin]
        id = "demo"
        enabled = true

        [runtime]
        enabled = false
        default_on = true
        """
        check(ManifestEditing.boolValue(twoSections, key: "enabled") == false,
              "[plugin] 段的同名键不干扰读取")
        check(ManifestEditing.boolValue(twoSections, key: "default_on") == true,
              "[runtime] 段的键读到")
        let commentedSection = twoSections.replacingOccurrences(
            of: "[runtime]", with: "[runtime] # 运行时开关"
        )
        check(ManifestEditing.boolValue(commentedSection, key: "default_on") == true,
              "带注释的表头仍能识别 [runtime] 段")

        // 替换保留缩进、注释、CRLF 和末尾换行。
        let crlf = "[plugin]\r\nid = \"demo\"\r\n\r\n[runtime]\r\ndefault_on = false # 保留\r\nenabled = true\r\n"
        let rewrittenCRLF = ManifestEditing.replacingBool(crlf, key: "default_on", value: true)
        check(rewrittenCRLF.contains("default_on = true # 保留"), "替换后行内注释保留")
        check(rewrittenCRLF.components(separatedBy: "\r\n").count
                == crlf.components(separatedBy: "\r\n").count,
              "CRLF 行数不变")
        check(rewrittenCRLF.hasSuffix("\r\n"), "CRLF 末尾换行保留")
        check(rewrittenCRLF.contains("enabled = true\r\n"), "未指定的键与行尾符保持原样")

        let tricky = """
        # default_on = true
        default_on_extra = true
        [runtime]
          default_on = false  # 开关
        enabled = true
        """
        let rewrittenTricky = ManifestEditing.replacingBool(tricky, key: "default_on", value: true)
        check(rewrittenTricky.contains("# default_on = true"), "注释行未被改写")
        check(rewrittenTricky.contains("default_on_extra = true"), "同前缀键未被改写")
        check(rewrittenTricky.contains("  default_on = true  # 开关"), "缩进与注释保留、值换成 true")
        check(rewrittenTricky.contains("enabled = true"), "其他键未被改写")

        // 字段级合并：bundle 的其它字段生效，用户改过的开关保留。
        let bundled = """
        [plugin]
        id = "demo"
        name = "新名字"
        command = "./demo"

        [runtime]
        enabled = true
        default_on = false
        timeout_ms = 8000
        args = []
        """
        let existing = """
        [plugin]
        id = "demo"
        name = "用户自定义名字"
        command = "./demo"

        [runtime]
        enabled = false
        default_on = true
        timeout_ms = 3000
        args = []
        """
        let merged = ManifestEditing.merged(
            bundle: bundled, existing: existing, runtimeKeys: ["default_on", "enabled"]
        )
        check(merged.contains("name = \"新名字\""), "bundle 的名字随升级更新")
        check(merged.contains("timeout_ms = 8000"), "bundle 的超时随升级更新")
        check(ManifestEditing.boolValue(merged, key: "default_on") == true,
              "用户改过的 default_on 保留")
        check(ManifestEditing.boolValue(merged, key: "enabled") == false,
              "用户改过的 enabled 保留")
        check(ManifestEditing.merged(bundle: bundled, existing: nil,
                                     runtimeKeys: ["default_on", "enabled"]) == bundled,
              "目标清单不存在时原样写入 bundle")

        // 组合逻辑：首次落地按凭据判定，之后沿用用户开关，其他插件按 bundle 值。
        let keys = ["default_on", "enabled"]
        let firstInstall = ManifestEditing.resolved(
            bundle: bundled, existing: nil, plugin: "command-code",
            hasCredentials: true, runtimeKeys: keys
        )
        check(ManifestEditing.boolValue(firstInstall, key: "default_on") == true,
              "command-code 首次落地有凭据时默认开")
        let firstInstallNoKey = ManifestEditing.resolved(
            bundle: bundled, existing: nil, plugin: "command-code",
            hasCredentials: false, runtimeKeys: keys
        )
        check(ManifestEditing.boolValue(firstInstallNoKey, key: "default_on") == false,
              "command-code 首次落地没有凭据时默认关")
        let laterStart = ManifestEditing.resolved(
            bundle: bundled, existing: existing, plugin: "command-code",
            hasCredentials: false, runtimeKeys: keys
        )
        check(ManifestEditing.boolValue(laterStart, key: "default_on") == true,
              "清单已存在时沿用用户开关，凭据状态不覆盖它")
        let otherPlugin = ManifestEditing.resolved(
            bundle: bundled, existing: nil, plugin: "clock",
            hasCredentials: true, runtimeKeys: keys
        )
        check(ManifestEditing.boolValue(otherPlugin, key: "default_on") == false,
              "其他插件首次落地沿用 bundle 的 default_on")

        // auth.json 路径跟随 HOME，缺失时回退用户目录。
        let fallback = URL(fileURLWithPath: "/Users/fallback", isDirectory: true)
        check(ManifestEditing.commandCodeAuthPath(env: ["HOME": "/Users/tester"],
                                                  homeDirectory: fallback).path
                == "/Users/tester/.commandcode/auth.json",
              "HOME 存在时按它拼路径")
        check(ManifestEditing.commandCodeAuthPath(env: [:], homeDirectory: fallback).path
                == "/Users/fallback/.commandcode/auth.json",
              "HOME 缺失时回退用户目录")

        // 凭据判定与 auth.rs 对齐。
        let emptyEnv: [String: String] = [:]
        check(ManifestEditing.hasCommandCodeCredentials(
            env: ["COMMAND_CODE_API_KEY": "user_from_env"], authJSON: nil),
            "环境变量非空即为有凭据")
        check(ManifestEditing.hasCommandCodeCredentials(
            env: ["COMMAND_CODE_API_KEY": "   "], authJSON: data("{\"apiKey\":\"user_x\"}")),
            "环境变量空白时回退 auth.json")
        check(!ManifestEditing.hasCommandCodeCredentials(env: emptyEnv, authJSON: nil),
              "两处都没有凭据")
        check(ManifestEditing.hasCommandCodeCredentials(
            env: emptyEnv, authJSON: data("{\"apiKey\":\"  user_x  \"}")),
            "apiKey 前后空白仍算有效")
        check(ManifestEditing.hasCommandCodeCredentials(
            env: emptyEnv,
            authJSON: data("{\"apiKey\":\"user_x\",\"codex\":{\"access\":\"secret\"}}")),
            "文件里另有 OAuth 令牌不影响判定")
        for invalid in [
            "{\"apiKey\":\"\"}",
            "{\"apiKey\":\"   \"}",
            "{\"apiKey\":123}",
            "{\"apiKey\":null}",
            "{\"apiKey\":{\"value\":\"x\"}}",
            "{\"codex\":{\"access\":\"x\"}}",
            "{\"nested\":{\"apiKey\":\"user_x\"}}",
            "not json",
            "[]",
        ] {
            check(!ManifestEditing.hasCommandCodeCredentials(env: emptyEnv, authJSON: data(invalid)),
                  "无效 auth.json 判为没有凭据: \(invalid)")
        }

        print("ManifestEditingTests: \(passed) checks passed")
    }
}
