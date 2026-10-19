#!/usr/bin/env python3
# X side of run_bridge_sync.sh: an X client of the bridge's engine display (DISPLAY) that moves, resizes,
# raises and flags windows the way their own clients would. Needs python3-xlib.
# SPDX-License-Identifier: MIT
import sys, time
from Xlib import display, X, Xatom, protocol
d = display.Display()
root = d.screen().root

def win(xid):
    return d.create_resource_object('window', int(xid, 0))

def geom(w):
    g = w.get_geometry()
    t = w.translate_coords(root, 0, 0)
    return (-t.x - g.border_width, -t.y - g.border_width, g.width + 2 * g.border_width, g.height + 2 * g.border_width)

def state_msg(w, action, atom):
    a = d.intern_atom(atom)
    ev = protocol.event.ClientMessage(window=w, client_type=d.intern_atom('_NET_WM_STATE'),
                                      data=(32, [action, a, 0, 1, 0]))
    root.send_event(ev, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
    d.flush()

cmd = sys.argv[1]
if cmd == 'desk':  # desk FILE TITLE KEY...: the probe's report of the desktop window titled TITLE
    import json
    rows = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
    row = next((r for r in rows if r['title'] == sys.argv[3]), {})
    print(','.join(str(row.get(k)) for k in sys.argv[4:]))
    sys.exit(0)
if cmd == 'order':  # order FILE TITLE...: those titles bottom to top as the desktop stacks them
    import json
    rows = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
    print(' '.join(r['title'] for r in rows if r['title'] in sys.argv[3:]))
    sys.exit(0)
xid = sys.argv[2]
w = win(xid)
if cmd == 'geom':
    print('%d,%d,%dx%d' % geom(w))
elif cmd == 'animate':  # x0 y0 dx steps ms: the client moves its own window, step by step
    x0, y0, dx, n, ms = (int(v) for v in sys.argv[3:8])
    for i in range(n + 1):
        w.configure(x=x0 + i * dx, y=y0)
        d.flush()
        time.sleep(ms / 1000)
    d.sync()
elif cmd == 'resize':  # w h
    w.configure(width=int(sys.argv[3]), height=int(sys.argv[4])); d.sync()
elif cmd == 'above':
    state_msg(w, 1, '_NET_WM_STATE_ABOVE'); d.sync()
elif cmd == 'urgent':
    h = w.get_wm_hints() or {}
    flags = (h.get('flags', 0) if isinstance(h, dict) else h.flags) | (1 << 8)
    w.set_wm_hints(flags=flags, input=1); d.sync()
elif cmd == 'raise':
    w.configure(stack_mode=X.Above); d.sync()
elif cmd == 'activate':
    ev = protocol.event.ClientMessage(window=w, client_type=d.intern_atom('_NET_ACTIVE_WINDOW'),
                                      data=(32, [1, 0, 0, 0, 0]))
    root.send_event(ev, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask); d.sync()
elif cmd == 'stack':  # the root's children (bottom to top) that are these xids
    ids = [int(v, 0) for v in sys.argv[2:]]
    print(' '.join('%#x' % c.id for c in root.query_tree().children if c.id in ids))
elif cmd == 'netstate':
    p = w.get_full_property(d.intern_atom('_NET_WM_STATE'), Xatom.ATOM)
    print(' '.join(d.get_atom_name(a) for a in (p.value if p else [])))
