//! Hiding the cursor on GNOME, through the bundled Shell extension.
//!
//! GNOME lets a Wayland client neither hide the cursor nor see keys typed
//! into other clients, so wbanish's extension does both inside gnome-shell.
//! We only start it with our options over $XDG_RUNTIME_DIR/wbanish.sock and
//! keep the connection open; it stops when we go away.  This needs neither
//! /dev/input nor the input group.
//!
//! The extension is also built into this binary, so that a wbanish installed
//! without the package (cargo install) can put it in place itself.

use std::error::Error;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify;

use crate::input::Mods;

struct Extension {
    uuid: &'static str,
    files: [(&'static str, &'static str); 2],
}

/// For GNOME 45 and later, which load extensions as ES modules.
const MODERN: Extension = Extension {
    uuid: "wbanish@ksobitov.github.io",
    files: [
        (
            "metadata.json",
            include_str!("../gnome-extension/wbanish@ksobitov.github.io/metadata.json"),
        ),
        (
            "extension.js",
            include_str!("../gnome-extension/wbanish@ksobitov.github.io/extension.js"),
        ),
    ],
};

/// The same extension for GNOME 42-44.
const LEGACY: Extension = Extension {
    uuid: "wbanish-legacy@ksobitov.github.io",
    files: [
        (
            "metadata.json",
            include_str!("../gnome-extension/wbanish-legacy@ksobitov.github.io/metadata.json"),
        ),
        (
            "extension.js",
            include_str!("../gnome-extension/wbanish-legacy@ksobitov.github.io/extension.js"),
        ),
    ],
};

const SOCKET: &str = "wbanish.sock";

/// How often to look for the extension when inotify has nothing to say.
const RETRY: Duration = Duration::from_secs(5);

/// Whether XDG_CURRENT_DESKTOP says GNOME.
pub fn is_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d == "GNOME"))
}

fn connect(path: &Path) -> Option<UnixStream> {
    UnixStream::connect(path).inspect_err(|e| debug!("{}: {e}", path.display())).ok()
}

/// GNOME Shell's major version, e.g. 50 for "GNOME Shell 50.1".
fn shell_version() -> Option<u32> {
    let out = Command::new("gnome-shell").arg("--version").stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().last()?.split('.').next()?.parse().ok()
}

fn user_extensions() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share")))?;
    Some(data.join("gnome-shell/extensions"))
}

fn system_extensions() -> impl Iterator<Item = PathBuf> {
    let dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let dirs: Vec<_> = dirs
        .as_bytes()
        .split(|&b| b == b':')
        .map(|d| PathBuf::from(OsStr::from_bytes(d)))
        .collect();
    dirs.into_iter().map(|d| d.join("gnome-shell/extensions"))
}

/// Makes sure GNOME Shell can find the extension: the package puts it in
/// /usr/share, a wbanish installed without it writes its own copy to
/// ~/.local/share, and keeps such a copy up to date, since GNOME Shell
/// prefers it to the system one.  Returns whether it wrote anything.
fn install(ext: &Extension) -> bool {
    let Some(user) = user_extensions().map(|d| d.join(ext.uuid)) else {
        return false;
    };
    let current = ext
        .files
        .iter()
        .all(|(name, text)| std::fs::read_to_string(user.join(name)).is_ok_and(|t| t == *text));
    let in_system = system_extensions().any(|d| d.join(ext.uuid).join("metadata.json").exists());
    if current || (!user.exists() && in_system) {
        return false;
    }
    let written = std::fs::create_dir_all(&user).and_then(|()| {
        ext.files.iter().try_for_each(|(name, text)| std::fs::write(user.join(name), text))
    });
    match written {
        Ok(()) => {
            eprintln!("wbanish: installed the GNOME Shell extension in {}", user.display());
            true
        }
        Err(e) => {
            eprintln!(
                "wbanish: can't install the GNOME Shell extension in {}: {e}",
                user.display()
            );
            false
        }
    }
}

/// What GNOME Shell says about the extension: its state number and error,
/// or None if it hasn't loaded it at all.
fn extension_state(uuid: &str) -> Option<(u32, String)> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Shell",
            "--object-path",
            "/org/gnome/Shell",
        ])
        .args(["--method", "org.gnome.Shell.Extensions.GetExtensionInfo", uuid])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let info = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        let start = info.find(&format!("'{name}': <"))? + name.len() + 5;
        Some(info[start..].split('>').next()?.to_owned())
    };
    let state = field("state")?.parse::<f64>().ok()? as u32;
    let error = field("error").unwrap_or_default().trim_matches('\'').to_owned();
    Some((state, error))
}

/// Explains why the extension isn't answering.  Returns the explanation and
/// whether waiting can help.
fn diagnose(ext: &Extension, installed: bool) -> (String, bool) {
    const LOGIN: &str = "GNOME Shell only finds new extensions when you log in, \
                         so log out and back in once";
    let switched_off = Command::new("gsettings")
        .args(["get", "org.gnome.shell", "disable-user-extensions"])
        .output()
        .is_ok_and(|o| o.stdout.starts_with(b"true"));
    if installed {
        return (LOGIN.into(), true);
    }
    if switched_off {
        return (
            "GNOME's extensions are switched off; turn on 'Use Extensions' in the Extensions app"
                .into(),
            true,
        );
    }
    match extension_state(ext.uuid) {
        None => (format!("the {} GNOME Shell extension isn't loaded; {LOGIN}", ext.uuid), true),
        // ERROR
        Some((3, error)) => {
            (format!("the {} GNOME Shell extension failed: {error}", ext.uuid), false)
        }
        // OUT_OF_DATE
        Some((4, _)) => {
            let version = shell_version().map_or_else(|| "this version".into(), |v| v.to_string());
            (
                format!(
                    "the {} GNOME Shell extension doesn't support GNOME Shell {version}",
                    ext.uuid
                ),
                false,
            )
        }
        // INACTIVE
        Some((2, _)) => (
            format!(
                "the {} GNOME Shell extension is turned off; turn it on in the Extensions app",
                ext.uuid
            ),
            true,
        ),
        Some(_) => (format!("the {} GNOME Shell extension isn't answering", ext.uuid), true),
    }
}

/// Starts the extension on a new connection, unless we are paused.
fn handshake(
    mut stream: UnixStream,
    start: &str,
    paused: bool,
) -> Result<UnixStream, Box<dyn Error>> {
    if paused {
        return Ok(stream);
    }
    stream.write_all(start.as_bytes())?;
    // An extension that doesn't answer is from an older wbanish, still
    // loaded because GNOME Shell only reloads extensions at login.
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reply = [0u8; 3];
    if stream.read_exact(&mut reply).is_err() || &reply != b"ok\n" {
        return Err("the running GNOME Shell extension is older than wbanish; \
                    log out and back in once to load the new one"
            .into());
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// Runs until killed, with SIGUSR1 arriving on `pause`.  When the extension
/// goes away (it is switched off, or gnome-shell disables and re-enables it
/// because another extension was switched off), waits for it to come back.
pub fn run(
    ignore: Mods,
    timeout: Option<Duration>,
    mut pause: UnixStream,
) -> Result<(), Box<dyn Error>> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .ok_or("XDG_RUNTIME_DIR is not set")?;
    let path = dir.join(SOCKET);
    let ms = timeout.map_or(0, |t| t.as_millis().max(1));
    let start = format!("start {} {ms}\n", ignore.names());

    let ext = if shell_version().is_some_and(|v| v < 45) { &LEGACY } else { &MODERN };
    let installed = install(ext);

    // Tells us when the extension creates its socket again.
    let inotify = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
    inotify::add_watch(
        &inotify,
        &dir,
        inotify::WatchFlags::CREATE | inotify::WatchFlags::MOVED_TO,
    )?;

    let mut fresh = connect(&path);
    if fresh.is_none() && !installed {
        // This starts the extension right away if gnome-shell already knows
        // about it, that is, if it was installed before this session began.
        debug!("enabling the {} GNOME Shell extension", ext.uuid);
        let _ = Command::new("gnome-extensions")
            .args(["enable", ext.uuid])
            .stderr(Stdio::null())
            .status();
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            fresh = connect(&path);
            if fresh.is_some() {
                break;
            }
        }
    }
    if fresh.is_none() {
        let (why, can_wait) = diagnose(ext, installed);
        if !can_wait {
            return Err(why.into());
        }
        eprintln!("wbanish: {why}; waiting for it");
    }

    let mut conn: Option<UnixStream> = None;
    let mut paused = false;
    // An outdated extension keeps its socket; try again only once it is
    // replaced, not every few seconds.
    let mut retry = true;
    loop {
        if let Some(s) = fresh.take() {
            match handshake(s, &start, paused) {
                Ok(s) => {
                    debug!("connected to the GNOME Shell extension");
                    conn = Some(s);
                }
                Err(e) => {
                    eprintln!("wbanish: {e}");
                    retry = false;
                }
            }
        }

        let wait = Timespec::try_from(RETRY)?;
        let (signal, created, extension) = {
            let mut fds =
                vec![PollFd::new(&pause, PollFlags::IN), PollFd::new(&inotify, PollFlags::IN)];
            if let Some(s) = &conn {
                fds.push(PollFd::new(s, PollFlags::IN));
            }
            let waiting = conn.is_none() && retry;
            match poll(&mut fds, waiting.then_some(&wait)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
                fds.get(2).map(|f| f.revents()),
            )
        };

        if signal {
            // One byte per signal; two signals in a row cancel out.
            let mut buf = [0u8; 64];
            let mut signals = 0;
            while let Ok(n) = pause.read(&mut buf)
                && n > 0
            {
                signals += n;
            }
            if signals % 2 == 1 {
                paused = !paused;
                eprintln!("wbanish: {}", if paused { "paused" } else { "resumed" });
                let msg = if paused { "stop\n" } else { start.as_str() };
                if let Some(s) = &mut conn
                    && s.write_all(msg.as_bytes()).is_err()
                {
                    conn = None;
                }
            }
        }

        if created {
            let mut buf = [MaybeUninit::<u8>::uninit(); 1024];
            let mut reader = inotify::Reader::new(&inotify, &mut buf);
            let mut ours = false;
            while let Ok(ev) = reader.next() {
                ours |= ev.file_name().is_some_and(|n| n.to_bytes() == SOCKET.as_bytes());
            }
            if ours && conn.is_none() {
                debug!("the GNOME Shell extension is back");
                retry = true;
                // Its socket is listening by the time it shows up.
                fresh = connect(&path);
            }
        }

        // The extension only answers "start" with "ok"; anything else, or
        // the end of the stream, means it went away.
        if let Some(flags) = extension
            && !flags.is_empty()
        {
            let mut buf = [0u8; 64];
            if matches!(conn.as_mut().map(|s| s.read(&mut buf)), Some(Ok(0) | Err(_))) {
                debug!("lost the GNOME Shell extension; waiting for it");
                conn = None;
            }
        }

        if conn.is_none() && fresh.is_none() && retry {
            fresh = connect(&path);
        }
    }
}
