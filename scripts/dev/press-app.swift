// A small Cocoa app for the press-edge dev bench (not shipped): a sidebar whose items switch the pane
// on the mouse press (as tabs, menus and many native sidebars do), items that switch it on the
// release, as a button does, and items that take keyboard focus on the press and switch it on the
// release. The pane is a coloured view and a label naming the item.
//   swiftc -O scripts/dev/press-app.swift -o /tmp/strobe-press-app
import AppKit

/// A sidebar item: an accessible button that switches the pane on its press or on its release.
final class Item: NSView {
    let title: String
    let onPress: Bool
    let act: (String) -> Void
    var down = false

    init(_ title: String, onPress: Bool, act: @escaping (String) -> Void) {
        self.title = title
        self.onPress = onPress
        self.act = act
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel(title)
        setAccessibilityIdentifier(title)
        widthAnchor.constraint(equalToConstant: 140).isActive = true
        heightAnchor.constraint(equalToConstant: 28).isActive = true
    }

    required init?(coder: NSCoder) { nil }

    override func draw(_ dirty: NSRect) {
        (down ? NSColor.selectedControlColor : NSColor.controlBackgroundColor).setFill()
        bounds.fill()
        (title as NSString).draw(at: NSPoint(x: 8, y: 6), withAttributes: [.font: NSFont.systemFont(ofSize: 13)])
    }

    override func accessibilityPerformPress() -> Bool {
        act(title)
        return true
    }

    /// The focus items take keyboard focus on the press and act on the release, as a push button does
    /// on most toolkits: their focus change on the press is no reaction.
    var focusable: Bool { title.hasPrefix("focus") }
    override var acceptsFirstResponder: Bool { focusable }

    override func mouseDown(with event: NSEvent) {
        down = true
        needsDisplay = true
        if focusable, window?.makeFirstResponder(self) == true {
            NSAccessibility.post(element: self, notification: .focusedUIElementChanged)
        }
        if onPress { act(title) }
    }

    override func mouseUp(with event: NSEvent) {
        down = false
        needsDisplay = true
        if !onPress && bounds.contains(convert(event.locationInWindow, from: nil)) { act(title) }
    }
}

final class App: NSObject, NSApplicationDelegate {
    let window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 640, height: 480), styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
    let pane = NSView()
    let label = NSTextField(labelWithString: "pane: none")
    let colours: [NSColor] = [.systemRed, .systemGreen, .systemBlue, .systemOrange, .systemPurple, .systemTeal]

    func show(_ name: String) {
        label.stringValue = "pane: \(name)"
        NSAccessibility.post(element: label, notification: .valueChanged)
        pane.layer?.backgroundColor = colours[abs(name.hashValue) % colours.count].cgColor
    }

    func applicationDidFinishLaunching(_ note: Notification) {
        window.title = "strobe press app"
        let items = ["press-a", "press-b", "release-a", "release-b", "focus-a", "focus-b"].map { name in
            Item(name, onPress: name.hasPrefix("press"), act: { [weak self] in self?.show($0) })
        }
        let sidebar = NSStackView(views: items)
        sidebar.orientation = .vertical
        sidebar.alignment = .leading
        label.setAccessibilityIdentifier("pane")
        pane.wantsLayer = true
        pane.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        let right = NSStackView(views: [label, pane])
        right.orientation = .vertical
        right.alignment = .leading
        pane.widthAnchor.constraint(equalToConstant: 440).isActive = true
        pane.heightAnchor.constraint(equalToConstant: 380).isActive = true
        let root = NSStackView(views: [sidebar, right])
        root.alignment = .top
        root.edgeInsets = NSEdgeInsets(top: 8, left: 8, bottom: 8, right: 8)
        window.contentView = root
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

let app = NSApplication.shared
let delegate = App()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
