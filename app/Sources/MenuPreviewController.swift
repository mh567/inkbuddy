import AppKit

/// 菜单下拉里的预览头部：上一行是 ◀ / 当前视图名 / ▶，下方显示墨水屏内容。
///
/// 不用 WebKit：WKWebView 放进 NSMenu 后渲染层会被系统回收，显示几秒就变白。
/// 直接拉 /api/frame 的结构化区域数据（与墨水屏同一份渲染结果），按百分比
/// 原生排版——和视图编辑器画布同一套几何模型。
final class MenuPreviewController: NSObject {
    let containerView: NSView

    private let prevButton: NSButton
    private let nextButton: NSButton
    private let nameLabel: NSTextField
    private let canvasView: NSView

    private var pollTimer: Timer?
    private var baseURL = ""
    private var lastSignature: String?

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

        canvasView = NSView(frame: .zero)

        super.init()

        let barHeight: CGFloat = 26
        let top = height
        prevButton.frame = NSRect(x: 8, y: top - barHeight - 2, width: 32, height: barHeight)
        nextButton.frame = NSRect(x: width - 40, y: top - barHeight - 2, width: 32, height: barHeight)
        nameLabel.frame = NSRect(x: 44, y: top - barHeight, width: width - 88, height: barHeight - 4)
        let inset: CGFloat = 8
        canvasView.frame = NSRect(
            x: inset,
            y: 6,
            width: width - inset * 2,
            height: height - barHeight - 14
        )

        containerView.addSubview(prevButton)
        containerView.addSubview(nameLabel)
        containerView.addSubview(nextButton)
        containerView.addSubview(canvasView)

        prevButton.target = self
        prevButton.action = #selector(prevClicked)
        nextButton.target = self
        nextButton.action = #selector(nextClicked)
    }

    @objc private func prevClicked() { onPrev?() }
    @objc private func nextClicked() { onNext?() }

    /// 菜单打开时调用：记录地址，立即拉一帧，之后每 500ms 刷新。
    func startPreview(address: String) {
        baseURL = address.hasSuffix("/") ? address : address + "/"
        lastSignature = nil
        poll()
        pollTimer?.invalidate()
        pollTimer = Timer.scheduledTimer(
            timeInterval: 0.5,
            target: self,
            selector: #selector(poll),
            userInfo: nil,
            repeats: true
        )
    }

    /// 菜单关闭时调用：停掉轮询。
    func stopPreview() {
        pollTimer?.invalidate()
        pollTimer = nil
    }

    @objc private func poll() {
        guard let url = URL(string: baseURL + "api/frame") else { return }
        var request = URLRequest(url: url)
        request.timeoutInterval = 2
        URLSession.shared.dataTask(with: request) { [weak self] data, _, _ in
            guard let data = data,
                  let payload = try? JSONDecoder().decode(FramePayload.self, from: data) else { return }
            DispatchQueue.main.async {
                guard let self = self,
                      payload.signature != self.lastSignature else { return }
                self.lastSignature = payload.signature
                self.render(payload.regions)
            }
        }.resume()
    }

    // MARK: - 渲染

    private struct FrameRegion: Decodable {
        struct Line: Decodable {
            let text: String
            let role: String
        }
        let plugin: String
        let x: Double
        let y: Double
        let w: Double
        let h: Double
        /// 服务端算好的字号缩放系数；老版本 host 没有这个字段，回退到启发式。
        let scale: Double?
        let lines: [Line]
    }

    private struct FramePayload: Decodable {
        let regions: [FrameRegion]
        let signature: String?
    }

    /// 按百分比把每个区域画到画布上：一个区域一个文本视图，
    /// 字号按区域高度缩放，保证小区域也能读。
    private func render(_ regions: [FrameRegion]) {
        for sub in canvasView.subviews {
            sub.removeFromSuperview()
        }
        if regions.isEmpty {
            let placeholder = NSTextField(labelWithString: "无可用扩展")
            placeholder.font = NSFont.systemFont(ofSize: 13)
            placeholder.textColor = .secondaryLabelColor
            placeholder.frame = canvasView.bounds
            placeholder.alignment = .center
            canvasView.addSubview(placeholder)
            return
        }
        let bounds = canvasView.bounds
        for region in regions {
            let regionRect = CGRect(
                x: bounds.minX + CGFloat(region.x / 100) * bounds.width,
                y: bounds.minY + (1 - CGFloat(region.y / 100) - CGFloat(region.h / 100)) * bounds.height,
                width: CGFloat(region.w / 100) * bounds.width,
                height: CGFloat(region.h / 100) * bounds.height
            )
            let label = NSTextField(labelWithString: "")
            label.isEditable = false
            label.attributedStringValue = attributedText(for: region, in: regionRect)
            label.frame = regionRect.insetBy(dx: 4, dy: 2)
            label.lineBreakMode = .byWordWrapping
            label.maximumNumberOfLines = 8
            label.alignment = .center
            canvasView.addSubview(label)
        }
    }

    /// 一个区域的富文本。优先用服务端算好的缩放系数（与墨水屏同源），
    /// 老版本 host 没带 scale 时回退到按区域高度的启发式。
    private func attributedText(for region: FrameRegion, in rect: CGRect) -> NSAttributedString {
        let scale = region.scale.map { max(0.3, min(1.0, $0)) }
            ?? max(0.45, min(1.0, Double(rect.height) / 140))
        let result = NSMutableAttributedString()
        for (index, line) in region.lines.enumerated() {
            let font: NSFont
            let color: NSColor
            switch line.role {
            case "primary":
                font = NSFont.systemFont(ofSize: 20 * scale, weight: .bold)
                color = .black
            case "normal", "text":
                font = NSFont.systemFont(ofSize: 15 * scale, weight: .regular)
                color = .black
            case "muted", "reading":
                font = NSFont.systemFont(ofSize: 13 * scale, weight: .regular)
                color = .darkGray
            default:
                font = NSFont.systemFont(ofSize: 15 * scale, weight: .regular)
                color = .black
            }
            result.append(NSAttributedString(
                string: line.text,
                attributes: [.font: font, .foregroundColor: color]
            ))
            if index < region.lines.count - 1 {
                result.append(NSAttributedString(string: "\n"))
            }
        }
        if result.length == 0 {
            result.append(NSAttributedString(
                string: "…",
                attributes: [.font: NSFont.systemFont(ofSize: 12), .foregroundColor: NSColor.lightGray]
            ))
        }
        return result
    }

    func setName(_ text: String) {
        nameLabel.stringValue = text
    }

    deinit {
        pollTimer?.invalidate()
    }
}
