#!/usr/bin/env python3
"""Unit tests (no display needed): XSETTINGS encoding, the settings mapping, key-binding mapping to what GTK 4 can
express, the editres stream parser, the Symbol-font mapping. Run: python3 -m unittest tests/test_unit.py"""
import os
import struct
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import gi  # noqa: E402
gi.require_version('Gtk', '4.0')

from x11_a11y_proxy import settings as S  # noqa: E402
from x11_a11y_proxy import editres as ER  # noqa: E402
from x11_a11y_proxy import mirror as M  # noqa: E402
from x11_a11y_proxy import inferred as INF  # noqa: E402
from x11_a11y_proxy import drawntext as DT  # noqa: E402


class XSettings(unittest.TestCase):
    def test_roundtrip(self):
        v = {'Net/ThemeName': 'HighContrast', 'Xft/DPI': 147456, 'Gtk/EnableAnimations': 0, 'X/Color': (1, 2, 3, 4)}
        b = S.encode_xsettings(v, 7, {'Xft/DPI': 7})
        self.assertEqual(S.decode_xsettings(b), v)
        self.assertEqual(struct.unpack('=I', b[4:8])[0], 7)

    def test_gnome_mapping(self):
        g = {'org.gnome.desktop.a11y.interface:high-contrast': True,
             'org.gnome.desktop.a11y.interface:reduced-motion': 'reduce',
             'org.gnome.desktop.interface:text-scaling-factor': 1.5,
             'org.gnome.desktop.interface:cursor-size': 48,
             'org.gnome.desktop.interface:enable-animations': True}
        xs, res = S.derive(g, core_font=lambda px: f'-x-sans-medium-r-normal--{px}-*-*-*-p-*-iso8859-1')
        self.assertEqual(xs['Net/ThemeName'], 'HighContrast')
        self.assertEqual(xs['Xft/DPI'], 144 * 1024)
        self.assertEqual(xs['Gtk/CursorThemeSize'], 48)
        self.assertEqual(xs['Gtk/EnableAnimations'], 0)
        self.assertEqual(res['Xft.dpi'], '144')
        self.assertEqual(res['Xcursor.size'], '48')
        self.assertEqual(res['*background'], 'white')
        self.assertIn('--20-', res['*font'])

    def test_reduced_motion_is_an_enum(self):
        xs, _ = S.derive({'org.gnome.desktop.a11y.interface:reduced-motion': 'no-preference',
                          'org.gnome.desktop.interface:enable-animations': True})
        self.assertEqual(xs['Gtk/EnableAnimations'], 1)

    def test_defaults_add_no_xt_colours(self):
        _, res = S.derive({})
        self.assertNotIn('*background', res)
        self.assertEqual(res['*customization'], '-color')


class GsdKeySet(unittest.TestCase):
    # the 53 keys gsd-xsettings published on GNOME 50's Xwayland (measured)
    GSD = ('Gdk/UnscaledDPI Gdk/WindowScalingFactor Gtk/AutoMnemonics Gtk/ButtonImages Gtk/CanChangeAccels '
           'Gtk/ColorPalette Gtk/ColorScheme Gtk/CursorBlinkTimeout Gtk/CursorThemeName Gtk/CursorThemeSize '
           'Gtk/DecorationLayout Gtk/DialogsUseHeader Gtk/EnablePrimaryPaste Gtk/FontName Gtk/IMModule '
           'Gtk/IMPreeditStyle Gtk/IMStatusStyle Gtk/KeyThemeName Gtk/KeynavUseCaret Gtk/MenuBarAccel Gtk/MenuImages '
           'Gtk/Modules Gtk/OverlayScrolling Gtk/RecentFilesEnabled Gtk/RecentFilesMaxAge Gtk/SessionBusId '
           'Gtk/ShellShowsAppMenu Gtk/ShellShowsDesktop Gtk/ShowInputMethodMenu Gtk/ShowStatusShapes '
           'Gtk/ShowUnicodeMenu Gtk/TimeoutInitial Gtk/TimeoutRepeat Gtk/TitlebarDoubleClick Gtk/TitlebarMiddleClick '
           'Gtk/TitlebarRightClick Gtk/ToolbarIconSize Gtk/ToolbarStyle Net/CursorBlink Net/CursorBlinkTime '
           'Net/DndDragThreshold Net/DoubleClickTime Net/EnableEventSounds Net/EnableInputFeedbackSounds '
           'Net/FallbackIconTheme Net/IconThemeName Net/SoundThemeName Net/ThemeName Xft/Antialias Xft/DPI '
           'Xft/HintStyle Xft/Hinting Xft/RGBA').split()

    def test_same_keys_plus_enable_animations(self):
        xs, _ = S.derive({'session-bus-id': 'x'})
        self.assertEqual(len(self.GSD), 53)
        self.assertEqual(set(xs) - set(self.GSD), {'Gtk/EnableAnimations'})
        self.assertEqual(set(self.GSD) - set(xs), set())

    def test_gsd_translations(self):
        g = {'org.gnome.desktop.wm.preferences:button-layout': 'appmenu:close',
             'org.gnome.desktop.sound:event-sounds': True, 'org.gnome.desktop.interface:gtk-im-module': '',
             'org.gnome.settings-daemon.plugins.xsettings:overrides': {'Gtk/ShowUnicodeMenu': 1}}
        xs, _ = S.derive(g)
        self.assertEqual(xs['Gtk/DecorationLayout'], 'menu:close')
        self.assertEqual(xs['Net/EnableEventSounds'], 1)
        self.assertEqual(xs['Gtk/IMModule'], 'ibus')
        self.assertEqual(xs['Gtk/ShowUnicodeMenu'], 1)


class Keys(unittest.TestCase):
    def node(self, role, binding='', attr=''):
        return {'role': role, 'actions': [['click', binding, '']], 'attrs': {'keyshortcuts': attr} if attr else {}}

    def test_gtk3_button(self):        # "<Alt>s": spoken as the accelerator of a push button
        self.assertEqual(M.key_config(self.node('push button', '<Alt>s'), True), ('Alt+S', None))

    def test_qt_check_box(self):       # "Alt+C": the letter as mnemonic (GTK 4 cannot carry "Alt+C" there)
        self.assertEqual(M.key_config(self.node('check box', 'Alt+C'), True), (None, 'C'))

    def test_java_button(self):        # "S;;"
        self.assertEqual(M.key_config(self.node('push button', 'S;;'), True), ('Alt+S', None))

    def test_chromium(self):           # binding "s", keyshortcuts "Alt+s"
        self.assertEqual(M.key_config(self.node('push button', 's', 'Alt+s'), True), (None, 'S'))

    def test_menu_accelerator(self):   # GTK 3 menu item "<Alt>o;;<Control>o"
        k, letter = M.key_config(self.node('menu item', '<Alt>o;;<Control>o'), True)
        self.assertEqual(k, 'Ctrl+O')

    def test_text_field(self):
        self.assertEqual(M.key_config(self.node('text', '<Alt>n'), False), ('Alt+N', None))


class Editres(unittest.TestCase):
    def test_widget_tree_reply(self):
        def s8(t):
            return struct.pack('>H', len(t)) + t.encode()
        body = struct.pack('>H', 2)
        body += struct.pack('>HI', 1, 0x10) + s8('xcalc') + s8('XCalc') + struct.pack('>I', 0x200019)
        body += struct.pack('>HII', 2, 0x10, 0x20) + s8('button1') + s8('Command') + struct.pack('>I', 0x20002d)
        body += s8('Xt')
        s = ER.Stream(body)
        n = s.u16()
        got = [(s.path(), s.s8(), s.s8(), s.u32()) for _ in range(n)]
        self.assertEqual(got[1], ([0x10, 0x20], 'button1', 'Command', 0x20002d))
        self.assertEqual(s.s8(), 'Xt')

    def test_get_values_request(self):
        req = ER.put_s8('label') + struct.pack('>H', 1) + ER.put_path([0x10, 0x20])
        self.assertEqual(req[:7], b'\x00\x05label')


class Inferred(unittest.TestCase):
    def test_symbol_font(self):
        self.assertEqual(INF.from_symbol_font('\xd6\x60'), '√')
        self.assertEqual(INF.from_symbol_font('\x70'), 'π')

    def test_display_placeholder(self):
        self.assertTrue(INF.DISPLAY_PLACEHOLDER.match('88888888888'))
        self.assertFalse(INF.DISPLAY_PLACEHOLDER.match('AC'))


class DrawnText(unittest.TestCase):
    """Core text requests as RECORD delivers them (client byte order, little-endian here)."""

    def test_poly_text8_and_image_text8(self):
        def pad(b):
            return b + b'\0' * ((4 - len(b) % 4) % 4)
        items = pad(bytes([5, 0]) + b'Apply')
        poly = struct.pack('<BBH', 74, 0, (16 + len(items)) // 4) + struct.pack('<IIhh', 0x60001d, 7, 23, 17) + items
        img_s = pad(b'Verbose')
        img = struct.pack('<BBH', 76, 7, (16 + len(img_s)) // 4) + struct.pack('<IIhh', 0x60001c, 7, 21, 17) + img_s
        noise = struct.pack('<BBH', 61, 0, 4) + b'\0' * 12      # ClearArea: skipped
        got = DT.parse_requests(noise + poly + img)
        self.assertEqual([(g[1], g[4]) for g in got], [(0x60001d, 'Apply'), (0x60001c, 'Verbose')])

    def test_poly_text_font_change_and_two_items(self):
        items = bytes([3, 0]) + b'Run' + bytes([255]) + struct.pack('>I', 9) + bytes([4, 2]) + b' now'
        items += b'\0' * ((4 - len(items) % 4) % 4)
        req = struct.pack('<BBH', 74, 0, (16 + len(items)) // 4) + struct.pack('<IIhh', 5, 7, 1, 2) + items
        self.assertEqual(DT.parse_requests(req)[0][4], 'Run now')

    def test_label_of_keeps_one_run_per_position(self):
        self.assertEqual(DT.label_of([(23, 17, 'Seven'), (23, 17, 'Seven')]), 'Seven')
        self.assertEqual(DT.label_of([(40, 17, 'B'), (3, 17, 'A')]), 'A B')


class Provenance(unittest.TestCase):
    def test_marks(self):
        self.assertEqual(M.provenance_mark('from Tk introspection'), '')
        self.assertEqual(M.provenance_mark('inferred from the X widget tree'), '')
        self.assertEqual(M.provenance_mark("inferred from the X widget tree; label not reported, the widget's name"),
                         "label not reported, the widget's name")
        self.assertEqual(M.provenance_mark("inferred from the X widget tree; label from the app's resource files"),
                         "label from the app's resource files")
        self.assertEqual(M.provenance_mark('inferred from pixels, confidence 0.90'), 'inferred from pixels, confidence 0.90')
        self.assertEqual(M.provenance_mark('inferred from the X window tree'), 'inferred from the X window tree')


class ReaderSentNodes(unittest.TestCase):
    """The reader sends a node's subtree before its first focus event unless the proxy has the node -- not merely
    because the node has an id (an earlier event gives it one). Electron replaces its document while loading; the new
    document's first event was 'busy' off, then its focus: the page never reached the mirrors (proxy_fix.md)."""

    def setUp(self):
        gi.require_version('Atspi', '2.0')
        from gi.repository import Atspi
        from x11_a11y_proxy import reader as R
        self.R, self.Atspi = R, Atspi
        self.lines = []
        R.out = lambda **k: self.lines.append(k)

    def fake(self, role, name='', parent=None, kids=()):
        Atspi = self.Atspi

        class SS:
            def __init__(s, st):
                s.st = st

            def contains(s, x):
                return x in s.st

        class O:
            def __init__(s):
                s.role, s.name, s.parent, s.kids = role, name, parent, list(kids)
                s.states = {Atspi.StateType.SHOWING, Atspi.StateType.FOCUSABLE}

            def get_role_name(s): return s.role
            def get_name(s): return s.name
            def get_description(s): return ''
            def get_parent(s): return s.parent
            def get_index_in_parent(s): return s.parent.kids.index(s) if s.parent else 0
            def get_child_count(s): return len(s.kids)
            def get_child_at_index(s, i): return s.kids[i]
            def get_state_set(s): return SS(s.states)
            def get_extents(s, ct): raise RuntimeError('no extents')
            def get_action_iface(s): return None
            def get_value_iface(s): return None
            def get_text_iface(s): return None
            def get_attributes(s): return {}
            def get_relation_set(s): return []
        return O()

    def ev(self, source, ty, d1=0, data=None):
        class E:
            pass
        e = E()
        e.source, e.type, e.detail1, e.any_data = source, ty, d1, data
        return e

    def test_focus_after_busy_sends_the_new_document(self):
        app = self.fake('application', 'electron')
        frame = self.fake('frame', 'Electron probe P', parent=app)
        old_doc = self.fake('document web', '', parent=frame)
        frame.kids = [old_doc]
        r = self.R.Reader()
        wid = r.nid(frame)
        r.window_of[wid] = wid
        r.send_tree(wid)                       # the proxy gets the window with the blank document
        new_doc = self.fake('document web', 'Electron probe', parent=frame)
        frame.kids = [new_doc]                 # Chromium replaced it while loading
        self.lines.clear()
        r._on_event(self.ev(new_doc, 'object:state-changed:busy', 0))
        r._on_event(self.ev(new_doc, 'object:state-changed:focused', 1))
        kinds = [(x.get('t'), x.get('kind'), x.get('root')) for x in self.lines]
        doc = r.ids[new_doc]
        self.assertIn(('tree', None, doc), kinds)
        self.assertLess(kinds.index(('tree', None, doc)), kinds.index(('ev', 'focus', None)))

    def test_focus_on_a_sent_node_sends_no_tree(self):
        app = self.fake('application', 'gtk')
        frame = self.fake('frame', 'w', parent=app)
        b = self.fake('push button', 'Seven', parent=frame)
        frame.kids = [b]
        r = self.R.Reader()
        wid = r.nid(frame)
        r.window_of[wid] = wid
        r.send_tree(wid)
        self.lines.clear()
        r._on_event(self.ev(b, 'object:state-changed:focused', 1))
        self.assertEqual([x.get('t') for x in self.lines], ['ev'])


class TextVfuncShapes(unittest.TestCase):
    """PyGObject wants the out array's length too from a Python vfunc (measured in the parity tests)."""
    def test_selection(self):
        import types
        ok, n, ranges = M.TextMirror.do_get_selection(types.SimpleNamespace(sel=[(1, 3)]))
        self.assertEqual((ok, n, ranges[0].start, ranges[0].length), (True, 1, 1, 2))
        self.assertEqual(M.TextMirror.do_get_selection(types.SimpleNamespace(sel=[])), (False, 0, []))

    def test_attributes(self):
        self.assertEqual(len(M.TextMirror.do_get_attributes(None, 0)), 5)


if __name__ == '__main__':
    unittest.main()
