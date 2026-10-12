// Reports the focused window to the Inkswitch daemon over D-Bus. This is the GNOME counterpart
// of the KWin script the daemon loads on KDE Plasma (src/kwin_script.js).

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.github.giobinda.Inkswitch';
const OBJECT_PATH = '/io/github/giobinda/Inkswitch';

export default class InkswitchExtension extends Extension {
    enable() {
        this._focusId = global.display.connect('notify::focus-window', () => this._report());
        // Report again whenever the daemon (re)starts, so it knows the window already in front.
        this._watchId = Gio.bus_watch_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameWatcherFlags.NONE,
            () => this._report(), null);
    }

    disable() {
        if (this._focusId) {
            global.display.disconnect(this._focusId);
            this._focusId = null;
        }
        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = null;
        }
    }

    _report() {
        const window = global.display.focus_window;
        if (!window || window.get_window_type() !== Meta.WindowType.NORMAL)
            return;
        // For Wayland windows the class is the application id, which is what profiles are tied to.
        const parameters = new GLib.Variant('(sss)', [
            window.get_wm_class() ?? '',
            window.get_wm_class_instance() ?? '',
            window.get_title() ?? '',
        ]);
        Gio.DBus.session.call(
            BUS_NAME, OBJECT_PATH, BUS_NAME, 'Focus', parameters, null,
            Gio.DBusCallFlags.NO_AUTO_START, -1, null,
            (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch {
                    // The daemon is not running; it gets a report as soon as it starts.
                }
            });
    }
}
