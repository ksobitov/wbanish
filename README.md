# wbanish

wbanish hides the mouse cursor when you start typing, and shows it again when
the mouse moves, a button is pressed or you scroll.
It is [xbanish](https://github.com/jcs/xbanish) for Wayland.

Some compositors can already do this on their own, and if yours can, use that:

| Compositor | Setting |
| --- | --- |
| Sway | `seat * hide_cursor when-typing enable` |
| Hyprland | `cursor { hide_on_key_press = true }` |
| niri | `cursor { hide-when-typing }` |
| KDE Plasma 6.2+ | the *Hide Cursor* desktop effect |

wbanish is for the others, such as river, labwc, Wayfire, COSMIC and older
Plasma.
It needs `wlr-layer-shell`, so it does not work on GNOME or Weston.

## Usage

```sh
wbanish [-d] [-i modifier]... [-t seconds]
```

- `-d` prints debugging messages to stderr.
- `-i modifier` doesn't hide the cursor for keys pressed while `modifier` is
  held: `shift`, `control` (`ctrl`), `alt` (`mod1`), `super` (`mod4`),
  `altgr` (`mod5`), `capslock` (`lock`), `numlock` (`mod2`), or `all` for
  every held modifier but not the locks. Repeat it to ignore several.
- `-t seconds` also hides the cursor after this many seconds without mouse
  activity.

Sending `SIGUSR1` pauses wbanish and sending it again resumes it, which is
handy for games:

```sh
pkill -USR1 wbanish
```

Pressing a modifier key alone never hides the cursor, so Ctrl+click and
Super+drag keep working.

## Installation

On Debian 12+ and Ubuntu 22.04+, add the APT repository once:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
sudo curl -fsSLo /etc/apt/keyrings/wbanish.asc https://ksobitov.github.io/wbanish/wbanish.asc
sudo tee /etc/apt/sources.list.d/wbanish.sources <<EOF
Types: deb
URIs: https://ksobitov.github.io/wbanish
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/wbanish.asc
EOF
sudo apt update
sudo apt install wbanish
```

Updates then come with `apt upgrade`.
The packages are also attached to each
[GitHub release](https://github.com/ksobitov/wbanish/releases).

Elsewhere, build it with Cargo:

```sh
cargo install --git https://github.com/ksobitov/wbanish
```

wbanish reads keyboards from `/dev/input/event*`, so you have to be in the
`input` group:

```sh
sudo usermod -aG input $USER
```

Then log out and back in.
Note that any program in that group can read every key you type; wbanish
itself only looks at keys to track modifiers and never stores or prints them.

Start it from your compositor's autostart, for example in labwc's
`~/.config/labwc/autostart`:

```sh
wbanish &
```

## How it works

Wayland has no way for a client to hide the global cursor, which is the main
reason xbanish couldn't just be ported.
A client can only set the cursor for its own surfaces, so when a key is
pressed wbanish puts a transparent, full-screen surface on the topmost
layer of every output and sets an empty cursor when the pointer enters it.
Keyboard focus stays with the window you are typing in.

The first motion, button press or scroll on that surface removes it.
Before removing it, wbanish sets the default cursor shape
(`wp-cursor-shape-v1`), so the cursor reappears at once even on compositors
that only give the pointer back to the window underneath on the next motion.

These pointer events come from the compositor after libinput, so touchpad
palm rejection and disable-while-typing still apply: a palm resting on the
touchpad won't bring the cursor back.

Keyboards are read from evdev because a Wayland client can't see keys typed
into other clients.
Mice and touchpads are only opened with `-t`, to know when they were last
used.

## Differences from xbanish

- A click or scroll that brings the cursor back is not passed on to the
  window underneath, because it lands on wbanish's overlay. This rarely
  matters because you usually move the mouse to find the cursor before
  clicking.
- While the cursor is hidden, the window underneath gets a pointer leave
  event, so hover effects and tooltips go away.
- xbanish's `-a` (always hide) would block the mouse completely, `-m` needs
  pointer warping that Wayland doesn't allow, and `-s` would swallow the
  scroll, so wbanish doesn't have them.
- Games that lock the pointer lose that lock while the overlay is up; pause
  wbanish with `SIGUSR1` while playing.
- Modifiers are tracked by key code, not through the keymap, so right Alt
  counts as both `alt` and `altgr`.
