# Accessibility of apps on the bridge's display (`-a11ybus`, `-reverseinput`)

An X app can run entirely on the bridge's stock-engine display (`-engine xvnc` or `xvfb`) and still be usable with a
screen reader on GNOME, through a GNOME-side proxy (`tools/x11-a11y-proxy` in the parent project) that shows each
of its windows on the desktop with accessible mirrors of its controls. Two flags prepare the engine for that.

## `-a11ybus`: a private accessibility bus for the display

Without anything on their X display that names one, toolkits find their accessibility (AT-SPI) bus through the
session D-Bus, so apps on the engine register on the desktop's bus: a screen reader then follows windows nobody can
see, and reads them while the keyboard goes elsewhere. With `-a11ybus` the bridge starts a `dbus-daemon` with
at-spi2-core's `accessibility.conf` (the registry is activated on it) in its private 0700 directory, and announces
it:

- as the `AT_SPI_BUS` property on the engine's root window (GTK 3 and Qt read it before asking the session bus --
  measured --, and so do Electron and Java/ATK, which use the same libatspi lookup);
- as `AT_SPI_BUS_ADDRESS` in the wrapped command's environment. GTK 4 needs that variable: it asks the session bus
  first and reads the root property only when that fails, so a GTK 4 app started on the engine any other way (a
  terminal with `DISPLAY=:20`, a service) still registers on the desktop's bus (measured).
  `x11-a11y-proxy -- COMMAND` sets it too, and the proxy reports a
  GTK 4 app it finds on the desktop's bus.

The bus stops with the bridge. Needs at-spi2-core (`/usr/share/defaults/at-spi2/accessibility.conf`) and
`dbus-daemon`; without them the bridge warns and goes on without it.

## `-reverseinput`: a master device for input that comes from the desktop

The engine's core pointer and keyboard carry the display's clients' input (VNC viewers, automation) to the desktop
and must reach no X window there. Without this flag the agent keeps an input-only window over the whole screen for
that, which also swallows any other device's clicks. With it:

- the agent creates an XInput2 master device `xwlbridge-reverse` (pointer + keyboard, each with an XTest slave);
- the input shield stays down; instead the agent holds an XInput2 grab of the core pointer and focuses the core
  keyboard on an input-only window of its own (re-asserted every 500 ms), so the core devices still reach no X
  window;
- raw events from the reverse master are never forwarded to the desktop (they are the desktop's own input, coming
  back); `XWLBRIDGE_REVERSE_ECHO=1` turns that off for testing.

A client drives it by making the reverse master its ClientPointer (XISetClientPointer with window None) and using
core XTest, and focuses an app's window with XISetFocus on the reverse keyboard. Needs Composite on the engine
(Xvfb and TigerVNC's Xvnc have it); otherwise the shield stays and a warning is logged.

Both flags are off by default.
