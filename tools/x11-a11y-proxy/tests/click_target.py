#!/usr/bin/env python3
"""Where on GNOME's screen (global logical = the virtual monitor's pixels) a control is, for a pointer click:
the window's rectangle from the window-geometry extension plus the control's WINDOW extents from AT-SPI.
Usage: click_target.py TITLE ROLE NAME   (window title; the AT-SPI tree searched is the session bus's)"""
import json, sys, subprocess, ast, gi
gi.require_version('Atspi', '2.0'); from gi.repository import Atspi
title, role, name = sys.argv[1:4]
snap = subprocess.run(['gdbus', 'call', '--session', '-d', 'org.gnome.Shell.Extensions.WindowGeometry', '-o',
                       '/org/gnome/Shell/Extensions/WindowGeometry', '-m',
                       'org.gnome.Shell.Extensions.WindowGeometry.Snapshot'], capture_output=True, text=True).stdout
d = json.loads(ast.literal_eval(snap)[0])
wins = [w for w in d['windows'] if w.get('title') == title]
if not wins: print('NONE no window', title); sys.exit(1)
w = wins[-1]
# X11 windows report WINDOW extents relative to the client area; GTK 4 (Wayland) relative to frame_rect
base = w['client_rect'] if w.get('client_type') == 'x11' else w['frame_rect']
def find(o, k=0):
    try:
        if o.get_role_name() == role and (o.get_name() or '') == name: return o
        if k > 30: return None
        for i in range(o.get_child_count()):
            r = find(o.get_child_at_index(i), k + 1)
            if r: return r
    except Exception: return None
desk = Atspi.get_desktop(0)
for i in range(desk.get_child_count()):
    a = desk.get_child_at_index(i)
    for j in range(a.get_child_count() if a else 0):
        win = a.get_child_at_index(j)
        if win and win.get_name() == title:
            o = find(win)
            if o:
                e = o.get_extents(Atspi.CoordType.WINDOW)
                print(int(base[0] + e.x + e.width / 2), int(base[1] + e.y + e.height / 2), w.get('client_type'), base, [e.x, e.y, e.width, e.height])
                sys.exit(0)
print('NONE no control', role, name); sys.exit(1)
