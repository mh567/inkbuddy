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
            // 没有区域就作废在途请求：晚到的响应不该把刚清空的画布又填回去。
            requestSeq += 1
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
        // 复杂视图首次渲染可能要等插件子进程冷启动（每个最长 8 秒），
        // 3 秒会静默失败，画布停在加载态直到下一次轮询。
        request.timeoutInterval = 10
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            guard let data = data,
                  let frame = try? JSONDecoder().decode(PreviewFrame.self, from: data) else {
                return
            }
            DispatchQueue.main.async {
                guard let self = self, seq == self.requestSeq else {
                    return
                }
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
///
/// 只有第一帧走 `loadHTMLString`，之后换成在文档内替换 `#frame`。
/// 整页重载会把 WebView 清成白页再重绘，菜单展开期间 WebKit 不重绘，
/// 白页就会一直停在那里；替换 DOM 不会清空图层，画面不会闪。
/// 每个预览位一个长寿命 WebView：懒创建、跨开关复用。
///
/// 站点的渲染状态（已载入的签名）也放在这里，视图重新挂载后可以直接
/// 继续走文档内替换，不必整页重载。站点关闭时调用 unload() 把页面卸掉，
/// WebContent 进程的占用会从 ~120 MB 掉回 ~11 MB——之前面板每开一次就
/// 新建一个 WebView，旧的 WebContent 进程不回收，空闲进程越攒越多。
final class PreviewWebViewHolder: NSObject, WKNavigationDelegate {
    /// 全站共享一份配置。macOS 12 起 WebKit 自动按 app 复用进程，
    /// 进程池 API 已废弃；共享配置主要是让偏好设置只有一份。
    private static let sharedConfiguration = WKWebViewConfiguration()

    private var _webView: WKWebView?
    /// 卸载轮次：unload 之后，在途的 DOM 替换回调直接作废，
    /// 不再把旧内容灌回刚卸干净的页面。
    private(set) var epoch = 0

    /// 首次挂载时创建。unload() 在预览从未显示过时直接返回，
    /// 不会为一个空白站点专门拉起 WebContent 进程。
    var webView: WKWebView {
        if let web = _webView { return web }
        let web = WKWebView(frame: .zero, configuration: Self.sharedConfiguration)
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        web.navigationDelegate = self
        _webView = web
        return web
    }

    /// 已载入内容的签名；空串表示当前没有有效内容，下一帧走整页载入。
    var signature: String = ""
    /// 是否已载入过整页；载入过之后只替换 `#frame`。
    var loaded = false

    /// 站点关闭时卸载内容，释放 WebContent 里的页面内存。
    func unload() {
        signature = ""
        loaded = false
        epoch += 1
        guard let web = _webView else { return }
        web.loadHTMLString("<!DOCTYPE html><html><body></body></html>", baseURL: nil)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        loaded = false
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        loaded = false
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        loaded = false
        signature = ""
    }
}

private struct InkPreviewWebView: NSViewRepresentable {
    let holder: PreviewWebViewHolder
    let frame: PreviewFrame?

    func makeNSView(context: Context) -> WKWebView {
        // 复用 holder 里的同一个 WebView，重挂载不再新建 WebContent 进程。
        holder.webView
    }

    func updateNSView(_ web: WKWebView, context: Context) {
        guard let frame = frame else {
            // 取数被清空（菜单关掉等）：下次拿到内容时强制刷新一次，
            // 免得签名相同就跳过，画布停在空白上。
            holder.signature = ""
            return
        }
        guard holder.signature != frame.signature else { return }
        holder.signature = frame.signature

        guard holder.loaded else {
            web.loadHTMLString(frame.html, baseURL: nil)
            holder.loaded = true
            return
        }

        // 文档已经在，替换内容区即可，不重载整页。
        guard let payload = Self.jsString(frame.html) else { return }
        let script = """
        (function () {
          var next = new DOMParser().parseFromString(\(payload), 'text/html');
          var incoming = next.getElementById('frame');
          var current = document.getElementById('frame');
          if (!incoming || !current) { return false; }
          current.replaceWith(document.importNode(incoming, true));
          if (next.body) { document.body.className = next.body.className; }
          return true;
        })()
        """
        let epoch = holder.epoch
        web.evaluateJavaScript(script) { result, _ in
            // 卸载发生在请求之后：结果作废，别把旧内容灌回空白页。
            guard holder.epoch == epoch else { return }
            // 出错或页面里找不到 #frame 时退回整页重载，下次重新走载入分支。
            if !(result as? Bool ?? false) {
                Self.reload(web, holder: holder, frame: frame)
            }
        }
    }

    /// 整页重载：替换 DOM 走不通时的兜底，并重新记下当前签名。
    private static func reload(_ web: WKWebView, holder: PreviewWebViewHolder, frame: PreviewFrame) {
        holder.signature = frame.signature
        holder.loaded = true
        web.loadHTMLString(frame.html, baseURL: nil)
    }

    /// 把一段文本编码成可以嵌进 JS 的字面量。
    private static func jsString(_ text: String) -> String? {
        guard let data = try? JSONSerialization.data(
            withJSONObject: text,
            options: .fragmentsAllowed
        ) else { return nil }
        return String(data: data, encoding: .utf8)
    }
}

/// 墨水屏预览画布：WebKit 版面 + 可选的编辑辅助线（区域边框和扩展名）。
/// 编辑器里显示辅助线，菜单栏里关掉，两处共用同一份页面渲染。
struct InkPreviewCanvas: View {
    let regions: [ViewRegion]
    let frame: PreviewFrame?
    let aspect: Double
    /// 本预览位的 WebView；由调用方持有，跨开关复用。
    let holder: PreviewWebViewHolder

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
            InkPreviewWebView(holder: holder, frame: frame)
            if regions.isEmpty {
                Text("画布上还没有扩展")
                    .font(.system(size: 12))
                    .foregroundColor(.secondary)
            } else if frame == nil {
                // 画布还留着上一次的内容，盖一层半透明白 + 转圈，
                // 切换视图时有明确的「正在加载」，不再像没反应。
                ZStack {
                    Color.white.opacity(0.82)
                    VStack(spacing: 6) {
                        ProgressView()
                            .controlSize(.small)
                        Text("正在加载…")
                            .font(.system(size: 11))
                            .foregroundColor(.secondary)
                    }
                }
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
    let holder: PreviewWebViewHolder

    private let timer = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    var body: some View {
        InkPreviewCanvas(
            regions: model.regions,
            frame: store.frame,
            aspect: model.canvasAspect,
            holder: holder
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
