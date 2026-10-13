"""x11-a11y-proxy: accessible GNOME windows for X apps that live entirely on the bridge's engine display.

For each top-level window an app maps on the engine display (`-engine xvnc|xvfb`, e.g. :20), a GTK 4 window on GNOME
shows its live pixels and carries one accessible mirror per control, generated from the app's AT-SPI tree on the
engine's private accessibility bus (or inferred, for apps without one). The person's keys and clicks go to the app
through the bridge's reverse master (no echo back to GNOME), focus follows both ways, and GNOME's high contrast,
text scaling, cursor size and reduced motion are mirrored into the engine (XSETTINGS + RESOURCE_MANAGER).

Usage: x11-a11y-proxy [--display :20] [options] [-- COMMAND ARGS...]
With a COMMAND it is started on the engine display with the right environment, and the proxy exits with it."""
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time

import gi
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')
from gi.repository import Gdk, Gio, GLib, Gtk  # noqa: E402

from . import mirror as M  # noqa: E402
from .xengine import Engine  # noqa: E402
from .window import ProxyWindow  # noqa: E402

ACCESSIBILITY_CONF = ('/usr/share/defaults/at-spi2/accessibility.conf',
                      '/usr/local/share/defaults/at-spi2/accessibility.conf')
IGNORED_TITLES = re.compile(r'^(Qt Selection Owner for .*|sun-awt-X11-.*|xwlbridge.*)$')


def parse_args(argv):
    ap = argparse.ArgumentParser(prog='x11-a11y-proxy', description=__doc__.split('\n\n')[1],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--display', default=os.environ.get('X11_A11Y_PROXY_DISPLAY'),
                    help='the engine display (default: $X11_A11Y_PROXY_DISPLAY, else :20)')
    ap.add_argument('--bus', default='auto', help="the engine's private AT-SPI bus: an address, or 'auto' (the "
                    "engine's AT_SPI_BUS root property, set by the bridge's -a11ybus; else one is started here)")
    ap.add_argument('--match', help='only mirror windows whose title matches this regular expression')
    ap.add_argument('--a11y-wait', type=float, default=4.0,
                    help='seconds to wait for a window\'s accessibility tree before inferring one (default 4)')
    ap.add_argument('--inferred', choices=('auto', 'off', 'only'), default='auto',
                    help='inferred mode for windows without a tree (auto), never (off), or for every window (only)')
    ap.add_argument('--uidet', help='command speaking the uidet sidecar protocol on stdin/stdout (OCR + detector)')
    ap.add_argument('--uidet-fifo', help='directory with FIFOs req/resp served by a uidet sidecar')
    ap.add_argument('--no-settings', action='store_true',
                    help="don't mirror GNOME's appearance/accessibility settings into the engine")
    ap.add_argument('--settings-only', action='store_true', help='only mirror the settings (no windows)')
    ap.add_argument('--no-modsync', action='store_true',
                    help="don't add modifiers GNOME latched (sticky keys) or fix lock states around forwarded keys")
    ap.add_argument('--title-prefix', default='', help='prefix for the GNOME windows\' titles (default none)')
    ap.add_argument('--log', help='log file (default stderr)')
    ap.add_argument('--verbose', '-v', action='store_true')
    ap.add_argument('command', nargs=argparse.REMAINDER, help='-- COMMAND ARGS: run it on the engine display')
    a = ap.parse_args(argv)
    if a.command and a.command[0] == '--':
        a.command = a.command[1:]
    return a


def engine_env(display, bus):
    """The environment an X app needs to live fully on the engine display, with its tree on the private bus."""
    env = dict(os.environ)
    env.pop('WAYLAND_DISPLAY', None)
    env.update({'DISPLAY': display, 'XDG_SESSION_TYPE': 'x11', 'GDK_BACKEND': 'x11', 'QT_QPA_PLATFORM': 'xcb',
                'SDL_VIDEODRIVER': 'x11', 'ELECTRON_OZONE_PLATFORM_HINT': 'x11', 'MOZ_ENABLE_WAYLAND': '0',
                'CLUTTER_BACKEND': 'x11', 'QT_LINUX_ACCESSIBILITY_ALWAYS_ON': '1', 'ACCESSIBILITY_ENABLED': '1',
                'GNOME_ACCESSIBILITY': '1'})
    if bus:
        env['AT_SPI_BUS_ADDRESS'] = bus
    jar = '/usr/share/java/java-atk-wrapper.jar'
    if os.path.isfile(jar):
        opts = env.get('JAVA_TOOL_OPTIONS', '')
        if 'assistive_technologies' not in opts:
            env['JAVA_TOOL_OPTIONS'] = (opts + ' -Djavax.accessibility.assistive_technologies='
                                        'org.GNOME.Accessibility.AtkWrapper -Xbootclasspath/a:' + jar).strip()
    return env


class Proxy(Gtk.Application):
    def __init__(self, args):
        super().__init__(application_id=None, flags=Gio.ApplicationFlags.NON_UNIQUE)
        self.args = args
        self.verbose = args.verbose
        self.modsync = not args.no_modsync
        self.title_prefix = args.title_prefix
        self.logf = open(args.log, 'a', buffering=1) if args.log else sys.stderr
        self.display = args.display or ':20'
        self.windows = {}        # X window id -> ProxyWindow
        self.by_at = {}          # reader window id -> ProxyWindow
        self.at_windows = {}     # reader window id -> info
        self.reader = None
        self.own_bus = None
        self.set_bus_prop = False
        self.settings = None
        self.inferrer = None
        self.uidet = None
        self.child = None
        self.cursor_src = None
        self.hold_ref = None

    def log(self, *a):
        t = time.time()
        self.logf.write(time.strftime('%H:%M:%S', time.localtime(t)) + f'.{int(t * 1000) % 1000:03d} ' +
                        ' '.join(str(x) for x in a) + '\n')

    # ---- start ----------------------------------------------------------------------------------------------------
    def do_activate(self):
        if self.hold_ref is None:
            self.hold_ref = self.hold()   # live without windows (apps come and go)
        M.install_css()
        self.engine = Engine(self.display, log=self.log)
        if not self.args.no_settings:
            from .settings import SettingsMirror
            try:
                self.settings = SettingsMirror(self.display, log=self.log)
                self.settings.start()
            except Exception as ex:
                self.log('settings: not mirrored:', repr(ex))
        if self.args.settings_only:
            return
        self.bus = self.resolve_bus()
        self.start_reader()
        if self.args.inferred != 'off':
            from .inferred import Inferrer, Uidet
            if self.args.uidet or self.args.uidet_fifo:
                try:
                    self.uidet = Uidet(cmd=self.args.uidet, fifo_dir=self.args.uidet_fifo, log=self.log, timeout=20)
                except Exception as ex:
                    self.log('uidet: unavailable:', repr(ex))
            self.inferrer = Inferrer(self.engine, self.display, uidet=self.uidet, log=self.log)
        self.engine.listeners.append(self.on_x)
        self.engine.damage_cb = self.on_damage
        GLib.io_add_watch(self.engine.fileno(), GLib.IO_IN, self.on_x_fd)
        for t in list(self.engine.toplevels.values()):
            if t.mapped:
                self.on_x('map', t)
        if self.args.command:
            self.launch(self.args.command)
        GLib.timeout_add(500, self.housekeeping)

    def resolve_bus(self):
        if self.args.bus and self.args.bus != 'auto':
            return self.args.bus
        prop = self.engine.root_string('AT_SPI_BUS')
        if prop:
            self.log('bus: the engine\'s private AT-SPI bus (root AT_SPI_BUS):', prop)
            return prop
        conf = next((c for c in ACCESSIBILITY_CONF if os.path.isfile(c)), None)
        if not conf or not shutil.which('dbus-daemon'):
            self.log('bus: no private bus (no at-spi2-core accessibility.conf or dbus-daemon); apps on the engine '
                     'register on the desktop\'s bus: NOT mirrored, and Orca may follow them')
            return None
        rt = os.environ.get('XDG_RUNTIME_DIR') or '/tmp'
        sock = os.path.join(rt, f'x11-a11y-proxy-{self.display.lstrip(":").replace(".", "_")}-{os.getpid()}')
        addr = f'unix:path={sock}'
        env = dict(os.environ, AT_SPI_BUS_ADDRESS=addr, DISPLAY=self.display)
        env.pop('WAYLAND_DISPLAY', None)
        self.own_bus = subprocess.Popen(['dbus-daemon', f'--config-file={conf}', f'--address={addr}', '--nofork',
                                         '--nopidfile'], env=env, stdout=subprocess.DEVNULL)
        for _ in range(100):
            if os.path.exists(sock):
                break
            time.sleep(0.02)
        self.engine.root.change_property(self.engine.atom('AT_SPI_BUS'), self.engine.atom('STRING'), 8,
                                         addr.encode())
        self.engine.d.sync()
        self.set_bus_prop = True
        self.log('bus: started a private AT-SPI bus for the engine (bridge without -a11ybus):', addr)
        return addr

    def start_reader(self):
        if not self.bus:
            return
        env = dict(os.environ, AT_SPI_BUS_ADDRESS=self.bus, DISPLAY=self.display)
        env.pop('WAYLAND_DISPLAY', None)
        pkg = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        env['PYTHONPATH'] = pkg + (':' + env['PYTHONPATH'] if env.get('PYTHONPATH') else '')
        self.reader = subprocess.Popen([sys.executable, '-m', 'x11_a11y_proxy.reader'], env=env,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
        self.reader_beat = time.monotonic() + 5.0   # start-up grace
        self.reader_loop_ms = 0
        # read the raw fd and split lines ourselves: with a buffered readline() per wake-up, a second message that
        # arrived in the same chunk waited in Python's buffer until the pipe woke us again (the next key): a check
        # box's "checked" right behind its focus came 3 s late (parity_r5..r7 Electron)
        fd = self.reader.stdout.fileno()
        os.set_blocking(fd, False)
        self.reader_buf = b''
        proc = self.reader
        # bound to this reader: after a restart, the old one's last bytes must not land in the new one's buffer
        GLib.io_add_watch(fd, GLib.IO_IN | GLib.IO_HUP, lambda f, c: self.on_reader_fd(f, c) if proc is self.reader
                          else False)

    def launch(self, cmd):
        env = engine_env(self.display, self.bus)
        self.log('launch:', cmd)
        self.child = subprocess.Popen(cmd, env=env)
        GLib.child_watch_add(GLib.PRIORITY_DEFAULT, self.child.pid, self.on_child_exit)

    def on_child_exit(self, pid, status):
        self.log('launch: the command exited', status)
        GLib.timeout_add(500, self.quit_all)

    # ---- X events -------------------------------------------------------------------------------------------------
    def on_x_fd(self, *_):
        self.engine.pump()
        return True

    def wanted(self, t):
        if t.geometry[2] < 8 or t.geometry[3] < 8 or IGNORED_TITLES.match(t.title or ''):
            return False
        if self.args.match and not re.search(self.args.match, t.title or ''):
            return False
        return True

    def on_x(self, kind, t):
        if kind == 'map':
            if t.id in self.windows or not self.wanted(t):
                return
            pw = ProxyWindow(self, t)
            self.windows[t.id] = pw
            self.log('window: mirroring', t)
            self.match_all()
            GLib.timeout_add(int(self.args.a11y_wait * 1000), self.maybe_infer, t.id)
        elif kind in ('unmap', 'destroy'):
            pw = self.windows.pop(t.id, None)
            if pw:
                self.log('window: gone', t)
                if pw.at_window in self.by_at:
                    del self.by_at[pw.at_window]
                pw.destroy()
        elif kind == 'configure':
            pw = self.windows.get(t.id)
            if pw:
                pw.resized()
            for w in self.windows.values():
                if t.id in w.popup_pics:
                    w.popup_changed(t, True)
        elif kind == 'title':
            pw = self.windows.get(t.id)
            if pw:
                pw.set_title()
                self.match_all()
        elif kind in ('popup-map', 'popup-unmap'):
            for w in self.windows.values():
                w.popup_changed(t, kind == 'popup-map')

    def on_damage(self, wid):
        pw = self.windows.get(wid)
        if pw:
            pw.damaged()
            return
        for w in self.windows.values():
            p = self.engine.popups.get(wid)
            if p is not None and wid in w.popup_pics:
                w.popup_changed(p, True)

    def frame_done(self, pw):
        pass

    # ---- the reader -------------------------------------------------------------------------------------------------
    def send(self, msg):
        if self.reader and self.reader.poll() is None:
            try:
                self.reader.stdin.write(json.dumps(msg) + '\n')
                self.reader.stdin.flush()
            except (BrokenPipeError, OSError):
                pass

    def request_tree(self, pw):
        if pw.at_window:
            self.send({'do': 'tree', 'window': pw.at_window})

    def request_tree_soon(self, pw, nid, ms=300):
        if getattr(pw, 'tree_src', None):
            return

        def run():
            pw.tree_src = None
            if nid not in pw.mirrors and pw.at_window:
                self.log(f'tree: focused node {nid} has no mirror: reading window {pw.at_window} again')
                self.request_tree(pw)
            return False
        pw.tree_src = GLib.timeout_add(ms, run)

    def on_reader_fd(self, fd, cond, quiet=False):
        eof = False
        while True:
            try:
                chunk = os.read(fd, 1 << 16)
            except BlockingIOError:
                break
            except OSError:
                eof = True
                break
            if not chunk:
                eof = True
                break
            self.reader_buf += chunk
        *lines, self.reader_buf = self.reader_buf.split(b'\n')
        for raw in lines:
            if raw.strip():
                self.on_reader_line(raw.decode('utf-8', errors='replace'))
        if eof:
            if not quiet:
                self.log('reader: gone')
            return False
        return True

    def on_reader_line(self, line):
        if not line:
            return True
        try:
            m = json.loads(line)
        except ValueError:
            return True
        t = m.get('t')
        if t == 'beat':
            self.reader_beat = time.monotonic()
            self.reader_loop_ms = m.get('loop_ms', 0)
            return True
        if self.verbose and t != 'tree':
            self.log('reader:', line.strip()[:300])
        try:
            if t == 'window':
                self.at_windows[m['id']] = m
                self.match_all()
            elif t == 'window-gone':
                self.at_windows.pop(m['id'], None)
                pw = self.by_at.pop(m['id'], None)
                if pw and m['id'] in pw.popup_roots:
                    pw.remove_popup(m['id'])
                elif pw:
                    pw.at_window = None
            elif t == 'tree':
                pw = self.by_at.get(m['window'])
                if pw:
                    self.log(f'tree: window {m["window"]} root {m["root"]}: {len(m["nodes"])} nodes in {m.get("ms")} ms')
                    pw.build(m['nodes'], m['root'])
                    pf = getattr(pw, 'pending_focus', None)
                    if pf is not None and pw.focus_mirror(pf):
                        pw.pending_focus = None
            elif t == 'ev':
                pw = self.by_at.get(m.get('window'))
                if pw:
                    if m['kind'] == 'focus':
                        self.log('focus: app ->', m['id'], pw.nodes.get(m['id'], {}).get('name'))
                        if m['id'] not in pw.mirrors and m['id'] != pw.at_window:
                            # the app focused a node we have no mirror for -- unknown, or known but not showing when
                            # the tree was read (Electron's page controls, read a moment before the page was laid
                            # out: proxy_fix el_suite2): read the window again unless something brings it first
                            self.request_tree_soon(pw, m['id'])
                    pw.node_event(m)
            elif t == 'done' and not m.get('ok'):
                self.log('reader: command failed', m)
            elif t == 'error':
                self.log('reader: error', m)
        except Exception:
            import traceback
            self.log('reader message failed:', traceback.format_exc())
        return True

    def match_all(self):
        """Pairs X top-levels with the reader's top-level accessibles: same pid and title, else same title, else
        same pid and size."""
        free = [pw for pw in self.windows.values() if pw.at_window is None and not pw.inferred]
        for aid, info in self.at_windows.items():
            if aid in self.by_at:
                continue
            name = info.get('name') or ''

            def score(pw):
                s = 0
                if name and name == (pw.top.title or ''):
                    s += 4
                if info.get('pid') and info['pid'] == pw.top.pid:
                    s += 2
                sb = info.get('sbox')
                if sb and sb[2] == pw.top.geometry[2] and sb[3] == pw.top.geometry[3]:
                    s += 1
                return s
            cands = sorted(((score(pw), pw) for pw in free), key=lambda x: -x[0])
            if cands and cands[0][0] >= 3:
                pw = cands[0][1]
                pw.at_window = aid
                pw.toolkit = (info.get('toolkit') or '').lower()
                pw.app_name = info.get('app') or ''
                self.by_at[aid] = pw
                free.remove(pw)
                self.log(f'window: 0x{pw.top.id:x} {pw.top.title!r} <-> accessible {aid} ({info.get("app")}, '
                         f'{info.get("toolkit")})')
                self.send({'do': 'tree', 'window': aid})
        # popups (menus, combo lists): accessible windows with no X top-level of their own; their mirrors go into
        # the app's window
        for aid, info in self.at_windows.items():
            if aid in self.by_at:
                continue
            owner = [pw for pw in self.windows.values() if pw.top.pid and pw.top.pid == info.get('pid')]
            if owner and (info.get('role') in ('window', 'menu', 'popup menu', 'tool tip', 'tooltip') or
                          not info.get('name')):
                pw = owner[0]
                pw.add_popup(aid, info)
                self.by_at[aid] = pw
                self.send({'do': 'tree', 'window': aid})
        # a window that was inferred and now has a tree: switch to it
        for pw in self.windows.values():
            if pw.inferred and pw.at_window is None:
                for aid, info in self.at_windows.items():
                    if aid not in self.by_at and info.get('name') == pw.top.title:
                        pw.inferred = False
                        pw.at_window = aid
                        self.by_at[aid] = pw
                        self.send({'do': 'tree', 'window': aid})

    def maybe_infer(self, wid):
        pw = self.windows.get(wid)
        if pw is None or pw.at_window is not None or self.inferrer is None:
            return False
        if self.args.inferred == 'off':
            return False
        self.log(f'window: 0x{wid:x} {pw.top.title!r}: no accessibility tree after {self.args.a11y_wait}s: inferring')
        self.leak_check(pw)
        rm = self.engine.root_string('RESOURCE_MANAGER')
        nodes, live, src = self.inferrer.build(pw.top, rm)
        pw.source = 'tk' if src.get('tk') else 'editres' if src.get('editres') else 'xtree'
        pw.local_nav = pw.source != 'tk'
        pw.build(nodes, 1, inferred=True)
        if pw.source == 'tk':
            fid = next((n['id'] for n in nodes if 'focused' in n.get('states', [])), None)
            if fid:
                pw.focus_mirror(fid)
            GLib.timeout_add(700, self.tk_poll, wid)
        for nid in live:
            node = pw.nodes.get(nid)
            if node:
                pw.live[nid] = self.make_live(pw, node)
        return False

    def leak_check(self, pw):
        """Is this window's app on the DESKTOP's accessibility bus instead of the engine's? GTK 4 asks the session
        bus for the accessibility bus before it reads the display's AT_SPI_BUS property (GTK 3, Qt and Java read the
        property first), so a GTK 4 app started on the engine without AT_SPI_BUS_ADDRESS -- from a terminal with
        DISPLAY=:20, by a service -- registers on the desktop's bus, and Orca follows its invisible window
        (a known limit). The proxy cannot move it; it says so (log and an announcement)."""
        if not pw.top.pid:
            return
        env = dict(os.environ)
        env.pop('AT_SPI_BUS_ADDRESS', None)
        if env.get('DISPLAY') == self.display:
            env.pop('DISPLAY')   # libatspi would take the engine's AT_SPI_BUS property: the private bus
        pkg = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        env['PYTHONPATH'] = pkg + (':' + env['PYTHONPATH'] if env.get('PYTHONPATH') else '')
        try:
            p = subprocess.Popen([sys.executable, '-m', 'x11_a11y_proxy.reader', '--list-apps'], env=env,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        except OSError:
            return

        def done(pid, status):
            try:
                apps = json.loads(p.stdout.read() or '[]')
            except ValueError:
                apps = []
            hit = [a for a in apps if a.get('pid') == pw.top.pid]
            if hit:
                a = hit[0]
                self.log(f'window: 0x{pw.top.id:x} {pw.top.title!r}: its app ({a["name"]}, {a["toolkit"]}, pid '
                         f'{a["pid"]}) is on the DESKTOP\'s accessibility bus, not the engine\'s: screen readers see '
                         f'its window, which nobody sees, and the mirrors here are only inferred. Start it with '
                         f'"x11-a11y-proxy --display {self.display} -- COMMAND", or with AT_SPI_BUS_ADDRESS={self.bus}')
                pw.leaked = a
                if pw.win.is_active() and hasattr(pw.root, 'announce'):
                    pw.root.announce(f'{a["name"]} is not using the private accessibility bus; '
                                     f'restart it through x11-a11y-proxy.', Gtk.AccessibleAnnouncementPriority.MEDIUM)
            p.stdout.close()
        GLib.child_watch_add(GLib.PRIORITY_DEFAULT, p.pid, done)

    # ---- Tk apps (introspection through Tk's send): poll focus and state after keys --------------------------
    def tk_poll(self, wid):
        pw = self.windows.get(wid)
        if pw is None or pw.source != 'tk':
            return False
        if pw.win.is_active():
            self.tk_refresh(pw)
        return True

    def tk_refresh_soon(self, pw):
        if getattr(pw, 'tk_src', None):
            return

        def run():
            pw.tk_src = None
            self.tk_refresh(pw)
            return False
        pw.tk_src = GLib.timeout_add(90, run)

    def tk_refresh(self, pw):
        nodes = self.inferrer.tk_nodes(pw.top)
        if nodes is None:
            return
        old = pw.nodes
        new_ids = {n['id'] for n in nodes}
        if new_ids - set(old) or any(i not in new_ids for i in pw.mirrors if i):
            pw.build(nodes, 1, inferred=True)   # widgets came or went: rebuild
        for n in nodes:
            o = old.get(n['id'])
            if o is None:
                continue
            for st in ('checked', 'sensitive'):
                if (st in n['states']) != (st in o.get('states', [])):
                    pw.node_event({'kind': 'state', 'id': n['id'], 'state': st, 'on': st in n['states']})
            if n.get('text') is not None and n.get('text') != o.get('text'):
                o['text'] = n['text']
                pw.node_event({'kind': 'text', 'id': n['id'], 'text': n['text'], 'caret': n.get('caret')})
            if n.get('value') and o.get('value') and n['value'][0] != o['value'][0]:
                o['value'] = n['value']
                pw.node_event({'kind': 'value', 'id': n['id'], 'value': n['value'][0]})
        fid = next((n['id'] for n in nodes if 'focused' in n['states']), None)
        ofid = next((i for i, n in old.items() if 'focused' in n.get('states', [])), None)
        for n in nodes:
            if n['id'] in old:
                old[n['id']]['states'] = n['states']
        if fid and fid != ofid:
            self.log('focus: tk ->', fid, pw.nodes.get(fid, {}).get('name'), pw.nodes.get(fid, {}).get('tkpath'))
            pw.focus_mirror(fid)

    def tk_focus(self, pw, node):
        found = self.inferrer.tk_apps.get(pw.top.id) if self.inferrer else None
        if found and node.get('tkpath'):
            self.inferrer.tk.send(found[0], found[1], f'focus {node["tkpath"]}')

    def make_live(self, pw, node):
        state = {'last': node.get('name'), 'at': 0.0, 'src': None}

        def check():
            if state['src']:
                return
            # re-read after the display settles; at most every 400 ms

            def run():
                state['src'] = None
                txt = self.inferrer.reread(pw.top, node)
                if self.verbose:
                    self.log('live: re-read', node['id'], repr(txt))
                if txt is None and not state.get('retried'):
                    state['retried'] = True    # mid-redraw, or a read the recogniser missed: once more
                    state['src'] = GLib.timeout_add(600, run)
                    return False
                state['retried'] = False
                if txt and txt != state['last']:
                    state['last'] = txt
                    m = pw.mirrors.get(node['id'])
                    if m:
                        m.update_property([Gtk.AccessibleProperty.LABEL], [txt])
                        m.announce(txt, Gtk.AccessibleAnnouncementPriority.MEDIUM)
                        self.log('live:', node['id'], repr(txt))
                return False
            state['src'] = GLib.timeout_add(400, run)
        return check

    # ---- mirrors -> app ---------------------------------------------------------------------------------------------
    def mirror_action(self, m, index):
        n = m.node
        if n.get('tkpath'):
            for pw in self.windows.values():
                if n.get('id') in pw.mirrors and pw.mirrors[n['id']] is m:
                    r = self.inferrer.tk_invoke(pw.top, n)
                    self.log('tk: invoke', n['tkpath'], '->', r)
                    self.tk_refresh_soon(pw)
                    return
            self.log('tk: invoke: no window for mirror', n.get('id'), n.get('tkpath'))
            return
        if n.get('provenance', '').startswith('inferred') or 'xwindow' in n:
            for pw in self.windows.values():
                if n.get('id') in pw.mirrors and pw.mirrors[n['id']] is m:
                    pw.click_node(n)
            return
        self.send({'do': 'action', 'id': n['id'], 'index': index})

    def mirror_focused(self, m):
        for pw in self.windows.values():
            if m.node.get('id') in pw.mirrors and pw.mirrors[m.node['id']] is m:
                pw.mirror_focused(m)
                return

    # ---- cursor -----------------------------------------------------------------------------------------------------
    def cursor_follow(self, pw):
        if self.cursor_src:
            return

        def run():
            self.cursor_src = None
            c = self.engine.cursor_image()
            if c is None:
                return False
            w, h, xh, yh, data = c
            key = (w, h, xh, yh, hash(data))
            if key == pw.cursor_serial:
                return False
            pw.cursor_serial = key
            tex = Gdk.MemoryTexture.new(w, h, Gdk.MemoryFormat.B8G8R8A8_PREMULTIPLIED, GLib.Bytes.new(data), w * 4)
            pw.pic.set_cursor(Gdk.Cursor.new_from_texture(tex, xh, yh, None))
            return False
        self.cursor_src = GLib.timeout_add(120, run)

    # ---- housekeeping / exit ---------------------------------------------------------------------------------------
    READER_SILENT_S = 4.0     # no heartbeat this long: the reader is frozen or dead
    READER_STUCK_S = 20.0     # its main loop blocked this long in a call into an app

    def housekeeping(self):
        now = time.monotonic()
        busy = now - getattr(self, 'hk_last', now) > 2.0   # we were busy ourselves (an inferred build, OCR)
        self.hk_last = now
        if self.reader and self.reader.poll() is None and busy:
            self.reader_beat = now   # its beats may still sit in the pipe: judge it next time
        elif self.reader and self.reader.poll() is None:
            self.on_reader_fd(self.reader.stdout.fileno(), GLib.IO_IN, quiet=True)   # beats already sent count
            silent = time.monotonic() - self.reader_beat
            why = (f'no heartbeat for {silent:.1f} s (frozen)' if silent > self.READER_SILENT_S else
                   f'its main loop blocked for {self.reader_loop_ms / 1000:.1f} s'
                   if self.reader_loop_ms > self.READER_STUCK_S * 1000 else None)
            if why:
                hist = [t for t in getattr(self, 'reader_kills', []) if time.monotonic() - t < 60]
                if len(hist) < 3:
                    self.reader_kills = hist + [time.monotonic()]
                    self.log(f'reader: {why}: killing it (commands queued to it are dropped) and starting another')
                    self.announce_all('Accessibility of this window is reconnecting.')
                    self.reader.kill()
                    try:
                        self.reader.wait(1)
                    except subprocess.TimeoutExpired:
                        pass
                elif not getattr(self, 'reader_gave_up', False):
                    self.reader_gave_up = True
                    self.log(f'reader: {why}, and 3 restarts in a minute: leaving it')
        if self.reader and self.reader.poll() is not None and self.bus:
            self.log('reader: exited; restarting')
            self.by_at.clear()
            self.at_windows.clear()
            for pw in self.windows.values():
                pw.at_window = None
            self.start_reader()
        return True

    def announce_all(self, text):
        """An announcement on the active mirrored window (GTK >= 4.14), for the person using a screen reader."""
        for pw in self.windows.values():
            if pw.win.is_active() and hasattr(pw.root, 'announce'):
                try:
                    pw.root.announce(text, Gtk.AccessibleAnnouncementPriority.MEDIUM)
                except Exception:   # noqa: BLE001
                    pass

    def quit_all(self, *_):
        self.log('exiting')
        try:
            self.engine.release_all()
        except Exception:
            pass
        if self.settings:
            self.settings.stop()
        if self.reader and self.reader.poll() is None:
            self.send({'do': 'quit'})
            try:
                self.reader.wait(2)
            except subprocess.TimeoutExpired:
                self.reader.kill()
        if self.set_bus_prop:
            try:
                self.engine.root.delete_property(self.engine.atom('AT_SPI_BUS'))
                self.engine.d.sync()
            except Exception:
                pass
        if self.own_bus:
            self.own_bus.terminate()
        if self.child and self.child.poll() is None:
            self.child.terminate()
        for pw in list(self.windows.values()):
            pw.destroy()
        self.quit()
        return False


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if not args.display:
        args.display = ':20'
    GLib.set_prgname('x11-a11y-proxy')
    GLib.set_application_name('X11 accessibility proxy')
    app = Proxy(args)
    for sig in (signal.SIGINT, signal.SIGTERM):
        GLib.unix_signal_add(GLib.PRIORITY_HIGH, sig, app.quit_all)
    return app.run([])


if __name__ == '__main__':
    sys.exit(main())
