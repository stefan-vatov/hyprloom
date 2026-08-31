//! Contract checks for launch correlation composed with the fake boundaries
//! (gqg.36.3.2).
//!
//! These scenarios wire the controlled fake-app helper, the fake-Hyprland
//! boundary, and the real compiled CLI together: readiness-gated delayed
//! mapping with pid-file substitution lets restore correlate a launched
//! window through genuine process-tree relation. Failure paths (missing
//! binaries, correlation timeouts, dry-run) are proven to fail closed
//! without duplicate or unrelated placement.

#![allow(unused_crate_dependencies, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/fake_hyprland.rs"]
mod fake_hyprland;
#[path = "support/process_fixture.rs"]
mod process_fixture;
#[path = "support/mod.rs"]
mod support;

use fake_hyprland::{FakeHyprland, HyprlandScenario, MappingEvent};
use process_fixture::{install_fake_app, pid_alive, read_process_runs, release_all, HelperHandle, CHILD_PID_FILE};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use support::Case;

const CORRELATION_WAIT: Duration = Duration::from_secs(5);

fn monitor_json() -> Value {
    json!({"id": 0, "name": "DP-1", "width": 1920, "height": 1080, "transform": 0, "x": 0, "y": 0})
}

fn mapped_client() -> Value {
    json!({
        "address": "0xapp", "class": "fake-app", "title": "sentinel-title",
        "stableId": "stable-app", "initialClass": "fake-app", "initialTitle": "sentinel-title",
        "workspace": {"id": 3, "name": "3"}, "monitor": 0,
        "at": [10, 20], "size": [800, 600], "floating": false,
        "fullscreen": 0, "focusHistoryID": 1,
        "pid": {"pid_file": CHILD_PID_FILE}
    })
}

fn launch_session() -> Value {
    json!({
        "name": "correlate",
        "created_at": "2026-01-01T00:00:00Z",
        "hyprland_version": "fixture",
        "monitors": [],
        "clients": [{
            "class": "fake-app", "title": "sentinel-title",
            "address": "0xgone", "stable_id": "stable-gone",
            "initial_class": "fake-app", "initial_title": "sentinel-title",
            "workspace": 3, "workspace_name": "3", "monitor": "DP-1",
            "at": [10, 20], "size": [800, 600], "floating": false,
            "fullscreen": 0, "focus_history_id": 0,
            "launch": {"command": "fake-app", "args": ["window"], "hint": null}
        }]
    })
}

fn gated_mapping_scenario(gate: &str, initial: Vec<Value>) -> HyprlandScenario {
    HyprlandScenario {
        clients: initial,
        monitors: vec![monitor_json()],
        workspaces: vec![json!({"id": 3, "name": "3"})],
        focused_address: None,
        version: "0.56.2-fake".to_owned(),
        batch_rejection: fake_hyprland::BatchRejection::ExitNonzero,
        mapping_events: vec![MappingEvent {
            after_dispatch: None,
            after_query: Some(1),
            gated_on: Some(gate.to_owned()),
            client: mapped_client(),
        }],
        faults: Vec::new(),
        request_gate: None,
    }
}

fn write_config(case: &mut Case, timeout_ms: u64) {
    let config = format!("[general]\nwindow_detect_timeout_ms = {timeout_ms}\nrestore_delay_ms = 0\n");
    case.write_file("config/hyprloom/config.toml", config.as_bytes());
}

fn wait_until_gone(pid: u32) -> bool {
    let deadline = Instant::now() + CORRELATION_WAIT;
    while Instant::now() < deadline && pid_alive(pid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    !pid_alive(pid)
}

fn reap_helpers(case: &Case) {
    for pid in release_all(case.root()) {
        assert!(wait_until_gone(pid), "helper {pid} must be reaped");
    }
}

#[test]
fn fake_maps_delayed_window_only_when_query_and_gate_match() {
    let mut case = Case::new("gate-unit");
    let gate = "artifacts/proc/child.pid";
    let fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario(gate, vec![]));

    let before = fake.call(&["clients", "-j"]);
    case.assert(
        "gate keeps the window hidden",
        serde_json::from_str::<Value>(&before.stdout_str())
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        &before.stdout_str(),
    );

    case.write_file("artifacts/proc/child.pid", b"424242");
    let after = fake.call(&["clients", "-j"]);
    let clients = serde_json::from_str::<Value>(&after.stdout_str()).unwrap();
    case.assert(
        "gate release maps the window",
        clients[0]["pid"] == json!(424_242),
        &format!("pid must be substituted from the pid file: {clients}"),
    );

    let again = fake.call(&["clients", "-j"]);
    let clients = serde_json::from_str::<Value>(&again.stdout_str()).unwrap();
    case.assert(
        "mapping fires exactly once",
        clients.as_array().unwrap().len() == 1,
        &format!("the event must be consumed: {clients}"),
    );
}

#[test]
fn restore_launches_missing_target_and_correlates_via_process_tree() {
    let mut case = Case::new("launch-correlate");
    install_fake_app(&mut case);
    let gate = CHILD_PID_FILE;
    let fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario(gate, vec![]));
    case.save_session("correlate", &launch_session());

    let run = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("report parses", &run);
    case.assert("restore succeeds", run.success(), &run.stderr_str());
    case.assert("target was launched", report["report"]["launched"] == 1, &format!("{report}"));
    case.assert(
        "window status is launched",
        report["report"]["windows"][0]["status"] == "launched",
        &format!("{report}"),
    );

    let topology = fake.topology();
    case.assert(
        "window is mapped in the fake",
        topology.clients.len() == 1,
        &format!("{:?}", topology.clients),
    );
    case.assert(
        "mapped pid is the genuine child pid",
        topology.clients[0]["pid"] != json!(0),
        "pid-file substitution must yield a live pid",
    );

    let runs = read_process_runs(&case);
    case.assert("exactly one launch", runs.len() == 1, &format!("{runs:?}"));
    case.assert(
        "launched argv is the scripted contract",
        runs[0].argv == vec!["window"],
        &format!("{:?}", runs[0].argv),
    );
    reap_helpers(&case);
}

#[test]
fn capture_round_trip_reports_window_unchanged() {
    let mut case = Case::new("round-trip");
    install_fake_app(&mut case);
    let mut handle = HelperHandle::spawn(&case, &["window"]);
    let ready = case.wait_for_signal("artifacts/proc/ready", Duration::from_secs(5));
    case.assert("helper signals readiness", ready, "window helper must become ready");
    let _fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario("artifacts/proc/ready", vec![]));

    let save = case.run("save", &["save", "demo"]);
    case.assert("capture succeeds", save.success(), &save.stderr_str());
    let session_path = case.root().join("data/hyprloom/sessions/demo.json");
    case.assert("session was written", session_path.exists(), "capture must persist the session");

    let run = case.run("restore", &["restore", "demo", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("report parses", &run);
    case.assert("restore succeeds", run.success(), &run.stderr_str());
    case.assert("existing window is unchanged", report["report"]["unchanged"] == 1, &format!("{report}"));

    handle.stop(CORRELATION_WAIT);
    reap_helpers(&case);
}

#[test]
fn unauthorized_launch_command_fails_per_window() {
    let mut case = Case::new("unauthorized");
    install_fake_app(&mut case);
    let fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario(CHILD_PID_FILE, vec![]));
    let mut session = launch_session();
    session["clients"][0]["launch"]["command"] = json!("definitely-not-on-path-hyprloom");
    case.save_session("correlate", &session);

    let run = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("unauthorized launch still reports", &run);
    case.assert("exit is nonzero", run.code() == Some(1), "an unauthorized launch must fail");
    case.assert("failure is per window", report["report"]["failed"] == 1, &format!("{report}"));
    case.assert(
        "diagnostic names the authorization rule",
        report["report"]["windows"][0]["message"].as_str().unwrap().contains("not authorized"),
        &format!("{report}"),
    );
    case.assert(
        "no window was mapped",
        fake.topology().clients.is_empty(),
        "nothing may map without a launch",
    );
    reap_helpers(&case);
}

#[test]
fn missing_binary_for_authorized_identity_fails_per_window() {
    let mut case = Case::new("missing-binary");
    install_fake_app(&mut case);
    let fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario(CHILD_PID_FILE, vec![]));
    let mut session = launch_session();
    // The command matches the class identity (authorized), but no binary of
    // that name exists on the controlled PATH.
    session["clients"][0]["launch"]["command"] = json!("fake-app-absent");
    session["clients"][0]["class"] = json!("fake-app-absent");
    session["clients"][0]["initial_class"] = json!("fake-app-absent");
    case.save_session("correlate", &session);

    let run = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("missing binary still reports", &run);
    case.assert("exit is nonzero", run.code() == Some(1), "a missing binary must fail the window");
    case.assert("failure is per window", report["report"]["failed"] == 1, &format!("{report}"));
    case.assert(
        "diagnostic names the missing binary",
        report["report"]["windows"][0]["message"].as_str().unwrap().contains("not found"),
        &format!("{report}"),
    );
    case.assert(
        "no window was mapped",
        fake.topology().clients.is_empty(),
        "nothing may map without a launch",
    );
    reap_helpers(&case);
}

#[test]
fn correlation_timeout_fails_without_duplicate_placement() {
    let mut case = Case::new("timeout");
    install_fake_app(&mut case);
    let fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario("artifacts/proc/never-maps", vec![]));
    case.save_session("correlate", &launch_session());
    write_config(&mut case, 500);

    let run = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("timeout still reports", &run);
    case.assert("exit is nonzero", run.code() == Some(1), "correlation timeout must fail");
    case.assert("failure is per window", report["report"]["failed"] == 1, &format!("{report}"));
    case.assert(
        "no window mapped",
        fake.topology().clients.is_empty(),
        "an uncorrelated window must not map",
    );

    let runs = read_process_runs(&case);
    case.assert("exactly one launch attempt", runs.len() == 1, "retries must not duplicate placement");
    reap_helpers(&case);
}

#[test]
fn dry_run_describes_launch_without_spawning() {
    let mut case = Case::new("dry-run");
    install_fake_app(&mut case);
    let fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario(CHILD_PID_FILE, vec![]));
    case.save_session("correlate", &launch_session());

    let run = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json", "--dry-run"]);
    let report = case.assert_single_json_document("dry-run report parses", &run);
    case.assert("dry-run succeeds", run.success(), &run.stderr_str());
    case.assert("dry run is labeled", report["dry_run"] == json!(true), &format!("{report}"));
    case.assert("nothing was spawned", read_process_runs(&case).is_empty(), "dry-run must not launch");
    case.assert("nothing was dispatched", fake.dispatch_count() == 0, "dry-run must not mutate");
    reap_helpers(&case);
}

#[test]
fn second_pass_after_launch_does_not_duplicate_placement() {
    let mut case = Case::new("second-pass");
    install_fake_app(&mut case);
    let _fake = FakeHyprland::spawn(&mut case, gated_mapping_scenario(CHILD_PID_FILE, vec![]));
    case.save_session("correlate", &launch_session());

    let first = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("first report parses", &first);
    case.assert(
        "first launch succeeds",
        first.success() && report["report"]["launched"] == 1,
        &format!("{report}"),
    );
    let runs_after_first = read_process_runs(&case).len();

    // The user re-saves so the session reflects the relaunched window: the
    // target set is now satisfied by the mapped identity. (Matching a
    // stale-saved session to the relaunched window without a re-save is the
    // relaunch-rematch scenario owned by bead gqg.28.)
    let mut satisfied = launch_session();
    satisfied["clients"][0]["address"] = json!("0xapp");
    satisfied["clients"][0]["stable_id"] = json!("stable-app");
    case.save_session("correlate", &satisfied);

    let second = case.run("restore", &["restore", "correlate", "--reconcile", "--report-json"]);
    let second_report = case.assert_single_json_document("second report parses", &second);
    case.assert("second pass succeeds", second.success(), &second.stderr_str());
    case.assert(
        "second pass launches nothing",
        second_report["report"]["launched"] == 0,
        &format!("{second_report}"),
    );
    case.assert(
        "second pass is unchanged",
        second_report["report"]["unchanged"] == 1,
        &format!("{second_report}"),
    );
    case.assert(
        "no duplicate placement",
        read_process_runs(&case).len() == runs_after_first,
        "a satisfied target set must not spawn again",
    );
    reap_helpers(&case);
}
