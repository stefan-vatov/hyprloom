//! Fake systemctl boundary fixture (gqg.36.4).
//!
//! A PATH-injected `systemctl` records every invocation as an ordered argv
//! array in a case-local log and answers with scripted results: unit state
//! queries (`is-active`/`is-enabled`) exit with a scripted state code, and a
//! named subcommand can be scripted to fail with bounded stderr. This keeps
//! autosave lifecycle tests deterministic and guarantees zero contact with
//! the real user manager.

#![allow(dead_code, clippy::missing_panics_doc, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use crate::support::Case;

/// Case-relative directory holding the systemd fixture's log.
pub const SYSTEMD_DIR: &str = "artifacts/systemd";
const LOG_FILE: &str = "log";

/// The PATH-injected fake systemctl. Verbs are the first non-flag argument;
/// state verbs exit with `$FAKE_SYSTEMCTL_STATE` (default: inactive), and a
/// verb named by `$FAKE_SYSTEMCTL_FAIL` exits 1 with bounded stderr.
const SYSTEMCTL_SH: &str = r#"#!/bin/sh
set -u
log=${FAKE_SYSTEMCTL_LOG:?fake systemctl requires FAKE_SYSTEMCTL_LOG}
{
  printf 'ARGV %s\n' "$#"
  for arg do
    printf '%s\n' "$arg"
  done
} >>"$log"
verb=""
for arg do
  case "$arg" in
    -*) ;;
    *)
      if [ -z "$verb" ]; then verb=$arg; fi
      ;;
  esac
done
if [ -n "${FAKE_SYSTEMCTL_FAIL:-}" ] && [ "$verb" = "$FAKE_SYSTEMCTL_FAIL" ]; then
  printf 'fake systemctl: %s refused\n' "$verb" >&2
  exit 1
fi
case "$verb" in
  is-active | is-enabled) exit "${FAKE_SYSTEMCTL_STATE:-1}" ;;
  *) exit 0 ;;
esac
"#;

/// Install the fake systemctl and wire its case environment entries.
pub fn install_systemctl(case: &mut Case) {
    let root = case.root().to_path_buf();
    let dir = root.join(SYSTEMD_DIR);
    fs::create_dir_all(&dir).unwrap();
    case.install_fake("systemctl", SYSTEMCTL_SH);
    let log = dir.join(LOG_FILE).display().to_string();
    case.set_env("FAKE_SYSTEMCTL_LOG", &log);
}

/// Script the named subcommand to fail with a bounded stderr line.
pub fn script_failure(case: &mut Case, verb: &str) {
    case.set_env("FAKE_SYSTEMCTL_FAIL", verb);
}

/// Script the unit state answers: 0 active/enabled, 1 inactive/disabled.
pub fn script_state(case: &mut Case, state_code: u8) {
    case.set_env("FAKE_SYSTEMCTL_STATE", &state_code.to_string());
}

/// One recorded systemctl invocation with its exact argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemctlCall {
    /// The first non-flag argument (the verb).
    pub verb: String,
    /// The full argv, never flattened.
    pub argv: Vec<String>,
}

/// Parse the ordered invocation log.
pub fn read_systemctl_log(case: &Case) -> Vec<SystemctlCall> {
    parse_log(&fs::read_to_string(log_path(case)).unwrap_or_default())
}

/// The exact log location inside the case root.
pub fn log_path(case: &Case) -> PathBuf {
    case.root().join(SYSTEMD_DIR).join(LOG_FILE)
}

fn parse_log(log: &str) -> Vec<SystemctlCall> {
    let mut calls = Vec::new();
    let mut lines = log.lines();
    while let Some(line) = lines.next() {
        let Ok(argc) = line.strip_prefix("ARGV ").unwrap_or("").parse::<usize>() else {
            continue;
        };
        let tokens: Vec<String> = lines.by_ref().take(argc).map(str::to_owned).collect();
        let verb = tokens.iter().find(|argument| !argument.starts_with('-')).cloned().unwrap_or_default();
        calls.push(SystemctlCall { verb, argv: tokens });
    }
    calls
}

/// Seed a legacy autosave unit pair so migration scenarios can run.
pub fn seed_legacy_units(case: &Case) -> (PathBuf, PathBuf) {
    let units = case.root().join("config/systemd/user");
    fs::create_dir_all(&units).unwrap();
    let service = units.join("hyprloom-autosave.service");
    let timer = units.join("hyprloom-autosave.timer");
    fs::write(&service, "[Unit]\nDescription=legacy\n").unwrap();
    fs::write(&timer, "[Unit]\nDescription=legacy timer\n").unwrap();
    (service, timer)
}

/// Whether the path is a regular file with private permissions (0600).
pub fn is_private_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o777 == 0o600)
}
