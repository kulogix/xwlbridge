#!/usr/bin/env python3
"""Self-drawn X app (plain Xlib, one window, no child windows, no accessibility): a label and three buttons drawn
as rectangles with text; a click on a button prints ACTIVATE <label>. Only pixels describe it: the proxy's
detector + OCR path. Usage: canvas_probe.py TAG"""
import sys
from Xlib import X, display
tag = sys.argv[1] if len(sys.argv) > 1 else 'x'
d = display.Display(); s = d.screen()
w = s.root.create_window(0, 0, 380, 200, 0, s.root_depth, X.InputOutput, X.CopyFromParent,
                         background_pixel=s.white_pixel, event_mask=X.ExposureMask | X.ButtonPressMask)
w.set_wm_name(f'Canvas probe {tag}'); w.set_wm_class('canvas_probe', 'CanvasProbe')
w.change_property(d.intern_atom('WM_PROTOCOLS'), d.intern_atom('ATOM'), 32, [d.intern_atom('WM_DELETE_WINDOW')])
fonts = d.list_fonts('-*-helvetica-bold-r-normal--0-0-0-0-p-0-iso8859-1', 5) or d.list_fonts('*', 5)
f = d.open_font(fonts[0].replace('--0-0-0-0-p-0-', '--20-*-*-*-p-*-') if fonts and '--0-0-' in fonts[0] else 'fixed')
gc = w.create_gc(foreground=s.black_pixel, background=s.white_pixel, font=f, line_width=2)
BUTTONS = [('OK', 20, 110), ('Cancel', 130, 110), ('Help', 260, 110)]
def draw():
    w.clear_area()
    w.draw_text(gc, 20, 50, b'Choose an action')
    for label, x, y in BUTTONS:
        w.rectangle(gc, x, y, 100, 44)
        w.draw_text(gc, x + 14, y + 29, label.encode())
    d.flush()
w.map(); d.flush(); print('READY', flush=True)
while True:
    e = d.next_event()
    if e.type == X.Expose and e.count == 0: draw()
    elif e.type == X.ButtonPress:
        for label, x, y in BUTTONS:
            if x <= e.event_x < x + 100 and y <= e.event_y < y + 44: print('ACTIVATE', label, flush=True)
