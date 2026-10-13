#!/usr/bin/env python3
"""Logs AT-SPI events (focus, state, value, text, caret, name, announcement, window) of the apps on the bus in
$AT_SPI_BUS_ADDRESS (or the session's a11y bus) as JSON lines: time, type, app, role, name, detail1, any_data.
Usage: evlog.py LABEL OUTFILE [APP_SUBSTRING]"""
import json, sys, time, gi
gi.require_version('Atspi', '2.0'); from gi.repository import Atspi, GLib
label, path, want = sys.argv[1], sys.argv[2], (sys.argv[3] if len(sys.argv) > 3 else '')
f = open(path, 'a', buffering=1)
def on(ev):
    try:
        o = ev.source; app = o.get_application(); an = app.get_name() if app else ''
        if want and want not in (an or ''): return
        rec = {'t': round(time.time(), 3), 'bus': label, 'type': ev.type, 'app': an, 'role': o.get_role_name(),
               'name': o.get_name(), 'd1': ev.detail1}
        if isinstance(ev.any_data, (str, int, float)): rec['data'] = ev.any_data
        if ev.type.startswith('object:text-changed'):
            try: rec['text'] = Atspi.Text.get_text(o, 0, -1)
            except Exception: pass
        if 'value' in ev.type:
            v = o.get_value_iface(); rec['value'] = v.get_current_value() if v else None
        f.write(json.dumps(rec) + '\n')
    except Exception as ex:
        f.write(json.dumps({'t': time.time(), 'error': repr(ex), 'type': ev.type}) + '\n')
l = Atspi.EventListener.new(on)
for et in ('object:state-changed:focused', 'object:state-changed:checked', 'object:value-changed',
           'object:property-change:accessible-value', 'object:text-changed', 'object:text-caret-moved',
           'object:property-change:accessible-name', 'object:announcement', 'window:activate', 'window:deactivate'):
    l.register(et)
GLib.MainLoop().run()
