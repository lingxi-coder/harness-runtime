import AppKit
import Foundation

// A disposable, local acceptance target. It records events received by this
// application only. Coordinates are global top-left screen points.
let output = URL(fileURLWithPath: CommandLine.arguments[1])
let app = NSApplication.shared
app.setActivationPolicy(.regular)
var state: [String: Any] = ["clicks": 0, "text": "", "scroll": [], "drag": [], "buttons": [], "flags": []]
func save() {
    if let data = try? JSONSerialization.data(withJSONObject: state, options: [.prettyPrinted, .sortedKeys]) {
        try? data.write(to: output.appendingPathComponent("observed.json"), options: .atomic)
    }
}
func append(_ key: String, _ value: Any) {
    var values = state[key] as? [Any] ?? []
    values.append(value)
    state[key] = values
    save()
}
class ProbeView: NSView {
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    override func draw(_ rect: NSRect) {
        NSColor.windowBackgroundColor.setFill(); bounds.fill()
    }
    override func scrollWheel(with event: NSEvent) {
        append("scroll", ["x": event.scrollingDeltaX, "y": event.scrollingDeltaY, "continuous": event.hasPreciseScrollingDeltas,
            "ticks_y": event.cgEvent?.getIntegerValueField(.scrollWheelEventDeltaAxis1) ?? 0,
            "points_y": event.cgEvent?.getIntegerValueField(.scrollWheelEventPointDeltaAxis1) ?? 0,
            "ticks_x": event.cgEvent?.getIntegerValueField(.scrollWheelEventDeltaAxis2) ?? 0,
            "points_x": event.cgEvent?.getIntegerValueField(.scrollWheelEventPointDeltaAxis2) ?? 0])
    }
    override func mouseDown(with event: NSEvent) { append("drag", ["kind": "down", "x": event.locationInWindow.x, "y": event.locationInWindow.y]) }
    override func mouseDragged(with event: NSEvent) { append("drag", ["kind": "move", "x": event.locationInWindow.x, "y": event.locationInWindow.y]) }
    override func mouseUp(with event: NSEvent) { append("drag", ["kind": "up", "x": event.locationInWindow.x, "y": event.locationInWindow.y]) }
    override func otherMouseDown(with event: NSEvent) { append("buttons", ["kind": "down", "button": event.buttonNumber]) }
    override func otherMouseUp(with event: NSEvent) { append("buttons", ["kind": "up", "button": event.buttonNumber]) }
}
class Target: NSObject, NSTextViewDelegate {
    @objc func click(_ sender: Any) { state["clicks"] = (state["clicks"] as? Int ?? 0) + 1; save() }
    func textDidChange(_ notification: Notification) {
        state["text"] = (notification.object as? NSTextView)?.string ?? ""; save()
    }
}
let target = Target()
let window = NSWindow(contentRect: NSRect(x: 150, y: 180, width: 900, height: 620), styleMask: [.titled, .closable], backing: .buffered, defer: false)
window.title = "LingXi Computer Use Acceptance"
window.isReleasedWhenClosed = false
let root = ProbeView(frame: NSRect(x: 0, y: 0, width: 900, height: 620))
window.contentView = root
let title = NSTextField(labelWithString: "Computer Use desktop acceptance — disposable local target")
title.frame = NSRect(x: 40, y: 12, width: 810, height: 30); root.addSubview(title)
let button = NSButton(title: "Click target", target: target, action: #selector(Target.click(_:)))
button.frame = NSRect(x: 40, y: 60, width: 200, height: 50); root.addSubview(button)
let field = NSTextView(frame: NSRect(x: 40, y: 135, width: 810, height: 40))
field.isEditable = true; field.isSelectable = true
field.isRichText = false; field.font = NSFont.systemFont(ofSize: 16)
field.delegate = target; root.addSubview(field)
let instructions = NSTextField(labelWithString: "Scroll on the left. Drag and click mouse side buttons on the right.")
instructions.frame = NSRect(x: 40, y: 190, width: 810, height: 25); root.addSubview(instructions)
let scroll = ProbeView(frame: NSRect(x: 40, y: 235, width: 350, height: 250)); root.addSubview(scroll)
let drag = ProbeView(frame: NSRect(x: 450, y: 235, width: 350, height: 250)); root.addSubview(drag)
NSEvent.addLocalMonitorForEvents(matching: [.flagsChanged]) { event in
    append("flags", ["shift": event.modifierFlags.contains(.shift), "key": event.keyCode]); return event
}
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
    state["text"] = field.string
    state["first_responder"] = window.firstResponder.map { String(describing: type(of: $0)) } ?? "none"
    state["key_window"] = window.isKeyWindow
    state["window_frame"] = NSStringFromRect(window.frame)
    save()
}
func point(_ local: NSPoint) -> [Double] {
    let screen = window.convertPoint(toScreen: root.convert(local, to: nil))
    return [screen.x, (NSScreen.screens.first?.frame.height ?? 0) - screen.y]
}
// Activation can cascade a new AppKit window after its first visible frame.
// Publish coordinates only once our key window has settled in the foreground.
var lastFrame = ""
var stableSince = Date()
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { timer in
    guard window.isKeyWindow && NSWorkspace.shared.frontmostApplication?.processIdentifier == ProcessInfo.processInfo.processIdentifier else {
        stableSince = Date()
        window.makeKeyAndOrderFront(nil)
        app.activate(ignoringOtherApps: true)
        return
    }
    let frame = NSStringFromRect(window.frame)
    if frame != lastFrame { lastFrame = frame; stableSince = Date(); return }
    guard Date().timeIntervalSince(stableSince) >= 0.4 else { return }
    timer.invalidate()
    let path = [NSPoint(x: 500, y: 300), NSPoint(x: 700, y: 450), NSPoint(x: 720, y: 300)]
    let ready: [String: Any] = ["pid": ProcessInfo.processInfo.processIdentifier,
        "bundle_id": Bundle.main.bundleIdentifier ?? "", "scale": window.backingScaleFactor,
        "window_frame": frame,
        "button": point(NSPoint(x: 140, y: 85)), "field": point(NSPoint(x: 200, y: 155)),
        "scroll": point(NSPoint(x: 210, y: 350)),
        "path": path.map(point),
        "window_path": path.map { let p = root.convert($0, to: nil); return [p.x, p.y] }]
    let data = try! JSONSerialization.data(withJSONObject: ready, options: [.prettyPrinted, .sortedKeys])
    try! data.write(to: output.appendingPathComponent("ready.json"), options: .atomic); save()
}
app.run()
