#!/usr/bin/env python3
"""A native GTK 4 window on GNOME (Wayland) that just takes focus: used to press keys while the proxy is not the
active window. Prints the text typed into its entry."""
import gi
gi.require_version('Gtk', '4.0'); from gi.repository import Gtk, Gio
app = Gtk.Application(application_id=None, flags=Gio.ApplicationFlags.NON_UNIQUE)
def act(a):
    w = Gtk.ApplicationWindow(application=a, title='witness'); e = Gtk.Entry(); w.set_child(e)
    e.connect('changed', lambda x: print('WITNESS', x.get_text(), flush=True)); w.present(); print('READY', flush=True)
app.connect('activate', act); app.run([])
