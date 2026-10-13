# Remote desktop on the lock screen (GNOME Shell extension)

`xwlbridge-lockscreen@xwlbridge` keeps remote desktop sessions open while the screen is locked, so a remote user
(RealVNC Server through xwlbridge, or any other client of mutter's RemoteDesktop/ScreenCast D-Bus APIs) sees the
lock screen and can unlock it with the account password, as with VNC on Xorg.

## Why it is needed

When the screen locks, gnome-shell switches to its `unlock-dialog` session mode. That mode does not allow
screencasts, so gnome-shell asks mutter to inhibit remote access, and mutter closes every remote desktop session
and refuses new ones until someone unlocks at the computer. Without the extension:

- xwlbridge shows a padlock to its X clients (and so to every VNC viewer) for as long as the screen is locked,
  rather than the last picture of the desktop;
- a remote user cannot unlock: someone has to unlock at the computer, and then the picture comes back by itself.

GNOME's own remote desktop server (gnome-remote-desktop, "Desktop Sharing") behaves the same way in a user
session.

## What it changes

It skips that one inhibition while the user's session is locked and undoes the skip at unlock (gnome-shell's
inhibit/uninhibit calls stay balanced). Enabled while the screen is already locked, it lifts the inhibition then.
The login screen is a different gnome-shell process that loads only extensions installed system-wide and enabled
for GDM, so a per-user install leaves it unaffected (see "Login screen" below).

Decide with this in mind: while the extension is enabled, every client of mutter's remote desktop and
screencast APIs keeps working on the lock screen. That includes a remote desktop server (which can then type
into the password prompt) and, for example, a browser that was already sharing the screen in a call. The lock
screen still asks for the password; nothing is shown that the lock screen does not show (lock-screen
notifications follow GNOME's "Lock Screen Notifications" setting). Turn the extension off to get GNOME's
default back.

## Install (per user, no root)

```sh
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r contrib/gnome-shell/xwlbridge-lockscreen@xwlbridge ~/.local/share/gnome-shell/extensions/
gnome-extensions enable xwlbridge-lockscreen@xwlbridge     # a running Wayland session only sees new extensions
# ...after the next login; to have it enabled then without a second step:
gsettings set org.gnome.shell enabled-extensions "$(python3 -c 'import ast,subprocess; \
  v=ast.literal_eval(subprocess.check_output(["gsettings","get","org.gnome.shell","enabled-extensions"],text=True).replace("@as ","")); \
  u="xwlbridge-lockscreen@xwlbridge"; print(v if u in v else v+[u])')"
```

Then log out and in once. Check with `gnome-extensions info xwlbridge-lockscreen@xwlbridge` (State: ACTIVE);
xwlbridge also logs at start which lock-screen behaviour viewers will get.

GNOME turns all user extensions off when gnome-shell crashes shortly after it started
(`org.gnome.Shell-disable-extensions.service`). If remote unlock stops working after such a crash, check
`gsettings get org.gnome.shell disable-user-extensions` and set it back to `false`.

## Login screen (GDM): experimental, for administrators

A remote user who logs out, or whose session ends (a gnome-shell crash ends a Wayland session), finds nothing to
connect to: the bridge and RealVNC run in the user's session, and GDM's login screen allows no remote access
(its 'gdm' mode inhibits it before any extension loads). With this extension enabled for GDM, the login screen
accepts mutter remote-desktop sessions, so a bridge running in the login screen's session can show it and type
the password into it. Tested on GDM/GNOME Shell 50.1 (Ubuntu 26.04): the login screen through the bridge in the
viewer, the password typed in the viewer, the user's session and its own service then took over.

1. Install system-wide (the login screen does not read users' home directories):
   `sudo cp -r xwlbridge-lockscreen@xwlbridge /usr/share/gnome-shell/extensions/`
2. Enable it for GDM. Ubuntu/Debian: append to `/etc/gdm3/greeter.dconf-defaults`
   ```
   [org/gnome/shell]
   enabled-extensions=['xwlbridge-lockscreen@xwlbridge']
   ```
   then `sudo /usr/share/gdm/generate-config` (Fedora/RHEL: a keyfile in `/etc/dconf/db/gdm.d/` and
   `sudo dconf update`). It applies from the next login screen on.
3. Run a bridge and a VNC server in the login screen's session. GDM 49+ runs each login screen as a new dynamic
   user (`gdm-greeter`, a fresh uid each time) with its own bus at `/run/user/<uid>/bus`, so the bridge has to
   be started per login screen as that user with that bus (`XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`,
   `WAYLAND_DISPLAY=wayland-0`). This was tested by hand (a transient unit with `systemd-run --uid=<uid>`, x11vnc
   on a local port with no authentication behind an ssh tunnel), not with a packaged unit or with RealVNC as
   that user. xwlbridge exits with 69 about 10 s after the login screen's session (and bus) ends.
4. Ports: RealVNC Server does not retry a listening port it found busy ("TcpListenerManager: not accepting
   connections: failed to listen on at least one address") and then runs without listening. A login-screen
   server on the same port as the user's must be gone before the user's service starts, or use another port.

What it exposes: anyone who can authenticate to the login-screen VNC server sees the login screen and can try
account passwords, use its accessibility and power menus, as at the computer. Leave it off unless that is the
intent; the per-user install (above) does not do this.

## Tested

GNOME Shell 50.1 (Ubuntu 26.04, real Wayland session in a KVM VM) with RealVNC Server 6.11 through xwlbridge and
the real RealVNC Viewer: locking with `loginctl lock-session`, `org.gnome.ScreenSaver.Lock` and Super+L typed in
the viewer; connecting to an already locked screen; unlocking with the password typed in the viewer; idle
lock and display blanking (a VM test of the session lifecycle). GNOME Shell 46 and 48
use the same remote-access code (`js/ui/main.js` `_sessionUpdated`, checked in 46.0 and 48.7) but the
extension was not run there.
