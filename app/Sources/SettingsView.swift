import SwiftUI

// MARK: - 设计常量

private enum Theme {
    static let ink = Color(red: 0.145, green: 0.129, blue: 0.361)
    static let radius: CGFloat = 10
    static let pageInset: CGFloat = 16
    static let groupGap: CGFloat = 18
    static let rowInset: CGFloat = 12
    static let rowHeight: CGFloat = 9
}

/// 状态语义色。同一个 tone 同时决定文字、图标和底色，避免各 Tab 自己配色。
private enum Tone {
    case brand, positive, caution, negative, muted

    var ink: Color {
        switch self {
        case .brand:    return Theme.ink
        case .positive: return Color(red: 0.13, green: 0.44, blue: 0.24)
        case .caution:  return Color(red: 0.63, green: 0.38, blue: 0.05)
        case .negative: return Color(red: 0.62, green: 0.17, blue: 0.17)
        case .muted:    return Color(NSColor.secondaryLabelColor)
        }
    }

    var wash: Color { ink.opacity(0.10) }
}

// MARK: - 基础组件（四个 Tab 共用）

/// 分组标题，右侧可放数量或补充信息。
private struct GroupHeader: View {
    let title: String
    var detail: String?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(title)
                .font(.system(size: 11, weight: .semibold))
                .foregroundColor(Color(NSColor.secondaryLabelColor))
            Spacer(minLength: 6)
            if let detail {
                Text(detail)
                    .font(.system(size: 10))
                    .foregroundColor(Color(NSColor.tertiaryLabelColor))
            }
        }
    }
}

/// 状态徽标：运行中 / 当前 / 已停用 等。
private struct Pill: View {
    let text: String
    let tone: Tone

    var body: some View {
        Text(text)
            .font(.system(size: 10, weight: .medium))
            .foregroundColor(tone.ink)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(Capsule().fill(tone.wash))
    }
}

private struct StatusDot: View {
    let tone: Tone
    var size: CGFloat = 8

    var body: some View {
        Circle().fill(tone.ink).frame(width: size, height: size)
    }
}

/// 列表行左侧的图标块。
private struct IconTile: View {
    let symbol: String
    let tone: Tone

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 7).fill(tone.wash)
            Image(systemName: symbol)
                .font(.system(size: 13, weight: .medium))
                .foregroundColor(tone.ink)
        }
        .frame(width: 30, height: 30)
    }
}

/// hover 状态由每行自己持有。命令行构建下 @State 宏不可用，@StateObject 是
/// 普通属性包装器，SwiftUI 会在视图生命周期内保留同一个实例，视图重建不会丢状态。
private final class HoverState: ObservableObject {
    @Published var hovering = false
}

/// 列表行：左信息、右操作，鼠标悬停时整行淡入底色。
private struct SettingRow<Leading: View, Trailing: View>: View {
    @StateObject private var hover = HoverState()
    let leading: Leading
    let trailing: Trailing

    init(
        @ViewBuilder leading: () -> Leading,
        @ViewBuilder trailing: () -> Trailing
    ) {
        self.leading = leading()
        self.trailing = trailing()
    }

    var body: some View {
        HStack(spacing: 10) {
            leading
            Spacer(minLength: 8)
            trailing
        }
        .padding(.horizontal, Theme.rowInset)
        .padding(.vertical, Theme.rowHeight)
        .background(
            RoundedRectangle(cornerRadius: 6)
                .fill(hover.hovering ? Theme.ink.opacity(0.055) : Color.clear)
        )
        .onHover { hover.hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hover.hovering)
    }
}

/// 空列表的引导：说明原因和下一步操作。
private struct EmptyHint: View {
    let symbol: String
    let title: String
    let hint: String

    var body: some View {
        VStack(spacing: 7) {
            Image(systemName: symbol)
                .font(.system(size: 21, weight: .light))
                .foregroundColor(Color(NSColor.tertiaryLabelColor))
            Text(title)
                .font(.system(size: 12, weight: .medium))
            Text(hint)
                .font(.system(size: 11))
                .foregroundColor(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 18)
        .padding(.horizontal, 12)
    }
}

/// 用视图已有的区域坐标画一张小布局图，让视图列表不再只有一行文字。
private struct RegionMap: View {
    let regions: [ViewRegion]

    var body: some View {
        GeometryReader { g in
            ZStack(alignment: .topLeading) {
                RoundedRectangle(cornerRadius: 3).fill(Color(NSColor.controlBackgroundColor))
                ForEach(regions) { region in
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(Theme.ink.opacity(0.5))
                        .frame(
                            width: g.size.width * CGFloat(region.w / 100),
                            height: g.size.height * CGFloat(region.h / 100)
                        )
                        .offset(
                            x: g.size.width * CGFloat(region.x / 100),
                            y: g.size.height * CGFloat(region.y / 100)
                        )
                }
            }
        }
        .frame(width: 40, height: 26)
        .overlay(
            RoundedRectangle(cornerRadius: 3)
                .strokeBorder(Color(NSColor.separatorColor), lineWidth: 0.5)
        )
    }
}

// MARK: - 设置窗口

struct SettingsView: View {
    @EnvironmentObject var state: AppState
    @EnvironmentObject var host: HostService
    @EnvironmentObject var updater: Updater

    /// 待确认停用的扩展（alert 确认后真正调用停用接口）
    @ObservedObject private var pending = PendingUninstall()

    /// 在线设备摘要：台数、设备短 ID 与对应的宽高比。
    static func deviceSummary(_ devices: [DeviceReport]) -> String {
        let heading = "\(devices.count) 台在线（60 秒内上报）"
        return ([heading] + devices.map(\.displayLabel)).joined(separator: "\n")
    }

    var body: some View {
        VStack(spacing: 0) {
            header
                .padding(.horizontal, 20)
                .padding(.top, 16)
                .padding(.bottom, 4)
            TabView {
                serviceTab.tabItem { Label("服务", systemImage: "antenna.radiowaves.left.and.right") }
                extensionsTab.tabItem { Label("扩展", systemImage: "puzzlepiece") }
                viewsTab.tabItem { Label("视图", systemImage: "square.stack") }
                generalTab.tabItem { Label("通用", systemImage: "gearshape") }
            }
        }
        .frame(width: 440, height: 560)
        .onAppear {
            host.fetchPlugins()
            host.fetchViews()
            host.refreshDevices()
            updater.check()
        }
        .alert(
            "停用扩展",
            isPresented: Binding(
                get: { pending.plugin != nil },
                set: { if !$0 { pending.plugin = nil } }
            ),
            presenting: pending.plugin
        ) { plugin in
            Button("停用") {
                host.setEnabled(plugin.id, enabled: false)
                pending.plugin = nil
            }
            Button("取消", role: .cancel) { pending.plugin = nil }
        } message: { plugin in
            Text("停用「\(plugin.name)」后它不再读取任何数据，墨水屏也不再显示它，视图里引用它的区域会被跳过。扩展文件仍保留在本机，随时可以再启用。")
        }
    }

    // MARK: 头部

    private var header: some View {
        HStack(spacing: 12) {
            ZStack {
                Circle().fill(Theme.ink.opacity(0.12)).frame(width: 46, height: 46)
                Circle().fill(Theme.ink.opacity(0.22)).frame(width: 32, height: 32)
                Path { p in
                    p.move(to: CGPoint(x: 23, y: 6))
                    p.addCurve(to: CGPoint(x: 15, y: 25), control1: CGPoint(x: 23, y: 6), control2: CGPoint(x: 15, y: 17))
                    p.addArc(center: CGPoint(x: 23, y: 25), radius: 8, startAngle: .degrees(180), endAngle: .degrees(0), clockwise: true)
                    p.addCurve(to: CGPoint(x: 23, y: 6), control1: CGPoint(x: 31, y: 17), control2: CGPoint(x: 23, y: 6))
                }
                .fill(Theme.ink)
                .frame(width: 46, height: 46)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text("inkbuddy").font(.system(size: 19, weight: .semibold))
                Text("墨水屏副屏扩展").font(.system(size: 12)).foregroundColor(.secondary)
            }
            Spacer()
            HStack(spacing: 6) {
                Text(host.statusText)
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                StatusDot(tone: host.running ? .positive : .muted)
            }
        }
    }

    // MARK: 卡片容器

    private func card<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        VStack(spacing: 0) { content() }
            .padding(.vertical, 4)
            .background(RoundedRectangle(cornerRadius: Theme.radius).fill(Color(NSColor.controlBackgroundColor)))
    }

    private func page<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: Theme.groupGap) { content() }
                .padding(Theme.pageInset)
        }
    }

    // MARK: 服务 Tab

    private var serviceTab: some View {
        page {
            serviceSection
            updateSection
        }
    }

    private var serviceSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            GroupHeader(title: "服务", detail: host.running ? "运行中" : "已停止")
            card {
                SettingRow(
                    leading: {
                        IconTile(symbol: host.running ? "antenna.radiowaves.left.and.right" : "zzz", tone: host.running ? .positive : .muted)
                        VStack(alignment: .leading, spacing: 2) {
                            HStack(spacing: 6) {
                                Text(host.running ? "副屏服务已启动" : "副屏服务未启动")
                                    .font(.system(size: 13, weight: .medium))
                                StatusDot(tone: host.running ? .positive : .muted, size: 6)
                            }
                            Text(host.running ? "墨水屏可以访问下面的地址" : "打开开关后墨水屏才能访问")
                                .font(.system(size: 11))
                                .foregroundColor(.secondary)
                        }
                    },
                    trailing: {
                        Toggle("启用副屏服务", isOn: Binding(
                            get: { host.running },
                            set: { on in on ? host.start() : host.stop() }
                        ))
                        .toggleStyle(.switch)
                        .labelsHidden()
                        .accessibilityLabel("启用副屏服务")
                    }
                )

                Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)

                SettingRow(
                    leading: {
                        IconTile(symbol: "link", tone: .brand)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("墨水屏访问地址")
                                .font(.system(size: 12, weight: .medium))
                            Text(host.lanAddress.isEmpty ? "检测中…" : host.lanAddress)
                                .font(.system(size: 11, design: .monospaced))
                                .foregroundColor(.secondary)
                                .textSelection(.enabled)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                    },
                    trailing: {
                        HStack(spacing: 6) {
                            Button {
                                host.copyAddress()
                                pending.markCopied()
                            } label: {
                                Label(pending.addressCopied ? "已复制" : "复制",
                                      systemImage: pending.addressCopied ? "checkmark" : "doc.on.doc")
                                    .font(.system(size: 11))
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                            .tint(pending.addressCopied ? Tone.positive.ink : Theme.ink)
                            .animation(.easeOut(duration: 0.15), value: pending.addressCopied)
                        }
                    }
                )

                if !host.devices.isEmpty {
                    Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)
                    SettingRow(
                        leading: {
                            IconTile(symbol: "ruler", tone: .brand)
                            VStack(alignment: .leading, spacing: 2) {
                                Text("在线设备")
                                    .font(.system(size: 12, weight: .medium))
                                Text(Self.deviceSummary(host.devices))
                                    .font(.system(size: 11))
                                    .foregroundColor(.secondary)
                            }
                        },
                        trailing: { EmptyView() }
                    )
                }

                Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)

                SettingRow(
                    leading: {
                        IconTile(symbol: "arrow.clockwise", tone: .muted)
                        Text("重启服务")
                            .font(.system(size: 12, weight: .medium))
                    },
                    trailing: {
                        Button("重启") { host.restart() }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                    }
                )
            }
        }
    }

    // MARK: 扩展 Tab

    private var extensionsTab: some View {
        page {
            VStack(alignment: .leading, spacing: 8) {
                GroupHeader(
                    title: "扩展",
                    detail: "\(host.plugins.filter { $0.enabled }.count) / \(host.plugins.count) 已启用"
                )
                card {
                    if host.plugins.isEmpty {
                        EmptyHint(
                            symbol: "puzzlepiece",
                            title: host.running ? "未发现扩展" : "服务未启动",
                            hint: host.running ? "把扩展放到本机插件目录后，重新打开这个窗口" : "先到「服务」打开副屏服务，再回来看扩展列表"
                        )
                    } else {
                        ForEach(host.plugins) { plugin in
                            pluginRow(plugin)
                            if plugin.id != host.plugins.last?.id {
                                Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)
                            }
                        }
                    }
                }
            }
        }
    }

    private func pluginRow(_ plugin: PluginInfo) -> some View {
        // 停用是用户主动选的，用中性色；启用了却没跑起来才是要提示的问题。
        let tone: Tone = !plugin.enabled ? .muted : (plugin.running ? .positive : .caution)
        let status = !plugin.enabled ? "已停用" : (plugin.running ? "运行中" : "未运行")

        return SettingRow(
            leading: {
                IconTile(symbol: "puzzlepiece", tone: tone)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(plugin.name)
                            .font(.system(size: 13, weight: .medium))
                        Pill(text: status, tone: tone)
                    }
                    Text(plugin.description)
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .help(plugin.description)
                }
            },
            trailing: {
                if plugin.enabled {
                    Button("停用") { pending.plugin = plugin }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                } else {
                    Button("启用") { host.setEnabled(plugin.id, enabled: true) }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .tint(Theme.ink)
                        .disabled(!plugin.available)
                        .help(plugin.available ? "启用后开始取数" : "这款扩展的清单里已禁用，本机无法启用")
                }
            }
        )
        .opacity(plugin.enabled ? 1 : 0.75)
    }

    // MARK: 视图 Tab（列表 + 应用；编辑在独立大窗口进行）

    private var viewsTab: some View {
        page {
            VStack(alignment: .leading, spacing: 8) {
                GroupHeader(title: "视图", detail: "\(host.viewsState.views.count) 个布局")
                card {
                    if host.viewsState.views.isEmpty {
                        EmptyHint(
                            symbol: "square.stack",
                            title: host.running ? "还没有视图" : "服务未启动",
                            hint: host.running ? "新建一个视图，把扩展排成想要的版面" : "先到「服务」打开副屏服务，再回来创建视图"
                        )
                    } else {
                        ForEach(host.viewsState.views) { view in
                            viewRow(view)
                            if view.id != host.viewsState.views.last?.id {
                                Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)
                            }
                        }
                    }

                    Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)

                    Button {
                        ViewEditorWindowController.shared.show(view: nil, host: host)
                    } label: {
                        HStack(spacing: 8) {
                            Image(systemName: "plus.circle")
                                .font(.system(size: 13))
                                .foregroundColor(Theme.ink)
                            Text("新建视图")
                                .font(.system(size: 12, weight: .medium))
                            Spacer()
                        }
                        .padding(.horizontal, Theme.rowInset)
                        .padding(.vertical, Theme.rowHeight)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    private func viewRow(_ view: SavedView) -> some View {
        let isCurrent = view.id == host.viewsState.current
        let missing = host.missingPluginIds(in: view)

        return SettingRow(
            leading: {
                RegionMap(regions: view.regions)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(view.name)
                            .font(.system(size: 13, weight: .medium))
                        if isCurrent {
                            Pill(text: "当前", tone: .positive)
                        }
                    }
                    let summary = viewSummary(view)
                    Text(summary)
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .help(summary)
                    if !missing.isEmpty {
                        let missingText = "缺少扩展：\(missing.joined(separator: "、"))"
                        Text(missingText)
                            .font(.system(size: 10))
                            .foregroundColor(Tone.caution.ink)
                            .lineLimit(1)
                            .help(missingText)
                    }
                }
            },
            trailing: {
                HStack(spacing: 6) {
                    Button("应用") { host.switchView(to: view.id) }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                        .tint(Theme.ink)
                    Button {
                        ViewEditorWindowController.shared.show(view: view, host: host)
                    } label: {
                        Image(systemName: "pencil")
                    }
                    .buttonStyle(.borderless)
                    .help("编辑视图")
                    Button(role: .destructive) { host.deleteView(view.id) } label: {
                        Image(systemName: "trash")
                    }
                    .buttonStyle(.borderless)
                    .help("删除视图")
                }
            }
        )
    }

    /// 布局摘要：每个区域一个扩展名，已停用的显示原始 id。
    private func viewSummary(_ view: SavedView) -> String {
        view.regions
            .map { host.pluginName(for: $0.plugin) ?? $0.plugin }
            .joined(separator: " + ")
    }

    // MARK: 更新

    private var updateSection: some View {
        let status = updateStatus
        return VStack(alignment: .leading, spacing: 8) {
            GroupHeader(title: "更新")
            card {
                SettingRow(
                    leading: {
                        IconTile(symbol: status.symbol, tone: status.tone)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(status.title)
                                .font(.system(size: 12, weight: .medium))
                            Text(status.detail)
                                .font(.system(size: 11))
                                .foregroundColor(.secondary)
                        }
                    },
                    trailing: { updateButton }
                )
            }
        }
    }

    private var updateStatus: (symbol: String, tone: Tone, title: String, detail: String) {
        switch updater.state {
        case .idle:
            return ("arrow.triangle.2.circlepath", .muted,
                    "当前版本 v\(updater.currentVersion)", "尚未检查更新")
        case .checking:
            return ("arrow.triangle.2.circlepath", .brand,
                    "正在检查更新…", "正在连接更新服务器")
        case .upToDate:
            return ("checkmark.circle", .positive,
                    "当前版本 v\(updater.currentVersion)", "已是最新版本")
        case .available(let version):
            return ("arrow.down.circle", .brand,
                    "有新版本 v\(version)", "当前 v\(updater.currentVersion)，可一键更新")
        case .downloading:
            return ("arrow.down.circle", .brand,
                    "正在下载新版本…", "完成后会自动安装")
        case .installing:
            return ("gear", .brand,
                    "正在安装，稍后自动重启", "不要强制退出应用")
        case .failed(let msg):
            return ("exclamationmark.triangle", .negative, "更新失败", msg)
        }
    }

    @ViewBuilder private var updateButton: some View {
        switch updater.state {
        case .available:
            Button("一键更新") { updater.install() }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .tint(Theme.ink)
        case .checking, .downloading, .installing:
            ProgressView().controlSize(.small)
        default:
            Button("检查更新") { updater.check() }
                .buttonStyle(.bordered)
                .controlSize(.small)
        }
    }

    // MARK: 通用 Tab

    private var generalTab: some View {
        page {
            VStack(alignment: .leading, spacing: 8) {
                GroupHeader(title: "通用", detail: "外观与启动")
                card {
                    SettingRow(
                        leading: {
                            IconTile(symbol: "menubar.rectangle", tone: .brand)
                            VStack(alignment: .leading, spacing: 2) {
                                Text("关闭窗口后保留图标")
                                    .font(.system(size: 12, weight: .medium))
                                Text("只留在菜单栏，不退出应用")
                                    .font(.system(size: 11))
                                    .foregroundColor(.secondary)
                            }
                        },
                        trailing: {
                            Toggle("关闭窗口后保留图标", isOn: $state.stayInMenuBar)
                                .toggleStyle(.switch)
                                .labelsHidden()
                                .accessibilityLabel("关闭窗口后保留图标")
                        }
                    )

                    Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)

                    SettingRow(
                        leading: {
                            IconTile(symbol: "power", tone: .brand)
                            VStack(alignment: .leading, spacing: 2) {
                                Text("开机启动")
                                    .font(.system(size: 12, weight: .medium))
                                Text("登录系统时自动运行")
                                    .font(.system(size: 11))
                                    .foregroundColor(.secondary)
                            }
                        },
                        trailing: {
                            Toggle("开机启动", isOn: $state.launchAtLogin)
                                .toggleStyle(.switch)
                                .labelsHidden()
                                .accessibilityLabel("开机启动")
                        }
                    )

                    if let err = state.lastLaunchError {
                        Divider().padding(.leading, 54).padding(.trailing, Theme.rowInset)
                        SettingRow(
                            leading: {
                                IconTile(symbol: "exclamationmark.triangle", tone: .negative)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text("登录项注册失败")
                                        .font(.system(size: 12, weight: .medium))
                                    Text(err)
                                        .font(.system(size: 11))
                                        .foregroundColor(Tone.negative.ink)
                                        .lineLimit(2)
                                }
                            },
                            trailing: { EmptyView() }
                        )
                    }
                }
            }
        }
    }
}

/// alert 的 presenting 目标与复制反馈（命令行构建下 @State 不可用，用 ObservableObject 承载）。
final class PendingUninstall: ObservableObject {
    @Published var plugin: PluginInfo?
    @Published var addressCopied = false
    private var copyWork: DispatchWorkItem?

    /// 连点复制时取消上一次的清除任务，避免第一次点击把第二次的反馈提前关掉。
    func markCopied() {
        copyWork?.cancel()
        addressCopied = true
        let work = DispatchWorkItem { [weak self] in
            self?.addressCopied = false
        }
        copyWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.6, execute: work)
    }
}
