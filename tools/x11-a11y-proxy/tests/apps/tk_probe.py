#!/usr/bin/env python3
"""Tk probe: same controls; Alt+S / Alt+C / Ctrl+K bound; prints ACTIVATE / TOGGLE / TEXT / VALUE / SHORTCUT and
what it sees of the settings (tk scaling, default font, colours from the option database)."""
import sys, tkinter as tk, tkinter.font as tkf
tag = sys.argv[1] if len(sys.argv) > 1 else 'x'
r = tk.Tk(); r.title(f'Tk probe {tag}')
tk.Label(r, text='Probe label').pack()
b = tk.Button(r, text='Seven', underline=0, command=lambda: print('ACTIVATE Seven', flush=True)); b.pack()
v = tk.IntVar(); c = tk.Checkbutton(r, text='Check me', underline=0, variable=v, command=lambda: print('TOGGLE', bool(v.get()), flush=True)); c.pack()
sv = tk.StringVar(); sv.trace_add('write', lambda *x: print('TEXT', sv.get(), flush=True)); tk.Entry(r, textvariable=sv).pack()
s = tk.Scale(r, from_=0, to=100, orient='horizontal', command=lambda x: print('VALUE', x, flush=True)); s.set(40); s.pack()
r.bind_all('<Alt-s>', lambda e: b.invoke()); r.bind_all('<Alt-c>', lambda e: c.invoke())
r.bind_all('<Control-k>', lambda e: print('SHORTCUT ctrl+k', flush=True))
def dump():
    f = tkf.nametofont('TkDefaultFont').actual()
    print('SETTINGS tk', {'tk_scaling': r.tk.call('tk', 'scaling'), 'font': f, 'button_font': b.cget('font'),
          'patchlevel': r.tk.call('info', 'patchlevel'), 'bg': r.cget('bg'), 'button_bg': b.cget('bg'),
          'button_fg': b.cget('fg'), 'winfo_fpixels_1i': r.winfo_fpixels('1i')}, flush=True)
def every():
    dump(); r.after(4000, every)
r.after(1500, every); print('READY', flush=True); r.mainloop()
