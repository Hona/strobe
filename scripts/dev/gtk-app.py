#!/usr/bin/env python3
# A small GTK 3 app for the Linux accessibility driver's dev bench (not shipped): a button that adds
# rows to a scrolled list and counts them, a text field mirrored by a label, a check box. Author ids
# go to AT-SPI's AccessibleId (ATK's accessible id).
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk


def keyed(widget, key):
    widget.get_accessible().set_accessible_id(key)
    return widget


win = Gtk.Window(title="strobe dev app")
win.set_default_size(640, 480)
win.connect("destroy", Gtk.main_quit)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
win.add(box)

count = keyed(Gtk.Label(label="rows: 0"), "count")
rows = keyed(Gtk.ListBox(), "rows")
add = keyed(Gtk.Button(label="Add"), "add")


added = [0]


def on_add(_):
    added[0] += 1
    n = added[0]
    rows.insert(keyed(Gtk.Label(label=f"row {n}"), f"row-{n}"), 0)
    rows.show_all()
    count.set_text(f"rows: {n}")


add.connect("clicked", on_add)
field = keyed(Gtk.Entry(), "field")
echo = keyed(Gtk.Label(label="echo: "), "echo")
field.connect("changed", lambda e: echo.set_text("echo: " + e.get_text()))
check = keyed(Gtk.CheckButton(label="Enabled"), "check")
scroll = keyed(Gtk.ScrolledWindow(), "scroll")
scroll.set_vexpand(True)
scroll.add(rows)
for i in range(60):
    rows.add(keyed(Gtk.Label(label=f"seed {i}"), f"seed-{i}"))
for w in (add, count, field, echo, check, scroll):
    box.pack_start(w, w is scroll, w is scroll, 0)
win.show_all()
Gtk.main()
