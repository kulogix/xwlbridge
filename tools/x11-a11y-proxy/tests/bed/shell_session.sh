#!/bin/bash
# Source inside a container (under dbus-run-session) to get a headless gnome-shell session with the startup
# overview closed and notification banners off. Exports WAYLAND_DISPLAY etc. for apps started afterwards.
#   SIZE=1280x720 (virtual monitor)
export XDG_RUNTIME_DIR=/tmp/xdg HOME=/tmp/home; mkdir -p -m700 $XDG_RUNTIME_DIR $HOME
dbus-update-activation-environment HOME XDG_RUNTIME_DIR 2>/dev/null
# gnome-shell aborts without a system bus: give it an empty stand-in (login1, polkit, ... just fail)
export DBUS_SYSTEM_BUS_ADDRESS=$(dbus-daemon --session --fork --print-address | head -1)
gsettings set org.gnome.shell welcome-dialog-last-shown-version '999'
gsettings set org.gnome.desktop.notifications show-banners false
gsettings set org.gnome.desktop.interface enable-animations false
gsettings set org.gnome.desktop.interface cursor-blink false   # a blinking caret would read as a change
# headless gnome-shell starts the pointer at (0,0), right on the hot corner's pointer barriers: the first
# injected absolute motion is clipped in one axis by a barrier and its pressure opens the overview
gsettings set org.gnome.desktop.interface enable-hot-corners false
pipewire >/tmp/pw.log 2>&1 & sleep 0.3; wireplumber >/tmp/wp.log 2>&1 & sleep 0.3
gnome-shell --headless --wayland --virtual-monitor ${SIZE:-1280x720} >/tmp/shell.log 2>&1 &
SHELL_PID=$!
for i in $(seq 300); do
  gdbus introspect --session -d org.gnome.Mutter.RemoteDesktop -o /org/gnome/Mutter/RemoteDesktop >/dev/null 2>&1 \
    && [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-0 XDG_CURRENT_DESKTOP=GNOME XDG_SESSION_TYPE=wayland
# the shell starts in the overview (on 48 only once its startup animation ran): leave it -- the property
# org.gnome.Shell.OverviewActive is writable -- until it has stayed shut for a second
ov() { gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell -m org.freedesktop.DBus.Properties.Get \
  org.gnome.Shell OverviewActive 2>/dev/null; }
shut=0
for i in $(seq 50); do
  if ov | grep -q true; then
    shut=0; gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell -m org.freedesktop.DBus.Properties.Set \
      org.gnome.Shell OverviewActive '<false>' >/dev/null
  else
    shut=$((shut + 1)); [ $shut -ge 5 ] && break
  fi
  sleep 0.2
done
echo "shell_session: gnome-shell pid $SHELL_PID"
