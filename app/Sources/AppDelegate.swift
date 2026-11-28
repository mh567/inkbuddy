import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var statusItem: NSStatusItem?
    private var mainWindow: NSWindow?
    private let preview = MenuPreviewController()
    /// 每次打开面板/切换视图都递增，异步回调据此丢弃上一周期的响应。
    private var panelGeneration = 0
    private var switchGeneration = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildStatusBar()
        buildMainWindow()
        HostService.shared.start()
    }

    func applicationWillTerminate(_ notification: Notification) {
        HostService.shared.stop()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showMainWindow()
        return true
    }

    // MARK: - 状态栏

    private func buildStatusBar() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let button = item.button {
            let image = NSImage(named: "MenuBarIcon")
            image?.isTemplate = true
            // 固定逻辑尺寸，避免 NSImage 按像素尺寸渲染撑高菜单栏
            image?.size = NSSize(width: 17, height: 17)
            button.image = image
            button.toolTip = "inkbuddy"
            button.target = self
            button.action = #selector(togglePreview)
            button.sendAction(on: [.leftMouseUp])
        }
        statusItem = item

        preview.onPrev = { [weak self] in self?.cycleView(-1) }
        preview.onNext = { [weak self] in self?.cycleView(1) }
        preview.onSettings = { [weak self] in
            self?.preview.close()
            self?.showMainWindow()
        }
        preview.onQuit = { NSApp.terminate(nil) }
    }

    /// 点状态栏图标：面板没开就打开，已经开着就收起。
    @objc private func togglePreview() {
        if preview.isShown {
            preview.close()
            return
        }
        guard let button = statusItem?.button else { return }
        panelGeneration += 1
        let generation = panelGeneration
        preview.setName(viewName())
        // 先按已有状态弹出并开始取数，再拉最新视图/插件，返回后刷新布局。
        preview.show(relativeTo: button)
        HostService.shared.fetchPlugins()
        HostService.shared.fetchViews { [weak self] in
            guard let self = self, self.preview.isShown, generation == self.panelGeneration else { return }
            self.preview.setName(self.viewName())
            self.preview.refreshLayout()
        }
    }

    private func viewName() -> String {
        HostService.shared.currentViewName ?? "未选择视图"
    }

    // MARK: - 视图操作

    /// 按 direction（-1 上一个 / 1 下一个）在已保存视图间循环应用。
    private func cycleView(_ direction: Int) {
        let host = HostService.shared
        let views = host.viewsState.views
        guard !views.isEmpty else { return }
        switchGeneration += 1
        let generation = switchGeneration
        let next: Int
        if let index = views.firstIndex(where: { $0.id == host.viewsState.current }) {
            next = (index + direction + views.count) % views.count
        } else {
            next = direction > 0 ? 0 : views.count - 1
        }
        let target = views[next]
        // 反馈要在点击瞬间出现：名字先换成目标视图、画布先清空，
        // 网络往返和首次渲染期间都显示「正在加载」。
        self.preview.prepareSwitch(name: target.name)
        // 顺手把所有启用的插件拉起来（describe 即懒启动），
        // 新视图首次渲染就不用等子进程逐个冷启动。
        host.fetchPlugins()
        host.switchView(to: target.id) { [weak self] in
            guard let self = self, self.preview.isShown, generation == self.switchGeneration else { return }
            self.preview.setName(self.viewName())
            // 视图已切换，画布立刻换成新布局和它的内容
            self.preview.refreshLayout()
        }
    }

    // MARK: - 主窗口

    private func buildMainWindow() {
        let rootView = SettingsView()
            .environmentObject(AppState.shared)
            .environmentObject(HostService.shared)
            .environmentObject(Updater.shared)
        let hosting = NSHostingView(rootView: rootView)
        hosting.frame = NSRect(x: 0, y: 0, width: 440, height: 560)

        let window = NSWindow(
            contentRect: hosting.frame,
            styleMask: [.titled, .closable, .miniaturizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.title = "inkbuddy"
        window.titlebarAppearsTransparent = true
        window.contentView = hosting
        window.center()
        window.delegate = self
        window.isReleasedWhenClosed = false
        window.minSize = NSSize(width: 420, height: 460)
        mainWindow = window

        NSApp.setActivationPolicy(.regular)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        if AppState.shared.stayInMenuBar {
            // 勾选时只隐藏窗口，服务继续运行
            sender.orderOut(nil)
            NSApp.setActivationPolicy(.accessory)
            return false
        }
        // 不勾选时真正关闭窗口并退出 app（服务在 willTerminate 中停止）
        return true
    }

    @objc private func showMainWindow() {
        NSApp.setActivationPolicy(.regular)
        mainWindow?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}
