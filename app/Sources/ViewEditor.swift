import SwiftUI
import AppKit

/// 区域互斥的最小间隙（百分比）：推开后留一点缝，浮点比较才稳定。
private let regionGap = 0.01
/// 最小区域尺寸（百分比），与 host sanitize 的下限一致。
private let regionMinSize = 5.0

/// 缩放把手所在的角。缩放时对角固定不动。
enum ResizeCorner {
    case topLeft, topRight, bottomLeft, bottomRight

    var isLeft: Bool { self == .topLeft || self == .bottomLeft }
    var isTop: Bool { self == .topLeft || self == .topRight }
}

/// 选中区域与邻居贴边共享的边。拖动共享边时分隔线语义：
/// 两块的相对边同时移动，不用先挪一块再挪另一块。
enum SharedEdgeSide {
    case top, bottom, left, right
}

/// 编辑用的纯几何矩形（百分比坐标，y 向下）。
private struct GeomRect {
    var x: Double
    var y: Double
    var w: Double
    var h: Double

    var maxX: Double { x + w }
    var maxY: Double { y + h }

    init(_ r: ViewRegion) {
        x = r.x; y = r.y; w = r.w; h = r.h
    }

    init(x: Double, y: Double, w: Double, h: Double) {
        self.x = x; self.y = y; self.w = w; self.h = h
    }

    func intersects(_ o: GeomRect) -> Bool {
        x < o.maxX - regionGap && o.x < maxX - regionGap
            && y < o.maxY - regionGap && o.y < maxY - regionGap
    }

    func toRegion(plugin: String) -> ViewRegion {
        ViewRegion(plugin: plugin, x: x, y: y, w: w, h: h).sanitized()
    }
}

/// 画布拖拽命中目标：手势开始时由起点位置一次性判定，拖动全程不变。
/// 把手、共享边条、区域本体都不再各自挂手势——它们随手势移动时
/// SwiftUI 可能中断并重启手势，命中目标来回切换，边缘就会抖动。
enum DragTarget {
    case move(index: Int)
    case resize(index: Int, corner: ResizeCorner)
    case sharedEdge(index: Int, side: SharedEdgeSide, neighbor: Int)
}

/// 视图编辑器状态。用 ObservableObject 而不是 @State：本项目的 swiftc
/// 命令行构建加载不了 SwiftUI 的宏实现，普通属性包装器不受影响。
final class ViewEditorModel: ObservableObject {
    /// nil = 未打开；"new" = 新建；其他 = 正在编辑的视图 id
    @Published var viewId: String?
    @Published var name: String = ""
    @Published var regions: [ViewRegion] = []
    /// 画布上选中的区域下标
    @Published var selected: Int?
    /// 画布宽高比（宽/高）。区域是百分比定位，切换比例时排版结果直接
    /// 对应实际设备；默认 16:9，可切换到墨水屏常见的竖屏比例。
    /// 持久化到 UserDefaults：升级或重启后仍记住上次选择。
    @Published var canvasAspect: Double =
        UserDefaults.standard.object(forKey: "editorCanvasAspect") as? Double ?? 16.0 / 9.0 {
        didSet {
            UserDefaults.standard.set(canvasAspect, forKey: "editorCanvasAspect")
        }
    }

    /// 手势起始时的区域快照。DragGesture 的 translation 是从手势开始的
    /// 累计值，若每次 onChanged 都叠加到「当前区域」，位移会被重复积分，
    /// 拖拽越拖越快。因此手势开始时记一次快照，之后都从快照算绝对位置。
    private var gestureStart: GeomRect?

    /// 共享边手势的快照：被拖的边涉及选中区域和邻居两块，都要记基准。
    private var edgeGestureStart: (index: Int, selected: GeomRect,
                                   neighbor: Int, neighborRect: GeomRect)?

    /// 画布统一拖拽手势的命中目标，手势开始时判定一次，结束时清空。
    /// 放在模型里：视图层的 @State 宏在命令行构建下不可用。
    var dragTarget: DragTarget?

    /// 保存进行中：禁用保存按钮，避免连点发起并发保存互相覆盖。
    @Published var isSaving = false

    /// 保存/应用后的短暂反馈文字，几秒后自动消失。
    @Published var statusMessage: String?
    private var statusWork: DispatchWorkItem?

    func showStatus(_ text: String) {
        statusWork?.cancel()
        statusMessage = text
        let work = DispatchWorkItem { [weak self] in self?.statusMessage = nil }
        statusWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.5, execute: work)
    }

    /// 手势开始时记录基准快照（幂等：onChanged 会连续触发多次）。
    func beginGesture(at index: Int) {
        edgeGestureStart = nil
        if gestureStart == nil {
            gestureStart = regions[safe: index].map(GeomRect.init)
        }
    }

    /// 手势结束，释放快照。
    func endGesture() {
        gestureStart = nil
    }

    /// 共享边手势开始：记录选中区域与邻居的几何基准。
    func beginEdgeGesture(at index: Int, neighbor: Int) {
        gestureStart = nil
        if let snap = edgeGestureStart, snap.index == index, snap.neighbor == neighbor {
            return
        }
        guard let s = regions[safe: index], let n = regions[safe: neighbor] else { return }
        edgeGestureStart = (index, GeomRect(s), neighbor, GeomRect(n))
    }

    func endEdgeGesture() {
        edgeGestureStart = nil
    }

    /// 常用画布比例。
    static let aspectOptions: [(String, Double)] = [
        ("16:9", 16.0 / 9.0),
        ("4:3", 4.0 / 3.0),
        ("1:1", 1.0),
        ("3:4", 3.0 / 4.0),
        ("9:16", 9.0 / 16.0),
    ]

    /// 下拉框选项：画布比例来自设备实测时不在常用列表里，动态补一个
    /// 「设备 x.xx」项，避免选中值没有对应 tag 显示为空。
    var aspectPickerOptions: [(String, Double)] {
        if Self.aspectOptions.contains(where: { abs($0.1 - canvasAspect) < 0.005 }) {
            return Self.aspectOptions
        }
        return [("设备 \(String(format: "%.2f", canvasAspect))", canvasAspect)]
            + Self.aspectOptions
    }

    /// 用设备实测的宽高比覆盖画布比例。服务端字号缩放用的就是设备上报的
    /// 这个值，编辑器/预览对齐它后，预览缩多少、墨水屏就缩多少。
    /// 设备还没轮询过（nil）时保持手选值。
    func applyDeviceAspect(_ aspect: Double?) {
        guard let aspect = aspect, aspect > 0.1 else { return }
        if abs(canvasAspect - aspect) > 0.001 {
            canvasAspect = aspect
        }
    }

    var isNew: Bool { viewId == "new" }

    func load(_ view: SavedView?) {
        if let view = view {
            viewId = view.id
            name = view.name
            regions = view.regions
        } else {
            viewId = "new"
            name = "自定义 \(HostService.shared.viewsState.views.count + 1)"
            regions = []
        }
        selected = nil
    }

    /// 把一个已安装扩展加进画布。区域互斥：优先找一块放得下的空白
    /// （按面积从大到小尝试）；画布已被占满时把面积最大的现有区域
    /// 对半切开，一半给新扩展，保证新区域有可用尺寸且不与谁重叠。
    /// 已在视图中的扩展不重复添加，改为选中它。
    func addPlugin(_ id: String) {
        if let index = regions.firstIndex(where: { $0.plugin == id }) {
            selected = index
            return
        }
        let rect = findFreeSpot() ?? splitLargestRegion()
        regions.append(rect.toRegion(plugin: id))
        selected = regions.count - 1
    }

    /// 在现有区域之外找一块空白：候选尺寸按面积降序，位置按 5% 网格扫描，
    /// 第一个互不重叠的位置即为落点。画布为空时返回全屏；找不到返回 nil。
    private func findFreeSpot() -> GeomRect? {
        if regions.isEmpty {
            return GeomRect(x: 0, y: 0, w: 100, h: 100)
        }
        let sizes: [(Double, Double)] = [
            (100, 50), (50, 100), (66.66, 66.66), (50, 50),
            (33.33, 50), (50, 33.33), (33.33, 33.33), (25, 25),
        ]
        for (w, h) in sizes {
            var py = 0.0
            while py + h <= 100 + 0.001 {
                var px = 0.0
                while px + w <= 100 + 0.001 {
                    let candidate = GeomRect(x: px, y: py, w: w, h: h)
                    let others = regions.map(GeomRect.init)
                    if !others.contains(where: { candidate.intersects($0) }) {
                        return candidate
                    }
                    px += 5
                }
                py += 5
            }
        }
        return nil
    }

    /// 没有空白可放时的兜底：把面积最大的区域沿长边对半切开，
    /// 一半留给原扩展，另一半返回给新扩展。任何情况下都不产生重叠。
    private func splitLargestRegion() -> GeomRect {
        let index = regions.indices.max {
            regions[$0].w * regions[$0].h < regions[$1].w * regions[$1].h
        } ?? 0
        let original = GeomRect(regions[index])
        let newHalf: GeomRect
        if original.w >= original.h {
            let half = original.w / 2
            newHalf = GeomRect(x: original.x + half, y: original.y, w: half, h: original.h)
            regions[index] = GeomRect(x: original.x, y: original.y, w: half, h: original.h)
                .toRegion(plugin: regions[index].plugin)
        } else {
            let half = original.h / 2
            newHalf = GeomRect(x: original.x, y: original.y + half, w: original.w, h: half)
            regions[index] = GeomRect(x: original.x, y: original.y, w: original.w, h: half)
                .toRegion(plugin: regions[index].plugin)
        }
        return newHalf
    }

    func removeSelected() {
        guard let index = selected, regions.indices.contains(index) else { return }
        regions.remove(at: index)
        selected = nil
    }

    /// 拖动：从手势快照平移到目标位置，再从其它区域里推出来（贴边卡住）。
    func moveRegion(at index: Int, dx: Double, dy: Double) {
        guard regions.indices.contains(index),
              let start = gestureStart ?? regions[safe: index].map(GeomRect.init) else { return }
        var rect = start
        rect.x += dx
        rect.y += dy
        // 画布边界：最小尺寸 5%，其余夹在 0-100。
        rect.w = min(max(rect.w, regionMinSize), 100)
        rect.h = min(max(rect.h, regionMinSize), 100)
        rect.x = min(max(rect.x, 0), 100 - rect.w)
        rect.y = min(max(rect.y, 0), 100 - rect.h)
        rect = pushOutOfOthers(rect, index: index)
        regions[index] = rect.toRegion(plugin: regions[index].plugin)
    }

    /// 缩放：anchor 是固定的对角，四条边分别处理；自由边不能越过
    /// 挡在前面的邻居，也不能出画布，最后保底最小尺寸。
    func resizeRegion(at index: Int, corner: ResizeCorner, dx: Double, dy: Double) {
        guard regions.indices.contains(index),
              let current = regions[safe: index] else { return }
        // 以手势快照为基准：translation 是累计值，不能用当前区域叠加。
        let s = gestureStart ?? GeomRect(current)
        var minX = s.x, maxX = s.maxX, minY = s.y, maxY = s.maxY
        if corner.isLeft {
            minX = min(max(s.x + dx, 0), maxX - regionMinSize)
        } else {
            maxX = min(max(s.maxX + dx, minX + regionMinSize), 100)
        }
        if corner.isTop {
            minY = min(max(s.y + dy, 0), maxY - regionMinSize)
        } else {
            maxY = min(max(s.maxY + dy, minY + regionMinSize), 100)
        }

        // 邻居约束跑两遍：第一遍可能改变跨度，影响第二遍的相交判断。
        for _ in 0..<2 {
            for (j, oRaw) in regions.enumerated() where j != index {
                let o = GeomRect(oRaw)
                let vOverlap = minY < o.maxY - regionGap && o.y < maxY - regionGap
                let hOverlap = minX < o.maxX - regionGap && o.x < maxX - regionGap
                if corner.isLeft, vOverlap, o.maxX <= s.x + regionGap {
                    minX = max(minX, o.maxX + regionGap)
                }
                if !corner.isLeft, vOverlap, o.x >= s.maxX - regionGap {
                    maxX = min(maxX, o.x - regionGap)
                }
                if corner.isTop, hOverlap, o.maxY <= s.y + regionGap {
                    minY = max(minY, o.maxY + regionGap)
                }
                if !corner.isTop, hOverlap, o.y >= s.maxY - regionGap {
                    maxY = min(maxY, o.y - regionGap)
                }
            }
            // 夹过之后自由边可能越过固定边，重新保底最小尺寸。
            if maxX - minX < regionMinSize {
                if corner.isLeft { minX = maxX - regionMinSize } else { maxX = minX + regionMinSize }
            }
            if maxY - minY < regionMinSize {
                if corner.isTop { minY = maxY - regionMinSize } else { maxY = minY + regionMinSize }
            }
        }
        regions[index] = GeomRect(x: minX, y: minY, w: maxX - minX, h: maxY - minY)
            .toRegion(plugin: current.plugin)
    }

    /// 找出选中区域贴边共享的边：与邻居边缘距离 ≤ 0.5% 且沿边投影
    /// 重叠超过 1% 才算贴边（0.5% 容差兼容存量数据里的小误差）。
    func sharedEdges(for index: Int) -> [(side: SharedEdgeSide, neighbor: Int)] {
        guard let r = regions[safe: index] else { return [] }
        let s = GeomRect(r)
        var result: [(side: SharedEdgeSide, neighbor: Int)] = []
        for (j, oRaw) in regions.enumerated() where j != index {
            let o = GeomRect(oRaw)
            let hOverlap = min(s.maxX, o.maxX) - max(s.x, o.x)
            let vOverlap = min(s.maxY, o.maxY) - max(s.y, o.y)
            if hOverlap > 1 {
                if abs(s.y - o.maxY) <= 0.5 { result.append((.top, j)) }
                if abs(s.maxY - o.y) <= 0.5 { result.append((.bottom, j)) }
            }
            if vOverlap > 1 {
                if abs(s.x - o.maxX) <= 0.5 { result.append((.left, j)) }
                if abs(s.maxX - o.x) <= 0.5 { result.append((.right, j)) }
            }
        }
        return result
    }

    /// 拖动共享边：分隔线语义，选中区域的边与邻居的正对边同时移动。
    /// 边界约束：两块都保持最小尺寸，且不能出画布。
    func dragSharedEdge(at index: Int, side: SharedEdgeSide, delta: Double) {
        guard let snap = edgeGestureStart, snap.index == index,
              regions.indices.contains(snap.neighbor) else { return }
        var sel = snap.selected
        var nbr = snap.neighborRect
        switch side {
        case .top:
            // 共享边向下移 delta：选中区域顶边下移（变矮），邻居变高。
            var y = sel.y + delta
            y = max(y, nbr.y + regionMinSize)
            y = min(y, min(sel.maxY - regionMinSize, 100))
            nbr.h = y - nbr.y
            sel.h = sel.maxY - y
            sel.y = y
        case .bottom:
            var y = sel.maxY + delta
            y = min(y, nbr.maxY - regionMinSize)
            y = max(y, max(sel.y + regionMinSize, 0))
            sel.h = y - sel.y
            nbr.h = nbr.maxY - y
            nbr.y = y
        case .left:
            var x = sel.x + delta
            x = max(x, nbr.x + regionMinSize)
            x = min(x, min(sel.maxX - regionMinSize, 100))
            nbr.w = x - nbr.x
            sel.w = sel.maxX - x
            sel.x = x
        case .right:
            var x = sel.maxX + delta
            x = min(x, nbr.maxX - regionMinSize)
            x = max(x, max(sel.x + regionMinSize, 0))
            sel.w = x - sel.x
            nbr.w = nbr.maxX - x
            nbr.x = x
        }
        regions[index] = sel.toRegion(plugin: regions[index].plugin)
        regions[snap.neighbor] = nbr.toRegion(plugin: regions[snap.neighbor].plugin)
    }

    /// 从其它区域里把 rect 推出来：沿穿透更小的轴退到贴边。
    /// 邻居不动（贴边卡住语义），只调整被拖动的这块。
    private func pushOutOfOthers(_ rect: GeomRect, index: Int) -> GeomRect {
        var r = rect
        for _ in 0..<8 {
            var moved = false
            for (j, oRaw) in regions.enumerated() where j != index {
                let o = GeomRect(oRaw)
                guard r.intersects(o) else { continue }
                let pushRight = o.maxX - r.x          // 往右退出
                let pushLeft = r.maxX - o.x           // 往左退出
                let pushDown = o.maxY - r.y           // 往下退出
                let pushUp = r.maxY - o.y             // 往上退出
                let px = min(pushRight, pushLeft)
                let py = min(pushDown, pushUp)
                if px <= py {
                    r.x += pushRight <= pushLeft ? px + regionGap : -(pushLeft + regionGap)
                } else {
                    r.y += pushDown <= pushUp ? py + regionGap : -(pushUp + regionGap)
                }
                r.x = min(max(r.x, 0), 100 - r.w)
                r.y = min(max(r.y, 0), 100 - r.h)
                moved = true
            }
            if !moved { break }
        }
        return r
    }

    /// 数值面板修改后同样执行互斥约束：平移推出去，保证任何状态不重叠。
    func updateRegion(at index: Int, transform: (inout ViewRegion) -> Void) {
        guard regions.indices.contains(index), let start = regions[safe: index] else { return }
        var region = start
        transform(&region)
        regions[index] = region.sanitized()
        let rect = pushOutOfOthers(GeomRect(regions[index]), index: index)
        regions[index] = rect.toRegion(plugin: region.plugin)
    }
}

extension ViewRegion {
    /// 编辑过程中的坐标就地校正（与 host sanitize 同规则）。
    func sanitized() -> ViewRegion {
        var r = self
        let size = 5.0...100.0
        r.w = min(max(r.w.isFinite ? r.w : 5, size.lowerBound), size.upperBound)
        r.h = min(max(r.h.isFinite ? r.h : 5, size.lowerBound), size.upperBound)
        r.x = min(max(r.x.isFinite ? r.x : 0, 0), 100 - r.w)
        r.y = min(max(r.y.isFinite ? r.y : 0, 0), 100 - r.h)
        r.x = (r.x * 100).rounded() / 100
        r.y = (r.y * 100).rounded() / 100
        r.w = (r.w * 100).rounded() / 100
        r.h = (r.h * 100).rounded() / 100
        return r
    }
}

/// 视图编辑窗口：左侧扩展面板、中间区域画布（拖拽移动/缩放）、右侧数值精调。
/// 视图的编辑都发生在这个窗口，设置窗口的视图页只负责列表和应用。
struct ViewEditorView: View {
    @ObservedObject var model: ViewEditorModel
    let host: HostService
    /// 内嵌实时预览的内容缓存，随编辑拖拽实时刷新。
    let previewStore: EditorPreviewStore

    var body: some View {
        VStack(spacing: 10) {
            // 顶部：名称 + 动作
            HStack(spacing: 10) {
                // 名称框限宽，把顶栏空间留给比例与按钮。
                TextField("视图名称", text: $model.name)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(size: 14))
                    .frame(width: 160)
                Button(model.isSaving ? "应用中…" : "应用") { save(thenApply: true) }
                    .buttonStyle(.bordered)
                    .controlSize(.regular)
                    .disabled(!canSave || model.isSaving)
                    .help("保存当前布局并应用到墨水屏，编辑窗口保持打开，方便继续调整")
                if let status = model.statusMessage {
                    Text(status)
                        .font(.system(size: 11))
                        .foregroundColor(.green)
                }
                Picker("比例", selection: $model.canvasAspect) {
                    ForEach(Array(model.aspectPickerOptions.enumerated()), id: \.offset) { _, option in
                        Text(option.0).tag(option.1)
                    }
                }
                .pickerStyle(.menu)
                .labelsHidden()
                .frame(width: 96)
                .help("画布宽高比：打开编辑器时自动同步设备实测比例；也可手动选择，重启后仍记住")
                Button("取消") { ViewEditorWindowController.shared.close() }
                    .buttonStyle(.borderless)
                    .controlSize(.regular)
                Button(model.isSaving ? "保存中…" : (model.isNew ? "创建" : "保存")) { save(thenApply: false) }
                    .buttonStyle(.borderedProminent)
                    .disabled(!canSave || model.isSaving)
            }
            .padding(.horizontal, 16)
            .padding(.top, 12)

            HStack(alignment: .top, spacing: 12) {
                // 左侧：已安装扩展面板
                pluginPanel
                    .frame(width: 170)
                // 中间：画布
                canvas
                // 右侧：数值精调 + 内嵌实时预览
                VStack(spacing: 10) {
                    inspector
                    previewPanel
                        .frame(height: 260)
                }
                .frame(width: 230)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 12)

            if !missing.isEmpty {
                Text("含未安装扩展：\(missing.joined(separator: "、"))，渲染时会跳过")
                    .font(.system(size: 11))
                    .foregroundColor(.orange)
            }
        }
        .frame(minWidth: 760, minHeight: 520)
    }

    private var canSave: Bool {
        !model.name.trimmingCharacters(in: .whitespaces).isEmpty && !model.regions.isEmpty
    }

    private var missing: [String] {
        model.regions.map { $0.plugin }.filter { host.pluginName(for: $0) == nil }
    }

    // MARK: - 左侧扩展面板

    private var pluginPanel: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("已安装扩展")
                .font(.system(size: 12, weight: .medium))
                .foregroundColor(.secondary)
            if host.plugins.filter({ $0.installed }).isEmpty {
                Text(host.running ? "未安装扩展" : "启动服务后显示")
                    .font(.system(size: 12))
                    .foregroundColor(.secondary)
            } else {
                ForEach(host.plugins.filter { $0.installed }) { plugin in
                    Button {
                        model.addPlugin(plugin.id)
                    } label: {
                        HStack {
                            Text(plugin.name)
                                .font(.system(size: 12))
                                .lineLimit(1)
                                .truncationMode(.tail)
                            Spacer()
                            Image(systemName: "plus.circle")
                                .font(.system(size: 11))
                                .foregroundColor(.secondary)
                        }
                    }
                    .buttonStyle(.borderless)
                }
            }
            Spacer()
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(NSColor.controlBackgroundColor)))
    }

    // MARK: - 中间画布（16:9，按百分比定位）

    private var canvas: some View {
        GeometryReader { geo in
            let size = fittedCanvasSize(in: geo.size)
            ZStack {
                Rectangle()
                    .fill(Color.white)
                    .overlay(
                        Rectangle().strokeBorder(Color.black.opacity(0.25), lineWidth: 1)
                    )
                // 非选中区域先画；选中的最后画并置顶，保证它的把手
                // 不会被邻居盖住或被邻居的把手抢占点击。
                ForEach(Array(model.regions.enumerated()), id: \.offset) { index, region in
                    if model.selected != index {
                        regionRect(index: index, region: region, canvas: size)
                    }
                }
                if let sel = model.selected, let region = model.regions[safe: sel] {
                    regionRect(index: sel, region: region, canvas: size)
                }
                // 共享边拖动条：置顶于所有区域之上。
                if let sel = model.selected {
                    ForEach(Array(model.sharedEdges(for: sel).enumerated()), id: \.offset) { _, edge in
                        sharedEdgeBar(selected: sel, edge: edge, canvas: size)
                    }
                }
                if model.regions.isEmpty {
                    Text("从左侧添加扩展开始布局")
                        .font(.system(size: 13))
                        .foregroundColor(.secondary)
                }
            }
            .frame(width: size.width, height: size.height)
            .gesture(canvasDragGesture(canvas: size))
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    /// 画布按设定的宽高比适配可用空间（与实时预览一致）。
    private func fittedCanvasSize(in available: CGSize) -> CGSize {
        let aspect = CGFloat(max(model.canvasAspect, 0.1))
        var w = available.width
        var h = w / aspect
        if h > available.height {
            h = available.height
            w = h * aspect
        }
        return CGSize(width: max(w, 120), height: max(h, 68))
    }

    @ViewBuilder private func regionRect(index: Int, region: ViewRegion, canvas: CGSize) -> some View {
        let isSelected = model.selected == index
        let rect = CGRect(
            x: region.x / 100 * Double(canvas.width),
            y: region.y / 100 * Double(canvas.height),
            width: region.w / 100 * Double(canvas.width),
            height: region.h / 100 * Double(canvas.height)
        )
        // SwiftUI 坐标系 y 向下，视图模型也是 y 向下（与渲染一致），无需翻转。
        ZStack {
            Rectangle()
                .fill(isSelected ? Color.accentColor.opacity(0.18) : Color.blue.opacity(0.10))
                .overlay(
                    Rectangle().strokeBorder(
                        isSelected ? Color.accentColor : Color.blue.opacity(0.55),
                        lineWidth: isSelected ? 2 : 1
                    )
                )
            Text(host.pluginName(for: region.plugin) ?? region.plugin)
                .font(.system(size: 11, weight: .medium))
                .lineLimit(1)
                .minimumScaleFactor(0.6)
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(Capsule().fill(Color.white.opacity(0.85)))
                .padding(4)
                .frame(maxWidth: rect.width - 8)
                .allowsHitTesting(false)
            // 四角缩放把手，对角固定。position 用区域本地坐标。
            // 只有选中区域显示把手：否则相邻区域的把手互相重叠，
            // 鼠标不知道会命中哪一个，边界不清晰。
            if isSelected {
                ForEach(Array(cornerAnchors(in: rect).enumerated()), id: \.offset) { item in
                    cornerHandle(index: index, corner: item.element.corner, canvas: canvas)
                        .position(x: item.element.point.x, y: item.element.point.y)
                }
            }
        }
        .frame(width: rect.width, height: rect.height)
        .position(x: rect.midX, y: rect.midY)
    }

    /// 四个把手的位置（区域本地坐标）与对应的缩放角。
    private func cornerAnchors(in rect: CGRect) -> [(corner: ResizeCorner, point: CGPoint)] {
        [
            (.topLeft, CGPoint(x: 0, y: 0)),
            (.topRight, CGPoint(x: rect.width, y: 0)),
            (.bottomLeft, CGPoint(x: 0, y: rect.height)),
            (.bottomRight, CGPoint(x: rect.width, y: rect.height)),
        ]
    }

    private func cornerHandle(index: Int, corner: ResizeCorner, canvas: CGSize) -> some View {
        Rectangle()
            .fill(Color.accentColor)
            .frame(width: 14, height: 14)
            .overlay(Rectangle().stroke(Color.white, lineWidth: 1.5))
            // 手势统一在画布层命中，把手只是视觉元素。
            .allowsHitTesting(false)
    }

    /// 共享边拖动条：选中区域与邻居贴边时显示在公共边上，
    /// 拖动它两块同时让出/占据空间（分隔线语义），
    /// 不用先挪一块再挪另一块。
    @ViewBuilder private func sharedEdgeBar(selected sel: Int,
                                            edge: (side: SharedEdgeSide, neighbor: Int),
                                            canvas: CGSize) -> some View {
        if let selRegion = model.regions[safe: sel],
           let nbrRegion = model.regions[safe: edge.neighbor] {
            let s = GeomRect(selRegion)
            let o = GeomRect(nbrRegion)
            let horizontal = edge.side == .top || edge.side == .bottom
            // 沿共享边的重叠投影（百分比→画布坐标），两端留 8pt 给角把手。
            let lo = horizontal
                ? max(s.x, o.x) / 100 * Double(canvas.width) + 8
                : max(s.y, o.y) / 100 * Double(canvas.height) + 8
            let hi = horizontal
                ? min(s.maxX, o.maxX) / 100 * Double(canvas.width) - 8
                : min(s.maxY, o.maxY) / 100 * Double(canvas.height) - 8
            let span = max(hi - lo, 12)
            let boundary = edge.side == .top ? s.y / 100 * Double(canvas.height)
                : edge.side == .bottom ? s.maxY / 100 * Double(canvas.height)
                : edge.side == .left ? s.x / 100 * Double(canvas.width)
                : s.maxX / 100 * Double(canvas.width)
            Rectangle()
                .fill(Color.accentColor)
                .overlay(Rectangle().stroke(Color.white, lineWidth: 1))
                .frame(width: horizontal ? span : 10,
                       height: horizontal ? 10 : span)
                // 手势统一在画布层命中，边条只是视觉元素。
                .allowsHitTesting(false)
                .position(
                    x: horizontal ? lo + span / 2 : boundary,
                    y: horizontal ? boundary : lo + span / 2
                )
        }
    }

    // MARK: - 画布统一拖拽手势

    /// 整个画布只有一个拖拽手势：起点位置一次性命中（把手 → 共享边条 →
    /// 区域本体），拖动全程目标不变。区域几何每帧更新不再干扰手势，
    /// 消除边缘抖动和命中歧义。
    /// minimumDistance 为 0：按下立即触发 onChanged，选中与命中一次完成。
    /// 之前区域本体上还挂了 onTapGesture 来选中，但子视图的 tap 会优先
    /// 占住第一次按压，父级的拖拽手势第一次收不到事件——表现为「点边缘
    /// 拖不动，松手再点一次才行」。tap 手势已删除，选中改由按下时的
    /// 命中测试完成，单击（按下即松开）同样能选中。
    private func canvasDragGesture(canvas: CGSize) -> some Gesture {
        DragGesture(minimumDistance: 0)
            .onChanged { value in
                if model.dragTarget == nil {
                    let target = hitTestDragTarget(at: value.startLocation, canvas: canvas)
                    model.dragTarget = target
                    switch target {
                    case .move(let index), .resize(let index, _):
                        model.selected = index
                        model.beginGesture(at: index)
                    case .sharedEdge(let index, _, let neighbor):
                        model.beginEdgeGesture(at: index, neighbor: neighbor)
                    case nil:
                        break
                    }
                }
                let dx = Double(value.translation.width / canvas.width * 100)
                let dy = Double(value.translation.height / canvas.height * 100)
                switch model.dragTarget {
                case .move(let index)?:
                    model.moveRegion(at: index, dx: dx, dy: dy)
                case .resize(let index, let corner)?:
                    model.resizeRegion(at: index, corner: corner, dx: dx, dy: dy)
                case .sharedEdge(let index, let side, _)?:
                    if side == .top || side == .bottom {
                        model.dragSharedEdge(at: index, side: side, delta: dy)
                    } else {
                        model.dragSharedEdge(at: index, side: side, delta: dx)
                    }
                case nil:
                    break
                }
            }
            .onEnded { _ in
                model.dragTarget = nil
                model.endGesture()
                model.endEdgeGesture()
            }
    }

    /// 起点命中测试：与渲染置顶顺序一致——选中区域的把手和共享边条
    /// 最优先，其次是区域本体（选中区域在最上层）。
    private func hitTestDragTarget(at point: CGPoint, canvas: CGSize) -> DragTarget? {
        if let sel = model.selected, let region = model.regions[safe: sel] {
            let rect = CGRect(
                x: region.x / 100 * Double(canvas.width),
                y: region.y / 100 * Double(canvas.height),
                width: region.w / 100 * Double(canvas.width),
                height: region.h / 100 * Double(canvas.height)
            )
            // 四角把手（视觉 14pt，命中半径 10pt）
            let corners: [(ResizeCorner, CGPoint)] = [
                (.topLeft, CGPoint(x: rect.minX, y: rect.minY)),
                (.topRight, CGPoint(x: rect.maxX, y: rect.minY)),
                (.bottomLeft, CGPoint(x: rect.minX, y: rect.maxY)),
                (.bottomRight, CGPoint(x: rect.maxX, y: rect.maxY)),
            ]
            for (corner, anchor) in corners
            where abs(point.x - anchor.x) <= 10 && abs(point.y - anchor.y) <= 10 {
                return .resize(index: sel, corner: corner)
            }
            // 共享边条（视觉厚 10pt，命中 ±7pt，两端 8pt 内缩给把手让位）
            let s = GeomRect(region)
            for edge in model.sharedEdges(for: sel) {
                guard let oRaw = model.regions[safe: edge.neighbor] else { continue }
                let o = GeomRect(oRaw)
                let horizontal = edge.side == .top || edge.side == .bottom
                let lo = horizontal
                    ? max(s.x, o.x) / 100 * Double(canvas.width) + 8
                    : max(s.y, o.y) / 100 * Double(canvas.height) + 8
                let hi = horizontal
                    ? min(s.maxX, o.maxX) / 100 * Double(canvas.width) - 8
                    : min(s.maxY, o.maxY) / 100 * Double(canvas.height) - 8
                let boundary = edge.side == .top ? s.y / 100 * Double(canvas.height)
                    : edge.side == .bottom ? s.maxY / 100 * Double(canvas.height)
                    : edge.side == .left ? s.x / 100 * Double(canvas.width)
                    : s.maxX / 100 * Double(canvas.width)
                let along = horizontal ? point.x : point.y
                let cross = horizontal ? point.y : point.x
                if along >= lo && along <= hi && abs(cross - boundary) <= 7 {
                    return .sharedEdge(index: sel, side: edge.side, neighbor: edge.neighbor)
                }
            }
        }
        // 区域本体：渲染顺序是非选中在前（数组序）、选中置顶，
        // 命中多个时取渲染在最上面的那块。
        if let sel = model.selected, let r = model.regions[safe: sel],
           regionContains(r, point, canvas: canvas) {
            return .move(index: sel)
        }
        var best: Int?
        for (index, r) in model.regions.enumerated() where index != model.selected {
            if regionContains(r, point, canvas: canvas) { best = index }
        }
        return best.map { .move(index: $0) }
    }

    /// 点是否落在区域内（含 2pt 余量，方便贴边抓取）。
    private func regionContains(_ region: ViewRegion, _ point: CGPoint, canvas: CGSize) -> Bool {
        let x0 = region.x / 100 * Double(canvas.width) - 2
        let y0 = region.y / 100 * Double(canvas.height) - 2
        let x1 = (region.x + region.w) / 100 * Double(canvas.width) + 2
        let y1 = (region.y + region.h) / 100 * Double(canvas.height) + 2
        return Double(point.x) >= x0 && Double(point.x) <= x1
            && Double(point.y) >= y0 && Double(point.y) <= y1
    }

    // MARK: - 右侧数值面板

    @ViewBuilder private var inspector: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("选中区域")
                .font(.system(size: 12, weight: .medium))
                .foregroundColor(.secondary)
            if let index = model.selected, let region = model.regions[safe: index] {
                Text(host.pluginName(for: region.plugin) ?? region.plugin)
                    .font(.system(size: 13, weight: .medium))
                numberRow("位置 X (%)", value: Binding(
                    get: { region.x },
                    set: { newValue in model.updateRegion(at: index) { r in r.x = newValue } }
                ))
                numberRow("位置 Y (%)", value: Binding(
                    get: { region.y },
                    set: { newValue in model.updateRegion(at: index) { r in r.y = newValue } }
                ))
                numberRow("宽度 (%)", value: Binding(
                    get: { region.w },
                    set: { newValue in model.updateRegion(at: index) { r in r.w = newValue } }
                ))
                numberRow("高度 (%)", value: Binding(
                    get: { region.h },
                    set: { newValue in model.updateRegion(at: index) { r in r.h = newValue } }
                ))
                Spacer()
                Button("移除该区域", role: .destructive) { model.removeSelected() }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            } else {
                Text("点选画布上的区域后可以拖动、缩放或在这里输入精确数值")
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                Spacer()
            }
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(NSColor.controlBackgroundColor)))
    }

    private func numberRow(_ label: String, value: Binding<Double>) -> some View {
        HStack {
            Text(label)
                .font(.system(size: 11))
                .foregroundColor(.secondary)
            Spacer(minLength: 4)
            TextField("0", value: value, format: .number)
                .textFieldStyle(.roundedBorder)
                .font(.system(size: 11))
                .frame(width: 58)
                .multilineTextAlignment(.trailing)
        }
    }

    // MARK: - 内嵌实时预览

    /// 与编辑画布同一几何模型的实时预览，放在数值面板下方：
    /// 拖拽时区域几何直接驱动预览排版，内容文本由 host /api/preview
    /// 提供（去抖 250ms + 每秒轮询），取代原来的悬浮预览窗口。
    private var previewPanel: some View {
        VStack(spacing: 6) {
            Text("实时预览")
                .font(.system(size: 12, weight: .medium))
                .foregroundColor(.secondary)
            EditorPreviewView(model: model, store: previewStore)
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(NSColor.controlBackgroundColor)))
    }

    // MARK: - 保存与应用

    /// 保存当前布局。thenApply 为 true 时保存成功后把该视图设为当前
    /// 视图（墨水屏下一次取帧即显示新布局）。两种情况都留在编辑窗口：
    /// 用户需要对着预览和墨水屏继续调整，关窗会打断调试。
    private func save(thenApply: Bool) {
        let name = model.name.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, !model.regions.isEmpty, !model.isSaving else { return }
        model.isSaving = true
        let view = SavedView(
            id: model.isNew
                ? "custom-\(Int(Date().timeIntervalSince1970 * 1000))-\(Int.random(in: 100...999))"
                : (model.viewId ?? ""),
            name: name,
            regions: model.regions
        )
        let finish: (Bool) -> Void = { ok in
            model.isSaving = false
            guard ok else { return }
            // 新建成功后记住 id，之后的保存走更新而不是重复创建。
            model.viewId = view.id
            if thenApply {
                // 应用：留在编辑窗口，用户对着墨水屏继续调整。
                host.switchView(to: view.id) {
                    model.showStatus("已应用到墨水屏")
                }
            } else {
                // 保存：落盘后关窗（原有交互）。
                ViewEditorWindowController.shared.close()
            }
        }
        if model.isNew {
            host.createView(view, completion: finish)
        } else {
            host.updateView(view, completion: finish)
        }
    }
}

extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}

/// 视图编辑窗口的持有者。窗口独立于设置窗口，尺寸更大，
/// 关闭后保留实例以便再次打开。
final class ViewEditorWindowController {
    static let shared = ViewEditorWindowController()

    private var window: NSWindow?
    private let model = ViewEditorModel()
    /// 内嵌实时预览的内容缓存：预览画在编辑窗口右侧，不再单独开窗。
    private let previewStore = EditorPreviewStore(port: HostService.shared.port)

    /// 打开编辑器。view 为 nil 时新建。同时关掉上一次的预览窗口，
    /// 避免上一个视图的预览留在屏幕上。
    func show(view: SavedView?, host: HostService) {
        let window: NSWindow
        if let existing = self.window {
            window = existing
        } else {
            window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 800, height: 560),
                styleMask: [.titled, .closable],
                backing: .buffered,
                defer: false
            )
            window.title = "编辑视图"
            window.isReleasedWhenClosed = false
            window.center()
            self.window = window
            // 点标题栏红色关闭按钮也走同一套清理：移除预览视图，
            // 停掉每秒一次的 /api/preview 轮询。
            NotificationCenter.default.addObserver(
                forName: NSWindow.willCloseNotification, object: window, queue: .main
            ) { [weak self] _ in
                self?.teardownContent()
            }
        }
        model.load(view)
        // 预览与墨水屏同源：优先用设备实测宽高比，打开时再同步一次最新值。
        // model 是控制器持有的单例，闭包里显式写 self 无循环引用风险。
        model.applyDeviceAspect(host.deviceAspect)
        host.refreshDeviceAspect { aspect in
            self.model.applyDeviceAspect(aspect)
        }
        let hosting = NSHostingView(rootView: ViewEditorView(model: model, host: host, previewStore: previewStore))
        window.contentView = hosting
        window.setContentSize(NSSize(width: 800, height: 560))
        NSApp.setActivationPolicy(.regular)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func close() {
        window?.orderOut(nil)
        teardownContent()
    }

    /// 关窗清理：替换 contentView 让 SwiftUI 视图树销毁，预览的 1s 轮询、
    /// 250ms 去抖订阅随视图生命周期一起取消（否则窗口只是 orderOut，
    /// 视图仍存活，每秒都会向 host 发 /api/preview，每次触发插件渲染）；
    /// 同时清空预览缓存，避免下次打开闪现上一个视图的内容。
    private func teardownContent() {
        if window?.contentView is NSHostingView<ViewEditorView> {
            window?.contentView = NSView()
        }
        previewStore.reset()
    }
}
