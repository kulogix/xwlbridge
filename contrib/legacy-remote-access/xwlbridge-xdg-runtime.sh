# /etc/profile.d/xwlbridge-xdg-runtime.sh (seen needed on RHEL 10)
# A daemon that starts the desktop user's processes with `su -` gives them a login environment without
# XDG_RUNTIME_DIR, so audio clients cannot find PipeWire/PulseAudio (TeamViewer 15.20's GUI then crashed about 20 s
# into every session). Fill in what pam_systemd would have set, only for the owner of an existing runtime directory.
if [ -z "${XDG_RUNTIME_DIR:-}" ] && [ -d "/run/user/$(id -u)" ] && [ -O "/run/user/$(id -u)" ]; then
  XDG_RUNTIME_DIR=/run/user/$(id -u)
  export XDG_RUNTIME_DIR
fi
