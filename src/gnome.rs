//! Hiding the cursor on GNOME, through the bundled Shell extension.
//!
//! GNOME has no wlr-layer-shell and no other way for a client to hide the
//! cursor, so the wbanish extension does it inside gnome-shell.  We send it
//! "hide" and "show" lines over $XDG_RUNTIME_DIR/wbanish.sock; it shows the
//! cursor again by itself as soon as the pointer moves.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

pub const UUID: &str = "wbanish@ksobitov.github.io";

pub struct Gnome {
    path: PathBuf,
    stream: Option<UnixStream>,
    /// Whether we asked for the cursor to be hidden and the mouse hasn't
    /// moved since, as far as we know.
    hidden: bool,
    warned: bool,
}

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("wbanish.sock"))
}

/// Whether we are in a GNOME session, or the extension is running anyway.
pub fn is_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d == "GNOME"))
        || socket_path().is_some_and(|p| p.exists())
}

impl Gnome {
    pub fn connect() -> Result<Self, String> {
        let path = socket_path().ok_or("XDG_RUNTIME_DIR is not set")?;
        let mut gnome = Gnome { path, stream: None, hidden: false, warned: false };
        if gnome.reconnect() {
            return Ok(gnome);
        }

        // This starts the extension right away if gnome-shell already knows
        // about it, that is, if it was installed before the session began.
        debug!("enabling the {UUID} GNOME Shell extension");
        match Command::new("gnome-extensions").args(["enable", UUID]).status() {
            Ok(s) if s.success() => {}
            Ok(s) => debug!("gnome-extensions enable: {s}"),
            Err(e) => debug!("gnome-extensions: {e}"),
        }
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if gnome.reconnect() {
                return Ok(gnome);
            }
        }
        gnome.warn();
        Ok(gnome)
    }

    fn reconnect(&mut self) -> bool {
        match UnixStream::connect(&self.path).and_then(|s| s.set_nonblocking(true).map(|()| s)) {
            Ok(s) => {
                debug!("connected to the GNOME Shell extension");
                self.stream = Some(s);
                self.warned = false;
                true
            }
            Err(e) => {
                debug!("{}: {e}", self.path.display());
                false
            }
        }
    }

    fn warn(&mut self) {
        if !self.warned {
            eprintln!(
                "wbanish: the {UUID} GNOME Shell extension isn't running; \
                 if wbanish was just installed, log out and back in"
            );
            self.warned = true;
        }
    }

    fn send(&mut self, msg: &[u8]) {
        if self.stream.is_none() && !self.reconnect() {
            self.warn();
            return;
        }
        // On any error, including a full buffer, drop the connection rather
        // than risk leaving half a line in it.
        if let Err(e) = self.stream.as_mut().unwrap().write_all(msg) {
            debug!("lost the GNOME Shell extension: {e}");
            self.stream = None;
            if e.kind() != io::ErrorKind::WouldBlock && self.reconnect() {
                let _ = self.stream.as_mut().unwrap().write_all(msg);
            }
        }
    }

    /// Asks for the cursor to be hidden; sent on every key press since we
    /// can't see the extension showing it again.
    pub fn hide(&mut self) {
        if !self.hidden {
            debug!("hiding cursor");
        }
        self.hidden = true;
        self.send(b"hide\n");
    }

    pub fn show(&mut self) {
        debug!("showing cursor");
        self.hidden = false;
        self.send(b"show\n");
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// The extension brings the cursor back on motion by itself.
    pub fn pointer_moved(&mut self) {
        self.hidden = false;
    }
}
