"""GTK 4 widgets that stand for the X app's controls on the desktop's accessibility bus.

Each mirror is placed over its control's pixels and draws nothing but a focus ring; what matters is its accessible
state, which GTK publishes over AT-SPI to Orca, the magnifier and every other assistive technology:
- role, name, description (tooltip), key shortcut, placeholder, orientation, level, read-only, required, ...
- states: checked/mixed, pressed, expanded, selected, disabled, invalid, busy;
- values through GtkAccessibleRange (GTK exposes AT-SPI Value only for its implementers), text and caret through
  GtkAccessibleText (GTK >= 4.14);
- actions: activatable controls (buttons, check boxes, radio buttons, menu items, links, tabs) are Gtk.Button
  subclasses, because GTK gives a GtkButton the AT-SPI action "click" *with the key binding taken from its
  KEY_SHORTCUTS* (";;Alt+C") — the only way to get a key binding into a GTK 4 action, and what Orca reads to speak
  a check box's shortcut. Other controls get the app's own actions as an action group ("app.<name>").
- relations (labelled-by, described-by, controls, flows-to, ...) between mirrors.
Any action, value or caret an assistive technology sets on a mirror is forwarded to the app (AT-SPI on the private
bus); the mirror then follows the app's own events."""
import re

import gi
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')
gi.require_version('Graphene', '1.0')
gi.require_version('Gsk', '4.0')
from gi.repository import Gdk, Gio, GLib, Graphene, Gsk, Gtk, Pango  # noqa: E402

ROLE = {
    'push button': 'BUTTON', 'button': 'BUTTON', 'toggle button': 'TOGGLE_BUTTON', 'check box': 'CHECKBOX',
    'radio button': 'RADIO', 'menu item': 'MENU_ITEM', 'check menu item': 'MENU_ITEM_CHECKBOX',
    'radio menu item': 'MENU_ITEM_RADIO', 'tearoff menu item': 'MENU_ITEM', 'link': 'LINK', 'page tab': 'TAB',
    'page tab list': 'TAB_LIST', 'text': 'TEXT_BOX', 'entry': 'TEXT_BOX', 'password text': 'TEXT_BOX',
    'terminal': 'TERMINAL', 'paragraph': 'PARAGRAPH', 'slider': 'SLIDER', 'spin button': 'SPIN_BUTTON',
    'scroll bar': 'SCROLLBAR', 'progress bar': 'PROGRESS_BAR', 'level bar': 'METER', 'combo box': 'COMBO_BOX',
    'list': 'LIST', 'list box': 'LIST_BOX', 'list item': 'LIST_ITEM', 'menu': 'MENU', 'menu bar': 'MENU_BAR',
    'popup menu': 'MENU', 'tool bar': 'TOOLBAR', 'status bar': 'STATUS', 'table': 'TABLE', 'tree table': 'TREE_GRID',
    'tree': 'TREE', 'tree item': 'TREE_ITEM', 'table cell': 'CELL', 'table row': 'ROW',
    'table column header': 'COLUMN_HEADER', 'table row header': 'ROW_HEADER', 'column header': 'COLUMN_HEADER',
    'row header': 'ROW_HEADER', 'label': 'LABEL', 'static': 'LABEL', 'caption': 'CAPTION', 'heading': 'HEADING',
    'image': 'IMG', 'icon': 'IMG', 'separator': 'SEPARATOR', 'panel': 'GROUP', 'grouping': 'GROUP',
    'section': 'SECTION', 'filler': 'GENERIC', 'scroll pane': 'GROUP', 'viewport': 'GENERIC', 'split pane': 'GROUP',
    'layered pane': 'GENERIC', 'root pane': 'GENERIC', 'internal frame': 'GROUP', 'option pane': 'GROUP',
    'document web': 'DOCUMENT', 'document frame': 'DOCUMENT', 'document text': 'DOCUMENT', 'article': 'ARTICLE',
    'form': 'FORM', 'landmark': 'LANDMARK', 'alert': 'ALERT', 'notification': 'ALERT', 'tooltip': 'TOOLTIP',
    'switch': 'SWITCH', 'block quote': 'BLOCK_QUOTE', 'canvas': 'IMG', 'drawing area': 'IMG', 'dial': 'SLIDER',
    'animation': 'IMG', 'description list': 'LIST', 'description term': 'LIST_ITEM', 'description value': 'GENERIC',
    'footer': 'GROUP', 'header': 'GROUP', 'math': 'MATH', 'comment': 'COMMENT', 'mark': 'GENERIC',
    'application': 'APPLICATION', 'dialog': 'DIALOG', 'frame': 'GENERIC', 'window': 'GENERIC',
}
ACTIVATABLE = {'push button', 'button', 'toggle button', 'check box', 'radio button', 'menu item', 'check menu item',
               'radio menu item', 'tearoff menu item', 'link', 'page tab', 'switch'}
TEXTUAL = {'text', 'entry', 'password text', 'terminal'}
RANGED = {'slider', 'spin button', 'scroll bar', 'progress bar', 'level bar', 'dial'}
# children that only repeat their parent's name (a button's label, an entry's inner text): not mirrored
REDUNDANT_CHILD = {'label', 'text', 'icon', 'image', 'filler'}
RELATIONS = {'labelled-by': 'LABELLED_BY', 'described-by': 'DESCRIBED_BY', 'controller-for': 'CONTROLS',
             'flows-to': 'FLOW_TO', 'flows-from': 'FLOW_FROM', 'details': 'DETAILS', 'error-message': 'ERROR_MESSAGE',
             'node-child-of': None, 'label-for': None, 'description-for': None, 'member-of': None}

CSS = b"""
.x11mirror { min-width: 0; min-height: 0; padding: 0; margin: 0; border: none; border-radius: 0;
             background: none; box-shadow: none; outline: none; color: transparent; }
"""


def install_css():
    p = Gtk.CssProvider()
    p.load_from_data(CSS)
    Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), p,
                                              Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)


def gtk_role(role):
    return getattr(Gtk.AccessibleRole, ROLE.get(role, 'GENERIC'), Gtk.AccessibleRole.GENERIC)


def tri(on, mixed=False):
    if mixed:
        return int(Gtk.AccessibleTristate.MIXED)
    return int(Gtk.AccessibleTristate.TRUE if on else Gtk.AccessibleTristate.FALSE)


_MODS = {'<alt>': 'Alt', '<control>': 'Control', '<ctrl>': 'Control', '<primary>': 'Control', '<shift>': 'Shift',
         '<super>': 'Meta', '<meta>': 'Meta', '<mod1>': 'Alt'}


def shortcut(binding):
    """An AT-SPI key binding ("<Alt>c", "mnemonic;path;accel", Qt's "Alt+C", Java's "alt C") as the ARIA-style
    KEY_SHORTCUTS GTK takes ("Alt+C"); several are space-separated."""
    if not binding:
        return None
    out = []
    for part in [p for p in binding.split(';') if p.strip()]:
        p = part.strip()
        mods = []
        low = p.lower()
        for m, name in _MODS.items():
            while m in low:
                i = low.index(m)
                low = low[:i] + low[i + len(m):]
                p = p[:i] + p[i + len(m):]
                if name not in mods:
                    mods.append(name)
        toks = re.split(r'[+ ]', p)
        key = toks[-1] if toks else ''
        for t in toks[:-1]:
            name = {'alt': 'Alt', 'ctrl': 'Control', 'control': 'Control', 'shift': 'Shift', 'meta': 'Meta',
                    'super': 'Meta'}.get(t.lower())
            if name and name not in mods:
                mods.append(name)
        if not key:
            continue
        key = key.upper() if len(key) == 1 else key
        s = '+'.join(mods + [key])
        if s not in out:
            out.append(s)
    return ' '.join(out) or None


SPEAKS_ACCELERATOR = {'push button', 'button', 'menu item', 'check menu item', 'radio menu item', 'link'}


def _label(seq):
    """A key sequence as a screen reader labels it ("<Alt>c", "Alt+C", "alt C" -> "Alt+C"; "s" -> "S")."""
    if not seq:
        return ''
    if len(seq) > 1 and not seq.startswith('<') and ',' not in seq:
        toks = re.split(r'[+ ]', seq)
        seq = ''.join(f'<{t}>' for t in toks[:-1]) + toks[-1]
    try:
        ok, key, mods = Gtk.accelerator_parse(seq)
        if ok and key:
            lab = Gtk.accelerator_get_label(key, mods)
            if lab and not lab.endswith('+'):
                return lab
    except Exception:
        pass
    return seq.replace('<', '').replace('>', '+').strip('+')


def app_keys(n):
    """(mnemonic, accelerator) of an app node the way Orca finds them: the ARIA keyshortcuts attribute, then the
    first action key binding "mnemonic;sequence;accelerator"."""
    attr = ((n.get('attrs') or {}).get('keyshortcuts') or '').split(' ')[0]
    binding = next((a[1] for a in n.get('actions') or [] if a[1]), '')
    parts = binding.split(';')
    mn = acc = ''
    if attr and len(_label(attr)) == 1:
        mn = attr
    elif parts[0] and 'Ctrl' not in _label(parts[0]) and 'Control' not in parts[0] and \
            _label(parts[0]).lower() not in (' ', 'space'):
        mn = parts[0]
    if attr and len(_label(attr)) > 1:
        acc = attr
    elif len(parts) == 3 and parts[2]:
        acc = parts[2]
    elif parts[-1] and 'Ctrl' in _label(parts[-1]) and len(parts) != 3:
        acc = parts[-1]
    return mn, acc


def key_config(n, button_like):
    """What to give a mirror so a screen reader finds the app's mnemonic and accelerator on it.

    GTK 4's knobs: KEY_SHORTCUTS (it becomes the `keyshortcuts` attribute and the accelerator slot of a GtkButton's
    "click" key binding) and a mnemonic label on a GtkButton (its letter becomes the binding's mnemonic slot, and GTK
    then *also* adds "Alt+<letter>" to `keyshortcuts`). Measured, a mnemonic
    slot holding "Alt+C" is impossible. So:
    - roles whose accelerator Orca speaks (push buttons, menu items, links): KEY_SHORTCUTS = the app's accelerator,
      else its mnemonic as a full key ("Alt+S"); no mnemonic label (it would add a second "Alt+S");
    - other activatable roles (check boxes, radio and toggle buttons, tabs): a mnemonic label with the app's
      mnemonic letter (Orca says "C" where GTK 3 / Qt apps give "Alt+C"; Java and Chromium give "C" too);
    - everything else: KEY_SHORTCUTS = the full mnemonic or the accelerator (for ATs that read the attribute)."""
    mn, acc = app_keys(n)
    role = n.get('role', '')
    lab = _label(mn) if mn else ''
    full = ('Alt+' + lab) if len(lab) == 1 else lab
    letter = lab[-1] if lab else None
    if not button_like:
        if role == 'menu':
            return (_label(acc) if acc else None), None   # Orca does not announce a menu's mnemonic natively
        return (_label(acc) if acc else full) or None, None
    if role in SPEAKS_ACCELERATOR:
        keys, letter = (_label(acc) if acc else full) or None, (letter if acc and mn else None)
    else:
        keys = _label(acc) if acc else None
    if letter and keys and keys.lower() == f'alt+{letter.lower()}':
        keys = None   # GTK adds "Alt+<letter>" for the mnemonic label itself
    return keys, letter


class _MirrorMixin:
    """Shared by every mirror: the app node it stands for, absolute placement of children, a focus ring."""

    def mirror_init(self, proxy, node):
        self.proxy = proxy
        self.node = node
        self.kids = []
        self.add_css_class('x11mirror')
        self.set_can_target(False)
        if not isinstance(self.get_layout_manager(), Gtk.FixedLayout):
            self.set_layout_manager(Gtk.FixedLayout())
        fc = Gtk.EventControllerFocus()
        fc.connect('enter', self._on_focus_enter)
        self.add_controller(fc)

    def _on_focus_enter(self, *_):
        self.queue_draw()
        self.proxy.mirror_focused(self)

    def place(self, child, x, y):
        child.set_parent(self)
        self.kids.append(child)
        lc = self.get_layout_manager().get_layout_child(child)
        lc.set_transform(Gsk.Transform().translate(Graphene.Point().init(x, y)))

    def clear_kids(self):
        for k in self.kids:
            if hasattr(k, 'clear_kids'):
                k.clear_kids()
            k.unparent()
        self.kids = []

    def snapshot_ring(self, s):
        if self.has_focus():
            w, h = self.get_width(), self.get_height()
            c = Gdk.RGBA(red=1, green=0.45, blue=0, alpha=1)
            for r in ((0, 0, w, 3), (0, h - 3, w, 3), (0, 0, 3, h), (w - 3, 0, 3, h)):
                s.append_color(c, Graphene.Rect().init(*r))


class Mirror(Gtk.Widget, _MirrorMixin):
    __gtype_name__ = 'X11A11yMirror'

    def __init__(self, proxy, node, **kw):
        super().__init__(**kw)
        self.mirror_init(proxy, node)

    def do_snapshot(self, s):
        for k in self.kids:
            self.snapshot_child(k, s)
        self.snapshot_ring(s)


class ButtonMirror(Gtk.Button, _MirrorMixin):
    """Activatable controls: AT-SPI action "click" (with the key binding) -> the app's default action."""
    __gtype_name__ = 'X11A11yButtonMirror'

    def __init__(self, proxy, node, **kw):
        super().__init__(**kw)
        self.set_has_frame(False)
        self.mirror_init(proxy, node)
        self.connect('clicked', lambda *_: proxy.mirror_action(self, 0))

    def do_snapshot(self, s):
        for k in self.kids:
            self.snapshot_child(k, s)
        self.snapshot_ring(s)

    def set_mnemonic(self, name, letter):
        """A mnemonic label (never drawn): GTK then reports `letter` in the mnemonic slot of the "click" action's
        key binding, where screen readers look for a check box's or radio button's mnemonic."""
        i = name.lower().find(letter.lower())
        text = (name[:i] + '_' + name[i:]) if i >= 0 else f'{name} (_{letter})'
        self.set_use_underline(True)
        self.set_label(text.replace('__', '_'))
        lab = self.get_child()
        if lab is not None:
            lab.set_can_target(False)
            if hasattr(lab, 'set_ellipsize'):
                lab.set_ellipsize(Pango.EllipsizeMode.END)   # its width must not grow the mirror
        # the name stays the app's (set explicitly); the label child is the button's own, as in GTK's buttons
        self.update_property([Gtk.AccessibleProperty.LABEL], [name])


class TextMirror(Gtk.Widget, Gtk.AccessibleText, _MirrorMixin):
    """A text field: the app's text, caret and selection through GtkAccessibleText; an AT moving the caret or the
    selection moves the app's."""
    __gtype_name__ = 'X11A11yTextMirror'

    def __init__(self, proxy, node, **kw):
        super().__init__(**kw)
        self.text, self.caret, self.sel = '', 0, []
        self.mirror_init(proxy, node)

    def do_snapshot(self, s):
        for k in self.kids:
            self.snapshot_child(k, s)
        self.snapshot_ring(s)

    # GtkAccessibleText: offsets are in characters; contents are UTF-8 bytes
    def do_get_contents(self, start, end):
        return GLib.Bytes.new(self.text[start:end if end >= 0 else None].encode())

    def do_get_contents_at(self, offset, granularity):
        t = self.text
        offset = max(0, min(offset, len(t)))
        G = Gtk.AccessibleTextGranularity
        if granularity == G.CHARACTER:
            if offset >= len(t):
                return GLib.Bytes.new(b''), offset, offset
            return GLib.Bytes.new(t[offset].encode()), offset, offset + 1
        if granularity == G.WORD:
            for m in re.finditer(r'\S+\s*', t):
                if m.start() <= offset < m.end() or (offset == len(t) and m.end() == len(t)):
                    return GLib.Bytes.new(m.group().encode()), m.start(), m.end()
        if granularity in (G.LINE, G.PARAGRAPH, G.SENTENCE):
            start = t.rfind('\n', 0, offset) + 1
            end = t.find('\n', offset)
            end = len(t) if end < 0 else end + 1
            return GLib.Bytes.new(t[start:end].encode()), start, end
        return GLib.Bytes.new(t.encode()), 0, len(t)

    def do_get_caret_position(self):
        return self.caret

    # PyGObject (3.48 and 3.56 measured) does not hide an out array's length argument in a vfunc implemented in
    # Python: get_selection must return (ok, n_ranges, ranges) and get_attributes (ok, n_ranges, ranges, names,
    # values). The shorter tuples raised "TypeError: Must be number, not list" on every call, so no selection ever
    # reached an AT (measured in the parity tests).
    def do_get_selection(self):
        if not self.sel:
            return False, 0, []
        ranges = []
        for s, e in self.sel:
            r = Gtk.AccessibleTextRange()
            r.start, r.length = s, max(0, e - s)
            ranges.append(r)
        return True, len(ranges), ranges

    def do_get_attributes(self, offset):
        return False, 0, [], [], []

    def do_get_default_attributes(self):
        return [], []

    def do_set_caret_position(self, offset):   # GTK >= 4.22: an AT moves the caret
        self.proxy.send({'do': 'set_caret', 'id': self.node['id'], 'offset': offset})
        return True

    def do_set_selection(self, i, r):          # GTK >= 4.22
        self.proxy.send({'do': 'set_selection', 'id': self.node['id'], 'start': r.start, 'end': r.start + r.length})
        return True

    def set_text(self, text, caret=None):
        old, self.text = self.text, text or ''
        if caret is not None:
            self.caret = max(0, min(caret, len(self.text)))
        if old != self.text:
            # the smallest change: common prefix/suffix (Orca speaks inserted/removed text from these)
            p = 0
            while p < min(len(old), len(self.text)) and old[p] == self.text[p]:
                p += 1
            q = 0
            while q < min(len(old), len(self.text)) - p and old[-1 - q] == self.text[-1 - q]:
                q += 1
            if len(old) - p - q > 0:
                self.update_contents(Gtk.AccessibleTextContentChange.REMOVE, p, len(old) - q)
            if len(self.text) - p - q > 0:
                self.update_contents(Gtk.AccessibleTextContentChange.INSERT, p, len(self.text) - q)
        self.update_caret_position()

    def set_caret(self, caret):
        self.caret = max(0, min(caret, len(self.text)))
        self.update_caret_position()

    def set_sel(self, sel):
        self.sel = sel or []
        self.update_selection_bound()


class RangeMirror(Gtk.Widget, Gtk.AccessibleRange, _MirrorMixin):
    """A slider / spin button / scroll bar / progress bar: AT-SPI Value; an AT setting the value sets the app's."""
    __gtype_name__ = 'X11A11yRangeMirror'

    def __init__(self, proxy, node, **kw):
        super().__init__(**kw)
        self.mirror_init(proxy, node)

    def do_snapshot(self, s):
        for k in self.kids:
            self.snapshot_child(k, s)
        self.snapshot_ring(s)

    def do_set_current_value(self, value):
        self.proxy.send({'do': 'set_value', 'id': self.node['id'], 'value': value})
        return True


def mirror_class(role):
    return (ButtonMirror if role in ACTIVATABLE else TextMirror if role in TEXTUAL else
            RangeMirror if role in RANGED else Mirror)


BROWSERS = ('chrome', 'chromium', 'google-chrome', 'brave', 'vivaldi', 'opera', 'msedge')


def make(proxy, node, inferred=False, toolkit='', app=''):
    """The mirror widget for one app node (without children)."""
    role = node.get('role', '')
    cls = mirror_class(role)
    grole = gtk_role(role)
    if role == 'document web' and toolkit.startswith('chromium') and not app.lower().startswith(BROWSERS):
        # an Electron app's web view: Orca keeps such apps in focus mode ("Focus mode is sticky"); as a GTK document
        # the mirror would put Orca in browse mode, where letters and digits are its navigation keys, not the app's
        grole = Gtk.AccessibleRole.GROUP
    m = cls(proxy, node, accessible_role=grole)
    apply_node(m, node, inferred, first=True)
    return m


def provenance_mark(prov):
    """What a mirror's description says about where its name came from, for an inferred node: nothing when the
    app or its toolkit reported the name (Tk introspection, editres GetValues, the text the app draws in the
    widget's window), otherwise the source (resource files, the widget's name, pixels, the X window tree)."""
    if prov.startswith('inferred from the X widget tree'):
        return prov.split('; ', 1)[1] if '; ' in prov else ''
    if 'pixels' in prov or prov.startswith('inferred from the X window tree') or prov == 'inferred':
        return prov
    return ''


def apply_node(m, n, inferred=False, first=False):
    """Pushes a node's properties and states onto its mirror (also after the app changed them)."""
    role = n.get('role', '')
    st = set(n.get('states', []))
    name = n.get('name') or ''
    if role in ('image', 'icon') and not name:
        name = n.get('image') or n.get('desc') or ''
    desc = n.get('desc') or ''
    mark = provenance_mark(n.get('provenance', 'inferred')) if inferred else ''
    if mark:
        desc = (desc + ' ' if desc else '') + mark
    props, vals = [Gtk.AccessibleProperty.LABEL, Gtk.AccessibleProperty.DESCRIPTION], [name, desc]
    keys, letter = key_config(n, isinstance(m, ButtonMirror))
    if keys:
        props.append(Gtk.AccessibleProperty.KEY_SHORTCUTS); vals.append(keys)
    if letter and isinstance(m, ButtonMirror) and first:
        m.set_mnemonic(name, letter)
    v = n.get('value')
    if v and isinstance(m, RangeMirror):
        cur, lo, hi = (float(x) if x is not None else 0.0 for x in v[:3])
        props += [Gtk.AccessibleProperty.VALUE_MIN, Gtk.AccessibleProperty.VALUE_MAX, Gtk.AccessibleProperty.VALUE_NOW]
        vals += [lo, hi, cur]
        if n.get('vtext'):
            props.append(Gtk.AccessibleProperty.VALUE_TEXT); vals.append(n['vtext'])
    if 'horizontal' in st or 'vertical' in st:
        props.append(Gtk.AccessibleProperty.ORIENTATION)
        vals.append(int(Gtk.Orientation.HORIZONTAL if 'horizontal' in st else Gtk.Orientation.VERTICAL))
    attrs = n.get('attrs') or {}
    if isinstance(m, TextMirror):
        ph = attrs.get('placeholder-text')
        if ph is None and role in ('text', 'entry') and n.get('desc'):
            ph = None
        if ph:
            props.append(Gtk.AccessibleProperty.PLACEHOLDER); vals.append(ph)
        props.append(Gtk.AccessibleProperty.MULTI_LINE); vals.append('multi-line' in st)
        props.append(Gtk.AccessibleProperty.READ_ONLY); vals.append('editable' not in st)
    if role == 'heading' and attrs.get('level', '').isdigit():
        props.append(Gtk.AccessibleProperty.LEVEL); vals.append(int(attrs['level']))
    if 'required' in st:
        props.append(Gtk.AccessibleProperty.REQUIRED); vals.append(True)
    if 'has-popup' in st:
        props.append(Gtk.AccessibleProperty.HAS_POPUP); vals.append(True)
    if 'modal' in st:
        props.append(Gtk.AccessibleProperty.MODAL); vals.append(True)
    if attrs.get('roledescription'):
        props.append(Gtk.AccessibleProperty.ROLE_DESCRIPTION); vals.append(attrs['roledescription'])
    try:
        m.update_property(props, vals)
    except Exception as ex:
        m.proxy.log('mirror: property', n.get('id'), role, repr(ex))
    apply_states(m, n)
    if isinstance(m, TextMirror):
        m.set_text(n.get('text', ''), n.get('caret', 0))
        if n.get('sel'):
            m.set_sel(n['sel'])
    # focusable: what the app says (labels and containers are reached by review/structural navigation)
    m.set_focusable('focusable' in st)
    if not isinstance(m, ButtonMirror) and n.get('actions') and first:
        install_actions(m, n)


def apply_states(m, n):
    role = n.get('role', '')
    st = set(n.get('states', []))
    S = Gtk.AccessibleState
    states, vals = [], []
    if role in ('check box', 'check menu item', 'radio button', 'radio menu item', 'switch') or 'checkable' in st:
        states.append(S.CHECKED); vals.append(tri('checked' in st, 'indeterminate' in st))
    if role == 'toggle button':
        states.append(S.PRESSED); vals.append(tri('pressed' in st or 'checked' in st, 'indeterminate' in st))
    if 'expandable' in st:
        states.append(S.EXPANDED); vals.append('expanded' in st)
    if 'selectable' in st:
        states.append(S.SELECTED); vals.append('selected' in st)
    states.append(S.DISABLED); vals.append(bool(st) and 'sensitive' not in st and 'enabled' not in st)
    if 'invalid-entry' in st:
        states.append(S.INVALID); vals.append(int(Gtk.AccessibleInvalidState.TRUE))
    states.append(S.BUSY); vals.append('busy' in st)
    if 'visited' in st:
        states.append(S.VISITED); vals.append(True)
    try:
        m.update_state(states, vals)
    except Exception as ex:
        m.proxy.log('mirror: state', n.get('id'), role, repr(ex))


def install_actions(m, n):
    """The app's own AT-SPI actions as a widget action group: GTK lists a widget's actions over AT-SPI (as
    "app.<name>"), and invoking one runs the app's action of the same index."""
    g = Gio.SimpleActionGroup()
    for i, a in enumerate(n.get('actions') or []):
        name = re.sub(r'[^A-Za-z0-9.-]+', '-', a[0] or f'action{i}').strip('-.') or f'action{i}'
        if g.lookup_action(name):
            continue
        sa = Gio.SimpleAction.new(name, None)
        sa.connect('activate', lambda s, p, i=i: m.proxy.mirror_action(m, i))
        g.add_action(sa)
    m.insert_action_group('app', g)


def set_relations(m, n, by_id):
    rels, vals = [], []
    for ty, targets in (n.get('rel') or {}).items():
        g = RELATIONS.get(ty)
        if not g:
            continue
        ws = [by_id[t] for t in targets if t in by_id and by_id[t] is not m]
        if not ws:
            continue
        rels.append(getattr(Gtk.AccessibleRelation, g))
        vals.append(ws)
    if not rels:
        return
    try:
        m.update_relation(rels, vals)
    except Exception:
        try:   # PyGObject versions that cannot marshal a list into the GValue: one relation at a time, GtkAccessibleList
            for r, ws in zip(rels, vals):
                if hasattr(Gtk, 'AccessibleList'):
                    m.update_relation([r], [Gtk.AccessibleList.new_from_list(ws)])
        except Exception as ex:
            m.proxy.log('mirror: relations', n.get('id'), repr(ex))
