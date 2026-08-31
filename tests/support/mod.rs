//! Shared isolated CLI/e2e harness for production-bug audit fixtures (gqg.36.1).
//!
//! `Case` runs the real compiled `hyprloom` binary with unique temporary
//! `HOME`/XDG roots and a controlled PATH, records deterministic ordered JSONL
//! evidence, and owns artifact lifecycle: successful cases clean up, failing
//! cases retain their artifacts and print forensics. Readiness is signaled by
//! explicit files, never sleeps; no wall clock, host PID, or live desktop
//! service participates in assertions.
//!
//! Process execution deliberately uses `std::process::Command` against
//! `CARGO_BIN_EXE_hyprloom`: it scrubs the environment with `env_clear`,
//! keeps the fixture surface explicit, and avoids `assert_cmd`'s deprecated
//! `cargo_bin!` lookup. `tempfile` owns the isolated roots.

#![allow(
    dead_code,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::unwrap_used,
    clippy::expect_used
)]

use hyprloom::session::{save_session, Session};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, DirEntry};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Schema version of the JSONL trace envelope; bump on incompatible changes.
pub const TRACE_SCHEMA_VERSION: u32 = 1;
/// Version stamp of the shared fixture contract; bump when fixture behavior changes.
pub const FIXTURE_VERSION: u32 = 1;
const MAX_CAPTURED_BYTES: usize = 8 * 1024;
const MAX_ENV_VALUE_BYTES: usize = 200;
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ROOT_TOKEN: &str = "<case-root>";
const TRACE_RELATIVE_PATH: &str = "artifacts/trace.jsonl";
const ARTIFACTS_DIR_NAME: &str = "artifacts";

static CASE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// One captured CLI invocation with its normalized trace identity.
#[derive(Debug, Clone)]
pub struct Invocation {
    argv: Vec<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
}

impl Invocation {
    /// Whether the CLI exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The CLI exit status, when it exited normally.
    pub const fn code(&self) -> Option<i32> {
        self.code
    }

    /// Captured stdout as lossy UTF-8.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Captured stderr as lossy UTF-8.
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// The variable payload of one trace event; the envelope is stamped by `emit`.
struct TraceEvent {
    component: &'static str,
    operation: String,
    argv: Vec<Value>,
    outcome: String,
    extra: Value,
}

/// An isolated, traced CLI/e2e case.
///
/// Dropping the case removes all artifacts unless the test thread is
/// panicking; then the directory is retained and a forensic report is
/// printed to stderr.
#[derive(Debug)]
pub struct Case {
    id: String,
    root: Option<TempDir>,
    envs: BTreeMap<String, String>,
    next_seq: u32,
    last_failure: Option<(String, String)>,
}

impl Case {
    /// Create a uniquely identified case with fresh isolated roots.
    pub fn new(name: &str) -> Self {
        let serial = CASE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("{name}-{serial:04}");
        let root = tempfile::tempdir().unwrap();
        for dir in ["home", "config", "data", "runtime", "bin", ARTIFACTS_DIR_NAME] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let mut case = Self {
            id,
            root: Some(root),
            envs: BTreeMap::new(),
            next_seq: 0,
            last_failure: None,
        };
        let isolated = case.allowlisted_env();
        let trace_env: BTreeMap<String, String> = isolated
            .iter()
            .map(|(key, value)| (key.clone(), case.redact_env_value(key, value)))
            .collect();
        case.emit(TraceEvent {
            component: "harness",
            operation: "case-start".to_owned(),
            argv: vec![json!(case.id)],
            outcome: "open".to_owned(),
            extra: json!({ "env": trace_env, "fixture_env": case.redacted_fixture_env() }),
        });
        case
    }

    /// The case root directory; all state lives inside it.
    pub fn root(&self) -> &Path {
        self.root.as_ref().expect("case root exists until drop").path()
    }

    /// The unique case identifier recorded in every trace event.
    pub fn case_id(&self) -> &str {
        &self.id
    }

    /// Install an executable fixture script into the controlled PATH.
    pub fn install_fake(&mut self, name: &str, contents: &str) {
        let path = self.root().join("bin").join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        self.emit(TraceEvent {
            component: "harness",
            operation: "install-fake".to_owned(),
            argv: vec![json!(name)],
            outcome: "ok".to_owned(),
            extra: json!({ "details": format!("{} bytes", contents.len()) }),
        });
    }

    /// Record an extra fixture-only environment variable for the CLI.
    pub fn set_env(&mut self, key: &str, value: &str) {
        self.envs.insert(key.to_owned(), value.to_owned());
    }

    /// Write a file inside the case root, creating parent directories.
    pub fn write_file(&mut self, relative: &str, contents: &[u8]) {
        let path = self.root().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        self.emit(TraceEvent {
            component: "harness",
            operation: "write-file".to_owned(),
            argv: vec![json!(relative)],
            outcome: "ok".to_owned(),
            extra: json!({ "details": format!("{} bytes", contents.len()) }),
        });
    }

    /// Persist a session document into the isolated session store.
    pub fn save_session(&mut self, name: &str, session: &Value) {
        let session: Session = serde_json::from_value(session.clone()).unwrap();
        let sessions_dir = self.root().join("data/hyprloom/sessions");
        save_session(&session, &sessions_dir).unwrap();
        self.emit(TraceEvent {
            component: "harness",
            operation: "save-session".to_owned(),
            argv: vec![json!(name)],
            outcome: "ok".to_owned(),
            extra: json!({}),
        });
    }

    /// Run the real compiled CLI with scrubbed environment and capture evidence.
    pub fn run(&mut self, operation: &str, args: &[&str]) -> Invocation {
        let state_before = self.state_hash();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_hyprloom"));
        command.env_clear();
        let allowlisted = self.allowlisted_env();
        for (key, value) in &allowlisted {
            command.env(key, value);
        }
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        command.args(args);
        let output = command.output().expect("spawn hyprloom");
        let state_after = self.state_hash();

        let invocation = Invocation {
            argv: self.normalized_argv(args),
            stdout: output.stdout,
            stderr: output.stderr,
            code: output.status.code(),
        };
        let outcome = match invocation.code {
            Some(0) => "success".to_owned(),
            Some(code) => format!("exit-{code}"),
            None => "signal".to_owned(),
        };
        let (stdout, stdout_truncated) = self.bounded_text(&invocation.stdout);
        let (stderr, stderr_truncated) = self.bounded_text(&invocation.stderr);
        self.emit(TraceEvent {
            component: "cli",
            operation: operation.to_owned(),
            argv: invocation.argv.iter().map(|argument| json!(argument)).collect(),
            outcome,
            extra: json!({
                "env_keys": allowlisted.keys().cloned().collect::<Vec<String>>(),
                "exit_status": invocation.code,
                "stdout": stdout,
                "stderr": stderr,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
                "state_hash_before": state_before,
                "state_hash_after": state_after,
            }),
        });
        invocation
    }

    /// Record an assertion with trace context; a failure panics with forensics.
    pub fn assert(&mut self, label: &str, condition: bool, detail: &str) {
        let outcome = if condition { "pass" } else { "fail" };
        let details = Self::bound_string(&self.normalize_text(detail), MAX_CAPTURED_BYTES);
        self.emit(TraceEvent {
            component: "harness",
            operation: "assert".to_owned(),
            argv: vec![json!(label)],
            outcome: outcome.to_owned(),
            extra: json!({ "details": details }),
        });
        if !condition {
            self.last_failure = Some((label.to_owned(), detail.to_owned()));
            panic!(
                "case {}: assertion '{label}' failed: {detail}\n{}",
                self.id,
                Self::forensics_at(self.root())
            );
        }
    }

    /// Assert stdout is exactly one parseable JSON document and return it.
    pub fn assert_single_json_document(&mut self, label: &str, invocation: &Invocation) -> Value {
        match serde_json::from_slice::<Value>(&invocation.stdout) {
            Ok(value) => {
                self.assert(label, true, "stdout is exactly one parseable json document");
                value
            }
            Err(error) => {
                let detail = format!(
                    "stdout is not exactly one json document: {error}; captured: {:?}",
                    invocation.stdout_str()
                );
                self.assert(label, false, &detail);
                unreachable!("assert panics on failure");
            }
        }
    }

    /// Assert a fatal pre-report failure: nonzero exit, empty stdout, stderr diagnostics.
    pub fn assert_fatal_pre_report(&mut self, label: &str, invocation: &Invocation) {
        let detail = format!(
            "exit {:?}, stdout {:?}, stderr {:?}",
            invocation.code,
            invocation.stdout_str(),
            invocation.stderr_str()
        );
        let ok = invocation.code != Some(0) && invocation.stdout.is_empty() && !invocation.stderr.is_empty();
        self.assert(label, ok, &detail);
    }

    /// Wait for an explicit readiness signal file; bounded, never a fixed sleep.
    pub fn wait_for_signal(&mut self, relative: &str, timeout: Duration) -> bool {
        let path = self.root().join(relative);
        let deadline = Instant::now() + timeout;
        let mut arrived = path.exists();
        while !arrived && Instant::now() < deadline {
            std::thread::sleep(SIGNAL_POLL_INTERVAL);
            arrived = path.exists();
        }
        self.emit(TraceEvent {
            component: "harness",
            operation: "wait-signal".to_owned(),
            argv: vec![json!(relative)],
            outcome: if arrived { "arrived" } else { "timeout" }.to_owned(),
            extra: json!({ "details": format!("deadline {timeout:?}") }),
        });
        arrived
    }

    /// SHA-256 manifest over the case state, excluding harness artifacts.
    pub fn state_hash(&self) -> String {
        let mut entries = Vec::new();
        Self::collect_state(self.root(), "", &mut entries);
        entries.sort();
        let mut hasher = Sha256::new();
        for entry in &entries {
            Self::hash_entry(&mut hasher, entry);
        }
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(64);
        for byte in digest {
            let _ = write!(hex, "{byte:02x}");
        }
        format!("sha256:{hex}")
    }

    /// Read the recorded trace events in order.
    pub fn read_trace(&self) -> Vec<Value> {
        let raw = fs::read_to_string(self.root().join(TRACE_RELATIVE_PATH)).unwrap_or_default();
        raw.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    /// The exact trace file location inside the case root.
    pub fn trace_path(&self) -> PathBuf {
        self.root().join(TRACE_RELATIVE_PATH)
    }

    fn hash_entry(hasher: &mut Sha256, entry: &(String, bool, u32, Option<Vec<u8>>)) {
        let (relative, is_dir, mode, contents) = entry;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update([u8::from(*is_dir)]);
        hasher.update(format!("{mode:o}").as_bytes());
        hasher.update([0]);
        hasher.update(contents.as_ref().map_or(0usize, Vec::len).to_le_bytes());
        if let Some(bytes) = contents {
            hasher.update(bytes);
        }
    }

    fn collect_state(dir: &Path, prefix: &str, entries: &mut Vec<(String, bool, u32, Option<Vec<u8>>)>) {
        let Ok(read_dir) = fs::read_dir(dir) else { return };
        let mut children: Vec<DirEntry> = read_dir.filter_map(std::result::Result::ok).collect();
        children.sort_by_key(DirEntry::file_name);
        for child in &children {
            Self::collect_child(child, prefix, entries);
        }
    }

    fn collect_child(child: &DirEntry, prefix: &str, entries: &mut Vec<(String, bool, u32, Option<Vec<u8>>)>) {
        let name = child.file_name().to_string_lossy().into_owned();
        if prefix.is_empty() && name == ARTIFACTS_DIR_NAME {
            return;
        }
        let relative = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let metadata = child.metadata().unwrap();
        let mode = metadata.permissions().mode();
        if metadata.is_dir() {
            entries.push((relative.clone(), true, mode, None));
            Self::collect_state(&child.path(), &relative, entries);
        } else {
            let contents = fs::read(child.path()).unwrap_or_default();
            entries.push((relative, false, mode, Some(contents)));
        }
    }

    fn allowlisted_env(&self) -> BTreeMap<String, String> {
        let root = self.root();
        let mut env = BTreeMap::new();
        env.insert("HOME".to_owned(), root.join("home").to_string_lossy().into_owned());
        env.insert(
            "PATH".to_owned(),
            format!("{}:{}", root.join("bin").display(), std::env::var("PATH").unwrap_or_default()),
        );
        env.insert("XDG_CONFIG_HOME".to_owned(), root.join("config").to_string_lossy().into_owned());
        env.insert("XDG_DATA_HOME".to_owned(), root.join("data").to_string_lossy().into_owned());
        env.insert("XDG_RUNTIME_DIR".to_owned(), root.join("runtime").to_string_lossy().into_owned());
        env
    }

    fn redacted_fixture_env(&self) -> BTreeMap<String, String> {
        self.envs
            .iter()
            .map(|(key, value)| (key.clone(), self.redact_env_value(key, value)))
            .collect()
    }

    fn redact_env_value(&self, key: &str, value: &str) -> String {
        let upper = key.to_uppercase();
        if ["TOKEN", "SECRET", "PASSWORD", "KEY"].iter().any(|marker| upper.contains(marker)) {
            return "<redacted>".to_owned();
        }
        if key == "PATH" {
            let segments = value.split(':').count();
            return format!("<fixture-bin>+{} system entries", segments.saturating_sub(1));
        }
        let normalized = self.normalize_text(value);
        Self::bound_string(&normalized, MAX_ENV_VALUE_BYTES)
    }

    fn normalized_argv(&self, arguments: &[&str]) -> Vec<String> {
        let mut argv = vec!["hyprloom".to_owned()];
        argv.extend(arguments.iter().map(|argument| self.normalize_text(argument)));
        argv
    }

    fn normalize_text(&self, text: &str) -> String {
        let root = self.root().display().to_string();
        if root.is_empty() {
            return text.to_owned();
        }
        text.replace(&root, ROOT_TOKEN)
    }

    fn bounded_text(&self, bytes: &[u8]) -> (String, bool) {
        if bytes.len() <= MAX_CAPTURED_BYTES {
            return (self.normalize_text(&String::from_utf8_lossy(bytes)), false);
        }
        let truncated = String::from_utf8_lossy(&bytes[..MAX_CAPTURED_BYTES]).into_owned();
        let remaining = bytes.len() - MAX_CAPTURED_BYTES;
        let mut text = self.normalize_text(&truncated);
        let _ = write!(text, " …<truncated {remaining} bytes>");
        (text, true)
    }

    fn bound_string(text: &str, limit: usize) -> String {
        if text.len() <= limit {
            return text.to_owned();
        }
        let mut cut = limit;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{} …<truncated>", &text[..cut])
    }

    fn emit(
        &mut self,
        TraceEvent {
            component,
            operation,
            argv,
            outcome,
            extra,
        }: TraceEvent,
    ) {
        self.next_seq += 1;
        let mut record = json!({
            "schema_version": TRACE_SCHEMA_VERSION,
            "case_id": self.id,
            "fixture_version": FIXTURE_VERSION,
            "seq": self.next_seq,
            "component": component,
            "operation": operation,
            "argv": argv,
            "outcome": outcome,
        });
        if let (Some(object), Value::Object(extra_object)) = (record.as_object_mut(), extra) {
            object.extend(extra_object);
        }
        let path = self.trace_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(file, "{record}").unwrap();
    }

    /// Trace tail plus the retained artifact location, rooted at `path`.
    fn forensics_at(path: &Path) -> String {
        let raw = fs::read_to_string(path.join(TRACE_RELATIVE_PATH)).unwrap_or_default();
        let mut tail: Vec<&str> = raw.lines().rev().take(6).collect();
        tail.reverse();
        format!(
            "trace tail (last {} events):\n{}\nartifacts retained at {}",
            tail.len(),
            tail.join("\n"),
            path.display()
        )
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return; // TempDir's own drop cleans the successful case.
        }
        if let Some(dir) = self.root.take() {
            let path = dir.path().to_path_buf();
            std::mem::forget(dir); // Retain artifacts for diagnosis.
            let header = match &self.last_failure {
                Some((label, detail)) => format!("hyprloom harness case {} FAILED: '{label}': {detail}", self.id),
                None => format!("hyprloom harness case {} FAILED", self.id),
            };
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{header}\n{}", Self::forensics_at(&path));
        }
    }
}
