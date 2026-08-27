# hyprloom

Save and restore [Hyprland](https://hyprland.org) window sessions.

When you reboot or after a power loss, hyprloom restores your applications to their correct workspaces and positions.

## Features

- **Save** current session — captures all windows, positions, workspaces, and monitor layout
- **Restore** saved session — relaunches apps and positions them precisely
- **Reconcile** saved session — reuses open windows, repairs only mismatches, and leaves extras alone
- **Replace** current desktop — saves a safety autosnapshot, closes current windows, restores one loaded target, and attempts recovery if it fails
- **Monitor-aware geometry** — adapts captured positions and sizes when a known monitor moves or changes resolution
- **Kitty terminal support** — restores working directory + shows hint of last command
- **Ghostty-aware restore** — recognizes the Linux app class and restores its working directory when configured
- **Smart filtering** — ignores transient windows (Waybar, Wofi, popups)
- **Dry run** — preview restore without executing
- **Brave profile support** — restores each profile to its configured workspace
- **Autosave** — periodic saves with automatic rotation via systemd timer
- **Configurable** — TOML config with per-app settings

## Installation

### From source

```bash
cargo install --locked --path .
```

### Arch Linux (AUR)

```bash
# Using your preferred AUR helper
yay -S hyprloom
```

The package installs `hyprflow` as a compatibility symlink, so existing
shortcuts and autosave references keep working while the fork uses the
`hyprloom` name.

The release workflow publishes the verified AUR metadata after a `v*` tag is
published from the fork. This checkout includes the matching `v0.3.0` package
checksum and a local tag for testing; the GitHub/AUR publication step still
requires the fork and tag to be pushed publicly.

## Usage

```bash
# Save current session
hyprloom save              # saves as "latest"
hyprloom save work         # saves as "work"

# Restore a session
hyprloom restore           # restores "latest"
hyprloom restore work      # restores "work"
hyprloom restore --dry-run # preview without executing
hyprloom restore work --reconcile # repair/reuse open windows; launch only missing ones
hyprloom replace work      # safety backup, close current windows, then restore work (with recovery on failure)
hyprloom recover           # recover an interrupted replacement without starting a restore
hyprloom restore --max-age 24h  # skip if session older than 24h
hyprloom restore --on-login     # print exec-once line for hyprland.conf

# Manage sessions
hyprloom list              # list all saved sessions
hyprloom delete work       # delete a session

# Show config
hyprloom config

# Autosave
hyprloom autosave              # check timer status
hyprloom autosave --now        # save now + rotate old
hyprloom autosave --install    # set up systemd timer
hyprloom autosave --uninstall  # remove systemd timer
```

## Configuration

Config file: `~/.config/hyprloom/config.toml`

```toml
[general]
default_session = "latest"
restore_delay_ms = 500
window_detect_timeout_ms = 5000
autosave_retain = 5

[filters]
ignore_classes = ["waybar", "wofi", "mako", "polkit", "nm-applet", "xdg-desktop-portal"]

[apps.kitty]
binary = "kitty"
capture_cwd = true
capture_last_command = true
hint_template = "# Last: {last_command}\n# Dir: {cwd}"

[apps."com.mitchellh.ghostty"]
binary = "ghostty"
capture_cwd = true

[apps.brave-browser]
binary = "brave"
```

### Reconciliation

Reconciliation is the additive restore mode. For every saved target, Hyprloom
first tries to match an existing window using its initial app identity, title,
working directory, and saved geometry. Each current window can be used at most
once. It then:

- does nothing for targets already in place;
- moves or resizes matched targets that are in the wrong workspace, monitor, or position;
- adapts geometry to the saved monitor origin and dimensions when available;
- launches only targets that are genuinely missing;
- leaves extra windows open and reports them.

Run it repeatedly or use it during login; once the target set is satisfied, the
pass is idempotent. Use the normal `restore` mode for additive launch/repair
behavior, or `replace`/Deskloom's Replace action when you want a clean desktop
first. Replace validates every target before closing the current windows and
keeps a safety autosnapshot named with the `autosave-` prefix.
Replace closes every current Hyprland client, including clients excluded by
the capture filters; use reconciliation when extras should remain untouched.

### Brave Profile Support

Hyprloom captures and restores Brave browser profiles individually. Since Brave
runs all windows in a single process, profile names come from Brave's `Local
State` file and are matched to profiles observed in the current windows when
no explicit mapping is configured.

With `profile_workspaces`, only the mapped profiles are captured and restored.
Without that mapping, only profiles positively observed in currently open Brave
windows are saved and restored; closed profiles in Local State are not launched
unexpectedly. Use `hyprloom config` to see detected profiles and their mapping
status.

```toml
[apps.brave-browser]
binary = "brave"
default_workspace = 1
profile_workspaces = { "Default" = 1, "Profile 1" = 6, "Profile 2" = 7 }
```

On restore, one Brave window is launched per captured profile and moved to its
configured workspace (or `default_workspace` when no per-profile mapping
exists). Profiles not in `profile_workspaces` are skipped when a mapping is
present.

### Autosave

Hyprloom can automatically save sessions at regular intervals using a systemd
timer. Autosave sessions are named with timestamps plus a process/sequence
suffix (`autosave-20260309T143000000000-1234-0`) and automatically rotated,
keeping only the last N saves.

```bash
# Check autosave status
hyprloom autosave

# Run autosave manually (capture + rotate)
hyprloom autosave --now

# Install systemd timer (saves every 10 minutes)
hyprloom autosave --install

# Remove systemd timer
hyprloom autosave --uninstall
```

Configure retention in `config.toml`:

```toml
[general]
autosave_retain = 5   # keep last 5 autosave sessions (default)
```

If an older hyprflow autosave timer exists, `hyprloom autosave --install`
migrates it to the hyprloom unit and preserves whether it was enabled.

### Restore on Login

To automatically restore your session when Hyprland starts:

```bash
hyprloom restore --on-login
```

This prints an `exec-once` line to add to `~/.config/hypr/hyprland.conf`:

```
exec-once = hyprloom restore --reconcile --max-age 24h
```

The `--max-age` flag prevents restoring stale sessions. Accepted formats:
`30m` (minutes), `24h` (hours), `7d` (days).

Restore only launches commands captured from the app identity or explicitly
authorized with an `apps.<class>.binary` setting. This keeps automatic restore
from executing an arbitrary command inserted into a session file.

### Sessions storage

Sessions are stored as JSON files in `~/.local/share/hyprloom/sessions/`.
Existing sessions from `~/.local/share/hyprflow/sessions/` are copied there on
first use and are never removed.

## How it works

**Save:** Captures window state via `hyprctl clients -j`, reads terminal CWD from `/proc`, and serializes to JSON.

**Restore:** Launches apps sequentially, polls for new windows via address diff, then positions each window using `hyprctl dispatch` with exact pixel coordinates.

**Reconcile:** Captures the current client list, creates a deterministic
one-to-one assignment to saved targets, refreshes each matched address before
repairing it, and uses the normal launch-and-detect path for missing targets.
Unmatched current windows are never closed by reconciliation.

## Requirements

- Hyprland 0.54+
- Linux (uses `/proc` for terminal CWD detection)
- Rust toolchain (for building from source)

## Contributing

Issues and pull requests are welcome. Please open an issue before submitting a large change so we can discuss the approach.

## License

MIT — see [LICENSE](LICENSE)
