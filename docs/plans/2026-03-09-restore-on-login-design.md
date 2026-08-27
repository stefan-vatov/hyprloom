# Restore on Login — Design

**Date:** 2026-03-09
**Branch:** feat/v0.2

## Overview

Add `--max-age` flag to `hyprloom restore` and `--on-login` flag that prints the
`exec-once` line for the user to paste into their Hyprland config.

## Flag: `--max-age <duration>`

Added to the existing `restore` subcommand.

- Accepts duration format: `30m`, `1h`, `24h`, `7d`
- Compares session's `created_at` against current time
- If session is older than the limit, skip restore and print warning
- If `--max-age` is not provided, restore always (current behavior unchanged)
- Works with any session, not just `latest`

### Behavior when session is too old

```
Session 'latest' is 3 days old (created 2026-03-06 14:30).
Skipping restore (max age: 24h).
```

Exit code: 0 (not an error — intentional skip)

## Flag: `--on-login`

Added to the existing `restore` subcommand.

- Prints the `exec-once` line and instructions, then exits
- Does NOT perform a restore

### Output

```
Add this line to ~/.config/hypr/hyprland.conf:

  exec-once = hyprloom restore --max-age 24h

This will restore your last saved session on login.
Sessions older than 24h will be skipped.
```

## Scope Exclusions

- Does NOT edit hyprland.conf automatically
- Does NOT delete sessions after restore
- Notifications on save/autosave → separate TODO v0.3 item
