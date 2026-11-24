import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSMenuDelegate {
    private var statusItem: NSStatusItem?
    private var mainWindow: NSWindow?
    private let preview = MenuPreviewController()

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
        preview.setName(viewName())
        // 预览由 app 自己拉本机 host，始终走 loopback，不受局域网可达性影响。
        preview.startPreview(address: "http://127.0.0.1:\(HostService.shared.port)/")
        // 拉最新视图状态，箭头切换和标题都用它
        HostService.shared.fetchViews { [weak self] in
            self?.preview.setName(self?.viewName() ?? "未选择视图")
        }
    }

    func menuDidClose(_ menu: NSMenu) {
        // 关闭后停掉预览页面的轮询
        preview.stopPreview()
    }

    // MARK: - 视图操作

    /// 按 direction（-1 上一个 / 1 下一个）在已保存视图间循环应用。
    private func cycleView(_ direction: Int) {
        let host = HostService.shared
        let views = host.viewsState.views
        guard !views.isEmpty else { return }
        let next: Int
        if let index = views.firstIndex(where: { $0.id == host.viewsState.current }) {
            next = (index + direction + views.count) % views.count
        } else {
            next = direction > 0 ? 0 : views.count - 1
        }
        host.switchView(to: views[next].id) { [weak self] in
            self?.preview.setName(self?.viewName() ?? "未选择视图")
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
