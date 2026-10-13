# RealVNC's own UI on a GNOME desktop (`-uidisplay`)

RealVNC Server starts its UI process (`vncserverui`) itself, on the X display it remotes. Under xwlbridge that
display is the bridge's, where nobody sees it, so the tray icon, the status window, Options, the Information
Center, connection notices and accept/reject prompts, chat and file transfer would all be out of reach. So
xwlbridge puts that UI on the desktop's own X server — on GNOME, mutter's Xwayland — where it is an ordinary X
application. That is the default when `vncserver-x11` is started from the desktop session (a local `DISPLAY`,
as a user unit gets it from GNOME):

```sh
# from inside the GNOME session (DISPLAY and XAUTHORITY are mutter's Xwayland)
xwlbridge -backend gnome vncserver-x11             # = -uidisplay auto
xwlbridge -backend gnome -uidisplay :1 vncserver-x11
xwlbridge -backend gnome -uidisplay none vncserver-x11   # keep the UI on the bridge's display, out of sight
```

What you get on GNOME (checked on Ubuntu 26.04, GNOME Shell 50.1, RealVNC Server 6.11):

| RealVNC UI | Where it shows | Notes |
|---|---|---|
| Status icon (tray) | top bar, through the AppIndicator extension's legacy tray (`ubuntu-appindicators@ubuntu.com` on Ubuntu, on by default; `appindicatorsupport@rgcjonas.gmail.com` elsewhere) | left click: status window; right click: RealVNC's menu (Open, Information Center, Options, Technical Support, About, Disconnect VNC Viewers, Chat, File Transfer, Connect to Listening VNC Viewer, Guest Access, Check for updates, Stop VNC Server) |
| Status window, Options, Information Center, About | ordinary windows, decorated by mutter | Options changes apply at once (e.g. a new port: the listener moves) |
| Connection notices | top right, for a few seconds | |
| Accept/reject prompt (Options > Connections > "Show accept/reject prompt") | a dialog; GNOME may announce it as "… is ready" instead of raising it | |
| Chat | both ways with the viewers | |
| File transfer | "File Transfer… > Send files…" sends to the viewers; files viewers send land in ~/Desktop (the dialog's "Fetch files to") | |
| Clipboard | through mutter's own X ⇄ Wayland clipboard sync | |

RealVNC draws the tray icon with GTK 2, which it loads at run time (its package does not depend on it): on
Ubuntu, `sudo apt install libgtk2.0-0t64`. Without GTK 2 there is no icon; everything else works.

Without a system tray (vanilla GNOME without the AppIndicator extension, or its legacy tray turned off) the
status icon has nowhere to go; xwlbridge says so in its log. `vncserver-x11 -showstatus` — what RealVNC's own
`realvnc-vncserverui-user.desktop` entry runs — opens the status window, whose menu button has the same menu.

## Configure RealVNC in its config file, not on the command line

RealVNC treats parameters given on its command line (`vncserver-x11 -RfbPort=5900 ...`) as preferences of the
running server only: the first time the Options dialog applies a change, the server reloads its settings from
its config files and **the command-line values are gone** (for example `-Authentication=VncAuth` falls back to
system authentication). The Options dialog also shows only what is in the files. So keep the settings in
`~/.vnc/config.d/vncserver-x11` — the file the dialog edits — and run `vncserver-x11` with no parameters:

```
# ~/.vnc/config.d/vncserver-x11 (mode 0600)
RfbPort=5900
Authentication=VncAuth
Encryption=AlwaysOn
```

(`vncpasswd -user` sets the VNC password there; the Options dialog can too.) xwlbridge warns when `-uidisplay` is combined with RealVNC parameters on the command line.

## How it works, and what it changes

RealVNC has no parameter for the UI's display: its core (`vncserver-x11-core`) starts `/usr/bin/vncserverui`
itself with `DISPLAY` set to the remoted display, and talks to it over stdin/stdout. xwlbridge therefore adds a
5 KB preload hook (built from `uihook/uihook.rs`, written to `$XDG_RUNTIME_DIR`) that, in `vncserverui` only,
points `DISPLAY`/`XAUTHORITY` at the UI display before `main`. The setuid launcher `vncserver-x11` would drop
the preload (glibc's secure-execution mode), so xwlbridge starts the core it would have launched,
`vncserver-x11-core`, directly. Consequences:

- **No RealVNC root helper.** Remote printing has to add a CUPS queue as the user, which works for members of
  the `lpadmin` group (Ubuntu's administrators are). System authentication (`Authentication=SystemAuth`) works
  for the desktop user's own account only; other local accounts are refused even when Permissions list them.
  VNC password authentication (`VncAuth`) is unaffected.
- **PRIMARY.** RealVNC's UI now reads and sets the desktop's PRIMARY selection itself (as on an X11 desktop):
  text merely highlighted on the desktop reaches viewers, and a viewer's clipboard also replaces the desktop's
  primary selection. `-noprimary` only covers the bridge's own clipboard bridging; RealVNC's `SendPrimary=0`
  does not stop it, only `SendCutText=0` (no clipboard to viewers at all) does.
- RealVNC's first-run privacy prompt now shows on the desktop once (both boxes come ticked).

Before starting RealVNC, xwlbridge connects to the UI display with the authority it will hand RealVNC (waiting up
to 30 s for a session's Xwayland) and checks for a system tray. If the display cannot be used yet, RealVNC starts
anyway (remote access does not wait) with its UI on the bridge's display, out of sight but working (RealVNC's
clipboard and prompts go through it), and xwlbridge moves the UI to the desktop as soon as the display answers
(it tries every 15 s; the move restarts the UI process, so it waits while a viewer is connected). If the UI keeps
exiting on the desktop's display (it went away, or refuses the UI: four restarts within a minute), new UI
processes go back to the bridge's display until it answers again, at most four moves an hour. The hook reads
that choice from a small gate file next to it (`xwlbridge-ui-<pid>.gate`), removed when xwlbridge exits. The hook is
written only into a directory owned by the user and writable by no one else, never through a symlink, and an
existing copy is reused only if it is the user's own, unwritable by others and byte-identical.

**The built-in engine.** RealVNC's UI must never run on the built-in engine's display as RealVNC leaves it: that
X server has no XKEYBOARD extension, and `vncserverui` 6.11 builds the keyboard layout name it reports to its core
from `XkbGetNames`, gets none, and dereferences the NULL — a segfault on every start (vncserverui+0x1ead5b), which
RealVNC answers by starting it again every 2 s, each crash running the system's crash reporter as root and leaving
the UI's tray helper behind. So with the built-in engine xwlbridge always runs RealVNC's core with the hook — also
with `-uidisplay none` and outside a desktop session — and in `vncserverui` the hook answers that request with the
layout name "unknown" (what RealVNC itself reports when it can't tell). The UI then runs on the bridge's display
like on any other X server. This needs `vncserver-x11` itself as the wrapped command (see below), and loses the
root helper (below); the stock engines (`-engine xvnc`/`xvfb`) have XKB and keep RealVNC's own launcher when the
UI stays on their display.

RealVNC's tray icon is a separate GTK2 helper of its UI process (`vncserverui -statusicon`). On GNOME it was
seen to die once of an X error (BadWindow) and RealVNC never starts it again, so xwlbridge watches it: if the
helper of a UI process that had one is gone, xwlbridge ends that UI process — RealVNC's core starts a new one, with
a new icon — but only while no viewer is connected (so no file transfer or chat is cut off), and at most three
times an hour.

Use `vncserver-x11` itself as the wrapped command: through `env`, a shell or another wrapper, the setuid
launcher runs in between and drops the hook (xwlbridge warns; with the built-in engine the UI then crash-loops).
