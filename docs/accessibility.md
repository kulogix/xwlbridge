# Accessibility of apps on the bridge's display

This page is for screen-reader users who run X apps that live on the bridge's display. There are two ways to use such apps with a screen reader on GNOME:

1. `-showwindows`: the app's windows are shown natively on the desktop. Their accessibility is then Xwayland's.
2. The proxy `tools/x11-a11y-proxy` (supported in 0.1.2). Use it for apps whose accessibility tree must stay off the desktop, or that have no tree.

See [../tools/x11-a11y-proxy/README.md](../tools/x11-a11y-proxy/README.md) for the proxy itself.

The proxy is a GNOME-side program (Python + GTK 4, MIT). It needs a stock engine: `-a11ybus` and `-reverseinput` work on `xvnc`, `xvfb` and `xwayland` (the measurements below used `xvnc`). It shows each X top-level window of an app as a GNOME window with live pixels. The window holds accessible GTK objects generated from the app's AT-SPI tree. The proxy reads that tree on the engine's private bus (`-a11ybus`). Keyboard and pointer go to the app through the bridge's `-reverseinput` master.

GNOME's accessibility settings (high contrast, text scaling, cursor size, reduced motion) are mirrored into the engine as XSETTINGS (the same 53 keys gsd-xsettings publishes on GNOME 50's Xwayland) and as X resources.

Apps without an accessibility tree (Tk, Xt/Athena such as xcalc, Motif) get one inferred from Tk's send, editres, the text they draw, their resource files and, optionally, OCR (a sidecar not in this repository).

## Quick start

The proxy is in a clone of this repository (`git clone https://github.com/kulogix/xwlbridge`), not in the release
bundle. Run these from the clone. `-engine xvnc` needs TigerVNC's Xvnc. Packages on Ubuntu/Debian (RHEL/AlmaLinux 10:
`python3-gobject gtk4 at-spi2-core`, plus `pip install --user python-xlib` because no repository packages python-xlib
there, and `-engine xwayland` instead of xvnc):

```sh
sudo apt install python3-gi gir1.2-gtk-4.0 gir1.2-atspi-2.0 python3-xlib dbus tigervnc-standalone-server
xwlbridge -backend gnome -engine xvnc -display 20 -a11ybus -reverseinput -nowrap &
tools/x11-a11y-proxy/x11-a11y-proxy --display :20 -- APP
```

## What was measured

Tests ran in real sessions: Ubuntu 24.04 "Ubuntu on Xorg" (native X11, Orca 46) and Ubuntu 26.04 with GNOME 50 (Orca 50). They used 11 probe apps. Orca speech was captured through a Speech Dispatcher logger.

- **Facts spoken** (name, role, state, value, typed echo, selection, where-am-I, flat review), bridge plus proxy compared with native Xorg:

| Toolkit | Bridge + proxy | Native Xorg |
|---|---|---|
| GTK 3 | 23/23 | the same or fewer |
| GTK 4 | 13/23 | 13/23 (the probe's Tab counts do not fit GTK 4's focus chain in any test setup) |
| Qt 5 | 23/23 | 22/23 |
| Qt 6 | 23/23 | 22/23 |
| Java | 22/23 | 21/23 |
| Electron | 23/23 | the same or fewer |
| GTK 3 menu | 4/4 | the same or fewer |
| Qt 6 menu | 3/4 | 2/4 |

- **App effects** (mnemonics, typing, slider keys, Space, accelerators, AT DoAction/SetValue, pointer, menus) were identical to native for every toolkit with an API: GTK 3, Qt 5, Qt 6 and Electron 8/8, Java 7/8, GTK 4 4/8, menus 2/2. The misses also happen natively.
- **Tab order** was identical to native for GTK 3, GTK 4, Qt 5, Qt 6, Java and Electron.
- **Apps without a tree** did better than natively. Tk: 3/4 facts. Motif and xcalc: 4/4 (native 1/4, only the key echo). On GNOME's Xwayland they are completely silent.
- **Settings carry-over** was equal to native for GTK 3, GTK 4, Qt 5, Qt 6, Java and Electron. Tk and Motif also get high contrast through the proxy.
- **Final proxy (0.1.0):** unit tests 24/24. The full parity suite with the release bridge reproduced these results. GTK 4's focus-order check counted 9 events against 10. This is an artefact of the test check after a fix. Speech and effects were identical.
- **Native comparison (no bridge):** on GNOME's own Xwayland, GTK 3, Qt 5, Qt 6, Java and Electron lose nothing against native Xorg. GTK 4 on GNOME 46 Wayland loses Orca's key echo. Tk, Motif and xcalc are completely silent.

## Known gaps

- Key repeat through the proxy runs at about 2/3 of the native rate. Holding a key for 1.5 s gave 23 characters against 35-36.
- GTK 4 apps must be started through the proxy (or with `AT_SPI_BUS_ADDRESS` set to the engine's bus). Otherwise they register on the desktop's bus.
- An assistive technology cannot move the focus to a mirror (GTK 4 refuses GrabFocus).
- Java's AT-SPI Value on buttons and check boxes is not mirrored.
- Electron and web content is mirrored as a group. Orca's browse mode and structural navigation are not available through it.
- Motif's own Tab traversal is not followed. The order is geometric.
- Motif gadgets (the OK, Cancel and Help buttons of standard dialogs) are not mirrored.
- Orca on GNOME 46 Wayland loses GTK 4 key echo. This happens natively, not only through the proxy.
- Without `-reverseinput` the proxy refuses clicks.
- The proxy restarts its AT-SPI reader if it freezes (at most 3 restarts a minute).

## Without the proxy

An app on the engine registers on the desktop's accessibility bus. Orca may then follow a window nobody can see.

With `-showwindows` the app's windows are visible and their accessibility is Xwayland's. In the tests where the app stays entirely on the engine, Orca's words were identical to Xwayland's for Java and Qt 6. `-showwindows` is experimental. Settings such as high contrast reach engine apps only with the proxy's `--settings-only` mirror (3/3 with it, 0/3 without).

## `-a11ybus`: a private accessibility bus for the display

Without anything on their X display that names one, toolkits find their AT-SPI bus through the session D-Bus. Apps on the engine then register on the desktop's bus. A screen reader follows windows nobody can see and reads them while the keyboard goes elsewhere.

With `-a11ybus` the bridge starts a `dbus-daemon` with at-spi2-core's `accessibility.conf` (the registry is activated on it) in its private 0700 directory. It announces the bus in two ways:

- As the `AT_SPI_BUS` property on the engine's root window. GTK 3 and Qt read it before asking the session bus (measured). Electron and Java/ATK use the same libatspi lookup and do the same.
- As `AT_SPI_BUS_ADDRESS` in the wrapped command's environment. GTK 4 needs this variable. It asks the session bus first and reads the root property only when that fails. A GTK 4 app started on the engine any other way (a terminal with `DISPLAY=:20`, a service) still registers on the desktop's bus (measured). `x11-a11y-proxy -- COMMAND` sets the variable too. The proxy reports a GTK 4 app it finds on the desktop's bus.

The bus stops with the bridge. It needs at-spi2-core (`/usr/share/defaults/at-spi2/accessibility.conf`) and `dbus-daemon`. Without them the bridge warns and goes on without the bus.

## `-reverseinput`: a master device for input that comes from the desktop

The engine's core pointer and keyboard carry the display clients' input (VNC viewers, automation) to the desktop. They must reach no X window there. Without this flag the agent keeps an input-only window over the whole screen for that, which also swallows other devices' clicks. With the flag:

- The agent creates an XInput2 master device `xwlbridge-reverse` (pointer and keyboard, each with an XTest slave).
- The input shield stays down. Instead the agent holds an XInput2 grab of the core pointer. It focuses the core keyboard on an input-only window of its own, re-asserted every 500 ms. The core devices still reach no X window.
- Raw events from the reverse master are never forwarded to the desktop, because they are the desktop's own input coming back. `XWLBRIDGE_REVERSE_ECHO=1` turns that off for testing.

A client drives it by making the reverse master its ClientPointer (XISetClientPointer with window None) and using core XTest. It focuses an app's window with XISetFocus on the reverse keyboard. The engine needs Composite (Xvfb and TigerVNC's Xvnc have it). Otherwise the shield stays and a warning is logged.

Both flags are off by default.

## Testing

Unit tests for the proxy (need PyGObject with the GTK 4 and Atspi typelibs):

```sh
python3 -m unittest discover -s tools/x11-a11y-proxy/tests -p test_unit.py
```

There are 24 tests. Container tests are in `tools/x11-a11y-proxy/tests/*.sh`. They need a GNOME test-bed image; see [tools/x11-a11y-proxy/tests/bed/README.md](../tools/x11-a11y-proxy/tests/bed/README.md).
