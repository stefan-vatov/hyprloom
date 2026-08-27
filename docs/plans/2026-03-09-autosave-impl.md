# Autosave Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `hyprloom autosave` subcommand with session rotation, systemd timer install/uninstall, and status display.

**Architecture:** New `src/autosave.rs` module handles rotation logic and systemd unit generation. Config gets `autosave_retain` field. Main.rs gets `Autosave` subcommand with `--now`, `--install`, `--uninstall` flags. No flag = status check.

**Tech Stack:** Rust, clap (derive), chrono, std::process::Command (systemctl), std::fs

**Design doc:** `docs/plans/2026-03-09-autosave-design.md`

---

### Task 1: Add `autosave_retain` to Config

**Files:**
- Modify: `src/config.rs` (GeneralConfig struct + default fn)

**Step 1: Write the failing test**

Add to `src/config.rs` tests module:

```rust
#[test]
fn test_config_autosave_retain_default() {
    let config = Config::default();
    assert_eq!(config.general.autosave_retain, 5);
}

#[test]
fn test_config_autosave_retain_from_toml() {
    let toml_str = r#"
[general]
autosave_retain = 10
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.general.autosave_retain, 10);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test test_config_autosave_retain -- --nocapture`
Expected: FAIL — no field `autosave_retain` on GeneralConfig

**Step 3: Implement**

In `src/config.rs`:

1. Add default function:
```rust
fn default_autosave_retain() -> usize {
    5
}
```

2. Add field to `GeneralConfig`:
```rust
#[serde(default = "default_autosave_retain")]
pub autosave_retain: usize,
```

3. Add to `Default for GeneralConfig`:
```rust
autosave_retain: default_autosave_retain(),
```

**Step 4: Run tests**

Run: `cargo test --lib -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```
feat: add autosave_retain config option (default 5)
```

---

### Task 2: Add rotation logic to session module

**Files:**
- Modify: `src/session.rs` (new functions: `list_autosave_sessions`, `rotate_autosaves`)

**Step 1: Write the failing tests**

Add to `src/session.rs` tests module:

```rust
#[test]
fn test_list_autosave_sessions_filters_by_prefix() {
    let dir = tempfile::tempdir().unwrap();
    save_session(&make_test_session("work"), dir.path()).unwrap();
    save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();
    save_session(&make_test_session("autosave-20260309T110000"), dir.path()).unwrap();

    let autosaves = list_autosave_sessions(dir.path()).unwrap();
    assert_eq!(autosaves.len(), 2);
    // Should be sorted newest first
    assert_eq!(autosaves[0].name, "autosave-20260309T110000");
    assert_eq!(autosaves[1].name, "autosave-20260309T100000");
}

#[test]
fn test_rotate_autosaves_keeps_n() {
    let dir = tempfile::tempdir().unwrap();
    save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();
    save_session(&make_test_session("autosave-20260309T110000"), dir.path()).unwrap();
    save_session(&make_test_session("autosave-20260309T120000"), dir.path()).unwrap();
    save_session(&make_test_session("autosave-20260309T130000"), dir.path()).unwrap();
    save_session(&make_test_session("work"), dir.path()).unwrap(); // manual — never pruned

    let pruned = rotate_autosaves(dir.path(), 2).unwrap();
    assert_eq!(pruned, 2); // removed 2 oldest

    let remaining = list_autosave_sessions(dir.path()).unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].name, "autosave-20260309T130000");
    assert_eq!(remaining[1].name, "autosave-20260309T120000");

    // Manual session untouched
    assert!(session_exists("work", dir.path()));
}

#[test]
fn test_rotate_autosaves_noop_when_under_limit() {
    let dir = tempfile::tempdir().unwrap();
    save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();

    let pruned = rotate_autosaves(dir.path(), 5).unwrap();
    assert_eq!(pruned, 0);
    assert_eq!(list_autosave_sessions(dir.path()).unwrap().len(), 1);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test test_list_autosave test_rotate -- --nocapture`
Expected: FAIL — functions not found

**Step 3: Implement**

In `src/session.rs`, add:

```rust
const AUTOSAVE_PREFIX: &str = "autosave-";

pub fn list_autosave_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let mut all = list_sessions(sessions_dir)?;
    all.retain(|s| s.name.starts_with(AUTOSAVE_PREFIX));
    Ok(all)
}

pub fn rotate_autosaves(sessions_dir: &Path, retain: usize) -> Result<usize, SessionError> {
    let autosaves = list_autosave_sessions(sessions_dir)?;
    let mut pruned = 0;
    if autosaves.len() > retain {
        for session in &autosaves[retain..] {
            delete_session(&session.name, sessions_dir)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}
```

**Step 4: Run tests**

Run: `cargo test --lib -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```
feat: add autosave rotation logic (list + prune by prefix)
```

---

### Task 3: Add autosave timestamp naming helper

**Files:**
- Modify: `src/session.rs` (new function: `autosave_name_now`)

**Step 1: Write the failing test**

```rust
#[test]
fn test_autosave_name_format() {
    let name = autosave_name_now();
    assert!(name.starts_with("autosave-"));
    // Format: autosave-YYYYMMDDTHHMMSS — total 24 chars
    assert_eq!(name.len(), 24);
    // The part after prefix should be digits + one T
    let ts = &name[9..];
    assert_eq!(ts.len(), 15); // 8 digits + T + 6 digits
    assert_eq!(&ts[8..9], "T");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_autosave_name_format -- --nocapture`
Expected: FAIL — function not found

**Step 3: Implement**

In `src/session.rs`:

```rust
pub fn autosave_name_now() -> String {
    let now = Utc::now();
    format!("autosave-{}", now.format("%Y%m%dT%H%M%S"))
}
```

**Step 4: Run tests**

Run: `cargo test --lib -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```
feat: add autosave timestamp name generator
```

---

### Task 4: Create `src/autosave.rs` — systemd unit generation

**Files:**
- Create: `src/autosave.rs`
- Modify: `src/lib.rs` (add `pub mod autosave;`)

**Step 1: Write the failing tests**

Create `src/autosave.rs` with tests only:

```rust
use std::path::{Path, PathBuf};

const SERVICE_NAME: &str = "hyprloom-autosave.service";
const TIMER_NAME: &str = "hyprloom-autosave.timer";

fn systemd_user_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd")
        .join("user")
}

fn service_content() -> String {
    let binary = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("hyprloom"));
    format!(
        "[Unit]\n\
         Description=Hyprloom autosave session\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} autosave --now\n",
        binary.display()
    )
}

fn timer_content() -> String {
    "[Unit]\n\
     Description=Hyprloom autosave timer\n\
     \n\
     [Timer]\n\
     OnUnitActiveSec=10min\n\
     OnBootSec=1min\n\
     \n\
     [Install]\n\
     WantedBy=timers.target\n"
        .to_string()
}

pub fn install(systemd_dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(systemd_dir)?;
    let service_path = systemd_dir.join(SERVICE_NAME);
    let timer_path = systemd_dir.join(TIMER_NAME);
    std::fs::write(&service_path, service_content())?;
    std::fs::write(&timer_path, timer_content())?;
    Ok((service_path, timer_path))
}

pub fn uninstall(systemd_dir: &Path) -> std::io::Result<()> {
    // Try to disable timer first (best-effort — may fail if not enabled)
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "hyprloom-autosave.timer"])
        .output();

    let service_path = systemd_dir.join(SERVICE_NAME);
    let timer_path = systemd_dir.join(TIMER_NAME);
    if timer_path.exists() {
        std::fs::remove_file(&timer_path)?;
    }
    if service_path.exists() {
        std::fs::remove_file(&service_path)?;
    }
    Ok(())
}

pub fn is_installed(systemd_dir: &Path) -> bool {
    systemd_dir.join(TIMER_NAME).exists()
}

pub fn is_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "hyprloom-autosave.timer"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "hyprloom-autosave.timer"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_install_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let (service, timer) = install(dir.path()).unwrap();
        assert!(service.exists());
        assert!(timer.exists());

        let service_text = std::fs::read_to_string(&service).unwrap();
        assert!(service_text.contains("autosave --now"));
        assert!(service_text.contains("[Service]"));

        let timer_text = std::fs::read_to_string(&timer).unwrap();
        assert!(timer_text.contains("OnUnitActiveSec=10min"));
        assert!(timer_text.contains("[Install]"));
    }

    #[test]
    fn test_is_installed_checks_timer_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_installed(dir.path()));
        install(dir.path()).unwrap();
        assert!(is_installed(dir.path()));
    }

    #[test]
    fn test_uninstall_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        assert!(is_installed(dir.path()));
        uninstall(dir.path()).unwrap();
        assert!(!is_installed(dir.path()));
    }

    #[test]
    fn test_uninstall_noop_when_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        // Should not error on missing files
        uninstall(dir.path()).unwrap();
    }

    #[test]
    fn test_service_content_format() {
        let content = service_content();
        assert!(content.contains("Type=oneshot"));
        assert!(content.contains("autosave --now"));
    }

    #[test]
    fn test_timer_content_format() {
        let content = timer_content();
        assert!(content.contains("OnBootSec=1min"));
        assert!(content.contains("WantedBy=timers.target"));
    }
}
```

**Step 2: Add module to lib.rs**

Add `pub mod autosave;` to `src/lib.rs`.

**Step 3: Run tests to verify they pass**

Run: `cargo test autosave -- --nocapture`
Expected: ALL PASS (tests and implementation are in the same step since this is a new file)

**Step 4: Run clippy**

Run: `cargo clippy --all-targets`
Expected: No new warnings

**Step 5: Commit**

```
feat: add autosave module with systemd install/uninstall
```

---

### Task 5: Wire `Autosave` subcommand into CLI

**Files:**
- Modify: `src/main.rs` (add Autosave variant to Commands enum, add match arm)

**Step 1: Add the subcommand enum variant**

In `src/main.rs`, add to `Commands` enum:

```rust
/// Manage autosave (status, run, install/uninstall timer)
Autosave {
    /// Run autosave now (capture + rotate)
    #[arg(long)]
    now: bool,
    /// Install systemd timer
    #[arg(long)]
    install: bool,
    /// Uninstall systemd timer
    #[arg(long)]
    uninstall: bool,
},
```

**Step 2: Add the match arm**

```rust
Commands::Autosave { now, install, uninstall } => {
    let systemd_dir = hyprloom::autosave::systemd_user_dir();

    if install {
        match hyprloom::autosave::install(&systemd_dir) {
            Ok((service_path, timer_path)) => {
                println!("Created:");
                println!("  {}", service_path.display());
                println!("  {}", timer_path.display());
                println!();
                println!("To enable and start:");
                println!("  systemctl --user enable --now hyprloom-autosave.timer");
                println!();
                println!("To check status:");
                println!("  systemctl --user status hyprloom-autosave.timer");
                println!("  journalctl --user -u hyprloom-autosave.service");
            }
            Err(e) => {
                eprintln!("Error installing autosave timer: {}", e);
                std::process::exit(1);
            }
        }
    } else if uninstall {
        match hyprloom::autosave::uninstall(&systemd_dir) {
            Ok(()) => println!("Autosave timer removed."),
            Err(e) => {
                eprintln!("Error uninstalling autosave timer: {}", e);
                std::process::exit(1);
            }
        }
    } else if now {
        let hyprctl = RealHyprctl;
        let process_info = RealProcessInfo;
        let name = hyprloom::session::autosave_name_now();

        match capture_session(&name, &hyprctl, &process_info, &config) {
            Ok(session) => {
                let client_count = session.clients.len();
                if let Err(e) = save_session(&session, &sessions_dir) {
                    eprintln!("Error saving autosave session: {}", e);
                    std::process::exit(1);
                }

                let retain = config.general.autosave_retain;
                let pruned = hyprloom::session::rotate_autosaves(&sessions_dir, retain)
                    .unwrap_or(0);

                let total = hyprloom::session::list_autosave_sessions(&sessions_dir)
                    .map(|s| s.len())
                    .unwrap_or(0);

                println!(
                    "Autosaved '{}' ({} windows). Retained {}, pruned {}.",
                    name, client_count, total, pruned
                );
            }
            Err(e) => {
                eprintln!("Error capturing session: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        // Status mode (no flags)
        let installed = hyprloom::autosave::is_installed(&systemd_dir);
        let active = hyprloom::autosave::is_active();

        if !installed {
            println!("Autosave is not configured.");
            println!("Run 'hyprloom autosave --install' to set up the systemd timer.");
        } else if !active {
            println!("Autosave timer is installed but not active.");
            println!("  systemctl --user enable --now hyprloom-autosave.timer");
        } else {
            println!("Autosave is active (every 10min).");
            match hyprloom::session::list_autosave_sessions(&sessions_dir) {
                Ok(sessions) if !sessions.is_empty() => {
                    let latest = &sessions[0];
                    println!(
                        "Last: {} — {} windows",
                        latest.created_at.format("%Y-%m-%d %H:%M:%S"),
                        latest.client_count
                    );
                    println!(
                        "Retained: {} sessions (oldest: {})",
                        sessions.len(),
                        sessions.last().unwrap().name
                    );
                }
                Ok(_) => println!("No autosave sessions yet."),
                Err(e) => println!("Could not list sessions: {e}"),
            }
            println!("To disable: hyprloom autosave --uninstall");
        }
    }
}
```

**Step 3: Add imports at top of main.rs**

The `save_session`, `capture_session` etc. are already imported. No new imports needed since we use full paths for the new functions.

**Step 4: Build and verify**

Run: `cargo build`
Expected: Compiles without errors

**Step 5: Run full test suite**

Run: `cargo test`
Expected: ALL PASS (existing + new tests)

**Step 6: Run clippy**

Run: `cargo clippy --all-targets`
Expected: No new warnings

**Step 7: Commit**

```
feat: wire autosave subcommand into CLI
```

---

### Task 6: Integration test

**Files:**
- Modify: `tests/cli_test.rs` (add autosave CLI tests)

**Step 1: Read existing integration tests**

Read `tests/cli_test.rs` to understand the test patterns used.

**Step 2: Add integration tests**

```rust
#[test]
fn test_autosave_status_not_configured() {
    let cmd = Command::cargo_bin("hyprloom").unwrap();
    // Without Hyprland running, at minimum we can test the --help output
    cmd.arg("autosave").arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("autosave"));
}
```

Note: Full integration tests for autosave --now require a running Hyprland session.
The `--install` and `--uninstall` could be tested with a tempdir override,
but that would require adding a `--systemd-dir` test flag or env var.
Keep integration tests minimal — unit tests in autosave.rs cover the logic.

**Step 3: Run integration tests**

Run: `cargo test --test cli_test`
Expected: ALL PASS

**Step 4: Commit**

```
test: add autosave CLI integration test
```

---

### Task 7: Make `systemd_user_dir` public and update list command

**Files:**
- Modify: `src/autosave.rs` (make `systemd_user_dir` pub)
- Modify: `src/main.rs` (update `List` to show autosave tag)

**Step 1: Update List output**

In `src/main.rs`, modify the `Commands::List` arm to tag autosave sessions:

```rust
for s in sessions {
    let tag = if s.name.starts_with("autosave-") { " [auto]" } else { "" };
    println!(
        "  {} — {} windows ({}){}",
        s.name, s.client_count,
        s.created_at.format("%Y-%m-%d %H:%M"),
        tag
    );
}
```

**Step 2: Build and verify**

Run: `cargo build`
Expected: Compiles

**Step 3: Commit**

```
feat: tag autosave sessions in list output
```

---

### Task 8: Update docs and config example

**Files:**
- Modify: `README.md` (add autosave section)
- Modify: `CHANGELOG.md` (add entry)
- Modify: `TODO.md` (mark autosave as done)

**Step 1: Update TODO.md**

Mark `- [x] Autosave daemon via systemd timer` as done.

**Step 2: Update CHANGELOG.md**

Add entry for autosave feature under v0.2.

**Step 3: Update README.md**

Add autosave section documenting `hyprloom autosave` usage with all flags.

**Step 4: Update config.toml example**

Add `autosave_retain = 5` to the example config in README.

**Step 5: Commit**

```
docs: add autosave documentation and update changelog
```

---

## Summary

| Task | Description | Tests |
|------|-------------|-------|
| 1 | `autosave_retain` config field | 2 unit |
| 2 | Rotation logic (list + prune) | 3 unit |
| 3 | Timestamp name generator | 1 unit |
| 4 | `src/autosave.rs` — systemd install/uninstall | 6 unit |
| 5 | Wire CLI subcommand | build + existing |
| 6 | Integration test | 1 integration |
| 7 | Tag autosave in list output | build |
| 8 | Docs update | — |

**Total new tests: ~13**
**Estimated commits: 8**
