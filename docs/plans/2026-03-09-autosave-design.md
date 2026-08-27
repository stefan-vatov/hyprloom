# Autosave with Rotation — Design

**Date:** 2026-03-09
**Branch:** feat/v0.2

## Overview

New subcommand `hyprloom autosave` that captures sessions with automatic rotation,
keeping only the last N autosave sessions. Manual sessions are never affected.

## Subcommand: `hyprloom autosave`

### Flags

| Flag          | Behavior                                          |
|---------------|---------------------------------------------------|
| *(no flag)*   | Check systemd timer status and guide user         |
| `--now`       | Capture + save + rotate (immediate execution)     |
| `--install`   | Generate systemd unit files, guide enable/start   |
| `--uninstall` | Stop timer, remove systemd unit files             |

### `--now` behavior

1. Capture session (same flow as `hyprloom save`)
2. Save with name `autosave-{timestamp}` (format: `autosave-20260309T143000`)
3. List sessions with prefix `autosave-`, sort by date
4. Remove oldest, keeping only last N (default 5, configurable)
5. Print: `Autosaved (5 windows). Retained 5/7, pruned 2.`

### No-flag behavior (status check)

**Timer not installed:**
```
Autosave is not configured.
Run 'hyprloom autosave --install' to set up the systemd timer.
```

**Installed but not active:**
```
Autosave timer is installed but not active.
  systemctl --user enable --now hyprloom-autosave.timer
```

**Active and running:**
```
Autosave is active (every 10min).
Last run: 2026-03-09 14:30:00 — 5 windows saved.
Retained: 5 sessions (oldest: 2026-03-09T133000)
To disable: hyprloom autosave --uninstall
```

### `--install` behavior

Generates two files in `~/.config/systemd/user/`:
- `hyprloom-autosave.service` — `ExecStart=hyprloom autosave --now`
- `hyprloom-autosave.timer` — `OnUnitActiveSec=10min`

Does NOT activate. Prints:
```
Created:
  ~/.config/systemd/user/hyprloom-autosave.service
  ~/.config/systemd/user/hyprloom-autosave.timer

To enable and start:
  systemctl --user enable --now hyprloom-autosave.timer

To check status:
  systemctl --user status hyprloom-autosave.timer
  journalctl --user -u hyprloom-autosave.service
```

### `--uninstall` behavior

1. `systemctl --user disable --now hyprloom-autosave.timer`
2. Remove both `.service` and `.timer` files
3. Print confirmation

## Config

```toml
[general]
autosave_retain = 5   # default: 5, number of autosave sessions to keep
```

## Naming Convention

- Autosave sessions: `autosave-20260309T143000` (ISO 8601 compact, no special chars)
- Manual sessions: any user-chosen name (unchanged)
- Rotation only touches sessions with `autosave-` prefix

## Scope Exclusions

- No daemon/event loop in the binary — systemd timer handles scheduling
- Manual sessions (`hyprloom save work`) are never pruned by rotation
- Prefix `autosave-` is not configurable
