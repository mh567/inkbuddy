import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var statusItem: NSStatusItem?
    private var mainWindow: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenuBar()
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

    private func buildMenuBar() {
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
        let open = NSMenuItem(title: "设置", action: #selector(showMainWindow), keyEquivalent: "")
        open.target = self
        menu.addItem(open)
        menu.addItem(NSMenuItem.separator())
        let quit = NSMenuItem(title: "退出 inkbuddy", action: #selector(quitApp), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)
        item.menu = menu
        statusItem = item
    }

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
