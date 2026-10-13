#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Part of xwlbridge (contrib/legacy-remote-access): see docs/legacy-remote-access-apps.md.
"""ICCCM WM_STATE for the top-level windows of a WM-less X display such as the bridge's :20 -- the one thing a window
manager does that X11 remote-access servers check (TeamViewer's GuiWindowCheck, for one).
Every mapped, non-override-redirect InputOutput child of the root gets WM_STATE = NormalState, and WithdrawnState
once unmapped -- for every client (TeamViewer 15.20's GUI windows carry no WM_CLASS, so a per-class filter fails).

This variant talks to libX11 through ctypes, so it needs nothing but
python3 and libX11 (RHEL 10 / EPEL 10 ship no python3-xlib). It rescans the root's children every POLL seconds
(default 0.25; a handful of XGetWindowAttributes per scan) and exits when the X server goes away, so run it as a
restartable unit. Same behaviour as wm_state.py (python-xlib, event-driven).

  wm_state_ctypes.py [DISPLAY] [POLL]          (default $DISPLAY or :20, 0.25 s)
"""
import ctypes
import ctypes.util
import os
import sys
import time

DISP = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get('DISPLAY', ':20')).encode()
POLL = float(sys.argv[2]) if len(sys.argv) > 2 else 0.25
x = ctypes.CDLL(ctypes.util.find_library('X11') or 'libX11.so.6')
Window = ctypes.c_ulong
Atom = ctypes.c_ulong


class XWindowAttributes(ctypes.Structure):
    _fields_ = [('x', ctypes.c_int), ('y', ctypes.c_int), ('width', ctypes.c_int), ('height', ctypes.c_int),
                ('border_width', ctypes.c_int), ('depth', ctypes.c_int), ('visual', ctypes.c_void_p),
                ('root', Window), ('class_', ctypes.c_int), ('bit_gravity', ctypes.c_int),
                ('win_gravity', ctypes.c_int), ('backing_store', ctypes.c_int), ('backing_planes', ctypes.c_ulong),
                ('backing_pixel', ctypes.c_ulong), ('save_under', ctypes.c_int), ('colormap', ctypes.c_ulong),
                ('map_installed', ctypes.c_int), ('map_state', ctypes.c_int), ('all_event_masks', ctypes.c_long),
                ('your_event_mask', ctypes.c_long), ('do_not_propagate_mask', ctypes.c_long),
                ('override_redirect', ctypes.c_int), ('screen', ctypes.c_void_p)]


ERRORS = [0]
XERRH = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
XIOERRH = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p)


def _err(dpy, ev):          # a window vanished between QueryTree and the next request: ignore (BadWindow)
    ERRORS[0] += 1
    return 0


def _ioerr(dpy):            # the X server went away: exit, the unit restarts us with the next server
    print(time.strftime('%H:%M:%S'), 'X server gone; exiting', flush=True)
    os._exit(1)


_eh, _ioh = XERRH(_err), XIOERRH(_ioerr)
x.XSetErrorHandler(_eh)
x.XSetIOErrorHandler(_ioh)
x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XDefaultRootWindow.restype = Window
x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x.XInternAtom.restype = Atom
x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
x.XQueryTree.argtypes = [ctypes.c_void_p, Window, ctypes.POINTER(Window), ctypes.POINTER(Window),
                         ctypes.POINTER(ctypes.POINTER(Window)), ctypes.POINTER(ctypes.c_uint)]
x.XGetWindowAttributes.argtypes = [ctypes.c_void_p, Window, ctypes.POINTER(XWindowAttributes)]
x.XGetWindowProperty.argtypes = [ctypes.c_void_p, Window, Atom, ctypes.c_long, ctypes.c_long, ctypes.c_int, Atom,
                                 ctypes.POINTER(Atom), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong),
                                 ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_void_p)]
x.XChangeProperty.argtypes = [ctypes.c_void_p, Window, Atom, Atom, ctypes.c_int, ctypes.c_int, ctypes.c_void_p,
                              ctypes.c_int]
x.XFree.argtypes = [ctypes.c_void_p]
x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]

dpy = x.XOpenDisplay(DISP)
if not dpy:
    print('cannot open display', DISP.decode(), file=sys.stderr, flush=True)
    sys.exit(1)
root = x.XDefaultRootWindow(dpy)
WM_STATE = x.XInternAtom(dpy, b'WM_STATE', 0)
INPUT_ONLY, IS_UNMAPPED = 2, 0


def get_state(w):
    t, f, n, after, data = Atom(), ctypes.c_int(), ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_void_p()
    if x.XGetWindowProperty(dpy, w, WM_STATE, 0, 2, 0, WM_STATE, ctypes.byref(t), ctypes.byref(f), ctypes.byref(n),
                            ctypes.byref(after), ctypes.byref(data)) != 0:
        return None
    v = None
    if data.value and n.value >= 1 and f.value == 32:
        v = ctypes.cast(data, ctypes.POINTER(ctypes.c_long))[0]     # format-32 data are longs in Xlib
    if data.value:
        x.XFree(data)
    return v


def set_state(w, want):
    buf = (ctypes.c_long * 2)(want, 0)                              # state, icon window (None)
    x.XChangeProperty(dpy, w, WM_STATE, WM_STATE, 32, 0, buf, 2)    # PropModeReplace


def scan():
    r, p, kids, n = Window(), Window(), ctypes.POINTER(Window)(), ctypes.c_uint()
    if not x.XQueryTree(dpy, root, ctypes.byref(r), ctypes.byref(p), ctypes.byref(kids), ctypes.byref(n)):
        return 0
    ws = [kids[i] for i in range(n.value)]
    if kids:
        x.XFree(kids)
    changed = 0
    for w in ws:
        a = XWindowAttributes()
        if not x.XGetWindowAttributes(dpy, w, ctypes.byref(a)):
            continue
        if a.override_redirect or a.class_ == INPUT_ONLY:
            continue
        want = 0 if a.map_state == IS_UNMAPPED else 1                # WithdrawnState / NormalState
        if get_state(w) != want:
            set_state(w, want)
            changed += 1
            if want:
                print(time.strftime('%H:%M:%S'), 'NormalState', hex(w), f'{a.width}x{a.height}', flush=True)
    x.XSync(dpy, 0)
    return changed


print(time.strftime('%H:%M:%S'), 'wm_state (ctypes) on', DISP.decode(), '- initial:', scan(), 'window(s)', flush=True)
while True:
    time.sleep(POLL)
    scan()
