//! Controlled fake-app process fixture (gqg.36.3.1).
//!
//! `fake-app` is a real executable installed into the case's controlled PATH.
//! Its lifecycles are scripted: one-shot runs log their exact argv and exit
//! with a scripted status, stay-alive runs block on a stop FIFO, and
//! daemonized runs detach a child that outlives the parent. Every invocation
//! appends its synthetic pid identity and argv as an ordered array to a
//! case-local log, so spaced arguments are never flattened. Coordination is
//! causal: a readiness file signals setup completion and releasing the FIFO
//! ends blocked helpers. `HelperHandle` reaps owned and detached helpers
//! with a bounded wait and a kill fallback; `release_all` reaps helpers
//! whose handles were lost, which is the cleanup path for failed cases.

#![allow(dead_code, clippy::missing_panics_doc, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use crate::support::Case;

/// Case-relative directory holding the process fixture's runtime state.
pub const PROC_DIR: &str = "artifacts/proc";
const RELEASE_WAIT: Duration = Duration::from_secs(5);
const ALIVE_POLL: Duration = Duration::from_millis(20);

/// The controlled helper executable. `$FAKE_APP_LOG`, `$FAKE_APP_STOP`, and
/// `$FAKE_APP_READY` are case-local paths. Modes: `stay` blocks on the FIFO,
/// `exit <code>` exits with a scripted status, `daemon` detaches a child
/// that logs its own identity and then blocks like `stay`.
const FAKE_APP_SH: &str = r#"#!/bin/sh
set -u
log=${FAKE_APP_LOG:?fake-app requires FAKE_APP_LOG}
stop=${FAKE_APP_STOP:?fake-app requires FAKE_APP_STOP}
ready=${FAKE_APP_READY:?fake-app requires FAKE_APP_READY}

record() {
  printf 'PID %s\n' "$$"
  printf 'ARGV %s\n' "$#"
  for arg do
    printf '%s\n' "$arg"
  done
} >>"$log"

case "${1:-}" in
  run)
    record "$@"
    printf '%s\n' "$$" >"$ready"
    exit 0
    ;;
  stay)
    record "$@"
    printf '%s\n' "$$" >"$ready"
    exec 3<"$stop"
    read -r line <&3
    exit 0
    ;;
  exit)
    record "$@"
    printf '%s\n' "$$" >"$ready"
    exit "${2:-0}"
    ;;
  window)
    record "$@"
    child_pid=${FAKE_APP_CHILD_PID:?fake-app window requires FAKE_APP_CHILD_PID}
    sh -c 'printf "%s\n" "$$" >"$1"; exec 3<"$2"; read -r line <&3' sh "$child_pid" "$stop" </dev/null >/dev/null 2>&1 &
    tries=0
    while [ ! -s "$child_pid" ] && [ "$tries" -lt 500 ]; do
      tries=$((tries + 1))
      sleep 0.01
    done
    printf '%s\n' "$$" >"$ready"
    exec 3<"$stop"
    read -r line <&3
    exit 0
    ;;
  daemon)
    record "$@"
    setsid sh -c '
      l=$1; s=$2; r=$3; shift 3
      printf "PID %s\n" "$$" >>"$l"
      printf "ARGV %s\n" "$#" >>"$l"
      for arg do
        printf "%s\n" "$arg" >>"$l"
      done
      printf "%s\n" "$$" >"$r"
      exec 3<"$s"
      read -r line <&3
    ' sh "$log" "$stop" "$ready" "$@" </dev/null >/dev/null 2>&1 &
    exit 0
    ;;
  *)
    record "$@"
    printf 'fake-app: unknown mode %s\n' "${1:-}" >&2
    exit 2
    ;;
esac
"#;

/// One logged helper invocation: synthetic identity plus exact argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRun {
    /// The helper's pid at logging time.
    pub pid: u32,
    /// The full invocation argv, never flattened.
    pub argv: Vec<String>,
}

/// Install the helper, its FIFO, and its case environment entries.
pub fn install_fake_app(case: &mut Case) {
    let root = case.root().to_path_buf();
    let proc_dir = root.join(PROC_DIR);
    fs::create_dir_all(&proc_dir).unwrap();
    let fifo = proc_dir.join("stop.fifo");
    let _ = fs::remove_file(&fifo);
    let created = Command::new("mkfifo").arg(&fifo).output().expect("run mkfifo");
    assert!(created.status.success(), "mkfifo failed: {created:?}");
    case.install_fake("fake-app", FAKE_APP_SH);
    let log = proc_dir.join("log").display().to_string();
    case.set_env("FAKE_APP_LOG", &log);
    let stop = fifo.display().to_string();
    case.set_env("FAKE_APP_STOP", &stop);
    let ready = proc_dir.join("ready").display().to_string();
    case.set_env("FAKE_APP_READY", &ready);
    let child_pid = proc_dir.join("child.pid").display().to_string();
    case.set_env("FAKE_APP_CHILD_PID", &child_pid);
}

/// The case-relative child pid file written by `window` mode helpers.
pub const CHILD_PID_FILE: &str = "artifacts/proc/child.pid";

/// Run the helper synchronously for one-shot lifecycles.
pub fn run_once(case: &Case, args: &[&str]) -> Output {
    let root = case.root();
    Command::new(root.join("bin/fake-app"))
        .args(args)
        .env("FAKE_APP_LOG", root.join(PROC_DIR).join("log"))
        .env("FAKE_APP_STOP", root.join(PROC_DIR).join("stop.fifo"))
        .env("FAKE_APP_READY", root.join(PROC_DIR).join("ready"))
        .output()
        .expect("run fake-app")
}

/// Parse every logged invocation in order.
pub fn read_process_runs(case: &Case) -> Vec<ProcessRun> {
    parse_runs(&fs::read_to_string(case.root().join(PROC_DIR).join("log")).unwrap_or_default())
}

/// Reap helpers whose handles were lost: release the FIFO and return the
/// pids discovered in the log so the caller can verify they are gone.
pub fn release_all(case_root: &Path) -> Vec<u32> {
    let proc_dir = case_root.join(PROC_DIR);
    let pids = parse_runs(&fs::read_to_string(proc_dir.join("log")).unwrap_or_default())
        .into_iter()
        .map(|run| run.pid)
        .collect::<Vec<_>>();
    release_fifo(&proc_dir.join("stop.fifo"));
    pids
}

/// Whether the pid still names a live process on this host.
///
/// Zombie entries (`Z` in `/proc/<pid>/stat`) are processes whose parent has
/// not reaped them yet; they can no longer run, so they count as dead. This
/// matters for helpers whose owning handle was leaked without a final wait.
pub fn pid_alive(pid: u32) -> bool {
    let Some(stat) = fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok() else {
        return false;
    };
    match stat.rsplit_once(')') {
        Some((_, remainder)) => !remainder.trim_start().starts_with('Z'),
        None => true,
    }
}

fn parse_runs(log: &str) -> Vec<ProcessRun> {
    let mut runs = Vec::new();
    let mut lines = log.lines();
    while let Some(line) = lines.next() {
        let Ok(pid) = line.strip_prefix("PID ").unwrap_or("").parse() else {
            continue;
        };
        let Some(arg_line) = lines.next() else { break };
        let Ok(argc) = arg_line.strip_prefix("ARGV ").unwrap_or("").parse::<usize>() else {
            continue;
        };
        let tokens: Vec<String> = lines.by_ref().take(argc).map(str::to_owned).collect();
        runs.push(ProcessRun { pid, argv: tokens });
    }
    runs
}

fn release_fifo(fifo: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: a FIFO write-open without a reader would otherwise block
    // forever. ENXIO simply means there is nothing left to release.
    let opened = fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(fifo);
    let _ = opened;
}

fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(ALIVE_POLL);
    }
    !pid_alive(pid)
}

fn kill_forced(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

/// Guard for one started helper (owned child or detached daemon child).
/// Dropping the handle releases the FIFO and reaps the helper, so failed
/// cases cannot leak blocked processes.
pub struct HelperHandle {
    pid: u32,
    stop_fifo: PathBuf,
    child: Option<std::process::Child>,
    released: bool,
}

impl HelperHandle {
    /// Start a helper as a direct child of the test process.
    pub fn spawn(case: &Case, args: &[&str]) -> Self {
        let root = case.root();
        let child = Command::new(root.join("bin/fake-app"))
            .args(args)
            .env("FAKE_APP_LOG", root.join(PROC_DIR).join("log"))
            .env("FAKE_APP_STOP", root.join(PROC_DIR).join("stop.fifo"))
            .env("FAKE_APP_READY", root.join(PROC_DIR).join("ready"))
            .env("FAKE_APP_CHILD_PID", root.join(PROC_DIR).join("child.pid"))
            .spawn()
            .expect("spawn fake-app");
        Self {
            pid: child.id(),
            stop_fifo: root.join(PROC_DIR).join("stop.fifo"),
            child: Some(child),
            released: false,
        }
    }

    /// Track an already-detached daemon child by its readiness-file pid.
    pub fn adopt_detached(case: &Case) -> Self {
        let raw = fs::read_to_string(case.root().join(PROC_DIR).join("ready")).unwrap_or_default();
        let pid = raw.trim().parse().expect("readiness file must carry the daemon pid");
        Self {
            pid,
            stop_fifo: case.root().join(PROC_DIR).join("stop.fifo"),
            child: None,
            released: false,
        }
    }

    /// The helper's synthetic pid identity.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the helper still runs.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => pid_alive(self.pid),
        }
    }

    /// The owned child's exit status; detached helpers have none.
    pub fn exit_cleanly(&mut self) -> bool {
        let status = self.status_of();
        match (&self.child, status) {
            (Some(_), Some(status)) => status.success(),
            _ => false,
        }
    }

    fn status_of(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut().and_then(|child| child.try_wait().ok().flatten())
    }

    /// Release the FIFO, wait bounded for exit, then force-kill and reap.
    pub fn stop(&mut self, timeout: Duration) {
        if self.released {
            return;
        }
        self.released = true;
        release_fifo(&self.stop_fifo);
        if wait_until_dead(self.pid, timeout) {
            self.reap_owned();
            return;
        }
        kill_forced(self.pid);
        let _ = wait_until_dead(self.pid, timeout);
        self.reap_owned();
    }

    fn reap_owned(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.try_wait();
        }
    }
}

impl Drop for HelperHandle {
    fn drop(&mut self) {
        if !self.released {
            self.stop(RELEASE_WAIT);
        }
    }
}
