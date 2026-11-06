// A small Cocoa app for the typing dev bench (not shipped): a search field whose every keystroke filters a
// list and draws for 100 ms after it (as a search box's results settle), and a caret mark beside the field
// that blinks: one frame every 530 ms, drawing unrelated to any input.
//   swiftc -O scripts/dev/type-app.swift -o /tmp/strobe-type-app
import AppKit

/// A bar whose width follows a value, redrawn on the main thread like a toolkit's own drawing.
final class Bar: NSView {
    var value: CGFloat = 0
    override func draw(_ dirty: NSRect) {
        NSColor.controlAccentColor.setFill()
        NSRect(x: 0, y: 0, width: bounds.width * value, height: bounds.height).fill()
    }
}

/// A caret mark, drawn or not.
final class Blink: NSView {
    var on = true
    override func draw(_ dirty: NSRect) {
        if on {
            NSColor.labelColor.setFill()
            bounds.fill()
        }
    }
}

/// A text field that reports a value set from outside (a bench's reset through accessibility) as typing.
final class Field: NSTextField {
    var changed: (() -> Void)?
    override var stringValue: String {
        didSet { changed?() }
    }
}

final class App: NSObject, NSApplicationDelegate, NSTextFieldDelegate, NSTableViewDataSource {
    let window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 640, height: 480), styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
    let field = Field(string: "")
    let echo = NSTextField(labelWithString: "echo: ")
    let bar = Bar()
    let blink = Blink()
    let table = NSTableView()
    let all = (0..<400).map { "item \($0) \(["search", "settings", "session", "share", "theme", "model"][$0 % 6])" }
    var shown: [String] = []
    var ticks = 0
    var drawing: Timer?

    func keyed<T: NSView>(_ v: T, _ id: String) -> T {
        v.setAccessibilityIdentifier(id)
        return v
    }

    func applicationDidFinishLaunching(_ note: Notification) {
        window.title = "strobe typing app"
        shown = all
        field.delegate = self
        field.changed = { [weak self] in self?.update() }
        field.placeholderString = "Search"
        let column = NSTableColumn(identifier: .init("row"))
        column.width = 600
        table.addTableColumn(column)
        table.headerView = nil
        table.dataSource = self
        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        let row = NSStackView(views: [keyed(field, "field"), blink])
        let stack = NSStackView(views: [row, keyed(echo, "echo"), bar, scroll])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.edgeInsets = NSEdgeInsets(top: 8, left: 8, bottom: 8, right: 8)
        field.widthAnchor.constraint(equalToConstant: 300).isActive = true
        blink.widthAnchor.constraint(equalToConstant: 2).isActive = true
        blink.heightAnchor.constraint(equalToConstant: 18).isActive = true
        bar.widthAnchor.constraint(equalToConstant: 300).isActive = true
        bar.heightAnchor.constraint(equalToConstant: 6).isActive = true
        scroll.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -16).isActive = true
        window.contentView = stack
        window.initialFirstResponder = nil
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(nil)
        // The caret mark blinks all the time, as a focused field's caret does (macOS 27's own caret
        // draws nothing strobe sees as the app's frames).
        Timer.scheduledTimer(withTimeInterval: 0.53, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.blink.on.toggle()
            self.blink.needsDisplay = true
        }
        NSApp.activate(ignoringOtherApps: true)
    }

    func controlTextDidChange(_ note: Notification) {
        update()
    }

    func update() {
        let q = field.stringValue.lowercased()
        echo.stringValue = "echo: " + field.stringValue
        shown = q.isEmpty ? all : all.filter { $0.contains(q) }
        table.reloadData()
        // 100 ms of drawing after the keystroke: the bar grows a frame at a time.
        ticks = 0
        drawing?.invalidate()
        drawing = Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] t in
            guard let self else { return t.invalidate() }
            self.ticks += 1
            self.bar.value = CGFloat(self.ticks) / 6
            self.bar.needsDisplay = true
            if self.ticks >= 6 { t.invalidate() }
        }
    }

    func numberOfRows(in tableView: NSTableView) -> Int { shown.count }
    func tableView(_ tableView: NSTableView, objectValueFor column: NSTableColumn?, row: Int) -> Any? { shown[row] }
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}

let app = NSApplication.shared
let delegate = App()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
