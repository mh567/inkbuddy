import SwiftUI
import AppKit
import Combine

/// /api/preview 的单条预览文本行。
struct PreviewLine: Codable {
    let text: String
    let role: String
}

/// /api/preview 的单个扩展内容。
struct PreviewContent: Codable {
    let plugin: String
    let lines: [PreviewLine]
    /// 按块分组的内容（新 host 才有）：块内行紧挨着，块间留间距，
    /// 与墨水屏页面 .block + .block margin-top 语义一致。
    let blocks: [PreviewBlock]?
    /// 内容按基准字号需要的高度（正方形屏幕下的 vh），
    /// 配合画布宽高比算 fit 缩放，公式与 host 一致。
    let contentUnits: Double
    /// 服务端当前是否为纯文本模式（老 host 没有该字段）：隐藏拼音行，
    /// 标题改为 5vw 粗体，与设备 body.mode-text 的 CSS 一致。
    let textMode: Bool?
}

/// /api/preview 的一个内容块。
struct PreviewBlock: Codable {
    let lines: [PreviewLine]
}

/// 预览内容缓存：把当前编辑中的区域里的扩展 id 发给 host，
/// 拿回每个扩展此刻的结构化文本行。编辑布局时去抖刷新，
/// 窗口开着时定时轮询，保证时钟走动、歌词切换也能看到。
final class EditorPreviewStore: ObservableObject {
    @Published private(set) var contents: [String: PreviewContent] = [:]
    private let port: Int
    /// 请求序号：localhost 上响应一般有序，但去抖刷新与定时轮询并发时
    /// 仍可能乱序返回，旧响应晚到会覆盖新数据，用序号丢弃过期响应。
    private var requestSeq = 0

    init(port: Int) {
        self.port = port
    }

    /// 清空缓存并作废在途请求。窗口关闭时调用，避免旧视图的内容
    /// 在下次打开时闪现。
    func reset() {
        requestSeq += 1
        contents = [:]
    }

    func fetch(plugins: [String]) {
        guard !plugins.isEmpty,
              let url = URL(string: "http://127.0.0.1:\(port)/api/preview") else { return }
        requestSeq += 1
        let seq = requestSeq
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["plugins": plugins])
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            guard let data = data,
                  let list = try? JSONDecoder().decode([PreviewContent].self, from: data) else { return }
            DispatchQueue.main.async {
                guard let self = self, seq == self.requestSeq else { return }
                var map: [String: PreviewContent] = [:]
                for item in list {
                    map[item.plugin] = item
                }
                self.contents = map
            }
        }.resume()
    }
}

/// 编辑器的实时预览画布：与编辑画布同一套百分比几何模型，
/// 内容用与墨水屏同源的结构化文本行原生排版。
/// 字号按区域面积占比缩放（与 host 的 ink_scale 同公式）。
/// 内嵌在编辑窗口右侧数值面板下方使用，不再单独开窗。
struct EditorPreviewView: View {
    @ObservedObject var model: ViewEditorModel
    @ObservedObject var store: EditorPreviewStore

    private let timer = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    var body: some View {
        GeometryReader { geo in
            let size = fittedCanvasSize(in: geo.size)
            canvas(size: size)
                .frame(width: size.width, height: size.height)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .padding(10)
        .onAppear(perform: fetchNow)
        .onReceive(model.$regions.debounce(for: .milliseconds(250), scheduler: DispatchQueue.main)) { _ in
            fetchNow()
        }
        .onReceive(timer) { _ in fetchNow() }
    }

    private func fetchNow() {
        store.fetch(plugins: model.regions.map { $0.plugin })
    }

    /// 画布按编辑器选定的宽高比适配窗口（与编辑画布一致）。
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

    private func canvas(size: CGSize) -> some View {
        ZStack {
            Rectangle()
                .fill(Color.white)
                .overlay(Rectangle().strokeBorder(Color.black.opacity(0.25), lineWidth: 1))
            if model.regions.isEmpty {
                Text("画布上还没有扩展")
                    .font(.system(size: 12))
                    .foregroundColor(.secondary)
            }
            ForEach(Array(model.regions.enumerated()), id: \.offset) { _, region in
                previewRegion(region: region, canvas: size)
            }
        }
    }

    @ViewBuilder private func previewRegion(region: ViewRegion, canvas: CGSize) -> some View {
        let width = region.w / 100 * Double(canvas.width)
        let height = region.h / 100 * Double(canvas.height)
        // 与 host region_scale 同公式：宽度占比与高度装得下内容两个约束取小。
        // 画布宽高比就是设备实测比例，与墨水屏一致时预览即所见即所得。
        let content = store.contents[region.plugin]
        let textMode = content?.textMode ?? false
        let widthScale = max(0.1, min(1.0, region.w / 100))
        let fitScale: Double = {
            guard let units = content?.contentUnits, units > 0.01, region.h > 0.01 else { return 1.0 }
            return max(0.1, min(1.0, region.h / (units * model.canvasAspect)))
        }()
        let scale = min(widthScale, fitScale)
        // 块结构：老 host 没有 blocks 时退化为把摊平行当一块。
        // 纯文本模式下隐藏拼音行（设备 body.mode-text .line-reading display:none）。
        let applyTextMode: ([PreviewLine]) -> [PreviewLine] = { textMode ? $0.filter { $0.role != "reading" } : $0 }
        let blocks: [[PreviewLine]] = {
            if let bs = content?.blocks, !bs.isEmpty {
                return bs.map { applyTextMode($0.lines) }.filter { !$0.isEmpty }
            }
            if let lines = content?.lines, !lines.isEmpty {
                let filtered = applyTextMode(lines)
                return filtered.isEmpty ? [] : [filtered]
            }
            return []
        }()
        ZStack {
            Rectangle()
                .strokeBorder(Color.blue.opacity(0.30), lineWidth: 1)
            // 块间距复刻页面 .block + .block margin-top: 2vh（乘 scale）。
            VStack(spacing: max(1, 2 * scale * Double(canvas.height) / 100)) {
                if blocks.isEmpty {
                    Text("…")
                        .font(.system(size: 11))
                        .foregroundColor(.gray)
                } else {
                    ForEach(Array(blocks.enumerated()), id: \.offset) { _, lines in
                        VStack(spacing: 0) {
                            ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                                lineView(line, scale: scale, canvas: canvas, textMode: textMode)
                            }
                        }
                    }
                }
            }
            .padding(4)
            // 扩展名标签，与编辑画布一致
            VStack {
                HStack {
                    Spacer()
                    Text(region.plugin)
                        .font(.system(size: 8, weight: .medium))
                        .padding(.horizontal, 4)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Color.white.opacity(0.85)))
                }
                Spacer()
            }
            .padding(2)
            .allowsHitTesting(false)
        }
        .frame(width: width, height: height)
        .position(
            x: (region.x + region.w / 2) / 100 * Double(canvas.width),
            y: (region.y + region.h / 2) / 100 * Double(canvas.height)
        )
    }

    /// 复刻墨水屏页面的行排版：字号、行高、外边距、最小高度全部按
    /// ink.html 的 CSS 规则（乘 scale），vw/vh 按预览画布尺寸换算。
    /// 与服务端 section_overrides 下发的数值一一对应。
    @ViewBuilder private func lineView(_ line: PreviewLine, scale: Double, canvas: CGSize, textMode: Bool) -> some View {
        let vw = Double(canvas.width) / 100
        let vh = Double(canvas.height) / 100
        switch line.role {
        case "primary":
            let fs = 6.2 * scale * vw
            bodyText(line.text, size: fs, weight: .bold, lineRatio: 1.4, color: .black)
                // 页面 .line-primary margin: 2.5vh 0
                .padding(.vertical, 2.5 * scale * vh)
        case "kv":
            let fs = 4.0 * scale * vw
            bodyText(line.text, size: fs, weight: .regular, lineRatio: 1.8, color: .black)
        case "muted":
            let fs = 5.0 * scale * vw
            bodyText(line.text, size: fs, weight: .regular, lineRatio: 1.4,
                     color: Color(NSColor(srgbRed: 0.54, green: 0.54, blue: 0.54, alpha: 1)))
                // 页面 .line-muted min-height: 7vw
                .frame(minHeight: 7 * scale * vw)
        case "reading":
            let fs = 4.6 * scale * vw
            bodyText(line.text, size: fs, weight: .regular, lineRatio: 1.25,
                     color: Color(NSColor(srgbRed: 0.31, green: 0.31, blue: 0.31, alpha: 1)))
                // 页面 .line-reading margin: -2.4vh 0 1.8vh（负上边距贴近上一行）
                .padding(.top, -2.4 * scale * vh)
                .padding(.bottom, 1.8 * scale * vh)
        case "text":
            if textMode {
                // body.mode-text .block-text p：5vw 粗体、去衬线、字距 0
                bodyText(line.text, size: 5.0 * scale * vw, weight: .bold, lineRatio: 1.6, color: .black)
            } else {
                let fs = 3.8 * scale * vw
                bodyText(line.text, size: fs, weight: .regular, lineRatio: 1.6, color: .black)
            }
        default:
            let fs = 5.0 * scale * vw
            bodyText(line.text, size: fs, weight: .regular, lineRatio: 1.4, color: .black)
        }
    }

    /// 行基础样式：行高用 lineSpacing 补到目标倍数（SwiftUI 默认约 1.2 倍），
    /// 行宽撑满、内容居中，与页面 .line 的 width:100%/居中一致。
    private func bodyText(_ text: String, size: Double, weight: Font.Weight,
                          lineRatio: Double, color: Color) -> some View {
        let fs = max(5, size)
        return Text(text)
            .font(.system(size: fs, weight: weight))
            .lineSpacing(fs * (lineRatio - 1.2))
            .foregroundColor(color)
            .lineLimit(3)
            .minimumScaleFactor(0.5)
            .frame(maxWidth: .infinity)
    }
}
