#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Part of xwlbridge (contrib/legacy-remote-access): see docs/legacy-remote-access-apps.md.
"""Multi-user front end for login1_x11_proxy.py (same directory).

The original proxy serves one user (socket 0600, its own connection to the real bus). A root daemon that starts its
helper processes as the desktop user (TeamViewer 15.20's teamviewerd -> TeamViewer_Desktop / TeamViewer as uid 1000)
needs one socket for both: this front end listens 0666 and serves each connection with the CLIENT's identity — a
connection from another uid is handled in a child that first drops to that uid/gid, so the real system bus sees the
same credentials it would have seen without the proxy (SASL EXTERNAL keeps working, no privilege is lent).
Rewrites: the original's (logind Session.Type "wayland" -> "x11", Display "" -> DISPLAY_VALUE); nothing else.

  login1_x11_proxy_mu.py SOCK [DISPLAY_VALUE]          (as root; SOCK's directory must exist)
"""
import os
import signal
import socket
import struct
import sys
import threading

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import login1_x11_proxy as P   # noqa: E402  (same directory; module-level SOCK/DISPLAY_VALUE from argv)


def main():
    try:
        os.unlink(P.SOCK)
    except FileNotFoundError:
        pass
    ls = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    ls.bind(P.SOCK)
    os.chmod(P.SOCK, 0o666)
    ls.listen(32)
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)       # children are reaped automatically
    P.log(f'multi-user: listening on {P.SOCK} -> {P.REAL} (Type wayland->x11'
          f'{", Display->" + P.DISPLAY_VALUE if P.DISPLAY_VALUE else ""})')
    me = os.geteuid()
    while True:
        cs, _ = ls.accept()
        pid, uid, gid = struct.unpack('3i', cs.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize('3i')))
        if uid == me:
            threading.Thread(target=P.Conn(cs).run, daemon=True).start()
            continue
        if os.fork() == 0:                               # child: become the client, then proxy this one connection
            try:
                ls.close()
                os.setgroups([])
                os.setgid(gid)
                os.setuid(uid)
                P.log(f'pid {pid} uid {uid}: served as uid {uid}')
                P.Conn(cs).run()
            except Exception as e:                      # noqa: BLE001
                P.log(f'pid {pid} uid {uid}: {e!r}')
            finally:
                os._exit(0)
        cs.close()


if __name__ == '__main__':
    main()
