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
  becomes a GNOME notification: `_NET_SYSTEM_TRAY_BEGIN_MESSAGE`, its `_NET_SYSTEM_TRAY_MESSAGE_DATA`
  chunks (20 bytes each, a length that need not be a multiple of 20) and `_NET_SYSTEM_TRAY_CANCEL_MESSAGE`
  are reassembled per icon; the notification's application name and summary are the icon's title, the body
  the message and its expiry the balloon's timeout. A message cancelled before it is whole shows nothing.
- An application's own notifications (`notify-send`, `libnotify`, Qt, Java, Electron) are not the tray's
  business: the programs xwlbridge starts keep the session bus (`DBUS_SESSION_BUS_ADDRESS`), so they go to
  GNOME directly, and with `-showwindows` they behave as for any GNOME application. When the application
  acts on its notification's default action and raises its X window, the window follows on the desktop like
  any other raise (it activates after recent input in that application, and asks for attention otherwise,
  as GNOME does for its own windows).

## Clicks

GNOME's AppIndicator extension does not tell an item about a single left click (it opens the item's menu)
and never calls `ContextMenu`: so each icon also carries a minimal `com.canonical.dbusmenu`:

| in the top bar | what happens on the X icon |
|---|---|
| **Open** (menu) | a left click (what a single click does on an X panel) |
| **Menu** (menu) | a right click: the application's own X menu opens and is shown on the desktop like any popup |
| **Middle click** (menu) | a middle click |
| double left click | `Activate` → a left click |
| middle click on the icon | `SecondaryActivate` → a middle click |
| wheel | `Scroll` → buttons 4/5 (vertical) or 6/7 (horizontal) |

A click is made with XTest on the bridge's reverse pointer device, on the X point that corresponds to the
GNOME point the click came from (for the menu entries: the session's pointer), so the application's own
menu opens where the user is and the bridge's pointer on the desktop never moves from it.

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

- GNOME's AppIndicator extension only opens the item's menu on a single left click; it never calls
  `ContextMenu`, and an item without a menu gets nothing. That is why the **Open** / **Menu** /
  **Middle click** entries are there, and why a double click is the only way to `Activate` an icon.
- The application's own menu opens where the pointer is, as on an X panel; the parked icon itself is not a
  place on the X screen.
- The tray is a `-showwindows` feature: from a VNC viewer, the icons are in GNOME's top bar, not on the
  bridge's display.