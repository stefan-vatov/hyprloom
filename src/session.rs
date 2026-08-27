use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;

// === Hyprloom session structs (what we save to disk) ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveProfile {
    pub directory: String, // "Default", "Profile 1", etc.
    pub name: String,      // "Credifit", "LinkPJ", etc.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub hyprland_version: String,
    pub monitors: Vec<Monitor>,
    pub clients: Vec<SessionClient>,
    #[serde(default)]
    pub brave_profiles: Vec<BraveProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub transform: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClient {
    pub class: String,
    pub title: String,
    /// Initial Hyprland app identity.  These fields are optional in spirit:
    /// older session files do not contain them and reconciliation falls back
    /// to `class` and `title` when they are empty.
    #[serde(default)]
    pub initial_class: String,
    #[serde(default)]
    pub initial_title: String,
    pub workspace: i32,
    pub monitor: String,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub floating: bool,
    pub fullscreen: u8,
    pub focus_history_id: i32,
    pub launch: LaunchInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchInfo {
    pub command: String,
    pub args: Vec<String>,
    pub hint: Option<String>,
}

// === Session storage ===

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session '{0}' not found")]
    NotFound(String),
    #[error("session '{0}' already exists")]
    AlreadyExists(String),
    #[error("invalid session name '{0}': use 1-128 ASCII letters, numbers, '.', '_' or '-'")]
    InvalidName(String),
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub client_count: usize,
}

pub fn save_session(session: &Session, sessions_dir: &Path) -> Result<(), SessionError> {
    validate_session_name(&session.name)?;
    std::fs::create_dir_all(sessions_dir)?;
    let path = sessions_dir.join(format!("{}.json", session.name));
    let json = serde_json::to_string_pretty(session)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load_session(name: &str, sessions_dir: &Path) -> Result<Session, SessionError> {
    validate_session_name(name)?;
    let path = sessions_dir.join(format!("{name}.json"));
    if !path.exists() {
        return Err(SessionError::NotFound(name.to_string()));
    }
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

pub fn list_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    if !sessions_dir.exists() {
        return Ok(vec![]);
    }
    let mut summaries = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(session) = serde_json::from_str::<Session>(&content) {
                    summaries.push(SessionSummary {
                        name: session.name.clone(),
                        created_at: session.created_at,
                        client_count: session.clients.len(),
                    });
                }
            }
        }
    }
    summaries.sort_by_key(|summary| Reverse(summary.created_at));
    Ok(summaries)
}

pub fn delete_session(name: &str, sessions_dir: &Path) -> Result<(), SessionError> {
    validate_session_name(name)?;
    let path = sessions_dir.join(format!("{name}.json"));
    if !path.exists() {
        return Err(SessionError::NotFound(name.to_string()));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

pub fn session_exists(name: &str, sessions_dir: &Path) -> bool {
    if validate_session_name(name).is_err() {
        return false;
    }
    sessions_dir.join(format!("{name}.json")).exists()
}

pub fn validate_session_name(name: &str) -> Result<(), SessionError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(SessionError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Copy legacy hyprflow sessions into the fork's storage without removing or
/// overwriting anything.  This is intentionally idempotent so a user can run
/// the fork repeatedly while keeping the original files as a rollback path.
pub fn migrate_legacy_sessions(
    sessions_dir: &Path,
    legacy_sessions_dir: &Path,
) -> Result<usize, SessionError> {
    if sessions_dir == legacy_sessions_dir || !legacy_sessions_dir.exists() {
        return Ok(0);
    }

    std::fs::create_dir_all(sessions_dir)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(legacy_sessions_dir)? {
        let entry = entry?;
        let source = entry.path();
        if source.extension().map(|ext| ext == "json").unwrap_or(false) {
            let destination = sessions_dir.join(entry.file_name());
            if !destination.exists() {
                std::fs::copy(source, destination)?;
                copied += 1;
            }
        }
    }
    Ok(copied)
}

// === Autosave helpers ===

pub const AUTOSAVE_PREFIX: &str = "autosave-";

pub fn autosave_name_now() -> String {
    let now = Utc::now();
    format!("autosave-{}", now.format("%Y%m%dT%H%M%S"))
}

/// Returns autosave sessions only (name starts with `AUTOSAVE_PREFIX`),
/// sorted by name descending. The timestamp format `YYYYMMDDTHHMMSS` sorts
/// lexicographically, so newest autosave is always first.
pub fn list_autosave_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let mut all = list_sessions(sessions_dir)?;
    all.retain(|s| s.name.starts_with(AUTOSAVE_PREFIX));
    // Sort by name descending — autosave-YYYYMMDDTHHMMSS sorts lexicographically
    all.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(all)
}

/// Deletes the oldest autosave sessions, keeping only the `retain` newest.
/// Returns the count of sessions deleted. Non-autosave sessions are untouched.
pub fn rotate_autosaves(sessions_dir: &Path, retain: usize) -> Result<usize, SessionError> {
    if retain == 0 {
        return Ok(0);
    }
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

/// Parses a human-readable duration string into a `chrono::Duration`.
///
/// Supported suffixes: `m` (minutes), `h` (hours), `d` (days).
/// Examples: `"30m"`, `"24h"`, `"7d"`.
pub fn parse_max_age(s: &str) -> Result<chrono::Duration, String> {
    if s.len() < 2 {
        return Err(format!("invalid duration: '{s}'"));
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration: '{s}'"))?;
    if num <= 0 {
        return Err(format!("duration must be greater than zero: '{s}'"));
    }
    let duration = match unit {
        "m" => chrono::Duration::try_minutes(num),
        "h" => chrono::Duration::try_hours(num),
        "d" => chrono::Duration::try_days(num),
        _ => {
            return Err(format!(
                "invalid duration unit '{unit}' in '{s}'. Use m, h, or d."
            ))
        }
    };
    duration.ok_or_else(|| format!("duration is out of range: '{s}'"))
}

// === Raw hyprctl JSON structs (what hyprctl returns) ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprClient {
    pub address: String,
    pub class: String,
    pub title: String,
    #[serde(default, rename = "initialClass")]
    pub initial_class: String,
    #[serde(default, rename = "initialTitle")]
    pub initial_title: String,
    pub workspace: HyprWorkspace,
    pub monitor: i32,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub floating: bool,
    pub fullscreen: u8,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprWorkspace {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprMonitor {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub transform: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    #[test]
    fn test_session_roundtrip() {
        let session = Session {
            name: "work".to_string(),
            created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            hyprland_version: "0.47.0".to_string(),
            monitors: vec![Monitor {
                name: "DP-4".to_string(),
                width: 2560,
                height: 1440,
                transform: 0,
            }],
            clients: vec![SessionClient {
                class: "kitty".to_string(),
                title: "Claude Code".to_string(),
                initial_class: "kitty".to_string(),
                initial_title: "kitty".to_string(),
                workspace: 4,
                monitor: "DP-4".to_string(),
                at: [12, 50],
                size: [842, 1378],
                floating: false,
                fullscreen: 0,
                focus_history_id: 3,
                launch: LaunchInfo {
                    command: "kitty".to_string(),
                    args: vec![],
                    hint: None,
                },
            }],
            brave_profiles: vec![],
        };

        let json = serde_json::to_string(&session).expect("serialization failed");
        let restored: Session = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(restored.name, session.name);
        assert_eq!(restored.hyprland_version, session.hyprland_version);
        assert_eq!(restored.created_at, session.created_at);

        assert_eq!(restored.monitors.len(), 1);
        let mon = &restored.monitors[0];
        assert_eq!(mon.name, "DP-4");
        assert_eq!(mon.width, 2560);
        assert_eq!(mon.height, 1440);
        assert_eq!(mon.transform, 0);

        assert_eq!(restored.clients.len(), 1);
        let client = &restored.clients[0];
        assert_eq!(client.class, "kitty");
        assert_eq!(client.title, "Claude Code");
        assert_eq!(client.workspace, 4);
        assert_eq!(client.monitor, "DP-4");
        assert_eq!(client.at, [12, 50]);
        assert_eq!(client.size, [842, 1378]);
        assert!(!client.floating);
        assert_eq!(client.fullscreen, 0);
        assert_eq!(client.focus_history_id, 3);
        assert_eq!(client.launch.command, "kitty");
        assert!(client.launch.args.is_empty());
        assert!(client.launch.hint.is_none());
    }

    fn make_test_session(name: &str) -> Session {
        Session {
            name: name.to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![SessionClient {
                class: "kitty".to_string(),
                title: "test".to_string(),
                initial_class: "kitty".to_string(),
                initial_title: "kitty".to_string(),
                workspace: 1,
                monitor: "DP-4".to_string(),
                at: [0, 0],
                size: [800, 600],
                floating: false,
                fullscreen: 0,
                focus_history_id: 0,
                launch: LaunchInfo {
                    command: "kitty".to_string(),
                    args: vec![],
                    hint: None,
                },
            }],
            brave_profiles: vec![],
        }
    }

    #[test]
    fn test_session_roundtrip_with_brave_profiles() {
        let session = Session {
            name: "brave-test".to_string(),
            created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![],
            brave_profiles: vec![
                BraveProfile {
                    directory: "Default".to_string(),
                    name: "Credifit".to_string(),
                },
                BraveProfile {
                    directory: "Profile 1".to_string(),
                    name: "LinkPJ".to_string(),
                },
            ],
        };

        let json = serde_json::to_string(&session).expect("serialization failed");
        let restored: Session = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(restored.brave_profiles.len(), 2);
        assert_eq!(restored.brave_profiles[0].directory, "Default");
        assert_eq!(restored.brave_profiles[0].name, "Credifit");
        assert_eq!(restored.brave_profiles[1].directory, "Profile 1");
        assert_eq!(restored.brave_profiles[1].name, "LinkPJ");
    }

    #[test]
    fn test_session_backward_compat_no_brave_profiles() {
        // A session JSON without the brave_profiles field (as saved by older versions).
        let json = r#"{
            "name": "old-session",
            "created_at": "2026-03-08T10:00:00Z",
            "hyprland_version": "0.54.0",
            "monitors": [],
            "clients": []
        }"#;

        let session: Session = serde_json::from_str(json).expect("deserialization must succeed");
        assert_eq!(
            session.brave_profiles.len(),
            0,
            "missing brave_profiles field should default to empty vec"
        );
    }

    #[test]
    fn test_save_and_load_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = make_test_session("work");
        save_session(&session, dir.path()).unwrap();
        let loaded = load_session("work", dir.path()).unwrap();
        assert_eq!(loaded.name, "work");
        assert_eq!(loaded.clients.len(), 1);
    }

    #[test]
    fn test_migrate_legacy_sessions_is_idempotent_and_non_destructive() {
        let legacy = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        save_session(&make_test_session("work"), legacy.path()).unwrap();

        assert_eq!(
            migrate_legacy_sessions(current.path(), legacy.path()).unwrap(),
            1
        );
        assert_eq!(load_session("work", current.path()).unwrap().name, "work");

        // Existing fork data is never overwritten, and a second pass copies nothing.
        let existing = make_test_session("work");
        save_session(&existing, current.path()).unwrap();
        assert_eq!(
            migrate_legacy_sessions(current.path(), legacy.path()).unwrap(),
            0
        );
        assert_eq!(load_session("work", current.path()).unwrap().name, "work");
    }

    #[test]
    fn test_list_sessions() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("a"), dir.path()).unwrap();
        save_session(&make_test_session("b"), dir.path()).unwrap();
        let list = list_sessions(dir.path()).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_delete_session() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("old"), dir.path()).unwrap();
        delete_session("old", dir.path()).unwrap();
        assert!(load_session("old", dir.path()).is_err());
    }

    #[test]
    fn test_load_nonexistent_session() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_session("nope", dir.path()).is_err());
    }

    #[test]
    fn test_list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let list = list_sessions(dir.path()).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_session_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!session_exists("x", dir.path()));
        save_session(&make_test_session("x"), dir.path()).unwrap();
        assert!(session_exists("x", dir.path()));
    }

    #[test]
    fn test_session_names_cannot_escape_storage_directory() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../escape", "nested/name", "..", "", "bad name"] {
            let mut session = make_test_session("safe");
            session.name = name.to_string();

            assert!(save_session(&session, dir.path()).is_err());
            assert!(load_session(name, dir.path()).is_err());
            assert!(delete_session(name, dir.path()).is_err());
            assert!(!session_exists(name, dir.path()));
        }
    }

    #[test]
    fn test_autosave_name_format() {
        let name = autosave_name_now();
        assert!(name.starts_with("autosave-"));
        // Format: autosave-YYYYMMDDTHHMMSS — total 24 chars
        assert_eq!(name.len(), 24);
        let ts = &name[9..];
        assert_eq!(ts.len(), 15);
        assert_eq!(&ts[8..9], "T");
    }

    #[test]
    fn test_list_autosave_sessions_filters_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("work"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T110000"), dir.path()).unwrap();

        let autosaves = list_autosave_sessions(dir.path()).unwrap();
        assert_eq!(autosaves.len(), 2);
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
        save_session(&make_test_session("work"), dir.path()).unwrap();

        let pruned = rotate_autosaves(dir.path(), 2).unwrap();
        assert_eq!(pruned, 2);

        let remaining = list_autosave_sessions(dir.path()).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].name, "autosave-20260309T130000");
        assert_eq!(remaining[1].name, "autosave-20260309T120000");

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

    #[test]
    fn test_rotate_autosaves_retain_zero_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();

        let pruned = rotate_autosaves(dir.path(), 0).unwrap();
        assert_eq!(pruned, 0);
        assert_eq!(list_autosave_sessions(dir.path()).unwrap().len(), 1);
    }

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
        assert!(parse_max_age("0m").is_err());
        assert!(parse_max_age("-1h").is_err());
        assert!(parse_max_age("9223372036854775807d").is_err());
    }

    #[test]
    fn test_parse_hyprctl_clients_fixture() {
        let raw = include_str!("../tests/fixtures/sample_clients.json");
        let clients: Vec<HyprClient> = serde_json::from_str(raw).expect("fixture parse failed");

        assert_eq!(clients.len(), 3);

        // First client: kitty
        let kitty = &clients[0];
        assert_eq!(kitty.address, "0x55c46f7e1350");
        assert_eq!(kitty.class, "kitty");
        assert_eq!(kitty.initial_class, "kitty");
        assert_eq!(kitty.title, "Claude Code");
        assert_eq!(kitty.workspace.id, 4);
        assert_eq!(kitty.workspace.name, "4");
        assert_eq!(kitty.monitor, 0);
        assert_eq!(kitty.at, [12, 50]);
        assert_eq!(kitty.size, [842, 1378]);
        assert!(!kitty.floating);
        assert_eq!(kitty.fullscreen, 0);
        assert_eq!(kitty.focus_history_id, 3);
        assert_eq!(kitty.pid, 9537);

        // Second client: brave-browser
        let brave = &clients[1];
        assert_eq!(brave.class, "brave-browser");
        assert_eq!(brave.workspace.id, 1);
        assert_eq!(brave.focus_history_id, 1);

        // Third client: obsidian
        let obsidian = &clients[2];
        assert_eq!(obsidian.class, "obsidian");
        assert_eq!(obsidian.title, "smart notes - Obsidian");
        assert_eq!(obsidian.workspace.id, 3);
        assert_eq!(obsidian.focus_history_id, 2);
        assert_eq!(obsidian.pid, 5000);
    }
}
