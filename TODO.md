# TODO

## v0.2

- [x] Fix: filter `/bin/zsh` and other bare shells from last-command hint (show nothing for idle shells)
- [x] Fix: correct monitor index to name mapping when hyprctl monitor order differs from client monitor index
- [x] Fix: count-based duplicate detection on restore (skip already-running windows)
- [x] Brave browser profile support (capture from Local State, restore with `--profile-directory`, configurable workspace mapping)
- [x] Autosave with rotation via systemd timer
- [x] Restore on login via `exec-once` in Hyprland config (`--on-login`, `--max-age`)
- [x] CI/CD pipeline: auto-publish to AUR on new release tag on main

## v0.3

- [ ] Autostart apps on restore: config-driven launch of apps that should always run, even if not in the saved session (e.g., `autostart = true` + `default_workspace = 2` for Obsidian)
- [ ] Desktop notifications on save/autosave (notify-send with next autosave time)
- [ ] Custom hooks per app (pre-save, post-restore shell commands)
- [ ] Dwindle layout tree preservation (split ratios)
- [x] Graceful fallback when monitor configuration changes between save and restore

## Future

- [ ] Plugin system for app-specific state capture
- [ ] Partial restore (single workspace)
- [ ] Layout message integration for split reconstruction
