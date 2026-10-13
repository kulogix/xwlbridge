"""A client of the Xt Editres protocol (libXmu EditresCom.c; Motif carries its own copy): Athena (xcalc, xterm's
menus), Motif and other Xt apps answer it without any accessibility support, giving every widget's name, class and
X window, and resource values such as a button's label. That is real structure, not pixels, for the apps AT-SPI does
not cover.

Flow (one request at a time): we own a selection (an atom of our own) and send the app's shell window an "Editres"
ClientMessage (format 32: time, that atom, request ident, protocol version 5). The app asks us to convert the
selection to the target "EditresCommand" and gets the command: ident (8), command (8), length (32), then the
command's data, MSB first, as type "EditresProtocol", format 8. It runs the command, then takes the selection
over; when we lose it (SelectionClear) we convert it to "EditresClientVal" and read the reply: ident (8), result
(8: 0 formatted, 1 unformatted message, 2 protocol mismatch), length (32), data.

SendWidgetTree reply: count (16), per widget: path [count (16), ids (32) ...], name (String8 = len16 + bytes),
class (String8), window (32: 0 unrealized, 2 non-widget object); then the toolkit name (String8).
GetValues request: resource name (String8), count (16), widget paths; reply: count (16), value (String8).
Widget ids travel as 32 bits: on 64-bit clients whose libXmu still truncates them GetValues cannot find the widget
(the reply says so); the tree is then used without values."""
import struct
import time

from Xlib import X, Xatom, error as xerror
from Xlib.protocol import event as xevent

PROTOCOL_VERSION = 5
SEND_WIDGET_TREE, SET_VALUES, GET_RESOURCES, GET_GEOMETRY, FIND_CHILD, GET_VALUES = range(6)


class Stream:
    def __init__(self, data):
        self.b, self.i = data, 0

    def u8(self):
        v = self.b[self.i]; self.i += 1; return v

    def u16(self):
        v = struct.unpack('>H', self.b[self.i:self.i + 2])[0]; self.i += 2; return v

    def u32(self):
        v = struct.unpack('>I', self.b[self.i:self.i + 4])[0]; self.i += 4; return v

    def s8(self):
        n = self.u16(); v = self.b[self.i:self.i + n]; self.i += n
        return v.decode('latin-1')

    def path(self):
        return [self.u32() for _ in range(self.u16())]

    def left(self):
        return len(self.b) - self.i


def put_s8(s):
    b = s.encode('latin-1', errors='replace')
    return struct.pack('>H', len(b)) + b


def put_path(ids):
    return struct.pack('>H', len(ids)) + b''.join(struct.pack('>I', i & 0xffffffff) for i in ids)


class Editres:
    """Synchronous editres requests over a python-xlib Display (its own connection: we read events here)."""

    def __init__(self, d, log=print, timeout=1.5):
        self.d, self.log, self.timeout = d, log, timeout
        self.root = d.screen().root
        self.win = self.root.create_window(-10, -10, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent,
                                           event_mask=X.PropertyChangeMask, override_redirect=1)
        self.a_editres = d.intern_atom('Editres')
        self.a_comm = d.intern_atom('EditresComm_x11a11yproxy')
        self.a_command = d.intern_atom('EditresCommand')
        self.a_protocol = d.intern_atom('EditresProtocol')
        self.a_client_value = d.intern_atom('EditresClientVal')
        self.a_prop = d.intern_atom('_X11_A11Y_PROXY_EDITRES')
        self.ident = 0

    def _time(self):
        self.win.change_property(self.a_prop, Xatom.STRING, 8, b'', X.PropModeAppend)
        self.d.flush()
        end = time.monotonic() + 1
        while time.monotonic() < end:
            if self.d.pending_events():
                ev = self.d.next_event()
                if ev.type == X.PropertyNotify and ev.window.id == self.win.id:
                    return ev.time
            else:
                time.sleep(0.002)
                self.d.sync()
        return X.CurrentTime

    def request(self, shell, command, data=b''):
        """Sends one command to the Xt app whose shell window is `shell`; returns (result code, reply bytes) or None."""
        self.ident = (self.ident + 1) & 0xff
        ident = self.ident
        payload = struct.pack('>BBI', ident, command, len(data)) + data
        t = self._time()
        self.win.set_selection_owner(self.a_comm, t)
        w = self.d.create_resource_object('window', shell)
        ev = xevent.ClientMessage(window=w, client_type=self.a_editres,
                                  data=(32, [t, self.a_comm, ident, PROTOCOL_VERSION, 0]))
        try:
            w.send_event(ev, event_mask=0)
            self.d.flush()
        except xerror.XError:
            return None
        end = time.monotonic() + self.timeout
        asked = False
        while time.monotonic() < end:
            if not self.d.pending_events():
                time.sleep(0.003)
                self.d.sync()
                continue
            e = self.d.next_event()
            if e.type == X.SelectionRequest and e.selection == self.a_comm:
                # the app fetches the command
                prop = e.property or e.target
                ok = e.target in (self.a_command, self.a_comm)
                if ok:
                    e.requestor.change_property(prop, self.a_protocol, 8, payload)
                reply = xevent.SelectionNotify(time=e.time, requestor=e.requestor, selection=e.selection,
                                               target=e.target, property=prop if ok else X.NONE)
                e.requestor.send_event(reply, event_mask=0)
                self.d.flush()
            elif e.type == X.SelectionClear and e.atom == self.a_comm and not asked:
                # the app ran the command and now owns the selection: fetch its reply
                self.win.convert_selection(self.a_comm, self.a_client_value, self.a_prop, e.time)
                self.d.flush()
                asked = True
            elif e.type == X.SelectionNotify and e.selection == self.a_comm and asked:
                if e.property == X.NONE:
                    return None
                p = self.win.get_full_property(self.a_prop, X.AnyPropertyType)
                self.win.delete_property(self.a_prop)
                if p is None:
                    return None
                raw = p.value if isinstance(p.value, bytes) else bytes(p.value)
                if len(raw) < 6 or raw[0] != ident:
                    return None
                code = raw[1]
                size = struct.unpack('>I', raw[2:6])[0]
                return code, raw[6:6 + size]
        return None

    def widget_tree(self, shell):
        """[{path, id, name, class, window}], toolkit — or None if the app does not answer editres."""
        r = self.request(shell, SEND_WIDGET_TREE)
        if r is None or r[0] != 0:
            return None
        s = Stream(r[1])
        out = []
        try:
            for _ in range(s.u16()):
                path = s.path()
                name = s.s8()
                cls = s.s8()
                win = s.u32()
                out.append({'path': path, 'id': path[-1] if path else 0, 'name': name, 'class': cls, 'window': win})
            toolkit = s.s8() if s.left() >= 2 else ''
        except (IndexError, struct.error):
            return None
        return out, toolkit

    def get_value(self, shell, path, resource):
        """A resource's value as a string, or None (64-bit ids truncated: the app may not find the widget)."""
        data = put_s8(resource) + struct.pack('>H', 1) + put_path(path)
        r = self.request(shell, GET_VALUES, data)
        if r is None:
            return None
        code, payload = r
        s = Stream(payload)
        try:
            if code == 0:
                n = s.u16()
                if n < 1:
                    return None
                # libXmu: one String8 per widget; a value it could not convert comes back as an error message
                return s.s8()
            return None
        except (IndexError, struct.error):
            return None

    def close(self):
        try:
            self.win.destroy()
            self.d.flush()
        except xerror.XError:
            pass
