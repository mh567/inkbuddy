import AppKit
import SwiftUI

/// 弹出面板的预览状态：当前视图的区域几何、设备实测宽高比、视图名。
final class MenuPreviewModel: ObservableObject {
    @Published var regions: [ViewRegion] = []
    @Published var aspect: Double = 16.0 / 9.0
    @Published var name: String = "未选择视图"
}

/// 状态栏弹出面板的内容：◀ 视图名 ▶、实时预览、设置与退出。
///
/// 预览画布与设置里的编辑器预览共用 InkPreviewCanvas，显示 host
/// /api/preview/html 生成的页面。这里只负责头部、底部按钮和轮询生命周期。
struct MenuPanelView: View {
    @ObservedObject var model: MenuPreviewModel
    @ObservedObject var store: EditorPreviewStore

    var onPrev: () -> Void
    var onNext: () -> Void
    var onSettings: () -> Void
    var onQuit: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                Button(action: onPrev) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 11, weight: .semibold))
                }
                .buttonStyle(.plain)
                .help("上一个视图")

                Spacer(minLength: 4)

                Text(model.name)
                    .font(.system(size: 12, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)

                Spacer(minLength: 4)

                Button(action: onNext) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 11, weight: .semibold))
                }
                .buttonStyle(.plain)
                .help("下一个视图")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)

            InkPreviewCanvas(
                regions: model.regions,
                frame: store.frame,
                aspect: model.aspect,
                showChrome: false
            )
            .frame(height: 300)
            .padding(.horizontal, 12)
            .padding(.bottom, 8)

            Divider()

            HStack(spacing: 8) {
                Button(action: onSettings) {
                    Text("设置").foregroundColor(.accentColor)
                }
                .buttonStyle(.plain)
                Spacer(minLength: 4)
                Button(action: onQuit) {
                    Text("退出 inkbuddy").foregroundColor(.accentColor)
                }
                .buttonStyle(.plain)
            }
            .font(.system(size: 12))
            .padding(.horizontal, 12)
            .padding(.vertical, 9)
        }
        .frame(width: MenuPanelView.panelWidth)
    }

    /// 面板宽度；高度由头部、画布和底部按钮决定。
    static let panelWidth: CGFloat = 340
}

/// 状态栏弹出面板：点击菜单栏图标弹出，点外面自动收起。
///
/// 预览原先放在 NSMenu 的自定义菜单项视图里，WebKit 在菜单跟踪期间不重绘，
/// 内容一变或画布一改尺寸就停在白页上。弹出面板是普通窗口，和设置窗口一样，
/// 网页能正常重绘。
final class MenuPreviewController: NSObject, NSPopoverDelegate {
    private let popover = NSPopover()
    private let store = EditorPreviewStore(port: HostService.shared.port)
    private let model = MenuPreviewModel()

    private var pollTimer: Timer?
    /// 面板是否处于预览周期：异步响应回来时据此判断是否已过期。
    private var isActive = false

    var onPrev: (() -> Void)?
    var onNext: (() -> Void)?
    var onSettings: (() -> Void)?
    var onQuit: (() -> Void)?

    var isShown: Bool { popover.isShown }

    override init() {
        super.init()
        let root = MenuPanelView(
            model: model,
            store: store,
            onPrev: { [weak self] in self?.onPrev?() },
            onNext: { [weak self] in self?.onNext?() },
            onSettings: { [weak self] in self?.onSettings?() },
            onQuit: { [weak self] in self?.onQuit?() }
        )
        let controller = NSHostingController(rootView: root)
        controller.sizingOptions = []
        controller.view.frame = NSRect(x: 0, y: 0, width: MenuPanelView.panelWidth, height: 376)
        popover.contentViewController = controller
        popover.contentSize = NSSize(width: MenuPanelView.panelWidth, height: 376)
        popover.behavior = .transient
        popover.animates = false
        popover.delegate = self
    }

    /// 在状态栏按钮下方弹出面板并开始取数。
    func show(relativeTo button: NSStatusBarButton) {
        popover.contentSize = NSSize(width: MenuPanelView.panelWidth, height: 376)
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        startPreview()
    }

    func close() {
        popover.performClose(nil)
    }

    func popoverDidClose(_ notification: Notification) {
        stopPreview()
    }

    /// 视图切换后立刻同步布局与新视图内容，不必等下一次轮询。
    func refreshLayout() {
        syncLayout()
        poll()
    }

    func setName(_ text: String) {
        if model.name != text {
            model.name = text
        }
    }

    // MARK: - 轮询

    /// 面板打开时调用：同步布局，取一帧，之后每秒刷新。
    private func startPreview() {
        isActive = true
        refreshLayout()
        pollTimer?.invalidate()
        let timer = Timer(timeInterval: 1, target: self, selector: #selector(poll), userInfo: nil, repeats: true)
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
        // 与编辑器一致：用设备实测比例算字号缩放，预览所见即墨水屏所见。
        HostService.shared.refreshDeviceAspect { [weak self] deviceAspect in
            guard let self = self, self.isActive, let deviceAspect = deviceAspect else { return }
            self.model.aspect = deviceAspect
        }
    }

    /// 面板关闭时调用：停掉轮询并清空缓存，避免下次打开闪现旧内容。
    private func stopPreview() {
        isActive = false
        pollTimer?.invalidate()
        pollTimer = nil
        store.reset()
    }

    /// 重读 host 当前的视图与插件状态。
    private func syncLayout() {
        let host = HostService.shared
        let current = host.viewsState.views.first { $0.id == host.viewsState.current }
        let regions: [ViewRegion] = current?.regions ?? stackedRegions(from: host)
        if model.regions != regions {
            model.regions = regions
        }
        if let deviceAspect = host.deviceAspect, model.aspect != deviceAspect {
            model.aspect = deviceAspect
        }
    }

    /// 无视图时的竖排等分布局，与 host 的 Region::stacked 同规则。
    private func stackedRegions(from host: HostService) -> [ViewRegion] {
        // 与 host 的无视图路径一致：所有启用的扩展，按扫描顺序竖排等分。
        let plugins = host.plugins.filter { $0.enabled && $0.available }
        let total = max(plugins.count, 1)
        let height = 100.0 / Double(total)
        return plugins.enumerated().map { index, plugin in
            ViewRegion(
                plugin: plugin.id,
                x: 0,
                y: height * Double(index),
                w: 100,
                h: height
            )
        }
    }

    @objc private func poll() {
        guard isActive else { return }
        syncLayout()
        store.fetch(regions: model.regions, aspect: model.aspect)
    }
}
