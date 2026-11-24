import Foundation
import Combine
import ServiceManagement

final class AppState: ObservableObject {
    static let shared = AppState()

    @Published var stayInMenuBar: Bool {
        didSet { defaults.set(stayInMenuBar, forKey: "stayInMenuBar") }
    }
    @Published var launchAtLogin: Bool {
        didSet {
            defaults.set(launchAtLogin, forKey: "launchAtLogin")
            applyLaunchAtLogin(launchAtLogin)
        }
    }

    private let defaults = UserDefaults.standard

    private init() {
        defaults.register(defaults: [
            "stayInMenuBar": true,
            "launchAtLogin": false
        ])
        stayInMenuBar = defaults.bool(forKey: "stayInMenuBar")
        // 以系统登录项的真实状态为准，而不是本地存储
        launchAtLogin = SMAppService.mainApp.status == .enabled
    }

    /// 开关切换时注册/移除系统登录项；失败则回滚开关并记录原因
    private func applyLaunchAtLogin(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.launchAtLogin = SMAppService.mainApp.status == .enabled
                self.lastLaunchError = error.localizedDescription
            }
        }
    }

    /// 最近一次登录项操作失败的原因，为 nil 表示没有错误
    @Published private(set) var lastLaunchError: String?
}
