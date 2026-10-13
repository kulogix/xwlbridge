#!/usr/bin/env python3
"""Stand-in for the person at the GNOME machine: injects pointer/keyboard input into mutter through a
RemoteDesktop session of its own (not the bridge's), so a test can ask "when the local user moves the mouse
or presses a key on GNOME, what does an X client on the bridge display see?" (global pointer, idle time,
global hotkeys, RECORD/XI2 observers, Barrier/Deskflow servers).

  gnome_input.py [--connector Meta-0] ACTION...
    move X Y        absolute pointer motion in that monitor's stream pixels
    rel DX DY       relative motion
    click [BTN]     press+release (evdev code; default 272 = BTN_LEFT)
    key CODE        press+release of an evdev keycode (e.g. 30 = KEY_A, 125 = KEY_LEFTMETA)
    down CODE / up CODE
    sleep SECS
Prints 'GNOME_INPUT ok N' after the last action."""
import sys, time, dbus, dbus.mainloop.glib
from gi.repository import GLib

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
args = sys.argv[1:]
conn = 'Meta-0'
if args[:1] == ['--connector']:
    conn, args = args[1], args[2:]
bus = dbus.SessionBus()
RD, SC = 'org.gnome.Mutter.RemoteDesktop', 'org.gnome.Mutter.ScreenCast'
rd_path = bus.get_object(RD, '/org/gnome/Mutter/RemoteDesktop').CreateSession(dbus_interface=RD)
rd = dbus.Interface(bus.get_object(RD, rd_path), RD + '.Session')
sid = bus.get_object(RD, rd_path).Get(RD + '.Session', 'SessionId', dbus_interface=dbus.PROPERTIES_IFACE)
sc_path = bus.get_object(SC, '/org/gnome/Mutter/ScreenCast').CreateSession(
    {'remote-desktop-session-id': sid}, dbus_interface=SC)
sc = bus.get_object(SC, sc_path)
stream = sc.RecordMonitor(conn, {'cursor-mode': dbus.UInt32(1)}, dbus_interface=SC + '.Session')
loop = GLib.MainLoop()
bus.add_signal_receiver(lambda node: loop.quit(), 'PipeWireStreamAdded', SC + '.Stream', path=stream)
rd.Start()
GLib.timeout_add(5000, loop.quit)
loop.run()
time.sleep(0.2)
n, i = 0, 0
while i < len(args):
    a = args[i]
    if a == 'move':
        rd.NotifyPointerMotionAbsolute(stream, float(args[i + 1]), float(args[i + 2])); i += 3
    elif a == 'rel':
        rd.NotifyPointerMotion(float(args[i + 1]), float(args[i + 2])); i += 3
    elif a == 'click':
        b = int(args[i + 1]) if i + 1 < len(args) and args[i + 1].isdigit() else 272
        i += 2 if i + 1 < len(args) and args[i + 1].isdigit() else 1
        rd.NotifyPointerButton(b, True); time.sleep(0.03); rd.NotifyPointerButton(b, False)
    elif a == 'key':
        k = int(args[i + 1]); i += 2
        rd.NotifyKeyboardKeycode(dbus.UInt32(k), True); time.sleep(0.03)
        rd.NotifyKeyboardKeycode(dbus.UInt32(k), False)
    elif a in ('down', 'up'):
        rd.NotifyKeyboardKeycode(dbus.UInt32(int(args[i + 1])), a == 'down'); i += 2
    elif a == 'sleep':
        time.sleep(float(args[i + 1])); i += 2
    else:
        sys.exit(f'gnome_input: bad action {a!r}')
    n += 1
    time.sleep(0.02)
time.sleep(0.2)
rd.Stop()
print('GNOME_INPUT ok', n, flush=True)
