import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var state: AppState
    @EnvironmentObject var host: HostService
    @EnvironmentObject var updater: Updater

    private let ink = Color(red: 0.145, green: 0.129, blue: 0.361)

    /// 待确认卸载的扩展（alert 确认后真正调用卸载接口）
    @ObservedObject private var pending = PendingUninstall()

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
            updater.check()
        }
        .alert(
            "卸载扩展",
            isPresented: Binding(
                get: { pending.plugin != nil },
                set: { if !$0 { pending.plugin = nil } }
            ),
            presenting: pending.plugin
        ) { plugin in
            Button("卸载", role: .destructive) {
                host.setInstalled(plugin.id, installed: false)
                pending.plugin = nil
            }
            Button("取消", role: .cancel) { pending.plugin = nil }
        } message: { plugin in
            Text("卸载「\(plugin.name)」后墨水屏不再显示它，视图里引用它的区域也会被跳过。扩展文件仍保留在本机，可随时重新安装。")
        }
    }

    // MARK: - 头部

    private var header: some View {
        HStack(spacing: 12) {
            ZStack {
                Circle().fill(ink.opacity(0.12)).frame(width: 46, height: 46)
                Circle().fill(ink.opacity(0.22)).frame(width: 32, height: 32)
                Path { p in
                    p.move(to: CGPoint(x: 23, y: 6))
                    p.addCurve(to: CGPoint(x: 15, y: 25), control1: CGPoint(x: 23, y: 6), control2: CGPoint(x: 15, y: 17))
                    p.addArc(center: CGPoint(x: 23, y: 25), radius: 8, startAngle: .degrees(180), endAngle: .degrees(0), clockwise: true)
                    p.addCurve(to: CGPoint(x: 23, y: 6), control1: CGPoint(x: 31, y: 17), control2: CGPoint(x: 23, y: 6))
                }
                .fill(ink)
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
                Circle().fill(host.running ? Color.green : Color.gray)
                    .frame(width: 9, height: 9)
            }
        }
    }

    // MARK: - 卡片容器

    private func card<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        VStack(spacing: 0) { content() }
            .padding(.horizontal, 12)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color(NSColor.controlBackgroundColor)))
    }

    private func sectionTitle(_ text: String) -> some View {
        Text(text)
            .font(.system(size: 12, weight: .medium))
            .foregroundColor(.secondary)
    }

    // MARK: - 服务 tab

    private var serviceTab: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                serviceSection
                updateSection
            }
            .padding(16)
        }
    }

    private var serviceSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("服务")
            card {
                Toggle("启用副屏扩展", isOn: Binding(
                    get: { host.running },
                    set: { on in on ? host.start() : host.stop() }
                ))
                .toggleStyle(.switch)
                .padding(.vertical, 10)
                Divider()
                VStack(alignment: .leading, spacing: 4) {
                    Text("墨水屏浏览器输入这个地址")
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                    HStack(spacing: 6) {
                        Text(host.lanAddress.isEmpty ? "检测中…" : host.lanAddress)
                            .font(.system(size: 12, design: .monospaced))
                            .textSelection(.enabled)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer(minLength: 4)
                        Button("复制") { host.copyAddress() }
                            .buttonStyle(.borderless)
                            .controlSize(.small)
                        Button("重启服务") { host.restart() }
                            .buttonStyle(.borderless)
                            .controlSize(.small)
                    }
                }
                .padding(.vertical, 10)
            }
        }
    }

    // MARK: - 扩展 tab（只做安装/卸载，不碰视图）

    private var extensionsTab: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                sectionTitle("扩展")
                Text("卸载后墨水屏不再显示该扩展；扩展文件保留在本机，随时可重新安装")
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                card {
                    if host.plugins.isEmpty {
                        Text(host.running ? "未发现扩展" : "启动服务后显示扩展列表")
                            .font(.system(size: 12))
                            .foregroundColor(.secondary)
                            .frame(maxWidth: .infinity, alignment: .center)
                            .padding(.vertical, 16)
                    } else {
                        ForEach(host.plugins) { plugin in
                            pluginRow(plugin)
                            if plugin.id != host.plugins.last?.id {
                                Divider()
                            }
                        }
                    }
                }
            }
            .padding(16)
        }
    }

    private func pluginRow(_ plugin: PluginInfo) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(plugin.name)
                        .font(.system(size: 13))
                    if plugin.installed && !plugin.running {
                        Text("未运行")
                            .font(.system(size: 10))
                            .foregroundColor(.orange)
                    }
                }
                Text(plugin.description)
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 6)
            if plugin.installed {
                Button("卸载", role: .destructive) { pending.plugin = plugin }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            } else {
                Button("安装") { host.setInstalled(plugin.id, installed: true) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
            }
        }
        .opacity(plugin.installed ? 1 : 0.65)
        .padding(.vertical, 10)
    }

    // MARK: - 视图 tab（列表 + 应用；编辑在独立大窗口进行）

    private var viewsTab: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                sectionTitle("视图")
                Text("视图是扩展的区域布局；「应用」让墨水屏按这个布局显示")
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                card {
                    if host.viewsState.views.isEmpty {
                        Text(host.running ? "暂无视图，点下方按钮创建" : "启动服务后显示视图")
                            .font(.system(size: 12))
                            .foregroundColor(.secondary)
                            .frame(maxWidth: .infinity, alignment: .center)
                            .padding(.vertical, 16)
                    } else {
                        ForEach(host.viewsState.views) { view in
                            viewRow(view)
                            if view.id != host.viewsState.views.last?.id {
                                Divider()
                            }
                        }
                    }
                    Divider()
                    Button {
                        ViewEditorWindowController.shared.show(view: nil, host: host)
                    } label: {
                        Label("新建视图", systemImage: "plus")
                            .font(.system(size: 12))
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderless)
                    .controlSize(.small)
                    .padding(.vertical, 10)
                }
            }
            .padding(16)
        }
    }

    private func viewRow(_ view: SavedView) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(view.name)
                        .font(.system(size: 13))
                    if view.id == host.viewsState.current {
                        Text("当前")
                            .font(.system(size: 10))
                            .foregroundColor(.green)
                    }
                }
                Text(viewSummary(view))
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                if !host.missingPluginIds(in: view).isEmpty {
                    Text("含未安装扩展：\(host.missingPluginIds(in: view).joined(separator: "、"))")
                        .font(.system(size: 10))
                        .foregroundColor(.orange)
                }
            }
            Spacer(minLength: 6)
            HStack(spacing: 8) {
                Button("应用") { host.switchView(to: view.id) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                Button("编辑") {
                    ViewEditorWindowController.shared.show(view: view, host: host)
                }
                .buttonStyle(.borderless)
                .controlSize(.small)
                Button("删除", role: .destructive) { host.deleteView(view.id) }
                    .buttonStyle(.borderless)
                    .controlSize(.small)
            }
        }
        .padding(.vertical, 10)
    }

    /// 布局摘要：每个区域一个扩展名，未安装的显示原始 id。
    private func viewSummary(_ view: SavedView) -> String {
        view.regions
            .map { host.pluginName(for: $0.plugin) ?? $0.plugin }
            .joined(separator: " + ")
    }

    // MARK: - 更新

    private var updateSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("更新")
            card {
                HStack(spacing: 6) {
                    statusLine
                    Spacer()
                    updateButton
                }
                .padding(.vertical, 10)
            }
        }
    }

    @ViewBuilder private var statusLine: some View {
        switch updater.state {
        case .idle:
            Text("当前版本 v\(updater.currentVersion)")
                .font(.system(size: 12))
        case .checking:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("正在检查更新…").font(.system(size: 12))
            }
        case .upToDate:
            HStack(spacing: 6) {
                Text("当前版本 v\(updater.currentVersion)").font(.system(size: 12))
                Text("已是最新").font(.system(size: 11)).foregroundColor(.green)
            }
        case .available(let version):
            Text("发现新版本 v\(version)（当前 v\(updater.currentVersion)）")
                .font(.system(size: 12))
        case .downloading:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("正在下载新版本…").font(.system(size: 12))
            }
        case .installing:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("正在安装，稍后会自动重启…").font(.system(size: 12))
            }
        case .failed(let msg):
            Text(msg).font(.system(size: 12)).foregroundColor(.orange)
        }
    }

    @ViewBuilder private var updateButton: some View {
        switch updater.state {
        case .available:
            Button("一键更新") { updater.install() }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
        case .checking, .downloading, .installing:
            EmptyView()
        default:
            Button("检查更新") { updater.check() }
                .buttonStyle(.borderless)
                .controlSize(.small)
        }
    }

    // MARK: - 通用 tab

    private var generalTab: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                sectionTitle("通用")
                card {
                    Toggle("关闭窗口后仅保留菜单栏图标", isOn: $state.stayInMenuBar)
                        .padding(.vertical, 10)
                    Divider()
                    Toggle("开机启动", isOn: $state.launchAtLogin)
                        .padding(.vertical, 10)
                    if let err = state.lastLaunchError {
                        Divider()
                        Text("登录项注册失败：\(err)")
                            .font(.system(size: 11))
                            .foregroundColor(.orange)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.vertical, 8)
                    }
                }
            }
            .padding(16)
        }
    }
}

/// alert 的 presenting 目标（@State 不可用，用 ObservableObject 承载）。
final class PendingUninstall: ObservableObject {
    @Published var plugin: PluginInfo?
}
