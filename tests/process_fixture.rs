//! Contract checks for the controlled fake-app process fixture (gqg.36.3.1).
//!
//! The helper is a real executable with scripted lifecycles: one-shot runs
//! that log their exact argv and exit, stay-alive runs blocked on a stop
//! FIFO, and daemonized runs whose detached child is reaped by the guard.
//! Coordination uses readiness files and FIFO release, never sleeps; the
//! guard verifies pids are gone with a bounded wait and a kill fallback,
//! including after a failed case.

#![allow(unused_crate_dependencies, clippy::unwrap_used, clippy::expect_used)]

#[path = "support/process_fixture.rs"]
mod process_fixture;
#[path = "support/mod.rs"]
mod support;

use process_fixture::{install_fake_app, pid_alive, read_process_runs, release_all, run_once, HelperHandle};
use std::time::{Duration, Instant};
use support::Case;

const RELEASE_WAIT: Duration = Duration::from_secs(5);

fn wait_until_gone(pid: u32) -> bool {
    let deadline = Instant::now() + RELEASE_WAIT;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !pid_alive(pid)
}

#[test]
fn one_shot_run_logs_exact_argv_array() {
    let mut case = Case::new("one-shot");
    install_fake_app(&mut case);

    let output = run_once(&case, &["run", "hello world", "second arg", "-x"]);
    case.assert("one-shot exits cleanly", output.status.success(), &format!("{output:?}"));

    let runs = read_process_runs(&case);
    case.assert("one invocation is logged", runs.len() == 1, &format!("{runs:?}"));
    case.assert(
        "argv stays an ordered array",
        runs[0].argv == vec!["run".to_owned(), "hello world".to_owned(), "second arg".to_owned(), "-x".to_owned()],
        "spaces must never split arguments",
    );
    case.assert("synthetic pid is recorded", runs[0].pid > 0, "identity must be recorded");
}

#[test]
fn exit_mode_propagates_the_scripted_status() {
    let mut case = Case::new("exit-mode");
    install_fake_app(&mut case);

    let output = run_once(&case, &["exit", "7", "payload"]);
    case.assert("helper exits with scripted code", output.status.code() == Some(7), &format!("{output:?}"));

    let runs = read_process_runs(&case);
    case.assert("run is logged before exiting", runs.len() == 1, &format!("{runs:?}"));
    case.assert("full argv is preserved", runs[0].argv.len() == 3, &format!("{:?}", runs[0].argv));
}

#[test]
fn stay_mode_blocks_until_released() {
    let mut case = Case::new("stay-mode");
    install_fake_app(&mut case);

    let mut handle = HelperHandle::spawn(&case, &["stay", "serve", "a b"]);
    let ready = case.wait_for_signal("artifacts/proc/ready", Duration::from_secs(5));
    case.assert("helper signals readiness", ready, "stay mode must write the readiness file");
    case.assert("helper is alive before release", handle.is_alive(), "stay mode must block");

    handle.stop(RELEASE_WAIT);
    case.assert("helper is gone after release", !handle.is_alive(), "release must end the helper");
    case.assert("exit status is clean", handle.exit_cleanly(), "FIFO release must exit 0");

    let runs = read_process_runs(&case);
    case.assert(
        "stay run is logged",
        runs.len() == 1 && runs[0].argv == vec!["stay", "serve", "a b"],
        &format!("{runs:?}"),
    );
}

#[test]
fn daemon_mode_detaches_a_child_that_the_guard_reaps() {
    let mut case = Case::new("daemon-mode");
    install_fake_app(&mut case);

    let parent = run_once(&case, &["daemon", "serve", "x y"]);
    case.assert("daemon parent exits immediately", parent.status.success(), &format!("{parent:?}"));
    let ready = case.wait_for_signal("artifacts/proc/ready", Duration::from_secs(5));
    case.assert(
        "detached child signals readiness",
        ready,
        "the daemon child must write the readiness file",
    );

    let mut handle = HelperHandle::adopt_detached(&case);
    case.assert("child is alive before release", handle.is_alive(), "daemon child must stay up");

    handle.stop(RELEASE_WAIT);
    case.assert("child is gone after release", !handle.is_alive(), "guard must reap the daemon child");

    let runs = read_process_runs(&case);
    case.assert("parent and child both logged", runs.len() == 2, &format!("{runs:?}"));
    case.assert(
        "child argv is the full scripted argv",
        runs[1].argv == vec!["daemon", "serve", "x y"],
        &format!("{:?}", runs[1].argv),
    );
    case.assert(
        "parent and child have distinct synthetic identities",
        runs[0].pid != runs[1].pid,
        "daemonization must produce a new pid identity",
    );
}

#[test]
fn drop_reaps_a_stay_helper_without_an_explicit_stop() {
    let mut case = Case::new("drop-guard");
    install_fake_app(&mut case);

    let pid;
    {
        let handle = HelperHandle::spawn(&case, &["stay"]);
        case.wait_for_signal("artifacts/proc/ready", Duration::from_secs(5));
        pid = handle.pid();
    } // handle dropped without stop(): Drop must release and reap.
    case.assert("dropped helper is reaped", wait_until_gone(pid), "the guard must reap on drop");
}

#[test]
fn cleanup_survives_a_failed_case_and_release_all_reaps_lost_helpers() {
    let failed = std::panic::catch_unwind(|| {
        let mut case = Case::new("failing-case");
        install_fake_app(&mut case);
        let handle = HelperHandle::spawn(&case, &["stay"]);
        case.wait_for_signal("artifacts/proc/ready", Duration::from_secs(5));
        let pid = handle.pid();
        std::mem::forget(handle); // simulate a lost handle; no Drop runs
        case.assert("deliberate failure", false, &format!("pid {pid} was leaked on purpose"));
    })
    .expect_err("the case must fail");

    let message = failed.downcast_ref::<String>().expect("panic payload is a string");
    assert!(message.contains("failing-case"), "failure must name the case: {message}");

    let artifact_dir = message
        .split("retained at ")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .expect("retained path must follow the marker");
    let retained_log = std::path::Path::new(artifact_dir).join("artifacts/proc/log");
    assert!(retained_log.exists(), "process evidence must be retained at {retained_log:?}");

    let leaked = release_all(std::path::Path::new(artifact_dir));
    assert!(!leaked.is_empty(), "the leaked helper pid must be discoverable from the log");
    assert!(
        leaked.iter().all(|pid| wait_until_gone(*pid)),
        "all leaked helpers must be reaped: {leaked:?}"
    );
    let _ = std::fs::remove_dir_all(artifact_dir);
}
