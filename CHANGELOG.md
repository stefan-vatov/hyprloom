# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.3.1] - 2026-08-27

### Fixed

- Safely correlate browser windows when Chromium or Brave reuses an existing process by requiring a focused, newly appeared window
- Skip legacy Brave windows without positive profile identity instead of moving a guessed profile
- Fail fast when a launched browser window cannot be correlated, leaving it untouched

## [0.3.0] - 2026-08-27

### Added

- Hyprloom fork identity and namespaced configuration, data, and systemd units
- `restore --reconcile` additive reconciliation mode with one-to-one matching
- Placement-only repair for existing windows, missing-window launches, and extra-window preservation
- Initial class/title capture and Ghostty binary/working-directory support
- Pinned/special-workspace fidelity and monitor-aware geometry adaptation
- Process-tree launch correlation, deterministic terminal discovery, and profile-aware matching
- Non-destructive migration of existing Hyprflow session files

### Fixed

- Reconciliation now uses global one-to-one matching, refreshes windows before repair, and exits safely when an address disappears
- Session writes are atomic and protected against symlinked session paths; autosave names remain unique under concurrent captures

## [0.2.1] - 2026-03-11

### Fixed

- Redirect spawned process stdout/stderr to /dev/null during restore, preventing noisy child process output from flooding the terminal

---

## [0.2.0] - 2026-03-10

### Added

- CI/CD pipeline: GitHub Actions for tests/lint + auto-publish to AUR on version tag
- Brave browser profile support: capture active profiles from `Local State`, restore one window per profile with `--profile-directory` flag
- Configurable workspace mapping per Brave profile via `profile_workspaces` in config.toml
- `hyprloom config` now displays detected Brave profiles with mapping status
- Count-based duplicate detection on restore: skips already-running windows, restores only missing count
- Autosave with rotation: `hyprloom autosave --now` captures and keeps last N sessions (configurable via `autosave_retain`)
- Systemd timer management: `hyprloom autosave --install` / `--uninstall` for automated periodic saves
- `--max-age` flag for restore: skip restore if session is older than specified duration (e.g., `24h`, `7d`)
- `--on-login` flag for restore: prints `exec-once` line for Hyprland config

### Fixed

- Filter plain shell hints (`/bin/zsh`, `bash`, `fish`, `sh`) from last-command detection — idle terminals no longer show noisy hints
- Monitor mapping now uses monitor ID instead of array index, fixing incorrect monitor assignment
- Race condition in Brave profile restore: snapshot addresses before spawning (not after)

---

## [0.1.0] - 2026-03-08

Initial release.

### Added

- `hyprloom save [name]` — save current Hyprland session (defaults to "latest")
- `hyprloom restore [name]` — restore saved session with sequential launch and exact pixel positioning
- `hyprloom list` — list all saved sessions with metadata
- `hyprloom delete <name>` — delete a named session
- `hyprloom config` — print current configuration
- `--dry-run` flag for restore preview without executing
- `--verbose` flag for detailed output during save and restore
- Kitty terminal support: restore working directory and show last command hint
- Configurable ignore list for transient windows (Waybar, Wofi, Mako, etc.)
- TOML configuration at `~/.config/hyprloom/config.toml`
- Sessions stored as JSON at `~/.local/share/hyprloom/sessions/`
- Trait-based abstraction (`HyprctlClient`, `ProcessInfoProvider`) for full unit testability
- AUR PKGBUILD for Arch Linux

### Fixed

- Skip kitten `__atexit__` helper process when capturing Kitty CWD to avoid reading the wrong working directory
- Derive `Default` for `Config` instead of manual implementation (clippy compliance)
