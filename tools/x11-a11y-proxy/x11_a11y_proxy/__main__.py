import sys

try:
    from .app import main
# ValueError: gi.require_version found no GTK 4; AttributeError: a `gi` namespace without PyGObject (Fedora)
except (ImportError, ValueError, AttributeError) as e:
    sys.stderr.write(
        f"x11-a11y-proxy: {e}\n"
        "It needs PyGObject with GTK 4 (>= 4.14), libatspi, python-xlib and dbus-daemon. Packages:\n"
        "  Debian/Ubuntu: sudo apt install python3-gi gir1.2-gtk-4.0 gir1.2-atspi-2.0 python3-xlib dbus\n"
        "  Fedora: sudo dnf install python3-gobject gtk4 at-spi2-core python3-xlib\n"
        "  RHEL/AlmaLinux 10: sudo dnf install python3-gobject gtk4 at-spi2-core; pip install --user python-xlib\n")
    sys.exit(1)

sys.exit(main())
