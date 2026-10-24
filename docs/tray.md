# The system tray on `-showwindows`

X applications put an icon in the system tray to stay reachable while running in the background: a chat
program, a backup tool, Java's `SystemTray`, Wine's notification area. On `-showwindows`, xwlbridge is that
tray, and it shows what the icons do in GNOME's top bar:

```
  X application ──XEmbed──▶ xwlbridge                              GNOME Shell
  (Java, Wine, GTK, ...)    _NET_SYSTEM_TRAY_S0 owner             ┌──────────────┐
        icon window  ◀─────  one container per icon  ──── SNI ──▶ │ AppIndicator │
                              (never shown as a window)           │  extension   │
        ▲  XTest click                │  title, pixels            └──────────────┘
        └────────────────────────────┴── a click on the icon in the top bar
```

## What is shown

- Every application that docks an XEmbed icon on the bridge's display (the display `-showwindows` creates)
  appears once in the top bar, with the icon the application draws.
- The tooltip is the icon's name; the menu (left click) offers **Open** (the same as a left double click on
  the icon), **Menu** (the application's own menu) and **Middle click**, which do what a click on the icon
  does for applications that have no menu of their own.
- A notification the application sends through the tray (a balloon message: "backup finished", "new mail")
  becomes a GNOME notification.
- An application's own notifications (`notify-send`, `libnotify`, Qt, Java, Electron) are not the tray's
  business: they already go to GNOME, and with `-showwindows` they behave as for any GNOME application.

## Requirements

- GNOME Shell with the **AppIndicator** extension, which on Ubuntu is installed and enabled by default
  (`gnome-extensions enable ubuntu-appindicators@ubuntu.com`); on other distributions install the
  *AppIndicator and KStatusNotifierItem Support* extension
  (`gnome-shell-extension-appindicator`). Without it GNOME has no place to show a status icon, and the
  icons stay invisible (the bridge says so once in its log, and `xwlbridge-ctl doctor` reports it).
- The X application must run on the bridge's display (as the programs xwlbridge starts do).

## Settings

`tray` (default `on`) is the `-notray` option: `xwlbridge -showwindows -notray` leaves the tray alone, as
0.2.0 did (no icon is taken and nothing is shown in the top bar).
`xwlbridge-ctl config set tray off` does the same for the service (see
[xwlbridge.conf](../packaging/xwlbridge.conf)).

## Limits

- A single left click on an icon in the top bar does nothing (GNOME only opens the menu with it); use
  **Open** in the menu, or a double click.
- The application's menu opens where the pointer is; the icon itself is not a place on the X screen.
- The tray is a `-showwindows` feature: from a VNC viewer, the icons are in GNOME's top bar, not on the
  bridge's display.