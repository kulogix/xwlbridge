#!/usr/bin/env python3
"""Menu probe: argv[1] = gtk3 | qt6, argv[2] = tag. A menu bar with "File": "Open" (Ctrl+O), "Save" (Ctrl+S),
and a button. Menus are separate X windows (override-redirect) and separate accessible windows: the proxy's
popup path. Prints ACTIVATE <item>."""
import sys
kit = sys.argv[1] if len(sys.argv) > 1 else 'gtk3'; tag = sys.argv[2] if len(sys.argv) > 2 else 'x'
if kit == 'gtk3':
    import gi
    gi.require_version('Gtk', '3.0'); from gi.repository import Gtk, Gdk
    w = Gtk.Window(title=f'GTK3 menu probe {tag}'); w.set_default_size(320, 200)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL); w.add(box)
    ag = Gtk.AccelGroup(); w.add_accel_group(ag)
    mb = Gtk.MenuBar(); box.pack_start(mb, False, False, 0)
    fm = Gtk.MenuItem.new_with_mnemonic('_File'); mb.append(fm)
    menu = Gtk.Menu(); fm.set_submenu(menu)
    for label, key in (('_Open', 'o'), ('_Save', 's')):
        it = Gtk.MenuItem.new_with_mnemonic(label)
        it.add_accelerator('activate', ag, Gdk.keyval_from_name(key), Gdk.ModifierType.CONTROL_MASK, Gtk.AccelFlags.VISIBLE)
        it.connect('activate', lambda x, l=label: print('ACTIVATE', l.strip('_'), flush=True)); menu.append(it)
    b = Gtk.Button(label='Seven'); b.connect('clicked', lambda *_: print('ACTIVATE Seven', flush=True)); box.pack_start(b, False, False, 0)
    w.connect('destroy', Gtk.main_quit); w.show_all(); b.grab_focus(); print('READY', flush=True); Gtk.main()
else:
    from PyQt6.QtWidgets import QApplication, QMainWindow, QPushButton
    from PyQt6.QtGui import QAction, QKeySequence
    app = QApplication(sys.argv[:1]); w = QMainWindow(); w.setWindowTitle(f'Qt6 menu probe {tag}')
    fm = w.menuBar().addMenu('&File')
    for label, key in (('&Open', 'Ctrl+O'), ('&Save', 'Ctrl+S')):
        a = QAction(label, w); a.setShortcut(QKeySequence(key)); a.triggered.connect(lambda c=False, l=label: print('ACTIVATE', l.strip('&'), flush=True)); fm.addAction(a)
    b = QPushButton('Seven'); b.clicked.connect(lambda: print('ACTIVATE Seven', flush=True)); w.setCentralWidget(b)
    w.resize(320, 200); w.show(); print('READY', flush=True); sys.exit(app.exec())
