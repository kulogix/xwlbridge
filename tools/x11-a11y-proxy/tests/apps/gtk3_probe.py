#!/usr/bin/env python3
"""GTK3 probe: label, button "_Seven" (tooltip), check "_Check me", entry (placeholder), scale 0-100 at 40, and a
Ctrl+K accelerator. Prints ACTIVATE / TOGGLE / TEXT / VALUE / SHORTCUT lines and the XSETTINGS-derived settings."""
import sys, gi
gi.require_version('Gtk', '3.0'); from gi.repository import Gtk, Gdk, GLib
tag = sys.argv[1] if len(sys.argv) > 1 else 'x'
w = Gtk.Window(title=f'GTK3 probe {tag}'); w.set_default_size(320, 220)
b = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6); w.add(b)
b.add(Gtk.Label(label='Probe label'))
s = Gtk.Button.new_with_mnemonic('_Seven'); s.connect('clicked', lambda *_: print('ACTIVATE Seven', flush=True)); b.add(s)
s.set_tooltip_text('Adds the digit seven')
c = Gtk.CheckButton.new_with_mnemonic('_Check me'); c.connect('toggled', lambda x: print('TOGGLE', x.get_active(), flush=True)); b.add(c)
e = Gtk.Entry(); e.set_placeholder_text('type here'); e.connect('changed', lambda x: print('TEXT', x.get_text(), flush=True)); b.add(e)
sc = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1); sc.set_value(40); b.add(sc)
sc.connect('value-changed', lambda x: print('VALUE', int(x.get_value()), flush=True))
ag = Gtk.AccelGroup(); w.add_accel_group(ag)
ag.connect(Gdk.keyval_from_name('k'), Gdk.ModifierType.CONTROL_MASK, 0, lambda *a: print('SHORTCUT ctrl+k', flush=True) or True)
w.connect('destroy', Gtk.main_quit); w.show_all()
def dump():
    st = Gtk.Settings.get_default()
    keys = ['gtk-theme-name', 'gtk-icon-theme-name', 'gtk-font-name', 'gtk-xft-dpi', 'gtk-cursor-theme-name',
            'gtk-cursor-theme-size', 'gtk-enable-animations', 'gtk-application-prefer-dark-theme']
    print('SETTINGS gtk3', {k: st.get_property(k) for k in keys}, 'scale', w.get_scale_factor(), flush=True)
    return True
GLib.timeout_add(1500, lambda: dump() and False); GLib.timeout_add(4000, dump)
print('READY', flush=True); Gtk.main()
