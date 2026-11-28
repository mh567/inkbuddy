import Foundation
import AppKit
import Combine

/// 从 GitHub Release 检查新版本并一键更新。
/// 更新流程：下载 dmg → 挂载并拷出 app 校验 → 写一段等待脚本并启动 →
/// 退出 app → 脚本等进程退干净后换掉旧 app 并重新打开。
final class Updater: ObservableObject {
    static let shared = Updater()
    private static let repo = "mh567/inkbuddy"
    /// 发版脚本固定产出的 dmg 名，见 docs/release.md
    private static let assetName = "InkBuddy-macOS-arm64.dmg"

    enum State: Equatable {
        case idle
        case checking
        case upToDate
        case available(version: String)
        case downloading
        case installing
        case failed(String)
    }

    @Published private(set) var state: State = .idle

    var currentVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0"
    }

    private var assetURL: URL?
    private var releaseVersion = ""

    func check() {
        guard state != .checking, state != .downloading, state != .installing else { return }
        state = .checking
        // 走网页端的 latest 跳转，从重定向后的地址里取 tag。
        // api.github.com 对未认证请求按出口 IP 限 60 次/小时，打满后一直返回 403，
        // 更新检查会长期失败；网页端这条路径不吃配额。
        let url = URL(string: "https://github.com/\(Self.repo)/releases/latest")!
        var req = URLRequest(url: url)
        req.timeoutInterval = 10
        URLSession.shared.dataTask(with: req) { [weak self] _, resp, err in
            DispatchQueue.main.async {
                guard let self else { return }
                if let err {
                    self.state = .failed(Self.describe(err))
                    return
                }
                guard let http = resp as? HTTPURLResponse else {
                    self.state = .failed("无法连接 GitHub")
                    return
                }
                guard http.statusCode == 200 else {
                    self.state = .failed("获取版本失败（HTTP \(http.statusCode)）")
                    return
                }
                guard let tag = Self.releaseTag(from: http.url) else {
                    self.state = .failed("没有找到发布版本")
                    return
                }
                let latest = tag.hasPrefix("v") ? String(tag.dropFirst()) : tag
                guard Self.isNewer(latest, than: self.currentVersion) else {
                    self.state = .upToDate
                    return
                }
                guard let asset = URL(string:
                        "https://github.com/\(Self.repo)/releases/download/\(tag)/\(Self.assetName)") else {
                    self.state = .failed("下载地址无效")
                    return
                }
                self.assetURL = asset
                self.releaseVersion = latest
                self.state = .available(version: latest)
            }
        }.resume()
    }

    func install() {
        guard let assetURL else { return }
        state = .downloading
        URLSession.shared.dataTask(with: assetURL) { [weak self] data, resp, err in
            guard let self else { return }
            let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
            guard let data, err == nil, code == 200 else {
                DispatchQueue.main.async { self.state = .failed("下载失败 (HTTP \(code))") }
                return
            }
            DispatchQueue.main.async { self.state = .installing }
            DispatchQueue.global(qos: .userInitiated).async {
                self.applyUpdate(payload: data)
            }
        }.resume()
    }

    // MARK: - 安装

    private func applyUpdate(payload: Data) {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory
            .appendingPathComponent("inkbuddy-update-\(UUID().uuidString)")
        do {
            try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
            let dmg = tmp.appendingPathComponent("InkBuddy.dmg")
            try payload.write(to: dmg)

            // 挂载 dmg，拷出 app，卸载
            let mount = tmp.appendingPathComponent("Mount")
            try fm.createDirectory(at: mount, withIntermediateDirectories: true)
            let attach = Process()
            attach.launchPath = "/usr/bin/hdiutil"
            attach.arguments = ["attach", dmg.path, "-nobrowse", "-readonly",
                                "-mountpoint", mount.path, "-quiet"]
            try attach.run()
            attach.waitUntilExit()
            guard attach.terminationStatus == 0 else {
                throw NSError(domain: "mount", code: 1)
            }
            let mountedApp = mount.appendingPathComponent("InkBuddy.app")
            let newApp = tmp.appendingPathComponent("InkBuddy.app")
            try fm.copyItem(at: mountedApp, to: newApp)
            let detach = Process()
            detach.launchPath = "/usr/bin/hdiutil"
            detach.arguments = ["detach", mount.path, "-quiet"]
            try detach.run()
            detach.waitUntilExit()

            guard fm.fileExists(atPath: newApp.appendingPathComponent("Contents/MacOS/InkBuddy").path) else {
                throw NSError(domain: "bundle", code: 2)
            }

            let appPath = Bundle.main.bundleURL.path
            let script = """
            #!/bin/sh
            # 等主程序与内嵌 host 退干净（按进程名精确匹配，避免误杀）
            while pgrep -x InkBuddy >/dev/null 2>&1; do sleep 0.3; done
            while pgrep -x inkbuddy-host >/dev/null 2>&1; do sleep 0.3; done
            rm -rf '\(appPath).old'
            mv '\(appPath)' '\(appPath).old'
            mv '\(newApp.path)' '\(appPath)' && open '\(appPath)' \
              || { mv '\(appPath).old' '\(appPath)'; open '\(appPath)'; }
            rm -rf '\(appPath).old'
            """
            let scriptPath = tmp.appendingPathComponent("update.sh").path
            try script.write(toFile: scriptPath, atomically: true, encoding: .utf8)
            try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: scriptPath)

            let sh = Process()
            sh.launchPath = "/bin/sh"
            sh.arguments = [scriptPath]
            sh.standardOutput = FileHandle.nullDevice
            sh.standardError = FileHandle.nullDevice
            try sh.run()

            DispatchQueue.main.async { NSApp.terminate(nil) }
        } catch {
            DispatchQueue.main.async { self.state = .failed("安装失败：\(error.localizedDescription)") }
        }
    }

    // MARK: - 版本比较

    /// 从重定向后的 URL 里取 tag，形如 .../releases/tag/v0.3.18 或 .../tag/v0.3.18
    static func releaseTag(from url: URL?) -> String? {
        guard let url else { return nil }
        let parts = url.pathComponents
        guard let i = parts.firstIndex(of: "tag"), i + 1 < parts.count else { return nil }
        let tag = parts[i + 1]
        return tag.isEmpty ? nil : tag
    }

    /// 把网络错误分成人话，避免一律报成「无法连接 GitHub」
    static func describe(_ error: Error) -> String {
        let ns = error as NSError
        guard ns.domain == NSURLErrorDomain else {
            return "检查更新失败：\(ns.localizedDescription)"
        }
        switch ns.code {
        case NSURLErrorNotConnectedToInternet:
            return "本机网络未连接"
        case NSURLErrorTimedOut:
            return "连接 GitHub 超时"
        case NSURLErrorCannotFindHost, NSURLErrorDNSLookupFailed:
            return "无法解析 github.com，检查 DNS 或代理设置"
        case NSURLErrorCannotConnectToHost:
            return "无法连接 github.com，检查网络或代理设置"
        case NSURLErrorAppTransportSecurityRequiresSecureConnection:
            return "连接被系统安全策略拦截"
        default:
            return "无法连接 GitHub（\(ns.code)）"
        }
    }

    /// 逐段比较语义化版本（1.2.3），段数不同按 0 补齐；无法解析时退到字符串比较
    private static func isNewer(_ a: String, than b: String) -> Bool {
        let av = a.split(separator: ".").map { Int($0) ?? 0 }
        let bv = b.split(separator: ".").map { Int($0) ?? 0 }
        let n = max(av.count, bv.count)
        for i in 0..<n {
            let x = i < av.count ? av[i] : 0
            let y = i < bv.count ? bv[i] : 0
            if x != y { return x > y }
        }
        return false
    }
}
