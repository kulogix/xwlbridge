"""One GNOME window per X top-level window of the engine display: its live pixels, the accessible mirrors of its
controls on top, and the person's keyboard and pointer forwarded to it."""
import time

import gi
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')
gi.require_version('Gsk', '4.0')
from gi.repository import Gdk, GLib, Graphene, Gsk, Gtk  # noqa: E402

from . import mirror as M  # noqa: E402
from .xengine import MODIFIER_KEYCODES, SHIFT, CONTROL, MOD1, MOD4, LOCK, MOD2, KC_CAPS, KC_NUM  # noqa: E402

FPS_CAP = 30
FOLLOW_MS = 120   # see node_event
REFOCUS_AGAIN_MS = 700   # see _refocus
STATE_FOCUS_MS = 150     # see focus_from_state
# GDK modifier -> (X mask, keycode pressed to get it)
MODSYNC = [(Gdk.ModifierType.SHIFT_MASK, SHIFT, 50), (Gdk.ModifierType.CONTROL_MASK, CONTROL, 37),
           (Gdk.ModifierType.ALT_MASK, MOD1, 64), (Gdk.ModifierType.SUPER_MASK, MOD4, 133)]


class ProxyWindow:
    def __init__(self, proxy, top):
        self.proxy, self.top = proxy, top
        self.engine = proxy.engine
        self.at_window = None       # the reader's window id (API mode)
        self.inferred = False
        self.source = None          # 'atspi', or for inferred mode 'tk' | 'editres' | 'xtree'
        self.local_nav = False      # Tab/Space handled here (the app has no focus chain we can follow)
        self.mirrors = {}           # node id -> mirror
        self.nodes = {}             # node id -> node
        self.popup_roots = {}       # reader popup window id -> mirror
        self.syncing_focus = False
        self.held = []              # app changes held until GTK has reported a focus (see node_event)
        self.toolkit = ''           # the app's toolkit as AT-SPI reports it, lower case
        self.dirty = True
        self.last_frame = 0.0
        self.refresh_src = None
        self.texture = None
        self.last_motion = None
        self.cursor_serial = None
        self.order = []             # inferred mode: Tab order
        self.live = {}              # node id -> callable re-reading a live value (inferred mode)
        self.created = time.monotonic()

        self.win = Gtk.ApplicationWindow(application=proxy, title=proxy.title_prefix + (top.title or top.wm_class[1]))
        self.win.set_default_size(max(1, top.geometry[2]), max(1, top.geometry[3]))
        self.win.set_resizable(True)
        ov = Gtk.Overlay()
        self.win.set_child(ov)
        # the pixels are decoration for assistive technologies: the mirrors carry the content
        self.pic = Gtk.Picture(accessible_role=Gtk.AccessibleRole.PRESENTATION)
        self.pic.set_can_shrink(True)
        self.pic.set_content_fit(Gtk.ContentFit.FILL)
        self.pic.set_halign(Gtk.Align.START)
        self.pic.set_valign(Gtk.Align.START)
        ov.set_child(self.pic)
        self.popup_layer = Gtk.Fixed(accessible_role=Gtk.AccessibleRole.PRESENTATION)
        self.popup_layer.set_can_target(False)
        ov.add_overlay(self.popup_layer)
        self.popup_pics = {}
        self.root = M.Mirror(proxy, {'id': 0, 'role': 'filler'}, accessible_role=Gtk.AccessibleRole.GENERIC)
        self.root.set_focusable(False)
        self.root.set_halign(Gtk.Align.START)
        self.root.set_valign(Gtk.Align.START)
        ov.add_overlay(self.root)
        self.ov = ov

        kc = Gtk.EventControllerKey()
        kc.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        kc.connect('key-pressed', self.on_key, True)
        kc.connect('key-released', self.on_key, False)
        self.win.add_controller(kc)
        gc = Gtk.GestureClick()
        gc.set_button(0)
        gc.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        gc.connect('pressed', self.on_button, True)
        gc.connect('released', self.on_button, False)
        ov.add_controller(gc)
        mc = Gtk.EventControllerMotion()
        mc.connect('motion', self.on_motion)
        mc.connect('enter', self.on_motion)
        ov.add_controller(mc)
        sc = Gtk.EventControllerScroll(flags=Gtk.EventControllerScrollFlags.BOTH_AXES |
                                       Gtk.EventControllerScrollFlags.DISCRETE)
        sc.connect('scroll', self.on_scroll)
        ov.add_controller(sc)
        self.win.connect('notify::is-active', self.on_active)
        if proxy.verbose:
            self.win.connect('notify::focus-widget', self._log_focus_widget)
        self.win.connect('close-request', self.on_close)
        if top.transient_for and top.transient_for in proxy.windows:
            self.win.set_transient_for(proxy.windows[top.transient_for].win)
        self.refresh()
        self.win.present()

    # ---- life -----------------------------------------------------------------------------------------------------
    def log(self, *a):
        self.proxy.log(f'[0x{self.top.id:x}]', *a)

    def destroy(self):
        if self.refresh_src:
            GLib.source_remove(self.refresh_src)
            self.refresh_src = None
        self.win.destroy()

    def _log_focus_widget(self, *_):
        f = self.win.get_focus()
        what = (f'mirror {f.node.get("id")} {f.node.get("role")} {f.node.get("name")!r}' if isinstance(f, M._MirrorMixin)
                else type(f).__name__ if f is not None else None)
        self.log('gtk focus ->', what, 'active', self.win.is_active())

    def on_close(self, *_):
        self.engine.close(self.top)   # WM_DELETE_WINDOW; the window goes when the app unmaps it
        return True

    def set_title(self):
        self.win.set_title(self.proxy.title_prefix + (self.top.title or self.top.wm_class[1]))

    def resized(self):
        g = self.top.geometry
        self.win.set_default_size(g[2], g[3])
        self.dirty = True
        self.schedule_refresh()

    # ---- pixels ---------------------------------------------------------------------------------------------------
    def damaged(self):
        self.dirty = True
        self.schedule_refresh()

    def schedule_refresh(self):
        if self.refresh_src:
            return
        wait = max(0, int((1.0 / FPS_CAP - (time.monotonic() - self.last_frame)) * 1000))
        self.refresh_src = GLib.timeout_add(wait, self._refresh_cb)

    def _refresh_cb(self):
        self.refresh_src = None
        self.refresh()
        return False

    def refresh(self):
        if not self.dirty:
            return
        self.dirty = False
        self.last_frame = time.monotonic()
        px = self.engine.read_pixels(self.top)
        if px is None:
            return
        w, h, depth, data = px
        fmt = Gdk.MemoryFormat.B8G8R8A8_PREMULTIPLIED if depth == 32 else Gdk.MemoryFormat.B8G8R8X8
        self.texture = Gdk.MemoryTexture.new(w, h, fmt, GLib.Bytes.new(data), w * 4)
        self.pic.set_paintable(self.texture)
        self.frames = getattr(self, 'frames', 0) + 1
        if self.proxy.verbose and self.frames % 150 == 0:
            self.log('frames shown', self.frames)
        self.pic.set_size_request(w, h)
        self.root.set_size_request(w, h)
        for nid, fn in list(self.live.items()):
            fn()
        self.proxy.frame_done(self)

    def popup_changed(self, p, shown):
        """An override-redirect window (menu, tooltip, combo list) of the engine: drawn over our pixels where it
        overlaps the window (they are separate X windows)."""
        old = self.popup_pics.pop(p.id, None)
        if old is not None:
            self.popup_layer.remove(old)
        if not shown:
            return
        ox, oy = self.top.geometry[0], self.top.geometry[1]
        x, y, w, h = p.geometry
        if x + w <= ox or y + h <= oy or x >= ox + self.top.geometry[2] or y >= oy + self.top.geometry[3]:
            return
        px = self.engine.read_pixels(p)
        if px is None:
            return
        pw, ph, depth, data = px
        fmt = Gdk.MemoryFormat.B8G8R8A8_PREMULTIPLIED if depth == 32 else Gdk.MemoryFormat.B8G8R8X8
        pic = Gtk.Picture.new_for_paintable(Gdk.MemoryTexture.new(pw, ph, fmt, GLib.Bytes.new(data), pw * 4))
        pic.set_can_shrink(False)
        pic.set_can_target(False)
        self.popup_layer.put(pic, x - ox, y - oy)
        self.popup_pics[p.id] = pic

    # ---- the accessible tree ------------------------------------------------------------------------------------
    def build(self, nodes, root_id, window_box=None, inferred=False):
        """Brings the mirrors of `root_id`'s subtree in line with reader nodes (WINDOW coordinates). Mirrors whose
        node is still there are updated in place, not recreated: a recreated focused mirror would make the screen
        reader announce it again."""
        self.inferred = inferred
        by_parent = {}
        for n in nodes:
            by_parent.setdefault(n.get('parent'), []).append(n)
            self.nodes[n['id']] = n
        top = next((n for n in nodes if n['id'] == root_id), None)
        if top is None:
            return
        f = self.win.get_focus()
        focus_id = f.node.get('id') if isinstance(f, M._MirrorMixin) else None
        if root_id == self.at_window or inferred:
            container, origin = self.root, (0, 0)
            kids = by_parent.get(root_id, [])
            scope = {nid for nid, m in self.mirrors.items() if not self._in_popup(m)}
        elif root_id in self.popup_roots:
            container = self.popup_roots[root_id]
            box = top.get('box') or [0, 0, 0, 0]
            origin = (box[0], box[1])
            sb = top.get('sbox')
            if sb and sb[2] > 0:
                # where it opened this time (engine SCREEN coordinates -> ours)
                ox, oy = self.engine.frame_origin(self.top.id)
                lc = self.root.get_layout_manager().get_layout_child(container)
                lc.set_transform(Gsk.Transform().translate(Graphene.Point().init(sb[0] - ox, sb[1] - oy)))
                container.set_size_request(sb[2], sb[3])
            kids = by_parent.get(root_id, [])
            scope = set(self._ids_under(container)) - {root_id}
            if 'showing' not in top.get('states', []):
                kids = []   # the popup closed
        else:
            old = self.mirrors.get(root_id)
            if old is None:
                return self.proxy.request_tree(self)   # a subtree we never had: fetch the whole window
            container = old.get_parent() if isinstance(old.get_parent(), M._MirrorMixin) else self.root
            kids = [top]
            pnode = self.nodes.get(top.get('parent')) if top.get('parent') else None
            origin = (pnode['box'][0], pnode['box'][1]) if pnode and pnode.get('box') and \
                container is not self.root else (0, 0)
            scope = set(self._ids_under(old))
        keep = set()
        self._reconcile(container, kids, origin, by_parent, inferred, keep)
        for nid in scope - keep:
            m = self.mirrors.pop(nid, None)
            if m is not None:
                self._detach(m)
        for nid, m in self.mirrors.items():
            if self.nodes.get(nid, {}).get('rel'):
                M.set_relations(m, self.nodes[nid], self.mirrors)
        if inferred:
            self.order = [m for m in self.mirrors.values() if m.get_focusable()]
            self.order.sort(key=lambda m: (m.node['box'][1] // 8, m.node['box'][0]))
        f = self.win.get_focus()
        if isinstance(f, M._MirrorMixin) and f.node.get('id') in self.mirrors:
            return   # focus kept (same mirror)
        if focus_id in self.mirrors:
            self.focus_mirror(focus_id)
        elif not inferred:
            fid = next((n['id'] for n in nodes if 'focused' in n.get('states', [])), None)
            if fid in self.mirrors:
                self.focus_from_state(fid)

    def _ids_under(self, m):
        out = [m.node.get('id')]
        for k in m.kids:
            out += self._ids_under(k)
        return out

    def _in_popup(self, m):
        p = m
        while isinstance(p, M._MirrorMixin):
            if p in self.popup_roots.values():
                return True
            p = p.get_parent()
        return False

    def _detach(self, m):
        parent = m.get_parent()
        if isinstance(parent, M._MirrorMixin) and m in parent.kids:
            parent.kids.remove(m)
        if parent is not None:
            m.unparent()

    def _reconcile(self, container, kids, origin, by_parent, inferred, keep):
        for n in kids:
            role = n.get('role', '')
            st = n.get('states', [])
            if not inferred and n.get('parent') is not None and 'showing' not in st and role not in ('menu',):
                continue
            box = n.get('box') or [0, 0, 0, 0]
            x, y = max(-10000, box[0] - origin[0]), max(-10000, box[1] - origin[1])
            m = self.mirrors.get(n['id'])
            if m is not None and (m.get_parent() is not container or type(m) is not M.mirror_class(role)):
                self.mirrors.pop(n['id'], None)
                self._detach(m)
                m = None
            if m is None:
                m = M.make(self.proxy, n, inferred, self.toolkit, getattr(self, 'app_name', ''))
                container.place(m, x, y)
                self.mirrors[n['id']] = m
            else:
                m.node = n
                M.apply_node(m, n, inferred)
                lc = container.get_layout_manager().get_layout_child(m)
                lc.set_transform(Gsk.Transform().translate(Graphene.Point().init(x, y)))
            m.set_size_request(max(0, box[2]), max(0, box[3]))
            keep.add(n['id'])
            skip_named = role in M.ACTIVATABLE or role in M.TEXTUAL or role in M.RANGED
            sub = [c for c in by_parent.get(n['id'], [])
                   if not (skip_named and c.get('role') in M.REDUNDANT_CHILD and not by_parent.get(c['id']))]
            self._reconcile(m, sub, (box[0], box[1]), by_parent, inferred, keep)

    def add_popup(self, aid, info):
        """A popup accessible window of the app (a menu, a combo list): its mirrors go over ours at its place."""
        if aid in self.popup_roots:
            return
        ox, oy = self.engine.frame_origin(self.top.id)
        sb = info.get('sbox') or [0, 0, 0, 0]
        # an unnamed generic container: the popup window itself is not something a screen reader announces on X
        # (Qt names it after the program), only the menu or list inside it
        holder = M.Mirror(self.proxy, {'id': aid, 'role': 'filler', 'name': '', 'states': ['showing']},
                          accessible_role=Gtk.AccessibleRole.GENERIC)
        holder.set_focusable(False)
        self.root.place(holder, sb[0] - ox, sb[1] - oy)
        holder.set_size_request(max(0, sb[2]), max(0, sb[3]))
        self.popup_roots[aid] = holder
        self.log('popup', aid, info.get('role'), info.get('name'), 'at', sb[0] - ox, sb[1] - oy)

    def remove_popup(self, aid):
        holder = self.popup_roots.pop(aid, None)
        if holder is None:
            return
        for nid in self._ids_under(holder):
            self.mirrors.pop(nid, None)
        self._detach(holder)

    def _forget(self, m):
        self.mirrors.pop(m.node.get('id'), None)
        for k in m.kids:
            self._forget(k)

    def focus_mirror(self, nid, tries=0):
        """Puts GNOME's focus on the mirror of the app's focused node `nid` (also while the window is inactive: GTK 4
        announces that focus when the window becomes active -- unless the window was activated in the same frame as
        the mirror was made, see `_refocus`)."""
        m = self.mirrors.get(nid)
        if m is None:
            return False
        self.want_focus = nid
        self.syncing_focus = True
        try:
            if not m.get_focusable():
                m.set_focusable(True)
            ok = m.grab_focus()
        finally:
            self.syncing_focus = False
        if not ok and tries < 30:
            # not mapped yet (the window was just shown): again in a moment, unless the app moved on
            GLib.timeout_add(60, lambda: (self.want_focus == nid and self.focus_mirror(nid, tries + 1)) and False)
        return True

    def focus_from_state(self, nid):
        """A node whose state says "focused" in a tree, with no focus event behind it (an app that focused it before
        anyone listened): focused a moment later, if it is still there and no focus event came meanwhile. Electron's
        first document is "focused" for a few ms before the page's document replaces it; focused at once, its empty
        mirror was announced when the window activated in that moment (proxy_fix el_suite2 run_6)."""
        if getattr(self, 'state_focus_src', None):
            GLib.source_remove(self.state_focus_src)
        asked = time.monotonic()

        def run():
            self.state_focus_src = None
            if getattr(self, 'last_focus_t', 0) >= asked:
                return False   # a focus event came meanwhile (Chromium's are applied 120 ms late: el_suite4 run_14)
            f = self.win.get_focus()
            if isinstance(f, M._MirrorMixin) and f.node.get('id') in self.mirrors:
                return False   # a focus event placed it
            n = self.nodes.get(nid)
            if nid in self.mirrors and n is not None and 'focused' in n.get('states', []):
                self.focus_mirror(nid)
            return False
        self.state_focus_src = GLib.timeout_add(STATE_FOCUS_MS, run)

    def _release_held(self):
        held, self.held = self.held, []
        for e in held:
            self.node_event(dict(e, _held=True))
        return False

    def node_event(self, ev):
        """An app event for a node of this window (from the reader)."""
        m = self.mirrors.get(ev['id'])
        n = self.nodes.get(ev['id'])
        k = ev['kind']
        # What a screen reader says on a focus change depends on the state it reads at that moment, and natively that
        # is toolkit-specific (measured): for GTK 3, Qt and Java it reads the state
        # its event cache had at the focus (a toggle reported after the focus is spoken as a change: "not checked ...
        # checked"); Chromium answers live, so a toggle right after the focus is already in ("checked"). GTK 4 itself
        # reports a mirror's focus a moment after grab_focus but a state change at once, so:
        # - cache-style sources: changes that follow a focus within FOLLOW_MS are held until GTK has reported the
        #   focus, in order;
        # - Chromium: the focus is held for FOLLOW_MS so the changes right behind it are in place first.
        now = time.monotonic()
        if k == 'active':
            # The app's window lost the X focus while its GNOME window is still the active one. Measured (GTK 4 on
            # GNOME 50): GTK 4 focuses itself with a core XSetInputFocus when it maps, before
            # the proxy made the reverse master its ClientPointer; the bridge then takes the core keyboard back
            # ("taking it back"), GTK 4 sees FocusOut and stops reporting focus changes for good. A window manager
            # would give the focus back to the active window: so do we (at most 5 times in 10 s).
            if not ev.get('on') and self.win.is_active():
                hist = [t for t in getattr(self, 'refocus_t', []) if now - t < 10] + [now]
                self.refocus_t = hist
                if len(hist) <= 5:
                    self.log('app window deactivated while active on GNOME: X focus again', hex(self.top.id))
                    GLib.timeout_add(100, lambda: (self.win.is_active() and self.engine.refocus(self.top)) and False)
            return
        live = self.toolkit.startswith('chromium')
        if k == 'focus' and not ev.get('_held'):
            self.last_focus_t = now
            if live:
                GLib.timeout_add(FOLLOW_MS, lambda: self.node_event(dict(ev, _held=True)) and False)
                return
        if k == 'focus':
            if not self.focus_mirror(ev['id']):
                self.pending_focus = ev['id']
            if self.held:
                GLib.timeout_add(FOLLOW_MS, self._release_held)
            return
        if not live and not ev.get('_held') and k in ('state', 'value', 'name', 'desc') and \
                (now - getattr(self, 'last_focus_t', 0)) * 1000 < FOLLOW_MS:
            self.held.append(ev)
            if len(self.held) == 1:
                GLib.timeout_add(FOLLOW_MS, self._release_held)
            return
        if m is None or n is None:
            return
        if k == 'state':
            st = set(n.get('states', []))
            (st.add if ev['on'] else st.discard)(ev['state'])
            n['states'] = sorted(st)
            M.apply_states(m, n)
            if ev['state'] == 'focusable':
                m.set_focusable(ev['on'])
        elif k == 'value' and isinstance(m, M.RangeMirror) and ev.get('value') is not None:
            props, vals = [Gtk.AccessibleProperty.VALUE_NOW], [float(ev['value'])]
            if ev.get('vtext'):
                props.append(Gtk.AccessibleProperty.VALUE_TEXT); vals.append(ev['vtext'])
            m.update_property(props, vals)
        elif k == 'text' and isinstance(m, M.TextMirror):
            m.set_text(ev['text'], ev.get('caret'))
        elif k == 'caret' and isinstance(m, M.TextMirror):
            m.set_caret(ev['caret'])
        elif k == 'selection' and isinstance(m, M.TextMirror):
            m.set_sel(ev.get('sel'))
        elif k == 'name':
            n['name'] = ev['name']
            m.update_property([Gtk.AccessibleProperty.LABEL], [ev['name']])
        elif k == 'desc':
            n['desc'] = ev['desc']
            m.update_property([Gtk.AccessibleProperty.DESCRIPTION], [ev['desc']])
        elif k == 'announce':
            pr = Gtk.AccessibleAnnouncementPriority
            m.announce(ev['text'], pr.HIGH if ev.get('politeness') == 2 else pr.MEDIUM)

    # ---- focus GNOME <-> X ----------------------------------------------------------------------------------------
    def on_active(self, *_):
        if self.win.is_active():
            self.engine.raise_window(self.top)
            self.engine.focus(self.top)
            # the app's focus, not whatever GTK picked on activation, and announced (see _refocus); once more later,
            # for a proxy whose objects GTK had not yet put on the bus when the window activated. Also when the app's
            # focus is not known yet: it may arrive between the activation and that moment.
            GLib.timeout_add(80, self._refocus)
            GLib.timeout_add(REFOCUS_AGAIN_MS, self._refocus)
            self.log('active: X focus ->', hex(self.top.id), 'now', hex(self.engine.focused_window() or 0))
            if self.inferred and self.local_nav and self.order and \
                    not isinstance(self.win.get_focus(), M._MirrorMixin):
                self.order[0].grab_focus()
        else:
            if self.proxy.verbose:
                self.log('inactive on GNOME')
            # keys held while focus left would never be released (stuck, auto-repeating keys)
            self.engine.release_all()
            self.engine.unfocus()

    def _refocus(self):
        """After activation: the app's focus on its mirror, as a focus change GTK reports. GTK 4 announces the
        focused widget of a window that becomes active only if that widget was exposed before: a mirror made and
        focused in the same frame as the activation (8 ms apart: Electron, when another window was active before)
        was announced to nobody, and Orca stayed silent until the person's first key (measured with
        Electron at start-up). So the focus is taken off it and put back: a real change,
        which GTK reports; when GTK had announced it already, Orca takes it as the focus it has and says nothing.
        (The bounce used to go through an unexposed Gtk.Label "sink", which GTK 4 never lets take the focus -- not
        selectable -- so it did nothing; a focusable unexposed widget instead is announced by Orca as "invalid".)
        It runs twice: GTK 4 puts a new app's objects on the accessibility bus a moment after it first needs them, and
        a window activated before that sends its activation and focus for objects a screen reader cannot resolve
        ("Object does not exist"): proxy_fix el_suite run_11, where the 80 ms bounce came just in time."""
        nid = getattr(self, 'want_focus', None)
        m = self.mirrors.get(nid)
        if m is None or not self.win.is_active():
            return False
        if self.win.get_focus() is m:
            self.syncing_focus = True
            try:
                self.win.set_focus(None)
            finally:
                self.syncing_focus = False
        self.focus_mirror(nid)
        return False

    def mirror_focused(self, m):
        """GTK focus moved onto a mirror. If the app did not move it (Orca's structural navigation, an AT's
        grab_focus, a click on the mirror), move the app's focus there too."""
        if self.inferred and self.local_nav:
            # Tab moved here in the proxy: a text field gets the app's focus the way a person gives it, by a click
            if not self.syncing_focus and m.node.get('role') in ('text', 'entry') and m.node.get('box'):
                self.click_node(m.node)
            return
        if self.syncing_focus:
            return
        if self.source == 'tk':
            self.proxy.tk_focus(self, m.node)
            return
        nid = m.node.get('id')
        if nid:
            self.proxy.send({'do': 'focus', 'id': nid})

    # ---- keyboard -------------------------------------------------------------------------------------------------
    def on_key(self, ctl, keyval, keycode, state, press):
        name = Gdk.keyval_name(keyval) or ''
        if self.inferred and self.local_nav:
            # the app has no focus chain we can see: Tab moves between mirrors here; Space/Return click the
            # focused hotspot; other keys go to the app
            if name in ('Tab', 'ISO_Left_Tab'):
                if press:
                    self.inferred_tab(-1 if (name == 'ISO_Left_Tab' or state & Gdk.ModifierType.SHIFT_MASK) else 1)
                return True
            f = self.win.get_focus()
            if name in ('space', 'Return', 'KP_Enter') and isinstance(f, M._MirrorMixin) and f.node.get('box'):
                if press:
                    self.click_node(f.node)
                return True
        extra = []
        if press and keycode not in MODIFIER_KEYCODES:
            extra = self.modsync(state)
        for kc in extra:
            self.engine.key(kc, True)
        self.engine.key(keycode, press)
        for kc in reversed(extra):
            if kc not in (KC_CAPS, KC_NUM):
                self.engine.key(kc, False)
        self.engine.flush()
        if self.source == 'tk' and not press:
            self.proxy.tk_refresh_soon(self)
        if self.proxy.verbose:
            self.log('key', 'down' if press else 'up', keycode, name, 'state', int(state), 'sync', extra)
        return True

    def modsync(self, state):
        """Modifiers GNOME applied to this key that the engine does not hold (sticky keys latch them in mutter, so
        their own press and release came and went), and lock states that differ (Caps/Num Lock toggled while
        another window had focus): the keycodes to press around the key (locks: tapped once)."""
        if not self.proxy.modsync:
            return []
        eff, locked = self.engine.modifier_state()
        extra = [kc for gmask, xmask, kc in MODSYNC if (state & gmask) and not (eff & xmask)]
        seat = self.win.get_display().get_default_seat()
        kbd = seat.get_keyboard() if seat else None
        if kbd is not None:
            caps = kbd.get_caps_lock_state()
            num = kbd.get_num_lock_state()
            if caps != bool(locked & LOCK):
                self.engine.key(KC_CAPS, True); self.engine.key(KC_CAPS, False)
            if num != bool(locked & MOD2):
                self.engine.key(KC_NUM, True); self.engine.key(KC_NUM, False)
        return extra

    # ---- pointer --------------------------------------------------------------------------------------------------
    def to_engine(self, x, y):
        ok, p = self.ov.compute_point(self.pic, Graphene.Point().init(x, y))
        x, y = (p.x, p.y) if ok else (x, y)
        ox, oy = self.engine.frame_origin(self.top.id)
        return ox + x, oy + y

    def on_button(self, g, n_press, x, y, press):
        if not self.engine.can_click():
            if press:
                self.log('click ignored: no reverse master (bridge without -reverseinput)')
            return
        b = g.get_current_button() or 1
        if b > 3:   # GDK numbers back/forward 8/9 like X
            b = {8: 8, 9: 9}.get(b, b)
        ex, ey = self.to_engine(x, y)
        self.engine.raise_window(self.top)
        self.engine.motion(ex, ey)
        self.engine.button(b, press)
        self.engine.flush()
        self.log('button', b, 'down' if press else 'up', int(x), int(y), '-> engine', int(ex), int(ey))

    def on_motion(self, ctl, x, y):
        if not self.engine.can_click():
            return
        p = (int(x), int(y))
        if p == self.last_motion:
            return
        self.last_motion = p
        ex, ey = self.to_engine(x, y)
        self.engine.motion(ex, ey)
        self.engine.flush()
        self.proxy.cursor_follow(self)

    def on_scroll(self, ctl, dx, dy):
        if not self.engine.can_click():
            return False
        for d, neg, pos in ((dy, 4, 5), (dx, 6, 7)):
            steps = int(round(abs(d))) or (1 if d else 0)
            for _ in range(steps):
                b = neg if d < 0 else pos
                self.engine.button(b, True)
                self.engine.button(b, False)
        self.engine.flush()
        return True

    def click_node(self, n):
        b = n['box']
        ox, oy = self.engine.frame_origin(self.top.id)
        x, y = ox + b[0] + b[2] // 2, oy + b[1] + b[3] // 2
        if not self.engine.can_click():
            self.log('hotspot click ignored: no reverse master')
            return
        self.engine.raise_window(self.top)
        self.engine.click_at(x, y)
        self.log('hotspot click', n.get('name'), b, '-> engine', x, y)

    # ---- inferred mode ------------------------------------------------------------------------------------------
    def inferred_tab(self, step):
        if not self.order:
            return
        f = self.win.get_focus()
        i = self.order.index(f) if f in self.order else -1
        self.order[(i + step) % len(self.order)].grab_focus()
