//! wbanish: hide the mouse cursor while typing on Wayland, like xbanish.

use std::error::Error;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};

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

use gnome::Gnome;
use input::{Activity, Input, Mods};
use overlay::Overlay;

/// How the cursor gets hidden: our own overlay where the compositor has
/// wlr-layer-shell, or the Shell extension on GNOME.
enum Hider {
    Overlay(Box<Overlay>),
    Gnome(Gnome),
}

impl Hider {
    fn hide(&mut self) {
        match self {
            Hider::Overlay(o) => o.hide(),
            Hider::Gnome(g) => g.hide(),
        }
    }

    fn show(&mut self) {
        match self {
            Hider::Overlay(o) => o.show(),
            Hider::Gnome(g) => g.show(),
        }
    }

    fn is_hidden(&self) -> bool {
        match self {
            Hider::Overlay(o) => o.is_hidden(),
            Hider::Gnome(g) => g.is_hidden(),
        }
    }

    /// The mouse was used, as seen on evdev.
    fn pointer_moved(&mut self) {
        if let Hider::Gnome(g) = self {
            g.pointer_moved();
        }
    }
}

const USAGE: &str = "usage: wbanish [-d] [-i modifier]... [-t seconds]";

#[derive(Debug, Default, PartialEq)]
struct Options {
    debug: bool,
    ignore: Mods,
    timeout: Option<Duration>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
            return Err(format!("unexpected argument '{arg}'"));
        };
        for (i, c) in flags.char_indices() {
            match c {
                'd' => opts.debug = true,
                'h' => return Err(String::new()),
                'V' => {
                    println!("wbanish {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
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
                        let secs: f64 = value
                            .parse()
                            .ok()
                            .filter(|s: &f64| *s > 0.0 && s.is_finite())
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
    Ok(opts)
}

fn main() -> ExitCode {
    let opts = match parse_args(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("wbanish: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(if e.is_empty() { 0 } else { 1 });
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

fn run(opts: &Options) -> Result<(), Box<dyn Error>> {
    let mut hider = if gnome::is_session() {
        Hider::Gnome(Gnome::connect()?)
    } else {
        Hider::Overlay(Box::new(Overlay::connect()?))
    };
    let mut input = Input::open(opts.timeout.is_some(), opts.ignore)?;

    // SIGUSR1 pauses and resumes, e.g. for games.
    let (mut sig_rx, sig_tx) = UnixStream::pair()?;
    sig_rx.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGUSR1, sig_tx)?;
    let mut paused = false;

    let mut last_pointer = Instant::now();
    loop {
        // Dispatch whatever Wayland has queued; pointer events on the
        // overlay may show the cursor.
        let guard = match &mut hider {
            Hider::Overlay(wl) => Some(loop {
                wl.dispatch_pending()?;
                if wl.take_pointer_activity() {
                    last_pointer = Instant::now();
                }
                wl.flush()?;
                if let Some(g) = wl.prepare_read() {
                    break g;
                }
            }),
            Hider::Gnome(_) => None,
        };

        let mut timeout = None;
        if let Some(t) = opts.timeout
            && !paused
            && !hider.is_hidden()
        {
            let idle = last_pointer.elapsed();
            if idle >= t {
                debug!("no mouse activity for {:.1}s", idle.as_secs_f64());
                hider.hide();
                drop(guard);
                continue;
            }
            timeout = Some(Timespec::try_from(t - idle)?);
        }

        // Fixed slots first, then one per input device.
        const SIGNAL: usize = 0;
        const INOTIFY: usize = 1;
        const WAYLAND: usize = 2;
        let devices = WAYLAND + usize::from(guard.is_some());
        let revents: Vec<PollFlags> = {
            let mut fds = vec![
                PollFd::new(&sig_rx, PollFlags::IN),
                PollFd::from_borrowed_fd(input.inotify_fd(), PollFlags::IN),
            ];
            if let Some(g) = &guard {
                fds.push(PollFd::from_borrowed_fd(g.connection_fd(), PollFlags::IN));
            }
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

        if let Some(g) = guard {
            if revents[WAYLAND].is_empty() {
                drop(g);
            } else {
                g.read()?;
            }
        }

        if !revents[SIGNAL].is_empty() {
            let mut buf = [0u8; 64];
            while matches!(sig_rx.read(&mut buf), Ok(n) if n > 0) {}
            paused = !paused;
            eprintln!("wbanish: {}", if paused { "paused" } else { "resumed" });
            if paused {
                hider.show();
            }
            last_pointer = Instant::now();
        }

        // Read devices back to front so removing one keeps the others' indices.
        for i in (0..input.len()).rev() {
            let flags = revents[devices + i];
            if flags.is_empty() {
                continue;
            }
            let alive = input.read(i, &mut |a| match a {
                Activity::Key if !paused => hider.hide(),
                Activity::Key => {}
                Activity::Pointer => {
                    last_pointer = Instant::now();
                    hider.pointer_moved();
                }
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
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn args() {
        assert_eq!(parse(&[]), Ok(Options::default()));
        let o = parse(&["-d", "-i", "shift", "-ictrl", "-t", "2.5"]).unwrap();
        assert!(o.debug);
        assert_eq!(o.ignore, Mods::SHIFT | Mods::CONTROL);
        assert_eq!(o.timeout, Some(Duration::from_millis(2500)));
        assert_eq!(parse(&["-dt3"]).unwrap().timeout, Some(Duration::from_secs(3)));
        assert!(parse(&["-i", "mod3"]).is_err());
        assert!(parse(&["-t", "0"]).is_err());
        assert!(parse(&["-t"]).is_err());
        assert!(parse(&["-m", "nw"]).is_err());
        assert!(parse(&["foo"]).is_err());
    }
}
