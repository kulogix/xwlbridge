#!/usr/bin/env python3
"""Qt probe: argv[1] = 5 or 6, argv[2] = tag. Same controls/outputs as the GTK probes (Ctrl+K: QShortcut)."""
import sys
ver = sys.argv[1] if len(sys.argv) > 1 else '6'; tag = sys.argv[2] if len(sys.argv) > 2 else 'x'
if ver == '5':
    from PyQt5.QtWidgets import QApplication, QWidget, QVBoxLayout, QLabel, QPushButton, QCheckBox, QLineEdit, QSlider, QShortcut
    from PyQt5.QtCore import Qt, QTimer
    from PyQt5.QtGui import QPalette, QGuiApplication, QKeySequence
else:
    from PyQt6.QtWidgets import QApplication, QWidget, QVBoxLayout, QLabel, QPushButton, QCheckBox, QLineEdit, QSlider
    from PyQt6.QtCore import Qt, QTimer
    from PyQt6.QtGui import QPalette, QGuiApplication, QKeySequence, QShortcut
app = QApplication(sys.argv[:1])
w = QWidget(); w.setWindowTitle(f'Qt{ver} probe {tag}'); l = QVBoxLayout(w)
l.addWidget(QLabel('Probe label'))
b = QPushButton('&Seven'); b.clicked.connect(lambda: print('ACTIVATE Seven', flush=True)); l.addWidget(b)
b.setToolTip('Adds the digit seven')
c = QCheckBox('&Check me'); c.toggled.connect(lambda v: print('TOGGLE', v, flush=True)); l.addWidget(c)
e = QLineEdit(); e.setPlaceholderText('type here'); e.textChanged.connect(lambda t: print('TEXT', t, flush=True)); l.addWidget(e)
o = Qt.Horizontal if ver == '5' else Qt.Orientation.Horizontal
s = QSlider(o); s.setRange(0, 100); s.setValue(40); l.addWidget(s)
s.valueChanged.connect(lambda v: print('VALUE', v, flush=True))
k = QShortcut(QKeySequence('Ctrl+K'), w); k.activated.connect(lambda: print('SHORTCUT ctrl+k', flush=True))
w.resize(320, 220); w.show()
def dump():
    scr = app.primaryScreen(); f = app.font()
    win = QPalette.Window if ver == '5' else QPalette.ColorRole.Window
    txt = QPalette.WindowText if ver == '5' else QPalette.ColorRole.WindowText
    extra = {}
    if ver == '6':
        try:
            sh = QGuiApplication.styleHints(); extra['colorScheme'] = str(sh.colorScheme())
            try: extra['contrast'] = str(sh.accessibility().contrastPreference())
            except Exception as ex: extra['contrast'] = f'n/a ({type(ex).__name__})'
        except Exception as ex: extra['err'] = str(ex)
    print('SETTINGS qt', ver, {'font': f.family(), 'pt': f.pointSizeF(), 'logicalDpi': scr.logicalDotsPerInch(),
          'dpr': scr.devicePixelRatio(), 'style': app.style().objectName(), 'platform': app.platformName(),
          'window': app.palette().color(win).name(), 'text': app.palette().color(txt).name(), **extra}, flush=True)
QTimer.singleShot(1500, dump); _t = QTimer(); _t.timeout.connect(dump); _t.start(4000)
print('READY', flush=True); sys.exit(app.exec() if ver == '6' else app.exec_())
