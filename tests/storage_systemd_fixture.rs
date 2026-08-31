//! Contract checks for storage, locking, and systemd fault fixtures
//! (gqg.36.4) plus the autosave retention policy (gqg.30).
//!
//! The fake systemctl records ordered argv-array invocations with scripted
//! state answers and per-subcommand failures, so autosave lifecycle tests
//! run deterministically with zero contact with the real user manager. The
//! request gate in the fake-Hyprland boundary holds a CLI mid-operation so
//! operation-lock contention can be choreographed causally, without sleeps.
//! Crash-seeded states (orphaned atomic-write temporaries) are expressible
//! as plain isolated filesystem seeds.

#![allow(unused_crate_dependencies, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/fake_hyprland.rs"]
mod fake_hyprland;
#[path = "support/mod.rs"]
mod support;
#[path = "support/systemd_fixture.rs"]
mod systemd_fixture;

use fake_hyprland::{FakeHyprland, HyprlandScenario};
use hyprloom::session::rotate_autosaves;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;
use support::Case;
use systemd_fixture::{clear_failure, install_systemctl, is_private_file, read_systemctl_log, script_failure, script_state, seed_legacy_units};

fn empty_desktop_scenario() -> HyprlandScenario {
    HyprlandScenario {
        clients: vec![],
        monitors: vec![json!({"id": 0, "name": "DP-1", "width": 1920, "height": 1080, "transform": 0})],
        workspaces: vec![],
        focused_address: None,
        version: "0.56.2-fake".to_owned(),
        batch_rejection: fake_hyprland::BatchRejection::ExitNonzero,
        mapping_events: vec![],
        faults: vec![],
        request_gate: None,
    }
}

fn existing_window_scenario() -> HyprlandScenario {
    let mut scenario = empty_desktop_scenario();
    scenario.clients = vec![json!({
        "address": "0xfixture", "class": "foot", "title": "sentinel-title",
        "stableId": "stable-one", "initialClass": "foot", "initialTitle": "sentinel-title",
        "workspace": {"id": 3, "name": "3"}, "monitor": 0,
        "at": [10, 20], "size": [800, 600], "floating": false,
        "fullscreen": 0, "focusHistoryID": 0, "pid": 0
    })];
    scenario
}

fn window_session() -> Value {
    json!({
        "name": "held",
        "created_at": "2026-01-01T00:00:00Z",
        "hyprland_version": "fixture",
        "monitors": [],
        "clients": [{
            "class": "foot", "title": "sentinel-title",
            "address": "0xfixture", "stable_id": "stable-one",
            "initial_class": "foot", "initial_title": "sentinel-title",
            "workspace": 3, "workspace_name": "3", "monitor": "DP-1",
            "at": [10, 20], "size": [800, 600], "floating": false,
            "fullscreen": 0, "focus_history_id": 0,
            "launch": {"command": "foot", "args": [], "hint": null}
        }]
    })
}

fn seed_autosave_store(case: &Case, count: usize) -> BTreeMap<String, u64> {
    use std::fs;
    let sessions_dir = case.root().join("data/hyprloom/sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let mut manifest = BTreeMap::new();
    for ordinal in 1..=count {
        let name = format!("autosave-20260101T000000000-0001-{ordinal:06}");
        let session = json!({
            "name": name,
            "created_at": "2026-01-01T00:00:00Z",
            "hyprland_version": "fixture",
            "monitors": [],
            "clients": [{
                "class": "foot", "title": "sentinel-title",
                "address": format!("0x{ordinal}"), "stable_id": format!("stable-{ordinal}"),
                "initial_class": "foot", "initial_title": "sentinel-title",
                "workspace": 3, "workspace_name": "3", "monitor": "DP-1",
                "at": [10, 20], "size": [800, 600], "floating": false,
                "fullscreen": 0, "focus_history_id": 0,
                "launch": {"command": "foot", "args": [], "hint": null}
            }]
        });
        let path = sessions_dir.join(format!("{name}.json"));
        fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&path, permissions).unwrap();
        manifest.insert(format!("{name}.json"), content_hash(&path));
    }
    manifest
}

fn content_hash(path: &std::path::Path) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    let bytes = std::fs::read(path).unwrap_or_default();
    hasher.write(&bytes);
    hasher.finish()
}

fn store_manifest(case: &Case) -> BTreeMap<String, u64> {
    use std::fs;
    let sessions_dir = case.root().join("data/hyprloom/sessions");
    let entries = fs::read_dir(sessions_dir).unwrap_or_else(|error| panic!("store must be listable: {error}"));
    entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .map(|path| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (name, content_hash(&path))
        })
        .collect()
}

fn retain_config(case: &mut Case, retain: usize) {
    let config = format!("[general]\nautosave_retain = {retain}\n");
    case.write_file("config/hyprloom/config.toml", config.as_bytes());
}

#[test]
fn autosave_install_writes_units_under_isolated_config() {
    let mut case = Case::new("autosave-install");
    install_systemctl(&mut case);

    let run = case.run("autosave", &["autosave", "--install"]);
    case.assert("install succeeds", run.success(), &run.stderr_str());
    case.assert("output announces creation", run.stdout_str().contains("Created:"), &run.stdout_str());

    let service = case.root().join("config/systemd/user/hyprloom-autosave.service");
    let timer = case.root().join("config/systemd/user/hyprloom-autosave.timer");
    case.assert("service unit written", service.exists(), "install must create the unit");
    case.assert("timer unit written", timer.exists(), "install must create the timer");
    case.assert(
        "units are regular files",
        service.is_file() && timer.is_file(),
        "units must be regular files",
    );
    let timer_text = std::fs::read_to_string(&timer).unwrap_or_default();
    case.assert(
        "timer schedules the documented cadence",
        timer_text.contains("OnUnitActiveSec=10min"),
        &timer_text,
    );
    case.assert(
        "no systemctl contact during install",
        read_systemctl_log(&case).is_empty(),
        "install prints instructions instead of calling the manager",
    );
}

#[test]
fn autosave_status_reports_state_from_fake_systemctl() {
    let mut case = Case::new("autosave-status");
    install_systemctl(&mut case);

    let uninstalled = case.run("autosave", &["autosave"]);
    case.assert("status succeeds without units", uninstalled.success(), &uninstalled.stderr_str());
    case.assert(
        "uninstalled state is reported",
        uninstalled.stdout_str().contains("Autosave is not configured."),
        &uninstalled.stdout_str(),
    );

    let install = case.run("autosave", &["autosave", "--install"]);
    case.assert("install succeeds", install.success(), &install.stderr_str());

    script_state(&mut case, 1);
    let inactive = case.run("autosave", &["autosave"]);
    case.assert(
        "installed-but-inactive is reported",
        inactive.stdout_str().contains("Autosave timer is installed but not active."),
        &inactive.stdout_str(),
    );

    script_state(&mut case, 0);
    let active = case.run("autosave", &["autosave"]);
    case.assert("status succeeds", active.success(), &active.stderr_str());
    case.assert(
        "active state is reported",
        active.stdout_str().contains("Autosave is active (every 10min)."),
        &active.stdout_str(),
    );

    let verbs: Vec<String> = read_systemctl_log(&case).into_iter().map(|call| call.verb).collect();
    case.assert(
        "state queries hit the fake manager",
        verbs.iter().filter(|verb| verb.as_str() == "is-active").count() >= 2 && verbs.iter().all(|verb| verb.as_str() != "enable"),
        &format!("{verbs:?}"),
    );
}

#[test]
fn scripted_disable_failure_refuses_uninstall_and_retains_units() {
    let mut case = Case::new("disable-failure");
    install_systemctl(&mut case);
    let install = case.run("autosave", &["autosave", "--install"]);
    case.assert("install seeds current units", install.success(), &install.stderr_str());
    seed_legacy_units(&case);
    script_failure(&mut case, "disable");

    let run = case.run("autosave", &["autosave", "--uninstall"]);
    case.assert("uninstall is refused on disable failure", run.code() == Some(1), &run.stdout_str());
    case.assert(
        "no success text is printed",
        !run.stdout_str().contains("Autosave timer removed."),
        &run.stdout_str(),
    );
    case.assert(
        "the stage-and-unit error names the timer",
        run.stderr_str().contains("could not disable autosave timer"),
        &run.stderr_str(),
    );
    case.assert(
        "current units are retained",
        case.root().join("config/systemd/user/hyprloom-autosave.timer").exists(),
        "no unit may be removed while a disable is unconfirmed",
    );
    case.assert(
        "legacy units are retained",
        case.root().join("config/systemd/user/hyprflow-autosave.timer").exists(),
        "no unit may be removed while a disable is unconfirmed",
    );
    let verbs: Vec<String> = read_systemctl_log(&case).into_iter().map(|call| call.verb).collect();
    case.assert(
        "exactly the first required disable was attempted",
        verbs == vec!["disable".to_owned()],
        &format!("{verbs:?}"),
    );

    // Idempotent retry once the manager cooperates again.
    clear_failure(&mut case);
    let retry = case.run("autosave", &["autosave", "--uninstall"]);
    case.assert("retry succeeds", retry.success(), &retry.stderr_str());
    case.assert(
        "retry prints the success contract",
        retry.stdout_str().contains("Autosave timer removed."),
        &retry.stdout_str(),
    );
    case.assert(
        "units are removed after confirmed disables",
        !case.root().join("config/systemd/user/hyprloom-autosave.timer").exists(),
        "confirmed disables must allow the removal phase",
    );
}

#[test]
fn successful_uninstall_confirms_disables_before_removal() {
    let mut case = Case::new("uninstall-ok");
    install_systemctl(&mut case);
    let install = case.run("autosave", &["autosave", "--install"]);
    case.assert("install seeds current units", install.success(), &install.stderr_str());
    seed_legacy_units(&case);

    let run = case.run("autosave", &["autosave", "--uninstall"]);
    case.assert("uninstall succeeds", run.success(), &run.stderr_str());
    case.assert(
        "success text is on stdout",
        run.stdout_str().contains("Autosave timer removed."),
        &run.stdout_str(),
    );
    let verbs: Vec<String> = read_systemctl_log(&case).into_iter().map(|call| call.verb).collect();
    case.assert(
        "disables are confirmed for current and legacy timers in order",
        verbs == vec!["disable".to_owned(), "disable".to_owned()],
        &format!("{verbs:?}"),
    );
    case.assert(
        "all units are gone",
        !case.root().join("config/systemd/user/hyprloom-autosave.timer").exists()
            && !case.root().join("config/systemd/user/hyprflow-autosave.timer").exists(),
        "confirmed uninstall removes every unit",
    );
}

#[test]
fn lock_contention_serializes_cli_processes() {
    let mut case = Case::new("lock-contention");
    let mut scenario = existing_window_scenario();
    scenario.request_gate = Some("artifacts/request-gate".to_owned());
    let fake = FakeHyprland::spawn(&mut case, scenario);
    case.save_session("held", &window_session());
    case.write_file("artifacts/request-gate", b"closed");

    // Process A takes the operation lock and blocks inside its first
    // hyprctl request because the gate file exists.
    let mut process_a = case.spawn_run("restore", &["restore", "held", "--reconcile"]);
    let queue = case.root().join("artifacts/fake-hyprland/queue");
    let reached = wait_for_file(&queue, Duration::from_secs(5));
    case.assert(
        "process A reached the gated boundary",
        reached,
        "the relay queue must show the held request",
    );

    // Process B cannot finish while A holds the lock: this is a causal
    // impossibility, not a timing observation, because the gate keeps A
    // mid-operation and the lock serializes B behind it.
    let mut process_b = case.spawn_run("list", &["list"]);
    case.assert("process B waits for the lock", process_b.is_running(), "B must block until A releases");

    case.remove_file("artifacts/request-gate");
    let output_a = case.wait_run(&mut process_a, Duration::from_secs(30));
    let output_b = case.wait_run(&mut process_b, Duration::from_secs(30));
    let out_a = output_a.expect("process A must finish after the gate opens");
    let out_b = output_b.expect("process B must finish after A releases the lock");
    case.assert("process A succeeds", out_a.success(), &out_a.stderr_str());
    case.assert("process B succeeds", out_b.success(), &out_b.stderr_str());
    case.assert(
        "process B saw the store after waiting",
        out_b.stdout_str().contains("held"),
        &format!("{:?}", out_b.stdout_str()),
    );
    case.assert(
        "the held window is untouched",
        fake.topology().clients.len() == 1,
        "no mutation may escape",
    );
}

fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

#[test]
fn orphan_temp_seeding_is_expressible_for_crash_points() {
    let mut case = Case::new("orphan-seed");
    install_systemctl(&mut case);
    case.save_session("held", &window_session());

    // Seed exactly what an interrupted atomic write would have left behind:
    // a hidden temp file matching the atomic-write grammar, private mode.
    let orphan = case.root().join("data/hyprloom/sessions/.held.json.4242.1.tmp");
    case.write_file("data/hyprloom/sessions/.held.json.4242.1.tmp", b"{}");
    let mut permissions = std::fs::metadata(&orphan).unwrap().permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&orphan, permissions).unwrap();
    case.assert(
        "orphan is a private file",
        is_private_file(&orphan),
        "seed must match the crash-point grammar",
    );

    let run = case.run("list", &["list"]);
    case.assert("listing still succeeds", run.success(), &run.stderr_str());
    case.assert("valid session remains listed", run.stdout_str().contains("held"), &run.stdout_str());
}

#[test]
fn rotation_keeps_exactly_the_configured_newest_count() {
    let mut case = Case::new("rotation-unit");
    let manifest = seed_autosave_store(&case, 6);
    case.assert("seeded six autosaves", manifest.len() == 6, "setup");

    let pruned = rotate_autosaves(&case.root().join("data/hyprloom/sessions"), 4).unwrap();
    case.assert("pruned exactly the excess", pruned == 2, &format!("pruned {pruned}"));
    let after = store_manifest(&case);
    case.assert("four survivors", after.len() == 4, &format!("{:?}", after.keys()));
    case.assert(
        "survivors are the newest four",
        !after.contains_key("autosave-20260101T000000000-0001-000001.json")
            && !after.contains_key("autosave-20260101T000000000-0001-000002.json")
            && after.contains_key("autosave-20260101T000000000-0001-000003.json")
            && after.contains_key("autosave-20260101T000000000-0001-000006.json"),
        &format!("{:?}", after.keys()),
    );
    case.assert(
        "retain zero clears the store",
        rotate_autosaves(&case.root().join("data/hyprloom/sessions"), 0).unwrap() == 4,
        "retain=0 must delete every autosave",
    );
}

#[test]
fn cli_autosave_honors_retain_above_the_legacy_cap() {
    let mut case = Case::new("retain-1001");
    install_systemctl(&mut case);
    let _fake = FakeHyprland::spawn(&mut case, empty_desktop_scenario());
    let manifest_before = seed_autosave_store(&case, 1002);
    retain_config(&mut case, 1001);

    let run = case.run("autosave", &["autosave", "--now"]);
    case.assert("autosave --now succeeds", run.success(), &run.stderr_str());

    let manifest_after = store_manifest(&case);
    case.assert(
        "exactly 1001 snapshots remain (1002 seeds + 1 capture - rotation to 1001)",
        manifest_after.len() == 1001,
        &format!("{}", manifest_after.len()),
    );

    // Deleted: precisely the two oldest seeds.
    case.assert(
        "oldest seed deleted",
        !manifest_after.contains_key("autosave-20260101T000000000-0001-000001.json"),
        "the configured policy keeps 1001; the two oldest must go",
    );
    case.assert(
        "second oldest seed deleted",
        !manifest_after.contains_key("autosave-20260101T000000000-0001-000002.json"),
        "the configured policy keeps 1001; the two oldest must go",
    );
    case.assert(
        "third oldest seed survives",
        manifest_after.contains_key("autosave-20260101T000000000-0001-000003.json"),
        "the configured policy keeps 1001; the third oldest must survive",
    );

    // No silent normalization: the captured document plus every survivor is
    // content-identical to what was seeded.
    let new_captures: Vec<&String> = manifest_after.keys().filter(|name| !manifest_before.contains_key(*name)).collect();
    case.assert("exactly one new capture exists", new_captures.len() == 1, &format!("{new_captures:?}"));
    let mut survivors_verified = 0;
    for (name, hash) in &manifest_before {
        if let Some(after_hash) = manifest_after.get(name) {
            assert_eq!(after_hash, hash, "survivor content must be byte-identical: {name}");
            survivors_verified += 1;
        } else {
            let deleted = name == "autosave-20260101T000000000-0001-000001.json" || name == "autosave-20260101T000000000-0001-000002.json";
            case.assert("only the two oldest seeds were deleted", deleted, &format!("unexpected deletion: {name}"));
        }
    }
    case.assert(
        "exactly the policy-authorized 1000 seeds survive",
        survivors_verified == 1000,
        &format!("{survivors_verified}"),
    );
}
