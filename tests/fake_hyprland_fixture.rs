//! Contract checks for the stateful fake-Hyprland boundary fixture (gqg.36.2).
//!
//! The fake is a scripted server: a PATH-injected `hyprctl` relay forwards
//! every request to a deterministic in-test state machine. Tests assert query
//! replies, dispatch state transitions, batch framing and rejection dialects,
//! scripted faults, delayed mapping events, and the ordered redacted fixture
//! log. CLI scenarios prove additive reconciliation, batch repairs, and
//! second-pass convergence through the real compiled binary. No live
//! compositor, wall-clock ordering, or sleep-based synchronization participates.

#![allow(unused_crate_dependencies, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/fake_hyprland.rs"]
mod fake_hyprland;
#[path = "support/mod.rs"]
mod support;

use fake_hyprland::{BatchRejection, FakeHyprland, Fault, HyprlandScenario, MappingEvent};
use serde_json::{json, Value};
use support::Case;

fn client_json(address: &str, stable: &str, workspace: i64, focus_history: i64) -> Value {
    json!({
        "address": address, "class": "foot", "title": "sentinel-title",
        "stableId": stable, "initialClass": "foot", "initialTitle": "sentinel-title",
        "workspace": {"id": workspace, "name": workspace.to_string()},
        "monitor": 0, "at": [10, 20], "size": [800, 600],
        "floating": false, "fullscreen": 0, "focusHistoryID": focus_history, "pid": 0
    })
}

fn monitor_json() -> Value {
    json!({"id": 0, "name": "DP-1", "width": 1920, "height": 1080, "transform": 0, "x": 0, "y": 0})
}

fn base_scenario(clients: Vec<Value>) -> HyprlandScenario {
    HyprlandScenario {
        clients,
        monitors: vec![monitor_json()],
        workspaces: vec![
            json!({"id": 1, "name": "1"}),
            json!({"id": 3, "name": "3"}),
            json!({"id": 4, "name": "4"}),
        ],
        focused_address: Some("0xone".to_owned()),
        version: "0.56.2-fake".to_owned(),
        batch_rejection: BatchRejection::ExitNonzero,
        mapping_events: Vec::new(),
        faults: Vec::new(),
        request_gate: None,
    }
}

fn session_json(workspace: i64) -> Value {
    json!({
        "name": "fake-fixture",
        "created_at": "2026-01-01T00:00:00Z",
        "hyprland_version": "fixture",
        "monitors": [],
        "clients": [{
            "class": "foot", "title": "sentinel-title",
            "address": "0xone", "stable_id": "stable-one",
            "initial_class": "foot", "initial_title": "sentinel-title",
            "workspace": workspace, "workspace_name": workspace.to_string(), "monitor": "DP-1",
            "at": [10, 20], "size": [800, 600], "floating": false,
            "fullscreen": 0, "focus_history_id": 0,
            "launch": {"command": "foot", "args": [], "hint": null}
        }]
    })
}

fn cli_case(name: &str, scenario: HyprlandScenario, session: &Value) -> (Case, FakeHyprland) {
    let mut case = Case::new(name);
    case.save_session("fake-fixture", session);
    let fake = FakeHyprland::spawn(&mut case, scenario);
    (case, fake)
}

#[test]
fn fake_answers_queries_from_scenario_state() {
    let mut case = Case::new("queries");
    let fake = FakeHyprland::spawn(&mut case, base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]));

    let clients = fake.call(&["clients", "-j"]);
    assert!(clients.success());
    let parsed: Value = serde_json::from_str(&clients.stdout_str()).unwrap();
    assert_eq!(parsed[0]["address"], "0xone");
    assert_eq!(parsed[0]["stableId"], "stable-one");

    let monitors = fake.call(&["monitors", "-j"]);
    assert_eq!(serde_json::from_str::<Value>(&monitors.stdout_str()).unwrap()[0]["name"], "DP-1");

    let workspaces = fake.call(&["workspaces", "-j"]);
    assert_eq!(
        serde_json::from_str::<Value>(&workspaces.stdout_str()).unwrap().as_array().unwrap().len(),
        3
    );

    let active = fake.call(&["activewindow", "-j"]);
    assert_eq!(serde_json::from_str::<Value>(&active.stdout_str()).unwrap()["address"], "0xone");

    let version = fake.call(&["version"]);
    assert!(version.stdout_str().starts_with("Hyprland 0.56.2-fake "), "version must stay parseable");

    let cursor = fake.call(&["cursorpos", "-j"]);
    assert_eq!(serde_json::from_str::<Value>(&cursor.stdout_str()).unwrap(), json!({"x": 0, "y": 0}));
}

#[test]
fn direct_dispatches_mutate_state_and_log_transitions() {
    let mut scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    scenario.focused_address = None;
    let mut case = Case::new("dispatches");
    let fake = FakeHyprland::spawn(&mut case, scenario);

    let moved = fake.call(&["dispatch", "movetoworkspacesilent", "4,address:0xone"]);
    assert!(moved.success(), "dispatch must succeed: {}", moved.stderr_str());
    let topology = fake.topology();
    assert_eq!(topology.clients[0]["workspace"]["id"], 4);

    let focused = fake.call(&["dispatch", "focuswindow", "address:0xone"]);
    assert!(focused.success());
    assert_eq!(fake.topology().focused, Some("0xone".to_owned()));

    let pinned = fake.call(&["dispatch", "pin", "address:0xone"]);
    assert!(pinned.success());
    assert_eq!(fake.topology().clients[0]["pinned"], json!(true));

    let resized = fake.call(&["dispatch", "resizewindowpixel", "exact", "640", "480,address:0xone"]);
    assert!(resized.success(), "resize must succeed: {}", resized.stderr_str());
    assert_eq!(fake.topology().clients[0]["size"], json!([640, 480]));

    let closed = fake.call(&["dispatch", "closewindow", "address:0xone"]);
    assert!(closed.success());
    assert!(fake.topology().clients.is_empty());
    assert_eq!(fake.topology().focused, None);

    let log = fake.fixture_trace();
    let transitions: Vec<&Value> = log.iter().filter(|event| event["kind"] == "dispatch").collect();
    assert!(transitions.len() >= 5, "every dispatch must be logged: {transitions:?}");
    assert!(
        transitions.iter().all(|event| event["state_before"] != event["state_after"]),
        "every applied dispatch must change topology identity: {transitions:?}"
    );
}

#[test]
fn batch_framing_applies_each_operation_in_order() {
    let mut case = Case::new("batch");
    let fake = FakeHyprland::spawn(&mut case, base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]));

    let batch = "dispatch movetoworkspacesilent 4,address:0xone ; dispatch focuswindow address:0xone";
    let reply = fake.call(&["--batch", batch]);
    assert!(reply.success(), "batch must succeed: {}", reply.stderr_str());

    let topology = fake.topology();
    assert_eq!(topology.clients[0]["workspace"]["id"], 4, "first batch op must apply");
    assert_eq!(topology.focused, Some("0xone".to_owned()), "second batch op must apply");

    let trace = fake.fixture_trace();
    let batches: Vec<&Value> = trace.iter().filter(|event| event["kind"] == "batch").collect();
    assert_eq!(batches.len(), 1, "batch framed as one request: {batches:?}");
    assert_eq!(
        batches[0]["operations"],
        json!(["movetoworkspacesilent 4,address:0xone", "focuswindow address:0xone"])
    );
}

#[test]
fn scripted_faults_fail_dispatches_without_state_mutation() {
    let mut scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    scenario.faults.push(Fault {
        matches: "movetoworkspacesilent".to_owned(),
        stderr: "fake rejected the move\n".to_owned(),
        exit_code: 1,
    });
    let mut case = Case::new("faults");
    let fake = FakeHyprland::spawn(&mut case, scenario);

    let before = fake.topology();
    let reply = fake.call(&["dispatch", "movetoworkspacesilent", "4,address:0xone"]);
    assert_eq!(reply.code, Some(1));
    assert_eq!(reply.stderr_str(), "fake rejected the move\n");
    assert_eq!(fake.topology().clients[0]["workspace"]["id"], before.clients[0]["workspace"]["id"]);

    let untouched = fake.call(&["dispatch", "focuswindow", "address:0xone"]);
    assert!(untouched.success(), "non-matching dispatches stay healthy");
}

#[test]
fn batch_rejection_dialects_separate_exit_from_text() {
    let mut scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    scenario.faults.push(Fault {
        matches: "focuswindow".to_owned(),
        stderr: "Invalid dispatcher\n".to_owned(),
        exit_code: 1,
    });
    let batch = "dispatch movetoworkspacesilent 4,address:0xone ; dispatch focuswindow address:0xone";

    let mut case = Case::new("batch-exit-nonzero");
    let fake = FakeHyprland::spawn(&mut case, scenario.clone());
    let reply = fake.call(&["--batch", batch]);
    assert_eq!(reply.code, Some(1), "default dialect must fail the batch");
    assert!(reply.stderr_str().contains("Invalid dispatcher"));
    assert_eq!(fake.topology().clients[0]["workspace"]["id"], 4, "ops before the failure stay applied");

    scenario.batch_rejection = BatchRejection::ExitZeroText;
    let mut zero_case = Case::new("batch-exit-zero");
    let zero_fake = FakeHyprland::spawn(&mut zero_case, scenario);
    let zero_reply = zero_fake.call(&["--batch", batch]);
    assert_eq!(zero_reply.code, Some(0), "zero-text dialect must exit zero");
    assert!(zero_reply.stdout_str().contains("Invalid dispatcher"), "rejection text rides stdout");
    assert_eq!(zero_fake.topology().clients[0]["workspace"]["id"], 4);
    assert_eq!(zero_fake.topology().focused, Some("0xone".to_owned()));
}

#[test]
fn mapping_events_inject_delayed_windows_after_a_dispatch() {
    let mut scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    scenario.mapping_events.push(MappingEvent {
        after_dispatch: Some(1),
        after_query: None,
        gated_on: None,
        client: client_json("0xlate", "stable-late", 5, 2),
    });
    let mut case = Case::new("mapping");
    let fake = FakeHyprland::spawn(&mut case, scenario);

    assert_eq!(fake.topology().clients.len(), 1);
    fake.call(&["dispatch", "focuswindow", "address:0xone"]);
    let topology = fake.topology();
    assert_eq!(topology.clients.len(), 2, "delayed window must appear after the scripted dispatch");
    let late = topology.clients.iter().find(|client| client["address"] == "0xlate").unwrap();
    assert_eq!(late["stableId"], "stable-late");
    assert_eq!(late["workspace"]["id"], 5);
}

#[test]
fn fixture_log_is_ordered_redacted_jsonl_with_case_identity() {
    let mut case = Case::new("log-contract");
    let case_id = case.case_id().to_owned();
    let fake = FakeHyprland::spawn(&mut case, base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]));
    fake.call(&["clients", "-j"]);
    fake.call(&["dispatch", "focuswindow", "address:0xone"]);

    let log = fake.fixture_trace();
    assert!(log.len() >= 2, "query and dispatch must be logged: {log:?}");
    let mut previous = 0;
    for event in &log {
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["case_id"], case_id.as_str(), "fixture log must carry the case identity");
        let seq = event["seq"].as_i64().expect("sequence must be numeric");
        assert!(seq > previous, "sequence must strictly increase: {log:?}");
        previous = seq;
        assert!(event["state_before"].is_string() && event["state_after"].is_string());
        let rendered = serde_json::to_string(event).unwrap();
        assert!(!rendered.contains("sentinel-title"), "titles must never be logged: {rendered}");
        assert!(event["reply"]["exit"].is_i64() && event["reply"]["stdout_bytes"].is_u64());
    }
    let kinds: Vec<&str> = log.iter().filter_map(|event| event["kind"].as_str()).collect();
    assert_eq!(kinds, vec!["clients", "dispatch"], "request kinds are logged in order");
}

#[test]
fn reconcile_against_the_fake_reports_unchanged_without_dispatches() {
    let scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 0)]);
    let session = session_json(3);
    let (mut case, fake) = cli_case("cli-unchanged", scenario, &session);
    let dispatches_before = fake.dispatch_count();

    let run = case.run("restore", &["restore", "fake-fixture", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("report is one json document", &run);
    case.assert("reconcile succeeds", run.success(), &run.stderr_str());
    case.assert("window already in place", report["report"]["unchanged"] == 1, &format!("{report}"));
    case.assert(
        "no dispatch was needed",
        fake.dispatch_count() == dispatches_before,
        "an in-place window must not be repaired",
    );
    case.assert(
        "topology untouched",
        fake.topology().clients[0]["workspace"]["id"] == 3,
        "additive reconciliation must not mutate in-place windows",
    );
}

#[test]
fn workspace_repair_uses_batch_and_second_pass_converges() {
    let scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    let session = session_json(4);
    let (mut case, fake) = cli_case("cli-converge", scenario, &session);

    let first = case.run("restore", &["restore", "fake-fixture", "--reconcile", "--report-json"]);
    case.assert("first reconcile succeeds", first.success(), &first.stderr_str());
    let report = case.assert_single_json_document("first report parses", &first);
    case.assert("window was moved", report["report"]["moved"] == 1, &format!("{report}"));
    case.assert(
        "state now carries workspace 4",
        fake.topology().clients[0]["workspace"]["id"] == 4,
        "repair must land",
    );
    let trace = fake.fixture_trace();
    let batches: Vec<&Value> = trace.iter().filter(|event| event["kind"] == "batch").collect();
    case.assert("repair flowed through batch framing", batches.len() == 1, &format!("{batches:?}"));
    let after_first = fake.dispatch_count();

    let second = case.run("restore", &["restore", "fake-fixture", "--reconcile", "--report-json"]);
    case.assert("second reconcile succeeds", second.success(), &second.stderr_str());
    let second_report = case.assert_single_json_document("second report parses", &second);
    case.assert(
        "second pass sees unchanged",
        second_report["report"]["unchanged"] == 1,
        &format!("{second_report}"),
    );
    case.assert(
        "second pass is idempotent",
        fake.dispatch_count() == after_first,
        "converged topology must not dispatch again",
    );
}

#[test]
fn exit_zero_batch_semantic_error_fails_the_repair() {
    // Hyprland's batch CLI exits zero after a completed request even when a
    // dispatcher rejected its command: the reply text is the only
    // per-command signal. The repair must fail closed instead of reporting
    // a successful move.
    let mut scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    scenario.batch_rejection = BatchRejection::ExitZeroText;
    scenario.faults.push(Fault {
        matches: "movetoworkspacesilent".to_owned(),
        stderr: "Invalid dispatcher".to_owned(),
        exit_code: 1,
    });
    let session = session_json(4);
    let (mut case, fake) = cli_case("batch-exit-zero-e2e", scenario, &session);

    let run = case.run("restore", &["restore", "fake-fixture", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("exit-zero semantic error still reports", &run);
    case.assert("exit is nonzero", run.code() == Some(1), "an unconfirmed repair must fail");
    case.assert("failure is per window", report["report"]["failed"] == 1, &format!("{report}"));
    case.assert(
        "diagnostic carries the compositor reply",
        report["report"]["windows"][0]["message"].as_str().unwrap().contains("Invalid dispatcher"),
        &format!("{report}"),
    );
    case.assert(
        "topology is preserved on the failed repair",
        fake.topology().clients[0]["workspace"]["id"] == 3,
        "no mutation may survive the rejected batch",
    );
}

#[test]
fn faulted_repair_reports_failure_and_preserves_topology() {
    let mut scenario = base_scenario(vec![client_json("0xone", "stable-one", 3, 1)]);
    scenario.faults.push(Fault {
        matches: "movetoworkspacesilent".to_owned(),
        stderr: "fake rejected the move\n".to_owned(),
        exit_code: 1,
    });
    let session = session_json(4);
    let (mut case, fake) = cli_case("cli-fault", scenario, &session);

    let run = case.run("restore", &["restore", "fake-fixture", "--reconcile", "--report-json"]);
    let report = case.assert_single_json_document("failed repair still reports", &run);
    case.assert("exit stays nonzero", run.code() == Some(1), "partial failure must exit nonzero");
    case.assert("failure is reported per window", report["report"]["failed"] == 1, &format!("{report}"));
    case.assert(
        "topology is preserved on failure",
        fake.topology().clients[0]["workspace"]["id"] == 3,
        "no mutation may survive a failed repair",
    );
}
