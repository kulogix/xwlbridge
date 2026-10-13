#!/usr/bin/env python3
"""The tree reader: reads the accessibility trees of the apps on the engine display from the engine's PRIVATE AT-SPI
bus and streams them to the proxy as JSON lines. Runs as its own process because libatspi picks its bus once, from
AT_SPI_BUS_ADDRESS, at start, while the proxy itself must be on the desktop's bus (where Orca reads it).

Out (stdout, one JSON object per line):
  {"t":"beat","loop_ms":N}                                    every second (liveness; N = age of the main loop's tick)
  {"t":"window","id":W,"app":..,"pid":..,"name":..,"role":..,"sbox":[x,y,w,h]}   a top-level accessible appeared
  {"t":"window-gone","id":W}
  {"t":"tree","window":W,"root":R,"nodes":[node...]}       full tree of W (R = W) or a subtree after a change (R)
  {"t":"ev","kind":"focus"|"state"|"value"|"text"|"caret"|"selection"|"name"|"desc"|"announce"|"active",
   "id":N,"window":W,...}
  node = {id, parent, idx, role, name, desc, states[], box[x,y,w,h] (WINDOW coords), sbox (SCREEN coords),
          actions[[name, keybinding, description]], value[cur,min,max,step], vtext, text, caret, sel[[s,e]],
          attrs{}, rel{type:[ids]}, image}
In (stdin): {"do":"tree","window":W} | {"do":"action","id":N,"index":I} | {"do":"action","id":N,"name":S}
  | {"do":"set_value","id":N,"value":V} | {"do":"focus","id":N} | {"do":"set_caret","id":N,"offset":O}
  | {"do":"set_selection","id":N,"start":S,"end":E} | {"do":"insert_text","id":N,"text":T}
  | {"do":"windows"} | {"do":"quit"}
Every command may carry "req" which the reply {"t":"done","req":..,"ok":..} echoes."""
import json
import os
import sys
import threading
import time

import gi
gi.require_version('Atspi', '2.0')
from gi.repository import Atspi, GLib  # noqa: E402

MAX_NODES = int(os.environ.get('X11_A11Y_PROXY_MAX_NODES', '4000'))
MAX_CHILDREN = 400   # per container that manages its descendants (big lists/tables): only this many
WINDOW_ROLES = {'frame', 'window', 'dialog', 'alert', 'file chooser', 'color chooser', 'font chooser'}
TEXT_ROLES = {'text', 'entry', 'password text', 'terminal', 'paragraph', 'document text', 'editbar', 'spin button',
              'combo box', 'date editor', 'section', 'heading', 'static', 'label', 'caption', 'document web',
              'document frame'}
STATES = [(n, getattr(Atspi.StateType, n.upper().replace(' ', '_').replace('-', '_')))
          for n in ('focusable', 'focused', 'checked', 'indeterminate', 'pressed', 'sensitive', 'enabled', 'showing',
                    'visible', 'editable', 'expandable', 'expanded', 'selectable', 'selected', 'multi-line',
                    'single-line', 'horizontal', 'vertical', 'modal', 'required', 'invalid-entry', 'busy',
                    'has-popup', 'read-only', 'active', 'checkable', 'manages-descendants', 'collapsed',
                    'multiselectable', 'default', 'visited', 'animated', 'has-tooltip', 'iconified')
          if hasattr(Atspi.StateType, n.upper().replace(' ', '_').replace('-', '_'))]


_out_lock = threading.Lock()


def out(**k):
    line = json.dumps(k, default=str) + '\n'
    with _out_lock:
        sys.stdout.write(line)
        sys.stdout.flush()


# Liveness (the proxy's watchdog reads it): a thread beats every second with the age of the main loop's last
# iteration. No beat: the process is frozen (SIGSTOP, swapped out) or dead; beats with a growing age: the main loop is
# stuck in a call into an app. Either way the proxy restarts the reader (measured: a frozen reader
# left the mirrors stale and silent, and the AT actions queued meanwhile ran when it thawed).
_loop_tick = [time.monotonic()]


def _tick():
    _loop_tick[0] = time.monotonic()
    return True


def _beat():
    while True:
        time.sleep(1.0)
        out(t='beat', loop_ms=round((time.monotonic() - _loop_tick[0]) * 1000))


def err(*a):
    sys.stderr.write('reader: ' + ' '.join(str(x) for x in a) + '\n')
    sys.stderr.flush()


# X11_A11Y_PROXY_READER_TRACE=FILE: every AT-SPI event the reader receives, as it receives it (diagnostics)
_trace = open(os.environ['X11_A11Y_PROXY_READER_TRACE'], 'a', buffering=1) \
    if os.environ.get('X11_A11Y_PROXY_READER_TRACE') else None


def trace(*a):
    if _trace:
        t = time.time()
        _trace.write(time.strftime('%H:%M:%S', time.localtime(t)) + f'.{int(t * 1000) % 1000:03d} ' +
                     ' '.join(str(x) for x in a) + '\n')


class Reader:
    def __init__(self):
        self.ids = {}        # Atspi.Accessible -> int
        self.objs = {}       # int -> Atspi.Accessible
        self.window_of = {}  # node id -> window id
        self.windows = {}    # window id -> info dict
        self.pending = {}    # node id -> GLib source (debounced subtree re-dumps)
        self.deferred = {}   # (kind, node id) -> GLib source (reads after the queued events)
        self.last_focus = {} # window id -> the node that last got the focus there
        self.texts = {}      # node id -> text as last read or as the app's insert/delete events left it
        self.no_callback = set()   # windows whose app answers calls too late to make any in event handling
        self.sent = set()    # node ids the proxy has received in a tree (an id alone does not mean that: any event
                             # gives its source one)
        self.next_id = 1

    def nid(self, o):
        n = self.ids.get(o)
        if n is None:
            n = self.ids[o] = self.next_id
            self.objs[n] = o
            self.next_id += 1
        return n

    # ---- windows --------------------------------------------------------------------------------------------------
    def scan(self):
        seen = set()
        d = Atspi.get_desktop(0)
        for i in range(d.get_child_count()):
            try:
                app = d.get_child_at_index(i)
                if app is None:
                    continue
                for j in range(app.get_child_count()):
                    w = app.get_child_at_index(j)
                    if w is None:
                        continue
                    # every child of an application is one of its top-level windows (Qt reports a plain
                    # QWidget window as a filler; popup menus are windows too)
                    role = w.get_role_name()
                    wid = self.nid(w)
                    seen.add(wid)
                    if wid not in self.windows:
                        try:
                            e = w.get_extents(Atspi.CoordType.SCREEN)
                            sbox = [e.x, e.y, e.width, e.height]
                        except Exception:
                            sbox = None
                        info = {'id': wid, 'app': app.get_name(), 'pid': app.get_process_id(), 'name': w.get_name(),
                                'role': role, 'sbox': sbox,
                                'toolkit': f'{app.get_toolkit_name()} {app.get_toolkit_version()}'}
                        self.windows[wid] = info
                        self.window_of[wid] = wid
                        out(t='window', **info)
            except Exception as ex:
                err('scan', repr(ex))
        for wid in list(self.windows):
            if wid not in seen:
                # GTK 4 drops its window from the application's children for a moment while a popover comes or
                # goes (measured on GNOME 50: the entry's "opens popup"); reporting it gone made the proxy forget the
                # window and lose every focus event until the next scan.
                # Gone means gone: the object no longer answers, or says it is defunct.
                if self.alive(self.objs.get(wid)):
                    err('scan', f'window {wid} missing from its application but alive: kept')
                    continue
                del self.windows[wid]
                out(t='window-gone', id=wid)
        return True

    @staticmethod
    def alive(o):
        if o is None:
            return False
        try:
            o.get_extents(Atspi.CoordType.WINDOW)    # a live call (role and states may come from libatspi's cache)
            return not o.get_state_set().contains(Atspi.StateType.DEFUNCT)
        except Exception:
            return False

    # ---- nodes ----------------------------------------------------------------------------------------------------
    def info(self, o, parent, idx):
        n = self.nid(o)
        r = {'id': n, 'parent': parent, 'idx': idx}
        try:
            r['role'] = o.get_role_name()
            r['name'] = o.get_name() or ''
            r['desc'] = o.get_description() or ''
        except Exception as ex:
            r['role'] = 'unknown'; r['name'] = ''; r['desc'] = ''; r['error'] = repr(ex)
            return r
        try:
            ss = o.get_state_set()
            r['states'] = [nm for nm, st in STATES if ss.contains(st)]
        except Exception:
            r['states'] = []
        for ct, key in ((Atspi.CoordType.WINDOW, 'box'), (Atspi.CoordType.SCREEN, 'sbox')):
            try:
                e = o.get_extents(ct)
                r[key] = [e.x, e.y, e.width, e.height]
            except Exception:
                r[key] = None
        try:
            a = o.get_action_iface()
            if a:
                r['actions'] = [[a.get_action_name(i) or '', a.get_key_binding(i) or '',
                                 a.get_action_description(i) or ''] for i in range(a.get_n_actions())]
        except Exception:
            pass
        try:
            v = o.get_value_iface()
            if v:
                r['value'] = [Atspi.Value.get_current_value(o), Atspi.Value.get_minimum_value(o),
                              Atspi.Value.get_maximum_value(o), Atspi.Value.get_minimum_increment(o)]
                try:
                    vt = Atspi.Value.get_text(o)
                    if isinstance(vt, str) and vt:
                        r['vtext'] = vt
                except Exception:
                    pass
        except Exception:
            pass
        try:
            t = o.get_text_iface()
            if t and (r['role'] in TEXT_ROLES or 'editable' in r['states']):
                cnt = Atspi.Text.get_character_count(o)
                r['text'] = Atspi.Text.get_text(o, 0, min(cnt, 20000))
                self.texts[n] = r['text']
                r['caret'] = Atspi.Text.get_caret_offset(o)
                sels = []
                for k in range(Atspi.Text.get_n_selections(o)):
                    rg = Atspi.Text.get_selection(o, k)
                    sels.append([rg.start_offset, rg.end_offset])
                r['sel'] = sels
        except Exception:
            pass
        try:
            attrs = o.get_attributes() or {}
            keep = {k: v for k, v in attrs.items() if k in ('placeholder-text', 'level', 'keyshortcuts', 'toolkit',
                                                                'xml-roles', 'tag', 'roledescription', 'live',
                                                                'container-live', 'id', 'class')}
            if keep:
                r['attrs'] = keep
        except Exception:
            pass
        try:
            rels = {}
            for rel in o.get_relation_set() or []:
                ty = rel.get_relation_type().value_nick
                rels[ty] = [self.nid(rel.get_target(k)) for k in range(rel.get_n_targets())]
            if rels:
                r['rel'] = rels
        except Exception:
            pass
        if r['role'] in ('image', 'icon'):
            try:
                im = o.get_image_iface()
                if im:
                    r['image'] = Atspi.Image.get_image_description(o) or ''
            except Exception:
                pass
        return r

    def walk(self, root, parent=None, idx=0, window=None):
        nodes, stack = [], [(root, parent, idx)]
        while stack and len(nodes) < MAX_NODES:
            o, p, i = stack.pop(0)
            r = self.info(o, p, i)
            if window is not None:
                self.window_of[r['id']] = window
            nodes.append(r)
            st = r.get('states', [])
            if p is not None and 'showing' not in st and r['role'] not in WINDOW_ROLES and r['role'] != 'menu':
                continue   # hidden subtrees (other tab pages, closed menus) are not mirrored
            try:
                cc = o.get_child_count()
            except Exception:
                cc = 0
            if 'manages-descendants' in st:
                cc = min(cc, MAX_CHILDREN)
            kids = []
            for j in range(cc):
                try:
                    c = o.get_child_at_index(j)
                except Exception:
                    c = None
                if c is not None:
                    kids.append((c, r['id'], j))
            stack[0:0] = kids   # depth-first, document order
        return nodes

    def send_tree(self, wid, root=None):
        w = self.objs.get(wid)
        if w is None:
            return
        o = self.objs.get(root) if root else w
        if o is None:
            return
        parent = None
        idx = 0
        if root and root != wid:
            try:
                p = o.get_parent()
                parent = self.ids.get(p)
                idx = o.get_index_in_parent()
            except Exception:
                pass
        t0 = time.monotonic()
        nodes = self.walk(o, parent, idx, window=wid)
        self.sent.update(n['id'] for n in nodes)
        out(t='tree', window=wid, root=root or wid, nodes=nodes, ms=round((time.monotonic() - t0) * 1000, 1),
            truncated=len(nodes) >= MAX_NODES)
        if (root or wid) == wid and wid in self.last_focus:
            # the focus the app reported before the proxy had this window (start-up)
            out(t='ev', kind='focus', id=self.last_focus[wid], window=wid, replay=True)

    def window_for(self, o, depth=0):
        """The window id an accessible belongs to (walking up), or None."""
        n = self.ids.get(o)
        if n is not None and n in self.window_of:
            return self.window_of[n]
        if depth > 60:
            return None
        try:
            p = o.get_parent()
        except Exception:
            return None
        if p is None:
            return None
        try:
            if p.get_role_name() == 'application':
                return self.ids.get(o)
        except Exception:
            return None
        w = self.window_for(p, depth + 1)
        if w is not None and n is not None:
            self.window_of[n] = w
        return w

    def schedule_subtree(self, o):
        wid = self.window_for(o)
        if wid is None:
            self.scan()
            return
        n = self.nid(o)
        if n in self.pending:
            return

        def fire():
            self.pending.pop(n, None)
            self.send_tree(wid, n)
            return False
        self.pending[n] = GLib.timeout_add(120, fire)

    # ---- events ---------------------------------------------------------------------------------------------------
    def on_event(self, ev):
        if _trace:
            try:
                o = ev.source
                trace('ev', ev.type, ev.detail1, repr(ev.any_data)[:60], 'id', self.ids.get(o), o.get_role_name(),
                      repr(o.get_name())[:40])
            except Exception as ex:
                trace('ev', ev.type, repr(ex))
        try:
            self._on_event(ev)
        except Exception as ex:
            err('event', ev.type, repr(ex))

    def _on_event(self, ev):
        o = ev.source
        ty = ev.type
        if o is None:
            return
        if ty.startswith('window:create') or ty.startswith('window:destroy'):
            GLib.timeout_add(50, lambda: self.scan() and False)
            return
        if ty.startswith('object:children-changed'):
            try:
                role = o.get_role_name()
            except Exception:
                return
            if role in ('application', 'desktop frame'):
                GLib.timeout_add(50, lambda: self.scan() and False)
                return
            self.schedule_subtree(o)
            return
        wid = self.window_for(o)
        if wid is None:
            return
        n = self.nid(o)
        # Has the proxy got this node? Not "has it an id": any event gives its source one. Chromium replaces an
        # Electron app's document while it loads and the new document's first event is often "busy" off, before
        # its focus: with "has an id" as the test, the focus that followed was taken for a known node, its subtree
        # was never sent, and the mirrors missed the whole page until the person's first Tab (measured
        # with Electron at start-up).
        known = n in self.sent
        if ty.startswith('object:state-changed:'):
            state = ty.split(':')[2]
            if state == 'focused':
                if ev.detail1:
                    if not known:
                        # a node the proxy has not seen (a new popup's item, a new document): its subtree first
                        self.send_tree(wid, n)
                    self.last_focus[wid] = n
                    out(t='ev', kind='focus', id=n, window=wid)
                return
            if state == 'showing' and ev.detail1 and not known:
                self.schedule_subtree(o)
                return
            out(t='ev', kind='state', id=n, window=wid, state=state, on=bool(ev.detail1))
            if state in ('showing', 'visible'):
                self.schedule_subtree(o.get_parent() or o)
        elif ty.startswith('object:property-change:accessible-name'):
            # the new value travels with the event: no call back into the app here
            name = ev.any_data if isinstance(ev.any_data, str) else None
            out(t='ev', kind='name', id=n, window=wid, name=name if name is not None else (o.get_name() or ''))
        elif ty.startswith('object:property-change:accessible-description'):
            desc = ev.any_data if isinstance(ev.any_data, str) else None
            out(t='ev', kind='desc', id=n, window=wid, desc=desc if desc is not None else (o.get_description() or ''))
        elif ty.startswith('object:value-changed') or ty.startswith('object:property-change:accessible-value'):
            if isinstance(ev.any_data, (int, float)) and not isinstance(ev.any_data, bool):
                out(t='ev', kind='value', id=n, window=wid, value=float(ev.any_data), from_event=True)
            self.defer('value', o, n, wid)
        elif ty.startswith('object:text-changed'):
            # the event carries the change (offset, length, the text inserted or removed): apply it to what we
            # have, without calling back into the app (Chromium and Qt answer late while busy)
            old = self.texts.get(n)
            data = ev.any_data if isinstance(ev.any_data, str) else None
            if old is not None and data is not None and 0 <= ev.detail1 <= len(old):
                if ty.startswith('object:text-changed:insert'):
                    new = old[:ev.detail1] + data + old[ev.detail1:]
                    caret = ev.detail1 + len(data)
                else:
                    new = old[:ev.detail1] + old[ev.detail1 + len(data):]
                    caret = ev.detail1
                self.texts[n] = new
                out(t='ev', kind='text', id=n, window=wid, text=new, caret=caret, from_event=True)
            self.defer('text', o, n, wid)   # and confirm by reading, after the queued events
        elif ty.startswith('object:text-caret-moved'):
            out(t='ev', kind='caret', id=n, window=wid, caret=ev.detail1)
        elif ty.startswith('object:text-selection-changed'):
            self.defer('selection', o, n, wid)
        elif ty.startswith('object:announcement'):
            msg = ev.any_data if isinstance(ev.any_data, str) else str(ev.any_data)
            out(t='ev', kind='announce', id=n, window=wid, text=msg, politeness=ev.detail1)
        elif ty.startswith('object:selection-changed'):
            self.schedule_subtree(o)
        elif ty.startswith('window:activate'):
            out(t='ev', kind='active', id=n, window=wid, on=True)
        elif ty.startswith('window:deactivate'):
            out(t='ev', kind='active', id=n, window=wid, on=False)

    def defer(self, kind, o, n, wid):
        """Reads that need a call back into the app run after the events already queued: an app may answer calls
        only once it is idle again (Qt 6 answered after the next key, so a synchronous read here held back the focus
        events behind it)."""
        key = (kind, n)
        if key in self.deferred or wid in self.no_callback:
            return

        def run():
            self.deferred.pop(key, None)
            if wid in self.no_callback:
                return False
            t0 = time.monotonic()
            try:
                if kind == 'value':
                    vt = None
                    try:
                        vt = Atspi.Value.get_text(o)
                        vt = vt if isinstance(vt, str) and vt else None
                    except Exception:
                        pass
                    out(t='ev', kind='value', id=n, window=wid, value=Atspi.Value.get_current_value(o), vtext=vt)
                elif kind == 'text':
                    cnt = Atspi.Text.get_character_count(o)
                    txt = Atspi.Text.get_text(o, 0, min(cnt, 20000))
                    if txt != self.texts.get(n):
                        self.texts[n] = txt
                        out(t='ev', kind='text', id=n, window=wid, text=txt, caret=Atspi.Text.get_caret_offset(o))
                elif kind == 'selection':
                    sels = []
                    for k in range(Atspi.Text.get_n_selections(o)):
                        rg = Atspi.Text.get_selection(o, k)
                        sels.append([rg.start_offset, rg.end_offset])
                    out(t='ev', kind='selection', id=n, window=wid, sel=sels)
            except Exception as ex:
                err('deferred', kind, repr(ex))
            ms = (time.monotonic() - t0) * 1000
            if ms > 300:
                # this app answers calls only late (Chromium, while it waits for input): every call holds the
                # events queued behind it, so for this window rely on what the events carry
                self.no_callback.add(wid)
                err(f'window {wid}: a {kind} read took {ms:.0f} ms; using event data only from now on')
                out(t='note', window=wid, msg='slow-calls', ms=round(ms))
            return False
        self.deferred[key] = GLib.idle_add(run)

    # ---- commands -------------------------------------------------------------------------------------------------
    def command(self, c):
        do = c.get('do')
        o = self.objs.get(c.get('id')) if 'id' in c else None
        ok = False
        if do == 'tree':
            self.send_tree(c['window'], c.get('root'))
            ok = True
        elif do == 'windows':
            self.windows.clear()
            self.scan()
            ok = True
        elif do == 'quit':
            loop.quit()
            return
        elif o is None:
            ok = False
        elif do == 'action':
            a = o.get_action_iface()
            if a:
                idx = c.get('index', 0)
                if 'name' in c:
                    names = [a.get_action_name(i) for i in range(a.get_n_actions())]
                    idx = names.index(c['name']) if c['name'] in names else -1
                ok = idx >= 0 and a.do_action(idx)
        elif do == 'set_value':
            ok = Atspi.Value.set_current_value(o, float(c['value']))
        elif do == 'focus':
            ok = o.get_component_iface().grab_focus() if o.get_component_iface() else False
        elif do == 'set_caret':
            ok = Atspi.Text.set_caret_offset(o, int(c['offset']))
        elif do == 'set_selection':
            ok = Atspi.Text.set_selection(o, 0, int(c['start']), int(c['end'])) or \
                Atspi.Text.add_selection(o, int(c['start']), int(c['end']))
        elif do == 'insert_text':
            et = o.get_editable_text_iface()
            if et:
                pos = Atspi.Text.get_caret_offset(o)
                ok = Atspi.EditableText.insert_text(o, pos, c['text'], len(c['text'].encode()))
        out(t='done', req=c.get('req'), do=do, id=c.get('id'), ok=bool(ok))


def list_apps():
    """--list-apps: the applications on this process's accessibility bus (the desktop's, when the proxy runs it
    without AT_SPI_BUS_ADDRESS) as one JSON line: [{name, pid, toolkit, windows}]."""
    Atspi.init()
    try:
        Atspi.set_timeout(800, 2000)
    except Exception:
        pass
    apps = []
    d = Atspi.get_desktop(0)
    for i in range(d.get_child_count()):
        try:
            a = d.get_child_at_index(i)
            if a is None:
                continue
            wins = []
            for j in range(min(a.get_child_count(), 20)):
                w = a.get_child_at_index(j)
                wins.append(w.get_name() if w is not None else '')
            apps.append({'name': a.get_name(), 'pid': a.get_process_id(), 'windows': wins,
                         'toolkit': f'{a.get_toolkit_name()} {a.get_toolkit_version()}'})
        except Exception as ex:
            err('list-apps', repr(ex))
    print(json.dumps(apps))


def main():
    global loop
    if '--list-apps' in sys.argv:
        return list_apps()
    Atspi.init()
    try:
        # a call an app does not answer holds every event behind it (libatspi blocks): Chromium left one
        # unanswered until the next key, delaying a check box's "checked" by the 3 s it waited (parity_r5)
        Atspi.set_timeout(int(os.environ.get("X11_A11Y_PROXY_CALL_TIMEOUT_MS", "500")), 15000)
    except Exception:
        pass
    r = Reader()
    lst = Atspi.EventListener.new(r.on_event)
    for et in ('object:state-changed', 'object:property-change:accessible-name',
               'object:property-change:accessible-description', 'object:property-change:accessible-value',
               'object:value-changed', 'object:text-changed', 'object:text-caret-moved',
               'object:text-selection-changed', 'object:children-changed', 'object:announcement',
               'object:selection-changed', 'window:create', 'window:destroy', 'window:activate',
               'window:deactivate'):
        try:
            lst.register(et)
        except Exception as ex:
            err('register', et, repr(ex))

    def on_stdin(src, cond):
        line = sys.stdin.readline()
        if not line:
            loop.quit()
            return False
        try:
            r.command(json.loads(line))
        except Exception as ex:
            out(t='error', msg=repr(ex), line=line.strip()[:200])
        return True
    GLib.io_add_watch(sys.stdin, GLib.IO_IN | GLib.IO_HUP, on_stdin)
    out(t='hello', bus=os.environ.get('AT_SPI_BUS_ADDRESS'), pid=os.getpid())
    r.scan()
    GLib.timeout_add(1000, r.scan)
    # Wake the main loop now and then: a message libdbus read off the socket during a blocking call sits in its
    # queue until the loop iterates again, and with nothing else to wake it that was the next key press
    # (Chromium's "checked" after Alt+C arrived 3 s late, with the next Tab; parity_r5)
    GLib.timeout_add(40, _tick)
    threading.Thread(target=_beat, daemon=True).start()
    loop = GLib.MainLoop()
    loop.run()


loop = None
if __name__ == '__main__':
    main()
