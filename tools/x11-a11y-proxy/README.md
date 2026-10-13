# x11-a11y-proxy — accessible GNOME windows for X apps that live on the bridge's display

An X app can run *entirely* under X on the xwlbridge bridge's engine display (`-engine xvnc|xvfb`, e.g. `:20`) — no
split display, every X feature it needs — and still be a first-class, accessible window on GNOME:

- **one GNOME window per X top-level window**, showing its live pixels (DAMAGE-driven), with the X title, transient
  dialogs as dialogs, close as `WM_DELETE_WINDOW`, popups (menus, tooltips) drawn where they overlap;
- **accessible objects generated from the app's AT-SPI tree** (read on the engine's private accessibility bus):
  role, name, description, mnemonic/shortcut, states (checked, pressed, expanded, selected, disabled, …), values
  (AT-SPI Value via GtkAccessibleRange), text with caret and selection (GtkAccessibleText), actions (Orca's
  "perform default action" runs the app's), relations, announcements. Orca, the magnifier and every AT-SPI client
  see the mirrors; the engine's own tree stays off the desktop's bus;
- **the person's keyboard and pointer forwarded** with XTest through the bridge's `xwlbridge-reverse` XInput2 master
  (`-reverseinput`): never echoed back to GNOME, never mixed with VNC viewers' input. Modifier and lock states are
  synchronised (sticky keys; Caps Lock and Num Lock toggled in another window: tests/keyboard.sh);
- **focus**: activating the GNOME window gives the app the X focus (as a window manager would: `WM_TAKE_FOCUS`, the
  reverse keyboard's focus, `_NET_ACTIVE_WINDOW`), and the app's focus moves GNOME's focus to its mirror, which the
  screen reader announces. The other way round is limited: GTK 4 refuses an assistive technology's
  `Component.GrabFocus` on a widget (measured), so an AT cannot move the focus to a mirror -- and through it the
  app's -- by itself; a click does reach the app (Orca's flat-review click, KP_Divide, goes through the pointer path);
- **apps without an accessibility tree** get one built from what X and the toolkit say without the app's help:
  Tk 8.6 introspection through Tk's own `send` (real classes, texts, states, values, focus); Xt's editres protocol
  for Athena (xcalc) and Motif widget classes and windows, with each label read, in this order, from editres
  GetValues, from the text the app draws in the widget's window (the X RECORD extension, while the window redraws),
  from the app's resource files, from the pixels (OCR), and last from the widget's name; the X window tree; and last
  the pixels — OCR and a lightweight UI detector in a sidecar process (not part of this repository). A name the app or its toolkit
  did not report itself (Tk send, editres GetValues, drawn text) says where it came from in its description ("label
  from the app's resource files", "label not reported, the widget's name", "inferred from pixels, confidence
  0.83");
- **GNOME's accessibility settings mirrored into the engine** : as XSETTINGS (`_XSETTINGS_S0`) the same
  keys and values gsd-xsettings publishes on GNOME's Xwayland (53 keys on GNOME 50: high contrast, text scaling,
  cursor size and the rest) plus `Gtk/EnableAnimations` (off for reduced motion, which gsd does not publish there);
  in RESOURCE_MANAGER `Xft.dpi`, `Xcursor.size`, and Xt/Tk resources for high contrast and large text; followed
  live. The engine's previous RESOURCE_MANAGER is restored when the proxy exits -- also when it is killed: a small
  cleanup process restores it the moment the proxy dies, and the next proxy does on start if that did not happen.

## Limits

- **Electron / web content is mirrored as a group, not a web document**, so that Orca keeps an Electron app in
  focus mode (as it does natively: "Focus mode is sticky") -- as a GTK document the mirror would switch Orca to
  browse mode, whose single-letter navigation keys would swallow the person's typing. The cost: Orca's browse mode
  and its structural navigation (headings, landmarks, links by letter) are not available for web content through
  the proxy; Tab, the app's own keys and Orca's flat review are. A web browser on the engine (its app name is a
  browser's) keeps the document role and with it browse mode.
- **GTK 4 apps must be started through the proxy** (`x11-a11y-proxy -- COMMAND`), the bridge's wrapped command, or
  with `AT_SPI_BUS_ADDRESS` set to the engine's private bus: GTK 4 asks the session bus for the accessibility bus
  before it reads the display's `AT_SPI_BUS` property, so a GTK 4 app started on the engine any other way (a terminal
  with `DISPLAY=:20`, a service) registers on the desktop's bus, where Orca follows its window that nobody sees. The
  proxy cannot move it; it logs and announces it. GTK 3 and Qt follow the property (measured; Java/ATK and Electron
  go through the same libatspi lookup, which reads it first).
- **An AT cannot move the focus** to a mirror (GTK 4 refuses `GrabFocus`); a check box's mnemonic is spoken "C",
  not "Alt+C" (GTK 4's action key binding); see docs/accessibility.md.
- **Motif's and Athena's keyboard traversal is not followed**: editres has no focus, so Tab moves between the
  mirrors in reading order inside the proxy and Space/Return click the focused one -- the same keys can reach a
  different control than Motif's own Tab chain would (measured: Tab x3 + Space activates Seven natively, toggles
  Check me through the proxy).
- If the AT-SPI reader stops answering (its process frozen, or stuck in a call into an app for 20 s), the proxy
  kills and restarts it, drops the commands queued to it, and announces "Accessibility of this window is
  reconnecting".

Measurements, parity results per toolkit and what differs from native X: docs/accessibility.md.

## Run

```sh
# the bridge with a private accessibility bus and the reverse master
xwlbridge -engine xvnc -display 20 -a11ybus -reverseinput -nowrap &
# an app on the engine, mirrored on GNOME (the proxy starts it with the right environment)
tools/x11-a11y-proxy/x11-a11y-proxy --display :20 -- xcalc
# or mirror whatever maps on :20 (apps started elsewhere need DISPLAY=:20 and AT_SPI_BUS_ADDRESS from the
# engine's AT_SPI_BUS root property -- GTK 4 apps cannot do without it, see Limits; `x11-a11y-proxy -- CMD` sets both)
tools/x11-a11y-proxy/x11-a11y-proxy --display :20
```

Options: `--match REGEX` (titles), `--inferred auto|off|only`, `--a11y-wait S` (default 4), `--uidet "CMD"` or
`--uidet-fifo DIR` (OCR + detector sidecar), `--no-settings`, `--settings-only`, `--no-modsync`, `--title-prefix`,
`--log FILE`, `-v` (with it the log has every GTK focus change); `X11_A11Y_PROXY_READER_TRACE=FILE` writes every
AT-SPI event the reader receives. With a bridge built without `-a11ybus` the proxy starts the private bus itself (and
sets the root property); without `-reverseinput` keys go through the core keyboard (the bridge's per-client XTest
attribution keeps them off the desktop while the app has the X focus) and clicks are refused (the agent's input
shield would take them and they would echo).

## Requirements and portability

Python ≥ 3.9, PyGObject with GTK ≥ 4.14 (AccessibleText; 4.22 for caret/selection set by an AT), libatspi
(at-spi2-core), python-xlib, dbus-daemon. Nothing is compiled; nothing mutter-specific is used (the proxy is an
ordinary GTK 4 Wayland client), so it runs on GNOME, on labwc (Raspberry Pi OS) and other wlroots compositors,
and on x86-64 and aarch64 alike. Packages: Debian/Ubuntu/Raspberry Pi OS `python3-gi gir1.2-gtk-4.0
gir1.2-atspi-2.0 python3-xlib dbus`; RHEL 10/AlmaLinux 10 `python3-gobject gtk4 at-spi2-core` plus
`pip install --user python-xlib` (no repository packages python-xlib there).
Optional: the uidet sidecar (container or local Python with
onnxruntime + rapidocr) for pixel fallback.

## Why Python + GTK 4

- The mirror must be a GTK 4 widget tree (the task, and the only toolkit whose accessibility API lets an app define
  arbitrary roles, properties, text and value interfaces from outside data: GtkAccessible, GtkAccessibleText,
  GtkAccessibleRange). PyGObject can implement those interfaces (measured in the prototype and here).
- libatspi must run in a *second* process on the private bus (it binds to one bus per process), which the
  standard library's subprocess + JSON lines make trivial.
- No build step: the same files run on x86-64 and on a Raspberry Pi, on Debian, Ubuntu and RHEL, with distro
  packages only. The hot path (pixels) is DAMAGE-driven X GetImage into a GdkMemoryTexture: copies, no per-pixel
  Python.
- Licences: the tool is MIT; it links nothing — it imports PyGObject (LGPL-2.1+), GTK/libatspi (LGPL-2.1+) and
  python-xlib (LGPL-2.1+) at run time, which MIT code may do. The optional OCR/detector sidecar keeps its own
  licences (it is not part of this repository).
- A Rust rewrite (gtk4-rs, or AccessKit inside the bridge's `-showwindows`) is a possible later step; this tool's
  behaviour is the specification for it.

## Files

| file | what |
|---|---|
| `x11-a11y-proxy` | launcher (`python3 -m x11_a11y_proxy`) |
| `x11_a11y_proxy/app.py` | the application: windows, private bus, reader, inferred mode, settings, launching |
| `x11_a11y_proxy/xengine.py` | the X side: windows, pixels, XTest through the reverse master, focus, WM protocols |
| `x11_a11y_proxy/reader.py` | AT-SPI reader on the private bus (own process; JSON lines) |
| `x11_a11y_proxy/mirror.py` | GTK 4 mirror widgets; role/state/key-binding mapping |
| `x11_a11y_proxy/window.py` | one GNOME window per X toplevel: pixels, mirrors, keyboard/pointer, focus |
| `x11_a11y_proxy/settings.py` | XSETTINGS + RESOURCE_MANAGER mirror of GNOME's settings |
| `x11_a11y_proxy/inferred.py` | inferred trees: Tk send, editres + resources, window tree, OCR/detector |
| `x11_a11y_proxy/editres.py`, `tksend.py` | editres and Tk send protocol clients |
| `x11_a11y_proxy/drawntext.py` | the text an app draws, from the X protocol (RECORD) |
| `x11_a11y_proxy/cleanup.py` | restores the engine's RESOURCE_MANAGER if the proxy dies without doing it |
| `tests/` | container test harness (parity vs Xwayland with Orca, settings carry-over, keyboard a11y, inferred) |

## Tests

`tests/test_unit.py` runs anywhere (`python3 -m unittest discover -s tools/x11-a11y-proxy/tests -p 'test_unit.py'`).
The other tests run in a container with a headless GNOME Shell session; `tests/bed/README.md` lists what that
image needs. Nothing runs on the host desktop.

| script | what | oracle |
|---|---|---|
| `tests/test_unit.py` | XSETTINGS encoding, gsd-xsettings' key set and mapping, key-binding mapping, editres parsing, drawn-text request parsing, provenance marks, the reader's rule for sending a subtree before a focus | `python3 -m unittest` |
| `tests/parity.sh` + `report.py` | each probe (GTK3, GTK4, Qt5, Qt6, Java/ATK, Electron, Tk, Motif, xcalc, GTK3/Qt6 menus) on GNOME's Xwayland (arm X) and behind the proxy (arm P), same keys, Orca 50 | Orca speech per step (Speech Dispatcher logger), app effects bound to the step that caused them (mnemonics, typing, values, shortcuts, AT actions, pointer) and compared per step between the arms, focus order on both buses, tree equivalence |
| `tests/settings.sh` + `settings_report.py` | high contrast, text scaling, cursor size, reduced motion: Xwayland + gsd-xsettings vs engine without/with the mirror | what each toolkit reports + screenshots |
| `tests/keyboard.sh` | sticky keys, Caps Lock and Num Lock sync, slow keys, AT focus, Orca's flat-review click | entry text, app effects |
| `tests/run_inferred.sh` (`inferred.sh`) | xcalc, a self-drawn Xlib window, Tk without introspection, with the uidet sidecar (second container, FIFOs) | Orca speech, app effects, live display announcements |
| `tests/perf.sh` | CPU/RSS idle and with a 640x480 window repainted at 30 fps | frames shown vs drawn |
| `tests/smoke.sh` | quick per-toolkit run | speech + effects |

```sh
docker run --rm --network none --memory=8g -u "$(id -u)" -v "$PWD":/w -e OUT=/w/out/a11y-proxy/parity \
  <bed-image> dbus-run-session bash /w/tools/x11-a11y-proxy/tests/parity.sh
python3 tools/x11-a11y-proxy/tests/report.py out/a11y-proxy/parity      # -> report.md / report.json
UIDET_IMAGE=... UIDET_MODELS=... tools/x11-a11y-proxy/tests/run_inferred.sh out/a11y-proxy/inferred  # two containers
```
