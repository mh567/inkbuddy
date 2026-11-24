import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var state: AppState
    @EnvironmentObject var host: HostService
    @EnvironmentObject var updater: Updater

    private let ink = Color(red: 0.145, green: 0.129, blue: 0.361)

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                header
                serviceSection
                extensionsSection
                updateSection
                generalSection
            }
            .padding(20)
            .frame(width: 440)
        }
        .frame(width: 440, height: 560)
        .onAppear {
            host.fetchPlugins()
            updater.check()
        }
    }

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

    private var serviceSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("服务")
            VStack(spacing: 10) {
                HStack {
                    Text("启用副屏扩展")
                    Spacer()
                    Toggle("", isOn: Binding(
                        get: { host.running },
                        set: { on in on ? host.start() : host.stop() }
                    ))
                    .labelsHidden()
                    .toggleStyle(.switch)
                }
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
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color(NSColor.controlBackgroundColor)))
        }
    }

    private var extensionsSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("扩展")
            VStack(spacing: 0) {
                if host.plugins.isEmpty {
                    Text(host.running ? "未发现扩展" : "启动服务后显示扩展列表")
                        .font(.system(size: 12))
                        .foregroundColor(.secondary)
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding(.vertical, 16)
                } else {
                    ForEach(host.plugins) { plugin in
                        HStack(alignment: .firstTextBaseline, spacing: 10) {
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 6) {
                                    Text(plugin.name)
                                        .font(.system(size: 13))
                                    if !plugin.running {
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
                            Toggle("", isOn: Binding(
                                get: { plugin.enabled },
                                set: { host.setPlugin(plugin.id, enabled: $0) }
                            ))
                            .labelsHidden()
                            .toggleStyle(.switch)
                        }
                        .padding(.vertical, 10)
                        if plugin.id != host.plugins.last?.id {
                            Divider()
                        }
                    }
                }
            }
            .padding(.horizontal, 12)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color(NSColor.controlBackgroundColor)))
        }
    }

    private var updateSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("更新")
            VStack(spacing: 10) {
                HStack(spacing: 6) {
                    statusLine
                    Spacer()
                    updateButton
                }
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color(NSColor.controlBackgroundColor)))
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

    private var generalSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionTitle("通用")
            VStack(spacing: 0) {
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
            .padding(.horizontal, 12)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color(NSColor.controlBackgroundColor)))
        }
    }

    private func sectionTitle(_ text: String) -> some View {
        Text(text)
            .font(.system(size: 12, weight: .medium))
            .foregroundColor(.secondary)
    }
}
