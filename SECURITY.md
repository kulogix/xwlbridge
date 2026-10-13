# Security policy

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | yes |
| older | no |

## Reporting a vulnerability

Report vulnerabilities privately. Use GitHub's private vulnerability reporting on https://github.com/kulogix/xwlbridge: open the Security tab and choose "Report a vulnerability".

If that is not available, open an issue that asks for a private contact. Do not put any details in the issue.

Do not open a public issue for a vulnerability.

## In scope

- The bridge's access control.
- The `-uidisplay` hook.
- The packaging scripts.
- The logind proxy in `contrib`.

## Security model

- The bridge display is your desktop. Any client that can connect sees the whole screen, types and clicks anywhere, and reads and writes the clipboard, with no portal prompt. Treat access to it like access to your logged-in session.
- The builtin engine admits only your user and root. The stock engines get a fresh MIT-MAGIC-COOKIE plus localuser entries for your uid and root only. Never run `xhost +` on it.
- GNOME's screen-sharing indicator is on whenever the desktop is mirrored. Stopping it there ends the mirror. It is not restarted unless `-reclaim` is set.
- VNC servers on top are reachable from the network unless you restrict them. Use `-localhost` with SSH tunnels, or a password and encryption (RealVNC: `Encryption=AlwaysOn`).
- With `-uidisplay`, RealVNC runs without its set-uid root helper. xwlbridge refuses set-uid X servers and RealVNC's Xvnc.
- xwlbridge needs no root and installs nothing system-wide. It stops what it started when it exits.
- The `-uidisplay` hook is written only into a directory owned by the user and writable by no one else.
- Input attribution (`-xtesthelpers`, `-xtestdrivers`) is an accident filter, not a security boundary.
- The legacy remote-access helpers run a root service, the logind proxy. It serves each connection with the client's own uid.

## Other vendors

Vulnerabilities in RealVNC or TeamViewer products go to those vendors.
