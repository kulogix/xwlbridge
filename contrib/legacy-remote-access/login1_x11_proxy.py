#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Part of xwlbridge (contrib/legacy-remote-access): see docs/legacy-remote-access-apps.md.
"""A system-bus proxy that makes logind's GNOME Wayland session look like an X11 session to ONE program.

Some remote-access servers (AnyDesk 8, TeamViewer before 15.29, ...) refuse incoming sessions, or capture the wrong
display, when logind's `Session.Type` of the console session is "wayland", whatever DISPLAY they are given. Started
with DBUS_SYSTEM_BUS_ADDRESS=unix:path=SOCK (or with SOCK bound over /run/dbus/system_bus_socket in its mount
namespace), such a program talks to the real system bus through this proxy, which rewrites only the replies to
org.freedesktop.DBus.Properties.Get/GetAll on org.freedesktop.login1.Session objects: Type "wayland" -> "x11" (and
Display "" -> DISPLAY_VALUE when given), so the program uses the X display of the bridge (e.g. :20). Nothing else is
changed; every other message is forwarded byte for byte. Unix-fd passing is declined during auth (the program then
works without fds).

  login1_x11_proxy.py SOCK [DISPLAY_VALUE]        (run as the same user as the program; Ctrl+C/SIGTERM stops it)
Log lines on stderr: every rewrite."""
import os, socket, sys, threading
from gi.repository import Gio, GLib

SOCK = sys.argv[1]
DISPLAY_VALUE = sys.argv[2] if len(sys.argv) > 2 else ''
REAL = '/run/dbus/system_bus_socket'
PROPS = 'org.freedesktop.DBus.Properties'
SESS = 'org.freedesktop.login1.Session'


def log(*a):
    print('login1_x11_proxy:', *a, file=sys.stderr, flush=True)


def recv_exact(s, n):
    b = b''
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return b


def read_msg(s):
    head = recv_exact(s, 16)
    need = Gio.DBusMessage.bytes_needed(head)
    return head + recv_exact(s, need - 16)


def fix_value(name, v):
    if name == 'Type' and v.get_type_string() == 's' and v.get_string() == 'wayland':
        return GLib.Variant('s', 'x11'), True
    if name == 'Display' and DISPLAY_VALUE and v.get_type_string() == 's' and v.get_string() == '':
        return GLib.Variant('s', DISPLAY_VALUE), True
    return v, False


class Conn:
    def __init__(self, cs):
        self.cs = cs
        self.ss = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.ss.connect(REAL)
        self.pending = {}          # serial -> ('get', prop) | ('getall',)
        self.lock = threading.Lock()

    def auth(self):
        """Relay the line-based SASL exchange; decline NEGOTIATE_UNIX_FD ourselves. Returns leftover bytes."""
        first = recv_exact(self.cs, 1)          # the credentials NUL byte
        self.ss.sendall(first)
        buf = b''
        while True:
            c = self.cs.recv(4096)
            if not c:
                raise EOFError
            buf += c
            while b'\r\n' in buf:
                line, buf = buf.split(b'\r\n', 1)
                if line.startswith(b'NEGOTIATE_UNIX_FD'):
                    self.cs.sendall(b'ERROR "fd passing not proxied"\r\n')
                    continue
                self.ss.sendall(line + b'\r\n')
                if line == b'BEGIN':
                    return buf
                if line.startswith(b'AUTH') or line.startswith(b'DATA'):
                    resp = b''
                    while b'\r\n' not in resp:
                        r = self.ss.recv(4096)
                        if not r:
                            raise EOFError
                        resp += r
                    self.cs.sendall(resp)

    def c2s(self, leftover):
        buf = leftover
        try:
            while True:
                while len(buf) < 16:
                    c = self.cs.recv(65536)
                    if not c:
                        raise EOFError
                    buf += c
                need = Gio.DBusMessage.bytes_needed(buf[:16])
                while len(buf) < need:
                    c = self.cs.recv(65536)
                    if not c:
                        raise EOFError
                    buf += c
                blob, buf = buf[:need], buf[need:]
                try:
                    m = Gio.DBusMessage.new_from_blob(blob, Gio.DBusCapabilityFlags.NONE)
                    if (m.get_message_type() == Gio.DBusMessageType.METHOD_CALL and m.get_interface() == PROPS
                            and (m.get_path() or '').startswith('/org/freedesktop/login1/session/')):
                        body = m.get_body()
                        if body is not None and body.n_children() >= 1 and body.get_child_value(0).get_string() == SESS:
                            with self.lock:
                                if m.get_member() == 'Get':
                                    self.pending[m.get_serial()] = ('get', body.get_child_value(1).get_string(), m.get_path())
                                elif m.get_member() == 'GetAll':
                                    self.pending[m.get_serial()] = ('getall', None, m.get_path())
                except Exception as e:  # never block traffic on a parse problem
                    log('c2s parse', e)
                self.ss.sendall(blob)
        except (EOFError, OSError):
            pass
        finally:
            for s in (self.ss, self.cs):
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def s2c(self):
        try:
            while True:
                blob = read_msg(self.ss)
                try:
                    m = Gio.DBusMessage.new_from_blob(blob, Gio.DBusCapabilityFlags.NONE)
                    if m.get_message_type() == Gio.DBusMessageType.METHOD_RETURN:
                        with self.lock:
                            p = self.pending.pop(m.get_reply_serial(), None)
                        if p:
                            body = m.get_body()
                            changed = False
                            if p[0] == 'get':
                                v = body.get_child_value(0).get_variant()
                                nv, changed = fix_value(p[1], v)
                                if changed:
                                    m.set_body(GLib.Variant('(v)', (nv,)))
                                    log(f'{p[2]} Get {p[1]}: {v.unpack()!r} -> {nv.unpack()!r}')
                            else:
                                d = body.get_child_value(0)
                                out = {}
                                for i in range(d.n_children()):
                                    e = d.get_child_value(i)
                                    k = e.get_child_value(0).get_string()
                                    v = e.get_child_value(1).get_variant()
                                    nv, ch = fix_value(k, v)
                                    if ch:
                                        log(f'{p[2]} GetAll {k}: {v.unpack()!r} -> {nv.unpack()!r}')
                                    changed |= ch
                                    out[k] = nv
                                if changed:
                                    m.set_body(GLib.Variant('(a{sv})', (out,)))
                            if changed:
                                blob = m.to_blob(Gio.DBusCapabilityFlags.NONE)
                except Exception as e:
                    log('s2c parse', e)
                self.cs.sendall(blob)
        except (EOFError, OSError):
            pass
        finally:
            for s in (self.ss, self.cs):
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def run(self):
        try:
            left = self.auth()
        except (EOFError, OSError):
            return
        threading.Thread(target=self.s2c, daemon=True).start()
        self.c2s(left)


def main():
    try:
        os.unlink(SOCK)
    except FileNotFoundError:
        pass
    ls = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    ls.bind(SOCK)
    os.chmod(SOCK, 0o600)
    ls.listen(16)
    log(f'listening on {SOCK} -> {REAL} (Type wayland->x11{", Display->" + DISPLAY_VALUE if DISPLAY_VALUE else ""})')
    while True:
        cs, _ = ls.accept()
        threading.Thread(target=Conn(cs).run, daemon=True).start()


if __name__ == '__main__':
    main()
