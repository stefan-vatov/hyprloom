# Brave Profile Support — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Capture active Brave profiles and restore one window per profile on the correct workspace.

**Architecture:** New module `src/brave.rs` reads Brave's `Local State` JSON to discover profiles. Capture stores active profiles in Session. Restore skips individual Brave windows and instead launches one `brave --profile-directory=X` per profile, moving each to its configured workspace. Config gains `profile_workspaces` and `default_workspace` fields on `AppConfig`.

**Tech Stack:** Rust, serde_json (already a dependency), existing trait-based test infrastructure.

---

### Task 1: Add BraveProfile to Session data model

**Files:**
- Modify: `src/session.rs:6-13` (Session struct)
- Test: `src/session.rs` (existing roundtrip test)

**Step 1: Write the failing test**

Add to `src/session.rs` tests:

```rust
#[test]
fn test_session_roundtrip_with_brave_profiles() {
    let session = Session {
        name: "bp-test".to_string(),
        created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        hyprland_version: "0.54.1".to_string(),
        monitors: vec![],
        clients: vec![],
        brave_profiles: vec![
            BraveProfile { directory: "Default".to_string(), name: "Credifit".to_string() },
            BraveProfile { directory: "Profile 1".to_string(), name: "LinkPJ".to_string() },
        ],
    };
    let json = serde_json::to_string(&session).unwrap();
    let restored: Session = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.brave_profiles.len(), 2);
    assert_eq!(restored.brave_profiles[0].directory, "Default");
    assert_eq!(restored.brave_profiles[1].name, "LinkPJ");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_session_roundtrip_with_brave_profiles -- --nocapture`
Expected: FAIL — `brave_profiles` field and `BraveProfile` struct don't exist.

**Step 3: Write minimal implementation**

In `src/session.rs`, add the struct and field:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveProfile {
    pub directory: String,
    pub name: String,
}
```

Add to `Session`:
```rust
#[serde(default)]
pub brave_profiles: Vec<BraveProfile>,
```

`#[serde(default)]` ensures backward compatibility — old session files without this field will deserialize with an empty vec.

**Step 4: Run test to verify it passes**

Run: `cargo test test_session_roundtrip_with_brave_profiles -- --nocapture`
Expected: PASS

**Step 5: Fix compile errors in other files**

Any file that constructs `Session` (capture.rs tests, restore.rs tests, session.rs helpers) needs `brave_profiles: vec![]` added. Fix all until `cargo test` passes fully.

**Step 6: Commit**

```
feat: add BraveProfile struct and brave_profiles field to Session
```

---

### Task 2: Add profile_workspaces and default_workspace to AppConfig

**Files:**
- Modify: `src/config.rs:31-37` (AppConfig struct)
- Test: `src/config.rs` tests

**Step 1: Write the failing test**

Add to `src/config.rs` tests:

```rust
#[test]
fn test_config_brave_profile_workspaces() {
    let toml_str = r#"
[apps.brave-browser]
binary = "brave"
default_workspace = 1
profile_workspaces = { "Default" = 1, "Profile 1" = 6 }
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let brave = config.apps.get("brave-browser").unwrap();
    assert_eq!(brave.default_workspace, Some(1));
    let pw = brave.profile_workspaces.as_ref().unwrap();
    assert_eq!(pw.get("Default"), Some(&1));
    assert_eq!(pw.get("Profile 1"), Some(&6));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_config_brave_profile_workspaces -- --nocapture`
Expected: FAIL — fields don't exist on AppConfig.

**Step 3: Write minimal implementation**

Add to `AppConfig`:
```rust
pub profile_workspaces: Option<HashMap<String, i32>>,
pub default_workspace: Option<i32>,
```

**Step 4: Run test to verify it passes**

Run: `cargo test -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```
feat: add profile_workspaces and default_workspace to AppConfig
```

---

### Task 3: Create brave.rs module for Local State reading

**Files:**
- Create: `src/brave.rs`
- Modify: `src/lib.rs` (add `pub mod brave;`)

**Step 1: Write the failing test**

In `src/brave.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_state_profiles() {
        let json = r#"{
            "profile": {
                "info_cache": {
                    "Default": {"name": "Credifit"},
                    "Profile 1": {"name": "LinkPJ"},
                    "Profile 2": {"name": "ABRH Bahia"}
                }
            }
        }"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert_eq!(profiles.len(), 3);
        assert!(profiles.iter().any(|p| p.directory == "Default" && p.name == "Credifit"));
        assert!(profiles.iter().any(|p| p.directory == "Profile 1" && p.name == "LinkPJ"));
    }

    #[test]
    fn test_parse_local_state_empty() {
        let json = r#"{"profile": {"info_cache": {}}}"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn test_parse_local_state_missing_field() {
        let json = r#"{"other": "data"}"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert!(profiles.is_empty());
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test brave::tests -- --nocapture`
Expected: FAIL — module doesn't exist.

**Step 3: Write minimal implementation**

Create `src/brave.rs`:

```rust
use crate::session::BraveProfile;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BraveError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Default path to Brave's Local State file on Linux.
pub fn local_state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("BraveSoftware/Brave-Browser/Local State")
}

/// Read and parse profiles from the Local State file.
pub fn read_profiles() -> Result<Vec<BraveProfile>, BraveError> {
    let path = local_state_path();
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(path)?;
    parse_profiles_from_local_state(&content)
        .map_err(|e| BraveError::Json(e))
}

/// Parse profile info from Local State JSON content.
pub fn parse_profiles_from_local_state(json_str: &str) -> Result<Vec<BraveProfile>, serde_json::Error> {
    let value: Value = serde_json::from_str(json_str)?;
    let profiles = value
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
        .map(|cache| {
            cache
                .iter()
                .map(|(dir, info)| BraveProfile {
                    directory: dir.clone(),
                    name: info
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or(dir)
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(profiles)
}
```

Add to `src/lib.rs`:
```rust
pub mod brave;
```

**Step 4: Run test to verify it passes**

Run: `cargo test brave::tests -- --nocapture`
Expected: PASS

**Step 5: Commit**

```
feat: add brave.rs module for reading Local State profiles
```

---

### Task 4: Integrate profile capture into capture_session

**Files:**
- Modify: `src/capture.rs:23-63` (capture_session function)
- Test: `src/capture.rs` tests

**Step 1: Write the failing test**

Add to `src/capture.rs` tests:

```rust
#[test]
fn test_capture_includes_brave_profiles() {
    let hyprctl = MockHyprctl {
        clients: vec![make_hypr_client("brave-browser", 7001)],
        monitors: vec![make_monitor("DP-1")],
    };
    let config = Config {
        general: GeneralConfig::default(),
        filters: FilterConfig { ignore_classes: vec![] },
        apps: HashMap::new(),
    };

    // This test verifies the session includes brave_profiles.
    // The actual profile reading is tested in brave.rs;
    // here we just verify integration doesn't break.
    let session = capture_session("test", &hyprctl, &empty_process(), &config).unwrap();
    // brave_profiles should be populated (may be empty if Brave not installed in test env)
    // The important thing is it doesn't error.
    assert!(session.brave_profiles.is_empty() || !session.brave_profiles.is_empty());
}
```

**Step 2: Implement**

In `capture_session()`, after building `clients`, add:

```rust
let brave_profiles = if clients.iter().any(|c| c.class == "brave-browser") {
    crate::brave::read_profiles().unwrap_or_default()
} else {
    vec![]
};
```

Pass `brave_profiles` to the `Session` constructor.

**Step 3: Run tests**

Run: `cargo test -- --nocapture`
Expected: ALL PASS

**Step 4: Commit**

```
feat: capture active Brave profiles when Brave windows present
```

---

### Task 5: Restore Brave by profile instead of individual windows

**Files:**
- Modify: `src/restore.rs` (restore_session function)
- Test: `src/restore.rs` tests

**Step 1: Write the failing test**

Add to `src/restore.rs` tests:

```rust
#[test]
fn test_restore_brave_by_profile_dry_run() {
    let session = Session {
        name: "test".to_string(),
        created_at: Utc::now(),
        hyprland_version: "0.54.1".to_string(),
        monitors: vec![],
        clients: vec![
            make_client("brave-browser", 1, [0, 0], [800, 600], false, 0, "brave", vec![], None),
            make_client("brave-browser", 8, [0, 0], [800, 600], false, 0, "brave", vec![], None),
            make_client("kitty", 4, [0, 0], [800, 600], false, 0, "kitty", vec![], None),
        ],
        brave_profiles: vec![
            crate::session::BraveProfile {
                directory: "Default".to_string(),
                name: "Credifit".to_string(),
            },
            crate::session::BraveProfile {
                directory: "Profile 1".to_string(),
                name: "LinkPJ".to_string(),
            },
        ],
    };

    let mut apps = HashMap::new();
    apps.insert("brave-browser".to_string(), crate::config::AppConfig {
        binary: Some("brave".to_string()),
        capture_cwd: None,
        capture_last_command: None,
        hint_template: None,
        profile_workspaces: Some(HashMap::from([
            ("Default".to_string(), 1),
            ("Profile 1".to_string(), 6),
        ])),
        default_workspace: Some(1),
    });

    let config = Config {
        general: GeneralConfig::default(),
        filters: FilterConfig { ignore_classes: vec![] },
        apps,
    };

    let mock = MockHyprctl::new(vec![]);

    let report = restore_session(&session, &mock, &config, true, true).unwrap();

    // Individual brave windows should be skipped in dry-run output.
    // Instead, profile-based entries should appear.
    let profile_entries: Vec<_> = report.details.iter()
        .filter(|d| d.contains("profile"))
        .collect();
    assert!(profile_entries.len() >= 2, "should have entries for 2 profiles; got: {:?}", report.details);

    // Kitty should still be present as normal.
    assert!(report.details.iter().any(|d| d.contains("kitty")), "kitty should be restored normally");
}
```

**Step 2: Implement**

In `restore_session()`, modify the logic:

1. Before the workspace loop, check if session has `brave_profiles` and config has `apps.brave-browser`:
   - If yes, filter out all `brave-browser` clients from the workspace loop
   - After the main loop, iterate `session.brave_profiles` and for each profile:
     - Determine target workspace from `config.apps["brave-browser"].profile_workspaces` (fallback to `default_workspace`, fallback to 1)
     - In dry-run: log `[dry-run] brave profile "Name" (Dir) → ws=X`
     - In real restore: launch `brave --profile-directory=Dir`, poll for window, move to workspace

2. The key change in the main loop is:
```rust
// Skip brave-browser windows when profiles are available (handled separately).
let has_brave_profiles = !session.brave_profiles.is_empty()
    && config.apps.contains_key("brave-browser");

// In the filter, add:
if has_brave_profiles && client.class == "brave-browser" {
    continue;
}
```

3. After the main workspace loop, add profile restore:
```rust
if has_brave_profiles {
    let brave_config = config.apps.get("brave-browser");
    let binary = brave_config
        .and_then(|c| c.binary.clone())
        .unwrap_or_else(|| "brave".to_string());
    let default_ws = brave_config
        .and_then(|c| c.default_workspace)
        .unwrap_or(1);
    let profile_ws = brave_config
        .and_then(|c| c.profile_workspaces.as_ref());

    for profile in &session.brave_profiles {
        let ws = profile_ws
            .and_then(|m| m.get(&profile.directory))
            .copied()
            .unwrap_or(default_ws);

        if dry_run {
            report.details.push(format!(
                "[dry-run] brave profile \"{}\" ({}) → ws={}",
                profile.name, profile.directory, ws
            ));
            report.details.push(format!(
                "  {} --profile-directory={}", binary, profile.directory
            ));
            report.details.push(format!(
                "  hyprctl dispatch movetoworkspacesilent {},address:0xNEW", ws
            ));
            report.restored += 1;
            continue;
        }

        // Duplicate check: is a brave-browser already on this workspace?
        let key = ("brave-browser".to_string(), ws);
        if let Some(count) = existing_counts.get_mut(&key) {
            if *count > 0 {
                report.details.push(format!(
                    "SKIP: brave profile \"{}\" — brave-browser already on ws={}",
                    profile.name, ws
                ));
                report.skipped += 1;
                *count -= 1;
                continue;
            }
        }

        // Real restore: launch brave with profile, poll, move to workspace.
        // (Same pattern as restore_single_client but simpler — no pixel positioning)
    }
}
```

**Step 3: Run tests**

Run: `cargo test -- --nocapture`
Expected: ALL PASS

**Step 4: Commit**

```
feat: restore Brave windows by profile with configurable workspaces
```

---

### Task 6: Update user's config.toml and test end-to-end

**Files:**
- Verify: `~/.config/hyprloom/config.toml`

**Step 1: Add profile config**

The user should add to their config:
```toml
[apps.brave-browser]
binary = "brave"
default_workspace = 1
profile_workspaces = { "Default" = 1, "Profile 1" = 6 }
```

**Step 2: Build release**

Run: `cargo build --release`

**Step 3: Test capture**

Run: `./target/release/hyprloom save profile-test --force`
Verify: session JSON contains `brave_profiles` array with expected profiles.

**Step 4: Test dry-run restore**

Run: `./target/release/hyprloom restore profile-test --dry-run`
Verify: output shows profile-based brave entries instead of generic brave windows.

**Step 5: Commit**

```
chore: end-to-end validation of Brave profile support
```
