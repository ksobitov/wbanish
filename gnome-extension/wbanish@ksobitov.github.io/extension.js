// wbanish's GNOME Shell extension: hides the mouse cursor while you type.
//
// GNOME doesn't let Wayland clients hide the cursor, or see keys typed into
// other clients, so this has to run inside gnome-shell.  wbanish connects to
// $XDG_RUNTIME_DIR/wbanish.sock and sends "start MODIFIERS TIMEOUT_MS"; while
// it stays connected, key presses hide the cursor and moving the pointer
// shows it again.  "stop", or closing the connection, turns that off.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

// Pressing one of these alone never hides the cursor, so Ctrl+click and
// Super+drag keep working.
const MODIFIER_KEYS = new Set([
    Clutter.KEY_Shift_L, Clutter.KEY_Shift_R, Clutter.KEY_Shift_Lock,
    Clutter.KEY_Control_L, Clutter.KEY_Control_R,
    Clutter.KEY_Alt_L, Clutter.KEY_Alt_R, Clutter.KEY_Meta_L, Clutter.KEY_Meta_R,
    Clutter.KEY_Super_L, Clutter.KEY_Super_R, Clutter.KEY_Hyper_L, Clutter.KEY_Hyper_R,
    Clutter.KEY_ISO_Level3_Shift, Clutter.KEY_ISO_Level5_Shift,
    Clutter.KEY_Caps_Lock, Clutter.KEY_Num_Lock,
]);

// wbanish's -i names.
const MODIFIER_MASKS = {
    shift: Clutter.ModifierType.SHIFT_MASK,
    control: Clutter.ModifierType.CONTROL_MASK,
    alt: Clutter.ModifierType.MOD1_MASK,
    super: Clutter.ModifierType.MOD4_MASK | Clutter.ModifierType.SUPER_MASK,
    altgr: Clutter.ModifierType.MOD5_MASK,
    capslock: Clutter.ModifierType.LOCK_MASK,
    numlock: Clutter.ModifierType.MOD2_MASK,
};

export default class WbanishExtension extends Extension {
    enable() {
        // GNOME 48 moved the tracker to the backend.
        this._tracker = global.backend.get_cursor_tracker?.() ??
            Meta.CursorTracker.get_for_display(global.display);
        this._idleMonitor = global.backend.get_core_idle_monitor();
        this._owner = null;
        this._ignore = 0;
        this._hidden = false;
        this._motionId = 0;
        this._activeWatch = 0;
        this._idleWatch = 0;
        this._connections = new Set();
        this._cancellable = new Gio.Cancellable();

        this._path = GLib.build_filenamev([GLib.get_user_runtime_dir(), 'wbanish.sock']);
        this._unlink();
        this._service = new Gio.SocketService();
        this._service.add_address(Gio.UnixSocketAddress.new(this._path),
            Gio.SocketType.STREAM, Gio.SocketProtocol.DEFAULT, null);
        this._service.connect('incoming', (_service, connection) => {
            this._serve(connection);
            return true;
        });
        this._service.start();
    }

    disable() {
        this._stop();
        this._cancellable.cancel();
        for (const connection of this._connections)
            connection.close(null);
        this._connections.clear();
        this._service.stop();
        this._service.close();
        this._service = null;
        this._unlink();
        this._tracker = null;
        this._idleMonitor = null;
    }

    _unlink() {
        try {
            Gio.File.new_for_path(this._path).delete(null);
        } catch {
            // Nothing left over from a previous session.
        }
    }

    _serve(connection) {
        this._connections.add(connection);
        const input = new Gio.DataInputStream({base_stream: connection.get_input_stream()});
        const readLine = () => input.read_line_async(GLib.PRIORITY_DEFAULT, this._cancellable, (stream, result) => {
            let line = null;
            try {
                [line] = stream.read_line_finish_utf8(result);
            } catch {
                // Cancelled, or wbanish went away.
            }
            if (line === null) {
                this._connections.delete(connection);
                if (this._owner === connection)
                    this._stop();
                connection.close(null);
                return;
            }
            const [command, ...args] = line.split(' ');
            if (command === 'start')
                this._start(connection, ...args);
            else if (command === 'stop' && this._owner === connection)
                this._stop();
            readLine();
        });
        readLine();
    }

    _start(owner, modifiers = '-', timeout = '0') {
        this._stop();
        this._owner = owner;
        this._ignore = 0;
        for (const name of modifiers.split(','))
            this._ignore |= MODIFIER_MASKS[name] ?? 0;
        const ms = parseInt(timeout);
        if (ms > 0)
            this._idleWatch = this._idleMonitor.add_idle_watch(ms, () => this._hide());
        this._watchKeys();
    }

    _stop() {
        this._owner = null;
        this._show();
        this._unwatchKeys();
        if (this._idleWatch) {
            this._idleMonitor.remove_watch(this._idleWatch);
            this._idleWatch = 0;
        }
    }

    // A user-active watch fires on the next input event of any kind, from
    // inside Mutter's event handling and before the event goes to the
    // focused client, so Clutter.get_current_event() is that event.
    _watchKeys() {
        if (this._activeWatch)
            return;
        this._activeWatch = this._idleMonitor.add_user_active_watch(() => {
            this._activeWatch = 0;
            this._onInput();
        });
    }

    _unwatchKeys() {
        if (this._activeWatch) {
            this._idleMonitor.remove_watch(this._activeWatch);
            this._activeWatch = 0;
        }
    }

    _onInput() {
        if (!this._owner)
            return;
        const event = Clutter.get_current_event();
        if (event?.type() === Clutter.EventType.KEY_PRESS &&
            !MODIFIER_KEYS.has(event.get_key_symbol()) &&
            (event.get_state() & this._ignore) === 0)
            this._hide();
        if (!this._hidden)
            this._watchKeys();
    }

    _hide() {
        if (this._hidden || !this._owner)
            return;
        this._hidden = true;
        this._unwatchKeys();
        // GNOME 49 replaced set_pointer_visible() with inhibitors.
        if (this._tracker.inhibit_cursor_visibility)
            this._tracker.inhibit_cursor_visibility();
        else
            this._tracker.set_pointer_visible(false);
        this._motionId = this._tracker.connect('position-invalidated', () => {
            this._show();
            this._watchKeys();
        });
    }

    _show() {
        if (!this._hidden)
            return;
        this._hidden = false;
        this._tracker.disconnect(this._motionId);
        this._motionId = 0;
        if (this._tracker.uninhibit_cursor_visibility)
            this._tracker.uninhibit_cursor_visibility();
        else
            this._tracker.set_pointer_visible(true);
    }
}
