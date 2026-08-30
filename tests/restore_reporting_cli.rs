//! Real CLI and filesystem checks for the opt-in restore report boundary.

#![allow(unused_crate_dependencies)]

use hyprloom::session::{save_session, Session};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use tempfile::TempDir;

struct DesktopFixture {
    root: TempDir,
    session: Session,
}

#[allow(clippy::unwrap_used)]
impl DesktopFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("hyprctl"), include_str!("fixtures/reporting-hyprctl.sh")).unwrap();
        fs::set_permissions(bin.join("hyprctl"), fs::Permissions::from_mode(0o755)).unwrap();
        let session: Session = serde_json::from_value(json!({
            "name": "report-fixture",
            "created_at": "2026-01-01T00:00:00Z",
            "hyprland_version": "fixture",
            "monitors": [],
            "clients": [{
                "class": "foot", "title": "Project — <editor>",
                "address": "0xfixture", "stable_id": "fixture-window-1",
                "initial_class": "foot", "initial_title": "foot",
                "workspace": 3, "workspace_name": "3", "monitor": "DP-1",
                "at": [10, 20], "size": [800, 600], "floating": false,
                "fullscreen": 0, "focus_history_id": 0,
                "launch": {"command": "foot", "args": [], "hint": null}
            }]
        }))
        .unwrap();
        let fixture = Self { root, session };
        fixture.save();
        fixture.set_clients(&json!([{
            "address": "0xfixture", "stableId": "fixture-window-1",
            "class": "foot", "title": "Project — <editor>",
            "initialClass": "foot", "initialTitle": "foot",
            "workspace": {"id": 3, "name": "3"}, "monitor": 0,
            "at": [10, 20], "size": [800, 600], "floating": false,
            "fullscreen": 0, "focusHistoryID": 0, "pid": 0
        }]));
        fixture
    }

    fn save(&self) {
        save_session(&self.session, &self.root.path().join("data/hyprloom/sessions")).unwrap();
    }

    fn set_clients(&self, clients: &Value) {
        fs::write(self.root.path().join("clients.json"), serde_json::to_vec(clients).unwrap()).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("hyprloom"));
        command
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("HYPRLOOM_REPORT_FIXTURE", self.root.path())
            .env(
                "PATH",
                format!("{}:{}", self.root.path().join("bin").display(), std::env::var("PATH").unwrap_or_default()),
            );
        command
    }

    fn restore(&self, args: &[&str]) -> Output {
        self.command()
            .args(["restore", &self.session.name, "--reconcile"])
            .args(args)
            .output()
            .unwrap()
    }

    fn assert_no_dispatches(&self) {
        assert!(!self.root.path().join("dispatches").exists());
    }
}

#[test]
fn json_report_describes_an_existing_window_without_verbose_output() {
    let fixture = DesktopFixture::new();

    let output = fixture.restore(&["--report-json"]);

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "reconcile");
    assert_eq!(report["session"], "report-fixture");
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["report"]["unchanged"], 1);
    assert_eq!(report["report"]["launched"], 0);
    assert_eq!(report["report"]["windows"][0]["status"], "unchanged");
    assert_eq!(report["report"]["windows"][0]["workspace"], 3);
    assert_eq!(report["report"]["windows"][0]["title"], "Project — <editor>");
    assert_eq!(report["report"]["windows"][0]["class"], "foot");
    assert!(report["report"]["details"].is_null());
    fixture.assert_no_dispatches();
}

fn reopened_webapp_fixture() -> DesktopFixture {
    let mut fixture = DesktopFixture::new();
    let target = &mut fixture.session.clients[0];
    target.class = "chrome-x.com__-Default".into();
    target.initial_class.clone_from(&target.class);
    target.title = "Home / X".into();
    target.initial_title = "x.com_/".into();
    target.address = Some("0xclosed-x".into());
    target.stable_id = Some("closed-x-id".into());
    target.workspace = 8;
    target.workspace_name = "8".into();
    // A mistaken launch must fail, never execute a real browser in this test.
    target.launch.command = "/nonexistent/reporting-webapp-launcher".into();
    fixture.save();
    fixture.set_clients(&json!([{
        "address": "0xreopened-x", "stableId": "reopened-x-id",
        "class": "chrome-x.com__-Default", "initialClass": "chrome-x.com__-Default",
        "title": "Home / X", "initialTitle": "x.com_/", "pid": 0,
        "workspace": {"id": 8, "name": "8"}, "monitor": 0,
        "at": [10, 20], "size": [800, 600], "floating": false,
        "fullscreen": 0, "focusHistoryID": 0
    }]));
    fixture
}

#[test]
fn reopened_webapp_is_reported_as_existing_without_launching() {
    let fixture = reopened_webapp_fixture();

    for args in [vec!["--report-json"], vec!["--report-json", "--dry-run"]] {
        let output = fixture.restore(&args);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["report"]["unchanged"], 1, "{report}");
        assert_eq!(report["report"]["launched"], 0);
        assert_eq!(report["report"]["extras"], 0);
        assert_eq!(report["report"]["windows"][0]["status"], "unchanged");
        assert!(output.status.success());
        fixture.assert_no_dispatches();
    }
}

#[test]
fn ambiguous_reopened_webapps_are_skipped_without_launching() {
    let fixture = reopened_webapp_fixture();
    let mut clients: Value = serde_json::from_slice(&fs::read(fixture.root.path().join("clients.json")).unwrap()).unwrap();
    let mut other = clients[0].clone();
    other["address"] = json!("0xanother-x");
    other["stableId"] = json!("another-x-id");
    clients.as_array_mut().unwrap().push(other);
    fixture.set_clients(&clients);

    for args in [vec!["--report-json"], vec!["--report-json", "--dry-run"]] {
        let output = fixture.restore(&args);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["report"]["skipped"], 1, "{report}");
        assert_eq!(report["report"]["matched"], 0);
        assert_eq!(report["report"]["launched"], 0);
        assert_eq!(report["report"]["extras"], 2);
        assert_eq!(report["report"]["failed"], 0);
        assert_eq!(report["report"]["windows"][0]["status"], "skipped");
        assert_eq!(output.status.code(), Some(1));
        fixture.assert_no_dispatches();
    }
}

#[test]
fn default_output_stays_human_readable_and_verbose_does_not_pollute_json() {
    let fixture = DesktopFixture::new();

    let human = fixture.restore(&[]);
    let json = fixture.restore(&["--report-json", "--verbose"]);

    assert_eq!(human.status.code(), json.status.code());
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        "Reconciled session 'report-fixture':\n  1 unchanged, 0 moved, 0 launched, 0 extra left alone, 0 skipped, 0 failed\n"
    );
    let report: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["report"]["windows"].as_array().unwrap().len(), 1);
    fixture.assert_no_dispatches();
}

#[test]
fn report_includes_extras_and_marks_dry_run_repairs_as_plans() {
    let mut fixture = DesktopFixture::new();
    fixture.session.clients[0].workspace = 4;
    fixture.session.clients[0].workspace_name = "4".to_string();
    fixture.save();

    let output = fixture.restore(&["--report-json", "--dry-run"]);

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["report"]["moved"], 1);
    assert_eq!(report["report"]["windows"][0]["status"], "moved");
    assert_eq!(report["report"]["windows"][0]["workspace"], 4);
    fixture.assert_no_dispatches();

    fixture.session.clients.clear();
    fixture.save();
    let output = fixture.restore(&["--report-json"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success());
    assert_eq!(report["report"]["extras"], 1);
    assert_eq!(report["report"]["windows"][0]["status"], "extra");
    assert_eq!(report["report"]["windows"][0]["workspace"], 3);
    fixture.assert_no_dispatches();
}

#[test]
fn failed_repairs_keep_nonzero_exit_and_report_the_window_and_reason() {
    let mut fixture = DesktopFixture::new();
    fixture.session.clients[0].at = [100, 200];
    fixture.save();

    let output = fixture
        .command()
        .env("HYPRLOOM_REPORT_FAIL_DISPATCH", "1")
        .args(["restore", "report-fixture", "--reconcile", "--report-json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["report"]["matched"], 1);
    assert_eq!(report["report"]["failed"], 1);
    assert_eq!(report["report"]["moved"], 0);
    assert_eq!(report["report"]["windows"][0]["status"], "failed");
    assert!(report["report"]["windows"][0]["message"]
        .as_str()
        .unwrap()
        .contains("fixture dispatch refused"));
    assert!(fixture.root.path().join("dispatches").exists());
}

#[test]
fn early_errors_do_not_invent_a_success_report() {
    let fixture = DesktopFixture::new();

    for args in [
        vec!["restore", "missing", "--reconcile", "--report-json"],
        vec!["restore", "report-fixture", "--reconcile", "--report-json", "--max-age", "invalid"],
        vec!["replace", "missing", "--report-json"],
    ] {
        let output = fixture.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    fixture.assert_no_dispatches();
}

#[test]
fn report_flag_requires_reconciliation_and_cannot_be_used_for_login_instructions() {
    let fixture = DesktopFixture::new();

    for args in [
        vec!["restore", "report-fixture", "--report-json"],
        vec!["restore", "report-fixture", "--reconcile", "--report-json", "--on-login"],
    ] {
        let output = fixture.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    fixture.assert_no_dispatches();
}

#[test]
fn autosave_fallback_notices_do_not_corrupt_the_json_document() {
    let mut fixture = DesktopFixture::new();
    fixture.session.name = "autosave-report-fixture".to_string();
    fixture.save();

    let output = fixture
        .command()
        .args(["restore", "latest", "--reconcile", "--report-json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["session"], "autosave-report-fixture");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Falling back"));
    fixture.assert_no_dispatches();
}

#[test]
fn replace_report_remains_a_failure_after_successful_empty_desktop_recovery() {
    let mut fixture = DesktopFixture::new();
    fixture.set_clients(&json!([]));
    let target = &mut fixture.session.clients[0];
    target.class = "true".to_string();
    target.initial_class = "true".to_string();
    target.launch.command = "true".to_string();
    fixture.save();
    fs::create_dir_all(fixture.root.path().join("config/hyprloom")).unwrap();
    let config = fixture.root.path().join("config/hyprloom/config.toml");
    fs::write(&config, "[general]\nwindow_detect_timeout_ms = 100\nrestore_delay_ms = 0\n").unwrap();
    fs::set_permissions(config, fs::Permissions::from_mode(0o600)).unwrap();

    let output = fixture.command().args(["replace", "report-fixture", "--report-json"]).output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["operation"], "replace");
    assert_eq!(report["report"]["failed"], 1);
    assert_eq!(report["report"]["launched"], 0);
    assert_eq!(report["report"]["windows"][0]["status"], "failed");
    assert_eq!(report["recovery"], "succeeded");
    fixture.assert_no_dispatches();
}
