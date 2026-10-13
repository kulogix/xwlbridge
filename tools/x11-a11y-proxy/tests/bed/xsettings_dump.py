#!/usr/bin/env python3
"""Print what an X client on $DISPLAY can learn about desktop settings: the _XSETTINGS_S0 owner and its decoded
_XSETTINGS_SETTINGS, and the RESOURCE_MANAGER (xrdb) lines. One JSON object."""
import json, struct
from Xlib import display, X
d = display.Display(); root = d.screen().root
out = {'display': d.get_display_name(), 'screen': [d.screen().width_in_pixels, d.screen().height_in_pixels,
       d.screen().width_in_mms, d.screen().height_in_mms]}
own = d.get_selection_owner(d.intern_atom('_XSETTINGS_S0'))
out['xsettings_owner'] = hex(own.id) if own else None
if own:
    p = own.get_full_property(d.intern_atom('_XSETTINGS_SETTINGS'), X.AnyPropertyType)
    b = bytes(p.value) if p else b''
    s = {}
    if b:
        e = '<' if b[0] == 0 else '>'
        serial, n = struct.unpack(e + 'II', b[4:12]); o = 12
        for _ in range(n):
            t, nl = b[o], struct.unpack(e + 'H', b[o + 2:o + 4])[0]; o += 4
            name = b[o:o + nl].decode(); o += (nl + 3) & ~3; o += 4
            if t == 0: v = struct.unpack(e + 'i', b[o:o + 4])[0]; o += 4
            elif t == 1:
                l = struct.unpack(e + 'I', b[o:o + 4])[0]; o += 4; v = b[o:o + l].decode(errors='replace'); o += (l + 3) & ~3
            else: v = struct.unpack(e + 'HHHH', b[o:o + 8]); o += 8
            s[name] = v
    out['xsettings'] = s
rm = root.get_full_property(d.intern_atom('RESOURCE_MANAGER'), X.AnyPropertyType)
out['resource_manager'] = bytes(rm.value).decode(errors='replace').splitlines() if rm else None
print(json.dumps(out, sort_keys=True))
