import XCTest

final class DeviceMediaTests: XCTestCase {
    func testNativeCapturePlaybackCameraAndAppScreen() {
        let app = XCUIApplication()
        app.launchArguments = ["-AppleLanguages", "(en)", "-AppleLocale", "en_US"]
        let monitor = addUIInterruptionMonitor(withDescription: "Media device permissions") { alert in
            for label in ["Allow", "OK", "Allow While Using App", "允许", "好", "使用App时允许", "Start Recording", "开始录制"] {
                let button = alert.buttons[label]
                if button.exists { button.tap(); return true }
            }
            return false
        }
        defer { removeUIInterruptionMonitor(monitor) }
        app.launch()
        XCTAssertTrue(app.buttons["startMedia"].waitForExistence(timeout: 10))
        app.buttons["startMedia"].tap()
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        func allowPrompts() {
            for _ in 0..<5 {
                if let alert = springboard.alerts.allElementsBoundByIndex.first {
                    for label in ["Allow", "OK", "允许", "好", "Start Recording", "开始录制"] {
                        if alert.buttons[label].exists { alert.buttons[label].tap(); break }
                    }
                }
                Thread.sleep(forTimeInterval: 1)
            }
        }
        allowPrompts()
        let report = app.textViews["mediaReport"]
        let ready = NSPredicate(format: "value CONTAINS 'State: 1'")
        expectation(for: ready, evaluatedWith: report)
        waitForExpectations(timeout: 30)
        Thread.sleep(forTimeInterval: 6)
        let cameraReport = report.value as? String ?? ""
        XCTAssertFalse(cameraReport.contains("Decoded camera: 0 "), cameraReport)
        app.buttons["startScreen"].tap()
        allowPrompts()
        Thread.sleep(forTimeInterval: 8)
        let screenReport = report.value as? String ?? ""
        let screenCount = screenReport.components(separatedBy: "App screen: ").last.flatMap { UInt64($0.trimmingCharacters(in: .whitespacesAndNewlines)) }
        XCTAssertGreaterThan(screenCount ?? 0, 0, screenReport)
        app.buttons["stopMedia"].tap()
    }
}
