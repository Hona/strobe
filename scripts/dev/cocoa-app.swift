// A small Cocoa app for the macOS accessibility driver's dev bench (not shipped; TextEdit is
// sandboxed, so strobe will not launch it): a button that adds rows to a scrolled list and counts
// them, a text field mirrored by a label, a check box. Author ids go to AXIdentifier.
//   swiftc -O scripts/dev/cocoa-app.swift -o /tmp/strobe-cocoa-app
import AppKit

/// A text field that selects all on ⌘A itself, without an Edit menu's key equivalent.
final class Field: NSTextField {
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if event.modifierFlags.contains(.command), event.charactersIgnoringModifiers == "a", let editor = currentEditor() {
            editor.selectAll(nil)
            return true
        }
        return super.performKeyEquivalent(with: event)
    }
}

final class App: NSObject, NSApplicationDelegate, NSTextFieldDelegate, NSTableViewDataSource {
    let window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 640, height: 480), styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
    let count = NSTextField(labelWithString: "rows: 0")
    let field = Field(string: "")
    let echo = NSTextField(labelWithString: "echo: ")
    let table = NSTableView()
    var rows = (0..<60).map { "seed \($0)" }
    var added = 0

    func keyed<T: NSView>(_ v: T, _ id: String) -> T {
        v.setAccessibilityIdentifier(id)
        return v
    }

    func applicationDidFinishLaunching(_ note: Notification) {
        window.title = "strobe dev app"
        let add = keyed(NSButton(title: "Add", target: self, action: #selector(onAdd)), "add")
        let check = keyed(NSButton(checkboxWithTitle: "Enabled", target: nil, action: nil), "check")
        field.delegate = self
        let column = NSTableColumn(identifier: .init("row"))
        column.width = 600
        table.addTableColumn(column)
        table.headerView = nil
        table.dataSource = self
        let scroll = keyed(NSScrollView(), "scroll")
        scroll.documentView = keyed(table, "rows")
        scroll.hasVerticalScroller = true
        let stack = NSStackView(views: [add, keyed(count, "count"), keyed(field, "field"), keyed(echo, "echo"), check, scroll])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.edgeInsets = NSEdgeInsets(top: 8, left: 8, bottom: 8, right: 8)
        field.widthAnchor.constraint(equalToConstant: 300).isActive = true
        scroll.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -16).isActive = true
        window.contentView = stack
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    /// What happened, on stderr (a launch's output, and a check without the Accessibility permission).
    func log(_ s: String) {
        FileHandle.standardError.write((s + "\n").data(using: .utf8)!)
    }

    @objc func onAdd() {
        added += 1
        log("add \(added) active=\(NSApp.isActive)")
        rows.insert("row \(added)", at: 0)
        table.reloadData()
        count.stringValue = "rows: \(added)"
    }

    func controlTextDidChange(_ note: Notification) {
        echo.stringValue = "echo: " + field.stringValue
        log("text \(field.stringValue) active=\(NSApp.isActive)")
    }

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }
    func tableView(_ tableView: NSTableView, objectValueFor column: NSTableColumn?, row: Int) -> Any? { rows[row] }
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}

let app = NSApplication.shared
let delegate = App()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
