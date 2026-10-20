//! wbanish: hide the mouse cursor while typing on Wayland, like xbanish.

use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{FlockOperation, flock};
use wayland_client::backend::WaylandError;

static DEBUG: AtomicBool = AtomicBool::new(false);

macro_rules! debug {
    ($($arg:tt)*) => {
        if crate::DEBUG.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("wbanish: {}", format_args!($($arg)*));
        }
    };
}

mod gnome;
mod input;
mod overlay;

use input::{Activity, Input, Mods};
use overlay::Overlay;

const USAGE: &str = "\
usage: wbanish [-d] [-i modifier]... [-t seconds]
    -d             print debugging messages
    -i modifier    don't hide the cursor for keys pressed with this modifier:
                   shift, control, alt, super, altgr, capslock, numlock or all
    -t seconds     also hide the cursor after this long without mouse activity
    -h, --help     show this help
    -V, --version  show the version
SIGUSR1 pauses and resumes wbanish.";

/// The longest -t we take, about 11 days.
const MAX_TIMEOUT: f64 = 1_000_000.0;

#[derive(Debug, Default, PartialEq)]
struct Options {
    debug: bool,
    ignore: Mods,
    timeout: Option<Duration>,
}

enum Parsed {
    Run(Options),
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut opts = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            _ => {}
        }
        let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty() && !f.starts_with('-'))
        else {
            return Err(format!("unexpected argument '{arg}'"));
        };
        for (i, c) in flags.char_indices() {
            match c {
                'd' => opts.debug = true,
                'h' => return Ok(Parsed::Help),
                'V' => return Ok(Parsed::Version),
                'i' | 't' => {
                    // The value is the rest of this argument or the next one.
                    let rest = &flags[i + 1..];
                    let value = if rest.is_empty() {
                        args.next().ok_or(format!("-{c} needs an argument"))?
                    } else {
                        rest.to_owned()
                    };
                    if c == 'i' {
                        opts.ignore |=
                            Mods::parse(&value).ok_or(format!("unknown modifier '{value}'"))?;
                    } else {
                        let secs = value
                            .parse::<f64>()
                            .ok()
                            .filter(|s| (0.001..=MAX_TIMEOUT).contains(s))
                            .ok_or(format!("invalid timeout '{value}'"))?;
                        opts.timeout = Some(Duration::from_secs_f64(secs));
                    }
                    break;
                }
                'a' | 'm' | 's' => {
                    return Err(format!("-{c} from xbanish is not supported on Wayland"));
                }
                _ => return Err(format!("unknown option '-{c}'")),
            }
        }
    }
    Ok(Parsed::Run(opts))
}

fn main() -> ExitCode {
    let args: Result<Vec<String>, _> =
        std::env::args_os().skip(1).map(|a| a.into_string()).collect();
    let parsed = match args {
        Ok(args) => parse_args(args),
        Err(arg) => Err(format!("unexpected argument {arg:?}")),
    };
    let opts = match parsed {
        Ok(Parsed::Run(o)) => o,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("wbanish {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("wbanish: {e}");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    DEBUG.store(opts.debug, Ordering::Relaxed);
    match run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wbanish: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Locks $XDG_RUNTIME_DIR/wbanish.lock for as long as the returned file is
/// open, so that a second wbanish, say one started by hand next to the
/// autostarted one, exits instead of fighting over the cursor.
fn lock() -> Result<File, Box<dyn Error>> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .ok_or("XDG_RUNTIME_DIR is not set")?;
    let path = PathBuf::from(dir).join("wbanish.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::WOULDBLOCK) => {
            Err(format!("wbanish is already running ({} is locked)", path.display()).into())
        }
        Err(e) => Err(format!("{}: {e}", path.display()).into()),
    }
}

/// Reads what SIGUSR1 wrote to the pipe and returns whether it toggles
/// pausing: one byte per signal, so two in a row cancel out.
fn take_signals(pipe: &mut UnixStream) -> bool {
    let mut buf = [0u8; 64];
    let mut signals = 0;
    while let Ok(n) = pipe.read(&mut buf)
        && n > 0
    {
        signals += n;
    }
    signals % 2 == 1
}

fn run(opts: &Options) -> Result<(), Box<dyn Error>> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "x11") {
        return Err("this is an X11 session; wbanish is for Wayland, use xbanish there".into());
    }
    let _lock = lock()?;

    // SIGUSR1 pauses and resumes, e.g. for games.
    let (mut sig_rx, sig_tx) = UnixStream::pair()?;
    sig_rx.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGUSR1, sig_tx)?;
    let mut paused = false;

    // Where the compositor has wlr-layer-shell we hide the cursor
    // ourselves; on GNOME, which hasn't, its Shell extension does.
    let mut wl = match Overlay::connect() {
        Ok(wl) => wl,
        Err(e) if e.is::<overlay::NoLayerShell>() && gnome::is_session() => {
            return gnome::run(opts.ignore, opts.timeout, sig_rx);
        }
        Err(e) => return Err(e),
    };
    let mut input = Input::open(opts.timeout.is_some(), opts.ignore)?;

    // Exiting while the overlay is up would leave the cursor invisible until
    // the mouse moves, so on these signals we show it first.
    let (quit_rx, quit_tx) = UnixStream::pair()?;
    for signal in
        [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP]
    {
        signal_hook::low_level::pipe::register(signal, quit_tx.try_clone()?)?;
    }

    let mut last_pointer = Instant::now();
    loop {
        // Dispatch whatever is queued; pointer events may show the cursor.
        let guard = loop {
            wl.dispatch_pending()?;
            if wl.take_pointer_activity() {
                last_pointer = Instant::now();
            }
            wl.flush()?;
            if let Some(g) = wl.prepare_read() {
                break g;
            }
        };

        let mut timeout = None;
        if let Some(t) = opts.timeout
            && !paused
            && !wl.is_hidden()
        {
            let idle = last_pointer.elapsed();
            if idle >= t {
                debug!("no mouse activity for {:.1}s", idle.as_secs_f64());
                wl.hide();
                drop(guard);
                continue;
            }
            timeout = Some(Timespec::try_from(t - idle)?);
        }

        // Fixed slots first, then one per input device.
        const WAYLAND: usize = 0;
        const SIGNAL: usize = 1;
        const INOTIFY: usize = 2;
        const QUIT: usize = 3;
        const DEVICES: usize = 4;
        let revents: Vec<PollFlags> = {
            let mut fds = vec![
                PollFd::from_borrowed_fd(guard.connection_fd(), PollFlags::IN),
                PollFd::new(&sig_rx, PollFlags::IN),
                PollFd::from_borrowed_fd(input.inotify_fd(), PollFlags::IN),
                PollFd::new(&quit_rx, PollFlags::IN),
            ];
            for i in 0..input.len() {
                fds.push(PollFd::from_borrowed_fd(input.device_fd(i), PollFlags::IN));
            }
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            fds.iter().map(|f| f.revents()).collect()
        };

        if revents[WAYLAND].is_empty() {
            drop(guard);
        } else {
            match guard.read() {
                Ok(_) => {}
                // Only events for objects we already destroyed, say covers
                // the compositor configured once more as we removed them.
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }

        if !revents[QUIT].is_empty() {
            debug!("exiting");
            wl.show();
            wl.flush()?;
            return Ok(());
        }

        if !revents[SIGNAL].is_empty() && take_signals(&mut sig_rx) {
            paused = !paused;
            eprintln!("wbanish: {}", if paused { "paused" } else { "resumed" });
            if paused {
                wl.show();
            }
            last_pointer = Instant::now();
        }

        // Read devices back to front so removing one keeps the others' indices.
        for i in (0..input.len()).rev() {
            let flags = revents[DEVICES + i];
            if flags.is_empty() {
                continue;
            }
            let alive = input.read(i, &mut |a| match a {
                Activity::Key if !paused => wl.hide(),
                Activity::Key => {}
                Activity::Pointer => last_pointer = Instant::now(),
            });
            if !alive || flags.intersects(PollFlags::ERR | PollFlags::HUP) {
                input.remove(i);
            }
        }

        if !revents[INOTIFY].is_empty() {
            input.hotplug();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        match parse_args(args.iter().map(|s| s.to_string()))? {
            Parsed::Run(o) => Ok(o),
            Parsed::Help => Err("help".into()),
            Parsed::Version => Err("version".into()),
        }
    }

    #[test]
    fn args() {
        assert_eq!(parse(&[]), Ok(Options::default()));
        let o = parse(&["-d", "-i", "shift", "-ictrl", "-t", "2.5"]).unwrap();
        assert!(o.debug);
        assert_eq!(o.ignore, Mods::SHIFT | Mods::CONTROL);
        assert_eq!(o.timeout, Some(Duration::from_millis(2500)));
        assert_eq!(parse(&["-dt3"]).unwrap().timeout, Some(Duration::from_secs(3)));
        assert_eq!(parse(&["-h"]), Err("help".into()));
        assert_eq!(parse(&["--help"]), Err("help".into()));
        assert_eq!(parse(&["-d", "--version"]), Err("version".into()));
        assert!(parse(&["-i", "mod3"]).unwrap_err().contains("modifier"));
        assert!(parse(&["-t", "0"]).is_err());
        assert!(parse(&["-t", "1e20"]).is_err());
        assert!(parse(&["-t", "nan"]).is_err());
        assert!(parse(&["-t"]).is_err());
        assert!(parse(&["-m", "nw"]).is_err());
        assert!(parse(&["--frobnicate"]).is_err());
        assert!(parse(&["foo"]).is_err());
    }
}
