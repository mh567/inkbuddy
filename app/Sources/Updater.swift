import Foundation
import AppKit
import Combine

/// 从 GitHub Release 检查新版本并一键更新。
/// 更新流程：下载 dmg → 挂载并拷出 app 校验 → 写一段等待脚本并启动 →
/// 退出 app → 脚本等进程退干净后换掉旧 app 并重新打开。
final class Updater: ObservableObject {
    static let shared = Updater()
    private static let repo = "mh567/inkbuddy"

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
        let url = URL(string: "https://api.github.com/repos/\(Self.repo)/releases/latest")!
        var req = URLRequest(url: url)
        req.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        req.timeoutInterval = 10
        URLSession.shared.dataTask(with: req) { [weak self] data, resp, err in
            DispatchQueue.main.async {
                guard let self else { return }
                guard let data, err == nil,
                      let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let tag = obj["tag_name"] as? String,
                      let assets = obj["assets"] as? [[String: Any]] else {
                    self.state = .failed("无法连接 GitHub")
                    return
                }
                let latest = tag.hasPrefix("v") ? String(tag.dropFirst()) : tag
                guard Self.isNewer(latest, than: self.currentVersion),
                      let asset = assets.first(where: { ($0["name"] as? String)?.hasPrefix("InkBuddy-macOS") == true }),
                      let raw = asset["browser_download_url"] as? String,
                      let url = URL(string: raw) else {
                    self.state = .upToDate
                    return
                }
                self.assetURL = url
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
