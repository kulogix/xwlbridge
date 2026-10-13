"""GNOME's appearance and accessibility settings mirrored into the engine display.

On GNOME's own Xwayland, gnome-settings-daemon's xsettings plugin owns `_XSETTINGS_S0` and writes `Xft.dpi` and
friends into the root's RESOURCE_MANAGER; the engine display has neither, so X apps there ignore high contrast,
large text, the cursor size and reduced motion. This module is that daemon for
the engine:

- XSETTINGS (the freedesktop spec; GTK 3/4, Java's gnome.* desktop properties, Chromium/Electron through GTK, and
  Qt's GTK platform theme read it): Net/ThemeName (HighContrast when high contrast is on, as gsd-xsettings does),
  Net/IconThemeName, Gtk/FontName, Xft/DPI (96 x text-scaling-factor, x1024), Xft/Antialias/Hinting/HintStyle/RGBA,
  Gtk/CursorThemeName, Gtk/CursorThemeSize, Gtk/EnableAnimations (enable-animations and not reduced-motion),
  Net/CursorBlink(Time), Gdk/WindowScalingFactor, Gdk/UnscaledDPI, Gtk/ColorScheme-ish (prefer dark via
  Gtk/ApplicationPreferDarkTheme is not an XSETTING, so color-scheme goes through the Settings portal only).
- RESOURCE_MANAGER on the engine's root (Xlib resources: Xft.dpi for Qt and Xft-using toolkits, Xcursor.size and
  Xcursor.theme for libXcursor, which every Xlib app's cursors go through), and with high contrast or large text
  generic Xt/Tk resources (*background, *foreground, *font ...) that Athena, Motif and Tk apps read at start.
  The root's previous RESOURCE_MANAGER is restored when the mirror stops.

Settings are read with Gio.Settings and followed live (changed signals). Which toolkit picks up what is measured by
tests/settings.sh (settings carry-over)."""
import os
import struct
import subprocess
import sys

from gi.repository import Gio, GLib
from Xlib import X, Xatom, display as xdisplay, error as xerror
from Xlib.protocol import event as xevent

IFACE = 'org.gnome.desktop.interface'
A11Y = 'org.gnome.desktop.a11y.interface'
A11Y_ROOT = 'org.gnome.desktop.a11y'
WM = 'org.gnome.desktop.wm.preferences'
PERIPH_MOUSE = 'org.gnome.desktop.peripherals.mouse'
SOUND = 'org.gnome.desktop.sound'
PRIVACY = 'org.gnome.desktop.privacy'
BACKGROUND = 'org.gnome.desktop.background'
XS_PLUGIN = 'org.gnome.settings-daemon.plugins.xsettings'
# gsd-xsettings' fixed entries (GNOME 50; measured)
GSD_FIXED = {
    'Gtk/MenuImages': 0, 'Gtk/ButtonImages': 0, 'Gtk/ShowInputMethodMenu': 0, 'Gtk/ShowUnicodeMenu': 0,
    'Gtk/AutoMnemonics': 1, 'Gtk/DialogsUseHeader': 1, 'Gtk/ShellShowsAppMenu': 0, 'Gtk/CanChangeAccels': 0,
    'Gtk/ColorPalette': 'black:white:gray50:red:purple:blue:light blue:green:yellow:orange:lavender:brown:'
                        'goldenrod4:dodger blue:pink:light green:gray10:gray30:gray75:gray90',
    'Net/FallbackIconTheme': 'gnome', 'Gtk/ToolbarStyle': 'both-horiz', 'Gtk/ToolbarIconSize': 'large',
    'Gtk/TimeoutInitial': 200, 'Gtk/TimeoutRepeat': 20, 'Gtk/IMPreeditStyle': 'callback',
    'Gtk/IMStatusStyle': 'callback', 'Gtk/MenuBarAccel': 'F10',
}

XSETTINGS_INT, XSETTINGS_STRING, XSETTINGS_COLOR = 0, 1, 2
# the engine's RESOURCE_MANAGER as it was before the mirror changed it, on the root while a mirror runs: one byte
# (1 = there was one, 0 = there was none) and the old value. A mirror that did not restore it (crash, SIGKILL) left
# its high-contrast resources to every Xt/Tk app started later; the cleanup process
# (cleanup.py) restores it the moment the proxy dies, and the next mirror on start if that did not happen either.
SAVED_RM = '_X11_A11Y_PROXY_SAVED_RM'


def _schema(name):
    src = Gio.SettingsSchemaSource.get_default()
    return Gio.Settings.new(name) if src and src.lookup(name, True) else None


def encode_xsettings(settings, serial, last_change):
    """The _XSETTINGS_SETTINGS property bytes (native byte order) for {name: int|str|(r,g,b,a)}."""
    order = 0 if struct.pack('=H', 1) == b'\x01\x00' else 1   # LSBFirst = 0, MSBFirst = 1
    out = [struct.pack('=B3xII', order, serial, len(settings))]
    for name in sorted(settings):
        v = settings[name]
        nb = name.encode()
        pad = (4 - len(nb) % 4) % 4
        if isinstance(v, bool) or isinstance(v, int):
            t = XSETTINGS_INT
        elif isinstance(v, tuple):
            t = XSETTINGS_COLOR
        else:
            t = XSETTINGS_STRING
        out.append(struct.pack('=BxH', t, len(nb)) + nb + b'\0' * pad + struct.pack('=I', last_change.get(name, 0)))
        if t == XSETTINGS_INT:
            out.append(struct.pack('=i', int(v)))
        elif t == XSETTINGS_STRING:
            vb = str(v).encode()
            out.append(struct.pack('=I', len(vb)) + vb + b'\0' * ((4 - len(vb) % 4) % 4))
        else:
            out.append(struct.pack('=HHHH', *v))
    return b''.join(out)


def decode_xsettings(data):
    """{name: value} from _XSETTINGS_SETTINGS bytes (for tests and dumps)."""
    order = '<' if data[0] == 0 else '>'
    serial, n = struct.unpack(order + 'II', data[4:12])
    off, out = 12, {}
    for _ in range(n):
        t, nlen = struct.unpack(order + 'BxH', data[off:off + 4]); off += 4
        name = data[off:off + nlen].decode(); off += nlen + (4 - nlen % 4) % 4
        off += 4   # last-change serial
        if t == XSETTINGS_INT:
            out[name] = struct.unpack(order + 'i', data[off:off + 4])[0]; off += 4
        elif t == XSETTINGS_STRING:
            vlen = struct.unpack(order + 'I', data[off:off + 4])[0]; off += 4
            out[name] = data[off:off + vlen].decode(); off += vlen + (4 - vlen % 4) % 4
        else:
            out[name] = struct.unpack(order + 'HHHH', data[off:off + 8]); off += 8
    return out


class GnomeSettings:
    """GNOME's current appearance/accessibility settings (Gio.Settings), as one dict, with a change callback."""
    KEYS = {
        IFACE: ('gtk-theme', 'icon-theme', 'cursor-theme', 'cursor-size', 'font-name', 'monospace-font-name',
                'text-scaling-factor', 'enable-animations', 'cursor-blink', 'cursor-blink-time',
                'font-antialiasing', 'font-hinting', 'font-rgba-order', 'color-scheme', 'scaling-factor',
                'gtk-key-theme', 'overlay-scrolling', 'cursor-blink-timeout', 'gtk-enable-primary-paste',
                'gtk-color-scheme'),
        A11Y: ('high-contrast', 'reduced-motion', 'show-status-shapes'),
        A11Y_ROOT: ('always-show-text-caret',),
        WM: ('button-layout', 'action-double-click-titlebar', 'action-middle-click-titlebar',
             'action-right-click-titlebar'),
        PERIPH_MOUSE: ('double-click', 'drag-threshold'),
        SOUND: ('theme-name', 'event-sounds', 'input-feedback-sounds'),
        PRIVACY: ('remember-recent-files', 'recent-files-max-age'),
        BACKGROUND: ('show-desktop-icons',),
        XS_PLUGIN: ('overrides',),
    }

    def __init__(self, on_change=None):
        self.objs = {}
        for schema, keys in self.KEYS.items():
            s = _schema(schema)
            if s is None:
                continue
            self.objs[schema] = s
            if on_change:
                s.connect('changed', lambda *_: on_change())

    def get(self):
        v = {}
        for schema, keys in self.KEYS.items():
            s = self.objs.get(schema)
            if s is None:
                continue
            have = set(s.props.settings_schema.list_keys())
            for k in keys:
                if k in have:
                    v[f'{schema}:{k}'] = s.get_value(k).unpack()
        return v


def derive(g, core_font=None):
    """(xsettings dict, Xlib resources dict) from GNOME's settings, the way gsd-xsettings maps them, plus
    Xt/Tk resources for high contrast and large text. `core_font(px)` names a scalable core (XLFD) font of that
    pixel size that the engine's X server has (Athena and Tk accept XLFD names), or None."""
    hc = bool(g.get(f'{A11Y}:high-contrast', False))
    rm = g.get(f'{A11Y}:reduced-motion', 'no-preference')   # GNOME 50: an enum, 'no-preference' | 'reduce'
    reduced = rm == 'reduce' or rm is True
    scaling = float(g.get(f'{IFACE}:text-scaling-factor', 1.0) or 1.0)
    win_scale = int(g.get(f'{IFACE}:scaling-factor', 0) or 0) or 1
    dpi = 96.0 * scaling
    cursor_size = int(g.get(f'{IFACE}:cursor-size', 24) or 24)
    cursor_theme = g.get(f'{IFACE}:cursor-theme', 'Adwaita') or 'Adwaita'
    theme = g.get(f'{IFACE}:gtk-theme', 'Adwaita') or 'Adwaita'
    dark = g.get(f'{IFACE}:color-scheme', 'default') == 'prefer-dark'
    if hc:
        theme = 'HighContrastInverse' if dark else 'HighContrast'
    font = g.get(f'{IFACE}:font-name', 'Sans 11') or 'Sans 11'
    aa = g.get(f'{IFACE}:font-antialiasing', 'grayscale')
    hint = g.get(f'{IFACE}:font-hinting', 'slight')
    rgba = g.get(f'{IFACE}:font-rgba-order', 'rgb')
    anim = bool(g.get(f'{IFACE}:enable-animations', True)) and not reduced
    layout = g.get(f'{WM}:button-layout', 'appmenu:close') or 'appmenu:close'
    # gsd: GNOME has no app menu any more -- its place in the layout is the window menu
    layout = ':'.join(','.join('menu' if b == 'appmenu' else b for b in side.split(',')) for side in layout.split(':'))
    xs = dict(GSD_FIXED)
    xs.update({
        'Net/ThemeName': theme,
        'Net/IconThemeName': g.get(f'{IFACE}:icon-theme') or 'Adwaita',
        'Net/CursorBlink': int(bool(g.get(f'{IFACE}:cursor-blink', True))),
        'Net/CursorBlinkTime': int(g.get(f'{IFACE}:cursor-blink-time', 1200) or 1200),
        'Net/DoubleClickTime': int(g.get(f'{PERIPH_MOUSE}:double-click', 400) or 400),
        'Net/DndDragThreshold': int(g.get(f'{PERIPH_MOUSE}:drag-threshold', 8) or 8),
        'Net/EnableEventSounds': int(bool(g.get(f'{SOUND}:event-sounds', True))),
        'Net/EnableInputFeedbackSounds': int(bool(g.get(f'{SOUND}:input-feedback-sounds', False))),
        'Net/SoundThemeName': g.get(f'{SOUND}:theme-name', 'freedesktop') or 'freedesktop',
        'Gtk/FontName': font,
        'Gtk/CursorThemeName': cursor_theme,
        'Gtk/CursorThemeSize': cursor_size * win_scale,
        # not published by gsd-xsettings in the measured GNOME 50 bed: the engine's addition, so reduced motion
        # reaches GTK apps (the only key here that GNOME's Xwayland does not have)
        'Gtk/EnableAnimations': int(anim),
        'Gtk/KeyThemeName': g.get(f'{IFACE}:gtk-key-theme', 'Default') or 'Default',
        'Gtk/DecorationLayout': layout,
        # gsd: the gtk-im-module setting, else IBus (measured: 'ibus' with no input sources and no IBus installed)
        'Gtk/IMModule': g.get(f'{IFACE}:gtk-im-module') or 'ibus',
        'Gtk/KeynavUseCaret': int(bool(g.get(f'{A11Y_ROOT}:always-show-text-caret', False))),
        'Gtk/RecentFilesEnabled': int(bool(g.get(f'{PRIVACY}:remember-recent-files', True))),
        'Gtk/RecentFilesMaxAge': int(g.get(f'{PRIVACY}:recent-files-max-age', -1)),
        'Gtk/ShellShowsDesktop': int(bool(g.get(f'{BACKGROUND}:show-desktop-icons', False))),
        'Gtk/TitlebarDoubleClick': g.get(f'{WM}:action-double-click-titlebar', 'toggle-maximize') or 'none',
        'Gtk/TitlebarMiddleClick': g.get(f'{WM}:action-middle-click-titlebar', 'none') or 'none',
        'Gtk/TitlebarRightClick': g.get(f'{WM}:action-right-click-titlebar', 'menu') or 'none',
        'Gtk/OverlayScrolling': int(bool(g.get(f'{IFACE}:overlay-scrolling', True))),
        'Xft/DPI': int(dpi * win_scale * 1024),
        'Xft/Antialias': int(aa != 'none'),
        'Xft/Hinting': int(hint != 'none'),
        'Xft/HintStyle': {'none': 'hintnone', 'slight': 'hintslight', 'medium': 'hintmedium',
                          'full': 'hintfull'}.get(hint, 'hintslight'),
        'Xft/RGBA': (rgba if aa == 'rgba' else 'none'),
        'Gtk/CursorBlinkTimeout': int(g.get(f'{IFACE}:cursor-blink-timeout', 10) or 10),
        'Gtk/EnablePrimaryPaste': int(bool(g.get(f'{IFACE}:gtk-enable-primary-paste', True))),
        'Gtk/ColorScheme': g.get(f'{IFACE}:gtk-color-scheme', '') or '',
        'Gtk/ShowStatusShapes': int(bool(g.get(f'{A11Y}:show-status-shapes', False))),
        'Gtk/DialogsUseHeader': 1,
        'Gtk/AutoMnemonics': 1,
        'Gtk/Modules': 'gail:atk-bridge',
        'Net/FallbackIconTheme': 'gnome',
        'Gdk/WindowScalingFactor': win_scale,
        'Gdk/UnscaledDPI': int(dpi * 1024),
    })
    if g.get('session-bus-id'):
        xs['Gtk/SessionBusId'] = g['session-bus-id']
    # gsd applies the plugin's "overrides" (a{sv}) last
    for k, v in (g.get(f'{XS_PLUGIN}:overrides') or {}).items():
        if isinstance(v, (bool, int)):
            xs[k] = int(v)
        elif isinstance(v, str):
            xs[k] = v
        elif isinstance(v, tuple) and len(v) == 4:
            xs[k] = tuple(int(c) for c in v)
    res = {
        '*customization': '-color',   # as gnome-settings-daemon: Xt apps load their colour app-defaults
        'Xft.dpi': f'{dpi * win_scale:g}',
        'Xft.antialias': '1' if aa != 'none' else '0',
        'Xft.hinting': '1' if hint != 'none' else '0',
        'Xft.hintstyle': xs['Xft/HintStyle'],
        'Xft.rgba': xs['Xft/RGBA'],
        'Xcursor.theme': cursor_theme,
        'Xcursor.size': str(cursor_size * win_scale),
    }
    # Athena / Motif / Tk read these generic resources at start (they have no XSETTINGS); only set when the person
    # asked for high contrast or larger text, so the apps' own look stays otherwise
    if hc:
        fg, bg = ('white', 'black') if dark else ('black', 'white')
        res.update({'*background': bg, '*foreground': fg, '*Background': bg, '*Foreground': fg,
                    '*borderColor': fg, '*highlightColor': fg, '*highlightBackground': bg,
                    '*activeBackground': fg, '*activeForeground': bg, '*selectBackground': fg,
                    '*selectForeground': bg, '*troughColor': bg, '*insertBackground': fg,
                    '*topShadowColor': fg, '*bottomShadowColor': fg, '*shadowThickness': '2',
                    '*highlightThickness': '3'})
    if scaling > 1.01:
        pt = round(10 * scaling)
        px = round(13 * scaling)
        # Motif with Xft: the default render table
        res.update({'*renderTable': 'rt', '*rt*fontType': 'FONT_IS_XFT', '*rt*fontName': 'Sans',
                    '*rt*fontSize': str(pt)})
        xlfd = core_font(px) if core_font else None
        if xlfd:
            # Athena (core fonts) and Tk (which accepts XLFD names) read "font"/"Font"; Motif's old fontList too
            res.update({'*font': xlfd, '*Font': xlfd, '*fontList': xlfd})
    return xs, res


def encode_resources(res):
    return ''.join(f'{k}:\t{v}\n' for k, v in sorted(res.items())).encode()


class SettingsMirror:
    """Owns `_XSETTINGS_S0` on the engine display and keeps it and RESOURCE_MANAGER in step with GNOME."""

    def __init__(self, display_name, log=print):
        self.log = log
        self.d = xdisplay.Display(display_name)
        self.root = self.d.screen().root
        self.atom = self.d.intern_atom
        self.sel = self.atom('_XSETTINGS_S0')
        self.prop = self.atom('_XSETTINGS_SETTINGS')
        self.rm = self.atom('RESOURCE_MANAGER')
        self.serial = 0
        self.last_change = {}
        self.current = {}
        self.owner = None
        self.saved_rm = None
        self.enabled = False
        self.gs = GnomeSettings(on_change=self.apply)
        self.watch = None

    def start(self):
        cur = self.d.get_selection_owner(self.sel)
        if cur not in (0, X.NONE) and getattr(cur, 'id', cur):
            self.log(f'settings: _XSETTINGS_S0 already owned by 0x{getattr(cur, "id", cur):x}; not taking it over')
            return False
        restore_leftover(self.d, self.log)
        self.owner = self.root.create_window(-10, -10, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent,
                                             event_mask=X.PropertyChangeMask, override_redirect=1)
        p = self.root.get_full_property(self.rm, Xatom.STRING)
        self.saved_rm = p.value if p else None
        self.root.change_property(self.atom(SAVED_RM), Xatom.STRING, 8,
                                  (b'\x01' + bytes(self.saved_rm)) if self.saved_rm is not None else b'\x00')
        self.d.sync()
        self.cleaner = start_cleaner(self.d.get_display_name(), self.log)
        t = self._timestamp()
        self.owner.set_selection_owner(self.sel, t)
        self.d.sync()
        if self.d.get_selection_owner(self.sel).id != self.owner.id:
            self.log('settings: could not own _XSETTINGS_S0')
            return False
        # tell clients a manager is here (ICCCM MANAGER message)
        ev = xevent.ClientMessage(window=self.root, client_type=self.atom('MANAGER'),
                                  data=(32, [t, self.sel, self.owner.id, 0, 0]))
        self.root.send_event(ev, event_mask=X.StructureNotifyMask)
        self.enabled = True
        self.apply()
        self.watch = GLib.io_add_watch(self.d.fileno(), GLib.IO_IN, self._on_x)
        self.log('settings: owning _XSETTINGS_S0 on the engine; mirroring GNOME settings')
        return True

    def _timestamp(self):
        self.owner.change_property(self.atom('_X11_A11Y_PROXY_TIME'), Xatom.STRING, 8, b'', X.PropModeAppend)
        self.d.flush()
        while True:
            ev = self.d.next_event()
            if ev.type == X.PropertyNotify and ev.window.id == self.owner.id:
                return ev.time

    def _on_x(self, *_):
        while self.d.pending_events():
            ev = self.d.next_event()
            if ev.type == X.SelectionClear and ev.atom == self.sel:
                self.log('settings: another XSETTINGS manager took over; stopping')
                self.enabled = False
        return True

    def apply(self):
        if not self.enabled:
            return
        g = self.gs.get()
        g['session-bus-id'] = self.session_bus_id()
        xs, res = derive(g, self.core_font)
        self.serial += 1
        for k, v in xs.items():
            if self.current.get(('x', k)) != v:
                self.last_change[k] = self.serial
                self.current[('x', k)] = v
        self.owner.change_property(self.prop, self.prop, 8, encode_xsettings(xs, self.serial, self.last_change))
        base = self.saved_rm.decode(errors='replace') if isinstance(self.saved_rm, bytes) else (self.saved_rm or '')
        keep = [ln for ln in base.splitlines() if ln and ln.split(':', 1)[0].strip() not in res]
        data = ('\n'.join(keep) + ('\n' if keep else '')).encode() + encode_resources(res)
        self.root.change_property(self.rm, Xatom.STRING, 8, data)
        self.d.sync()
        self.log('settings: applied serial', self.serial, {k: xs[k] for k in ('Net/ThemeName', 'Xft/DPI',
                 'Gtk/CursorThemeSize', 'Gtk/EnableAnimations')}, 'resources', len(res))

    def session_bus_id(self):
        """The session bus's id, as gsd-xsettings publishes it (Gtk/SessionBusId)."""
        if not hasattr(self, '_bus_id'):
            self._bus_id = None
            try:
                bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
                r = bus.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', 'GetId',
                                  None, GLib.VariantType('(s)'), Gio.DBusCallFlags.NONE, 1000, None)
                self._bus_id = r.unpack()[0]
            except Exception as ex:   # noqa: BLE001 -- no session bus: gsd would not run either
                self.log('settings: no session bus id:', repr(ex))
        return self._bus_id

    def core_font(self, px):
        """A scalable sans core font the engine's X server has, at `px` pixels (XLFD), or None."""
        if not hasattr(self, '_core_fonts'):
            try:
                self._core_fonts = [f for f in self.d.list_fonts('-*-*-medium-r-normal--0-0-0-0-*-0-iso8859-1', 400)]
            except xerror.XError:
                self._core_fonts = []
        for want in ('dejavu sans', 'nimbus sans', 'liberation sans', 'helvetica', 'utopia', 'charter'):
            for f in self._core_fonts:
                parts = f.split('-')
                if len(parts) == 15 and parts[2].lower() == want:
                    parts[7], parts[8], parts[9], parts[10], parts[12] = str(px), '*', '*', '*', '*'
                    return '-'.join(parts)
        return None

    def stop(self):
        if not self.enabled and self.owner is None:
            return
        try:
            if self.saved_rm is None:
                self.root.delete_property(self.rm)
            else:
                self.root.change_property(self.rm, Xatom.STRING, 8, self.saved_rm)
            self.root.delete_property(self.atom(SAVED_RM))
            if self.owner is not None:
                self.owner.destroy()
            self.d.sync()
        except xerror.XError:
            pass
        self.enabled = False


def restore_leftover(d, log=print):
    """If a mirror died without restoring RESOURCE_MANAGER (its saved copy is still on the root and no XSETTINGS
    manager runs), restore it. Returns True if it did."""
    root = d.screen().root
    marker = d.intern_atom(SAVED_RM)
    try:
        p = root.get_full_property(marker, Xatom.STRING)
    except xerror.XError:
        return False
    if p is None:
        return False
    raw = bytes(p.value) if p.value is not None else b''
    rm = d.intern_atom('RESOURCE_MANAGER')
    if raw[:1] == b'\x01':
        root.change_property(rm, Xatom.STRING, 8, raw[1:])
    else:
        root.delete_property(rm)
    root.delete_property(marker)
    d.sync()
    log('settings: restored the RESOURCE_MANAGER a mirror left behind (it did not exit cleanly)')
    return True


def start_cleaner(display_name, log=print):
    """The process that restores the engine's resources when this proxy dies in any way (cleanup.py): it waits
    for the end of a pipe only we hold open."""
    pkg = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    env = dict(os.environ, PYTHONPATH=pkg + (':' + os.environ['PYTHONPATH'] if os.environ.get('PYTHONPATH') else ''))
    try:
        return subprocess.Popen([sys.executable, '-m', 'x11_a11y_proxy.cleanup', '--display', display_name],
                                stdin=subprocess.PIPE, env=env, start_new_session=True)
    except OSError as ex:
        log('settings: no cleanup process:', repr(ex))
        return None
