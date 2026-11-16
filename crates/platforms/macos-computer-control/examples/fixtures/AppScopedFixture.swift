import AppKit

// Isolated acceptance target. It neither activates itself nor touches any
// application/business data. Launch with `open -gn -a <bundle> --args <receipt>`.
final class AppScopedFixture: NSObject, NSApplicationDelegate {
    private let receipt: URL
    private var window: NSWindow!
    private let field = NSTextField(string: "initial fixture text")
    private let counter = NSTextField(labelWithString: "Applied 0")
    private var count = 0
    private var lastState: Data?
    private var timer: Timer?

    init(receipt: URL) { self.receipt = receipt }
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        window = NSWindow(contentRect: NSRect(x: 120, y: 180, width: 520, height: 280),
                          styleMask: [.titled, .closable, .miniaturizable, .resizable],
                          backing: .buffered, defer: false)
        window.title = "LingXi app-scoped acceptance fixture"
        window.isReleasedWhenClosed = false
        let content = window.contentView!
        let title = NSTextField(labelWithString: "Isolated background AX fixture")
        title.frame = NSRect(x: 24, y: 226, width: 460, height: 24)
        field.frame = NSRect(x: 24, y: 174, width: 460, height: 30)
        field.setAccessibilityTitle("Fixture input")
        field.setAccessibilityIdentifier("fixture-input")
        let button = NSButton(title: "Apply", target: self, action: #selector(apply))
        button.frame = NSRect(x: 24, y: 114, width: 100, height: 32)
        button.setAccessibilityIdentifier("fixture-apply")
        counter.frame = NSRect(x: 148, y: 120, width: 320, height: 24)
        [title, field, button, counter].forEach(content.addSubview)
        let menu = NSMenu()
        let appItem = NSMenuItem(title: "Fixture", action: nil, keyEquivalent: "")
        let appMenu = NSMenu(title: "Fixture")
        let setItem = NSMenuItem(title: "Set via Menu", action: #selector(setViaMenu), keyEquivalent: "")
        setItem.target = self
        let commandItem = NSMenuItem(title: "Fixture", action: nil, keyEquivalent: "")
        let commandMenu = NSMenu(title: "Fixture")
        commandMenu.addItem(setItem)
        commandItem.submenu = commandMenu
        let quit = NSMenuItem(title: "Quit Fixture", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "")
        appMenu.addItem(quit)
        appItem.submenu = appMenu
        menu.addItem(appItem)
        menu.addItem(commandItem)
        NSApp.mainMenu = menu
        window.orderFront(nil)
        persist()
        timer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in self?.persist() }
    }
    @objc private func apply() { count += 1; counter.stringValue = "Applied \(count)"; persist() }
    @objc private func setViaMenu() { field.stringValue = "menu fixture text"; persist() }
    private func persist() {
        let object: [String: Any] = ["fixture": "lingxi-app-scoped", "text": field.stringValue,
                                     "applied": count, "pid": ProcessInfo.processInfo.processIdentifier]
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              data != lastState else { return }
        do { try data.write(to: receipt, options: .atomic); lastState = data }
        catch { fputs("Could not write fixture receipt\n", stderr) }
    }
}
let destination = CommandLine.arguments.dropFirst().first.map(URL.init(fileURLWithPath:))
    ?? FileManager.default.temporaryDirectory.appendingPathComponent("lingxi-app-scoped-fixture.json")
let delegate = AppScopedFixture(receipt: destination)
NSApplication.shared.delegate = delegate
NSApplication.shared.run()
