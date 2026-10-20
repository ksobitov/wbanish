// GNOME doesn't let Wayland clients hide the cursor, so wbanish asks this
// extension to do it.  wbanish connects to $XDG_RUNTIME_DIR/wbanish.sock
// and writes "hide" or "show" lines; the cursor also comes back by itself
// as soon as the pointer moves.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

export default class WbanishExtension extends Extension {
    enable() {
        // GNOME 48 moved the tracker to the backend.
        this._tracker = global.backend.get_cursor_tracker?.() ??
            Meta.CursorTracker.get_for_display(global.display);
        this._hidden = false;
        this._motionId = 0;
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
        this._show();
        this._cancellable.cancel();
        for (const connection of this._connections)
            connection.close(null);
        this._connections.clear();
        this._service.stop();
        this._service.close();
        this._service = null;
        this._unlink();
        this._tracker = null;
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
                connection.close(null);
                return;
            }
            if (line === 'hide')
                this._hide();
            else if (line === 'show')
                this._show();
            readLine();
        });
        readLine();
    }

    _hide() {
        if (this._hidden)
            return;
        this._hidden = true;
        // GNOME 49 replaced set_pointer_visible() with inhibitors.
        if (this._tracker.inhibit_cursor_visibility)
            this._tracker.inhibit_cursor_visibility();
        else
            this._tracker.set_pointer_visible(false);
        this._motionId = this._tracker.connect('position-invalidated', () => this._show());
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
