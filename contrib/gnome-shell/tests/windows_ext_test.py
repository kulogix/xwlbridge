#!/usr/bin/env python3
# End-to-end test of the xwlbridge-windows@xwlbridge extension against a real (headless) gnome-shell.
#
# Run it through run_windows_ext.sh (a scratch session: never the desktop you are logged in to). This file is
# both the test and its helper processes: a minimal Wayland client written against the wire protocol
# (wl_shm buffers, xdg-shell, xdg_toplevel_tag_v1), with the D-Bus calls made from the same process, so the
# extension's process check (bus caller pid == window pid) sees what it sees with the bridge.
#
#   windows_ext_test.py                     the test (prints "ok"/"FAIL" lines, exits 1 on a failure)
#   windows_ext_test.py --window TAG        helper: shows one tagged window until killed
#   windows_ext_test.py --listen SECONDS    helper: subscribes to the extension's signals without calling it
#   windows_ext_test.py --call METHOD ARGS  helper: one call from another process (prints the reply/error)
# SPDX-License-Identifier: MIT
import mmap
import os
import select
import socket
import struct
import subprocess
import sys
import time

import gi
gi.require_version('Gio', '2.0')
from gi.repository import Gio, GLib  # noqa: E402

BUS_NAME = 'org.xwlbridge.Windows'
PATH = '/org/xwlbridge/Windows'
IFACE = 'org.xwlbridge.Windows1'


# ---------------------------------------------------------------------------------------------- Wayland


def _pad(b):
    return b + b'\0' * (-len(b) % 4)


def wl_string(s):
    b = s.encode() + b'\0'
    return struct.pack('<I', len(b)) + _pad(b)


class Wayland:
    def __init__(self):
        path = os.path.join(os.environ['XDG_RUNTIME_DIR'], os.environ.get('WAYLAND_DISPLAY', 'wayland-0'))
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.setblocking(False)
        self.next_id = 2
        self.handlers = {1: self._display_event}
        self.inbuf = b''
        self.globals = {}
        self.errors = []
        self.registry = self.new(self._registry_event)
        self.send(1, 1, struct.pack('<I', self.registry))  # wl_display.get_registry
        self.roundtrip()

    def new(self, handler=None):
        oid = self.next_id
        self.next_id += 1
        self.handlers[oid] = handler or (lambda op, data: None)
        return oid

    def send(self, obj, opcode, payload=b'', fds=()):
        msg = struct.pack('<II', obj, ((8 + len(payload)) << 16) | opcode) + payload
        anc = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack(f'{len(fds)}i', *fds))] if fds else []
        while True:
            try:
                self.sock.sendmsg([msg], anc)
                return
            except BlockingIOError:
                select.select([], [self.sock], [], 1)

    def bind(self, iface, version, handler=None):
        name, ver = self.globals[iface]
        oid = self.new(handler)
        self.send(self.registry, 0, struct.pack('<I', name) + wl_string(iface) + struct.pack('<II', min(ver, version), oid))
        return oid

    def roundtrip(self, timeout=5):
        done = []
        cb = self.new(lambda op, data: done.append(1))
        self.send(1, 0, struct.pack('<I', cb))  # wl_display.sync
        end = time.time() + timeout
        while not done and time.time() < end:
            self.dispatch(0.05)
        if not done:
            raise RuntimeError('wayland roundtrip timed out')

    def dispatch(self, timeout):
        r, _, _ = select.select([self.sock], [], [], timeout)
        if r:
            try:
                data = self.sock.recv(65536)
            except BlockingIOError:
                data = b''
            if r and data == b'':
                raise RuntimeError('compositor closed the connection')
            self.inbuf += data
        while len(self.inbuf) >= 8:
            obj, word = struct.unpack_from('<II', self.inbuf)
            size = word >> 16
            if len(self.inbuf) < size:
                break
            body = self.inbuf[8:size]
            self.inbuf = self.inbuf[size:]
            h = self.handlers.get(obj)
            if h:
                h(word & 0xffff, body)

    def _display_event(self, op, data):
        if op == 0:  # error(object, code, message)
            obj, code, n = struct.unpack_from('<III', data)
            msg = data[12:12 + n - 1].decode(errors='replace')
            self.errors.append((obj, code, msg))
            print(f'wayland error: object {obj} code {code}: {msg}', file=sys.stderr)

    def _registry_event(self, op, data):
        if op == 0:  # global(name, interface, version)
            name, n = struct.unpack_from('<II', data)
            iface = data[8:8 + n - 1].decode()
            off = 8 + n + (-n % 4)
            (ver,) = struct.unpack_from('<I', data, off)
            self.globals[iface] = (name, ver)


class Toplevel:
    """One xdg_toplevel with an shm buffer, redrawn at every configured size."""

    def __init__(self, wl, tag, title='xwlbridge extension test', size=(400, 300)):
        self.wl = wl
        self.size = size
        self.pending = None
        self.configured = 0
        self.closed = False
        if not hasattr(wl, 'compositor'):
            wl.compositor = wl.bind('wl_compositor', 4)
            wl.shm = wl.bind('wl_shm', 1)
            wl.wm_base = wl.bind('xdg_wm_base', 1, self._wm_base_event_for(wl))
            wl.tagmgr = wl.bind('xdg_toplevel_tag_manager_v1', 1) if 'xdg_toplevel_tag_manager_v1' in wl.globals else None
        self.surface = wl.new()
        wl.send(wl.compositor, 0, struct.pack('<I', self.surface))
        self.xdg_surface = wl.new(self._xdg_surface_event)
        wl.send(wl.wm_base, 2, struct.pack('<II', self.xdg_surface, self.surface))
        self.toplevel = wl.new(self._toplevel_event)
        wl.send(self.xdg_surface, 1, struct.pack('<I', self.toplevel))
        wl.send(self.toplevel, 2, wl_string(title))
        wl.send(self.toplevel, 3, wl_string('org.xwlbridge.ExtensionTest'))
        if tag is not None:
            if wl.tagmgr is None:
                raise RuntimeError('the compositor offers no xdg_toplevel_tag_manager_v1')
            wl.send(wl.tagmgr, 1, struct.pack('<I', self.toplevel) + wl_string(tag))
        wl.send(self.surface, 6)  # commit, no buffer: asks for the first configure

    @staticmethod
    def _wm_base_event_for(wl):
        def handler(op, data):
            if op == 0:  # ping(serial)
                wl.send(wl.wm_base, 3, data[:4])
        return handler

    def _toplevel_event(self, op, data):
        if op == 0:  # configure(width, height, states)
            w, h = struct.unpack_from('<ii', data)
            self.pending = (w, h)
        elif op == 1:
            self.closed = True

    def _xdg_surface_event(self, op, data):
        if op != 0:
            return
        (serial,) = struct.unpack_from('<I', data)
        if self.pending and self.pending[0] > 0 and self.pending[1] > 0:
            self.size = self.pending
        self.pending = None
        self.wl.send(self.xdg_surface, 4, struct.pack('<I', serial))  # ack_configure
        self.draw()
        self.configured += 1

    def draw(self):
        wl = self.wl
        w, h = self.size
        stride = w * 4
        nbytes = stride * h
        fd = os.memfd_create('xwlbridge-ext-test')
        os.ftruncate(fd, nbytes)
        with mmap.mmap(fd, nbytes) as m:
            m.write(b'\x60\x90\x30\xff' * (w * h))
        pool = wl.new()
        wl.send(wl.shm, 0, struct.pack('<Ii', pool, nbytes), fds=[fd])
        os.close(fd)
        buf = wl.new(lambda op, data: None)
        wl.send(pool, 0, struct.pack('<Iiiiii', buf, 0, w, h, stride, 1))  # XRGB8888
        wl.send(pool, 1)  # destroy the pool (the buffer keeps it)
        wl.send(self.xdg_surface, 3, struct.pack('<iiii', 0, 0, w, h))  # set_window_geometry
        wl.send(self.surface, 1, struct.pack('<Iii', buf, 0, 0))  # attach
        wl.send(self.surface, 2, struct.pack('<iiii', 0, 0, w, h))  # damage
        wl.send(self.surface, 6)  # commit

    def destroy(self):
        wl = self.wl
        wl.send(self.toplevel, 0)
        wl.send(self.xdg_surface, 0)
        wl.send(self.surface, 0)


# ---------------------------------------------------------------------------------------------- D-Bus


class Ext:
    def __init__(self):
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION)
        self.signals = []
        self.bus.signal_subscribe(BUS_NAME, IFACE, None, PATH, None, Gio.DBusSignalFlags.NONE, self._on_signal)

    def _on_signal(self, conn, sender, path, iface, name, params):
        self.signals.append((name, params.unpack()))

    def call(self, method, sig=None, *args):
        params = GLib.Variant(sig, args) if sig else None
        reply = self.bus.call_sync(BUS_NAME, PATH, IFACE, method, params, None, Gio.DBusCallFlags.NONE, 5000, None)
        return reply.unpack() if reply is not None else None

    def error(self, method, sig=None, *args):
        try:
            self.call(method, sig, *args)
        except GLib.Error as e:
            return Gio.DBusError.get_remote_error(e) or str(e)
        return None


# ---------------------------------------------------------------------------------------------- the test

PASS = 0
FAIL = 0


def now_ms():
    """An input timestamp as the compositor makes them (CLOCK_MONOTONIC in ms)."""
    return int(time.clock_gettime(time.CLOCK_MONOTONIC) * 1000) & 0xffffffff


def check(name, cond, detail=''):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f'ok   {name}', flush=True)
    else:
        FAIL += 1
        print(f'FAIL {name}' + (f'\n     {detail}' if detail else ''), flush=True)


def pump(wl, seconds, until=None):
    end = time.time() + seconds
    ctx = GLib.MainContext.default()
    while time.time() < end:
        wl.dispatch(0.01)
        while ctx.pending():
            ctx.iteration(False)
        if until and until():
            return True
    return bool(until and until())


def state_of(ext, tag):
    for t, d in ext.call('GetState')[0]:
        if t == tag:
            return d
    return None


def last_changed(ext, tag):
    for name, args in reversed(ext.signals):
        if name == 'Changed' and args[0] == tag:
            return args
    return None


def other_process(*args, timeout=10):
    p = subprocess.run([sys.executable, os.path.abspath(__file__), *args], capture_output=True, text=True,
                       timeout=timeout)
    return p.stdout.strip()


def main():
    wl = Wayland()
    ext = Ext()
    check('compositor offers xdg_toplevel_tag_manager_v1', 'xdg_toplevel_tag_manager_v1' in wl.globals)

    # -- public calls
    check('Version() == 1', ext.call('Version') == (1,))
    feats = ext.call('Features')[0]
    check('Features() lists move-resize, initial-position, state', all(f in feats for f in ('move-resize', 'initial-position', 'state')), feats)
    mons = ext.call('GetMonitors')[0]
    check('GetMonitors(): two virtual monitors, one primary', len(mons) == 2 and sum(1 for m in mons if m[4]) == 1, mons)
    print('     monitors:', mons)
    m0 = mons[0]
    wa0 = m0[2]

    # -- initial position before the window exists
    tag_a = 'xwlbridge:99:0x400001'
    ext.call('SetInitialPosition', '(sii)', tag_a, 300, 200)
    a = Toplevel(wl, tag_a)
    pump(wl, 5, lambda: (s := state_of(ext, tag_a)) is not None and s['shown'] and a.configured >= 1)
    s = state_of(ext, tag_a)
    check('window A tracked and shown', s is not None and s['shown'], s)
    check('SetInitialPosition before the window: opens at 300,200', s is not None and tuple(s['rect'][:2]) == (300, 200), s and s['rect'])
    pump(wl, 0.5)
    first = [args for name, args in ext.signals if name == 'Changed' and args[0] == tag_a]
    check('first Changed for A already at 300,200 (no jump reported)', bool(first) and tuple(first[0][1][:2]) == (300, 200), first[:3])

    # -- MoveResize: move + resize
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, 100, 120, 500, 350, 0)
    check('MoveResize reply = asked rect (no clamp)', tuple(reply[0]) == (100, 120, 500, 350), reply)
    ok = pump(wl, 5, lambda: (c := last_changed(ext, tag_a)) is not None and tuple(c[1]) == (100, 120, 500, 350))
    check('Changed(A) reports 100,120 500x350 after the client drew it', ok, last_changed(ext, tag_a))
    check('client got a 500x350 configure', a.size == (500, 350), a.size)

    # -- move only, size kept (w,h = 0)
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, 160, 140, 0, 0, 0)
    check('MoveResize move-only reply (applied at once)', tuple(reply[0]) == (160, 140, 500, 350), reply)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and tuple(c[1]) == (160, 140, 500, 350))
    check('Changed(A) after a move-only', ok, last_changed(ext, tag_a))

    # -- clamp to the work area of the window's monitor
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, wa0[0] - 400, wa0[1] + 50, 0, 0, 1)
    check('clamp: x pulled back to the work area', tuple(reply[0][:2]) == (wa0[0], wa0[1] + 50), (reply, wa0))
    # (A is 500 wide: mostly on monitor 0, then mostly past its right edge, on monitor 1)
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, wa0[0] + wa0[2] - 300, wa0[1], 0, 0, 1)
    check('clamp: right edge kept inside', reply[0][0] + reply[0][2] == wa0[0] + wa0[2], (reply, wa0))
    wa1_ = mons[1][2]
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, wa0[0] + wa0[2] - 100, wa0[1], 0, 0, 1)
    check('clamp: mostly on the next monitor -> inside that one', reply[0][0] == wa1_[0], (reply, wa1_))
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, 0, 0, 9000, 9000, 1)
    check('clamp: size limited to the work area', tuple(reply[0]) == tuple(wa0), (reply, wa0))
    ok = pump(wl, 5, lambda: (c := last_changed(ext, tag_a)) is not None and tuple(c[1]) == tuple(wa0))
    check('Changed(A) = the work area', ok, (last_changed(ext, tag_a), wa0))
    # -- the clamp is the target monitor's, and a monitor-sized rectangle stays
    m1, wa1 = mons[1][1], mons[1][2]
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, wa1[0] + 40, wa1[1] + 40, 300, 200, 1)
    check('clamp: a move to the other monitor goes there', tuple(reply[0]) == (wa1[0] + 40, wa1[1] + 40, 300, 200),
          (reply, wa1))
    pump(wl, 2)
    reply = ext.call('MoveResize', '(siiiiu)', tag_a, *m0[1], 1)
    check('clamp: a monitor-sized rectangle is not shrunk', tuple(reply[0]) == tuple(m0[1]), (reply, m0))
    pump(wl, 2)
    ext.call('MoveResize', '(siiiiu)', tag_a, 200, 150, 420, 320, 0)
    pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and tuple(c[1]) == (200, 150, 420, 320))

    # -- bad arguments
    check('a tag without the xwlbridge: prefix -> InvalidArgs',
          ext.error('MoveResize', '(siiiiu)', 'other:1', 0, 0, 10, 10, 0) == 'org.freedesktop.DBus.Error.InvalidArgs')
    check('an unknown tag -> NoSuchWindow',
          ext.error('Raise', '(s)', 'xwlbridge:99:0xdead') == 'org.xwlbridge.Windows1.Error.NoSuchWindow')
    check('position out of range -> InvalidArgs',
          ext.error('MoveResize', '(siiiiu)', tag_a, 1 << 24, 0, 10, 10, 0) == 'org.freedesktop.DBus.Error.InvalidArgs')

    # -- an untagged-prefix window of ours is never touched or listed
    b = Toplevel(wl, 'notours:1', title='not ours')
    pump(wl, 2, lambda: b.configured >= 1)
    check('GetState lists only xwlbridge: windows', [t for t, _ in ext.call('GetState')[0]] == [tag_a], ext.call('GetState'))

    # -- another process: cannot see or touch A
    out = other_process('--call', 'MoveResize', tag_a)
    check('another process: MoveResize(A) -> NoSuchWindow', 'NoSuchWindow' in out, out)
    out = other_process('--call', 'GetState')
    check('another process: GetState() is empty', out == '([],)', out)
    out = other_process('--call', 'Version')
    check('another process: Version() answers', out == '(1,)', out)

    # -- a window of another process with an xwlbridge: tag: we cannot touch it
    tag_c = 'xwlbridge:98:0x500001'
    helper = subprocess.Popen([sys.executable, os.path.abspath(__file__), '--window', tag_c])
    time.sleep(2)
    check('another process\'s tagged window -> NoSuchWindow for us',
          ext.error('MoveResize', '(siiiiu)', tag_c, 0, 0, 100, 100, 0) == 'org.xwlbridge.Windows1.Error.NoSuchWindow')
    check('  ...and not in our GetState', tag_c not in [t for t, _ in ext.call('GetState')[0]])
    # a listener that never calls the extension hears nothing (signals are unicast to the owner)
    listener = subprocess.Popen([sys.executable, os.path.abspath(__file__), '--listen', '4'], stdout=subprocess.PIPE, text=True)
    time.sleep(1)

    # -- state changes
    ext.signals.clear()
    ext.call('SetAbove', '(sb)', tag_a, True)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and c[4] & 16)
    check('SetAbove(true) -> Changed flags has above (16)', ok, last_changed(ext, tag_a))
    check('  ...GetState above', state_of(ext, tag_a)['above'] is True)
    ext.call('SetAbove', '(sb)', tag_a, False)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and not c[4] & 16)
    check('SetAbove(false) -> cleared', ok, last_changed(ext, tag_a))

    check('Activate(timestamp 0) -> InvalidArgs', ext.error('Activate', '(su)', tag_a, 0) ==
          'org.freedesktop.DBus.Error.InvalidArgs')
    ext.call('Activate', '(su)', tag_a, now_ms())
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and c[4] & 32)
    check('Activate -> focused (32)', ok, last_changed(ext, tag_a))

    ext.call('Minimize', '(sb)', tag_a, True)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and c[4] & 2)
    check('Minimize(true) -> minimized (2)', ok, last_changed(ext, tag_a))
    ext.call('Minimize', '(sb)', tag_a, False)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and not c[4] & 2)
    check('Minimize(false) -> cleared', ok, last_changed(ext, tag_a))

    ext.call('Stick', '(sb)', tag_a, True)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and c[3] == -1)
    check('Stick(true) -> workspace -1', ok, last_changed(ext, tag_a))
    ext.call('Stick', '(sb)', tag_a, False)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and c[3] >= 0)
    check('Stick(false) -> a workspace again', ok, last_changed(ext, tag_a))
    check('SetWorkspace(99) -> InvalidArgs', ext.error('SetWorkspace', '(si)', tag_a, 99) == 'org.freedesktop.DBus.Error.InvalidArgs')
    check('SetWorkspace(0) accepted', ext.error('SetWorkspace', '(si)', tag_a, 0) is None)
    check('SetDemandsAttention(true/false) accepted', ext.error('SetDemandsAttention', '(sb)', tag_a, True) is None and
          ext.error('SetDemandsAttention', '(sb)', tag_a, False) is None)
    check('Raise accepted', ext.error('Raise', '(s)', tag_a) is None)

    # -- SetInitialPosition on a shown window moves it now
    ext.call('SetInitialPosition', '(sii)', tag_a, 250, 260)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and tuple(c[1][:2]) == (250, 260))
    check('SetInitialPosition on a shown window moves it', ok, last_changed(ext, tag_a))

    # -- second window of ours: stacking
    tag_d = 'xwlbridge:99:0x400002'
    d = Toplevel(wl, tag_d, size=(200, 150))
    pump(wl, 4, lambda: (s := state_of(ext, tag_d)) is not None and s['shown'])
    ext.call('Activate', '(su)', tag_d, now_ms())
    pump(wl, 2)
    restacks = [args[0] for name, args in ext.signals if name == 'Restacked']
    check('Restacked lists our windows bottom-to-top', bool(restacks) and restacks[-1] == [tag_a, tag_d], restacks[-3:])
    st = ext.call('GetState')[0]
    check('GetState stack order matches', [t for t, dd in sorted(st, key=lambda e: e[1]['stack'])] == [tag_a, tag_d], st)
    ext.call('Activate', '(su)', tag_a, now_ms())
    pump(wl, 2)
    restacks = [args[0] for name, args in ext.signals if name == 'Restacked']
    check('Activate(A) -> Restacked [D, A]', bool(restacks) and restacks[-1] == [tag_d, tag_a], restacks[-3:])
    # -- a window that is not showing goes below every showing one (it stays mapped in X)
    ext.call('Minimize', '(sb)', tag_a, True)
    pump(wl, 2)
    restacks = [args[0] for name, args in ext.signals if name == 'Restacked']
    check('Minimize(A) -> Restacked [A, D]', bool(restacks) and restacks[-1] == [tag_a, tag_d], restacks[-3:])
    ext.call('Minimize', '(sb)', tag_a, False)
    ext.call('Activate', '(su)', tag_a, now_ms())
    pump(wl, 2)
    if ext.error('SetWorkspace', '(si)', tag_a, 1) is None:  # a second workspace exists (dynamic workspaces)
        pump(wl, 2)
        restacks = [args[0] for name, args in ext.signals if name == 'Restacked']
        check('A on another workspace -> Restacked [A, D]', bool(restacks) and restacks[-1] == [tag_a, tag_d],
              restacks[-3:])
        ext.call('SetWorkspace', '(si)', tag_a, 0)
        pump(wl, 2)
    ext.call('Activate', '(su)', tag_a, now_ms())
    pump(wl, 2)

    lout, _ = listener.communicate(timeout=10)
    check('a process that never called the extension received no signal', lout.strip() == '0', lout)

    # -- window gone
    ext.signals.clear()
    d.destroy()
    ok = pump(wl, 3, lambda: any(n == 'WindowGone' and a_[0] == tag_d for n, a_ in ext.signals))
    check('WindowGone(D) after the toplevel is destroyed', ok, ext.signals[-3:])

    # -- disable / enable the extension (as the lock screen does)
    shell_ext = Gio.DBusProxy.new_sync(ext.bus, Gio.DBusProxyFlags.NONE, None, 'org.gnome.Shell.Extensions',
                                       '/org/gnome/Shell/Extensions', 'org.gnome.Shell.Extensions', None)
    # a second window, minimized across the disable/enable
    tag_e = 'xwlbridge:99:0x400003'
    Toplevel(wl, tag_e, size=(180, 120))
    pump(wl, 4, lambda: (s := state_of(ext, tag_e)) is not None and s['shown'])
    ext.call('Minimize', '(sb)', tag_e, True)
    pump(wl, 2, lambda: (c := last_changed(ext, tag_e)) is not None and c[4] & 2)
    shell_ext.call_sync('DisableExtension', GLib.Variant('(s)', ('xwlbridge-windows@xwlbridge',)), 0, 5000, None)
    pump(wl, 1)
    check('disabled: the name is gone', ext.error('Version') is not None)
    shell_ext.call_sync('EnableExtension', GLib.Variant('(s)', ('xwlbridge-windows@xwlbridge',)), 0, 5000, None)
    pump(wl, 1)
    check('enabled again: Version answers', ext.call('Version') == (1,))
    s = state_of(ext, tag_a)
    check('  ...and GetState has A again (found by its tag)', s is not None and tuple(s['rect'][:2]) == (250, 260), s)
    s = state_of(ext, tag_e)
    check('  ...a window minimized across it: shown and minimized', s is not None and s['shown'] and s['minimized'], s)
    ext.signals.clear()
    ext.call('Minimize', '(sb)', tag_e, False)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_e)) is not None and not c[4] & 2)
    check('  ...restoring it sends Changed', ok, ext.signals[-3:])
    ext.signals.clear()
    ext.call('MoveResize', '(siiiiu)', tag_a, 300, 300, 0, 0, 0)
    ok = pump(wl, 3, lambda: (c := last_changed(ext, tag_a)) is not None and tuple(c[1][:2]) == (300, 300))
    check('  ...signals flow again after the resync', ok, ext.signals[-3:])

    helper.terminate()
    helper.wait(5)
    check('no Wayland protocol error', not wl.errors, wl.errors)
    print(f'\n{PASS} passed, {FAIL} failed')
    return 1 if FAIL else 0


def helper_window(tag):
    wl = Wayland()
    Toplevel(wl, tag, title='other process')
    while True:
        wl.dispatch(0.1)


def helper_listen(seconds):
    bus = Gio.bus_get_sync(Gio.BusType.SESSION)
    got = []
    bus.signal_subscribe(BUS_NAME, IFACE, None, PATH, None, Gio.DBusSignalFlags.NONE, lambda *a: got.append(a[4]))
    ctx = GLib.MainContext.default()
    end = time.time() + seconds
    while time.time() < end:
        ctx.iteration(False)
        time.sleep(0.01)
    print(len(got))


def helper_call(method, *args):
    bus = Gio.bus_get_sync(Gio.BusType.SESSION)
    params = None
    if method == 'MoveResize':
        params = GLib.Variant('(siiiiu)', (args[0], 0, 0, 100, 100, 0))
    try:
        r = bus.call_sync(BUS_NAME, PATH, IFACE, method, params, None, Gio.DBusCallFlags.NONE, 5000, None)
        print(r.unpack() if r is not None else '()')
    except GLib.Error as e:
        print(Gio.DBusError.get_remote_error(e) or e.message)


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--window':
        helper_window(sys.argv[2])
    elif len(sys.argv) > 1 and sys.argv[1] == '--listen':
        helper_listen(float(sys.argv[2]))
    elif len(sys.argv) > 1 and sys.argv[1] == '--call':
        helper_call(*sys.argv[2:])
    else:
        sys.exit(main())
