#!/usr/bin/env python3
"""Does what a screen reader's commands do, through AT-SPI on the session's accessibility bus: the default action of
an object, or setting a value. Finds the first object with ROLE and NAME in the app whose name contains APP (or in
a window titled TITLE). Usage: at_do.py APP|title=TITLE ROLE NAME action [INDEX] | setvalue V | focus"""
import sys, time, gi
gi.require_version('Atspi', '2.0'); from gi.repository import Atspi
app_sel, role, name, what = sys.argv[1:5]
arg = sys.argv[5] if len(sys.argv) > 5 else None
def find(o, k=0):
    try:
        if o.get_role_name() == role and (o.get_name() or '') == name: return o
        if k > 30: return None
        for i in range(o.get_child_count()):
            r = find(o.get_child_at_index(i), k + 1)
            if r: return r
    except Exception: return None
d = Atspi.get_desktop(0); target = None
for i in range(d.get_child_count()):
    a = d.get_child_at_index(i)
    if a is None: continue
    if app_sel.startswith('title='):
        for j in range(a.get_child_count()):
            w = a.get_child_at_index(j)
            if w and w.get_name() == app_sel[6:]: target = find(w)
    elif app_sel in (a.get_name() or ''):
        target = find(a)
    if target: break
if not target: print('AT_DO not found', app_sel, role, name); sys.exit(1)
if what == 'action':
    n = Atspi.Action.get_n_actions(target)
    names = [Atspi.Action.get_action_name(target, i) for i in range(n)]
    i = int(arg) if arg else 0
    print('AT_DO action', i, names, Atspi.Action.do_action(target, i))
elif what == 'setvalue':
    print('AT_DO setvalue', arg, Atspi.Value.set_current_value(target, float(arg)))
elif what == 'focus':
    print('AT_DO focus', target.get_component_iface().grab_focus())
