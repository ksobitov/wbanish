// wbanish's GNOME Shell extension: hides the mouse cursor while you type.
//
// GNOME doesn't let Wayland clients hide the cursor, or see keys typed into
// other clients, so this has to run inside gnome-shell.  wbanish connects to
// $XDG_RUNTIME_DIR/wbanish.sock and sends "start MODIFIERS TIMEOUT_MS", which
// is answered with "ok"; while it stays connected, key presses hide the
// cursor, and moving the pointer, clicking or scrolling shows it again.
// "stop", or closing the connection, turns that off.
//
// This is the GNOME 42-44 copy, in the extension format before ES modules;
// it is the same code as wbanish@ksobitov.github.io, keep the two in step.

const {Clutter, Gio, GLib, Meta} = imports.gi;

// Pressing one of these alone never hides the cursor, so Ctrl+click,
// Super+drag and switching layouts with Alt+Shift keep it.
const MODIFIER_KEYS = new Set([
    Clutter.KEY_Shift_L, Clutter.KEY_Shift_R, Clutter.KEY_Shift_Lock,
    Clutter.KEY_Control_L, Clutter.KEY_Control_R,
    Clutter.KEY_Alt_L, Clutter.KEY_Alt_R, Clutter.KEY_Meta_L, Clutter.KEY_Meta_R,
    Clutter.KEY_Super_L, Clutter.KEY_Super_R, Clutter.KEY_Hyper_L, Clutter.KEY_Hyper_R,
    Clutter.KEY_ISO_Level3_Shift, Clutter.KEY_ISO_Level5_Shift, Clutter.KEY_Mode_switch,
    Clutter.KEY_ISO_Next_Group, Clutter.KEY_ISO_Prev_Group,
    Clutter.KEY_ISO_First_Group, Clutter.KEY_ISO_Last_Group,
    Clutter.KEY_Caps_Lock, Clutter.KEY_Num_Lock,
]);

// Events that mean the mouse, touchpad or tablet is in use.
const POINTER_EVENTS = new Set([
    Clutter.EventType.MOTION, Clutter.EventType.BUTTON_PRESS, Clutter.EventType.SCROLL,
    Clutter.EventType.TOUCHPAD_SWIPE, Clutter.EventType.TOUCHPAD_PINCH,
    Clutter.EventType.TOUCHPAD_HOLD, Clutter.EventType.PROXIMITY_IN,
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

// Commands are a few bytes; a longer line is not from wbanish.
const MAX_LINE = 256;

class WbanishExtension {
    enable() {
        this._tracker = Meta.CursorTracker.get_for_display(global.display);
        this._idleMonitor = global.backend.get_core_idle_monitor();
        this._seat = Clutter.get_default_backend().get_default_seat();
        this._owner = null;
        this._ignore = 0;
        this._timeout = 0;
        this._timeoutId = 0;
        this._lastPointer = 0;
        this._hidden = false;
        this._activeWatch = 0;
        this._clients = new Set();
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
        for (const client of this._clients)
            client.connection.close(null);
        this._clients.clear();
        this._service.stop();
        this._service.close();
        this._service = null;
        this._unlink();
        this._tracker = null;
        this._idleMonitor = null;
        this._seat = null;
    }

    _unlink() {
        try {
            Gio.File.new_for_path(this._path).delete(null);
        } catch {
            // Nothing left over from a previous session.
        }
    }

    _serve(connection) {
        const client = {connection, buffer: '', writing: false};
        this._clients.add(client);
        this._read(client);
    }

    _drop(client) {
        if (!this._clients.delete(client))
            return;
        if (this._owner === client)
            this._stop();
        client.connection.close(null);
    }

    // Reads in small chunks so that a stray client can't make gnome-shell
    // buffer an endless line.
    _read(client) {
        const input = client.connection.get_input_stream();
        input.read_bytes_async(MAX_LINE, GLib.PRIORITY_DEFAULT, this._cancellable, (stream, result) => {
            let bytes = null;
            try {
                bytes = stream.read_bytes_finish(result);
            } catch {
                // Cancelled, or wbanish went away.
            }
            if (!this._clients.has(client))
                return;
            if (!bytes || bytes.get_size() === 0) {
                this._drop(client);
                return;
            }
            client.buffer += new TextDecoder().decode(bytes.toArray());
            let newline;
            while ((newline = client.buffer.indexOf('\n')) >= 0) {
                const line = client.buffer.slice(0, newline);
                client.buffer = client.buffer.slice(newline + 1);
                this._command(client, line);
            }
            if (client.buffer.length > MAX_LINE)
                this._drop(client);
            else if (this._clients.has(client))
                this._read(client);
        });
    }

    _command(client, line) {
        const [command, ...args] = line.split(' ');
        if (command === 'start') {
            this._start(client, ...args);
            this._reply(client, 'ok\n');
        } else if (command === 'stop' && this._owner === client) {
            this._stop();
        }
    }

    // Never blocks gnome-shell: a client that doesn't read its replies is
    // dropped instead.
    _reply(client, text) {
        if (client.writing) {
            this._drop(client);
            return;
        }
        client.writing = true;
        const output = client.connection.get_output_stream();
        output.write_bytes_async(new GLib.Bytes(new TextEncoder().encode(text)),
            GLib.PRIORITY_DEFAULT, this._cancellable, (stream, result) => {
                client.writing = false;
                try {
                    stream.write_bytes_finish(result);
                } catch {
                    this._drop(client);
                }
            });
    }

    _start(owner, modifiers = '-', timeout = '0') {
        this._stop();
        this._owner = owner;
        this._ignore = 0;
        for (const name of modifiers.split(','))
            this._ignore |= MODIFIER_MASKS[name] ?? 0;
        const ms = parseInt(timeout);
        this._timeout = ms > 0 ? ms : 0;
        this._lastPointer = GLib.get_monotonic_time();
        this._watch();
        this._armTimeout();
    }

    _stop() {
        this._owner = null;
        this._show();
        if (this._activeWatch) {
            this._idleMonitor.remove_watch(this._activeWatch);
            this._activeWatch = 0;
        }
        if (this._timeoutId) {
            GLib.source_remove(this._timeoutId);
            this._timeoutId = 0;
        }
    }

    // A user-active watch fires on the next input event of any kind, from
    // inside Mutter's event handling and before the event goes to a client,
    // so Clutter.get_current_event() is that event.  Showing the cursor at
    // that point also gives the pointer back to the window in time for the
    // click or scroll that caused it.
    _watch() {
        if (this._activeWatch)
            return;
        this._activeWatch = this._idleMonitor.add_user_active_watch(() => {
            this._activeWatch = 0;
            this._onInput();
            if (this._owner)
                this._watch();
        });
    }

    _onInput() {
        const event = Clutter.get_current_event();
        if (!event || !this._owner)
            return;
        const type = event.type();
        if (type === Clutter.EventType.KEY_PRESS) {
            if (!this._hidden &&
                !(event.get_flags() & Clutter.EventFlags.FLAG_REPEATED) &&
                !MODIFIER_KEYS.has(event.get_key_symbol()) &&
                (event.get_state() & this._ignore) === 0)
                this._hide();
        } else if (POINTER_EVENTS.has(type)) {
            this._lastPointer = GLib.get_monotonic_time();
            this._show();
            this._armTimeout();
        }
    }

    // For -t: hides the cursor once the pointer has been still for that long.
    _armTimeout() {
        if (!this._timeout || this._timeoutId || this._hidden || !this._owner)
            return;
        const idle = (GLib.get_monotonic_time() - this._lastPointer) / 1000;
        const wait = Math.max(1, Math.ceil(this._timeout - idle));
        this._timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, wait, () => {
            this._timeoutId = 0;
            if ((GLib.get_monotonic_time() - this._lastPointer) / 1000 >= this._timeout)
                this._hide();
            else
                this._armTimeout();
            return GLib.SOURCE_REMOVE;
        });
    }

    _hide() {
        if (this._hidden || !this._owner)
            return;
        this._hidden = true;
        // Hiding the cursor also takes the pointer away from the window under
        // it, unless that is inhibited; keep it there, as xbanish does, so
        // clicks, scrolling, hover effects and pointer locks keep working.
        this._seat.inhibit_unfocus();
        this._tracker.set_pointer_visible(false);
    }

    _show() {
        if (!this._hidden)
            return;
        this._hidden = false;
        this._tracker.set_pointer_visible(true);
        this._seat.uninhibit_unfocus();
    }
}

// eslint-disable-next-line no-unused-vars
function init() {
    return new WbanishExtension();
}
