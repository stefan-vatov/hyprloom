//! Real CLI and filesystem checks for the Deskloom machine inventory protocol
//! (`list --json`, `--if-revision` mutation guards, and the dispatch start
//! marker emitted when an operation acquires the helper's operation lock).

#![allow(unused_crate_dependencies)]

use hyprloom::session::save_session;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// An isolated XDG root around the real compiled binary.
struct StoreFixture {
    root: TempDir,
}

#[allow(clippy::unwrap_used)]
impl StoreFixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.path().join("data/hyprloom/sessions")
    }

    fn seed(&self, name: &str, created: &str, windows: usize) -> PathBuf {
        let clients: Vec<Value> = (0..windows)
            .map(|index| {
                json!({
                    "class": "foot", "title": format!("window {index}"),
                    "address": format!("0x{index}"), "stable_id": format!("window-{index}"),
                    "initial_class": "foot", "initial_title": "foot",
                    "workspace": 3, "workspace_name": "3", "monitor": "DP-1",
                    "at": [10, 20], "size": [800, 600], "floating": false,
                    "fullscreen": 0, "focus_history_id": 0,
                    "launch": {"command": "foot", "args": [], "hint": null}
                })
            })
            .collect();
        let session: hyprloom::session::Session = serde_json::from_value(json!({
            "name": name,
            "created_at": created,
            "hyprland_version": "fixture",
            "monitors": [],
            "clients": clients,
        }))
        .unwrap();
        let sessions_dir = self.sessions_dir();
        save_session(&session, &sessions_dir).unwrap();
        sessions_dir.join(format!("{name}.json"))
    }

    /// Install a fake `hyprctl` that serves one fixture client so `save` can
    /// capture without a live Hyprland session.
    fn with_hyprctl_fixture(&self) {
        let bin = self.root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("hyprctl"), include_str!("fixtures/reporting-hyprctl.sh")).unwrap();
        fs::set_permissions(bin.join("hyprctl"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            self.root.path().join("clients.json"),
            serde_json::to_vec(&json!([{
                "address": "0xfixture", "stableId": "fixture-window-1",
                "class": "foot", "title": "Project — <editor>",
                "initialClass": "foot", "initialTitle": "foot",
                "workspace": {"id": 3, "name": "3"}, "monitor": 0,
                "at": [10, 20], "size": [800, 600], "floating": false,
                "fullscreen": 0, "focusHistoryID": 0, "pid": 0
            }]))
            .unwrap(),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("hyprloom"));
        command
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("HYPRLOOM_REPORT_FIXTURE", self.root.path());
        let bin = self.root.path().join("bin");
        if bin.exists() {
            let path = std::env::var("PATH").unwrap_or_default();
            command.env("PATH", format!("{}:{path}", bin.display()));
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

/// The revision the protocol promises: first 16 hex chars of SHA-256 over
/// the raw session file bytes.
#[allow(clippy::unwrap_used)]
fn file_revision(path: &Path) -> String {
    let digest = Sha256::digest(fs::read(path).unwrap());
    let mut hex = String::new();
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[allow(clippy::unwrap_used)]
fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn dispatch_markers(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|line| line.starts_with("dispatch: started ")).collect()
}

#[test]
fn list_json_emits_versioned_machine_inventory() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 2);
    let autosave = fixture.seed("autosave-20260102T000000-9999-1", "2026-01-02T00:00:00Z", 1);

    let output = fixture.run(&["list", "--json"]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["protocol"], "deskloom.inventory");
    let sessions = document["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    let work_entry = sessions.iter().find(|s| s["name"] == "work").unwrap();
    assert_eq!(work_entry["revision"], json!(file_revision(&work)));
    assert_eq!(work_entry["revision"].as_str().unwrap().len(), 16);
    assert_eq!(work_entry["windows"], 2);
    assert_eq!(work_entry["automatic"], false);
    let created = work_entry["created"].as_str().unwrap();
    let parsed = chrono::DateTime::parse_from_rfc3339(created).unwrap();
    assert_eq!(parsed.to_rfc3339(), "2026-01-01T00:00:00+00:00");

    let autosave_entry = sessions.iter().find(|s| s["name"] == "autosave-20260102T000000-9999-1").unwrap();
    assert_eq!(autosave_entry["revision"], json!(file_revision(&autosave)));
    assert_eq!(autosave_entry["windows"], 1);
    assert_eq!(autosave_entry["automatic"], true);

    // Newest capture first, matching the human listing order.
    assert_eq!(sessions[0]["name"], "autosave-20260102T000000-9999-1");
    assert_eq!(sessions[1]["name"], "work");
}

#[test]
fn list_json_on_an_empty_store_emits_an_empty_inventory() {
    let fixture = StoreFixture::new();

    let output = fixture.run(&["list", "--json"]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["protocol"], "deskloom.inventory");
    assert_eq!(document["sessions"], json!([]));
}

#[test]
fn human_list_output_stays_byte_identical() {
    let fixture = StoreFixture::new();
    fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    fixture.seed("autosave-20260102T000000-9999-1", "2026-01-02T00:00:00Z", 2);

    let output = fixture.run(&["list"]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        "Saved sessions:\n  \
         autosave-20260102T000000-9999-1 — 2 windows (2026-01-02 00:00) [auto]\n  \
         work — 1 windows (2026-01-01 00:00)\n"
    );
}

#[test]
fn delete_with_matching_revision_deletes_the_session() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    let revision = file_revision(&work);

    let output = fixture.run(&["delete", "work", "--if-revision", &revision]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!work.exists(), "the guarded delete must remove the session");
}

#[test]
fn delete_with_stale_revision_conflicts_without_deleting() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    let bytes_before = fs::read(&work).unwrap();

    let output = fixture.run(&["delete", "work", "--if-revision", "0123456789abcdef"]);

    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["error"], "revision-conflict");
    assert_eq!(document["expected"], "0123456789abcdef");
    assert_eq!(document["actual"], json!(file_revision(&work)));
    assert_eq!(fs::read(&work).unwrap(), bytes_before, "a revision conflict must mutate nothing");
}

#[test]
fn delete_with_missing_session_conflicts_with_null_actual() {
    let fixture = StoreFixture::new();

    let output = fixture.run(&["delete", "never-saved", "--if-revision", "0123456789abcdef"]);

    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["error"], "revision-conflict");
    assert_eq!(document["expected"], "0123456789abcdef");
    assert!(document["actual"].is_null());
    assert!(!fixture.sessions_dir().join("never-saved.json").exists());
}

#[test]
fn delete_with_malformed_revision_is_a_usage_error() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    let bytes_before = fs::read(&work).unwrap();

    let output = fixture.run(&["delete", "work", "--if-revision", "not-a-revision"]);

    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty(), "usage errors must not emit machine documents");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("16"), "the explanation must state the required format: {stderr}");
    assert_eq!(fs::read(&work).unwrap(), bytes_before);
}

#[test]
fn save_force_with_matching_revision_overwrites_the_session() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    fixture.with_hyprctl_fixture();
    let revision = file_revision(&work);

    let output = fixture.run(&["save", "work", "--force", "--if-revision", &revision]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Saved session 'work'"), "{stdout}");
    assert_ne!(file_revision(&work), revision, "an overwrite must produce a new content revision");
}

#[test]
fn save_force_with_stale_revision_conflicts_without_touching_the_file() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    fixture.with_hyprctl_fixture();
    let bytes_before = fs::read(&work).unwrap();

    let output = fixture.run(&["save", "work", "--force", "--if-revision", "0123456789abcdef"]);

    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["error"], "revision-conflict");
    assert_eq!(document["expected"], "0123456789abcdef");
    assert_eq!(document["actual"], json!(file_revision(&work)));
    assert_eq!(fs::read(&work).unwrap(), bytes_before, "a stale save must not recapture or rewrite");
}

#[test]
fn save_force_with_missing_session_conflicts_instead_of_creating() {
    let fixture = StoreFixture::new();
    fixture.with_hyprctl_fixture();

    let output = fixture.run(&["save", "brand-new", "--force", "--if-revision", "0123456789abcdef"]);

    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["error"], "revision-conflict");
    assert!(document["actual"].is_null());
    assert!(!fixture.sessions_dir().join("brand-new.json").exists());
}

#[test]
fn replace_with_stale_revision_conflicts_before_any_capture() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    fixture.with_hyprctl_fixture();
    let bytes_before = fs::read(&work).unwrap();

    let output = fixture.run(&["replace", "work", "--if-revision", "0123456789abcdef"]);

    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    let document = stdout_json(&output);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["error"], "revision-conflict");
    assert_eq!(document["expected"], "0123456789abcdef");
    assert_eq!(document["actual"], json!(file_revision(&work)));
    // The guard must fire before the safety snapshot is captured.
    let seeded: Vec<String> = fs::read_dir(fixture.sessions_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(seeded, vec!["work.json"], "no safety backup may appear: {seeded:?}");
    assert_eq!(fs::read(&work).unwrap(), bytes_before);
}

#[test]
fn revision_conflicts_keep_the_plain_text_explanation_on_stderr() {
    let fixture = StoreFixture::new();
    fixture.seed("work", "2026-01-01T00:00:00Z", 1);

    let output = fixture.run(&["delete", "work", "--if-revision", "0123456789abcdef"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("work"), "stderr must name the session: {stderr}");
    assert!(stderr.contains("0123456789abcdef"), "stderr must state the expected revision: {stderr}");
}

#[test]
fn dispatch_marker_is_emitted_once_when_an_operation_starts() {
    let fixture = StoreFixture::new();
    fixture.seed("work", "2026-01-01T00:00:00Z", 1);

    let output = fixture.run(&["list"]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        dispatch_markers(&String::from_utf8_lossy(&output.stderr)),
        vec!["dispatch: started list -"]
    );
}

#[test]
fn dispatch_marker_names_the_target_session() {
    let fixture = StoreFixture::new();

    let output = fixture.run(&["delete", "missing-name"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        dispatch_markers(&String::from_utf8_lossy(&output.stderr)),
        vec!["dispatch: started delete missing-name"]
    );
}

#[test]
fn dispatch_marker_precedes_a_guarded_save() {
    let fixture = StoreFixture::new();
    let work = fixture.seed("work", "2026-01-01T00:00:00Z", 1);
    fixture.with_hyprctl_fixture();

    let output = fixture.run(&["save", "work", "--force", "--if-revision", &file_revision(&work)]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        dispatch_markers(&String::from_utf8_lossy(&output.stderr)),
        vec!["dispatch: started save work"]
    );
}
