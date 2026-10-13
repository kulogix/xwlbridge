#!/usr/bin/env python3
"""Dumps the accessible tree of every app (or those whose name contains APP) on the bus in $AT_SPI_BUS_ADDRESS
(default: the session's a11y bus) as JSON: per node role, name, description, states, WINDOW extents, actions with
key bindings, value, text, attributes, relations, child count. Usage: treedump.py [APP_SUBSTRING] > dump.json"""
import json, sys, gi
gi.require_version('Atspi', '2.0'); from gi.repository import Atspi
want = sys.argv[1] if len(sys.argv) > 1 else ''
KEEP = ('focusable', 'focused', 'checked', 'indeterminate', 'pressed', 'sensitive', 'enabled', 'showing', 'visible',
        'editable', 'expandable', 'expanded', 'selectable', 'selected', 'horizontal', 'vertical', 'multi-line',
        'single-line', 'active', 'checkable', 'read-only', 'required')
def node(o, depth):
    r = {'depth': depth}
    try:
        r['role'] = o.get_role_name(); r['name'] = o.get_name(); r['desc'] = o.get_description()
    except Exception as ex:
        return {'depth': depth, 'error': repr(ex)}
    try:
        ss = o.get_state_set(); r['states'] = [s for s in KEEP if ss.contains(getattr(Atspi.StateType, s.upper().replace('-', '_')))]
    except Exception: pass
    try:
        e = o.get_extents(Atspi.CoordType.WINDOW); r['box'] = [e.x, e.y, e.width, e.height]
    except Exception: pass
    try:
        a = o.get_action_iface()
        if a: r['actions'] = [[a.get_action_name(i), a.get_key_binding(i)] for i in range(a.get_n_actions())]
    except Exception: pass
    try:
        v = o.get_value_iface()
        if v: r['value'] = [v.get_current_value(), v.get_minimum_value(), v.get_maximum_value()]
    except Exception: pass
    try:
        t = o.get_text_iface()
        if t: r['text'] = Atspi.Text.get_text(o, 0, -1); r['caret'] = Atspi.Text.get_caret_offset(o)
    except Exception: pass
    try:
        at = o.get_attributes()
        if at: r['attrs'] = {k: v for k, v in at.items() if k in ('placeholder-text', 'keyshortcuts', 'level', 'toolkit')}
    except Exception: pass
    try:
        rs = o.get_relation_set()
        if rs: r['rel'] = sorted({x.get_relation_type().value_nick for x in rs})
    except Exception: pass
    return r
out = []
d = Atspi.get_desktop(0)
for i in range(d.get_child_count()):
    app = d.get_child_at_index(i)
    if app is None: continue
    try: name = app.get_name()
    except Exception: continue
    if want and want not in (name or ''): continue
    nodes, stack = [], [(app, 0)]
    while stack and len(nodes) < 400:
        o, k = stack.pop()
        n = node(o, k); nodes.append(n)
        if 'error' in n: continue
        if k > 0 and 'showing' not in n.get('states', []) and n['role'] not in ('frame', 'window', 'dialog'): continue
        try: cc = o.get_child_count()
        except Exception: cc = 0
        for j in reversed(range(min(cc, 200))):
            try: c = o.get_child_at_index(j)
            except Exception: c = None
            if c is not None: stack.append((c, k + 1))
    out.append({'app': name, 'pid': app.get_process_id(), 'nodes': nodes})
json.dump(out, sys.stdout, indent=1)
