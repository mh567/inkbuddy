import Foundation

@main
struct CanvasRatioTests {
    static func main() {
        let suite = "inkbuddy.canvas-ratio-tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }

        let wide = DeviceReport(id: "wide01-device", aspect: 16.0 / 9.0, seen: 1)
        let tall = DeviceReport(id: "tall01-device", aspect: 0.75, seen: 1)
        let updatedWide = DeviceReport(id: wide.id, aspect: 1.6, seen: 2)

        defaults.set(wide.id, forKey: "followDeviceId")
        defaults.set(4.0 / 3.0, forKey: "editorCanvasAspect")
        defaults.set(0.75, forKey: "menuPreviewAspect")

        let host = HostService(defaults: defaults)
        precondition(host.canvasRatioLabel == "wide01 · 4:3（离线）")

        host.updateDevices([wide, tall])
        precondition(host.canvasRatioLabel == "跟随 wide01 · 16:9")
        host.updateDevices([tall])
        precondition(host.canvasAspect == wide.aspect)
        precondition(host.canvasRatioLabel == "wide01 · 16:9（离线）")
        host.updateDevices([updatedWide, tall])
        precondition(host.canvasRatioLabel == "跟随 wide01 · 8:5")
        host.followDevice(wide)
        precondition(host.canvasAspect == updatedWide.aspect)

        host.followDevice(tall)
        precondition(host.canvasAspect == tall.aspect)
        precondition(HostService(defaults: defaults).canvasRatioLabel == "tall01 · 3:4（离线）")

        // 即使预设的数值等于设备比例，选择预设仍会切换到手动模式。
        host.selectManualAspect(0.75)
        host.updateDevices([wide])
        precondition(host.followDeviceId == nil)
        precondition(host.canvasRatioLabel == "手动 3:4")
        precondition(HostService(defaults: defaults).canvasRatioLabel == "手动 3:4")

        defaults.removeObject(forKey: "editorCanvasAspect")
        defaults.set(tall.id, forKey: "followDeviceId")
        precondition(HostService(defaults: defaults).canvasRatioLabel == "tall01 · 3:4（离线）")
        defaults.removePersistentDomain(forName: suite)
        precondition(HostService(defaults: defaults).canvasRatioLabel == "手动 16:9")

        print("CanvasRatioTests: follow, offline, reconnect, manual and legacy settings passed")
    }
}
