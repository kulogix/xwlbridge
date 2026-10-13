"""Inferred mode: an accessible structure for X apps that publish no accessibility tree (Athena/xcalc, Motif, Tk 8.6,
SDL and other self-drawn apps), from what the X server and the toolkit say without the app's help, in order of trust:

1. Xt's editres protocol (Athena, Motif and every other Xt app answer it): each widget's name, class and window ->
   role from the class, exact rectangle from the window.
2. Labels from the app's own resources: Athena buttons usually get their label from the app-defaults file
   (xcalc: `XCalc*ti.button5.label: 7`), so the resource database the toolkit itself reads (app-defaults for the
   WM_CLASS class + the display's RESOURCE_MANAGER) is queried with the widget's full name/class path. Motif
   labels default to the widget name.
3. The X window tree (every Xt and Tk widget is an X window): exact hotspot rectangles where editres is silent (Tk).
4. Pixels, last: OCR (RapidOCR via the uidet sidecar) names what nothing else names, and the lightweight detector
   (OmniParser icon_detect via the sidecar) finds controls in self-drawn areas no window covers.

Every mirror built here says how it was found in its description ("inferred from the X widget tree", "inferred
from pixels, confidence 0.83"), so a screen-reader user can tell it from what an app reports itself."""
import base64
import json
import os
import re
import select
import subprocess
import time

from Xlib import X, error as xerror, rdb

from .drawntext import DrawnText, label_of
from .editres import Editres
from .tksend import TkSend

XT_ROLES = {
    'Command': 'push button', 'Repeater': 'push button', 'MenuButton': 'push button', 'Toggle': 'toggle button',
    'Label': 'label', 'AsciiText': 'text', 'Text': 'text', 'Scrollbar': 'scroll bar', 'List': 'list',
    'SimpleMenu': 'menu', 'SmeBSB': 'menu item', 'Panner': 'slider', 'StripChart': 'image',
    'XmPushButton': 'push button', 'XmPushButtonGadget': 'push button', 'XmArrowButton': 'push button',
    'XmArrowButtonGadget': 'push button', 'XmDrawnButton': 'push button', 'XmCascadeButton': 'menu item',
    'XmCascadeButtonGadget': 'menu item', 'XmToggleButton': 'check box', 'XmToggleButtonGadget': 'check box',
    'XmLabel': 'label', 'XmLabelGadget': 'label', 'XmText': 'text', 'XmTextField': 'text', 'XmScale': 'slider',
    'XmScrollBar': 'scroll bar', 'XmList': 'list', 'XmComboBox': 'combo box', 'XmSpinBox': 'spin button',
    'XmSimpleSpinBox': 'spin button', 'XmSeparator': 'separator', 'XmSeparatorGadget': 'separator',
}
TK_ROLES = {
    'Button': 'push button', 'TButton': 'push button', 'Checkbutton': 'check box', 'TCheckbutton': 'check box',
    'Radiobutton': 'radio button', 'TRadiobutton': 'radio button', 'Entry': 'entry', 'TEntry': 'entry',
    'Spinbox': 'spin button', 'TSpinbox': 'spin button', 'Scale': 'slider', 'TScale': 'slider', 'Label': 'label',
    'TLabel': 'label', 'Message': 'label', 'Listbox': 'list', 'Text': 'text', 'Canvas': 'canvas',
    'Menubutton': 'push button', 'TMenubutton': 'push button', 'TCombobox': 'combo box', 'Scrollbar': 'scroll bar',
    'TScrollbar': 'scroll bar', 'TProgressbar': 'progress bar', 'Frame': 'panel', 'TFrame': 'panel',
    'Labelframe': 'panel', 'TLabelframe': 'panel', 'Panedwindow': 'split pane', 'TPanedwindow': 'split pane',
    'TNotebook': 'page tab list', 'Toplevel': 'panel', 'TSeparator': 'separator',
}
TK_FOCUSABLE = {'Button', 'TButton', 'Checkbutton', 'TCheckbutton', 'Radiobutton', 'TRadiobutton', 'Entry', 'TEntry',
                'Spinbox', 'TSpinbox', 'Scale', 'TScale', 'Listbox', 'Text', 'Menubutton', 'TMenubutton', 'TCombobox',
                'TNotebook'}
INTERACTIVE = {'push button', 'toggle button', 'check box', 'text', 'scroll bar', 'list', 'menu item', 'slider',
               'combo box', 'spin button'}
LABELLED = {'push button', 'toggle button', 'check box', 'label', 'menu item'}   # their drawn text is their label
NO_NAME_ROLES = {'panel', 'separator', 'scroll bar', 'list', 'menu', 'text'}
# what editres GetValues answers instead of a value: libXmu/Motif's VerifyWidget message (a widget id it could not
# match: always for Motif on 64-bit, for Athena widgets whose address has bit 31 set) and a missing resource
EDITRES_FAILURES = {'This widget no longer exists in the client.', 'NoValue'}
LABEL_RESOURCES = {'Xm': ('labelString', 'LabelString'), '': ('label', 'Label')}
APP_DEFAULTS = ('/etc/X11/app-defaults', '/usr/share/X11/app-defaults', '/usr/lib/X11/app-defaults',
                '/usr/local/share/X11/app-defaults')
# the Adobe Symbol font's encoding (labels drawn in it are Latin-1 bytes): the characters calculator-like apps use
SYMBOL = {0x22: '\u2200', 0x24: '\u2203', 0x27: '\u220b', 0x2a: '\u2217', 0x2d: '\u2212', 0x40: '\u2245',
          0x44: '\u0394', 0x46: '\u03a6', 0x47: '\u0393', 0x4c: '\u039b', 0x50: '\u03a0', 0x51: '\u0398',
          0x53: '\u03a3', 0x57: '\u03a9', 0x60: '', 0x61: '\u03b1', 0x62: '\u03b2', 0x63: '\u03c7',
          0x64: '\u03b4', 0x65: '\u03b5', 0x66: '\u03c6', 0x67: '\u03b3', 0x6c: '\u03bb', 0x6d: '\u03bc',
          0x70: '\u03c0', 0x71: '\u03b8', 0x73: '\u03c3', 0x74: '\u03c4', 0x77: '\u03c9', 0xa3: '\u2264',
          0xa5: '\u221e', 0xb1: '\u00b1', 0xb3: '\u2265', 0xb4: '\u00d7', 0xb8: '\u00f7', 0xb9: '\u2260',
          0xba: '\u2261', 0xbb: '\u2248', 0xd6: '\u221a', 0xd7: '\u22c5', 0xd8: '\u00ac', 0xd9: '\u2227',
          0xda: '\u2228', 0xe5: '\u2211', 0xf2: '\u222b'}
DISPLAY_PLACEHOLDER = re.compile(r'^(8{4,}|0{4,}|8+\.8{2,})$')   # "88888888888": a display sized for its widest content


def from_symbol_font(text):
    return ''.join(SYMBOL.get(ord(c), c) if ord(c) < 256 else c for c in text)


GENERIC_NAME = re.compile(r'^(button|btn|w|widget|item|form|box|label|text|key)?_?\d+$', re.I)


class Uidet:
    """The uidet sidecar (tools/ui_detector/sidecar): JSON-RPC lines over a command's stdin/stdout, or over two
    FIFOs (DIR/req, DIR/resp) served by a sidecar elsewhere (another container)."""

    def __init__(self, cmd=None, fifo_dir=None, log=print, timeout=30):
        self.log, self.timeout, self.n = log, timeout, 0
        self.buf = b''
        self.p = None
        if cmd:
            self.p = subprocess.Popen(cmd, shell=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                                      bufsize=1)
            self.w, self.r = self.p.stdin, self.p.stdout
        else:
            self.w = open(os.path.join(fifo_dir, 'req'), 'w', buffering=1)
            self.r = open(os.path.join(fifo_dir, 'resp'), 'r', buffering=1)
        # the sidecar may still be loading its model: don't wait for it here (the first real call waits)
        self.info = self.call('ping', _timeout=1.0) or {}
        self.log('uidet:', json.dumps(self.info)[:300] if self.info else 'not answering yet')

    def call(self, method, _timeout=None, **params):
        self.n += 1
        try:
            self.w.write(json.dumps({'jsonrpc': '2.0', 'id': self.n, 'method': method, 'params': params}) + '\n')
            self.w.flush()
            end = time.monotonic() + (_timeout or self.timeout)
            fd = self.r.fileno()
            while time.monotonic() < end:
                # lines are split here from raw reads (a buffered readline would hide a line already read from
                # the pipe from select())
                while b'\n' in self.buf:
                    line, self.buf = self.buf.split(b'\n', 1)
                    try:
                        m = json.loads(line)
                    except ValueError:
                        continue
                    if m.get('id') == self.n:
                        return m.get('result')
                ready, _, _ = select.select([fd], [], [], max(0.0, end - time.monotonic()))
                if not ready:
                    break
                chunk = os.read(fd, 1 << 20)
                if not chunk:
                    break
                self.buf += chunk
        except (OSError, ValueError) as ex:
            self.log('uidet: call failed', method, repr(ex))
        return None


def _app_resources(wm_class, root_resources):
    """The resource database an Xt app with class `wm_class` reads: its app-defaults file, then the display's
    RESOURCE_MANAGER (later entries win at equal precision, as in Xt)."""
    db = rdb.ResourceDB()
    for d in [os.environ.get('XAPPLRESDIR', '')] + list(APP_DEFAULTS):
        f = os.path.join(d, wm_class) if d else ''
        if f and os.path.isfile(f):
            try:
                with open(f, encoding='latin-1') as fh:   # Xt resource files are Latin-1 (\262 escapes too)
                    db.insert_string(fh.read())
            except Exception:
                pass
            break
    if root_resources:
        try:
            db.insert_string(root_resources)
        except Exception:
            pass
    return db


def _ocr_ok(text, score):
    return bool(text and text.strip()) and score >= 50


class Inferrer:
    def __init__(self, engine, display_name, uidet=None, log=print):
        self.engine, self.log, self.uidet = engine, log, uidet
        from Xlib import display as xdisplay
        self.d = xdisplay.Display(display_name)   # editres reads events: a connection of its own
        self.editres = Editres(self.d, log=log)
        self.drawn = DrawnText(display_name, log=log)
        self.tk = TkSend(self.d, log=log)
        self.tk_apps = {}   # X toplevel id -> (comm window, interp name, {path: node id}, alt letters)

    # ---- sources ---------------------------------------------------------------------------------------------------
    def window_rects(self, top):
        """Every mapped InputOutput descendant of `top`: id -> [x, y, w, h] relative to top."""
        out = {}
        root = self.d.create_resource_object('window', top.id)

        def walk(w, ox, oy):
            try:
                kids = w.query_tree().children
            except xerror.XError:
                return
            for c in kids:
                try:
                    a = c.get_attributes(); g = c.get_geometry()
                except xerror.XError:
                    continue
                x, y = ox + g.x, oy + g.y
                if a.map_state == X.IsViewable:
                    if a.win_class == X.InputOutput:
                        out[c.id] = [x, y, g.width, g.height]
                    walk(c, x, y)
        walk(root, 0, 0)
        return out

    def image(self, top):
        px = self.engine.read_pixels(top)
        if px is None:
            return None, None
        import gi
        gi.require_version('Gdk', '4.0')
        from gi.repository import Gdk, GLib
        w, h, depth, data = px
        fmt = Gdk.MemoryFormat.B8G8R8A8_PREMULTIPLIED if depth == 32 else Gdk.MemoryFormat.B8G8R8X8
        tex = Gdk.MemoryTexture.new(w, h, fmt, GLib.Bytes.new(data), w * 4)
        png = tex.save_to_png_bytes().get_data()
        return base64.b64encode(png).decode(), (w, h)

    def ocr_rects(self, b64, rects):
        if not self.uidet or not rects:
            return [None] * len(rects)
        r = self.uidet.call('ocr.read', png_b64=b64, rects=rects) or {}
        reads = r.get('reads', [])
        out = []
        for rect in rects:
            hit = next((x for x in reads if [round(v) for v in x['rect']] == [round(v) for v in rect]), None)
            out.append((hit['text'], hit['score']) if hit and _ocr_ok(hit['text'], hit.get('score', 0)) else None)
        return out

    # ---- the tree ----------------------------------------------------------------------------------------------
    def build(self, top, root_resources=None):
        """Reader-style nodes for X window `top` (parent ids, WINDOW boxes), every one marked inferred."""
        t0 = time.monotonic()
        rects = self.window_rects(top)
        nodes = [{'id': 1, 'parent': None, 'role': 'frame', 'name': top.title, 'states': ['showing'],
                  'box': [0, 0, top.geometry[2], top.geometry[3]], 'provenance': 'inferred'}]
        src = {'editres': False, 'tk': False, 'resources': 0, 'ocr': 0, 'detector': 0, 'windows': len(rects)}
        live = []
        tk = None if os.environ.get('X11_A11Y_PROXY_NO_TKSEND') else self.tk_nodes(top)
        tree = None if tk else self.editres.widget_tree(top.id)
        if tk:
            nodes = tk
            src['tk'] = True
        elif tree:
            widgets, toolkit = tree
            src['editres'] = True
            src.update({'getvalues': 0, 'drawn': 0, 'widget names': 0})
            db = _app_resources(top.wm_class[1], root_resources)
            names_by_id = {w['id']: w['name'] for w in widgets}
            classes_by_id = {w['id']: w['class'] for w in widgets}
            # the text each widget draws (its label, for buttons, labels and toggles): asked for once per window
            drawn = self.drawn.capture(top.id, [w['window'] for w in widgets if w['window'] in rects]) or {}
            getvalues = True   # off for this app after a Motif refusal (it can never answer on 64-bit)
            node_of = {}
            for w in widgets:
                box = rects.get(w['window'])
                role = XT_ROLES.get(w['class'])
                if box is None or (role is None and w['window'] <= 2):
                    continue
                if role is None:
                    role = 'panel'
                name, prov, conf = '', 'inferred from the X widget tree', 1.0
                live_display = False
                font = ''
                if role not in ('panel', 'separator', 'scroll bar', 'list', 'menu', 'text'):
                    xm = w['class'].startswith('Xm')
                    rn, rc = LABEL_RESOURCES['Xm' if xm else '']
                    path_n = '.'.join(names_by_id.get(i, '?') for i in w['path'])
                    path_c = '.'.join([top.wm_class[1]] + [classes_by_id.get(i, '?') for i in w['path'][1:]])
                    res = db.get(f'{path_n}.{rn}', f'{path_c}.{rc}', None)
                    res = str(res).replace('\\n', ' ').strip() if res else ''
                    font = str(db.get(f'{path_n}.font', f'{path_c}.Font', '') or '')
                    if res and DISPLAY_PLACEHOLDER.match(res):
                        # a display whose text the app sets at run time: read it live (pixels), not this
                        role, live_display = 'label', True
                    elif role in LABELLED:
                        # the label, in order of trust: the app's own value (editres GetValues), the text it draws
                        # in the widget's window, its resource files (what Xt reads unless code sets the label)
                        gv = None
                        if getvalues:
                            gv = self.editres.get_value(top.id, w['path'], rn)
                            if gv is None or gv in EDITRES_FAILURES:
                                if xm:
                                    getvalues = False
                                gv = None
                        dr = label_of(drawn.get(w['window'], []))
                        if gv:
                            name = gv.replace('\n', ' ').strip()
                            src['getvalues'] += 1
                        elif dr:
                            name = dr
                            src['drawn'] += 1
                        elif res:
                            name = res
                            src['resources'] += 1
                            prov = "inferred from the X widget tree; label from the app's resource files"
                    elif res:
                        name = res
                        src['resources'] += 1
                        prov = "inferred from the X widget tree; label from the app's resource files"
                    if name and 'symbol' in font.lower():
                        name = from_symbol_font(name)
                    if not name and not live_display:
                        prov = 'inferred from the X widget tree; label from pixels'
                parent = next((node_of[i] for i in reversed(w['path'][:-1]) if i in node_of), 1)
                n = {'id': len(nodes) + 1, 'parent': parent, 'role': role, 'name': name, 'desc': '',
                     'states': ['showing', 'visible', 'sensitive', 'enabled'] + (['focusable'] if role in INTERACTIVE
                                                                                 else []) +
                     (['editable', 'single-line'] if role == 'text' else []),
                     'box': box, 'provenance': prov, 'conf': conf, 'xwindow': w['window'], 'xclass': w['class'],
                     'xname': w['name']}
                if live_display:
                    n['states'] = ['showing', 'visible', 'sensitive', 'enabled', 'focusable']
                    n['provenance'] = 'display; text from pixels'
                    live.append(n['id'])   # xcalc's LCD: re-read when it changes
                nodes.append(n)
                node_of[w['id']] = n['id']
        else:
            # no editres (Tk, plain Xlib): leaf windows are the controls
            leaves = [wid for wid, r in rects.items()
                      if not any(o != wid and _inside(r, rr) and rr[2] * rr[3] < r[2] * r[3] for o, rr in rects.items())]
            for wid in sorted(leaves, key=lambda i: (rects[i][1], rects[i][0])):
                r = rects[wid]
                if r[2] < 6 or r[3] < 6:
                    continue
                nodes.append({'id': len(nodes) + 1, 'parent': 1, 'role': 'push button', 'name': '', 'desc': '',
                              'states': ['showing', 'visible', 'sensitive', 'enabled', 'focusable'], 'box': r,
                              'provenance': 'inferred from the X window tree', 'conf': 0.5, 'xwindow': wid})
        # pixels: OCR names for what has none; the detector for what no window covers
        unnamed = [n for n in nodes[1:] if not n['name'] and n['role'] not in ('panel', 'separator', 'scroll bar')]
        need = unnamed + [n for n in nodes if n['id'] in live]
        if self.uidet and (need or len(nodes) <= 2):
            b64, size = self.image(top)
            if b64:
                rects = [(self.ink_box(top, n['box']) or n['box']) if n['id'] in live else n['box'] for n in need]
                reads = self.ocr_rects(b64, rects)
                for i, n in enumerate(need):
                    if reads[i] is None and n['id'] in live:
                        z = self.ocr_zoomed(top, rects[i])
                        reads[i] = (z, 90.0) if z else None
                for n, rd in zip(need, reads):
                    if rd:
                        n['name'] = rd[0].strip()
                        n['conf'] = round(min(n.get('conf', 1.0), rd[1] / 100), 2)
                        n['provenance'] = f'{n["provenance"].split(";")[0]}; label from pixels, confidence {n["conf"]:.2f}' \
                            if n['provenance'].startswith('inferred from the X') else \
                            f'inferred from pixels, confidence {n["conf"]:.2f}'
                        src['ocr'] += 1
                covered = [n['box'] for n in nodes[1:]]
                if len(nodes) <= 2 or any(n['role'] == 'panel' for n in nodes[1:]):
                    det = self.uidet.call('ui.parse', png_b64=b64, imgsz=640) or {}
                    for e in det.get('elements', []):
                        r = [round(v) for v in e['rect']]
                        if any(_inside(c, r) and c[2] * c[3] < 4 * r[2] * r[3] for c in covered if c[2] * c[3] <
                               size[0] * size[1] * 0.5):
                            continue   # a widget window already stands for it
                        conf = e.get('conf') or 0
                        if conf < 0.5 and not e.get('text'):
                            continue
                        nodes.append({'id': len(nodes) + 1, 'parent': 1, 'role': 'push button',
                                      'name': (e.get('text') or '').strip(), 'desc': '',
                                      'states': ['showing', 'visible', 'sensitive', 'enabled', 'focusable'], 'box': r,
                                      'provenance': f'inferred from pixels, confidence {conf:.2f}', 'conf': conf})
                        src['detector'] += 1
        for n in nodes[1:]:
            if not n['name'] and n.get('xname') and n['id'] not in live and n['role'] not in NO_NAME_ROLES and \
                    (n['xclass'].startswith('Xm') or not GENERIC_NAME.match(n['xname'])):
                # nothing reported or drawn a label (Xft text, no OCR sidecar): Motif's labelString defaults to the
                # widget's name, which is often all there is -- but code may have set another label: say so
                n['name'] = n['xname']
                n['provenance'] = "inferred from the X widget tree; label not reported, the widget's name"
                src['widget names'] += 1
            if not n['name'] and n['role'] in ('push button', 'toggle button', 'check box', 'menu item'):
                n['name'] = 'unlabelled control'
        self.log(f'inferred: 0x{top.id:x} {top.title!r}: {len(nodes) - 1} nodes {src} '
                 f'in {round((time.monotonic() - t0) * 1000)} ms')
        return nodes, live, src

    # ---- Tk ----------------------------------------------------------------------------------------------------
    def tk_nodes(self, top):
        """Nodes for a Tk app's window from the interpreter itself (Tk's send), or None if it is not Tk."""
        found = self.tk_apps.get(top.id)
        if found is None:
            try:
                kids = [c.id for c in self.d.create_resource_object('window', top.id).query_tree().children]
            except xerror.XError:
                kids = []
            it = self.tk.interp_for(top.id, kids)
            if it is None:
                self.log(f'tk: 0x{top.id:x} is no Tk interpreter\'s window (interps: {self.tk.interps()})')
                return None
            self.log(f'tk: 0x{top.id:x} belongs to interpreter {it[1]!r} (comm window 0x{it[0]:x})')
            found = self.tk_apps[top.id] = (it[0], it[1], {}, self.tk.bound_alt_keys(*it))
        comm, name, ids, alt = found
        r = self.tk.walk(comm, name)
        if r is None:
            return None
        focus, widgets = r
        ox, oy = self.engine.frame_origin(top.id)
        nodes = [{'id': 1, 'parent': None, 'role': 'frame', 'name': top.title, 'states': ['showing'],
                  'box': [0, 0, top.geometry[2], top.geometry[3]], 'provenance': 'from Tk introspection'}]
        by_path = {}
        for w in widgets:
            if w['path'] == '.' or not w['mapped']:
                continue
            role = TK_ROLES.get(w['class'], 'panel')
            nid = ids.setdefault(w['path'], len(ids) + 2)
            st = ['showing', 'visible']
            if w.get('-state', 'normal') != 'disabled':
                st += ['sensitive', 'enabled']
            if w['class'] in TK_FOCUSABLE and w.get('-takefocus', '') not in ('0',):
                st.append('focusable')
            if w['path'] == focus:
                st.append('focused')
            if role == 'check box':
                on = w.get('-onvalue', '1')
                if w.get('varvalue') == on:
                    st.append('checked')
            if role == 'radio button' and w.get('varvalue') is not None and w.get('varvalue') == w.get('-value'):
                st.append('checked')
            if role in ('entry', 'text', 'spin button', 'combo box'):
                st.append('editable')
                st.append('single-line' if role != 'text' else 'multi-line')
            if w.get('-orient') in ('horizontal', 'vertical'):
                st.append(w['-orient'])
            name = w.get('-text', '') if role in ('push button', 'check box', 'radio button', 'label') else \
                w.get('-label', '')
            n = {'id': nid, 'parent': by_path.get(w['path'].rsplit('.', 1)[0] or '.', 1), 'role': role,
                 'name': name, 'desc': '', 'states': st,
                 'box': [w['rx'] - ox, w['ry'] - oy, w['w'], w['h']],
                 'provenance': 'from Tk introspection', 'conf': 1.0, 'tkpath': w['path'], 'xclass': w['class']}
            ul = w.get('-underline', '')
            if ul.lstrip('-').isdigit() and int(ul) >= 0 and int(ul) < len(name) and name[int(ul)].lower() in alt:
                n['actions'] = [['click', f'<Alt>{name[int(ul)].lower()}', '']]
            elif role in ('push button', 'check box', 'radio button'):
                n['actions'] = [['click', '', '']]
            if role == 'slider' and w.get('value'):
                try:
                    n['value'] = [float(w['value']), float(w.get('-from', 0)), float(w.get('-to', 100)), 1.0]
                except ValueError:
                    pass
            if role in ('entry', 'text', 'spin button', 'combo box'):
                n['text'] = w.get('value', '')
                n['caret'] = int(w['cursor']) if w.get('cursor', '').isdigit() else len(n['text'])
            nodes.append(n)
            by_path[w['path']] = nid
        return nodes

    def tk_invoke(self, top, node):
        found = self.tk_apps.get(top.id)
        if found and node.get('tkpath'):
            return self.tk.invoke(found[0], found[1], node['tkpath'])
        return None

    def ink_box(self, top, box, pad=3):
        """The part of `box` that differs from its background (the text a display shows), padded: a recogniser
        reads one short word far better from a tight crop than from a mostly empty display."""
        px = self.engine.read_pixels(top)
        if px is None:
            return box
        w, h, depth, data = px
        x0, y0, bw, bh = [int(v) for v in box]
        x1, y1 = min(w, x0 + bw), min(h, y0 + bh)
        x0, y0 = max(0, x0), max(0, y0)

        def lum(x, y):
            i = (y * w + x) * 4
            return data[i] * 114 + data[i + 1] * 587 + data[i + 2] * 299
        corners = sorted(lum(x, y) for x, y in ((x0 + 2, y0 + 2), (x1 - 3, y0 + 2), (x0 + 2, y1 - 3), (x1 - 3, y1 - 3)))
        bg = corners[1]
        xs, ys = [], []
        for y in range(y0 + 1, y1 - 1):
            for x in range(x0 + 1, x1 - 1):
                if abs(lum(x, y) - bg) > 60000:
                    xs.append(x); ys.append(y)
        if not xs:
            return None   # blank
        return [max(0, min(xs) - pad), max(0, min(ys) - pad), max(xs) - min(xs) + 1 + 2 * pad,
                max(ys) - min(ys) + 1 + 2 * pad]

    def ocr_zoomed(self, top, box, zoom=4, pad=8):
        """OCR of `box` cut out, padded with its background and enlarged: a recogniser misses single digits drawn
        small (xcalc's display); four times larger and with a margin it reads them."""
        import gi
        gi.require_version('GdkPixbuf', '2.0')
        from gi.repository import GdkPixbuf, GLib
        px = self.engine.read_pixels(top)
        if px is None:
            return None
        w, h, depth, data = px
        x0, y0, bw, bh = [int(v) for v in box]
        x0, y0 = max(0, x0), max(0, y0)
        bw, bh = min(bw, w - x0), min(bh, h - y0)
        if bw <= 0 or bh <= 0:
            return None
        rgb = bytearray()
        bg = data[(y0 * w + x0) * 4:(y0 * w + x0) * 4 + 3]
        W, H = bw + 2 * pad, bh + 2 * pad
        for y in range(H):
            for x in range(W):
                sx, sy = x0 + x - pad, y0 + y - pad
                if 0 <= x - pad < bw and 0 <= y - pad < bh:
                    i = (sy * w + sx) * 4
                    rgb += bytes((data[i + 2], data[i + 1], data[i]))
                else:
                    rgb += bytes((bg[2], bg[1], bg[0]))
        pb = GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(bytes(rgb)), GdkPixbuf.Colorspace.RGB, False, 8, W, H, W * 3)
        pb = pb.scale_simple(W * zoom, H * zoom, GdkPixbuf.InterpType.BILINEAR)
        ok, png = pb.save_to_bufferv('png', [], [])
        if not ok:
            return None
        r = self.uidet.call('ocr.read', png_b64=base64.b64encode(png).decode()) or {}
        reads = [x for x in r.get('reads', []) if _ocr_ok(x.get('text'), x.get('score', 0))]
        return ' '.join(x['text'] for x in sorted(reads, key=lambda x: x['rect'][0])).strip() or None

    def reread(self, top, node):
        """A live label's text now (OCR of its rectangle), or None."""
        if not self.uidet:
            return None
        b64, _ = self.image(top)
        if not b64:
            return None
        ink = self.ink_box(top, node['box'])
        rd = self.ocr_rects(b64, [ink])[0] if ink else None
        if rd is None and ink:
            txt = self.ocr_zoomed(top, ink)
            if txt:
                return txt
        if rd is None:
            # a display's digits are often too few for the recogniser on a tight crop: read the whole window and
            # take the lines inside the box
            r = self.uidet.call('ocr.read', png_b64=b64) or {}
            inside = [x for x in r.get('reads', []) if _inside(node['box'], [round(v) for v in x['rect']])]
            if inside:
                return ' '.join(x['text'] for x in sorted(inside, key=lambda x: x['rect'][0])).strip()
        return rd[0].strip() if rd else None


def _inside(outer, inner):
    cx, cy = inner[0] + inner[2] / 2, inner[1] + inner[3] / 2
    return outer[0] <= cx < outer[0] + outer[2] and outer[1] <= cy < outer[1] + outer[3]
