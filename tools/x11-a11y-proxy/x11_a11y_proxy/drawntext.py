"""The text an X client draws, read from the X protocol: the RECORD extension delivers a copy of the client's core
text requests (PolyText8/16, ImageText8/16: drawable, position, string) while the proxy asks the client's windows to
redraw (ClearArea with exposures, which the client answers by drawing what it shows).

Why: Motif labels set in code (UIL, Builder Xcessory, XtSetValues) are in no resource file, and editres cannot read
them on 64-bit -- Motif decodes the widget ids of a GetValues request with libXmu's decoder, which ORs in a base
address only libXmu's own editres handler sets (never in a Motif app), and libXmu's 32-bit decoder sign-extends ids
with bit 31 set, so even Athena widgets above that line are "no longer in the client" (measured with
editres). What a label widget draws into its own window is its label, exactly. Text drawn client-side with Xft
(RENDER glyphs) carries glyph indices, not characters: it is not read here (the pixels are, by OCR, when the sidecar
runs).

Nothing is changed in the app: it redraws what it already shows."""
import struct
import sys
import threading
import time

from Xlib import display as xdisplay, error as xerror
from Xlib.ext import record

POLY_TEXT_8, POLY_TEXT_16, IMAGE_TEXT_8, IMAGE_TEXT_16 = 74, 75, 76, 77


def parse_requests(data, big_endian=False):
    """[(opcode, drawable, x, y, text)] for the core text requests in a block of client requests (other requests
    are skipped). 8-bit strings are Latin-1 (the font's own encoding: a Symbol font's bytes are mapped by the
    caller); 16-bit ones are taken as UCS-2 (ISO 10646 fonts)."""
    e = '>' if big_endian else '<'
    out, i = [], 0
    while i + 4 <= len(data):
        op = data[i]
        n4 = struct.unpack(e + 'H', data[i + 2:i + 4])[0]
        hdr = 4
        if n4 == 0:   # BIG-REQUESTS: the length follows as 32 bits
            if i + 8 > len(data):
                break
            n4 = struct.unpack(e + 'I', data[i + 4:i + 8])[0]
            hdr = 8
        ln = n4 * 4
        if ln < hdr:
            break
        req = data[i:i + ln]
        i += ln
        if op not in (POLY_TEXT_8, POLY_TEXT_16, IMAGE_TEXT_8, IMAGE_TEXT_16) or len(req) < hdr + 12:
            continue
        drw, _gc, x, y = struct.unpack(e + 'IIhh', req[hdr:hdr + 12])
        body = req[hdr + 12:]
        wide = op in (POLY_TEXT_16, IMAGE_TEXT_16)
        if op in (IMAGE_TEXT_8, IMAGE_TEXT_16):
            n = req[1]
            s = body[:2 * n if wide else n]
            out.append((op, drw, x, y, _decode(s, wide)))
            continue
        parts, j = [], 0
        while j < len(body):
            n = body[j]
            if n == 0:
                break   # padding
            if n == 255:   # a font change: 255 + 4 bytes (font id, MSB first)
                j += 5
                continue
            s = body[j + 2:j + 2 + (2 * n if wide else n)]
            parts.append(_decode(s, wide))
            j += 2 + (2 * n if wide else n)
        if parts:
            out.append((op, drw, x, y, ''.join(parts)))
    return out


def _decode(b, wide):
    if wide:
        return b.decode('utf-16-be', errors='replace')
    return b.decode('latin-1')


class DrawnText:
    """Captures the text one X client draws while some of its windows redraw."""

    def __init__(self, display_name, log=print):
        self.name, self.log = display_name, log
        try:
            d = xdisplay.Display(display_name)
            self.available = d.has_extension('RECORD')
            d.close()
        except Exception:
            self.available = False
        if not self.available:
            self.log('drawn text: the engine has no RECORD extension; labels drawn by apps are not read')

    def capture(self, client_window, windows, timeout=2.0, quiet=0.3):
        """{window id: [(x, y, text), ...]} -- the core text the client owning `client_window` draws into any
        window while each of `windows` is cleared with exposures -- or None (no RECORD, or it failed)."""
        if not self.available or not windows:
            return None
        ctl = rec = None
        got, last = [], [time.monotonic()]
        started = threading.Event()
        try:
            ctl = xdisplay.Display(self.name)
            rec = xdisplay.Display(self.name)
            ctx = ctl.record_create_context(0, [client_window], [{
                'core_requests': (POLY_TEXT_8, IMAGE_TEXT_16), 'core_replies': (0, 0),
                'ext_requests': (0, 0, 0, 0), 'ext_replies': (0, 0, 0, 0), 'delivered_events': (0, 0),
                'device_events': (0, 0), 'errors': (0, 0), 'client_started': False, 'client_died': False}])
            ctl.sync()   # the context must exist before the other connection enables it

            def cb(reply):
                if reply.category == record.StartOfData:
                    started.set()
                elif reply.category == record.FromClient:
                    swapped = bool(getattr(reply, 'client_swapped', False))   # relative to us
                    got.extend(parse_requests(reply.data, (sys.byteorder == 'big') != swapped))
                    last[0] = time.monotonic()

            def run():
                try:
                    rec.record_enable_context(ctx, cb)
                except Exception as ex:   # noqa: BLE001 -- reported, capture gives up
                    self.log('drawn text: RECORD failed:', repr(ex))
                    started.set()
            th = threading.Thread(target=run, daemon=True)
            th.start()
            started.wait(1.0)
            for w in windows:
                try:
                    ctl.create_resource_object('window', w).clear_area(0, 0, 0, 0, exposures=True)
                except xerror.XError:
                    pass
            ctl.sync()
            t0 = last[0] = time.monotonic()
            while time.monotonic() - t0 < timeout:
                time.sleep(0.05)
                if time.monotonic() - last[0] > quiet:
                    break
            ctl.record_disable_context(ctx)
            ctl.sync()
            th.join(1.0)
            ctl.record_free_context(ctx)
            ctl.sync()
        except Exception as ex:   # noqa: BLE001 -- an X error or a missing request: no drawn text
            self.log('drawn text: capture failed:', repr(ex))
            return None
        finally:
            for d in (rec, ctl):
                try:
                    if d is not None:
                        d.close()
                except Exception:   # noqa: BLE001
                    pass
        out = {}
        for _op, drw, x, y, text in got:
            out.setdefault(drw, []).append((x, y, text))
        return out


def label_of(runs):
    """One label from the text runs drawn into a window: the last complete redraw's runs, in reading order."""
    if not runs:
        return ''
    seen, uniq = set(), []
    for x, y, t in reversed(runs):   # a redraw repeats what was drawn before: keep each position once
        if (x, y) in seen:
            continue
        seen.add((x, y))
        uniq.append((x, y, t))
    uniq.sort(key=lambda r: (r[1], r[0]))
    return ' '.join(t for _x, _y, t in uniq if t.strip()).strip()
