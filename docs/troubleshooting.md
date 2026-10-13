# Troubleshooting

### Wayland compositor issues

This is the first thing you should check if you have issues.

Ensure `WAYLAND_DISPLAY` is set.

With `-backend wlr`, ensure your compositor implements the protocols listed under [How it works](../README.md#how-it-works) in the README (wlr-screencopy or ext-image-copy-capture, virtual pointer and keyboard, data-control). `-backend gnome` uses mutter's D-Bus APIs instead; `xwlbridge-ctl doctor` checks them.

Missing protocols will usually just cause the corresponding feature to become a no-op.

If you're using a Smithay-based compositor and compositor keybindings don't work, you'll need to use [wl-uinput-proxy](https://github.com/pgaskin/wl-uinput-proxy) to work around niri-wm/niri#403 and Smithay/smithay#1903. Note that this workaround does not work for nested compositors, only ones using the system input devices and outputs.

If your compositor does not implement a supported virtual input protocol, you can use [wl-uinput-proxy](https://github.com/pgaskin/wl-uinput-proxy) for that too. 

By default, the virtual input devices go on the first seat (or the one named by `-seat`). With `-seat transient`, xwlbridge asks the compositor for a seat of its own using `ext_transient_seat_v1`, so remote input has its own cursor and keyboard focus (the clipboard stays on the first real seat, since selections are per-seat). If the compositor doesn't support it or denies it, xwlbridge logs a warning and uses the first seat instead. This does not apply with wl-uinput-proxy, since uinput devices always go to the compositor's own seat.

If the local display is asleep (e.g., swayidle turned it off), the compositor can't capture it, and clients see a frozen screen. If the compositor supports `zwlr_output_power_manager_v1` (selected with `-pwrmgr`, default `auto`), xwlbridge turns an output back on when a client starts watching the screen, and again if it goes off while one is watching. It never turns outputs off.

With `-backend gnome`, the same is done through mutter's `org.gnome.Mutter.DisplayConfig` `PowerSaveMode` (`-pwrmgr auto` or `gnome`, the default, so this is on unless `-pwrmgr none` is given): the screen is turned on when a remote desktop session starts and when it goes into power save while one runs. When the viewers have gone (and when xwlbridge exits or is stopped by a signal) and mutter saw no input at all since then (no local user, no remote input), the power save mode it was in is set again; otherwise it is left on for the idle policy (gsd-power) to handle. Note that on a real gnome-shell an idle session also raises the screen shield, which blocks remote access until it is dismissed, so this mostly matters where the shield is not involved.

GNOME is supported through mutter's own D-Bus APIs (`-backend gnome`), without the portal. KDE Plasma is not supported (there is no backend for it): it prefers [`org.freedesktop.portal.RemoteDesktop`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) via [xdg-desktop-protocol](https://flatpak.github.io/xdg-desktop-portal/), which uses [PipeWire](https://pipewire.org/) for screen capture and [libei](https://libinput.pages.freedesktop.org/libei/) for virtual input.

### Ensuring the X server works with Xlib

This is the second thing you should check if you have issues. You can add `-verbose -xtrace` after `xwlbridge` for debug output.

```bash
# test handshake, extensions, props
xwlbridge xdpyinfo

# test display layout
# note: compare this to your real display layout
xwlbridge xrandr --query

# test basic keyboard stuff
# note: you can run wev to see events
xwlbridge xte 'sleep 1' 'str test' 'sleep 1'

# test the keymap as XKB clients see it (rules names, components, a full dump)
xwlbridge setxkbmap -query
xwlbridge setxkbmap -print
xwlbridge sh -c 'xkbcomp -xkb "$DISPLAY" - | head'

# test typing through XKB (xdotool needs it)
xwlbridge xdotool sleep 1 type 'Hello World!'

# test shifted keys
# note: you can run wev to see events
xwlbridge xte 'sleep 1' 'str TeSt' 'sleep 1'

# test non-compositor keyboard shortcut
# note: change this to something harmless but visible
# note: you can run wev to see events
xwlbridge xte 'sleep 1' 'keydown Control_L' 'key Tab' 'keyup Control_L' 'sleep 1'

# test compositor keyboard shortcut
# note: change this to something harmless but visible
# note: you may need to prefix it with wl-uinput-proxy
xwlbridge xte 'sleep 1' 'keydown Super_L' 'key d' 'keyup Super_L' 'sleep 1'

# test shifted compositor keyboard shortcut
# note: change this to something harmless but visible
# note: you may need to prefix it with wl-uinput-proxy
xwlbridge xte 'sleep 1' 'keydown Super_L' 'keydown Shift_L' 'key d' 'keyup Shift_L' 'keyup Super_L' 'sleep 1'

# test shifted compositor keyboard shortcut with pre-shifted character
# note: change this to something harmless but visible
# note: you may need to prefix it with wl-uinput-proxy
xwlbridge xte 'sleep 1' 'keydown Super_L' 'keydown Shift_L' 'key D' 'keyup Shift_L' 'keyup Super_L' 'sleep 1'

# test x-to-wayland clipboard (i.e., ctrl+c/ctrl+v)
xwlbridge sh -c "echo '> copy'; date | tee /dev/stderr | xclip -selection clipboard; echo '> paste'; WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-paste"

# test x-to-wayland primary clipboard (i.e., middle-click)
xwlbridge sh -c "echo '> copy'; date | tee /dev/stderr | xclip -selection primary; echo '> paste'; WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-paste --primary"

# test wayland-to-x clipboard (i.e., ctrl+c/ctrl+v)
xwlbridge sh -c "echo '> copy'; date | tee /dev/stderr | WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-copy; echo '> paste'; xclip -selection clipboard -out"

# test wayland-to-x primary clipboard (i.e., middle-click)
xwlbridge sh -c "echo '> copy'; date | tee /dev/stderr | WAYLAND_DISPLAY=$WAYLAND_DISPLAY wl-copy --primary; echo '> paste'; xclip -selection primary -out"

# test screen capture
# note: if you see "screen capture started" after "capture", increase the sleep time
xwlbridge -verbose -profile sh -c "for x in 1 2 3; do echo \$x; xwd -root -silent >/dev/null; sleep 1; done; echo capture; xwd -root -silent | magick xwd:- png:/tmp/screen.png"

```

### Log messages

#### xwlbridge

If you're using `ext_image_copy_capture_v1` and see a mesage like `ext capture frame failed (buffer-constraints)` after changing outputs, it's fine as long as it also doesn't stop working.

If you see a warning about `client selected events we don't deliver`, it's usually harmless unless your problem is directly related to a listed event.

If you see a message like `unhandled request`, it needs to either be stubbed or implemented in [x11/conn.rs](../src/bridge/x11/conn.rs).

`released N key(s)/button(s) a disconnected X client was holding` (every engine): an X client disconnected or
crashed with keys or mouse buttons down, and the bridge let them go, so that nothing stays stuck on the desktop
(the compositor counts a key as down while any device holds it, so a stuck remote Ctrl would also turn the local
user's typing into shortcuts). A client connected longer than 1 s has them released at once. A *one-shot* client,
one that disconnects within 1 s of connecting, is treated as on Xorg instead: what it leaves held stays down
for the next client to use and release, as `xdotool keydown Shift_L; xdotool key 8; xdotool keyup Shift_L`
(three processes) and RustDesk, which injects every key event through an X connection of its own, rely on. It goes
up by itself once no X client has injected anything for `-xtestgrace` milliseconds (default 1000; input from any
client keeps it down, so Ctrl held in RustDesk while clicking stays down) and never later than 10 s after its client
left; the message then ends `(a one-shot client's; no X client released them in time)`. With `-verbose`,
`one-shot X client ... left after N ms: its N key(s)/button(s) stay down` marks the start. Scripts that hold a key in one
process and pause longer than that before the next (`xdotool keydown ctrl; sleep 5; xdotool keyup ctrl`) need
`-xtestpersist`, which keeps whatever any client leaves held until a client releases it, as Xorg does (on every
engine); `-xtestgrace 0` releases even a one-shot client's keys at once (the earlier behaviour).

With `-backend gnome` and the stock engines (`-engine xvnc`/`xvfb`):

- `xvfb: agent stopped: ...` then `xvfb: restarting the agent`: the agent (the bridge's X client of the stock
  X server) lost its connection while the X server lives on (an X error, a client killing it); whatever it held
  is released on the desktop and a new agent takes over.
- `xvfb: the X server is gone; exiting` (exit status 69), or `the agent stopped N times in 60s; exiting`: the
  stock X server (`-engine xvnc`/`xvfb`) died. The bridge stops the wrapped command (RealVNC Server would
  otherwise wait for its display forever) and exits; run it under a service manager that restarts it
  (`Restart=on-failure`). `gnome: the backend thread panicked` exits the same way with status 70.
- `the compositor hasn't read input for Ns` / `input came faster than the compositor read it`: mutter wasn't
  reading the input socket (a stall, or an X client injecting faster than it reads). Input is held back and
  pointer motion and wheel notches coalesced meanwhile (a merged wheel event scrolls at most 120 notches: mutter
  replays a scroll notch by notch). At most 65536 events are held: past that, pointer motion and wheel
  notches between key/button presses and releases are merged first (each click keeps its position), and only
  then whole taps are dropped (a press with its release: a keystroke, a click, a whole drag; never a lone press
  or release, so nothing is left down). Pointer motion reaches mutter at most 500 times a second whatever
  input it is mixed with (after a burst of up to 16 when a click or key follows a move), so mutter never
  takes an X client's flood for an unresponsive window; input behind a paced motion waits with it, in order.
  Human and VNC-viewer rates are never held back, but after an automation flood faster than that (an XTest
  client sending thousands of moves at once) a key or click queued behind it waits until the backlog has drained:
  about 4 s after 2000 moves, up to about 33 s at the 65536-event cap. Taps dropped at the cap are logged (`... key/
  button taps dropped`) whether or not mutter stalled.
- `screen sharing was stopped from GNOME`: the local user clicked Stop on the sharing indicator; sharing
  resumes once every viewer has been gone for 10 s (`-reclaim` takes it back at once). When gnome-shell closes
  the session (`closed by gnome-shell`), the bridge decides within 3 s what it meant: the screen shield coming up
  is a lock (`the screen locked and GNOME closed remote access ...`; sharing resumes after unlock, and viewers see
  a padlock meanwhile), a monitor change just before is that change (`the session closed with a monitor change;
  recreating it`), and anything else is the user's Stop.
- `xvfb: can't upload the compositor keymap ... (retrying)`: `xkbcomp` failed or is missing (package
  `x11-xkb-utils`); until it succeeds, X clients see a US keyboard and type wrong characters on other layouts.

Exit status, for a service manager (use `Restart=on-failure`): the wrapped command's own status normally; **69** when
what xwlbridge mirrors is gone for good: the stock X server of `-engine xvfb|xvnc` died (also when the wrapped
command, an X client, exited first because its display went), or (GNOME) the session bus closed xwlbridge's
connection (the connection broke, or its signal stream ended) and no stop request followed within 10 s. A D-Bus
call that only took longer than its 5 s timeout (a stalled gnome-shell) does not count: the bridge retries and
serves once gnome-shell answers. The command is stopped first in both cases. **70**: the GNOME backend failed.

On GNOME, `the screen locked and GNOME closed remote access: xwlbridge-lockscreen@xwlbridge is installed but not active`
means viewers get a padlock instead of the lock screen. GNOME turns all user extensions off after gnome-shell fails
soon after starting; check `gsettings get org.gnome.shell disable-user-extensions` (see
[contrib/gnome-shell](../contrib/gnome-shell/README.md)).

#### RealVNC

If RealVNC logs `TcpListenerManager: not accepting connections: failed to listen on at least one address`, its port
(or one address of it, e.g. `[::]`) was in use when it started. RealVNC Server 6.11 does not retry: it keeps running
without listening until it is restarted. Find the other listener (`ss -ltnp 'sport = :5900'`) and restart the service.

On a machine with RealVNC Server, `sudo apt install tigervnc-standalone-server` (for `-engine xvnc`) also repoints
`/usr/bin/vncserver` (RealVNC's link to its virtual-mode `vncserver-virtual`) to TigerVNC's `vncserver` through
update-alternatives; RealVNC's `/usr/bin/Xvnc`, `vncserver-x11` and its service mode are not touched, and xwlbridge
never runs RealVNC's Xvnc. If you use RealVNC's virtual mode, put it back with `sudo update-alternatives --remove
vncserver /usr/bin/tigervncserver && sudo ln -sfn vncserver-virtual /usr/bin/vncserver`, or use `-engine xvfb`
(`sudo apt install xvfb`, no alternatives involved; a fixed screen size) instead.

To get more verbose logs, add the `-Log *:stderr:100` RealVNC parameter. You can also filter the logs (`-Log` is comma-separated), see `vncserver-x11 -help all`.

### Exit status (for service managers)

xwlbridge's exit status tells a service manager (systemd's `Restart=on-failure`, docker's `--restart
on-failure`) whether to start it again:

| status | when |
|---|---|
| 0 | a stop was asked for (SIGTERM, SIGINT or SIGHUP: `systemctl stop`, Ctrl+C) and the wrapped command ended because of it (it exited 0, or died of / exited 128 + one of those signals), or `-nowrap` was stopped; also when the command exits 0 by itself. Whatever else goes down during a stop (a stock X server a service manager signals along with us) is expected |
| the command's | the command exited by itself; 128 + n when it was killed by signal n (not one we passed on for a stop) |
| 69 | the stock X server (`-engine xvnc`/`xvfb`) died (also when the command, an X client, exited first because of it), or its agent keeps failing; or (GNOME) the session bus closed our connection and no stop came within 10 s; the command is stopped first |
| 70 | the GNOME backend failed (`gnome: the backend thread panicked`); the command is stopped first |
| 1, 2 | startup failure (no display, X server won't start, command can't be spawned); 2 is a usage error |

A command that reports a failure of its own while stopping still exits with it. During a stop, 69 and 70 are not
reported (the stop wins).

So a systemd unit running xwlbridge (the packaged `xwlbridge-realvnc.service`, or your own `-nowrap` unit `PartOf=` and
`WantedBy=graphical-session.target`) ends `inactive`, not `failed`, after `systemctl stop` and after a logout,
and needs no `SuccessExitStatus=`. Older builds could exit non-zero when stopped (143 = 128 + SIGTERM with
`-nowrap`), which left such a unit `failed` after every logout; adding `SuccessExitStatus=143` is not the cure, since
143 is also the status of a wrapped command killed by SIGTERM from elsewhere, which `Restart=on-failure` should
restart. A wrapped command that exits non-zero on SIGTERM (x11vnc exits 2) makes every stop a failure: wrap it in a
script that exits 0 then.

### Dynamic resolution

Dynamic resolution needs `-dynres`. With `-backend wlr` it also needs a compositor which supports `zwlr_output_manager_v1` (selected with `-outmgr`, default `auto`), and for RealVNC, the [hook](../dynres/README.md); GNOME uses DisplayConfig instead (see [GNOME](#gnome) below). On wlroots it is meant for a headless or nested compositor (e.g., `WLR_BACKENDS=headless sway`), where the output supports arbitrary modes.

When X client (vncagent-x11, or `xrandr`) sets the CRTC to a new mode, xwlbridge attempts to change the output's mode and waits for it before replying to the X request.

To test it without RealVNC:

```bash
# note: replace HEADLESS-1 with the output name from `xwlbridge xrandr --query`
xwlbridge -dynres -verbose sh -c 'xrandr --newmode 1024x640 0 1024 0 0 0 640 0 0 0; xrandr --addmode HEADLESS-1 1024x640; xrandr --output HEADLESS-1 --mode 1024x640; xrandr --query'
```

If it says `Configure crtc 0 failed`, check the xwlbridge log for:

- `dynamic resolution is off (see -dynres)`: run with `-dynres`.
- `output configuration is off (see -outmgr)`: `-outmgr none` was given.
- `the compositor has no zwlr_output_manager_v1`: the compositor does not support changing output
- `the compositor accepted ... but the output did not change`: the compositor accepted the configuration but ignored it (nested niri does this since it always uses the window size).
- `the compositor rejected ...`: the compositor refused the mode (probably because the output doesn't support it)


Note that RealVNC will set all CRTCs to the requested size, but will not update the position, so outputs may overlap with multiple monitors unless the compositor re-arranges them automatically (niri does, and so does sway for outputs without an explicit `position`).

On a rotated output, the mode is set in the native panel orientation, and on a scaled output the size is in physical pixels, as everything else in RandR.

RealVNC disables the CRTC and resizes the screen before it asks for the new mode, and does not undo either if that fails. As a workaround, xwlbridge restores the original output configuration if that happens (and logs `restored the screen to the compositor's outputs`).

#### GNOME

With `-backend gnome`, `-dynres` uses `org.gnome.Mutter.DisplayConfig` (`-outmgr auto` or `gnome`): the monitor is switched to one of its **own** modes of exactly the requested size with `ApplyMonitorsConfig` method 1 (temporary: not saved, no "Keep changes?" prompt), and monitors to its right or below move with its edge so the layout stays valid. Every monitor keeps its underscanning, colour mode (HDR) and RGB range, and a fixed-rate mode stays fixed-rate (VRR modes are only picked for a monitor already running one). mutter has no way to add a mode over D-Bus, so a size the monitor does not list is refused (`monitor Meta-0 has no 1366x768 mode (it has ...)`). A headless `--virtual-monitor` has exactly one mode; a `ScreenCast.RecordVirtual` monitor created with a `modes` list has those.

The layout the change replaced is put back once someone has watched since it was made and nobody watches any more (after the usual 10 s linger), and when xwlbridge exits — normally, or on SIGTERM/SIGINT/SIGHUP, also with `-nowrap` (not on SIGKILL: then the temporary layout stays until the next monitor change or login). It is left alone if the monitors were reconfigured some other way in the meantime (the local user, a hotplug, a changed colour mode). `-dynreskeep` keeps the new resolution instead. Without `-dynres` nothing is ever changed: the X client's request fails. A monitor is never turned off: every enabled monitor is part of each configuration, and a layout whose monitors or modes are gone is not put back.

With `-engine xvnc`, RandR requests go to Xvnc itself, which grants them; once a client's new screen size has stood for 200 ms (xrandr passes through an intermediate size), the agent carries it to the desktop's monitor (single-monitor desktops only; one request at a time, the newest size wins), and about a second after the client's last change puts the X screen back to the desktop's size if the desktop can't follow, or without `-dynres`. An output a client turned off (RealVNC does so first in a resize; `xrandr --off`) is turned back on then. Xvfb has a fixed size: a client may shrink it with RandR, and the agent puts it back the same way. On the built-in engine, a request that reaches the desktop and fails is rolled back at once (see above), and outputs an X client left off, moved or at another size without asking the desktop are put back after a second.

Which viewers ask for a new size (measured with RealVNC Server 6.11 in user mode on xwlbridge, on headless mutter 50.1):

| viewer | server | resize request reaches X |
|---|---|---|
| RealVNC Viewer 6.22 | Server 6.11 | no: the viewer has no dynamic resolution |
| RealVNC Viewer 7.15.1, `DynamicResolution=1` | Server 6.11 | no: the viewer offers its `DynamicResolution` extension, Server 6.11 does not enable it |
| TigerVNC viewer 1.15, `RemoteResize=1` | Server 6.11 | no: Server 6.11 does not know ExtendedDesktopSize (`[unknown encoding -308]`), so no `SetDesktopSize` is sent |
| any | Server 6.11 with `RandR=1024x768,1600x900` | the listed sizes are added to the X output as modes (`1024x768_vnc`); an X client selecting one (`xrandr --output VNC-0 --mode 1600x900_vnc`) goes through `-dynres` |
| RealVNC Viewer 7.x | Server 7.x in user/service mode + the [hook](../dynres/README.md) + a licence with dynamic resolution | yes: RRSetScreenSize + RRSetCrtcConfig (that request sequence was replayed and works on every engine; not run against a real 7.x server) |

Server 6.11's own display-mode support is that `RandR` list (its `-geometry` is refused in user mode); its virtual mode runs RealVNC's own X server instead of showing the desktop, so it is no option here.

Also, note that different compositor implementations validate the refresh rate for a custom mode differently (wlroots' nested backends reject any but 0, but niri ignores 0), so xwlbridge tries both.

### Performance issues

Try `-profile` with and without `-nodamage`.

Note that RealVNC, by default, tests XDAMAGE for 6 seconds at startup if available and will only continue to use it if enough updates are seen and it's faster than it's own damage tracking. You can also force it with the [`CaptureMethod`](https://help.realvnc.com/hc/en-us/articles/360002251297-RealVNC-Server-Parameter-Reference) vncserver parameter.


### New RealVNC versions

- Confirm clipboard handling.
- Enumerate IPC features to see if there's anything major which might need to be handled.
- Look at `vncagent-x11` and `vncserver-x11-core`, confirm the X methods, extensions, and events it uses.
- Test everything.

### Keyboard (XKB) limitations

The built-in X server serves the compositor's keymap through the core keyboard mapping and an XKEYBOARD extension
that describes it but can't change it (`-noxkb` hides the extension; clients then use the core mapping alone, and
`xdotool` crashes without it).

- The X keyboard is **one layout** of the compositor keymap: the one the compositor is in, served as XKB's only
  group. XTest keycodes are interpreted by the compositor in whatever group *it* is in, and the bridge cannot
  switch that, so the X side follows it instead. GNOME switches between up to four input sources by locking a
  group of one keymap (no new keymap); with the GNOME backend the bridge follows that switch while it has a
  session (mutter reports the group to its input connection), and X clients are told about the new keyboard as
  for a new keymap. Between sessions the last known group is kept, and input typed while no session runs is
  replayed on the keys that have its keysyms in the layout the next session reports (a switch made meanwhile is
  followed; level-1 keysyms). (GNOME's keymap for a single `de` source is `pc+de+us:2+inet(evdev)`: the `us`
  group 2 is GNOME's shortcut fallback; it is never served, but shortcuts use it, see below.) The wlroots backend
  always serves the first layout.
- The compositor's keyboard can't be changed from X: `setxkbmap de`, `xkbcomp file $DISPLAY`, XKB SetMap and
  remapping a keycode the layout uses (`xmodmap -e 'keycode 38 = b'`) fail with BadImplementation (or find
  nothing). Change the layout in the compositor (GNOME Settings, input sources); X clients are told about the new
  keymap (XkbNewKeyboardNotify, XkbMapNotify and MappingNotify) and reload it.
- **Spare keycodes** (ones the layout leaves empty) take keysyms with `ChangeKeyboardMapping`, as xdotool, x11vnc,
  pynput and RealVNC Server do for a keysym they find nowhere else. Pressing such a keycode types its keysym where
  the current layout has it, at any level: the bridge presses that key with the modifiers its level needs (Shift,
  ISO_Level3_Shift for AltGr), and lets the modifiers go before the client's next key. The stock engines
  (`-engine xvfb`/`xvnc`) do the same with the keysyms their X server shows on those keycodes.
- **Shortcuts with a non-Latin layout active** (Ctrl+C, Ctrl+V, Ctrl+Z, ... with `ru`, `uk`, `el`, `he`, `ar`):
  with Control, Alt or Super held, a keysym the current layout lacks but another layout of the compositor's
  keymap has is typed on that layout's key, pressed as it is. The compositor applies its current layout there
  (Ctrl + Cyrillic_es), and GTK, Qt and GNOME match the shortcut on that key, exactly as for a local user. GNOME
  keeps a Latin layout in every keymap for this. Without such a modifier the keysym is not typed (it would come
  out as the current layout's character on that key).
- **Characters that the current layout does not have cannot be typed from X**: `xdotool type 'é'` on a
  `us` layout types nothing for the `é` and still **exits 0** (the bridge logs the keysym once per client). Scripts
  that check xdotool's exit code will not notice; type only what the layout has, or switch the compositor layout
  first. Levels reachable only through a locking modifier (keypad digits with NumLock off) are not typed either.
- The core keyboard mapping (`GetKeyboardMapping`, `xmodmap -pke`) is laid out as Xorg's: levels 1 and 2 in
  columns 0 and 1, AltGr levels in columns 4 and 5 (7 keysyms per keycode), so tools that pick modifiers by column
  (xte, x11vnc) press ISO_Level3_Shift for AltGr characters. Unlike Xorg, columns 2 and 3 (group 2) are left empty
  rather than repeating group 1. With `-noxkb` the previous four-column layout is served instead (AltGr levels in
  columns 2 and 3), which core-only clients such as RealVNC without XKB expect.
- x11vnc picks its XKB mode by itself when the keymap is there at its start (a desktop with a keyboard); on a
  headless seat the keymap only comes with the first session and x11vnc stays in its core mode, which reaches AltGr
  characters through spare keycodes. Both type a German layout exactly.
- `GetState` reports no modifiers or LEDs (the compositor's lock state is not followed), geometry is not served
  (`xkbcomp -xkb` warns about it), and key behaviours and radio groups are empty.
- `WarpPointer` moves the desktop's pointer like an XTest motion. Every window of this server is at the root's
  origin with the root's size, so window coordinates are screen coordinates; the source-rectangle condition is
  checked against the pointer position the server reports (`QueryPointer`).

### Clipboard and window property limitations

The X server does not implement stuff needed for displaying real windows, so some things don't work:

- Window properties are per connection. Properties set by one client are not visible to other clients. Tools that pass data between clients through root-window properties (cut buffers, `RESOURCE_MANAGER`) will not work. Atoms are global.
- `CLIPBOARD` and `PRIMARY` are the only fully functional selection types, and are always fetched by xwlbridge (as a `UTF8_STRING`) immediately after taken by a client, and published as a wayland selection. Changes to the selection after ownership is taken are not handled. There is a timeout of 10 seconds for fetches to stop misbehaving applications from stalling it. This works fine for RealVNC and most other VNC servers.
- At most 254 X clients can be connected at once, as with Xorg (X resource ids have 29 bits, and each client gets 2^21 of them). The next one is refused with `Maximum number of clients reached` and the log says `refusing an X client: all 254 client slots are in use`; a slot is free again as soon as any client disconnects, so hitting this usually means a script leaks connections.

### Generic X tools on the built-in server

The built-in server answers what screenshot, recording, automation and hotkey tools ask at start-up, but some of it only so that they keep running:

- `Bell` (scrot rings one per screenshot) rings the compositor's system bell, through `xdg_system_bell_v1` on a Wayland connection of its own, made by the first bell. On a compositor without that global, a bell does nothing. Bells closer together than 100 ms ring once.
- Passive grabs (`GrabKey`, `GrabButton`: xbindkeys, sxhkd, push-to-talk) are accepted and **never fire**: the keys and buttons you press go to the compositor, not through this server.
- `SetScreenSaver` is kept so that `xset q` shows what was set, and `ForceScreenSaver` does nothing; the desktop blanks and locks the screen. There is no MIT-SCREEN-SAVER extension (no idle time for X clients).
- `RRSetCrtcGamma` (redshift, xgamma) is kept and read back, and never applied: use the desktop's night light.
- `RRGetMonitors` lists one monitor per enabled output, named after it, the desktop's primary one primary (OBS's XSHM source needs these); an output lists the modes the compositor offers for it (GNOME), but choosing another one is a resolution change, refused unless `-dynres`.

- `WarpPointer` (xdotool mousemove, Java Robot, PyAutoGUI) moves the desktop's pointer, as XTest motion does; not
  under `-viewonly`. Bells still ring under `-viewonly`.

For everything else (windows, drawing, fonts, XInput, RECORD, MIT-SCREEN-SAVER), use a stock X server with `-engine xvnc` or `-engine xvfb`.

Programs the bridge starts are pinned to X11 (`GDK_BACKEND=x11`, `QT_QPA_PLATFORM=xcb`, ...; see the README). On the
built-in server that changes where GTK 4 and Qt 6 children go: they used to open the desktop's Wayland and now open
this server, which lacks XInput2 and (without XKB) XKEYBOARD, so they abort (`Gdk-ERROR: XInput2 support not found on
display`, `qt.qpa.xcb: XKeyboard extension not present`). Run GUI programs on a stock engine, or on the desktop and
point only their capture here (docs/no-split.md).

### FAQ

- **How is this different than Xwayland?** \
  The built-in engine (from upstream xwlrvnc) is not based on the xserver source code (it's entirely from scratch), and its only purpose is to bridge the outputs/screen/clipboard/keyboard/keymap/pointer/cursor to Wayland. It does not support any window-management or drawing features. The stock engines (`-engine xvnc`, `xvfb`, `xwayland`) are complete X servers kept in step with the desktop by an agent. Unlike GNOME's own Xwayland, either way the root window is the whole desktop and injected input reaches every window.

- **How do I configure RealVNC?** \
  In `~/.vnc/config.d/vncserver-x11`, or with its Options dialog, which `-uidisplay` puts on your desktop. Command-line parameters are forgotten when RealVNC reloads its settings ([realvnc-server-6.md](realvnc-server-6.md)).

- **How do I stop my selection from constantly getting copied?** \
  RealVNC merges the two clipboards. You can inhibit the primary (i.e., selection) clipboard by adding the `-noprimary` option.

- **Why doesn't *\<RealVNC feature\>* work?** \
  See what was tested in [realvnc-server-6.md](realvnc-server-6.md). Also, ensure your RealVNC license supports the feature you're trying to use.

- **Can I use this with other VNC servers?** \
  Yes: x11vnc 0.9.17 and TigerVNC's x0vncserver were tested on the bridge (see the README). For a VNC server that speaks Wayland itself, there is also [w0vncserver](https://tigervnc.org/doc/w0vncserver.html).

- **Can I increase reduce latency / increase the FPS?** \
  Yes, as long as your network and compositor are fast enough, at the cost of higher CPU usage. Try `xwlbridge -fps 60 -nodamage vncserver-x11 -CaptureMethod=1 -PollInterval=10 -PollCursorTime=150 -CompareFB=TRUE`, which makes RealVNC do the screen diffing, sends frames at a higher rate, and increases the update interval for client-rendered cursors. Do not set `PollCursorTime` too short or it may drop cursor input randomly.

- **Can I completely disable the unsupported VNC Server features?** \
  Yes, as long as you have a RealVNC subscription which lets you control the required [parameters](https://help.realvnc.com/hc/en-us/articles/360002251297-RealVNC-Server-Parameter-Reference#permissions-0-80). Try adding `-ConnNotifyTimeout=0 -DisableTrayIcon=2 -EnableChat=FALSE -QueryConnect=FALSE -RecordQuery=FALSE` to the `vncserver-x11` arguments. This isn't really necessary though, those features will just do nothing if enabled since the GUI stuff is stubbed out.

- **Can I show a single monitor only?** \
  Yes, use the `-Monitor` RealVNC parameter. The XRandR output names match Wayland (you can check them with your compositor, or using `xwlbridge xrandr --query`).

- **Can I run multiple instances of RealVNC?** \
  Not with the packaged service: RealVNC runs one user-mode server per user, and a second one exits with `Existing server found; exiting.` (see [realvnc-server-6.md](realvnc-server-6.md)). Upstream's answer, for bridges you start by hand on a wlroots compositor, was to give each `vncserver-x11` its own `-RfbPort` and add `-newinstance` to the end of its command; this fork has not tested that.

- **Which injected (XTest) input reaches the desktop?** (`-engine xvfb`/`xvnc`) \
  Input an X client injects is meant either for the desktop or for windows that live on the bridge's own display (a tray proxy such as KDE's `xembedsniproxy` clicking an icon, Xpra's server, `xdotool` typing into a window it focused there). The agent tells them apart per client, through RECORD:
  - engine helpers, `-xtesthelpers` (default `xembedsniproxy,xpra`): never forwarded;
  - desktop drivers, `-xtestdrivers` (default: RealVNC's `vncagent`/`vncserver-x11(-core)`, `x11vnc`, `x0vncserver`, `vino-server`, `krfb`, TeamViewer's capture process (`TeamViewer_Desktop`), and the Barrier/Input Leap/Deskflow/Synergy clients): always forwarded, even while a window on the bridge display has the X focus or a grab;
  - everyone else (`xdotool`, PyAutoGUI, Java's `Robot`, scripts): forwarded unless the X server sends it to another client's window on the bridge display (a key while such a window has the X focus or the keyboard grabbed, a click or motion while the pointer is grabbed, a click over such a window).

  Names are matched against the program's `comm`, executable and `argv[0]` (and the script, for `python3 script.py`); lists are comma-separated, `default` stands for the built-in list and `none` for an empty one, and `-xtestdrivers all` turns the window check off. With `-verbose`, the log says how each client was classified and why input stayed on the display. The built-in engine drops engine helpers' input (it has no client windows to send anything to).

  This keeps input meant for the bridge display from landing on the desktop by accident; it is not a security boundary (a program can be named anything, and any client of the bridge display may inject input). Known limits: a passive button grab (a hotkey daemon's mouse binding) is seen only after its click, which is then forwarded; a release always goes through, so a helper's release of a button another client is dragging with ends that drag; input that can't be paired with its request (no RECORD) is forwarded.

- **When does the bridge start GNOME's screen sharing, and its indicator?** (`-backend gnome`) \
  Only while something needs the desktop: an X client watching the screen (DAMAGE) or reading it (`GetImage`, `ShmGetImage`: scrot, xwd, ffmpeg x11grab, Java's `Robot`, PyAutoGUI), or injecting input (XTest). GNOME shows its sharing indicator for as long as that lasts:
  - the session stops 10 s (`-warm SECS`) after the last watcher, read or input, and 30 s (`-linger SECS`) after a client that read the screen went away, so a burst of one-shot screenshots finds it running;
  - `-alwayson` (= `-warm always`) keeps it up from start to stop, for hosts that need every first screenshot current (the indicator stays);
  - on `-engine xvfb`/`xvnc`, a client that connects while the session is stopped starts it at once (`-coldconnect start`, the default), so a slow reader (Java's `Robot`, ffmpeg) gets the current desktop; a fast one-shot tool (scrot, xwd, maim, import) reads within a few milliseconds of connecting and gets a **black** screen on its first shot after the session stopped (black, never an old picture: an idle stop paints the screen black; the person's Stop shows a pause picture), and current pictures from then on. A program whose connection started the session but neither read the screen nor injected input (xprop or xclip in a loop) no longer starts it by connecting. `-coldconnect none` starts the session only for reads and input;
  - on the built-in engine a screen read waits for the first frame (up to `-coldwait` ms, default 2000), so even the first shot is current;
  - input that arrives while no session runs waits for it (sessions come up in a fraction of a second; a client's input is dropped whole once any of it has waited 5 s, never typed without its beginning).

- **How does the clipboard work on GNOME with a stock engine?** \
  With `-engine xvfb`/`xvnc` and the desktop's Xwayland reachable (xwlbridge started from the session, with its `DISPLAY`), `-clipboard auto` relays selections X to X between the bridge display and GNOME's Xwayland, which mutter keeps in step with the Wayland clipboard: every type the owner offers (text, images, HTML, ...), CLIPBOARD and PRIMARY, large values in steps (INCR, up to 64 MiB), and no screen-sharing session or indicator needed. Every client of the bridge display can read and set the desktop's clipboard at any time, as any Xwayland client can; the bridge display lets in only the desktop user (by uid, or its cookie). `-clipboard session` uses mutter's remote-desktop clipboard instead (text only, CLIPBOARD only, only while a session runs), as the built-in engine does. The relay stays connected to the desktop's Xwayland, so where mutter closes Xwayland when no X client is left (`autoclose-xwayland`) it keeps it running (about 60 MB). Values in transit are capped at 128 MiB; an owner that doesn't answer a conversion is asked again every 0.4 s for up to 10 s (xclip, for one, ignores other requests while a clipboard manager reads a large value from it).
