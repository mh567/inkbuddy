import Foundation
import Combine
import AppKit

/// 一个扩展。enabled 是用户的启用开关：停用只是本地标记，扩展文件仍在
/// 磁盘上，渲染时跳过、子进程停掉。available 是 manifest 的构建级开关，
/// 为 false 时这款扩展在本机用不了。

/// 插件声明的一个子样式，id 传给插件的 render 请求，name 用于界面展示。
struct PluginVariant: Identifiable, Codable, Hashable {
    let id: String
    let name: String
}

struct PluginInfo: Identifiable, Codable {
    let id: String
    let name: String
    let version: String
    let description: String
    var enabled: Bool
    let available: Bool
    let defaultOn: Bool
    let running: Bool
    /// 插件提供的子样式，缺省为空数组。
    var variants: [PluginVariant]

    enum CodingKeys: String, CodingKey {
        case id, name, version, description, enabled, available, running, variants
        case defaultOn = "default_on"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        version = try c.decode(String.self, forKey: .version)
        description = try c.decode(String.self, forKey: .description)
        enabled = try c.decode(Bool.self, forKey: .enabled)
        available = try c.decode(Bool.self, forKey: .available)
        defaultOn = try c.decode(Bool.self, forKey: .defaultOn)
        running = try c.decode(Bool.self, forKey: .running)
        variants = try c.decodeIfPresent([PluginVariant].self, forKey: .variants) ?? []
    }

    /// setEnabled 里重建对象时用：保留已有字段，只改 enabled / running。
    init(id: String, name: String, version: String, description: String,
         enabled: Bool, available: Bool, defaultOn: Bool, running: Bool,
         variants: [PluginVariant] = []) {
        self.id = id
        self.name = name
        self.version = version
        self.description = description
        self.enabled = enabled
        self.available = available
        self.defaultOn = defaultOn
        self.running = running
        self.variants = variants
    }
}

/// 一个屏幕区域：把某个扩展放到屏幕的哪个位置、占多大。
/// x/y 是左上角，w/h 是宽高，全部为屏幕百分比（0-100）。
/// variant 是该区域选用的插件子样式；nil 表示插件自己决定默认。
struct ViewRegion: Codable, Equatable, Identifiable {
    var plugin: String
    var x: Double
    var y: Double
    var w: Double
    var h: Double
    var variant: String?

    var id: String { plugin }

    /// 菜单预览的竖排区域用这个构造器，variant 取默认值。
    init(plugin: String, x: Double, y: Double, w: Double, h: Double, variant: String? = nil) {
        self.plugin = plugin
        self.x = x
        self.y = y
        self.w = w
        self.h = h
        self.variant = variant
    }

    /// 预览缓存的 key。与 host 的 preview_key 规则一致。
    var previewKey: String {
        if let variant = variant, !variant.isEmpty {
            return "\(plugin):\(variant)"
        }
        return plugin
    }
}

/// 一个具名视图：扩展的区域布局。对应 host 的 /api/views。
struct SavedView: Identifiable, Codable {
    let id: String
    let name: String
    var regions: [ViewRegion]
}

/// host 的视图状态：当前视图指针 + 已保存视图列表。
struct ViewsState: Codable {
    var current: String?
    var views: [SavedView]
}

/// /api/health 里的一台在线设备：页面生成的设备 id、实测视口宽高比
/// （宽/高）、上次上报时刻的 Unix 秒。
struct DeviceReport: Identifiable, Decodable, Equatable {
    let id: String
    let aspect: Double
    let seen: Double
}

/// /api/health 的负载：60 秒内上报过的设备清单。
private struct HealthInfo: Decodable {
    let devices: [DeviceReport]?
}

/// 管理内嵌的 Rust host 进程。host 通过 INKBUDDY_HOME 读取配置、插件和缓存，
/// 全部放在 app 专属的 Application Support 目录，和旧项目解耦。
final class HostService: ObservableObject {
    static let shared = HostService()

    @Published private(set) var running = false
    @Published private(set) var lanAddress = ""
    @Published private(set) var statusText = "服务未启动"
    @Published var plugins: [PluginInfo] = []
    @Published var viewsState: ViewsState = ViewsState(current: nil, views: [])
    /// 60 秒内上报过的设备清单。每台设备各自的比例互不覆盖，
    /// 控制端明确选择某台后跟随它。nil 等价于空列表。
    @Published private(set) var devices: [DeviceReport] = []
    /// 编辑器/菜单预览跟随的设备 id；nil = 画布用手选比例。
    /// 持久化到 UserDefaults，重启后仍记住。
    @Published var followDeviceId: String? {
        didSet {
            if let id = followDeviceId {
                UserDefaults.standard.set(id, forKey: "followDeviceId")
            } else {
                UserDefaults.standard.removeObject(forKey: "followDeviceId")
            }
        }
    }
    @Published var port: Int {
        didSet {
            UserDefaults.standard.set(port, forKey: "hostPort")
            refreshAddress()
        }
    }
    /// 设备清单请求序号，旧响应到达时丢弃。
    private var devicesSeq = 0
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
        followDeviceId = defaults.string(forKey: "followDeviceId")
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
            // 等端口就绪后探测状态并拉取扩展/视图列表
            DispatchQueue.global().asyncAfter(deadline: .now() + 0.8) { [weak self] in
                self?.probe()
                self?.fetchPlugins()
                self?.fetchViews()
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

        // 二进制与辅助文件每次启动都用 bundle 里的版本覆盖，保证 app 升级后插件同步更新。
        // plugin.toml 每次启动用 bundle 版本打底，用户改过的 default_on / enabled 保留。
        // 按目录枚举，bundle 里新增的插件由这段自动带到运行时目录。
        let bundled = (try? fileManager.contentsOfDirectory(
            at: bundlePlugins,
            includingPropertiesForKeys: [.isDirectoryKey]
        )) ?? []
        for src in bundled.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
            guard (try? src.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true else {
                continue
            }
            let name = src.lastPathComponent
            let dst = plugins.appendingPathComponent(name, isDirectory: true)
            try? fileManager.createDirectory(at: dst, withIntermediateDirectories: true)
            guard let files = try? fileManager.contentsOfDirectory(at: src, includingPropertiesForKeys: nil) else {
                NSLog("InkBuddy: 读取 bundle 插件目录失败 \(src.path)")
                continue
            }
            for file in files {
                let target = dst.appendingPathComponent(file.lastPathComponent)
                if file.lastPathComponent == "plugin.toml" {
                    syncManifest(bundle: file, target: target, plugin: name)
                    continue
                }
                try? fileManager.removeItem(at: target)
                do {
                    try fileManager.copyItem(at: file, to: target)
                } catch {
                    NSLog("InkBuddy: 复制插件文件失败 \(file.lastPathComponent): \(error.localizedDescription)")
                }
            }
            if name == "lyrics" {
                for helper in ["MediaRemoteMini.dylib", "mediaremote-mini.pl"] {
                    if !fileManager.fileExists(atPath: dst.appendingPathComponent(helper).path) {
                        NSLog("InkBuddy: 歌词插件缺少 \(helper)")
                    }
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

    /// 清单每次启动用 bundle 版本写入，名字、命令路径、超时等字段随 app 升级更新。
    /// default_on 与 enabled 是用户可改的运行时开关，沿用目标文件里已有的值。
    /// command-code 首次落地时按本机凭据判定 default_on。
    private func syncManifest(bundle: URL, target: URL, plugin: String) {
        guard let bundled = try? String(contentsOf: bundle, encoding: .utf8) else { return }
        let exists = fileManager.fileExists(atPath: target.path)
        let existing: String?
        if exists {
            // 文件存在却读不出来时直接收手，保留用户改过的开关。
            guard let text = try? String(contentsOf: target, encoding: .utf8) else { return }
            existing = text
        } else {
            existing = nil
        }
        let text = ManifestEditing.resolved(
            bundle: bundled,
            existing: existing,
            plugin: plugin,
            hasCredentials: commandCodeHasCredentials(),
            runtimeKeys: ["default_on", "enabled"]
        )
        try? text.write(to: target, atomically: true, encoding: .utf8)
    }

    /// 判定本机是否有 Command Code 凭据，口径与插件 auth.rs 一致。
    private func commandCodeHasCredentials() -> Bool {
        let env = ProcessInfo.processInfo.environment
        let auth = ManifestEditing.commandCodeAuthPath(
            env: env,
            homeDirectory: fileManager.homeDirectoryForCurrentUser
        )
        return ManifestEditing.hasCommandCodeCredentials(
            env: env,
            authJSON: try? Data(contentsOf: auth)
        )
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
                if let http = response as? HTTPURLResponse, http.statusCode == 200, let data = data {
                    self.statusText = "运行中"
                    if let health = try? JSONDecoder().decode(HealthInfo.self, from: data) {
                        self.devices = health.devices ?? []
                    }
                } else {
                    self.statusText = "服务未响应，请检查端口"
                }
            }
        }.resume()
    }

    /// 拉取在线设备清单，完成后主线程回调。编辑器与菜单预览用它
    /// 刷新设备列表并跟随所选设备的实测比例。
    func refreshDevices(completion: (([DeviceReport]) -> Void)? = nil) {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/health") else { return }
        devicesSeq += 1
        let seq = devicesSeq
        var request = URLRequest(url: url)
        request.timeoutInterval = 2
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            let devices = (data.flatMap { try? JSONDecoder().decode(HealthInfo.self, from: $0).devices }) ?? []
            DispatchQueue.main.async {
                // 设备清单是全局共享状态，旧响应晚到会把新列表覆盖回去。
                guard let self = self, seq == self.devicesSeq else { return }
                self.devices = devices
                completion?(devices)
            }
        }.resume()
    }

    /// 当前跟随目标：所选设备仍在在线清单里时返回它的上报；
    /// 设备离线时返回 nil，调用方保持当前值。
    var followTarget: DeviceReport? {
        devices.first { $0.id == followDeviceId }
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

    // MARK: - 扩展管理（启用/停用）

    /// 从 host 拉取扩展列表。
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

    /// 启用/停用一个扩展。停用只打标记并停掉子进程，文件仍在扩展目录。
    func setEnabled(_ id: String, enabled: Bool, completion: ((Bool) -> Void)? = nil) {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/plugins/\(id)/enabled") else { return }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["enabled": enabled])
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] _, response, _ in
            let ok = (response as? HTTPURLResponse)?.statusCode == 200
            DispatchQueue.main.async {
                if ok, let self = self,
                   let index = self.plugins.firstIndex(where: { $0.id == id }) {
                    let old = self.plugins[index]
                    self.plugins[index] = PluginInfo(
                        id: old.id,
                        name: old.name,
                        version: old.version,
                        description: old.description,
                        enabled: enabled,
                        available: old.available,
                        defaultOn: old.defaultOn,
                        running: enabled ? old.running : false,
                        variants: old.variants
                    )
                } else if !ok {
                    // 失败时重拉，保证界面与服务端一致
                    self?.fetchPlugins()
                }
                completion?(ok)
                if ok {
                    // host 对扩展是懒启动：/api/plugins 的 describe 会拉起子进程
                    // 并把 running 置为 true。启用后立刻重拉一次，稍后再补一次，
                    // 避免列表一直停留在「未运行」。
                    self?.fetchPlugins()
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
                        self?.fetchPlugins()
                    }
                }
            }
        }.resume()
    }

    // MARK: - 视图管理

    /// 从 host 拉取视图列表。完成后在主线程回调，便于菜单立即重建。
    func fetchViews(completion: (() -> Void)? = nil) {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/views") else { return }
        var request = URLRequest(url: url)
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            guard let self = self, let data = data,
                  let state = try? JSONDecoder().decode(ViewsState.self, from: data) else {
                if let completion = completion {
                    DispatchQueue.main.async { completion() }
                }
                return
            }
            DispatchQueue.main.async {
                self.viewsState = state
                completion?()
            }
        }.resume()
    }

    /// 应用一个视图：显示跟随该视图的区域布局，下一次取帧即生效。
    func switchView(to id: String?, completion: (() -> Void)? = nil) {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/views/current") else { return }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        // id 为 nil 时必须显式给 NSNull：Optional 装箱进 JSONSerialization
        // 会抛 NSException（try? 捕获不了），进程直接崩溃。
        let payload: [String: Any] = id.map { ["id": $0] } ?? ["id": NSNull()]
        request.httpBody = try? JSONSerialization.data(withJSONObject: payload)
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] _, response, _ in
            let ok = (response as? HTTPURLResponse)?.statusCode == 200
            DispatchQueue.main.async {
                if ok, let self = self {
                    self.viewsState.current = id
                }
                completion?()
            }
        }.resume()
    }

    /// 整体保存视图列表（host 的 POST /api/views 是全量替换语义）。
    /// 成功后本地列表同步。
    func saveViews(_ views: [SavedView], completion: ((Bool) -> Void)? = nil) {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/views") else { return }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let payload: [String: Any] = [
            "views": views.map { view in
                [
                    "id": view.id,
                    "name": view.name,
                    "regions": view.regions.map { r in
                        var region: [String: Any] = [
                            "plugin": r.plugin, "x": r.x, "y": r.y, "w": r.w, "h": r.h
                        ]
                        // nil 不写这个键，host 侧按缺省处理。
                        if let variant = r.variant, !variant.isEmpty {
                            region["variant"] = variant
                        }
                        return region
                    }
                ]
            }
        ]
        request.httpBody = try? JSONSerialization.data(withJSONObject: payload)
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] _, response, _ in
            let ok = (response as? HTTPURLResponse)?.statusCode == 200
            DispatchQueue.main.async {
                if ok {
                    self?.viewsState.views = views
                }
                completion?(ok)
            }
        }.resume()
    }

    /// 所有视图变更都走「重拉 → 修改 → 写回」。POST /api/views 是全量替换
    /// 语义，直接用本地 viewsState 写会把其它端新建的视图整份覆盖掉。
    private func mutateViews(
        _ mutate: @escaping (inout ViewsState) -> Bool,
        completion: ((Bool) -> Void)? = nil
    ) {
        fetchViews { [weak self] in
            guard let self = self else {
                completion?(false)
                return
            }
            var state = self.viewsState
            guard mutate(&state) else {
                completion?(false)
                return
            }
            self.saveViews(state.views) { ok in
                if ok {
                    self.viewsState.current = state.current
                }
                completion?(ok)
            }
        }
    }

    /// 新建视图。不自动应用；应用是显式动作。
    func createView(_ view: SavedView, completion: ((Bool) -> Void)? = nil) {
        mutateViews({ state in
            state.views.append(view)
            return true
        }, completion: completion)
    }

    /// 更新已有视图（重命名/改布局）。若该视图是当前视图，保存后
    /// 重新应用一次，让墨水屏显示与视图内容保持一致。
    func updateView(_ view: SavedView, completion: ((Bool) -> Void)? = nil) {
        mutateViews({ state in
            guard let index = state.views.firstIndex(where: { $0.id == view.id }) else {
                return false
            }
            state.views[index] = view
            return true
        }) { [weak self] ok in
            guard ok else {
                completion?(false)
                return
            }
            // 编辑的是当前应用的视图时重新应用一次，保持显示同步。
            if self?.viewsState.current == view.id {
                self?.switchView(to: view.id) { completion?(true) }
            } else {
                completion?(true)
            }
        }
    }

    /// 删除一个视图。当前视图被删除时 host 会自动清掉指针，本地同步。
    func deleteView(_ id: String, completion: ((Bool) -> Void)? = nil) {
        mutateViews({ state in
            let before = state.views.count
            state.views.removeAll { $0.id == id }
            guard state.views.count < before else { return false }
            if state.current == id {
                state.current = nil
            }
            return true
        }, completion: completion)
    }

    // MARK: - 辅助

    /// 扩展的显示名；已停用的扩展返回 nil。
    func pluginName(for id: String) -> String? {
        plugins.first { $0.id == id && $0.enabled }?.name
    }

    /// 视图里引用了但本机已停用的扩展 id。
    func missingPluginIds(in view: SavedView) -> [String] {
        view.regions.map { $0.plugin }.filter { pluginName(for: $0) == nil }
    }

    /// 当前视图的显示名；无视图时返回 nil。
    var currentViewName: String? {
        guard let id = viewsState.current else { return nil }
        return viewsState.views.first { $0.id == id }?.name
    }

    func copyAddress() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(lanAddress, forType: .string)
    }
}
