# SPDX-License-Identifier: MIT
"""A separate GTK process offering real external file-list drags to Strata."""

import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, Gtk


class FileDragSource(Gtk.Application):
    def do_activate(self):
        window = Gtk.ApplicationWindow(application=self, title="Foreign file selection")
        window.set_default_size(240, 80)
        button = Gtk.Button(label="Foreign file selection")
        content = Gdk.ContentProvider.new_for_value(
            Gdk.FileList.new_from_array([Gio.File.new_for_path(path) for path in sys.argv[1:]])
        )
        drag = Gtk.DragSource(actions=Gdk.DragAction.COPY)
        drag.connect("prepare", lambda *_: content)
        drag.connect("drag-end", lambda *_: button.set_label("Foreign drag finished"))
        button.add_controller(drag)
        window.set_child(button)
        window.present()


FileDragSource(application_id="io.github.lgse.Strata.TestDragSource").run([])
