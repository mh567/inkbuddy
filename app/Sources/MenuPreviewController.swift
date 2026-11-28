import AppKit
import SwiftUI

/// 弹出面板的预览状态：当前视图的区域几何和视图名。
final class MenuPreviewModel: ObservableObject {
    @Published var regions: [ViewRegion] = []
    @Published var name: String = "未选择视图"
}

/// 面板里的小按钮：悬停时垫一层圆角底色，看得见可点范围，按下再压暗一点。
/// hover 状态用 ObservableObject 承载：命令行构建下 @State 宏不可用，
/// 且 ButtonStyle 本身不是 View，包装器要放进内层的 View 才有生命周期。
private final class PanelHoverState: ObservableObject {
    @Published var hovering = false
}

private struct HoverButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        HoverBody(configuration: configuration)
    }

    private struct HoverBody: View {
        let configuration: Configuration
        @StateObject private var hover = PanelHoverState()

        var body: some View {
            configuration.label
                .background(
                    RoundedRectangle(cornerRadius: 6)
                        .fill(hover.hovering ? Color.primary.opacity(0.10) : Color.clear)
                )
                .opacity(configuration.isPressed ? 0.55 : 1)
                .onHover { hover.hovering = $0 }
                .animation(.easeOut(duration: 0.12), value: hover.hovering)
        }
    }
}

/// 状态栏弹出面板的内容：◀ 视图名 ▶、实时预览、设置与退出。
///
/// 预览画布与设置里的编辑器预览共用 InkPreviewCanvas，显示 host
/// /api/preview/html 生成的页面。这里只负责头部、底部按钮和轮询生命周期。
struct MenuPanelView: View {
    @ObservedObject var model: MenuPreviewModel
    @ObservedObject var host: HostService
    @ObservedObject var store: EditorPreviewStore
    let holder: PreviewWebViewHolder

    var onPrev: () -> Void
    var onNext: () -> Void
    var onSettings: () -> Void
    var onQuit: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                Button(action: onPrev) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 12, weight: .semibold))
                        // 只有图标时点击区跟着图标走，11pt 的箭头几乎点不中；
                        // 固定一块 32×26 的热区，整块都算按钮。
                        .frame(width: 32, height: 26)
                        .contentShape(Rectangle())
                }
                .buttonStyle(HoverButtonStyle())
                .help("上一个视图")

                Spacer(minLength: 4)

                Text(model.name)
                    .font(.system(size: 12, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)

                Spacer(minLength: 4)

                Button(action: onNext) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .semibold))
                        .frame(width: 32, height: 26)
                        .contentShape(Rectangle())
                }
                .buttonStyle(HoverButtonStyle())
                .help("下一个视图")
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)

            InkPreviewCanvas(
                regions: model.regions,
                frame: store.frame,
                aspect: host.canvasAspect,
                holder: holder,
                showChrome: false
            )
            .frame(height: 300)
            .padding(.horizontal, 12)
            .padding(.bottom, 8)

            Divider()

            HStack(spacing: 8) {
                Button(action: onSettings) {
                    Text("设置")
                        .foregroundColor(.accentColor)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .contentShape(Rectangle())
                }
                .buttonStyle(HoverButtonStyle())
                Spacer(minLength: 4)
                Button(action: onQuit) {
                    Text("退出 inkbuddy")
                        .foregroundColor(.accentColor)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .contentShape(Rectangle())
                }
                .buttonStyle(HoverButtonStyle())
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
    /// 面板的预览 WebView：长寿命，跨开关复用，关闭时只卸内容。
    private let previewHolder = PreviewWebViewHolder()

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
            host: HostService.shared,
            store: store,
            holder: previewHolder,
            onPrev: { [weak self] in self?.onPrev?() },
            onNext: { [weak self] in self?.onNext?() },
            onSettings: { [weak self] in self?.onSettings?() },
            onQuit: { [weak self] in self?.onQuit?() }
        )
        let controller = NSHostingController(rootView: root)
        controller.sizingOptions = []
        controller.view.frame = NSRect(x: 0, y: 0, width: MenuPanelView.panelWidth, height: 384)
        popover.contentViewController = controller
        popover.contentSize = NSSize(width: MenuPanelView.panelWidth, height: 384)
        popover.behavior = .transient
        popover.animates = false
        popover.delegate = self
    }

    /// 在状态栏按钮下方弹出面板并开始取数。
    func show(relativeTo button: NSStatusBarButton) {
        popover.contentSize = NSSize(width: MenuPanelView.panelWidth, height: 384)
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        startPreview()
    }

    func close() {
        popover.performClose(nil)
    }

    func popoverDidClose(_ notification: Notification) {
        stopPreview()
    }

    /// 点击切换后马上调用：名字换成目标视图、画布清空。
    /// 加载指示立刻出现，不等 POST 返回，也不等新内容算好。
    func prepareSwitch(name: String) {
        if model.name != name {
            model.name = name
        }
        store.reset()
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
    }

    /// 面板关闭时调用：停掉轮询、清空缓存，并把 WebView 里的页面卸掉。
    /// 不销毁 WebView 本身（下次打开接着用），但 WebContent 里的页面内存
    /// 会随之释放，空闲时占用从 ~120 MB 掉回 ~11 MB。
    private func stopPreview() {
        isActive = false
        pollTimer?.invalidate()
        pollTimer = nil
        store.reset()
        previewHolder.unload()
    }

    /// 重读 host 当前的视图与插件状态。
    private func syncLayout() {
        let host = HostService.shared
        let current = host.viewsState.views.first { $0.id == host.viewsState.current }
        let regions: [ViewRegion] = current?.regions ?? stackedRegions(from: host)
        if model.regions != regions {
            model.regions = regions
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
        let host = HostService.shared
        host.refreshDevices()
        syncLayout()
        store.fetch(regions: model.regions, aspect: host.canvasAspect)
    }
}
