// X11 window integration (xwlbridge) — GNOME Shell 49–50.
//
// A Wayland client cannot place its own toplevel or learn where it is. xwlbridge's -showwindows shows each X
// window as a Wayland toplevel, so without help an X window that moves itself (a dock dragged by its own
// handle, a slide-in panel, a saved session position) cannot move on the desktop, and the X app never learns
// where GNOME put it. This extension is that help: a small D-Bus service on the session bus
//
//   name org.xwlbridge.Windows, object /org/xwlbridge/Windows, interface org.xwlbridge.Windows1
//
// through which the bridge moves, resizes, activates and restacks its own windows and hears about desktop-side
// changes to them. docs/extension.md describes every method and signal.
//
// Security model (the whole point of the checks below):
// - A process can act only on windows it created itself. A window is reachable only if it is a Wayland toplevel
//   (not an X11 window of GNOME's own Xwayland, not override-redirect) whose xdg_toplevel_tag_v1 tag starts with
//   "xwlbridge:". Windows are found by that tag (Meta.Window.get_tag), never by title, app id or stacking position.
//   Tagging is unprivileged, so the prefix is not a credential: what keeps a process from harming others is that
//   it reaches only its own windows, and that Activate cannot take the keyboard from another app (below).
// - Every method that names or lists windows asks the bus for the caller's process id
//   (org.freedesktop.DBus.GetConnectionUnixProcessID) and serves only windows whose Wayland client has that
//   process id (Meta.Window.get_pid). A window of another process, tagged or not, answers "no such window",
//   exactly like a tag that does not exist, so callers cannot probe other processes' windows.
// - Activate focuses the window only while the keyboard is on none of the other apps' windows (no focus, or a
//   window of the caller's process); otherwise the window demands attention. A timestamp of 0 is refused.
// - Signals about windows are sent to the owning process's bus names only (unicast), not broadcast.
// - Version, Features and GetMonitors carry nothing private and answer anyone. Per-process caps keep one
//   process from crowding out another's subscriptions or initial positions.
// Lock screen: gnome-shell disables extensions in its unlock-dialog mode, so the service goes away while the
// screen is locked and comes back at unlock; the bridge watches the name and resyncs with GetState.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Mtk from 'gi://Mtk';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.xwlbridge.Windows';
const OBJECT_PATH = '/org/xwlbridge/Windows';
const IFACE = 'org.xwlbridge.Windows1';
const API_VERSION = 1;
const TAG_PREFIX = 'xwlbridge:';
const MAX_TAG_LENGTH = 256;

const ERR_NO_WINDOW = 'org.xwlbridge.Windows1.Error.NoSuchWindow';
const ERR_INVALID_ARGS = 'org.freedesktop.DBus.Error.InvalidArgs';
const ERR_ACCESS_DENIED = 'org.freedesktop.DBus.Error.AccessDenied';
const ERR_FAILED = 'org.freedesktop.DBus.Error.Failed';

// MoveResize flags
const MOVE_CLAMP = 1;           // keep the rectangle inside the work area of the monitor it is (mostly) on
const MOVE_USER = 2;            // the move is the user's doing (mutter applies its user-action constraints)

// Changed flags
const STATE_USER = 1;           // a grab (an interactive move or resize by the user) is active on the window
const STATE_MINIMIZED = 2;
const STATE_MAXIMIZED = 4;
const STATE_FULLSCREEN = 8;
const STATE_ABOVE = 16;
const STATE_FOCUSED = 32;

const COORD_LIMIT = 1 << 20;    // |x|, |y| for a desktop position
const SIZE_LIMIT = 1 << 16;     // width, height
const MAX_SUBSCRIBERS = 64;     // bus names that get signals
const MAX_SUBSCRIBERS_PER_PID = 4;
const MAX_PENDING = 256;        // SetInitialPosition entries waiting for their window
const MAX_PENDING_PER_PID = 64;
const PENDING_TTL_US = 60 * GLib.USEC_PER_SEC;

const FEATURES = [
    'move-resize', 'clamp-work-area', 'initial-position', 'activate', 'raise', 'above', 'demands-attention',
    'stick', 'workspace', 'minimize', 'state', 'monitors', 'restacked', 'work-area',
];

const IFACE_XML = `
<node>
  <interface name="${IFACE}">
    <method name="Version">
      <arg type="u" name="version" direction="out"/>
    </method>
    <method name="Features">
      <arg type="as" name="features" direction="out"/>
    </method>
    <method name="MoveResize">
      <arg type="s" name="tag" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/>
      <arg type="i" name="height" direction="in"/>
      <arg type="u" name="flags" direction="in"/>
      <arg type="(iiii)" name="applied" direction="out"/>
    </method>
    <method name="SetInitialPosition">
      <arg type="s" name="tag" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <method name="Activate">
      <arg type="s" name="tag" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
    <method name="Raise">
      <arg type="s" name="tag" direction="in"/>
    </method>
    <method name="SetAbove">
      <arg type="s" name="tag" direction="in"/>
      <arg type="b" name="above" direction="in"/>
    </method>
    <method name="SetDemandsAttention">
      <arg type="s" name="tag" direction="in"/>
      <arg type="b" name="demands" direction="in"/>
    </method>
    <method name="Stick">
      <arg type="s" name="tag" direction="in"/>
      <arg type="b" name="stick" direction="in"/>
    </method>
    <method name="SetWorkspace">
      <arg type="s" name="tag" direction="in"/>
      <arg type="i" name="index" direction="in"/>
    </method>
    <method name="Minimize">
      <arg type="s" name="tag" direction="in"/>
      <arg type="b" name="minimize" direction="in"/>
    </method>
    <method name="GetState">
      <arg type="a(sa{sv})" name="windows" direction="out"/>
    </method>
    <method name="GetMonitors">
      <arg type="a(i(iiii)(iiii)db)" name="monitors" direction="out"/>
    </method>
    <method name="GetWorkArea">
      <arg type="(iiii)" name="workArea" direction="out"/>
    </method>
    <signal name="Changed">
      <arg type="s" name="tag"/>
      <arg type="(iiii)" name="rect"/>
      <arg type="i" name="monitor"/>
      <arg type="i" name="workspace"/>
      <arg type="u" name="flags"/>
      <arg type="u" name="serial"/>
    </signal>
    <signal name="Restacked">
      <arg type="as" name="tags"/>
    </signal>
    <signal name="MonitorsChanged"/>
    <signal name="WindowGone">
      <arg type="s" name="tag"/>
    </signal>
  </interface>
</node>`;

class DBusError extends Error {
    constructor(name, message) {
        super(message);
        this.dbusName = name;
    }
}

function checkTag(tag) {
    if (typeof tag !== 'string' || !tag.startsWith(TAG_PREFIX) || tag.length > MAX_TAG_LENGTH)
        throw new DBusError(ERR_INVALID_ARGS, `not an xwlbridge window tag: ${String(tag).slice(0, 80)}`);
}

function checkPosition(x, y) {
    if (!Number.isInteger(x) || !Number.isInteger(y) || Math.abs(x) > COORD_LIMIT || Math.abs(y) > COORD_LIMIT)
        throw new DBusError(ERR_INVALID_ARGS, `position out of range: ${x},${y}`);
}

function isMaximized(win) {
    if (typeof win.is_maximized === 'function')
        return win.is_maximized();
    // mutter < 49
    return win.get_maximized() === Meta.MaximizeFlags.BOTH;
}

function rectArray(r) {
    return [r.x, r.y, r.width, r.height];
}

export default class XwlbridgeWindows extends Extension {
    enable() {
        this._handlers = [];            // [GObject, handler id] of display-wide signals
        this._windows = new Map();      // Meta.Window -> {tagIds, tracked: null | {tag, pid, ids, shown, last}}
        this._subscribers = new Map();  // unique bus name -> {pid, watchId}
        this._pidLookups = new Map();   // unique bus name -> Promise<pid> while the bus answers
        this._pending = new Map();      // `${pid}\n${tag}` -> {x, y, expires}
        this._dirty = new Set();        // tracked windows whose state may have changed
        this._restackDirty = false;
        this._monitorsDirty = false;
        this._lastStack = new Map();    // pid -> tags bottom-to-top, joined with '\n'
        this._grabWindow = null;
        this._serial = 0;
        this._flushId = 0;

        this._hasTags = typeof Meta.Window.prototype.get_tag === 'function';
        if (!this._hasTags)
            console.warn(`${this.uuid}: this mutter has no window tags (Meta.Window.get_tag); no window can be served`);
        const proto = Meta.Window.prototype;
        this._hasVisibility = typeof proto.showing_on_its_workspace === 'function' &&
            typeof proto.located_on_workspace === 'function';

        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this._methods());
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS_NAME,
            Gio.BusNameOwnerFlags.NONE, null,
            () => console.warn(`${this.uuid}: could not own ${BUS_NAME} (another owner?)`));

        // without tags no window can be ours: only Version and Features are served (Features is empty)
        if (!this._hasTags)
            return;
        const display = global.display;
        this._connect(display, 'window-created', (_d, win) => this._watchWindow(win));
        this._connect(display, 'restacked', () => this._queueRestack());
        this._connect(display, 'grab-op-begin', (_d, win) => this._grabChanged(win));
        this._connect(display, 'grab-op-end', () => this._grabChanged(null));
        this._connect(display, 'workareas-changed', () => this._queueMonitors());
        this._connect(global.backend.get_monitor_manager(), 'monitors-changed', () => this._queueMonitors());
        // what is showing changes the order (windows that are not showing go to the bottom)
        this._connect(global.workspace_manager, 'active-workspace-changed', () => this._queueRestack());
        for (const win of display.list_all_windows())
            this._watchWindow(win);
    }

    disable() {
        if (this._flushId)
            GLib.source_remove(this._flushId);
        this._flushId = 0;
        for (const [obj, id] of this._handlers)
            obj.disconnect(id);
        this._handlers = [];
        for (const win of [...this._windows.keys()])
            this._unwatchWindow(win, false);
        for (const sub of this._subscribers.values())
            Gio.bus_unwatch_name(sub.watchId);
        this._subscribers.clear();
        this._pidLookups.clear();
        this._pending.clear();
        this._dirty.clear();
        this._lastStack.clear();
        this._grabWindow = null;
        if (this._nameId)
            Gio.bus_unown_name(this._nameId);
        this._nameId = 0;
        this._dbus.unexport();
        this._dbus = null;
    }

    _connect(obj, signal, handler) {
        this._handlers.push([obj, obj.connect(signal, handler)]);
    }

    // ------------------------------------------------------------------ windows

    _watchWindow(win) {
        if (this._windows.has(win) || !this._hasTags)
            return;
        // only Wayland toplevels: an X11 window of GNOME's Xwayland can carry a tag too (_NET_WM_WINDOW_TAG), and
        // an override-redirect window is not the window manager's to move or activate
        if (win.get_client_type() !== Meta.WindowClientType.WAYLAND || win.is_override_redirect())
            return;
        const entry = {tagIds: [], tracked: null};
        this._windows.set(win, entry);
        entry.tagIds.push(win.connect('notify::tag', () => this._retag(win)));
        entry.tagIds.push(win.connect('unmanaged', () => this._unwatchWindow(win, true)));
        this._retag(win);
    }

    _unwatchWindow(win, notify) {
        const entry = this._windows.get(win);
        if (!entry)
            return;
        this._untrack(win, entry, notify);
        for (const id of entry.tagIds)
            win.disconnect(id);
        this._windows.delete(win);
    }

    _retag(win) {
        const entry = this._windows.get(win);
        if (!entry)
            return;
        const tag = win.get_tag();
        const ours = typeof tag === 'string' && tag.startsWith(TAG_PREFIX) && tag.length <= MAX_TAG_LENGTH;
        if (entry.tracked && entry.tracked.tag === tag)
            return;
        this._untrack(win, entry, true);
        if (!ours)
            return;
        const pid = win.get_pid();
        if (!(pid > 0))
            return;
        const tracked = {tag, pid, ids: [], shown: false, last: null};
        entry.tracked = tracked;
        const dirty = () => this._markDirty(win);
        for (const signal of ['position-changed', 'size-changed', 'workspace-changed', 'notify::minimized',
            'notify::maximized-horizontally', 'notify::maximized-vertically', 'notify::fullscreen',
            'notify::above', 'notify::appears-focused', 'notify::on-all-workspaces', 'notify::main-monitor'])
            tracked.ids.push(win.connect(signal, dirty));
        // showing or not moves it in the order Restacked reports
        for (const signal of ['workspace-changed', 'notify::minimized', 'notify::on-all-workspaces'])
            tracked.ids.push(win.connect(signal, () => this._queueRestack()));
        tracked.ids.push(win.connect('shown', () => {
            // The first show: mutter has just placed the window and not painted it yet, so a waiting initial
            // position applied here is where the first frame appears (a move before this point is undone by
            // mutter's placement; measured on GNOME 50.1).
            if (!tracked.shown) {
                tracked.shown = true;
                this._applyPending(win, tracked);
            }
            this._markDirty(win);
        }));
        // shown before we saw its tag (tagged late, or the extension enabled again after the lock screen)
        tracked.shown = win.mapped === true || win.minimized === true;
        if (tracked.shown)  // tagged after it was shown: move it now
            this._applyPending(win, tracked);
        this._markDirty(win);
        this._queueRestack();
    }

    _untrack(win, entry, notify) {
        const tracked = entry.tracked;
        if (!tracked)
            return;
        entry.tracked = null;
        for (const id of tracked.ids)
            win.disconnect(id);
        this._dirty.delete(win);
        if (this._grabWindow === win)
            this._grabWindow = null;
        if (notify) {
            this._emitTo(tracked.pid, 'WindowGone', new GLib.Variant('(s)', [tracked.tag]));
            this._queueRestack();
        }
    }

    _trackedWindows(pid) {
        const out = [];
        for (const [win, entry] of this._windows) {
            if (entry.tracked && entry.tracked.pid === pid)
                out.push(win);
        }
        return out;
    }

    // The caller's window with this tag; another process's window with the same tag is "no such window" too.
    _find(tag, pid) {
        checkTag(tag);
        for (const [win, entry] of this._windows) {
            if (entry.tracked && entry.tracked.tag === tag && entry.tracked.pid === pid)
                return win;
        }
        throw new DBusError(ERR_NO_WINDOW, `no window ${tag} of the calling process`);
    }

    _grabChanged(win) {
        const old = this._grabWindow;
        this._grabWindow = win && this._windows.get(win)?.tracked ? win : null;
        if (old)
            this._markDirty(old);
        if (this._grabWindow)
            this._markDirty(this._grabWindow);
    }

    _state(win) {
        const r = win.get_frame_rect();
        let flags = 0;
        if (this._grabWindow === win)
            flags |= STATE_USER;
        if (win.minimized)
            flags |= STATE_MINIMIZED;
        if (isMaximized(win))
            flags |= STATE_MAXIMIZED;
        if (win.is_fullscreen())
            flags |= STATE_FULLSCREEN;
        if (win.is_above())
            flags |= STATE_ABOVE;
        if (win.has_focus())
            flags |= STATE_FOCUSED;
        const workspace = win.is_on_all_workspaces() ? -1 : win.get_workspace()?.index() ?? -1;
        return {rect: rectArray(r), monitor: win.get_monitor(), workspace, flags};
    }

    // ------------------------------------------------------------------ initial position

    _applyPending(win, tracked) {
        const key = `${tracked.pid}\n${tracked.tag}`;
        const p = this._pending.get(key);
        if (!p)
            return;
        this._pending.delete(key);
        if (p.expires < GLib.get_monotonic_time() || win.is_fullscreen() || isMaximized(win))
            return;
        const r = win.get_frame_rect();
        if (r.x !== p.x || r.y !== p.y)
            win.move_frame(false, p.x, p.y);
    }

    _setInitialPosition(pid, [tag, x, y]) {
        checkTag(tag);
        checkPosition(x, y);
        for (const [win, entry] of this._windows) {
            const t = entry.tracked;
            if (t && t.tag === tag && t.pid === pid && t.shown) {
                if (!win.is_fullscreen() && !isMaximized(win))
                    win.move_frame(false, x, y);
                return null;
            }
        }
        this._storePending(pid, tag, x, y);
        return null;
    }

    _storePending(pid, tag, x, y) {
        const now = GLib.get_monotonic_time();
        for (const [k, p] of this._pending) {
            if (p.expires < now)
                this._pending.delete(k);
        }
        const key = `${pid}\n${tag}`;
        this._pending.delete(key);
        // a process pushes out only its own entries (oldest first); at the global ceiling, the oldest entry of
        // the process with the most
        const own = [...this._pending].filter(([, p]) => p.pid === pid);
        if (own.length >= MAX_PENDING_PER_PID)
            this._pending.delete(own[0][0]);
        while (this._pending.size >= MAX_PENDING)
            this._pending.delete(this._oldestOfBusiest(this._pending));
        this._pending.set(key, {pid, x, y, expires: now + PENDING_TTL_US});
    }

    // The first key (oldest) of the pid with the most entries in map (values carry .pid).
    _oldestOfBusiest(map) {
        const counts = new Map();
        for (const v of map.values())
            counts.set(v.pid, (counts.get(v.pid) ?? 0) + 1);
        let busiest = null, most = -1;
        for (const [pid, n] of counts) {
            if (n > most) {
                busiest = pid;
                most = n;
            }
        }
        for (const [k, v] of map) {
            if (v.pid === busiest)
                return k;
        }
        return map.keys().next().value;
    }

    // ------------------------------------------------------------------ signals out

    _markDirty(win) {
        this._dirty.add(win);
        this._scheduleFlush();
    }

    _queueRestack() {
        this._restackDirty = true;
        this._scheduleFlush();
    }

    _queueMonitors() {
        this._monitorsDirty = true;
        this._scheduleFlush();
    }

    // Changes are coalesced: a drag moves the window on every pointer motion and one change sets several
    // properties; the bridge gets one Changed per window per main-loop turn, and only when something differs.
    _scheduleFlush() {
        if (this._flushId)
            return;
        this._flushId = GLib.idle_add(GLib.PRIORITY_DEFAULT, () => {
            this._flushId = 0;
            try {
                this._flush();
            } catch (e) {
                console.error(`${this.uuid}: ${e}`);
            }
            return GLib.SOURCE_REMOVE;
        });
    }

    _flush() {
        const dirty = [...this._dirty];
        this._dirty.clear();
        for (const win of dirty) {
            const tracked = this._windows.get(win)?.tracked;
            if (!tracked || !tracked.shown)
                continue;
            const s = this._state(win);
            const l = tracked.last;
            if (l && l.monitor === s.monitor && l.workspace === s.workspace && l.flags === s.flags &&
                l.rect.every((v, i) => v === s.rect[i]))
                continue;
            tracked.last = s;
            this._serial = (this._serial + 1) >>> 0;
            tracked.serial = this._serial;
            this._emitTo(tracked.pid, 'Changed', new GLib.Variant('(s(iiii)iiuu)',
                [tracked.tag, s.rect, s.monitor, s.workspace, s.flags, this._serial]));
        }
        if (this._restackDirty) {
            this._restackDirty = false;
            const pids = new Set([...this._subscribers.values()].map(s => s.pid));
            for (const pid of pids) {
                const tags = this._stacked(pid).map(w => this._windows.get(w).tracked.tag);
                const key = tags.join('\n');
                if (this._lastStack.get(pid) === key)
                    continue;
                this._lastStack.set(pid, key);
                this._emitTo(pid, 'Restacked', new GLib.Variant('(as)', [tags]));
            }
        }
        if (this._monitorsDirty) {
            this._monitorsDirty = false;
            for (const name of this._subscribers.keys())
                this._emit(name, 'MonitorsChanged', null);
        }
    }

    // The caller's windows bottom to top: those not showing (minimised, on another workspace) first, below
    // every showing one. They stay mapped in the caller's X server, so this keeps X's topmost window under any
    // point a showing one.
    _stacked(pid) {
        const wins = this._trackedWindows(pid);
        const sorted = wins.length > 1 ? global.display.sort_windows_by_stacking(wins) : wins;
        const active = global.workspace_manager.get_active_workspace();
        const showing = win => this._hasVisibility
            ? win.showing_on_its_workspace() && win.located_on_workspace(active)
            : !win.minimized;
        return [...sorted.filter(w => !showing(w)), ...sorted.filter(showing)];
    }

    _emitTo(pid, signal, params) {
        for (const [name, sub] of this._subscribers) {
            if (sub.pid === pid)
                this._emit(name, signal, params);
        }
    }

    _emit(destination, signal, params) {
        try {
            Gio.DBus.session.emit_signal(destination, OBJECT_PATH, IFACE, signal, params);
        } catch (e) {
            console.warn(`${this.uuid}: ${signal} to ${destination}: ${e.message}`);
        }
    }

    // ------------------------------------------------------------------ callers

    // The caller's process id. A caller of anything but Version and Features is remembered (unique bus names
    // are never reused) and gets the signals about its own windows until its connection closes.
    _callerPid(sender, subscribe = true) {
        const known = this._subscribers.get(sender);
        if (known)
            return Promise.resolve(known.pid);
        let lookup = this._pidLookups.get(sender);
        if (lookup)
            return lookup;
        lookup = new Promise((resolve, reject) => {
            Gio.DBus.session.call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
                'GetConnectionUnixProcessID', new GLib.Variant('(s)', [sender]), new GLib.VariantType('(u)'),
                Gio.DBusCallFlags.NONE, -1, null, (conn, res) => {
                    try {
                        const [pid] = conn.call_finish(res).deepUnpack();
                        resolve(pid);
                    } catch (e) {
                        reject(new DBusError(ERR_ACCESS_DENIED, `cannot identify the caller: ${e.message}`));
                    }
                });
        }).then(pid => {
            this._pidLookups.delete(sender);
            return pid;
        }, e => {
            this._pidLookups.delete(sender);
            throw e;
        });
        this._pidLookups.set(sender, lookup);
        return lookup.then(pid => {
            if (subscribe && this._dbus && pid > 0)
                this._addSubscriber(sender, pid);
            return pid;
        });
    }

    _addSubscriber(name, pid) {
        if (this._subscribers.has(name))
            return;
        // a process crowds out only its own names (oldest first); at the global ceiling, the oldest name of the
        // process with the most
        const own = [...this._subscribers].filter(([, s]) => s.pid === pid);
        if (own.length >= MAX_SUBSCRIBERS_PER_PID)
            this._dropSubscriber(own[0][0]);
        if (this._subscribers.size >= MAX_SUBSCRIBERS)
            this._dropSubscriber(this._oldestOfBusiest(this._subscribers));
        const watchId = Gio.bus_watch_name_on_connection(Gio.DBus.session, name,
            Gio.BusNameWatcherFlags.NONE, null, () => this._dropSubscriber(name));
        this._subscribers.set(name, {pid, watchId});
    }

    _dropSubscriber(name) {
        const sub = this._subscribers.get(name);
        if (!sub)
            return;
        this._subscribers.delete(name);
        Gio.bus_unwatch_name(sub.watchId);
        if (![...this._subscribers.values()].some(s => s.pid === sub.pid))
            this._lastStack.delete(sub.pid);
    }

    // Runs a method for the caller: fn(pid, args) returns the reply (a GLib.Variant tuple) or null.
    _serve(fn, args, invocation, subscribe = true) {
        const dbus = this._dbus;
        this._callerPid(invocation.get_sender(), subscribe).then(pid => {
            if (this._dbus !== dbus || !dbus)
                throw new DBusError(ERR_FAILED, 'the extension was disabled');
            invocation.return_value(fn.call(this, pid, args));
        }).catch(e => {
            if (e instanceof DBusError) {
                invocation.return_dbus_error(e.dbusName, e.message);
            } else {
                console.error(`${this.uuid}: ${e}`);
                invocation.return_dbus_error(ERR_FAILED, String(e?.message ?? e));
            }
        });
    }

    _methods() {
        const m = {};
        const add = (name, fn, subscribe = true) => {
            m[`${name}Async`] = (args, invocation) => this._serve(fn, args, invocation, subscribe);
        };
        add('Version', () => new GLib.Variant('(u)', [API_VERSION]), false);
        add('Features', () => new GLib.Variant('(as)', [this._hasTags ? FEATURES : []]), false);
        add('MoveResize', this._moveResize);
        add('SetInitialPosition', this._setInitialPosition);
        add('Activate', (pid, [tag, timestamp]) => {
            const win = this._find(tag, pid);
            // 0 would be "now", which mutter does not check for focus stealing
            if (!timestamp)
                throw new DBusError(ERR_INVALID_ARGS, 'Activate needs the timestamp of the user input it follows');
            // a timestamp is easy to make up (compositor time is the monotonic clock in ms), so it does not
            // decide alone: the keyboard is taken only from no window or from the caller's own
            const focus = global.display.focus_window;
            if (focus && focus !== win && focus.get_pid() !== pid) {
                win.set_demands_attention();
                return null;
            }
            win.activate(timestamp);
            return null;
        });
        add('Raise', (pid, [tag]) => {
            this._find(tag, pid).raise();
            return null;
        });
        add('SetAbove', (pid, [tag, above]) => {
            const win = this._find(tag, pid);
            if (above)
                win.make_above();
            else
                win.unmake_above();
            return null;
        });
        add('SetDemandsAttention', (pid, [tag, demands]) => {
            const win = this._find(tag, pid);
            if (demands)
                win.set_demands_attention();
            else
                win.unset_demands_attention();
            return null;
        });
        add('Stick', (pid, [tag, stick]) => {
            const win = this._find(tag, pid);
            if (stick)
                win.stick();
            else
                win.unstick();
            return null;
        });
        add('SetWorkspace', (pid, [tag, index]) => {
            const win = this._find(tag, pid);
            const n = global.workspace_manager.get_n_workspaces();
            if (!Number.isInteger(index) || index < 0 || index >= n)
                throw new DBusError(ERR_INVALID_ARGS, `workspace ${index} does not exist (0-${n - 1})`);
            win.change_workspace_by_index(index, false);
            return null;
        });
        add('Minimize', (pid, [tag, minimize]) => {
            const win = this._find(tag, pid);
            if (minimize)
                win.minimize();
            else
                win.unminimize();
            return null;
        });
        add('GetState', this._getState);
        add('GetMonitors', this._getMonitors);
        add('GetWorkArea', this._getWorkArea);
        return m;
    }

    // ------------------------------------------------------------------ methods

    _moveResize(pid, [tag, x, y, width, height, flags]) {
        const win = this._find(tag, pid);
        checkPosition(x, y);
        const cur = win.get_frame_rect();
        // a maximized or fullscreen window stays as the desktop has it: the caller follows the reply
        if (win.is_fullscreen() || isMaximized(win))
            return new GLib.Variant('((iiii))', [rectArray(cur)]);
        if (width <= 0)
            width = cur.width;
        if (height <= 0)
            height = cur.height;
        if (width > SIZE_LIMIT || height > SIZE_LIMIT)
            throw new DBusError(ERR_INVALID_ARGS, `size out of range: ${width}x${height}`);
        let r = {x, y, width, height};
        if (flags & MOVE_CLAMP)
            r = this._clamp(win, r);
        const user = (flags & MOVE_USER) !== 0;
        if (r.width === cur.width && r.height === cur.height) {
            if (r.x !== cur.x || r.y !== cur.y)
                win.move_frame(user, r.x, r.y);
        } else {
            win.move_resize_frame(user, r.x, r.y, r.width, r.height);
        }
        // A move is applied at once (and constrained by mutter: the frame rect is the truth). A new size
        // waits for the client to draw it, so until then the reply is the rectangle asked for (after the
        // clamp); Changed carries the final one.
        const now = win.get_frame_rect();
        const applied = now.width === r.width && now.height === r.height ? rectArray(now) : rectArray(r);
        return new GLib.Variant('((iiii))', [applied]);
    }

    // Inside the work area of the monitor the rectangle asked for is (mostly) on, which may be another monitor
    // than the window's now. A rectangle that is exactly a monitor (a borderless "fullscreen" window) stays.
    _clamp(win, r) {
        const display = global.display;
        const rect = new Mtk.Rectangle({x: r.x, y: r.y, width: r.width, height: r.height});
        let monitor = display.get_monitor_index_for_rect(rect);
        if (monitor < 0)
            monitor = win.get_monitor();
        if (monitor < 0)
            return r;
        const geometry = display.get_monitor_geometry(monitor);
        if (geometry.x === r.x && geometry.y === r.y && geometry.width === r.width && geometry.height === r.height)
            return r;
        const wa = win.get_work_area_for_monitor(monitor);
        const width = Math.min(r.width, wa.width);
        const height = Math.min(r.height, wa.height);
        return {
            x: Math.max(wa.x, Math.min(r.x, wa.x + wa.width - width)),
            y: Math.max(wa.y, Math.min(r.y, wa.y + wa.height - height)),
            width,
            height,
        };
    }

    _getState(pid) {
        const out = this._stacked(pid).map((win, stack) => {
            const tracked = this._windows.get(win).tracked;
            const s = this._state(win);
            return [tracked.tag, {
                rect: new GLib.Variant('(iiii)', s.rect),
                monitor: new GLib.Variant('i', s.monitor),
                workspace: new GLib.Variant('i', s.workspace),
                minimized: new GLib.Variant('b', (s.flags & STATE_MINIMIZED) !== 0),
                maximized: new GLib.Variant('b', (s.flags & STATE_MAXIMIZED) !== 0),
                fullscreen: new GLib.Variant('b', (s.flags & STATE_FULLSCREEN) !== 0),
                above: new GLib.Variant('b', (s.flags & STATE_ABOVE) !== 0),
                focused: new GLib.Variant('b', (s.flags & STATE_FOCUSED) !== 0),
                user: new GLib.Variant('b', (s.flags & STATE_USER) !== 0),
                shown: new GLib.Variant('b', tracked.shown),
                stack: new GLib.Variant('u', stack),
                flags: new GLib.Variant('u', s.flags),
                serial: new GLib.Variant('u', tracked.serial ?? 0),
            }];
        });
        return new GLib.Variant('(a(sa{sv}))', [out]);
    }

    _getMonitors() {
        const display = global.display;
        const workspace = global.workspace_manager.get_active_workspace();
        const primary = display.get_primary_monitor();
        const out = [];
        for (let i = 0; i < display.get_n_monitors(); i++) {
            out.push([i, rectArray(display.get_monitor_geometry(i)),
                rectArray(workspace.get_work_area_for_monitor(i)), display.get_monitor_scale(i), i === primary]);
        }
        return new GLib.Variant('(a(i(iiii)(iiii)db))', [out]);
    }

    // The work area of the active workspace across every monitor (what _NET_WORKAREA is), asked for only when
    // this version of the extension is there ('work-area' in FEATURES).
    _getWorkArea() {
        const ws = global.workspace_manager.get_active_workspace();
        return new GLib.Variant('((iiii))', [rectArray(ws.get_work_area_all_monitors())]);
    }
}
