//! Hiding the cursor on GNOME, through the bundled Shell extension.
//!
//! GNOME lets a Wayland client neither hide the cursor nor see keys typed
//! into other clients, so the wbanish extension does both inside
//! gnome-shell.  We only start it with our options over
//! $XDG_RUNTIME_DIR/wbanish.sock and keep the connection open; it stops
//! when we go away.  This needs neither /dev/input nor the input group.

use std::error::Error;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::input::Mods;

pub const UUID: &str = "wbanish@ksobitov.github.io";

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("wbanish.sock"))
}

/// Whether we are in a GNOME session, or the extension is running anyway.
pub fn is_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d == "GNOME"))
        || socket_path().is_some_and(|p| p.exists())
}

fn connect(path: &PathBuf) -> Option<UnixStream> {
    UnixStream::connect(path).inspect_err(|e| debug!("{}: {e}", path.display())).ok()
}

/// Runs until killed, with SIGUSR1 arriving on `pause`.
pub fn run(
    ignore: Mods,
    timeout: Option<Duration>,
    mut pause: UnixStream,
) -> Result<(), Box<dyn Error>> {
    let path = socket_path().ok_or("XDG_RUNTIME_DIR is not set")?;
    let start = format!("start {} {}\n", ignore.names(), timeout.map_or(0, |t| t.as_millis()));

    let mut stream = connect(&path);
    if stream.is_none() {
        // This starts the extension right away if gnome-shell already knows
        // about it, that is, if it was installed before this session began.
        debug!("enabling the {UUID} GNOME Shell extension");
        match Command::new("gnome-extensions").args(["enable", UUID]).status() {
            Ok(s) if s.success() => {}
            Ok(s) => debug!("gnome-extensions enable: {s}"),
            Err(e) => debug!("gnome-extensions: {e}"),
        }
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            stream = connect(&path);
            if stream.is_some() {
                break;
            }
        }
    }
    let Some(mut stream) = stream else {
        return Err(format!(
            "the {UUID} GNOME Shell extension isn't running; GNOME Shell only \
             finds newly installed extensions when you log in, so log out and \
             back in once"
        )
        .into());
    };
    debug!("connected to the GNOME Shell extension");
    stream.write_all(start.as_bytes())?;

    let mut paused = false;
    loop {
        let mut fds = [PollFd::new(&pause, PollFlags::IN), PollFd::new(&stream, PollFlags::IN)];
        match poll(&mut fds, None::<&Timespec>) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let (signal, extension) = (fds[0].revents(), fds[1].revents());

        if !signal.is_empty() {
            let mut buf = [0u8; 64];
            while matches!(pause.read(&mut buf), Ok(n) if n > 0) {}
            paused = !paused;
            eprintln!("wbanish: {}", if paused { "paused" } else { "resumed" });
            let msg = if paused { "stop\n" } else { start.as_str() };
            stream.write_all(msg.as_bytes())?;
        }
        // The extension never writes, so this means it went away.
        if !extension.is_empty() {
            return Err("the GNOME Shell extension was disabled".into());
        }
    }
}
