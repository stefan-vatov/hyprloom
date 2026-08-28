//! Library components for capturing, restoring, and reconciling Hyprland sessions.

// Integration-test-only dependencies are passed to the library test target by
// Cargo, even though they are consumed by `tests/` rather than this crate.
// Keep the package-wide lint strict for production code while avoiding a false
// positive on that target.
#![cfg_attr(test, allow(unused_crate_dependencies))]

// The package's binary uses clap, while this library target also enables the
// package-wide `unused_crate_dependencies` lint.
use clap as _;

/// Install and manage the autosave systemd units.
pub mod autosave;
/// Read and filter Chromium/Brave browser profiles.
pub mod brave;
/// Capture the current Hyprland desktop into a session snapshot.
pub mod capture;
/// Load and validate Hyprloom configuration.
pub mod config;
/// Query Hyprland and dispatch compositor commands.
pub mod hyprctl;
/// Choose one-to-one assignments between saved and current windows.
pub mod matching;
/// Write user-facing warning messages without relying on print macros.
pub mod output;
mod placement;
mod platform;
/// Inspect process trees used for terminal and browser identity.
pub mod process;
/// Restore sessions and reconcile existing windows with saved placement.
pub mod restore;
/// Persist sessions and replacement transaction state.
pub mod session;
