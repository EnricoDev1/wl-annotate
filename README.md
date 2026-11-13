# wl-annotate

Draw on your screen in Wayland. Press a key, the screen freezes, draw with the mouse, press `q` to finish.

When toggled, `wl-annotate` captures a screencopy snapshot of every output and displays it as a fullscreen layer-shell overlay. You draw on that overlay, not in a window containing a screenshot.

The project ships two binaries:

| Binary | Role |
|---|---|
| `wl-annotate` | Long-lived Wayland client (the daemon). Owns the overlays while you draw and must be running before you press the keybind. Listens on a Unix socket. |
| `wl-annotatectl` | Small socket client, meant to be spawned by compositor keybinds. |

## Requirements

- A compositor that supports `zwlr_layer_shell_v1` and `zwlr_screencopy_manager_v1`, such as niri, Sway, Hyprland, labwc, Wayfire or river. GNOME and KDE are not supported.
- No special permissions: no `input` group, udev rules or portals.

## Installation

From a local checkout:

```sh
cargo install --path .
```

From the Git repository:

```sh
cargo install --git https://github.com/EnricoDev1/wl-annotate
```

From crates.io (once published):

```sh
cargo install wl-annotate
```

Both binaries are installed to `~/.cargo/bin`.

## Setup

Setup has two parts: start the daemon with your session, and bind a compositor key to `wl-annotatectl toggle`.

### 1. Start the daemon

**Option A: systemd user service (recommended)**

```sh
cp dist/wl-annotate.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now wl-annotate.service
```

The service starts with your session, restarts on crash and stops at logout. It requires a compositor session integrated with systemd (for example niri or sway). `ExecStart` assumes the binary is at `~/.cargo/bin/wl-annotate`; edit the unit file if you installed it elsewhere.

View logs with:

```sh
journalctl --user -f -u wl-annotate.service
```

**Option B: compositor autostart**

This is not supervised, so a crash will not restart the daemon.

niri (`~/.config/niri/config.kdl`):

```kdl
spawn-at-startup "wl-annotate"
```

sway / i3:

```
exec wl-annotate
```

Hyprland:

```
exec-once = wl-annotate
```

Using both options together is safe: a newly started daemon retires the previous one.

### 2. Bind a key

niri:

```kdl
Mod+D { spawn "wl-annotatectl" "toggle"; }
```

sway / i3:

```
bindsym $mod+d exec wl-annotatectl toggle
```

Hyprland:

```
bind = SUPER, D, exec, wl-annotatectl toggle
```

### Other control methods

- `wl-annotatectl on | off | clear | save | status | quit`
- `kill -USR1 <pid>` toggles drawing; `kill -USR2 <pid>` clears the canvas.
- **One-shot mode:** bind `wl-annotate --once` directly to run without a daemon. Each activation pays the startup cost of one screencopy.

## Drawing controls

| Input | Action |
|---|---|
| Left drag | Draw |
| Mouse wheel | Change pen size (the cursor ring updates live) |
| `r` `b` `g` `m` `w` | Pen color: red, blue, green, magenta, white |
| `c` or right click | Clear annotations |
| `s` | Save PNG snapshot(s) to `save_dir` |
| `q` or `Esc` | Stop drawing and restore the screen |

The cursor is shown as a ring matching the size of the pen.

## Configuration

Settings live in `~/.config/wl-annotate/config`. The file is optional.

```ini
color    = #ff0000                  # initial pen color: #rgb, #rrggbb or #rrggbbaa
pen_size = 4                        # in logical pixels
socket   = ~/run/wl-annotate.sock
save_dir = ~/Pictures
```

Command-line flags override the config file:

```sh
wl-annotate --color '#00ff00' --pen 6 --once -v
```

## Troubleshooting

Run with `-v` for live diagnostics.

**`wl-annotatectl: cannot connect`**
The daemon is not running. See [Start the daemon](#1-start-the-daemon).

**`q` does nothing**
Either the key is bound in your compositor config (compositor binds take priority over the focused overlay), or more than one daemon is running. Check with `pgrep -af wl-annotate`, then kill the extras.

**Startup reports a protocol as `MISSING`**
Your compositor lacks wlr layer-shell or screencopy support (this is the case on GNOME and KDE).

## Development

```sh
cargo test
cargo build --release
```

The `examples/` directory contains development-only input injectors (`inject`, `pointer-inject`, `dump_globals`) used for end-to-end testing against a live compositor. They are not installed with the crate.

## License

MIT. See [LICENSE](LICENSE).
