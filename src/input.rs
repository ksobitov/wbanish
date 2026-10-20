//! Keyboard (and, for the idle timeout, pointer) activity read from evdev.
//!
//! Wayland doesn't let a client see keys typed into other clients, so we read
//! /dev/input/event* directly, which needs membership in the `input` group.
//! Key codes are only inspected to track modifiers; they are never stored or
//! printed.

use std::fs::OpenOptions;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use evdev::raw_stream::RawDevice;
use evdev::{AbsoluteAxisCode, EventType, InputEvent, KeyCode, LedCode, RelativeAxisCode};
use rustix::fs::OFlags;
use rustix::fs::inotify;

const INPUT_DIR: &str = "/dev/input";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods(u8);

impl Mods {
    pub const SHIFT: Mods = Mods(1 << 0);
    pub const CONTROL: Mods = Mods(1 << 1);
    pub const ALT: Mods = Mods(1 << 2);
    pub const SUPER: Mods = Mods(1 << 3);
    pub const ALTGR: Mods = Mods(1 << 4);
    pub const CAPSLOCK: Mods = Mods(1 << 5);
    pub const NUMLOCK: Mods = Mods(1 << 6);
    /// Every modifier that is held down, as opposed to the locks.
    pub const HELD: Mods = Mods(0b1_1111);
    const LOCKS: Mods = Mods(Self::CAPSLOCK.0 | Self::NUMLOCK.0);

    pub fn parse(name: &str) -> Option<Mods> {
        Some(match name.to_ascii_lowercase().as_str() {
            "shift" => Self::SHIFT,
            "control" | "ctrl" => Self::CONTROL,
            "alt" | "mod1" => Self::ALT,
            "super" | "logo" | "mod4" => Self::SUPER,
            "altgr" | "mod5" => Self::ALTGR,
            "capslock" | "lock" => Self::CAPSLOCK,
            "numlock" | "mod2" => Self::NUMLOCK,
            "all" => Self::HELD,
            _ => return None,
        })
    }

    pub fn intersects(self, other: Mods) -> bool {
        self.0 & other.0 != 0
    }

    /// Comma-separated names, as the GNOME Shell extension takes them, or
    /// "-" for none.
    pub fn names(self) -> String {
        let names: Vec<&str> = [
            (Self::SHIFT, "shift"),
            (Self::CONTROL, "control"),
            (Self::ALT, "alt"),
            (Self::SUPER, "super"),
            (Self::ALTGR, "altgr"),
            (Self::CAPSLOCK, "capslock"),
            (Self::NUMLOCK, "numlock"),
        ]
        .into_iter()
        .filter(|&(m, _)| self.intersects(m))
        .map(|(_, name)| name)
        .collect();
        if names.is_empty() { "-".to_owned() } else { names.join(",") }
    }
}

impl std::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, rhs: Mods) -> Mods {
        Mods(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Mods {
    fn bitor_assign(&mut self, rhs: Mods) {
        self.0 |= rhs.0;
    }
}

/// The modifier bits a key sets while held, or None if it isn't a modifier.
/// Without the keymap we can't know what right Alt does, so it counts as
/// both Alt and AltGr.
fn modifier(key: KeyCode) -> Option<Mods> {
    Some(match key {
        KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => Mods::SHIFT,
        KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => Mods::CONTROL,
        KeyCode::KEY_LEFTALT => Mods::ALT,
        KeyCode::KEY_RIGHTALT => Mods::ALT | Mods::ALTGR,
        KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => Mods::SUPER,
        KeyCode::KEY_CAPSLOCK | KeyCode::KEY_NUMLOCK => Mods::default(),
        _ => return None,
    })
}

fn is_keyboard_key(code: u16) -> bool {
    // Below BTN_MISC, or KEY_OK up to just before BTN_TRIGGER_HAPPY.
    code < 0x100 || (0x160..0x2c0).contains(&code)
}

fn is_pointer_button(code: u16) -> bool {
    // BTN_LEFT..BTN_TASK, and the BTN_TOOL_*/BTN_TOUCH/BTN_STYLUS range.
    (0x110..=0x117).contains(&code) || (0x140..=0x14f).contains(&code)
}

/// What a single evdev event means to us.
#[derive(Debug, PartialEq, Eq)]
enum Event {
    /// A modifier key went down (true) or up (false).
    Modifier(Mods, bool),
    /// A key press or autorepeat that may hide the cursor.
    Key,
    Pointer,
    /// The kernel dropped events, so held keys must be read again.
    Dropped,
}

fn classify(ty: EventType, code: u16, value: i32) -> Option<Event> {
    match ty {
        EventType::KEY => {
            if let Some(m) = modifier(KeyCode::new(code)) {
                Some(Event::Modifier(m, value != 0))
            } else if is_pointer_button(code) {
                (value == 1).then_some(Event::Pointer)
            } else {
                // value 0 is a release, 2 an autorepeat.
                (value != 0 && is_keyboard_key(code)).then_some(Event::Key)
            }
        }
        EventType::RELATIVE => {
            let moved = matches!(
                RelativeAxisCode(code),
                RelativeAxisCode::REL_X
                    | RelativeAxisCode::REL_Y
                    | RelativeAxisCode::REL_WHEEL
                    | RelativeAxisCode::REL_HWHEEL
            );
            (moved && value != 0).then_some(Event::Pointer)
        }
        EventType::ABSOLUTE => matches!(
            AbsoluteAxisCode(code),
            AbsoluteAxisCode::ABS_X
                | AbsoluteAxisCode::ABS_Y
                | AbsoluteAxisCode::ABS_MT_POSITION_X
                | AbsoluteAxisCode::ABS_MT_POSITION_Y
        )
        .then_some(Event::Pointer),
        EventType::SYNCHRONIZATION if code == SYN_DROPPED => Some(Event::Dropped),
        _ => None,
    }
}

const SYN_DROPPED: u16 = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum Activity {
    /// A key was pressed that should hide the cursor.
    Key,
    /// The mouse, touchpad or tablet was used.
    Pointer,
}

struct Device {
    path: PathBuf,
    dev: RawDevice,
    /// Modifiers currently held on this device.
    mods: Mods,
}

pub struct Input {
    devices: Vec<Device>,
    inotify: OwnedFd,
    want_pointers: bool,
    ignore: Mods,
}

impl Input {
    /// Opens all keyboards, and pointing devices too if `want_pointers`.
    pub fn open(want_pointers: bool, ignore: Mods) -> Result<Self, String> {
        let inotify = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
            .map_err(|e| format!("inotify: {e}"))?;
        inotify::add_watch(
            &inotify,
            INPUT_DIR,
            inotify::WatchFlags::CREATE | inotify::WatchFlags::ATTRIB,
        )
        .map_err(|e| format!("can't watch {INPUT_DIR}: {e}"))?;

        let mut input = Input { devices: Vec::new(), inotify, want_pointers, ignore };
        let entries = std::fs::read_dir(INPUT_DIR).map_err(|e| format!("{INPUT_DIR}: {e}"))?;
        let mut denied = 0;
        for entry in entries.flatten() {
            if entry.file_name().as_bytes().starts_with(b"event")
                && let Err(e) = input.add(&entry.path())
                && e.kind() == io::ErrorKind::PermissionDenied
            {
                denied += 1;
            }
        }

        if !input.devices.iter().any(|d| is_keyboard(&d.dev)) {
            if denied > 0 {
                return Err(format!(
                    "no permission to read {INPUT_DIR}/event*; add yourself to the \
                     'input' group (sudo usermod -aG input $USER) and log in again"
                ));
            }
            eprintln!("wbanish: no keyboard found, waiting for one to be plugged in");
        }
        if want_pointers && !input.devices.iter().any(|d| is_pointer(&d.dev)) {
            eprintln!("wbanish: no readable mouse or touchpad, -t will hide it while in use");
        }
        Ok(input)
    }

    fn add(&mut self, path: &Path) -> io::Result<()> {
        // A node that was created again belongs to a new device.
        self.devices.retain(|d| d.path != path);

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(OFlags::NONBLOCK.bits() as i32)
            .open(path)
            .inspect_err(|e| debug!("{}: {e}", path.display()))?;
        let dev = RawDevice::try_from(file)?;
        let name = dev.name().unwrap_or("?").to_owned();

        let keyboard = is_keyboard(&dev);
        if !keyboard && !(self.want_pointers && is_pointer(&dev)) {
            debug!("{}: ignoring {name}", path.display());
            return Ok(());
        }
        debug!(
            "{}: using {name} ({})",
            path.display(),
            if keyboard { "keyboard" } else { "pointer" }
        );
        let mut d = Device { path: path.to_owned(), dev, mods: Mods::default() };
        d.sync_mods();
        self.devices.push(d);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn device_fd(&self, i: usize) -> BorrowedFd<'_> {
        self.devices[i].dev.as_fd()
    }

    pub fn inotify_fd(&self) -> BorrowedFd<'_> {
        self.inotify.as_fd()
    }

    pub fn remove(&mut self, i: usize) {
        let d = self.devices.remove(i);
        debug!("{}: removed", d.path.display());
    }

    /// Reads everything available from device `i`.  Returns false if the
    /// device is gone and should be removed.
    pub fn read(&mut self, i: usize, activity: &mut impl FnMut(Activity)) -> bool {
        let mut events: Vec<InputEvent> = Vec::new();
        let d = &mut self.devices[i];
        loop {
            let err = match d.dev.fetch_events() {
                Ok(evs) => {
                    let before = events.len();
                    events.extend(evs);
                    if events.len() == before {
                        break;
                    }
                    continue;
                }
                Err(e) => e,
            };
            match err.kind() {
                io::ErrorKind::WouldBlock => break,
                io::ErrorKind::Interrupted => continue,
                _ => {
                    debug!("{}: {err}", d.path.display());
                    return false;
                }
            }
        }
        for ev in events {
            if let Some(a) = self.handle(i, ev) {
                activity(a);
            }
        }
        true
    }

    fn handle(&mut self, i: usize, ev: InputEvent) -> Option<Activity> {
        match classify(ev.event_type(), ev.code(), ev.value())? {
            Event::Modifier(m, down) => {
                let d = &mut self.devices[i];
                d.mods = if down { d.mods | m } else { Mods(d.mods.0 & !m.0) };
                // A bare modifier never hides: Ctrl+click and Super+drag
                // need the pointer.
                None
            }
            Event::Key => {
                let mut active = self.devices.iter().fold(Mods::default(), |a, d| a | d.mods);
                if self.ignore.intersects(Mods::LOCKS) {
                    active |= self.devices[i].locks();
                }
                if self.ignore.intersects(active) {
                    debug!("ignoring key pressed with an ignored modifier");
                    return None;
                }
                Some(Activity::Key)
            }
            Event::Pointer => Some(Activity::Pointer),
            Event::Dropped => {
                self.devices[i].sync_mods();
                None
            }
        }
    }

    /// Opens devices that appeared in /dev/input or just became readable.
    pub fn hotplug(&mut self) {
        let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
        let mut paths = Vec::new();
        let mut reader = inotify::Reader::new(&self.inotify, &mut buf);
        while let Ok(ev) = reader.next() {
            if ev.events().contains(inotify::ReadFlags::QUEUE_OVERFLOW) {
                // Events were lost; look at every device node again.
                paths.extend(
                    std::fs::read_dir(INPUT_DIR)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter(|e| e.file_name().as_bytes().starts_with(b"event"))
                        .map(|e| e.path())
                        .filter(|p| !self.devices.iter().any(|d| &d.path == p)),
                );
                continue;
            }
            let Some(name) = ev.file_name() else { continue };
            if !name.to_bytes().starts_with(b"event") {
                continue;
            }
            let path = Path::new(INPUT_DIR).join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            let created = ev.events().contains(inotify::ReadFlags::CREATE);
            if created || !self.devices.iter().any(|d| d.path == path) {
                paths.push(path);
            }
        }
        paths.sort();
        paths.dedup();
        for path in paths {
            // Fails with EACCES until udev has fixed the permissions; we
            // get another ATTRIB event when it does.
            let _ = self.add(&path);
        }
    }
}

impl Device {
    fn sync_mods(&mut self) {
        let Ok(keys) = self.dev.get_key_state() else {
            return;
        };
        self.mods = keys.iter().filter_map(modifier).fold(Mods::default(), |a, m| a | m);
    }

    /// CapsLock and NumLock, as shown by the keyboard's LEDs.
    fn locks(&self) -> Mods {
        let Ok(leds) = self.dev.get_led_state() else {
            return Mods::default();
        };
        let mut m = Mods::default();
        if leds.contains(LedCode::LED_CAPSL) {
            m |= Mods::CAPSLOCK;
        }
        if leds.contains(LedCode::LED_NUML) {
            m |= Mods::NUMLOCK;
        }
        m
    }
}

fn is_keyboard(dev: &RawDevice) -> bool {
    if is_joystick(dev) {
        return false;
    }
    dev.supported_keys().is_some_and(|keys| keys.iter().any(|k| is_keyboard_key(k.code())))
}

fn is_pointer(dev: &RawDevice) -> bool {
    if is_joystick(dev) {
        return false;
    }
    let keys = dev.supported_keys();
    let has_key = |k| keys.is_some_and(|keys| keys.contains(k));
    let rel = dev.supported_relative_axes().is_some_and(|r| r.contains(RelativeAxisCode::REL_X));
    let abs = dev.supported_absolute_axes().is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_X));
    rel || has_key(KeyCode::BTN_LEFT)
        || (abs && (has_key(KeyCode::BTN_TOUCH) || has_key(KeyCode::BTN_TOOL_PEN)))
}

fn is_joystick(dev: &RawDevice) -> bool {
    dev.supported_keys().is_some_and(|keys| {
        !keys.contains(KeyCode::BTN_LEFT)
            && (keys.contains(KeyCode::BTN_SOUTH) || keys.contains(KeyCode::BTN_TRIGGER))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mods() {
        assert_eq!(Mods::parse("Shift"), Some(Mods::SHIFT));
        assert_eq!(Mods::parse("ctrl"), Some(Mods::CONTROL));
        assert_eq!(Mods::parse("mod4"), Some(Mods::SUPER));
        assert_eq!(Mods::parse("all"), Some(Mods::HELD));
        assert!(!Mods::HELD.intersects(Mods::LOCKS));
        assert_eq!(Mods::parse("mod3"), None);
        assert_eq!(Mods::default().names(), "-");
        assert_eq!((Mods::SUPER | Mods::SHIFT).names(), "shift,super");
    }

    #[test]
    fn modifiers() {
        assert_eq!(modifier(KeyCode::KEY_RIGHTALT), Some(Mods::ALT | Mods::ALTGR));
        assert_eq!(modifier(KeyCode::KEY_CAPSLOCK), Some(Mods::default()));
        assert_eq!(modifier(KeyCode::KEY_A), None);
    }

    #[test]
    fn events() {
        let key = |code: KeyCode, value| classify(EventType::KEY, code.code(), value);
        assert_eq!(key(KeyCode::KEY_A, 1), Some(Event::Key));
        assert_eq!(key(KeyCode::KEY_A, 2), Some(Event::Key));
        assert_eq!(key(KeyCode::KEY_A, 0), None);
        assert_eq!(key(KeyCode::KEY_LEFTCTRL, 1), Some(Event::Modifier(Mods::CONTROL, true)));
        assert_eq!(key(KeyCode::KEY_LEFTCTRL, 0), Some(Event::Modifier(Mods::CONTROL, false)));
        assert_eq!(key(KeyCode::BTN_LEFT, 1), Some(Event::Pointer));
        assert_eq!(key(KeyCode::BTN_LEFT, 0), None);
        assert_eq!(key(KeyCode::BTN_SOUTH, 1), None);

        let rel = |code: RelativeAxisCode, value| classify(EventType::RELATIVE, code.0, value);
        assert_eq!(rel(RelativeAxisCode::REL_X, 3), Some(Event::Pointer));
        assert_eq!(rel(RelativeAxisCode::REL_WHEEL, -1), Some(Event::Pointer));
        assert_eq!(rel(RelativeAxisCode::REL_X, 0), None);
        assert_eq!(rel(RelativeAxisCode::REL_DIAL, 1), None);

        let abs = |code: AbsoluteAxisCode| classify(EventType::ABSOLUTE, code.0, 100);
        assert_eq!(abs(AbsoluteAxisCode::ABS_MT_POSITION_X), Some(Event::Pointer));
        assert_eq!(abs(AbsoluteAxisCode::ABS_PRESSURE), None);

        assert_eq!(classify(EventType::SYNCHRONIZATION, SYN_DROPPED, 0), Some(Event::Dropped));
        assert_eq!(classify(EventType::SYNCHRONIZATION, 0, 0), None);
        assert_eq!(classify(EventType::LED, 0, 1), None);
    }

    #[test]
    fn key_ranges() {
        assert!(is_keyboard_key(KeyCode::KEY_A.code()));
        assert!(is_keyboard_key(KeyCode::KEY_VOLUMEUP.code()));
        assert!(!is_keyboard_key(KeyCode::BTN_LEFT.code()));
        assert!(!is_keyboard_key(KeyCode::BTN_SOUTH.code()));
        assert!(is_pointer_button(KeyCode::BTN_LEFT.code()));
        assert!(is_pointer_button(KeyCode::BTN_TOUCH.code()));
        assert!(!is_pointer_button(KeyCode::BTN_SOUTH.code()));
    }
}
