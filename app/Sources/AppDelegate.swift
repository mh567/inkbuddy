import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSMenuDelegate {
    private var statusItem: NSStatusItem?
    private var mainWindow: NSWindow?
    private let preview = MenuPreviewController()
    private var menuIsOpen = false
    /// 每次打开菜单/切换视图都递增，异步回调据此丢弃上一周期的响应。
    private var menuGeneration = 0
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

    // MARK: - 菜单栏

    private func buildStatusBar() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let button = item.button {
            let image = NSImage(named: "MenuBarIcon")
            image?.isTemplate = true
            // 固定逻辑尺寸，避免 NSImage 按像素尺寸渲染撑高菜单栏
            image?.size = NSSize(width: 17, height: 17)
            button.image = image
            button.toolTip = "inkbuddy"
        }

        let menu = NSMenu()
        menu.delegate = self

        // 预览头部：◀ / 当前视图名 / ▶ + 墨水屏页面实时预览
        let header = NSMenuItem()
        header.view = preview.containerView
        menu.addItem(header)
        menu.addItem(NSMenuItem.separator())

        // 菜单只有预览头部（◀/视图名/▶）+ 设置/退出；视图的应用与管理在设置窗口
        menu.addItem(makeItem(title: "设置", action: #selector(showMainWindow)))
        menu.addItem(makeItem(title: "退出 inkbuddy", action: #selector(quitApp), key: "q"))

        item.menu = menu
        statusItem = item

        preview.onPrev = { [weak self] in self?.cycleView(-1) }
        preview.onNext = { [weak self] in self?.cycleView(1) }
    }

    private func makeItem(title: String, action: Selector, key: String = "") -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.target = self
        return item
    }

    private func viewName() -> String {
        HostService.shared.currentViewName ?? "未选择视图"
    }

    // MARK: - 菜单打开/关闭

    func menuWillOpen(_ menu: NSMenu) {
        menuIsOpen = true
        menuGeneration += 1
        let generation = menuGeneration
        preview.setName(viewName())
        // 画布要画当前视图的区域布局，先拉最新视图状态再启动预览，
        // 否则首次打开菜单会用旧布局。回调里再确认菜单仍开着，避免
        // 超时慢响应在菜单关闭后才把轮询启动起来。
        HostService.shared.fetchPlugins()
        HostService.shared.fetchViews { [weak self] in
            guard let self = self, self.menuIsOpen, generation == self.menuGeneration else { return }
            self.preview.setName(self.viewName())
            self.preview.startPreview()
        }
    }

    func menuDidClose(_ menu: NSMenu) {
        menuIsOpen = false
        // 关闭后停掉预览页面的轮询
        preview.stopPreview()
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
        host.switchView(to: views[next].id) { [weak self] in
            guard let self = self, self.menuIsOpen, generation == self.switchGeneration else { return }
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

    @objc private func quitApp() {
        NSApp.terminate(nil)
    }
}
