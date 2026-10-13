// Remote desktop on the lock screen (xwlrvnc) — GNOME Shell 46–50.
//
// When the screen locks, gnome-shell switches to its 'unlock-dialog' session mode, which does not allow
// screencasts, and tells mutter's remote access controller to inhibit remote access
// (js/ui/main.js _sessionUpdated). mutter then closes every RemoteDesktop/ScreenCast session and refuses new
// ones until the screen is unlocked, so a remote user sees a frozen picture, cannot type the password, and
// someone has to unlock at the computer.
//
// This extension skips that one inhibition while the session is locked and undoes the skip at unlock, so the
// controller's count stays balanced. Sessions keep running through the lock; viewers see the lock screen and
// can unlock it with the account's password, which the lock screen still asks for as usual. The login screen
// (GDM's own gnome-shell, 'gdm' mode, which inhibits before any extension loads) is affected only where an
// administrator installed the extension system-wide and enabled it for GDM; enable() then lifts that
// inhibition (README: "Login screen").
//
// What it changes, so you can decide: while it is enabled, every client of mutter's remote desktop and
// screencast APIs keeps capturing on the lock screen (a remote desktop server, but also e.g. a browser that
// is sharing the screen), and a remote desktop client that already had a session can type into the
// password prompt. Nothing is captured that the lock screen does not show; notifications on the lock screen
// follow GNOME's "Lock Screen Notifications" setting. Disable the extension to get GNOME's default back.

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

/** The session modes where gnome-shell inhibits remote access and this extension lifts that. */
function keepRemoteAccess(mode) {
    // the user's locked session; the login screen only when an administrator installed the extension
    // system-wide and enabled it for the greeter (see README)
    return mode.isLocked || mode.isGreeter;
}

export default class RemoteDesktopOnLockScreen extends Extension {
    enable() {
        const controller = global.backend.get_remote_access_controller?.();
        if (!controller) {
            console.warn(`${this.uuid}: this mutter has no remote access controller; nothing to do`);
            return;
        }
        this._controller = controller;
        this._skipped = false;
        // the class methods, looked up before the instance overrides below shadow them
        const inhibit = controller.inhibit_remote_access;
        const uninhibit = controller.uninhibit_remote_access;
        this._inhibit = inhibit;
        controller.inhibit_remote_access = () => {
            if (keepRemoteAccess(Main.sessionMode)) {
                this._skipped = true;
                console.log(`${this.uuid}: ${Main.sessionMode.currentMode}: remote desktop sessions stay open`);
                return;
            }
            inhibit.call(controller);
        };
        controller.uninhibit_remote_access = () => {
            if (this._skipped) {
                this._skipped = false;
                return;
            }
            uninhibit.call(controller);
        };
        // Enabled where gnome-shell has already inhibited remote access (always so on the login screen,
        // which inhibits before extensions load; or enabled while locked): lift it now and swallow
        // gnome-shell's own uninhibit later.
        if (!Main.sessionMode.allowScreencast && keepRemoteAccess(Main.sessionMode) &&
            !global.backend.is_headless()) {
            uninhibit.call(controller);
            this._skipped = true;
            console.log(`${this.uuid}: ${Main.sessionMode.currentMode}: remote access allowed`);
        }
    }

    disable() {
        const controller = this._controller;
        if (!controller)
            return;
        delete controller.inhibit_remote_access;
        delete controller.uninhibit_remote_access;
        if (this._skipped) {
            // gnome-shell believes remote access is inhibited here: make it so, so that its uninhibit
            // (at unlock) balances
            this._skipped = false;
            this._inhibit.call(controller);
        }
        this._controller = null;
    }
}
