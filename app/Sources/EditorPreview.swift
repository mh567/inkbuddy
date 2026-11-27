import SwiftUI
import AppKit
import Combine
import WebKit

/// host `/api/preview/html` 的返回：一份完整 HTML 文档，样式与设备同源。
struct PreviewFrame: Codable {
    let html: String
    let signature: String
}

/// 预览取数：把当前编辑中的区域（含子样式）和画布宽高比发给 host，
/// 拿回与墨水屏完全相同的页面。host 负责渲染，控制端只负责显示。
/// 编辑布局时去抖刷新，窗口开着时定时轮询，时钟走动也能看到。
final class EditorPreviewStore: ObservableObject {
    @Published private(set) var frame: PreviewFrame?
    private let port: Int
    /// 请求序号：去抖刷新与定时轮询并发时响应可能乱序，旧响应晚到
    /// 会覆盖新布局，用序号丢弃过期响应。
    private var requestSeq = 0

    init(port: Int) {
        self.port = port
    }

    /// 清空缓存并作废在途请求。窗口关闭时调用，避免旧视图内容闪现。
    func reset() {
        requestSeq += 1
        frame = nil
    }

    func fetch(regions: [ViewRegion], aspect: Double) {
        guard !regions.isEmpty,
              let url = URL(string: "http://127.0.0.1:\(port)/api/preview/html") else {
            DispatchQueue.main.async { self.frame = nil }
            return
        }
        requestSeq += 1
        let seq = requestSeq
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let payload: [String: Any] = [
            "regions": regions.map { r in
                var item: [String: Any] = [
                    "plugin": r.plugin,
                    "x": r.x, "y": r.y, "w": r.w, "h": r.h
                ]
                if let variant = r.variant, !variant.isEmpty {
                    item["variant"] = variant
                }
                return item
            },
            "aspect": aspect
        ]
        request.httpBody = try? JSONSerialization.data(withJSONObject: payload)
        request.timeoutInterval = 3
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            guard let data = data,
                  let frame = try? JSONDecoder().decode(PreviewFrame.self, from: data) else { return }
            DispatchQueue.main.async {
                guard let self = self, seq == self.requestSeq else { return }
                // 文档相同时不更新，避免 WebView 重载导致预览闪一下。
                if self.frame?.signature != frame.signature {
                    self.frame = frame
                }
            }
        }.resume()
    }
}

/// AppKit 包装：把 host 生成的页面交给 WebKit 渲染。
/// 这是预览与墨水屏一致的根源——两边跑的是同一份 HTML 和同一份 CSS，
/// 控制端不再自己复刻字号、块间距和 HTML 块样式。
private struct InkPreviewWebView: NSViewRepresentable {
    let frame: PreviewFrame?

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var signature: String = ""
    }

    func makeNSView(context: Context) -> WKWebView {
        let web = WKWebView()
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        return web
    }

    func updateNSView(_ web: WKWebView, context: Context) {
        guard let frame = frame else { return }
        guard context.coordinator.signature != frame.signature else { return }
        context.coordinator.signature = frame.signature
        web.loadHTMLString(frame.html, baseURL: nil)
    }
}

/// 墨水屏预览画布：WebKit 版面 + 可选的编辑辅助线（区域边框和扩展名）。
/// 编辑器里显示辅助线，菜单栏里关掉，两处共用同一份页面渲染。
struct InkPreviewCanvas: View {
    let regions: [ViewRegion]
    let frame: PreviewFrame?
    let aspect: Double

    /// 是否画出区域边框与扩展名标签。编辑器里用来对照编辑画布，菜单栏里关掉。
    var showChrome: Bool = true

    var body: some View {
        GeometryReader { geo in
            let size = fittedCanvasSize(in: geo.size)
            canvas(size: size)
                .frame(width: size.width, height: size.height)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    /// 画布按给定的宽高比适配可用空间（与编辑画布一致）。
    private func fittedCanvasSize(in available: CGSize) -> CGSize {
        let a = CGFloat(max(aspect, 0.1))
        var w = available.width
        var h = w / a
        if h > available.height {
            h = available.height
            w = h * a
        }
        return CGSize(width: max(w, 120), height: max(h, 68))
    }

    private func canvas(size: CGSize) -> some View {
        ZStack {
            Rectangle().fill(Color.white)
            InkPreviewWebView(frame: frame)
            if regions.isEmpty {
                Text("画布上还没有扩展")
                    .font(.system(size: 12))
                    .foregroundColor(.secondary)
            } else if frame == nil {
                Text("…")
                    .font(.system(size: 11))
                    .foregroundColor(.gray)
            }
            if showChrome {
                ForEach(Array(regions.enumerated()), id: \.offset) { _, region in
                    chrome(region: region, canvas: size)
                }
            }
            // 墨水屏外框画在最上层：WebView 的页面白底铺满，
            // 描边只有落在它上面才看得见。这圈线表示设备屏幕边界，
            // 独立于区域辅助线，始终显示。
            Rectangle()
                .strokeBorder(Color.black.opacity(0.25), lineWidth: 1)
                .allowsHitTesting(false)
        }
        .clipped()
    }

    /// 编辑辅助线只画边框和名字，内容由 WebKit 按真实区域渲染。
    @ViewBuilder private func chrome(region: ViewRegion, canvas: CGSize) -> some View {
        let width = region.w / 100 * Double(canvas.width)
        let height = region.h / 100 * Double(canvas.height)
        ZStack {
            Rectangle().strokeBorder(Color.blue.opacity(0.30), lineWidth: 1)
            VStack {
                HStack {
                    Spacer()
                    Text(label(for: region))
                        .font(.system(size: 8, weight: .medium))
                        .lineLimit(1)
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
        .allowsHitTesting(false)
    }

    private func label(for region: ViewRegion) -> String {
        guard let variant = region.variant, !variant.isEmpty else { return region.plugin }
        return "\(region.plugin) · \(variant)"
    }
}

/// 编辑器的实时预览：负责取数节奏（编辑去抖 + 定时轮询），
/// 渲染交给 InkPreviewCanvas，与菜单栏共用同一套页面。
struct EditorPreviewView: View {
    @ObservedObject var model: ViewEditorModel
    @ObservedObject var store: EditorPreviewStore

    private let timer = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    var body: some View {
        InkPreviewCanvas(
            regions: model.regions,
            frame: store.frame,
            aspect: model.canvasAspect
        )
        .padding(10)
        .onAppear(perform: fetchNow)
        .onReceive(model.$regions.debounce(for: .milliseconds(250), scheduler: DispatchQueue.main)) { _ in
            fetchNow()
        }
        .onReceive(timer) { _ in fetchNow() }
    }

    private func fetchNow() {
        store.fetch(regions: model.regions, aspect: model.canvasAspect)
    }
}
