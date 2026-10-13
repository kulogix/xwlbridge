"""Tk introspection through Tk's own `send` protocol (tkUnixSend.c), for Tk 8.6 apps (tkinter, Tcl/Tk tools) that
have no accessibility support: the widget tree with classes, texts, states, variable values, entry/scale contents
and the keyboard focus, read from the interpreter itself instead of from pixels.

Protocol: interpreters register as "<comm window hex> <name>" lines in the root's "InterpRegistry" property. A
command is appended to the target's comm window "Comm" property as "\\0c\\0-n NAME\\0-r OURWIN SERIAL\\0-s SCRIPT\\0";
the result comes back appended to our window's "Comm" property as "\\0r\\0-s SERIAL\\0-r RESULT\\0" (+ "-c CODE").
Tk only accepts commands when the X server's access control is on and lists nothing but server-interpreted
localuser entries (or nothing) — exactly how the bridge's engine is set up (xhost +si:localuser:$USER).

The scripts sent only read state (and, for actions a screen reader asks for, invoke/set exactly as a click or a
key would); they run inside the app with its privileges, which are the person's own."""
import time

from Xlib import X, Xatom, error as xerror

FS, RS = '\x1f', '\x1e'

# one record per widget: path, class, X id, root x/y, width, height, mapped, then option=value pairs
WALK = r'''apply {{} {
  set q [list .]; set out {}
  while {[llength $q]} {
    set w [lindex $q 0]; set q [lrange $q 1 end]
    foreach c [winfo children $w] { lappend q $c }
    set rec [list $w [winfo class $w] [winfo id $w] [winfo rootx $w] [winfo rooty $w] [winfo width $w] \
      [winfo height $w] [winfo ismapped $w] [winfo toplevel $w]]
    foreach o {-text -state -underline -variable -textvariable -onvalue -offvalue -value -from -to -orient -label \
               -show -takefocus -image} {
      if {![catch {$w cget $o} v]} { lappend rec "$o=$v" }
    }
    if {![catch {$w cget -variable} vn] && $vn ne ""} { if {![catch {set ::$vn} vv]} { lappend rec "varvalue=$vv" } }
    switch -- [winfo class $w] {
      Entry - TEntry - Spinbox - TSpinbox - TCombobox { catch { lappend rec "value=[$w get]" } ; catch { lappend rec "cursor=[$w index insert]" } }
      Scale - TScale { catch { lappend rec "value=[$w get]" } }
      Text { catch { lappend rec "value=[$w get 1.0 end-1c]" } }
      Listbox { catch { lappend rec "value=[join [$w get 0 end] {, }]" } }
    }
    lappend out [join $rec "\x1f"]
  }
  return "[focus]\x1e[join $out "\x1e"]"
}}'''


class TkSend:
    def __init__(self, d, log=print, timeout=1.0):
        self.d, self.log, self.timeout = d, log, timeout
        self.root = d.screen().root
        self.comm = d.intern_atom('Comm')
        self.registry = d.intern_atom('InterpRegistry')
        self.win = self.root.create_window(-10, -10, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent,
                                           event_mask=X.PropertyChangeMask, override_redirect=1)
        self.serial = 0

    def interps(self):
        """[(comm window id, interp name)] registered on this display."""
        try:
            p = self.root.get_full_property(self.registry, X.AnyPropertyType)
        except xerror.XError:
            return []
        if not p:
            return []
        raw = p.value if isinstance(p.value, bytes) else bytes(p.value)
        out = []
        for entry in raw.split(b'\0'):
            entry = entry.strip()
            if b' ' not in entry:
                continue
            wid, name = entry.split(b' ', 1)
            try:
                out.append((int(wid, 16), name.decode(errors='replace')))
            except ValueError:
                pass
        return out

    def send(self, comm_window, name, script):
        """Runs `script` in interpreter `name`; returns (code, result) or None."""
        self.serial += 1
        req = (f'\0c\0-n {name}\0-r {self.win.id:x} {self.serial}\0-s {script}\0').encode()
        target = self.d.create_resource_object('window', comm_window)
        try:
            target.change_property(self.comm, Xatom.STRING, 8, req, X.PropModeAppend)
            self.d.flush()
        except xerror.XError:
            return None
        end = time.monotonic() + self.timeout
        while time.monotonic() < end:
            if not self.d.pending_events():
                time.sleep(0.002)
                self.d.sync()
                continue
            ev = self.d.next_event()
            if ev.type != X.PropertyNotify or ev.window.id != self.win.id or ev.atom != self.comm or \
                    ev.state != X.PropertyNewValue:
                continue
            p = self.win.get_property(self.comm, X.AnyPropertyType, 0, 1 << 22, True)
            if not p:
                continue
            raw = p.value if isinstance(p.value, bytes) else bytes(p.value)
            for msg in raw.split(b'\0r\0')[1:]:
                fields = {}
                for f in msg.split(b'\0'):
                    if f.startswith(b'-') and b' ' in f:
                        k, v = f.split(b' ', 1)
                        fields[k.decode()] = v.decode(errors='replace')
                    elif f.startswith(b'-'):
                        fields[f.decode()] = ''
                if fields.get('-s') == str(self.serial):
                    return int(fields.get('-c', '0') or 0), fields.get('-r', '')
        return None

    def interp_for(self, toplevel_id, related=()):
        """The interpreter owning X window `toplevel_id` (a Tk toplevel's wrapper window; Tk reports its toplevels'
        own windows, which are children of it: pass those as `related`)."""
        script = ('set x {}; foreach t [concat . [winfo children .]] { if {[winfo toplevel $t] eq $t} '
                  '{ lappend x [wm frame $t] [winfo id $t] } }; set x')
        for comm, name in self.interps():
            r = self.send(comm, name, script)
            if r and r[0] == 0:
                ids = set()
                for tok in r[1].split():
                    try:
                        ids.add(int(tok, 16))
                    except ValueError:
                        pass
                if toplevel_id in ids or ids & set(related):
                    return comm, name
        return None

    def bound_alt_keys(self, comm, name):
        """Letters the app binds with Alt (all / toplevel bindings): Tk's -underline is only drawn, so a mnemonic is
        announced only when something actually handles it."""
        r = self.send(comm, name, 'concat [bind all] [bind .] [bind Button] [bind Checkbutton]')
        out = set()
        if r and r[0] == 0:
            import re
            for m in re.finditer(r'<(?:Alt|Meta)-(?:Key-)?([A-Za-z0-9])>', r[1]):
                out.add(m.group(1).lower())
        return out

    def walk(self, comm, name):
        """(focus path, [widget dicts]) or None."""
        r = self.send(comm, name, WALK)
        if not r or r[0] != 0:
            return None
        focus, _, body = r[1].partition(RS)
        widgets = []
        for rec in body.split(RS):
            f = rec.split(FS)
            if len(f) < 9:
                continue
            w = {'path': f[0], 'class': f[1], 'id': int(f[2], 16) if f[2].startswith('0x') else int(f[2] or 0, 0),
                 'rx': int(f[3]), 'ry': int(f[4]), 'w': int(f[5]), 'h': int(f[6]), 'mapped': f[7] == '1',
                 'top': f[8]}
            for kv in f[9:]:
                k, _, v = kv.partition('=')
                w[k] = v
            widgets.append(w)
        return focus, widgets

    def focus(self, comm, name):
        r = self.send(comm, name, 'focus')
        return r[1] if r and r[0] == 0 else None

    def invoke(self, comm, name, path):
        return self.send(comm, name, f'{path} invoke')

    def close(self):
        try:
            self.win.destroy()
            self.d.flush()
        except xerror.XError:
            pass
