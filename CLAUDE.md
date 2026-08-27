# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build --release              # Release binary (~2 MB)
cargo test                         # All unit and CLI tests
cargo test --lib                   # Library tests only
cargo test --test cli_test         # CLI integration tests only
cargo test <test_name>             # Single test by name
cargo clippy --all-targets         # Lint (known warnings: 5x assert_cmd::cargo_bin deprecated, 1x too-many-arguments)
cargo install --path .             # Install to ~/.cargo/bin/
```

## Architecture

Hyprloom is a Rust CLI that captures and restores Hyprland window sessions via `hyprctl` IPC.

### Core Modules

- **main.rs** — clap-derived CLI with subcommands: `save`, `restore`, `list`, `delete`, `config`
- **capture.rs** — Queries `hyprctl clients/monitors`, resolves CWD/last-command per window, builds `Session`
- **restore.rs** — Plans one-to-one reconciliation, refreshes matched addresses, spawns missing binaries sequentially, polls for new window identity, and positions via `hyprctl dispatch`
- **session.rs** — `Session`/`SessionClient`/`LaunchInfo` data model, JSON file I/O under `$XDG_DATA_HOME/hyprloom/sessions/`
- **config.rs** — TOML config at `$XDG_CONFIG_HOME/hyprloom/config.toml`, per-app capture settings, ignore-class filters
- **hyprctl.rs** — `HyprctlClient` trait + `RealHyprctl` (shells out to `hyprctl`) + `MockHyprctl` for tests
- **process.rs** — `ProcessInfoProvider` trait + `RealProcessInfo` (reads `/proc`) + `MockProcessInfo` for tests
- **brave.rs** — Reads Brave `Local State` JSON, extracts profile info, filters by config

### Key Design Pattern

Trait-based dependency injection (`HyprctlClient`, `ProcessInfoProvider`) enables full unit testing without a running Hyprland session. Mocks record dispatches and return fixture data.

### Restore Flow Detail

Each window is restored sequentially: spawn → poll for new address (100ms intervals, configurable timeout) → position via hyprctl dispatch commands. The address-diff approach detects which new window belongs to which spawn.

Reconciliation captures current clients, matches targets one-to-one using
initial identity, title, working-directory, profile, and geometry evidence,
refreshes each matched address before repair, and launches only unmatched
targets. Extra current windows are preserved. Saved monitor origins and sizes
are used to adapt geometry when the same monitor changes layout.

### Test Fixtures

`tests/fixtures/` contains real `hyprctl` JSON output (3 windows on 2 monitors) used by both unit and integration tests.

## Active Development

Branch `main` contains the Hyprloom fork and its reconciliation implementation.
