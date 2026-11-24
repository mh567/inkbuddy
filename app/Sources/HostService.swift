import Foundation
import Combine
import AppKit

struct PluginInfo: Identifiable, Codable {
    let id: String
    let name: String
    let version: String
    let description: String
    var enabled: Bool
    let defaultOn: Bool
    let running: Bool

    enum CodingKeys: String, CodingKey {
        case id, name, version, description, enabled, running
        case defaultOn = "default_on"
    }
}

/// 管理内嵌的 Rust host 进程。host 通过 INKBUDDY_HOME 读取配置、插件和缓存，
/// 全部放在 app 专属的 Application Support 目录，和旧项目解耦。
final class HostService: ObservableObject {
    static let shared = HostService()

    @Published private(set) var running = false
    @Published private(set) var lanAddress = ""
    @Published private(set) var statusText = "服务未启动"
    @Published var plugins: [PluginInfo] = []
    @Published var port: Int {
        didSet {
            UserDefaults.standard.set(port, forKey: "hostPort")
            refreshAddress()
        }
    }
    private var process: Process?
    private var lanHost = "127.0.0.1"
    private let fileManager = FileManager.default

    private var appSupport: URL {
        let base = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return base.appendingPathComponent("inkbuddy", isDirectory: true)
    }
    private var homeURL: URL { appSupport }

    private init() {
        let defaults = UserDefaults.standard
        port = defaults.object(forKey: "hostPort") as? Int ?? 9012
    }

    // MARK: - 生命周期

    func start() {
        guard process == nil else { return }
        // 上次 app 被强杀时 host 可能残留并占着端口，先清掉本 app 的旧进程
        killStaleHostIfNeeded()
        prepareHome()
        migrateLegacyCacheIfNeeded()

        let bundle = Bundle.main.bundleURL.appendingPathComponent("Contents/Resources")
        let host = bundle.appendingPathComponent("inkbuddy-host")

        let task = Process()
        task.executableURL = host
        task.arguments = []
        var env = ProcessInfo.processInfo.environment
        env["INKBUDDY_HOME"] = homeURL.path
        env["PATH"] = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        env["RUST_LOG"] = "info"
        task.environment = env

        let pipe = Pipe()
        task.standardOutput = pipe
        task.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            if data.isEmpty { return }
            if let line = String(data: data, encoding: .utf8) {
                print("[inkbuddy-host] \(line)", terminator: "")
            }
        }

        task.terminationHandler = { [weak self] proc in
            DispatchQueue.main.async {
                self?.process = nil
                self?.running = false
                self?.statusText = "服务已停止（退出码 \(proc.terminationStatus)）"
            }
        }

        do {
            try task.run()
            process = task
            running = true
            statusText = "服务启动中…"
            lanHost = detectLANHost()
            lanAddress = buildAddress(host: lanHost)
            // 等端口就绪后探测状态并拉取插件列表
            DispatchQueue.global().asyncAfter(deadline: .now() + 0.8) { [weak self] in
                self?.probe()
                self?.fetchPlugins()
            }
        } catch {
            statusText = "服务启动失败：\(error.localizedDescription)"
        }
    }

    /// 清理上次强杀 app 后残留的 host（只匹配本 app 资源目录下的进程）
    private func killStaleHostIfNeeded() {
        // 按进程名精确匹配（inkbuddy-host 足够独特），避免 pgrep -f 匹配到命令行
        let task = Process()
        task.launchPath = "/usr/bin/pgrep"
        task.arguments = ["-x", "inkbuddy-host"]
        let pipe = Pipe()
        task.standardOutput = pipe
        try? task.run()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        guard let text = String(data: data, encoding: .utf8) else { return }
        var found = false
        for line in text.split(separator: "\n") {
            guard let pid = Int32(line.trimmingCharacters(in: .whitespaces)) else { continue }
            if pid != ProcessInfo.processInfo.processIdentifier {
                kill(pid, SIGTERM)
                found = true
            }
        }
        if found { Thread.sleep(forTimeInterval: 0.3) }
    }

    func stop() {
        process?.terminate()
        process = nil
        running = false
        statusText = "服务已停止"
    }

    func restart() {
        stop()
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) { [weak self] in
            DispatchQueue.main.async { self?.start() }
        }
    }

    // MARK: - 准备运行时目录

    private func prepareHome() {
        let plugins = homeURL.appendingPathComponent("plugins", isDirectory: true)
        try? fileManager.createDirectory(at: plugins, withIntermediateDirectories: true)
        let bundlePlugins = Bundle.main.bundleURL
            .appendingPathComponent("Contents/Resources/plugins", isDirectory: true)

        // 每次启动都用 bundle 里的版本覆盖，保证 app 升级后插件同步更新
        for name in ["lyrics", "clock"] {
            let src = bundlePlugins.appendingPathComponent(name, isDirectory: true)
            let dst = plugins.appendingPathComponent(name, isDirectory: true)
            try? fileManager.createDirectory(at: dst, withIntermediateDirectories: true)
            if let files = try? fileManager.contentsOfDirectory(at: src, includingPropertiesForKeys: nil) {
                for file in files {
                    let target = dst.appendingPathComponent(file.lastPathComponent)
                    try? fileManager.removeItem(at: target)
                    try? fileManager.copyItem(at: file, to: target)
                }
            }
        }

        // 写配置（端口来自设置）
        let config = """
        [server]
        bind = "0.0.0.0"
        port = \(port)

        [display]
        title = "InkBuddy"
        refresh_ms = 500
        """
        try? config.write(to: homeURL.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
    }

    /// 从旧的 ~/.inkbuddy/cache 迁移歌词缓存，避免升级后缓存丢失
    private func migrateLegacyCacheIfNeeded() {
        let legacy = fileManager.homeDirectoryForCurrentUser
            .appendingPathComponent(".inkbuddy/cache", isDirectory: true)
        let target = homeURL.appendingPathComponent("cache", isDirectory: true)
        guard fileManager.fileExists(atPath: legacy.path),
              !fileManager.fileExists(atPath: target.path) else { return }
        try? fileManager.copyItem(at: legacy, to: target)
    }

    // MARK: - 状态探测

    private func probe() {
        var request = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/api/health")!)
        request.timeoutInterval = 2
        URLSession.shared.dataTask(with: request) { [weak self] data, response, _ in
            DispatchQueue.main.async {
                guard let self = self else { return }
                if let http = response as? HTTPURLResponse, http.statusCode == 200, data != nil {
                    self.statusText = "运行中"
                } else {
                    self.statusText = "服务未响应，请检查端口"
                }
            }
        }.resume()
    }

    private func detectLANHost() -> String {
        var candidates: [String] = []
        for i in 0...9 {
            let task = Process()
            task.launchPath = "/usr/sbin/ipconfig"
            task.arguments = ["getifaddr", "en\(i)"]
            let pipe = Pipe()
            task.standardOutput = pipe
            try? task.run()
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            if let ip = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
               !ip.isEmpty, !ip.hasPrefix("127.") {
                candidates.append(ip)
            }
        }
        candidates.sort { a, b in rank(a) < rank(b) }
        return candidates.first ?? "127.0.0.1"
    }

    /// 生成墨水屏地址。显示设置走服务端同步，地址保持简洁，不带参数。
    private func buildAddress(host: String) -> String {
        return "http://\(host):\(port)/"
    }

    private func refreshAddress() {
        lanAddress = buildAddress(host: lanHost)
    }

    private func rank(_ ip: String) -> Int {
        let parts = ip.split(separator: ".").compactMap { Int($0) }
        guard parts.count == 4 else { return 9 }
        if parts[0] == 192 && parts[1] == 168 { return 0 }
        if parts[0] == 10 { return 1 }
        if parts[0] == 172 && (16...31).contains(parts[1]) { return 2 }
        return 3
    }

    // MARK: - 扩展管理

    /// 从 host 拉取插件列表，用服务端返回的 enabled 作为开关状态。
    func fetchPlugins() {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/plugins") else { return }
        var request = URLRequest(url: url)
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            guard let data = data,
                  let list = try? JSONDecoder().decode([PluginInfo].self, from: data) else { return }
            DispatchQueue.main.async { self?.plugins = list }
        }.resume()
    }

    /// 切换某个扩展的启用状态，推送完整的启用列表给 host。
    func setPlugin(_ id: String, enabled: Bool) {
        guard let index = plugins.firstIndex(where: { $0.id == id }) else { return }
        plugins[index].enabled = enabled
        let ids = plugins.filter { $0.enabled }.map { $0.id }
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/plugins/active") else { return }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["ids": ids])
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] _, response, _ in
            // 推送失败时回滚开关，避免界面和服务端状态不一致
            let ok = (response as? HTTPURLResponse)?.statusCode == 200
            if !ok {
                DispatchQueue.main.async {
                    if let index = self?.plugins.firstIndex(where: { $0.id == id }) {
                        self?.plugins[index].enabled = !enabled
                    }
                }
            }
        }.resume()
    }

    func copyAddress() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(lanAddress, forType: .string)
    }
}
