#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Part of xwlbridge (contrib/legacy-remote-access): see docs/legacy-remote-access-apps.md.
"""The one thing a window manager does that X11 remote-access servers check for, on a WM-less X display such as the
bridge's :20 (TeamViewer's GuiWindowCheck, for one): ICCCM WM_STATE on top-level
windows. Every mapped, non-override-redirect InputOutput child of the root gets WM_STATE = NormalState, and
WithdrawnState once unmapped -- the same for every client (TeamViewer 15.20's GUI windows carry no WM_CLASS, so a
per-class filter cannot find them). Event-driven (SubstructureNotify on the root) plus a full rescan every 5 s;
exits when the X server goes away (run it as a restartable user unit).

  wm_state.py [DISPLAY]          (default $DISPLAY)
"""
import os
import select
import sys
import time

from Xlib import X, display, error

d = display.Display(sys.argv[1] if len(sys.argv) > 1 else os.environ.get('DISPLAY', ':20'))
root = d.screen().root
WM_STATE = d.intern_atom('WM_STATE')
root.change_attributes(event_mask=X.SubstructureNotifyMask)


def set_state(w, mapped):
    try:
        a = w.get_attributes()
        if a.override_redirect or a.win_class == X.InputOnly:
            return False
        want = 1 if mapped else 0          # NormalState / WithdrawnState
        cur = w.get_full_property(WM_STATE, WM_STATE)
        if cur is None or cur.value[0] != want:
            w.change_property(WM_STATE, WM_STATE, 32, [want, 0])
            return True
    except error.XError:
        pass
    return False


def rescan():
    n = 0
    for w in root.query_tree().children:
        try:
            n += set_state(w, w.get_attributes().map_state != X.IsUnmapped)
        except error.XError:
            pass
    d.flush()
    return n


print(time.strftime('%H:%M:%S'), 'wm_state on', d.get_display_name(), '- initial:', rescan(), 'window(s)', flush=True)
last = time.monotonic()
fd = d.fileno()
while True:
    r, _, _ = select.select([fd], [], [], 5.0)
    while d.pending_events():
        e = d.next_event()
        if e.type == X.MapNotify and not e.override:
            if set_state(e.window, True):
                print(time.strftime('%H:%M:%S'), 'NormalState', hex(e.window.id), flush=True)
        elif e.type == X.UnmapNotify:
            set_state(e.window, False)
    d.flush()
    if time.monotonic() - last > 5:
        rescan()
        last = time.monotonic()
