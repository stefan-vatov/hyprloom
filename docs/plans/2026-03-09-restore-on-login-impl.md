# Restore on Login Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `--max-age` and `--on-login` flags to `hyprloom restore` for safe session restore at Hyprland startup.

**Architecture:** Duration parsing helper in `session.rs`, age check before restore in `main.rs`, `--on-login` prints exec-once line and exits.

**Tech Stack:** Rust, clap (derive), chrono (Duration/Utc)

**Design doc:** `docs/plans/2026-03-09-restore-on-login-design.md`

---

### Task 1: Add duration parsing helper

**Files:**
- Modify: `src/session.rs` (new function + tests)

**Step 1: Write failing tests**

Add to `src/session.rs` tests module:

```rust
#[test]
fn test_parse_duration_minutes() {
    assert_eq!(parse_max_age("30m").unwrap(), chrono::Duration::minutes(30));
}

#[test]
fn test_parse_duration_hours() {
    assert_eq!(parse_max_age("24h").unwrap(), chrono::Duration::hours(24));
}

#[test]
fn test_parse_duration_days() {
    assert_eq!(parse_max_age("7d").unwrap(), chrono::Duration::days(7));
}

#[test]
fn test_parse_duration_invalid() {
    assert!(parse_max_age("abc").is_err());
    assert!(parse_max_age("10x").is_err());
    assert!(parse_max_age("").is_err());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test parse_max_age -- --nocapture`
Expected: FAIL — function not found

**Step 3: Implement**

Add to `src/session.rs`, above the `#[cfg(test)]` block:

```rust
pub fn parse_max_age(s: &str) -> Result<chrono::Duration, String> {
    if s.len() < 2 {
        return Err(format!("invalid duration: '{s}'"));
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str.parse().map_err(|_| format!("invalid duration: '{s}'"))?;
    match unit {
        "m" => Ok(chrono::Duration::minutes(num)),
        "h" => Ok(chrono::Duration::hours(num)),
        "d" => Ok(chrono::Duration::days(num)),
        _ => Err(format!("invalid duration unit '{unit}' in '{s}'. Use m, h, or d.")),
    }
}
```

**Step 4: Run tests**

Run: `cargo test --lib -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```
feat: add duration parser for --max-age flag (30m, 24h, 7d)
```

---

### Task 2: Add `--max-age` and `--on-login` flags to restore

**Files:**
- Modify: `src/main.rs` (Restore variant + match arm)

**Step 1: Add flags to Restore variant**

```rust
Restore {
    /// Session name (default: "latest")
    name: Option<String>,
    /// Preview without executing
    #[arg(short, long)]
    dry_run: bool,
    /// Skip restore if session is older than duration (e.g., 24h, 7d, 30m)
    #[arg(long)]
    max_age: Option<String>,
    /// Print exec-once line for Hyprland config
    #[arg(long)]
    on_login: bool,
},
```

**Step 2: Handle `--on-login` early exit**

At the start of the `Commands::Restore` match arm, before the existing code:

```rust
Commands::Restore { name, dry_run, max_age, on_login } => {
    if on_login {
        println!("Add this line to ~/.config/hypr/hyprland.conf:");
        println!();
        println!("  exec-once = hyprloom restore --max-age 24h");
        println!();
        println!("This will restore your last saved session on login.");
        println!("Sessions older than 24h will be skipped.");
        return;
    }

    let name = name.unwrap_or_else(|| config.general.default_session.clone());

    match load_session(&name, &sessions_dir) {
        Ok(session) => {
            // Max age check
            if let Some(ref age_str) = max_age {
                match hyprloom::session::parse_max_age(age_str) {
                    Ok(max_duration) => {
                        let age = chrono::Utc::now() - session.created_at;
                        if age > max_duration {
                            println!(
                                "Session '{}' is too old (created {}).",
                                name,
                                session.created_at.format("%Y-%m-%d %H:%M")
                            );
                            println!("Skipping restore (max age: {}).", age_str);
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            // ... existing restore_session call unchanged
```

**Step 3: Add `use chrono;` if not already imported**

Check if `chrono` is already usable in main.rs. It's a dependency but may not be imported. Add `use chrono::Utc;` if needed, or use full path `chrono::Utc::now()`.

**Step 4: Build and run full test suite**

Run: `cargo build && cargo test`
Expected: ALL PASS

**Step 5: Commit**

```
feat: add --max-age and --on-login flags to restore command
```

---

### Task 3: Update docs

**Files:**
- Modify: `README.md` (restore usage + on-login section)
- Modify: `CHANGELOG.md` (new entries)
- Modify: `TODO.md` (mark done)

**Step 1: Update TODO.md**

Change `- [ ] Restore on login via \`exec-once\` in Hyprland config` to `- [x]`.

**Step 2: Update CHANGELOG.md**

Add to `### Added` under `[Unreleased] (v0.2.0)`:

```
- `--max-age` flag for restore: skip restore if session is older than specified duration (e.g., `24h`, `7d`)
- `--on-login` flag for restore: prints `exec-once` line for Hyprland config
```

**Step 3: Update README.md**

Add to the restore usage block (after `hyprloom restore --dry-run`):

```bash
hyprloom restore --max-age 24h  # skip if session older than 24h
hyprloom restore --on-login     # print exec-once line for hyprland.conf
```

Add a new section "### Restore on Login" after the Autosave section:

```markdown
### Restore on Login

To automatically restore your session when Hyprland starts:

```bash
hyprloom restore --on-login
```

This prints an `exec-once` line to add to `~/.config/hypr/hyprland.conf`:

```
exec-once = hyprloom restore --max-age 24h
```

The `--max-age` flag prevents restoring stale sessions. Accepted formats:
`30m` (minutes), `24h` (hours), `7d` (days).
```

**Step 4: Commit**

```
docs: add restore on login documentation
```

---

## Summary

| Task | Description | Tests |
|------|-------------|-------|
| 1 | Duration parser | 4 unit |
| 2 | --max-age + --on-login flags | build + existing |
| 3 | Docs update | — |

**Total new tests: 4**
**Estimated commits: 3**
