"""The X side of the proxy: one connection to the engine display (the bridge's X server, e.g. :20).

- finds the top-level windows clients map there (and override-redirect popups), with title, class, pid,
  transient-for and WM_PROTOCOLS, and follows them (map/unmap/configure/property changes);
- reads their pixels (the agent redirects every window offscreen with Composite, so GetImage of a window reads its
  own contents) when DAMAGE says they changed;
- injects the person's keys and clicks with XTest. With the bridge's `-reverseinput` this goes through the
  "xwlrvnc-reverse" XInput2 master (made our ClientPointer, so our core XTest uses its XTest devices; the app's
  window gets its keyboard focus with XISetFocus): the agent never forwards that master's events to the desktop
  (no echo), and the display's own pointer/keyboard (VNC viewers) keep reaching no X window. Without it, keys go
  through the core keyboard (the bridge's per-client XTest attribution keeps them off the desktop while the app has
  the X focus) and clicks are refused (the agent's input shield would take them and they would echo).

python-xlib (pure Python, LGPL-2.1+) lacks a few XInput2 requests; they are defined here with its request
framework."""
import os
import struct
import time

from Xlib import X, Xatom, display as xdisplay, error as xerror
from Xlib.ext import xinput, xtest
from Xlib.protocol import rq, event as xevent

REVERSE_MASTER = 'xwlrvnc-reverse'


# ---- XInput2 requests python-xlib does not have -------------------------------------------------------------------
class _XISetClientPointer(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'), rq.Opcode(44), rq.RequestLength(),
                         rq.Card32('window'), rq.Card16('deviceid'), rq.Pad(2))


class _XISetFocus(rq.Request):
    _request = rq.Struct(rq.Card8('opcode'), rq.Opcode(49), rq.RequestLength(),
                         rq.Card32('focus'), rq.Card32('time'), rq.Card16('deviceid'), rq.Pad(2))


class _XIGetFocus(rq.ReplyRequest):
    _request = rq.Struct(rq.Card8('opcode'), rq.Opcode(50), rq.RequestLength(), rq.Card16('deviceid'), rq.Pad(2))
    _reply = rq.Struct(rq.ReplyCode(), rq.Pad(1), rq.Card16('sequence_number'), rq.ReplyLength(),
                       rq.Card32('focus'), rq.Pad(20))


class _XIQueryPointer(rq.ReplyRequest):
    _request = rq.Struct(rq.Card8('opcode'), rq.Opcode(40), rq.RequestLength(),
                         rq.Card32('window'), rq.Card16('deviceid'), rq.Pad(2))
    _reply = rq.Struct(rq.ReplyCode(), rq.Pad(1), rq.Card16('sequence_number'), rq.ReplyLength(),
                       rq.Card32('root'), rq.Card32('child'), rq.Int32('root_x'), rq.Int32('root_y'),
                       rq.Int32('win_x'), rq.Int32('win_y'), rq.Card8('same_screen'), rq.Pad(1),
                       rq.Card16('buttons_len'), rq.Card32('mods_base'), rq.Card32('mods_latched'),
                       rq.Card32('mods_locked'), rq.Card32('mods_effective'),
                       rq.Card8('group_base'), rq.Card8('group_latched'), rq.Card8('group_locked'),
                       rq.Card8('group_effective'))


# X modifier masks and the keycodes (evdev + 8, the keymap the bridge uploads is the compositor's) that set them
SHIFT, LOCK, CONTROL, MOD1, MOD2, MOD4 = X.ShiftMask, X.LockMask, X.ControlMask, X.Mod1Mask, X.Mod2Mask, X.Mod4Mask
KC_SHIFT_L, KC_CONTROL_L, KC_ALT_L, KC_SUPER_L, KC_CAPS, KC_NUM = 50, 37, 64, 133, 66, 77
MODIFIER_KEYCODES = {50: SHIFT, 62: SHIFT, 37: CONTROL, 105: CONTROL, 64: MOD1, 108: MOD1, 133: MOD4, 134: MOD4,
                     66: LOCK, 77: MOD2}


class TopLevel:
    """What the proxy needs to know about one X window."""
    __slots__ = ('id', 'title', 'wm_class', 'pid', 'transient_for', 'protocols', 'geometry', 'depth', 'mapped',
                 'override_redirect', 'damage', 'input_hint')

    def __init__(self, wid):
        self.id = wid
        self.title, self.wm_class, self.pid, self.transient_for = '', ('', ''), None, None
        self.protocols, self.geometry, self.depth, self.mapped = set(), (0, 0, 1, 1), 24, False
        self.override_redirect, self.damage, self.input_hint = False, None, True

    def __repr__(self):
        return f'<X 0x{self.id:x} {self.title!r} {self.geometry} or={self.override_redirect}>'


class Engine:
    """One connection to the engine display. Not thread-safe: use from the GLib main loop only."""

    def __init__(self, name, log=print):
        self.log = log
        self.d = xdisplay.Display(name)
        self.name = name
        self.screen = self.d.screen()
        self.root = self.screen.root
        self.atoms = {}
        self.toplevels = {}       # id -> TopLevel (InputOutput, not override-redirect, mapped or not)
        self.popups = {}          # id -> TopLevel (override-redirect, mapped)
        self.listeners = []       # callables(kind, TopLevel)
        self.damage_cb = None     # callable(wid)
        self.held_keys = set()    # keycodes we hold down on the engine
        self.held_buttons = set()
        self.xi_opcode = None
        self.reverse = None       # (pointer id, keyboard id) of the bridge's reverse master
        self.has_damage = self.d.has_extension('DAMAGE')
        self._xi_init()
        if self.has_damage:
            self.d.damage_query_version()
        self.root.change_attributes(event_mask=X.SubstructureNotifyMask | X.PropertyChangeMask)
        self._scan()

    # ---- atoms / properties --------------------------------------------------------------------------------------
    def atom(self, name):
        a = self.atoms.get(name)
        if a is None:
            a = self.atoms[name] = self.d.intern_atom(name)
        return a

    def get_prop(self, win, name, type_=X.AnyPropertyType):
        try:
            p = win.get_full_property(self.atom(name), type_)
        except xerror.XError:
            return None
        return p.value if p else None

    def root_string(self, name):
        v = self.get_prop(self.root, name)
        if v is None:
            return None
        return v.decode(errors='replace') if isinstance(v, bytes) else str(v)

    # ---- XInput2 / reverse master ------------------------------------------------------------------------------
    def _xi_init(self):
        if not self.d.has_extension('XInputExtension'):
            self.log('xengine: no XInput extension; core input only')
            return
        self.xi_opcode = self.d.display.get_extension_major('XInputExtension')
        xinput.XIQueryVersion(display=self.d.display, opcode=self.xi_opcode, major_version=2, minor_version=2)
        self.find_reverse()

    def find_reverse(self):
        """The bridge's reverse master (-reverseinput), made our ClientPointer so our XTest goes through it."""
        self.reverse = None
        if self.xi_opcode is None:
            return None
        ptr = kbd = None
        for dev in self.d.xinput_query_device(xinput.AllDevices).devices:
            name = dev.name.decode() if isinstance(dev.name, bytes) else dev.name
            if name == REVERSE_MASTER + ' pointer' and dev.use == xinput.MasterPointer:
                ptr = dev.deviceid
            elif name == REVERSE_MASTER + ' keyboard' and dev.use == xinput.MasterKeyboard:
                kbd = dev.deviceid
        if ptr and kbd:
            self.reverse = (ptr, kbd)
            _XISetClientPointer(display=self.d.display, opcode=self.xi_opcode, window=0, deviceid=ptr)
            self.d.sync()
            self.log(f'xengine: reverse master found (pointer {ptr}, keyboard {kbd}); input goes through it')
        else:
            self.log('xengine: no reverse master (bridge without -reverseinput): keys through the core keyboard, '
                     'clicks disabled')
        return self.reverse

    def set_client_pointer(self, wid):
        """Makes the reverse master the ClientPointer of the client owning `wid`, so its core focus changes, grabs and
        pointer queries refer to the device the proxy drives."""
        if self.reverse:
            try:
                _XISetClientPointer(display=self.d.display, opcode=self.xi_opcode, window=wid, deviceid=self.reverse[0])
            except xerror.XError as ex:
                self.log('xengine: XISetClientPointer', ex)

    # ---- window discovery -----------------------------------------------------------------------------------------
    def _scan(self):
        for w in self.root.query_tree().children:
            self._consider(w.id, initial=True)

    def _consider(self, wid, initial=False):
        w = self.d.create_resource_object('window', wid)
        try:
            a = w.get_attributes()
            g = w.get_geometry()
        except xerror.XError:
            return None
        if a.win_class != X.InputOutput:
            return None
        t = TopLevel(wid)
        t.override_redirect = bool(a.override_redirect)
        t.mapped = a.map_state == X.IsViewable
        t.depth = g.depth
        t.geometry = (g.x, g.y, g.width, g.height)
        if t.override_redirect:
            if t.mapped:
                self.popups[wid] = t
                self._watch_damage(t)
                self._emit('popup-map', t)
            return t
        try:
            w.change_attributes(event_mask=X.PropertyChangeMask | X.StructureNotifyMask)
        except xerror.XError:
            return None
        self._read_props(t)
        self.toplevels[wid] = t
        if t.mapped:
            self._watch_damage(t)
            self._emit('map', t)
        return t

    def _read_props(self, t):
        w = self.d.create_resource_object('window', t.id)
        name = self.get_prop(w, '_NET_WM_NAME', self.atom('UTF8_STRING'))
        if name is None:
            name = self.get_prop(w, 'WM_NAME')
        t.title = name.decode('utf-8', errors='replace') if isinstance(name, bytes) else (name or '')
        cls = self.get_prop(w, 'WM_CLASS')
        if isinstance(cls, bytes):
            parts = cls.split(b'\0')
            t.wm_class = (parts[0].decode(errors='replace'), parts[1].decode(errors='replace') if len(parts) > 1 else '')
        pid = self.get_prop(w, '_NET_WM_PID')
        t.pid = int(pid[0]) if pid is not None and len(pid) else None
        tf = self.get_prop(w, 'WM_TRANSIENT_FOR')
        t.transient_for = int(tf[0]) if tf is not None and len(tf) else None
        prots = self.get_prop(w, 'WM_PROTOCOLS')
        t.protocols = set()
        for a in (prots or []):
            try:
                t.protocols.add(self.d.get_atom_name(a))
            except xerror.XError:
                pass
        hints = self.get_prop(w, 'WM_HINTS')
        # WM_HINTS: flags, input, ...; InputHint = 1
        t.input_hint = not (hints is not None and len(hints) > 1 and hints[0] & 1 and not hints[1])

    def _watch_damage(self, t):
        if not self.has_damage or t.damage is not None:
            return
        try:
            w = self.d.create_resource_object('window', t.id)
            t.damage = w.damage_create(1)   # DamageReportRawRectangles=0, DeltaRectangles=1, BoundingBox=2, NonEmpty=3
        except xerror.XError:
            t.damage = None

    def _unwatch_damage(self, t):
        if t.damage is not None:
            try:
                self.d.damage_destroy(t.damage)
            except xerror.XError:
                pass
            t.damage = None

    def _emit(self, kind, t):
        for cb in list(self.listeners):
            try:
                cb(kind, t)
            except Exception as ex:   # a listener's bug must not stop event handling
                self.log('xengine: listener error', kind, t, repr(ex))

    def frame_origin(self, wid):
        """Where window `wid` is on the engine's root (absolute)."""
        w = self.d.create_resource_object('window', wid)
        t = w.translate_coords(self.root, 0, 0)
        return -t.x, -t.y

    # ---- event pump (call when the X fd is readable) -------------------------------------------------------------
    def pump(self):
        n = 0
        while self.d.pending_events():
            ev = self.d.next_event()
            n += 1
            try:
                self._handle(ev)
            except xerror.XError as ex:
                self.log('xengine: X error while handling', ev.__class__.__name__, ex)
        return n

    def _handle(self, ev):
        t = ev.type
        if self.has_damage and ev.__class__.__name__ == 'DamageNotify':
            if self.damage_cb:
                self.damage_cb(ev.drawable.id if hasattr(ev.drawable, 'id') else ev.drawable)
            return
        if t == X.CreateNotify:
            return
        if t == X.MapNotify:
            wid = ev.window.id
            if ev.event.id != self.root.id:
                return
            tl = self.toplevels.get(wid)
            if tl is None:
                self._consider(wid)
            else:
                tl.mapped = True
                self._read_props(tl)
                try:
                    g = ev.window.get_geometry()
                    tl.geometry = (g.x, g.y, g.width, g.height)
                except xerror.XError:
                    pass
                self._watch_damage(tl)
                self._emit('map', tl)
        elif t == X.UnmapNotify:
            if ev.event.id != self.root.id:
                return
            wid = ev.window.id
            if wid in self.popups:
                p = self.popups.pop(wid)
                self._unwatch_damage(p)
                self._emit('popup-unmap', p)
            elif wid in self.toplevels:
                tl = self.toplevels[wid]
                tl.mapped = False
                self._unwatch_damage(tl)
                self._emit('unmap', tl)
        elif t == X.DestroyNotify:
            wid = ev.window.id
            for table, kind in ((self.popups, 'popup-unmap'), (self.toplevels, 'destroy')):
                if wid in table:
                    tl = table.pop(wid)
                    tl.damage = None   # gone with the window
                    self._emit(kind, tl)
        elif t == X.ConfigureNotify:
            wid = ev.window.id
            tl = self.toplevels.get(wid) or self.popups.get(wid)
            if tl is not None and ev.event.id in (self.root.id, wid):
                new = (ev.x, ev.y, ev.width, ev.height)
                if new != tl.geometry:
                    tl.geometry = new
                    self._emit('configure', tl)
        elif t == X.PropertyNotify:
            if ev.window.id == self.root.id:
                self._emit('root-property', ev)
                return
            tl = self.toplevels.get(ev.window.id)
            if tl is not None:
                name = self.d.get_atom_name(ev.atom)
                if name in ('WM_NAME', '_NET_WM_NAME', 'WM_TRANSIENT_FOR', 'WM_PROTOCOLS', 'WM_CLASS', '_NET_WM_PID'):
                    old = tl.title
                    self._read_props(tl)
                    if tl.title != old:
                        self._emit('title', tl)
        elif t == X.ReparentNotify:
            # a window reparented away from the root is no longer a toplevel (and one to the root becomes one)
            wid = ev.window.id
            if ev.parent.id != self.root.id and wid in self.toplevels:
                tl = self.toplevels.pop(wid)
                self._emit('destroy', tl)
            elif ev.parent.id == self.root.id and wid not in self.toplevels:
                self._consider(wid)

    # ---- pixels ---------------------------------------------------------------------------------------------------
    def read_pixels(self, t):
        """(width, height, depth, bytes BGRX/BGRA) of window t, or None. Clears its damage first."""
        w = self.d.create_resource_object('window', t.id)
        if t.damage is not None:
            try:
                self.d.damage_subtract(t.damage, 0, 0)
            except xerror.XError:
                pass
        try:
            g = w.get_geometry()
            img = w.get_image(0, 0, g.width, g.height, X.ZPixmap, 0xffffffff)
        except xerror.XError:
            return None
        return g.width, g.height, g.depth, img.data

    def cursor_image(self):
        """The cursor the client pointer (the reverse master, if any) shows now: (w, h, xhot, yhot, argb bytes)."""
        if not self.d.has_extension('XFIXES'):
            return None
        try:
            self.d.xfixes_query_version()
            c = self.d.xfixes_get_cursor_image(self.root)
        except Exception:
            return None
        data = struct.pack('=%dI' % len(c.cursor_image), *c.cursor_image)
        return c.width, c.height, c.xhot, c.yhot, data

    # ---- input ----------------------------------------------------------------------------------------------------
    def focus(self, t):
        """Gives app window t the keyboard focus of the device we type with (the reverse keyboard, or the core one),
        the way a window manager would: WM_TAKE_FOCUS if it asks for it, XSetInputFocus if it accepts input, and
        _NET_ACTIVE_WINDOW on the root."""
        w = self.d.create_resource_object('window', t.id)
        self.set_client_pointer(t.id)
        now = self._server_time()
        try:
            if t.input_hint or 'WM_TAKE_FOCUS' not in t.protocols:
                if self.reverse:
                    _XISetFocus(display=self.d.display, opcode=self.xi_opcode, focus=t.id, time=X.CurrentTime,
                                deviceid=self.reverse[1])
                else:
                    w.set_input_focus(X.RevertToParent, X.CurrentTime)
            if 'WM_TAKE_FOCUS' in t.protocols:
                self._wm_protocol(w, 'WM_TAKE_FOCUS', now)
            self.root.change_property(self.atom('_NET_ACTIVE_WINDOW'), Xatom.WINDOW, 32, [t.id])
            self.d.sync()
        except xerror.XError as ex:
            self.log('xengine: focus', t, ex)

    def unfocus(self):
        """No app window has our keyboard any more (only matters without the reverse master: the core keyboard also
        carries the display's own clients' typing)."""
        if not self.reverse:
            try:
                self.d.set_input_focus(X.PointerRoot, X.RevertToPointerRoot, X.CurrentTime)
                self.d.sync()
            except xerror.XError:
                pass

    def focused_window(self):
        try:
            if self.reverse:
                return _XIGetFocus(display=self.d.display, opcode=self.xi_opcode, deviceid=self.reverse[1]).focus
            return self.d.get_input_focus().focus.id
        except (xerror.XError, AttributeError):
            return None

    def close(self, t):
        w = self.d.create_resource_object('window', t.id)
        if 'WM_DELETE_WINDOW' in t.protocols:
            self._wm_protocol(w, 'WM_DELETE_WINDOW', self._server_time())
        else:
            w.kill_client()
        self.d.sync()

    def _wm_protocol(self, w, name, when):
        ev = xevent.ClientMessage(window=w, client_type=self.atom('WM_PROTOCOLS'),
                                  data=(32, [self.atom(name), when, 0, 0, 0]))
        w.send_event(ev, event_mask=0)

    def _server_time(self):
        """A server timestamp (WM_TAKE_FOCUS and selection owners want a real one): a zero-length property append
        on our own window and its PropertyNotify."""
        if not hasattr(self, '_time_win'):
            self._time_win = self.root.create_window(-10, -10, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent,
                                                     event_mask=X.PropertyChangeMask, override_redirect=1)
        self._time_win.change_property(self.atom('_X11_A11Y_PROXY_TIME'), Xatom.STRING, 8, b'', X.PropModeAppend)
        self.d.flush()
        deadline = time.monotonic() + 0.5
        while time.monotonic() < deadline:
            ev = self.d.next_event() if self.d.pending_events() else None
            if ev is None:
                time.sleep(0.002)
                self.d.sync()
                continue
            if ev.type == X.PropertyNotify and ev.window.id == self._time_win.id:
                return ev.time
            self._handle(ev)
        return X.CurrentTime

    def modifier_state(self):
        """(mods_effective, mods_locked) of the keyboard we type with (the reverse one, or the core one)."""
        try:
            if self.reverse:
                r = _XIQueryPointer(display=self.d.display, opcode=self.xi_opcode, window=self.root.id,
                                    deviceid=self.reverse[0])
                return r.mods_effective, r.mods_locked
            q = self.root.query_pointer()
            return q.mask & 0xff, q.mask & (LOCK | MOD2)
        except xerror.XError:
            return 0, 0

    def key(self, keycode, press):
        xtest.fake_input(self.d, X.KeyPress if press else X.KeyRelease, keycode)
        (self.held_keys.add if press else self.held_keys.discard)(keycode)

    def can_click(self):
        return self.reverse is not None

    def motion(self, x, y):
        xtest.fake_input(self.d, X.MotionNotify, x=int(x), y=int(y))

    def button(self, n, press):
        xtest.fake_input(self.d, X.ButtonPress if press else X.ButtonRelease, n)
        (self.held_buttons.add if press else self.held_buttons.discard)(n)

    def click_at(self, x, y, n=1):
        self.motion(x, y)
        self.button(n, True)
        self.button(n, False)
        self.d.sync()

    def raise_window(self, t):
        """Puts window t above the others on the engine (the reverse pointer only reaches the topmost window at a
        point; nobody sees the engine's stacking)."""
        try:
            self.d.create_resource_object('window', t.id).configure(stack_mode=X.Above)
        except xerror.XError:
            pass

    def release_all(self):
        for kc in list(self.held_keys):
            self.key(kc, False)
        for b in list(self.held_buttons):
            self.button(b, False)
        try:
            self.d.sync()
        except Exception:
            pass

    def flush(self):
        self.d.flush()

    def fileno(self):
        return self.d.fileno()
