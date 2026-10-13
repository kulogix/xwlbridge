"""Restores what a proxy changed on the engine display if the proxy dies without doing it (a crash, SIGKILL).
Started by the settings mirror with a pipe as stdin whose write end only the proxy holds: when the proxy ends in any
way the pipe ends, and if the proxy's saved copy of RESOURCE_MANAGER is still on the root (a clean exit removes it),
it is put back -- the high-contrast and large-text resources would otherwise reach every Xt/Tk app started later.
Runs in its own session, so a terminal's Ctrl+C or a signal to the proxy's process group leaves it to do this.
Usage: python3 -m x11_a11y_proxy.cleanup --display :20"""
import argparse
import signal
import sys
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--display', required=True)
    a = ap.parse_args()
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, signal.SIG_IGN)
    while sys.stdin.buffer.read(4096):
        pass                     # the proxy never writes: this returns at its end
    from Xlib import display as xdisplay
    from .settings import restore_leftover
    for _ in range(3):
        try:
            d = xdisplay.Display(a.display)
            restore_leftover(d, log=lambda *m: sys.stderr.write('x11-a11y-proxy cleanup: ' + ' '.join(map(str, m)) + '\n'))
            d.close()
            return 0
        except Exception as ex:   # noqa: BLE001 -- the engine may be gone too: nothing to restore then
            err = ex
            time.sleep(0.3)
    sys.stderr.write(f'x11-a11y-proxy cleanup: engine {a.display} not reachable: {err!r}\n')
    return 1


if __name__ == '__main__':
    sys.exit(main())
