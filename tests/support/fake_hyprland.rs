//! Stateful fake-Hyprland boundary fixture (gqg.36.2).
//!
//! A PATH-injected `hyprctl` relay forwards each request to a deterministic
//! state machine running inside the test process. The scenario declares
//! initial clients/monitors/workspaces/focus, the version probe reply, the
//! batch rejection dialect, scripted faults, and delayed mapping events.
//! Every request advances one deterministic event sequence and is logged to
//! `artifacts/hyprland.jsonl` with case identity, monotonic sequence, request
//! kind, normalized argv, scripted reply summary, and before/after topology
//! identity. Client titles/URLs/CWDs never enter the log: query replies are
//! summarized as byte counts, and tests keep dispatch argv to addresses,
//! monitors, and workspaces (dispatch argv cannot carry titles or URLs).
//!
//! Address reuse is expressed purely by scenario state: later mapping events
//! may reuse an existing address while carrying a new stable ID. Command
//! tokenization inside batch payloads reuses `hyprloom::hyprctl`'s own
//! parser so the fake cannot drift from the product's framing rules.

#![allow(dead_code, clippy::missing_panics_doc, clippy::unwrap_used, clippy::expect_used)]

use hyprloom::hyprctl::parse_dispatch_args;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::support::Case;

/// Version stamp of the fake-Hyprland fixture contract.
pub const FAKE_HYPRLAND_VERSION: u32 = 1;
const TRACE_SCHEMA_VERSION: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const TRACE_FILE: &str = "hyprland.jsonl";

/// How the fake reports a rejected operation inside a batch payload.
///
/// `ExitNonzero` mirrors an honest compositor that fails the request.
/// `ExitZeroText` mirrors the dialect that exits zero while printing
/// rejection text, which audit beads must be able to reproduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchRejection {
    /// Fail the whole batch with a nonzero exit status.
    #[default]
    ExitNonzero,
    /// Exit zero while printing rejection text on stdout.
    ExitZeroText,
}

/// A scripted failure for dispatches whose joined command contains `matches`.
#[derive(Debug, Clone)]
pub struct Fault {
    /// Substring matched against the logical dispatch command.
    pub matches: String,
    /// Stderr text the fake replies with.
    pub stderr: String,
    /// Exit status the fake replies with.
    pub exit_code: i32,
}

/// A client that appears only after a scripted trigger (delayed mapping).
///
/// The trigger is either the Nth dispatch attempt or the Nth `clients`
/// query. An optional gate names a case-relative readiness file: the event
/// stays pending until that file exists, which couples compositor state to
/// helper-process state without sleeps. A client `pid` may be a
/// `{"pid_file": "<case-relative path>"}` reference; the fake resolves it at
/// mapping time so correlations run against genuine live pids.
#[derive(Debug, Clone)]
pub struct MappingEvent {
    /// Dispatch-attempt count after which the client maps.
    pub after_dispatch: Option<usize>,
    /// `clients` query count after which the client maps.
    pub after_query: Option<usize>,
    /// Case-relative readiness file that must exist before mapping.
    pub gated_on: Option<String>,
    /// Full Hyprland client JSON for the newly mapped window.
    pub client: Value,
}

/// The declarative scenario backing one fake compositor instance.
#[derive(Debug, Clone, Default)]
pub struct HyprlandScenario {
    /// Initial client windows.
    pub clients: Vec<Value>,
    /// Initial monitor topology.
    pub monitors: Vec<Value>,
    /// Initial workspace list.
    pub workspaces: Vec<Value>,
    /// Address of the focused window; `None` maps to Hyprland's `0x0`.
    pub focused_address: Option<String>,
    /// Version token reported by the `version` probe.
    pub version: String,
    /// Dialect used to report rejected batch operations.
    pub batch_rejection: BatchRejection,
    /// Delayed mapping events keyed by dispatch-attempt count.
    pub mapping_events: Vec<MappingEvent>,
    /// Scripted faults matched against logical dispatch commands.
    pub faults: Vec<Fault>,
    /// Case-relative gate file: while it exists, every request is held
    /// unanswered, so the calling CLI stays mid-operation (deterministic
    /// lock-holding for contention scenarios).
    pub request_gate: Option<String>,
}

/// One replayed reply from the fake boundary.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Process exit status.
    pub code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

impl Reply {
    /// Whether the fake exited zero.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Captured stdout as lossy UTF-8.
    pub fn stdout_str(&self) -> String {
        self.stdout.clone()
    }

    /// Captured stderr as lossy UTF-8.
    pub fn stderr_str(&self) -> String {
        self.stderr.clone()
    }

    fn ok(stdout: &str) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn fail(stderr: &str, code: i32) -> Self {
        Self {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }
}

/// A topology snapshot for invariant assertions.
#[derive(Debug, Clone)]
pub struct Topology {
    /// Current clients in mapping order.
    pub clients: Vec<Value>,
    /// Address of the focused window, when any.
    pub focused: Option<String>,
}

struct Request {
    uniq: String,
    args: Vec<String>,
}

struct Compositor {
    case_id: String,
    case_root: PathBuf,
    scenario: HyprlandScenario,
    clients: Vec<Value>,
    monitors: Vec<Value>,
    workspaces: Vec<Value>,
    focused: Option<String>,
    dispatch_count: usize,
    clients_queries: usize,
    request_gate: Option<String>,
    pending_events: Vec<MappingEvent>,
    pending_operations: Vec<String>,
    seq: u32,
    events: Vec<Value>,
}

impl Compositor {
    fn new(case_id: String, case_root: PathBuf, scenario: HyprlandScenario) -> Self {
        let pending_events = scenario.mapping_events.clone();
        let request_gate = scenario.request_gate.clone();
        Self {
            clients: scenario.clients.clone(),
            monitors: scenario.monitors.clone(),
            workspaces: scenario.workspaces.clone(),
            focused: scenario.focused_address.clone(),
            case_id,
            case_root,
            scenario,
            dispatch_count: 0,
            clients_queries: 0,
            request_gate,
            pending_events,
            pending_operations: Vec::new(),
            seq: 0,
            events: Vec::new(),
        }
    }

    fn identity(&self) -> String {
        let canonical = serde_json::to_string(&(&self.clients, &self.focused)).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(hex, "{byte:02x}");
        }
        format!("sha256:{hex}")
    }

    fn answer(&mut self, argv: &[String]) -> Reply {
        match argv.first().map(String::as_str) {
            Some("clients") => {
                self.clients_queries += 1;
                self.apply_mapping_events();
                Reply::ok(&format_json(&self.clients))
            }
            Some("monitors") => Reply::ok(&format_json(&self.monitors)),
            Some("workspaces") => Reply::ok(&format_json(&self.workspaces)),
            Some("activewindow") => {
                let address = self.focused.clone().unwrap_or_else(|| "0x0".to_owned());
                Reply::ok(&format_json(&json!({"address": address})))
            }
            Some("cursorpos") => Reply::ok("{\"x\": 0, \"y\": 0}\n"),
            Some("version") => Reply::ok(&format!("Hyprland {} built from the fake fixture\n", self.scenario.version)),
            Some("dispatch") => self.answer_dispatch(&argv[1..]),
            Some("--batch") => argv.get(1).map_or_else(
                || Reply::fail("fake-hyprctl: --batch requires a payload\n", 1),
                |payload| self.answer_batch(payload),
            ),
            Some(other) => Reply::fail(&format!("fake-hyprctl: unknown command {other}\n"), 1),
            None => Reply::fail("fake-hyprctl: no command given\n", 1),
        }
    }

    fn answer_dispatch(&mut self, args: &[String]) -> Reply {
        let command = args.join(" ");
        self.dispatch_count += 1;
        if let Some(fault) = self.matching_fault(&command) {
            return Reply::fail(&fault.stderr, fault.exit_code);
        }
        self.apply(args);
        self.apply_mapping_events();
        Reply::ok("ok\n")
    }

    fn answer_batch(&mut self, payload: &str) -> Reply {
        let commands = split_batch(payload);
        self.pending_operations = commands.iter().map(|command| logical_operation(command)).collect();
        if let Some(rejection) = commands.iter().find_map(|command| self.step_batch(command)) {
            return rejection;
        }
        Reply::ok("ok\n")
    }

    /// Apply one batch operation; `Some` carries the scripted rejection.
    fn step_batch(&mut self, command: &str) -> Option<Reply> {
        let trimmed = command.trim();
        self.dispatch_count += 1;
        if let Some(fault) = self.matching_fault(trimmed) {
            return Some(self.reject_batch(&fault.stderr, fault.exit_code));
        }
        let Some(logical) = trimmed.strip_prefix("dispatch ") else {
            return Some(self.reject_batch(&format!("fake-hyprctl: invalid dispatch: {trimmed}\n"), 1));
        };
        match parse_dispatch_args(logical) {
            Ok(args) => {
                self.apply(&args);
                self.apply_mapping_events();
                None
            }
            Err(error) => Some(self.reject_batch(&format!("fake-hyprctl: invalid dispatch syntax: {error}\n"), 1)),
        }
    }

    fn reject_batch(&self, stderr: &str, code: i32) -> Reply {
        match self.scenario.batch_rejection {
            BatchRejection::ExitNonzero => Reply {
                code: Some(code),
                stdout: String::new(),
                stderr: stderr.to_owned(),
            },
            BatchRejection::ExitZeroText => Reply {
                code: Some(0),
                stdout: "Invalid dispatcher\n".to_owned(),
                stderr: String::new(),
            },
        }
    }

    fn matching_fault(&self, command: &str) -> Option<Fault> {
        self.scenario.faults.iter().find(|fault| command.contains(&fault.matches)).cloned()
    }

    fn apply_mapping_events(&mut self) {
        let pending = std::mem::take(&mut self.pending_events);
        let (fired, remaining): (Vec<MappingEvent>, Vec<MappingEvent>) = pending.into_iter().partition(|event| self.mapping_event_is_due(event));
        self.pending_events = remaining;
        for event in fired {
            let client = self.resolve_client(event.client.clone());
            self.clients.push(client);
        }
    }

    /// An event is due once its trigger count is reached (or passed, so a
    /// gate can hold it open across later queries) and its gate file exists.
    fn mapping_event_is_due(&self, event: &MappingEvent) -> bool {
        let dispatch_reached = event.after_dispatch.is_some_and(|n| self.dispatch_count >= n);
        let query_reached = event.after_query.is_some_and(|n| self.clients_queries >= n);
        if !dispatch_reached && !query_reached {
            return false;
        }
        event.gated_on.as_ref().is_none_or(|gate| self.case_root.join(gate).exists())
    }

    /// Substitute `{"pid_file": ...}` pid references with the live pid the
    /// helper recorded, so correlations see genuine process identity.
    fn resolve_client(&self, mut client: Value) -> Value {
        let Some(reference) = client["pid"].get("pid_file").and_then(Value::as_str) else {
            return client;
        };
        let raw = fs::read_to_string(self.case_root.join(reference)).unwrap_or_default();
        client["pid"] = json!(raw.trim().parse::<u32>().unwrap_or(0));
        client
    }

    fn apply(&mut self, args: &[String]) {
        let Some(operation) = args.first() else { return };
        match operation.as_str() {
            "movetoworkspacesilent" => self.apply_workspace_move(args.get(1).map(String::as_str)),
            "focuswindow" => self.apply_focus(args.get(1).map(String::as_str)),
            "closewindow" | "killwindow" => self.apply_close(args.get(1).map(String::as_str)),
            "pin" => self.apply_pin(args.get(1).map(String::as_str)),
            "resizewindowpixel" => self.apply_pixel(args, false),
            "movewindowpixel" => self.apply_pixel(args, true),
            // Remaining compositor operations are recorded but leave the
            // modeled topology unchanged until a regression needs them.
            _ => {}
        }
    }

    fn apply_focus(&mut self, target: Option<&str>) {
        if let Some(address) = target.and_then(|token| token.strip_prefix("address:")) {
            self.focused = Some(address.to_owned());
        }
    }

    fn apply_workspace_move(&mut self, target: Option<&str>) {
        let Some(target) = target else { return };
        let mut parts = target.splitn(2, ',');
        let workspace_token = parts.next().unwrap_or_default();
        let address = parts
            .next()
            .and_then(|token| token.strip_prefix("address:"))
            .unwrap_or_default()
            .to_owned();
        let Some(client) = self.clients.iter_mut().find(|client| client["address"] == json!(address)) else {
            return;
        };
        let (id, name) = workspace_identity(workspace_token);
        client["workspace"] = json!({"id": id, "name": name});
    }

    fn apply_close(&mut self, target: Option<&str>) {
        let Some(address) = target.and_then(|token| token.strip_prefix("address:")) else {
            return;
        };
        self.clients.retain(|client| client["address"] != json!(address));
        if self.focused.as_deref() == Some(address) {
            self.focused = None;
        }
    }

    fn apply_pin(&mut self, target: Option<&str>) {
        let Some(address) = target.map(|token| token.strip_prefix("address:").unwrap_or(token)) else {
            return;
        };
        if let Some(client) = self.clients.iter_mut().find(|client| client["address"] == json!(address)) {
            client["pinned"] = json!(true);
        }
    }

    fn apply_pixel(&mut self, args: &[String], position: bool) {
        // Shape: <op> exact <first> <second>,address:<address>
        let (Some(first), Some(second)) = (args.get(2), args.get(3)) else {
            return;
        };
        let Some(address) = second.split(',').nth(1).and_then(|token| token.strip_prefix("address:")) else {
            return;
        };
        let second_value: i64 = second.split(',').next().and_then(|token| token.parse().ok()).unwrap_or(0);
        let first_value: i64 = first.parse().unwrap_or(0);
        let Some(client) = self.clients.iter_mut().find(|client| client["address"] == json!(address)) else {
            return;
        };
        if position {
            client["at"] = json!([first_value, second_value]);
        } else {
            client["size"] = json!([first_value, second_value]);
        }
    }

    fn build_event(&mut self, request: &Request, reply: &Reply, before: &str, kind: &str) -> Value {
        self.seq += 1;
        let after = self.identity();
        let event = json!({
            "schema_version": TRACE_SCHEMA_VERSION,
            "fixture": "fake-hyprland",
            "fixture_version": FAKE_HYPRLAND_VERSION,
            "case_id": self.case_id,
            "seq": self.seq,
            "kind": kind,
            "argv": request.args,
            "operations": self.pending_operations,
            "reply": {
                "exit": reply.code,
                "stdout_bytes": reply.stdout.len(),
                "stderr_bytes": reply.stderr.len(),
                "summary": reply_summary(reply),
            },
            "state_before": before,
            "state_after": after,
        });
        self.pending_operations = Vec::new();
        self.events.push(event.clone());
        event
    }
}

fn workspace_identity(token: &str) -> (i64, String) {
    token.parse::<i64>().map_or_else(
        |_| (-1, token.strip_prefix("name:").unwrap_or(token).to_owned()),
        |id| (id, token.to_owned()),
    )
}

fn reply_summary(reply: &Reply) -> String {
    if reply.success() {
        return "ok".to_owned();
    }
    let first_line = reply.stderr.lines().next().unwrap_or("failed");
    let mut summary: String = first_line.chars().take(120).collect();
    summary.push('…');
    summary
}

fn format_json<T: serde::Serialize>(value: &T) -> String {
    let mut rendered = serde_json::to_string(value).unwrap_or_default();
    rendered.push('\n');
    rendered
}

/// Split a batch payload on unescaped top-level ` ; ` separators, then
/// unescape the product's `\;` and `\\` sequences inside each command.
fn split_batch(payload: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = payload.chars().peekable();
    while let Some(character) = chars.next() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => {
                escaped = true;
                current.push(character);
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(character);
                current.push(character);
            }
            character if Some(character) == quote => {
                quote = None;
                current.push(character);
            }
            ' ' if quote.is_none() && chars.peek() == Some(&';') => {
                chars.next();
                flush_batch_separator(&mut chars, &mut current, &mut commands);
            }
            character => current.push(character),
        }
    }
    commands.push(current);
    commands.iter().map(|command| unescape_command(command)).collect()
}

/// Consume the characters after a top-level `;`: a space finalizes the
/// batch command; anything else is literal payload content.
fn flush_batch_separator<I: Iterator<Item = char>>(chars: &mut I, current: &mut String, commands: &mut Vec<String>) {
    match chars.next() {
        Some(' ') => commands.push(std::mem::take(current).trim_end().to_owned()),
        other => {
            current.push(' ');
            current.push(';');
            if let Some(character) = other {
                current.push(character);
            }
        }
    }
}

fn unescape_command(command: &str) -> String {
    command.replace("\\;", ";").replace("\\\\", "\\")
}

/// Strip the `dispatch ` framing keyword from one batch operation.
fn logical_operation(command: &str) -> String {
    let trimmed = command.trim();
    let logical = trimmed.strip_prefix("dispatch ").unwrap_or(trimmed);
    logical.to_owned()
}

/// The PATH-injected `hyprctl`: forwards one request to the in-test
/// responder and replays its scripted reply through files as readiness
/// signals. PID gives the request uniqueness; ordering is owned by the
/// responder, which serves strictly in queue order.
const RELAY_SH: &str = r#"#!/bin/sh
set -u
dir=${FAKE_HYPRCTL_DIR:?fake-hyprctl requires FAKE_HYPRCTL_DIR}
uniq=$$
{
  printf 'REQ %s %s\n' "$uniq" "$#"
  for arg do
    printf '%s\n' "$arg"
  done
} >>"$dir/queue"
tries=0
limit=1200
while [ "$tries" -lt "$limit" ]; do
  if [ -f "$dir/responses/$uniq.done" ]; then
    cat "$dir/responses/$uniq.out"
    cat "$dir/responses/$uniq.err" >&2
    exit "$(cat "$dir/responses/$uniq.code")"
  fi
  tries=$((tries + 1))
  sleep 0.05
done
printf 'fake-hyprctl: responder never answered request %s\n' "$uniq" >&2
exit 124
"#;

/// Handle to one running fake compositor; dropping it stops the responder.
pub struct FakeHyprland {
    dir: PathBuf,
    trace_path: PathBuf,
    relay_path: PathBuf,
    shared: Arc<Mutex<Compositor>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl fmt::Debug for FakeHyprland {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FakeHyprland").field("dir", &self.dir).finish_non_exhaustive()
    }
}

impl FakeHyprland {
    /// Install the relay into the case and start the responder thread.
    pub fn spawn(case: &mut Case, scenario: HyprlandScenario) -> Self {
        let case_id = case.case_id().to_owned();
        let root = case.root().to_path_buf();
        let dir = root.join("artifacts/fake-hyprland");
        fs::create_dir_all(dir.join("responses")).unwrap();
        case.install_fake("hyprctl", RELAY_SH);
        let dir_display = dir.display().to_string();
        case.set_env("FAKE_HYPRCTL_DIR", &dir_display);
        let shared = Arc::new(Mutex::new(Compositor::new(case_id, root.clone(), scenario)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_dir = dir.clone();
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || responder_loop(thread_dir, thread_shared, thread_stop));
        Self {
            trace_path: root.join("artifacts").join(TRACE_FILE),
            relay_path: root.join("bin/hyprctl"),
            dir,
            shared,
            stop,
            join: Some(join),
        }
    }

    /// Invoke the relay exactly as the CLI would and capture the reply.
    pub fn call(&self, args: &[&str]) -> Reply {
        let output = Command::new(&self.relay_path)
            .args(args)
            .env("FAKE_HYPRCTL_DIR", &self.dir)
            .output()
            .expect("run fake hyprctl relay");
        Reply {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Snapshot the current topology for invariant assertions.
    pub fn topology(&self) -> Topology {
        let state = self.shared.lock().unwrap();
        Topology {
            clients: state.clients.clone(),
            focused: state.focused.clone(),
        }
    }

    /// Number of dispatch attempts seen so far, including batch operations.
    pub fn dispatch_count(&self) -> usize {
        self.shared.lock().unwrap().dispatch_count
    }

    /// In-memory request events, for exact argv and ordering assertions.
    pub fn events(&self) -> Vec<Value> {
        self.shared.lock().unwrap().events.clone()
    }

    /// Read the ordered redacted fixture log from disk.
    pub fn fixture_trace(&self) -> Vec<Value> {
        let raw = fs::read_to_string(&self.trace_path).unwrap_or_default();
        raw.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }
}

impl Drop for FakeHyprland {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn responder_loop(dir: PathBuf, shared: Arc<Mutex<Compositor>>, stop: Arc<AtomicBool>) {
    let mut cursor = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let progressed = pump(&dir, &shared, &mut cursor);
        if !progressed {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn pump(dir: &Path, shared: &Arc<Mutex<Compositor>>, cursor: &mut usize) -> bool {
    let Ok(queue) = fs::read_to_string(dir.join("queue")) else {
        return false;
    };
    let requests = drain_queue(cursor, &queue);
    let progressed = !requests.is_empty();
    for request in &requests {
        handle_request(dir, shared, request);
    }
    progressed
}

fn handle_request(dir: &Path, shared: &Arc<Mutex<Compositor>>, request: &Request) {
    hold_request_gate(dir, shared);
    let kind = request.args.first().map_or("empty", String::as_str).to_owned();
    let mut state = shared.lock().unwrap();
    let before = state.identity();
    let reply = state.answer(&request.args);
    let kind = if kind == "--batch" { "batch" } else { kind.as_str() };
    let event = state.build_event(request, &reply, &before, kind);
    drop(state);
    write_response(dir, &request.uniq, &reply);
    append_event(dir, &event);
}

fn hold_request_gate(dir: &Path, shared: &Arc<Mutex<Compositor>>) {
    loop {
        let gate = {
            let state = shared.lock().unwrap();
            state.request_gate.clone()
        };
        let Some(gate) = gate else { return };
        if !dir.parent().unwrap_or(dir).join(&gate).exists() {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn write_response(dir: &Path, uniq: &str, reply: &Reply) {
    let base = dir.join("responses").join(uniq);
    let _ = fs::write(base.with_extension("out"), &reply.stdout);
    let _ = fs::write(base.with_extension("err"), &reply.stderr);
    let _ = fs::write(base.with_extension("code"), reply.code.unwrap_or(1).to_string());
    let _ = fs::write(base.with_extension("done"), b"done");
}

fn append_event(dir: &Path, event: &Value) {
    let path = dir.parent().unwrap_or(dir).join(TRACE_FILE);
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{event}");
}

fn drain_queue(cursor: &mut usize, queue: &str) -> Vec<Request> {
    let bytes = queue.as_bytes();
    let mut requests = Vec::new();
    while let Some(header_end) = find_newline(bytes, *cursor) {
        let Some((uniq, argc)) = parse_header(&queue[*cursor..header_end]) else {
            *cursor = header_end + 1;
            continue;
        };
        let Some((tokens, position)) = collect_args(queue, bytes, header_end + 1, argc) else {
            break; // incomplete request; wait for more bytes
        };
        *cursor = position;
        requests.push(Request { uniq, args: tokens });
    }
    requests
}

fn find_newline(bytes: &[u8], from: usize) -> Option<usize> {
    bytes[from..].iter().position(|byte| *byte == b'\n').map(|offset| from + offset)
}

fn parse_header(header: &str) -> Option<(String, usize)> {
    let mut parts = header.split_whitespace();
    let marker = parts.next()?;
    if marker != "REQ" {
        return None;
    }
    let uniq = parts.next()?.to_owned();
    let argc = parts.next()?.parse().ok()?;
    Some((uniq, argc))
}

fn collect_args(queue: &str, bytes: &[u8], mut position: usize, argc: usize) -> Option<(Vec<String>, usize)> {
    let mut tokens = Vec::with_capacity(argc);
    for _ in 0..argc {
        let line_end = find_newline(bytes, position)?;
        tokens.push(queue[position..line_end].to_owned());
        position = line_end + 1;
    }
    Some((tokens, position))
}
