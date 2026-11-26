import AppKit
import SwiftUI

/// 菜单侧的预览布局来源：当前视图的区域几何 + 设备实测宽高比。
/// 编辑器的 ViewEditorModel 带一堆编辑态（选中、手势），菜单不需要，单独一个轻量模型。
final class MenuPreviewModel: ObservableObject {
    @Published var regions: [ViewRegion] = []
    @Published var aspect: Double = 16.0 / 9.0
}

/// 菜单栏预览的 SwiftUI 壳：把共享画布接到菜单侧的取数对象上。
/// store 的 frame 是 @Published，宿主返回新页面时画布会自己重绘，
/// 不用控制器去比对内容差异。
private struct MenuPreviewRoot: View {
    @ObservedObject var model: MenuPreviewModel
    @ObservedObject var store: EditorPreviewStore

    var body: some View {
        InkPreviewCanvas(
            regions: model.regions,
            frame: store.frame,
            aspect: model.aspect,
            showChrome: false
        )
    }
}

/// 菜单栏下拉里的预览头部：上一行是 ◀ / 当前视图名 / ▶，下方显示墨水屏内容。
///
/// 预览画布与设置里的编辑器预览共用 InkPreviewCanvas，显示 host
/// /api/preview/html 生成的页面。菜单这里只负责视图切换头部和轮询生命周期：
/// 菜单打开时拉当前视图的区域布局并定时取页面，关闭时停掉轮询。
final class MenuPreviewController: NSObject {
    let containerView: NSView

    private let prevButton: NSButton
    private let nextButton: NSButton
    private let nameLabel: NSTextField

    private let store = EditorPreviewStore(port: HostService.shared.port)
    private let model = MenuPreviewModel()
    private let hostingView: NSHostingView<MenuPreviewRoot>

    private var pollTimer: Timer?
    /// 菜单是否正处于预览周期：异步响应回来时据此判断是否已过期。
    private var isActive = false

    var onPrev: (() -> Void)?
    var onNext: (() -> Void)?

    private let width: CGFloat = 320
    private let height: CGFloat = 292

    override init() {
        containerView = NSView(frame: NSRect(x: 0, y: 0, width: width, height: height))

        prevButton = NSButton(title: "◀", target: nil, action: nil)
        prevButton.isBordered = false
        prevButton.font = NSFont.systemFont(ofSize: 15, weight: .medium)

        nextButton = NSButton(title: "▶", target: nil, action: nil)
        nextButton.isBordered = false
        nextButton.font = NSFont.systemFont(ofSize: 15, weight: .medium)

        nameLabel = NSTextField(labelWithString: "未选择视图")
        nameLabel.alignment = .center
        nameLabel.font = NSFont.systemFont(ofSize: 12, weight: .medium)
        nameLabel.lineBreakMode = .byTruncatingMiddle

        let barHeight: CGFloat = 26
        let top = height
        prevButton.frame = NSRect(x: 8, y: top - barHeight - 2, width: 32, height: barHeight)
        nextButton.frame = NSRect(x: width - 40, y: top - barHeight - 2, width: 32, height: barHeight)
        nameLabel.frame = NSRect(x: 44, y: top - barHeight, width: width - 88, height: barHeight - 4)
        let inset: CGFloat = 8
        let canvasRect = NSRect(
            x: inset,
            y: 6,
            width: width - inset * 2,
            height: height - barHeight - 14
        )

        // SwiftUI 画布嵌进菜单项：和编辑器预览是同一个视图，只关掉区域边框与
        // 扩展名标签，让菜单里看到的更接近墨水屏本身。
        hostingView = NSHostingView(rootView: MenuPreviewRoot(model: model, store: store))
        hostingView.frame = canvasRect

        super.init()

        containerView.addSubview(prevButton)
        containerView.addSubview(nameLabel)
        containerView.addSubview(nextButton)
        containerView.addSubview(hostingView)

        prevButton.target = self
        prevButton.action = #selector(prevClicked)
        nextButton.target = self
        nextButton.action = #selector(nextClicked)
    }

    @objc private func prevClicked() { onPrev?() }
    @objc private func nextClicked() { onNext?() }

    /// 菜单打开时调用：同步当前视图布局，立即取一帧，之后每秒刷新。
    func startPreview() {
        isActive = true
        refreshLayout()
        pollTimer?.invalidate()
        let timer = Timer(timeInterval: 1, target: self, selector: #selector(poll), userInfo: nil, repeats: true)
        // NSMenu 展开时主线程跑在 eventTracking 模式，scheduledTimer 默认只注册
        // 在 default 模式，菜单打开期间不会触发。放进 common 才能持续刷新。
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
        // 与编辑器一致：用设备实测比例算字号缩放，预览所见即墨水屏所见。
        HostService.shared.refreshDeviceAspect { [weak self] deviceAspect in
            guard let self = self, self.isActive, let deviceAspect = deviceAspect else { return }
            // 菜单可能在这次请求返回前就关了，别再改写下一个打开周期的画布比例。
            self.model.aspect = deviceAspect
        }
    }

    /// 菜单关闭时调用：停掉轮询并清空缓存，避免下次打开闪现旧视图内容。
    func stopPreview() {
        isActive = false
        pollTimer?.invalidate()
        pollTimer = nil
        store.reset()
    }

    /// 视图切换后立刻同步布局与新视图内容，不必等下一次轮询。
    func refreshLayout() {
        syncLayout()
        poll()
    }

    /// 重读 host 当前的视图与插件状态。每次轮询都跑一遍：菜单刚打开时插件
    /// 列表可能还没拉回来，下一秒补上即可；箭头切换后也会读到最新的当前视图。
    private func syncLayout() {
        let host = HostService.shared
        let current = host.viewsState.views.first { $0.id == host.viewsState.current }
        // 有视图时用视图的区域布局；没视图时墨水屏按 default_on 竖排显示，
        // 菜单要跟着显示同样的内容，不能空着。
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
        // 与 host 的 default_ids() 同条件：enabled && default_on && 已安装。
        let plugins = host.plugins.filter { $0.installed && $0.enabled && $0.defaultOn }
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
        syncLayout()
        store.fetch(regions: model.regions, aspect: model.aspect)
    }

    func setName(_ text: String) {
        nameLabel.stringValue = text
    }
}
