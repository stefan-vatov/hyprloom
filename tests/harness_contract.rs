//! Contract checks for the isolated CLI/e2e harness that audit fixtures share (gqg.36.1).
//!
//! These tests prove the harness contract itself: real compiled binary under
//! isolated XDG roots, ordered redacted JSONL evidence, output-channel
//! contracts, failure retention forensics, readiness signals, and state
//! snapshot hashes. They never touch a live desktop or systemd manager.

#![allow(unused_crate_dependencies, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/mod.rs"]
mod support;

use serde_json::{json, Value};
use support::Case;

fn reporting_session() -> Value {
    json!({
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
    })
}

fn reporting_clients() -> Value {
    json!([{
        "address": "0xfixture", "stableId": "fixture-window-1",
        "class": "foot", "title": "Project — <editor>",
        "initialClass": "foot", "initialTitle": "foot",
        "workspace": {"id": 3, "name": "3"}, "monitor": 0,
        "at": [10, 20], "size": [800, 600], "floating": false,
        "fullscreen": 0, "focusHistoryID": 0, "pid": 0
    }])
}

/// A reconcile case wired with the shared reporting fixture.
fn reporting_case(name: &str) -> Case {
    let mut case = Case::new(name);
    case.install_fake("hyprctl", include_str!("fixtures/reporting-hyprctl.sh"));
    let root = case.root().display().to_string();
    case.set_env("HYPRLOOM_REPORT_FIXTURE", &root);
    case.save_session("report-fixture", &reporting_session());
    let clients = serde_json::to_vec(&reporting_clients()).unwrap();
    case.write_file("clients.json", &clients);
    case
}

#[test]
fn harness_runs_real_binary_in_isolated_roots_and_cleans_up_on_success() {
    let case_id;
    let root_display;
    {
        let mut case = Case::new("list-success");
        case_id = case.case_id().to_owned();
        root_display = case.root().to_path_buf();

        let run = case.run("list", &["list"]);
        case.assert("exit is success", run.success(), "list must exit 0 on an empty store");
        case.assert(
            "empty-store human output",
            run.stdout_str() == "No saved sessions.\n",
            &format!("stdout was: {:?}", run.stdout_str()),
        );
        case.assert(
            "no stderr on success",
            run.stderr_str().is_empty(),
            &format!("stderr was: {:?}", run.stderr_str()),
        );

        let events = case.read_trace();
        let cli_runs = events.iter().filter(|event| event["component"] == "cli").count();
        case.assert(
            "trace has case start plus one run",
            events[0]["operation"] == "case-start" && cli_runs == 1,
            &format!("events: {events:?}"),
        );
        case.assert(
            "case start records fixture version",
            events[0]["fixture_version"] == support::FIXTURE_VERSION,
            "fixture version must be stamped on the case",
        );
        case.assert(
            "run event is normalized",
            events[1]["argv"] == json!(["hyprloom", "list"])
                && events[1]["seq"] == 2
                && events[1]["component"] == "cli"
                && events[1]["operation"] == "list"
                && events[1]["exit_status"] == 0,
            &format!("run event: {}", events[1]),
        );
        assert_eq!(events[1]["case_id"], case_id.as_str());
    }
    // Dropping the case after only passing assertions removes every artifact.
    assert!(
        !root_display.exists(),
        "successful case must clean its directory: {}",
        root_display.display()
    );
}

#[test]
fn trace_evidence_is_redacted_bounded_and_normalized() {
    let mut case = Case::new("evidence-shape");
    case.set_env("HYPRLOOM_TEST_TOKEN", "do-not-record");

    let run = case.run("list", &["list"]);
    case.assert("list succeeded", run.success(), &run.stderr_str());

    let events = case.read_trace();
    let start = &events[0];
    let env = start["env"].as_object().expect("case start must record env");
    let keys: Vec<&String> = env.keys().collect();
    assert_eq!(keys, vec!["HOME", "PATH", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"]);
    assert!(
        !serde_json::to_string(env).unwrap().contains("do-not-record"),
        "secret fixture values must be redacted: {env:?}"
    );
    assert_eq!(env["HOME"], json!("<case-root>/home"));
    let run_event = &events[1];
    assert_eq!(
        run_event["env_keys"],
        json!(["HOME", "PATH", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"])
    );
    let rendered = serde_json::to_string(&events).unwrap();
    assert!(
        !rendered.contains(&case.root().to_string_lossy().into_owned()),
        "absolute case-root paths must be normalized to <case-root>: {rendered}"
    );
    assert!(run_event["state_hash_before"] != Value::Null && run_event["state_hash_after"] != Value::Null);
}

#[test]
fn output_channel_contracts_are_enforced_through_the_harness() {
    let mut case = reporting_case("report-contract");

    let human = case.run("restore", &["restore", "report-fixture", "--reconcile"]);
    case.assert("human reconcile succeeds", human.success(), &human.stderr_str());
    case.assert(
        "human output is the documented summary line",
        human.stdout_str() == "Reconciled session 'report-fixture':\n  1 unchanged, 0 moved, 0 launched, 0 extra left alone, 0 skipped, 0 failed\n",
        &format!("stdout was: {:?}", human.stdout_str()),
    );

    let reported = case.run("restore", &["restore", "report-fixture", "--reconcile", "--report-json"]);
    case.assert("report reconcile succeeds", reported.success(), &reported.stderr_str());
    let report = case.assert_single_json_document("report is one json document", &reported);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "reconcile");
    assert_eq!(report["report"]["unchanged"], 1);
    assert_eq!(report["report"]["windows"][0]["status"], "unchanged");

    let fatal = case.run("restore", &["restore", "missing", "--reconcile", "--report-json"]);
    case.assert_fatal_pre_report("missing session is fatal and empty on stdout", &fatal);

    let events = case.read_trace();
    let assertions: Vec<&Value> = events
        .iter()
        .filter(|e| e["component"] == "harness" && e["operation"] == "assert")
        .collect();
    assert!(assertions.len() >= 5, "assertions must be traced: {assertions:?}");
    assert!(
        assertions.iter().all(|a| a["outcome"] == "pass"),
        "all traced assertions must pass here: {assertions:?}"
    );
}

#[test]
fn failing_assertions_retain_artifacts_and_print_forensics() {
    let retained_root = std::panic::catch_unwind(|| {
        let mut case = Case::new("retention");
        let run = case.run("list", &["list"]);
        case.assert("deliberate failure", false, &format!("run stdout was {:?}", run.stdout_str()));
    })
    .expect_err("the failing assertion must panic");

    let message = retained_root.downcast_ref::<String>().expect("panic payload is a string").clone();
    // The forensic report is printed to stderr by the drop guard; the panic
    // payload itself carries the same context for the test framework.
    assert!(message.contains("case retention"), "panic must name the case: {message}");
    assert!(message.contains("deliberate failure"), "panic must name the failed assertion: {message}");
    assert!(message.contains("trace tail"), "panic must include the trace tail: {message}");
    assert!(message.contains("retained at"), "panic must point at retained artifacts: {message}");

    let artifact_dir = message
        .split("retained at ")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .expect("retained path must follow the marker");
    let retained = std::path::PathBuf::from(artifact_dir);
    assert!(retained.exists(), "retained artifacts must exist at {artifact_dir}");
    assert!(retained.join("artifacts/trace.jsonl").exists(), "trace must be retained");
    let _ = std::fs::remove_dir_all(&retained);
}

#[test]
fn readiness_signals_are_explicit_and_bounded() {
    let mut case = Case::new("readiness");

    case.write_file("signals/ready", b"ok");
    let arrived = case.wait_for_signal("signals/ready", std::time::Duration::from_millis(500));
    case.assert("existing signal arrives", arrived, "a present signal must resolve immediately");

    let absent = case.wait_for_signal("signals/never", std::time::Duration::from_millis(60));
    case.assert("absent signal times out", !absent, "an absent signal must hit the deadline");

    let events = case.read_trace();
    let waits: Vec<&Value> = events.iter().filter(|e| e["operation"] == "wait-signal").collect();
    assert_eq!(waits.len(), 2, "both waits must be traced: {waits:?}");
    assert_eq!(waits[0]["outcome"], json!("arrived"));
    assert_eq!(waits[1]["outcome"], json!("timeout"));
}

#[test]
fn state_hashes_capture_before_and_after_evidence() {
    let mut case = reporting_case("state-hashes");
    let before = case.state_hash();

    let run = case.run("list", &["list"]);
    case.assert("list succeeds", run.success(), &run.stderr_str());
    let after = case.state_hash();
    assert_ne!(before, after, "mutating the store must change the state manifest hash");
    case.assert("state hash is sha256-prefixed", after.starts_with("sha256:"), &after);

    let events = case.read_trace();
    let run_event = events.iter().find(|e| e["operation"] == "list").expect("list is traced");
    assert_eq!(run_event["state_hash_before"], json!(before));
    assert_eq!(run_event["state_hash_after"], json!(after));
}
