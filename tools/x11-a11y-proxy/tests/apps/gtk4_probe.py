#!/usr/bin/env python3
"""GTK4 probe (x11 backend on the engine): the same controls and outputs as gtk3_probe.py."""
import sys, gi
gi.require_version('Gtk', '4.0'); from gi.repository import Gtk, Gdk, GLib, Gio
tag = sys.argv[1] if len(sys.argv) > 1 else 'x'
app = Gtk.Application(application_id=None, flags=Gio.ApplicationFlags.NON_UNIQUE)
def act(a):
    w = Gtk.ApplicationWindow(application=a, title=f'GTK4 probe {tag}'); w.set_default_size(320, 220)
    b = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6); w.set_child(b)
    b.append(Gtk.Label(label='Probe label'))
    s = Gtk.Button.new_with_mnemonic('_Seven'); s.connect('clicked', lambda *_: print('ACTIVATE Seven', flush=True)); b.append(s)
    s.set_tooltip_text('Adds the digit seven')
    c = Gtk.CheckButton.new_with_mnemonic('_Check me'); c.connect('toggled', lambda x: print('TOGGLE', x.get_active(), flush=True)); b.append(c)
    e = Gtk.Entry(); e.set_placeholder_text('type here'); e.connect('changed', lambda x: print('TEXT', x.get_text(), flush=True)); b.append(e)
    sc = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1); sc.set_value(40); b.append(sc)
    sc.connect('value-changed', lambda x: print('VALUE', int(x.get_value()), flush=True))
    sh = Gtk.ShortcutController(); sh.set_scope(Gtk.ShortcutScope.GLOBAL)
    sh.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string('<Control>k'),
                                     Gtk.CallbackAction.new(lambda *x: print('SHORTCUT ctrl+k', flush=True) or True)))
    w.add_controller(sh)
    w.present()
    def dump():
        st = Gtk.Settings.get_default()
        keys = ['gtk-theme-name', 'gtk-icon-theme-name', 'gtk-font-name', 'gtk-xft-dpi', 'gtk-cursor-theme-name',
                'gtk-cursor-theme-size', 'gtk-enable-animations', 'gtk-interface-color-scheme', 'gtk-interface-contrast',
                'gtk-interface-reduced-motion']
        vals = {}
        for k in keys:
            try: vals[k] = st.get_property(k)
            except Exception: pass
        print('SETTINGS gtk4', {k: (v.value_nick if hasattr(v, 'value_nick') else v) for k, v in vals.items()},
              'scale', w.get_scale_factor(), flush=True)
        return True
    GLib.timeout_add(1500, lambda: dump() and False); GLib.timeout_add(4000, dump)
    print('READY', flush=True)
app.connect('activate', act); app.run([])
