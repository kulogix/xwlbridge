#!/usr/bin/env python3
"""Animation load for the pixel path: a W x H window repainted in full FPS times a second (alternating colours,
plus a moving bar), counting frames. Usage: anim_probe.py TAG [W H FPS SECONDS]"""
import sys, time
from Xlib import X, display
tag = sys.argv[1]; W, H, FPS, SECS = (int(a) for a in (sys.argv[2:6] if len(sys.argv) > 5 else (640, 480, 30, 3600)))
d = display.Display(); s = d.screen()
w = s.root.create_window(0, 0, W, H, 0, s.root_depth, X.InputOutput, X.CopyFromParent, background_pixel=s.black_pixel)
w.set_wm_name(f'Anim probe {tag}'); w.map(); d.flush()
cm = s.default_colormap
cols = [cm.alloc_color(r, g, b).pixel for r, g, b in ((65535, 0, 0), (0, 0, 65535))]
gcs = [w.create_gc(foreground=c) for c in cols]
n, t0 = 0, time.monotonic()
while time.monotonic() - t0 < SECS:
    g = gcs[n % 2]
    w.fill_rectangle(g, 0, 0, W, H); w.fill_rectangle(gcs[(n + 1) % 2], (n * 7) % W, 0, 20, H)
    d.flush(); n += 1
    time.sleep(max(0, t0 + n / FPS - time.monotonic()))
    if n % (FPS * 5) == 0: print('FRAMES', n, flush=True)
