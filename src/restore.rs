use crate::config::{app_config_for, is_ignored_class, Config};
use crate::hyprctl::{HyprClient, HyprMonitor, HyprctlClient, HyprctlError};
use crate::process::{
    find_profile_discovery, select_terminal_process, ProcessInfoProvider, RealProcessInfo,
};
use crate::session::{BraveProfile, Monitor, Session, SessionClient};
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// ── Error ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("hyprctl error: {0}")]
    Hyprctl(#[from] HyprctlError),
    #[error("no session found")]
    NoSession,
    #[error("could not unambiguously identify the new '{class}' window; candidates: {addresses}")]
    AmbiguousWindow { class: String, addresses: String },
    #[error("could not safely correlate the new '{class}' window with its launch")]
    UncorrelatedWindow { class: String },
    #[error("window {address} disappeared before reconciliation completed")]
    WindowDisappeared { address: String },
    #[error("window {address} changed before reconciliation completed")]
    WindowIdentityChanged { address: String },
    #[error("launch command '{command}' for {target} is not authorized by app identity or config")]
    UntrustedLaunch { target: String, command: String },
    #[error("binary '{command}' for {target} was not found")]
    MissingLaunchBinary { target: String, command: String },
    #[error("timed out waiting for existing windows to close")]
    ReplaceTimeout,
    #[error("replacement transaction could not be recorded: {0}")]
    Transaction(String),
    #[error("replacement completed but its finalization marker could not be recorded: {0}")]
    TransactionAfterRestore(String),
    #[error("replacement session has no restorable windows")]
    NoRestorableTargets,
    #[error("safety snapshot cannot be recovered safely: {reason}")]
    UnsafeRecoverySnapshot { reason: String },
    #[error("replacement target cannot be recovered safely: {reason}")]
    UnsafeReplacementTarget { reason: String },
    #[error(
        "reconciliation is limited to {limit} saved/current windows (got {targets} saved and {current} current)"
    )]
    TooManyWindows {
        targets: usize,
        current: usize,
        limit: usize,
    },
}

pub const MAX_RECONCILIATION_WINDOWS: usize = 512;

// ── Report ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub skipped: usize,
    pub failed: usize,
    pub details: Vec<String>,
}

/// A small boundary around process creation.  The real implementation is
/// used by the CLI, while tests can provide a deterministic launcher without
/// starting applications on the developer's desktop.
pub trait ProcessLauncher {
    fn spawn(&self, command: &str, args: &[String]) -> Result<LaunchedProcess, std::io::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchedProcess {
    /// PID returned by the launcher.  Some test or desktop launchers cannot
    /// provide one, so reconciliation retains an identity-based fallback.
    pub pid: Option<u32>,
    /// Linux process-start timestamp, when it was available immediately after
    /// launch.  PIDs can be reused, so root-process correlation must not rely
    /// on the number alone when this evidence exists.
    pub start_time: Option<u64>,
}

pub struct RealProcessLauncher;

impl ProcessLauncher for RealProcessLauncher {
    fn spawn(&self, command: &str, args: &[String]) -> Result<LaunchedProcess, std::io::Error> {
        Command::new(command)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|child| {
                let pid = child.id();
                LaunchedProcess {
                    pid: Some(pid),
                    start_time: RealProcessInfo.get_start_time(pid).ok(),
                }
            })
    }
}

/// The outcome of one reconciliation pass.  `unchanged` and `moved` count
/// target windows that were already running; `launched` counts only missing
/// targets that had to be opened.  `extras` are deliberately left alone.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub matched: usize,
    pub unchanged: usize,
    pub moved: usize,
    pub launched: usize,
    pub extras: usize,
    pub skipped: usize,
    pub failed: usize,
    pub details: Vec<String>,
}

/// A current Hyprland window enriched with the bits of process state that are
/// useful for distinguishing multiple terminal windows.
#[derive(Debug, Clone)]
pub struct ObservedClient {
    pub client: HyprClient,
    pub monitor_name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub profile_directory: Option<String>,
    pub process_start_time: Option<u64>,
    /// A process flag is not always window-specific. Chromium-based browsers
    /// commonly reuse one PID for several top-level windows.
    pub profile_identity_ambiguous: bool,
    /// Best-effort command line for the process that owns this window.  It is
    /// supplemental relaunch evidence; it is never used as sole ownership
    /// proof because several windows can share one executable.
    pub process_command: Option<String>,
}

impl ObservedClient {
    pub fn from_hypr_client(
        client: HyprClient,
        monitor_name: Option<String>,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            monitor_name,
            cwd,
            profile_directory: None,
            process_start_time: None,
            profile_identity_ambiguous: false,
            process_command: None,
        }
    }

    pub fn with_profile_directory(
        client: HyprClient,
        monitor_name: Option<String>,
        cwd: Option<PathBuf>,
        profile_directory: Option<String>,
    ) -> Self {
        Self {
            client,
            monitor_name,
            cwd,
            profile_directory,
            process_start_time: None,
            profile_identity_ambiguous: false,
            process_command: None,
        }
    }
}

/// Windows consumed during one reconciliation pass.  Process metadata can be
/// temporarily partial between compositor refreshes, so exact hash-key
/// equality would allow the same live window to be consumed twice.  Stable ID
/// is authoritative when both observations expose it; otherwise the address,
/// PID, and any available process start time provide a conservative fallback.
#[derive(Debug, Default, Clone)]
struct ConsumedWindows {
    entries: Vec<ObservedClient>,
}

impl ConsumedWindows {
    fn contains(&self, observed: &ObservedClient) -> bool {
        self.entries
            .iter()
            .any(|entry| observed_windows_are_same(entry, observed))
    }

    fn insert(&mut self, observed: &ObservedClient) {
        if !self.contains(observed) {
            self.entries.push(observed.clone());
        }
    }
}

fn observed_windows_are_same(left: &ObservedClient, right: &ObservedClient) -> bool {
    let left_stable_id = left.client.stable_id.as_deref().filter(|id| !id.is_empty());
    let right_stable_id = right
        .client
        .stable_id
        .as_deref()
        .filter(|id| !id.is_empty());
    if let (Some(left), Some(right)) = (left_stable_id, right_stable_id) {
        return left.eq_ignore_ascii_case(right);
    }
    if left_stable_id.is_some() || right_stable_id.is_some() {
        // A compositor response which loses a previously visible stable ID is
        // incomplete.  Address/PID evidence cannot prove that a newly
        // created window is the same client, especially when a browser or
        // terminal reuses both the process and address.
        return false;
    }
    if left.process_start_time.is_some() || right.process_start_time.is_some() {
        // Process start time identifies the owner process, not the window.
        // With no stable window ID, treating an address/PID/start-time tuple
        // as a window identity can consume or repair a different window
        // created by that same process.
        return false;
    }
    if left.client.address != right.client.address || left.client.pid != right.client.pid {
        return false;
    }
    match (left.process_start_time, right.process_start_time) {
        (Some(left), Some(right)) => left == right,
        // A transiently unavailable timestamp must not make one live window
        // look new.  Address+PID is still bounded to this compositor pass;
        // a different PID or a positive start-time mismatch was rejected
        // above.
        _ => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    ExactIdentity,
    AppIdentity,
    ProfileIdentity,
    ClassFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcilePair {
    pub target_index: usize,
    pub current_index: usize,
    pub score: i32,
    pub kind: MatchKind,
}

/// Build a deterministic one-to-one assignment between saved targets and the
/// windows that currently exist.  Strong identity evidence (initial class,
/// title, and terminal working directory) wins first; geometry is used as a
/// stable tie-breaker for otherwise identical app windows.
pub fn plan_reconciliation(
    targets: &[SessionClient],
    current: &[ObservedClient],
) -> Vec<Option<ReconcilePair>> {
    plan_reconciliation_with_policy(targets, current, MatchPolicy::Normal, false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchPolicy {
    Normal,
    ReplacementCompletion,
}

fn plan_reconciliation_with_policy(
    targets: &[SessionClient],
    current: &[ObservedClient],
    policy: MatchPolicy,
    require_strong_identity: bool,
) -> Vec<Option<ReconcilePair>> {
    if targets.is_empty() {
        return vec![];
    }

    let mut candidates = vec![vec![None; current.len()]; targets.len()];

    for (target_index, target) in targets.iter().enumerate() {
        for (current_index, observed) in current.iter().enumerate() {
            let candidate = if require_strong_identity {
                allowed_match_with_policy(target, observed, false, policy)
            } else {
                match_score_with_policy(target, observed, policy)
            };
            if let Some((score, kind)) = candidate {
                if relaunch_fallback_identity_matches(target, observed)
                    && current
                        .iter()
                        .filter(|candidate| relaunch_fallback_identity_matches(target, candidate))
                        .count()
                        != 1
                {
                    // A restarted app with two indistinguishable windows is
                    // not safe to assign by geometry alone.  Leave both
                    // untouched and let the caller launch only when there is
                    // a unique fallback candidate.
                    continue;
                }
                if generic_chromium_match_is_safe_with_policy(target, observed, current, policy) {
                    candidates[target_index][current_index] = Some(ReconcilePair {
                        target_index,
                        current_index,
                        score,
                        kind,
                    });
                }
            }
        }
    }

    // Pad the smaller side with zero-weight dummy rows or columns.  A valid
    // match always has a positive score, so zero-weight real/real edges are
    // the optional-unmatched representation: they may be selected by the
    // assignment solver, but are ignored below because there is no candidate.
    // Using max(n, m), instead of adding n+m dummy vertices, keeps the
    // cubic Hungarian pass practical for the bounded reconciliation input.
    let size = targets.len().max(current.len());
    let mut weights = vec![vec![0_i64; size]; size];
    for (target_index, row) in candidates.iter().enumerate() {
        for (current_index, candidate) in row.iter().enumerate() {
            weights[target_index][current_index] =
                candidate.map(|pair| i64::from(pair.score)).unwrap_or(0);
        }
    }

    let assignment = maximum_weight_assignment(&weights);
    let mut plan = vec![None; targets.len()];

    for target_index in 0..targets.len() {
        if let Some(Some(current_index)) = assignment.get(target_index) {
            if *current_index < current.len() {
                plan[target_index] = candidates[target_index][*current_index];
            }
        }
    }

    plan
}

/// Solve a square maximum-weight assignment problem using the Hungarian
/// algorithm.  Returning one column per row keeps the caller's unmatched
/// handling explicit through dummy edges, rather than silently forcing a bad
/// app/window pairing.
fn maximum_weight_assignment(weights: &[Vec<i64>]) -> Vec<Option<usize>> {
    let size = weights.len();
    if size == 0 {
        return vec![];
    }
    assert!(weights.iter().all(|row| row.len() == size));

    // This is the standard primal-dual Hungarian formulation for minimising
    // costs.  Negating weights turns it into the maximum-weight variant.
    let infinity = i64::MAX / 4;
    let mut u = vec![0_i64; size + 1];
    let mut v = vec![0_i64; size + 1];
    let mut p = vec![0_usize; size + 1];
    let mut way = vec![0_usize; size + 1];

    for row in 1..=size {
        p[0] = row;
        let mut column = 0;
        let mut minv = vec![infinity; size + 1];
        let mut used = vec![false; size + 1];

        loop {
            used[column] = true;
            let current_row = p[column];
            let mut delta = infinity;
            let mut next_column = 0;

            for candidate_column in 1..=size {
                if used[candidate_column] {
                    continue;
                }
                let cost = -weights[current_row - 1][candidate_column - 1]
                    - u[current_row]
                    - v[candidate_column];
                if cost < minv[candidate_column] {
                    minv[candidate_column] = cost;
                    way[candidate_column] = column;
                }
                if minv[candidate_column] < delta {
                    delta = minv[candidate_column];
                    next_column = candidate_column;
                }
            }

            for candidate_column in 0..=size {
                if used[candidate_column] {
                    u[p[candidate_column]] += delta;
                    v[candidate_column] -= delta;
                } else if candidate_column > 0 {
                    minv[candidate_column] -= delta;
                }
            }

            column = next_column;
            if p[column] == 0 {
                break;
            }
        }

        loop {
            let previous_column = way[column];
            p[column] = p[previous_column];
            column = previous_column;
            if column == 0 {
                break;
            }
        }
    }

    let mut assignment = vec![None; size];
    for column in 1..=size {
        if p[column] != 0 {
            assignment[p[column] - 1] = Some(column - 1);
        }
    }
    assignment
}

fn match_score(target: &SessionClient, observed: &ObservedClient) -> Option<(i32, MatchKind)> {
    match_score_with_policy(target, observed, MatchPolicy::Normal)
}

fn match_score_with_policy(
    target: &SessionClient,
    observed: &ObservedClient,
    policy: MatchPolicy,
) -> Option<(i32, MatchKind)> {
    let current = &observed.client;
    if !classes_match(target, current) {
        return None;
    }
    let saved_address_matches = target
        .address
        .as_deref()
        .is_some_and(|address| same_nonempty(address, &current.address));
    let captured_cwd_matches = matches!(
        (launch_cwd(target), observed.cwd.as_ref()),
        (Some(target_cwd), Some(current_cwd)) if target_cwd == *current_cwd
    );
    if saved_address_matches {
        if target.stable_id.as_deref().is_none_or(str::is_empty)
            && current
                .stable_id
                .as_deref()
                .is_some_and(|stable_id| !stable_id.is_empty())
        {
            // A legacy snapshot cannot prove that a current stable-ID window
            // at this address is the same client.  Do not downgrade the
            // window-specific compositor identity to PID/address evidence.
            return None;
        }
        match exact_window_identity_matches(target, observed) {
            Some(false) => {
                // Hyprland can recycle an address after a window closes.
                // Never let a positively different identity inherit the old
                // window's placement.
                return None;
            }
            Some(true) => {}
            None if window_identity_is_process_only(target, observed) => {
                // A process identity is not a window identity.  On a
                // compositor without a stable window ID, address reuse by a
                // shared process cannot be distinguished from the captured
                // client, so do not repair this address in place.
                return None;
            }
            None if target_has_identity_evidence(target) => {
                // A partial identity is not proof that an address still
                // belongs to the captured window.  Launch a replacement
                // only after the caller has ruled out an existing window
                // whose identity is temporarily unavailable.
                return None;
            }
            None => {
                // Legacy snapshots without any identity metadata retain the
                // historical address behavior.
            }
        }
    }
    if policy == MatchPolicy::Normal
        && target
            .address
            .as_deref()
            .is_some_and(|address| !address.is_empty() && !saved_address_matches)
        && !relaunch_fallback_identity_matches(target, observed)
    {
        // A current window with a different address is only eligible when a
        // safe relaunch signature (profile, terminal CWD, or executable plus
        // title) proves that it is the restarted target.  Address reuse and
        // generic browser title matches remain ineligible.
        return None;
    }
    if is_generic_chromium_client(target)
        && !generic_chromium_identity_matches_with_policy(target, current, policy)
    {
        // Ordinary Chromium windows share one class and commonly one PID.
        // A title is not window ownership: two tabs can show the same title,
        // and a page title can change after capture.  Reuse only a compositor
        // identity captured for that window; legacy snapshots without one
        // remain safely unmatched.
        return None;
    }
    if policy == MatchPolicy::Normal
        && target.address.as_deref().is_some_and(|address| {
            !address.is_empty()
                && !saved_address_matches
                && !relaunch_fallback_identity_matches(target, observed)
        })
    {
        // A saved address identifies a specific live window.  Once that
        // window is gone, same-class/title evidence is not enough to move a
        // different window into its place; leave it as an extra and launch
        // the missing target instead.
        return None;
    }
    if is_brave_client(target)
        && target
            .profile_directory
            .as_deref()
            .is_none_or(str::is_empty)
    {
        // Older snapshots did not record a profile directory.  A class/title
        // match is not enough to identify which shared Brave profile owns the
        // current window, so never repair it in place.
        return None;
    }
    if is_brave_client(target)
        && (target.profile_identity_ambiguous || observed.profile_identity_ambiguous)
    {
        // A class/title match cannot identify a Brave profile when either
        // side came from a shared or otherwise profile-unknown process.
        return None;
    }
    if target.profile_directory.is_none()
        && !saved_address_matches
        && !captured_cwd_matches
        && strong_identity_conflict(target, observed)
    {
        return None;
    }

    let mut score = 1_000;
    let mut kind = MatchKind::ClassFallback;
    if saved_address_matches {
        // Hyprland's address identifies the exact live window.  Prefer it
        // over geometry when two windows share the same app/title and one has
        // moved since capture.  A missing address remains a normal legacy
        // fallback so a closed/reopened application can still be restored by
        // its app identity.
        score += 5_000;
        kind = MatchKind::ExactIdentity;
    }
    if is_generic_chromium_client(target) {
        score += 2_000;
        kind = MatchKind::ExactIdentity;
    }

    if let Some(target_profile) = &target.profile_directory {
        if let Some(current_profile) = &observed.profile_directory {
            if !same_nonempty(target_profile, current_profile) {
                return None;
            }
            score += 1_200;
            kind = MatchKind::ProfileIdentity;
        } else {
            // A known profile must never consume an unknown Brave window: the
            // browser commonly shares one process across profiles, so a
            // class-only match could silently put the wrong profile in place.
            return None;
        }
    }

    if same_nonempty(&target.class, &current.class) {
        score += 150;
    } else {
        score += 50;
    }

    if same_nonempty(&target.initial_class, &current.initial_class) {
        score += 350;
        if kind != MatchKind::ExactIdentity {
            kind = MatchKind::AppIdentity;
        }
    }

    if same_nonempty(&target.title, &current.title) {
        score += 650;
        kind = MatchKind::ExactIdentity;
    } else if same_nonempty(&target.initial_title, &current.initial_title) {
        score += 450;
        if kind != MatchKind::ExactIdentity {
            kind = MatchKind::AppIdentity;
        }
    } else if titles_similar(&target.title, &current.title) {
        score += 180;
    }

    if let (Some(target_cwd), Some(current_cwd)) = (launch_cwd(target), &observed.cwd) {
        if target_cwd == *current_cwd {
            score += 900;
            kind = MatchKind::ExactIdentity;
        } else {
            // A CWD mismatch is evidence against this candidate, but not a
            // reason to launch a duplicate when it is the only same-app
            // window available.  Reconciliation fixes placement safely; it
            // does not kill a live terminal just to change its shell state.
            score -= 180;
        }
    }

    if workspace_matches(target, current) {
        score += 180;
    }
    if let Some(monitor_name) = &observed.monitor_name {
        if same_nonempty(&target.monitor, monitor_name) {
            score += 140;
        }
    }

    if target.at == current.at {
        score += 120;
    } else {
        score -= manhattan_distance(target.at, current.at).min(120) as i32;
    }
    if target.size == current.size {
        score += 90;
    } else {
        score -= manhattan_distance(target.size, current.size).min(90) as i32;
    }

    Some((score, kind))
}

/// Reject combinations which are positively identified as different windows.
/// Initial titles are compositor-provided stable identity.  A current title
/// can change during normal use, so a current/target title mismatch alone is
/// not enough; when both stable initial titles disagree, however, a same-class
/// fallback would consume an unrelated extra.
fn strong_identity_conflict(target: &SessionClient, observed: &ObservedClient) -> bool {
    let current = &observed.client;
    let title_evidence_agrees = same_nonempty(&target.title, &current.title)
        || same_nonempty(&target.initial_title, &current.initial_title)
        || same_nonempty(&target.title, &current.initial_title)
        || same_nonempty(&target.initial_title, &current.title)
        || titles_similar(&target.title, &current.title);
    let current_title_conflict = !target.title.is_empty()
        && !current.title.is_empty()
        && !same_nonempty(&target.title, &current.title)
        && !titles_similar(&target.title, &current.title);
    let initial_title_conflict = !is_ghostty_class(target)
        && !target.initial_title.is_empty()
        && !current.initial_title.is_empty()
        && !same_nonempty(&target.initial_title, &current.initial_title)
        && !title_evidence_agrees;
    let title_conflict =
        current_title_conflict && !same_nonempty(&target.initial_title, &current.initial_title);
    let cwd_conflict = matches!(
        (launch_cwd(target), observed.cwd.as_ref()),
        (Some(target_cwd), Some(current_cwd)) if target_cwd != *current_cwd
    );

    initial_title_conflict || title_conflict && cwd_conflict
}

/// Compare the optional process identity captured for a window with the
/// process currently reported for it.  `None` means the available session
/// format or provider did not contain enough evidence; callers must not turn
/// that absence into a positive match.
fn process_identity_matches(target: &SessionClient, observed: &ObservedClient) -> Option<bool> {
    let saved_pid = target.pid?;
    let current_pid = observed.client.pid;
    if saved_pid != current_pid {
        return Some(false);
    }
    match (target.process_start_time, observed.process_start_time) {
        (Some(saved), Some(current)) => Some(saved == current),
        // A PID without a start-time match is not sufficient: the PID may
        // have been reused, or the provider may simply have returned partial
        // metadata.  The caller may still use a matching stable window ID.
        (Some(_), None) | (None, _) => None,
    }
}

fn stable_identity_matches(target: &SessionClient, observed: &ObservedClient) -> Option<bool> {
    let saved_stable_id = target.stable_id.as_deref().filter(|id| !id.is_empty());
    let current_stable_id = observed
        .client
        .stable_id
        .as_deref()
        .filter(|id| !id.is_empty());
    match (saved_stable_id, current_stable_id) {
        (Some(saved), Some(current)) => Some(saved.eq_ignore_ascii_case(current)),
        _ => None,
    }
}

fn window_identity_is_process_only(target: &SessionClient, observed: &ObservedClient) -> bool {
    let target_has_stable_id = target
        .stable_id
        .as_deref()
        .is_some_and(|stable_id| !stable_id.is_empty());
    let current_has_stable_id = observed
        .client
        .stable_id
        .as_deref()
        .is_some_and(|stable_id| !stable_id.is_empty());
    !target_has_stable_id
        && !current_has_stable_id
        && (target.pid.is_some()
            || target.process_start_time.is_some()
            || observed.process_start_time.is_some())
}

fn exact_window_identity_matches(
    target: &SessionClient,
    observed: &ObservedClient,
) -> Option<bool> {
    // A captured stable ID is window-specific even when several windows
    // share one browser PID.  If the current compositor does not expose it,
    // do not silently downgrade that snapshot to weaker process evidence.
    if target
        .stable_id
        .as_deref()
        .is_some_and(|stable_id| !stable_id.is_empty())
    {
        return stable_identity_matches(target, observed);
    }
    if observed
        .client
        .stable_id
        .as_deref()
        .is_some_and(|stable_id| !stable_id.is_empty())
    {
        // The saved snapshot predates stable IDs, so a current stable ID has
        // no parity value to compare against.  Do not silently downgrade a
        // window-specific compositor identity to PID-only evidence.
        return None;
    }
    if window_identity_is_process_only(target, observed) {
        // PID/start-time evidence is process-scoped.  It cannot establish
        // that a recycled Hyprland address still belongs to the same window.
        return None;
    }
    process_identity_matches(target, observed)
}

fn title_identity_agrees(target: &SessionClient, observed: &ObservedClient) -> bool {
    let current = &observed.client;
    same_nonempty(&target.title, &current.title)
        || same_nonempty(&target.initial_title, &current.initial_title)
        || same_nonempty(&target.title, &current.initial_title)
        || same_nonempty(&target.initial_title, &current.title)
        || titles_similar(&target.title, &current.title)
}

fn process_command_matches_target(target: &SessionClient, observed: &ObservedClient) -> bool {
    let expected_binary = effective_binary(target);
    let Some(expected) = Path::new(&expected_binary)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let Some(actual) = observed
        .process_command
        .as_deref()
        .and_then(|command| command.split_whitespace().next())
        .and_then(|command| Path::new(command).file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    expected.eq_ignore_ascii_case(actual)
}

/// Identify a restarted non-Chromium target without trusting a title alone.
/// A terminal working directory is the strongest fallback; an executable
/// basename plus title is useful for ordinary single-window applications.
/// Generic Chromium remains deliberately fail-closed because its shared
/// process cannot establish ownership from either signal.
fn relaunch_fallback_identity_matches(target: &SessionClient, observed: &ObservedClient) -> bool {
    if target.address.as_deref().is_none_or(|address| {
        address.is_empty() || same_nonempty(address, &observed.client.address)
    }) || !classes_match(target, &observed.client)
        || is_generic_chromium_client(target)
        || is_webapp_class(target)
    {
        return false;
    }

    if is_brave_client(target) {
        return !target.profile_identity_ambiguous
            && !observed.profile_identity_ambiguous
            && matches!(
                (&target.profile_directory, &observed.profile_directory),
                (Some(target_profile), Some(current_profile))
                    if same_nonempty(target_profile, current_profile)
            );
    }

    let cwd_matches = matches!(
        (launch_cwd(target), observed.cwd.as_ref()),
        (Some(target_cwd), Some(current_cwd)) if target_cwd == *current_cwd
    );
    let command_matches = process_command_matches_target(target, observed);
    title_identity_agrees(target, observed) && (cwd_matches || command_matches)
}

fn target_has_identity_evidence(target: &SessionClient) -> bool {
    target.pid.is_some()
        || target.process_start_time.is_some()
        || target
            .stable_id
            .as_deref()
            .is_some_and(|stable_id| !stable_id.is_empty())
}

fn replacement_safety_identity_matches(
    safety_client: &SessionClient,
    observed: &ObservedClient,
) -> bool {
    if !classes_match(safety_client, &observed.client) {
        return false;
    }
    if exact_window_identity_matches(safety_client, observed) == Some(true) {
        // A stable ID is unique only within the lifetime of the compositor;
        // after a restart its counter can be reused.  Across a changed
        // address, require the relaunch evidence below instead of treating
        // the stable ID alone as proof that this is the old window.
        if safety_client
            .address
            .as_deref()
            .is_some_and(|address| same_nonempty(address, &observed.client.address))
        {
            return true;
        }
    }

    // A reopened window has a new address and usually a new process.  Exact
    // stable title evidence is still useful as a negative proof here: if a
    // current window is indistinguishable from a pre-replacement window, it
    // must not be used to claim that a replacement target was restored.
    same_nonempty(&safety_client.title, &observed.client.title)
        && (safety_client.initial_title.is_empty()
            || observed.client.initial_title.is_empty()
            || same_nonempty(&safety_client.initial_title, &observed.client.initial_title))
        || (same_nonempty(&safety_client.initial_title, &observed.client.initial_title)
            && matches!(
                (launch_cwd(safety_client), observed.cwd.as_ref()),
                (Some(safety_cwd), Some(current_cwd)) if safety_cwd == *current_cwd
            ))
}

/// A same-initial-title match can be a legitimate live window whose title has
/// changed since capture, so ordinary reconciliation keeps it eligible.  A
/// crash-recovery pass has a different safety requirement: it must not claim
/// that an unrelated generic same-class window proves the replacement target
/// was restored.  This predicate is used only by that conservative pass.
fn ambiguous_title_identity(target: &SessionClient, observed: &ObservedClient) -> bool {
    let current = &observed.client;
    target.profile_directory.is_none()
        && launch_cwd(target).is_none()
        && same_nonempty(&target.initial_title, &current.initial_title)
        && !target.title.is_empty()
        && !current.title.is_empty()
        && !same_nonempty(&target.title, &current.title)
        && !titles_similar(&target.title, &current.title)
}

fn allowed_match(
    target: &SessionClient,
    observed: &ObservedClient,
    allow_ambiguous_identity: bool,
) -> Option<(i32, MatchKind)> {
    allowed_match_with_policy(
        target,
        observed,
        allow_ambiguous_identity,
        MatchPolicy::Normal,
    )
}

fn allowed_match_with_policy(
    target: &SessionClient,
    observed: &ObservedClient,
    allow_ambiguous_identity: bool,
    policy: MatchPolicy,
) -> Option<(i32, MatchKind)> {
    if policy == MatchPolicy::ReplacementCompletion
        && !replacement_completion_identity_matches(target, observed)
    {
        return None;
    }
    let (score, kind) = match_score_with_policy(target, observed, policy)?;
    if !allow_ambiguous_identity
        && (kind == MatchKind::ClassFallback || ambiguous_title_identity(target, observed))
    {
        return None;
    }
    Some((score, kind))
}

fn replacement_completion_identity_matches(
    target: &SessionClient,
    observed: &ObservedClient,
) -> bool {
    // Recovery runs after the original target may have been closed and
    // relaunched.  A matching title/class is not ownership proof: an
    // unrelated window can legitimately have the same title.  Only a
    // compositor stable ID or a complete saved process identity can prove
    // that the current window is the captured target.
    let address_matches = target
        .address
        .as_deref()
        .is_some_and(|address| same_nonempty(address, &observed.client.address));
    if address_matches {
        return target
            .stable_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .zip(
                observed
                    .client
                    .stable_id
                    .as_deref()
                    .filter(|id| !id.is_empty()),
            )
            .is_some_and(|(target_id, current_id)| target_id.eq_ignore_ascii_case(current_id));
    }
    replacement_relaunch_identity_matches(target, observed)
}

fn replacement_relaunch_identity_matches(
    target: &SessionClient,
    observed: &ObservedClient,
) -> bool {
    if target.address.as_deref().is_none_or(|address| {
        address.is_empty() || same_nonempty(address, &observed.client.address)
    }) || !classes_match(target, &observed.client)
    {
        return false;
    }
    if is_generic_chromium_client(target) {
        return false;
    }
    if is_brave_client(target) {
        return !target.profile_identity_ambiguous
            && !observed.profile_identity_ambiguous
            && matches!(
                (&target.profile_directory, &observed.profile_directory),
                (Some(target_profile), Some(current_profile))
                    if same_nonempty(target_profile, current_profile)
            );
    }
    if is_webapp_class(target) {
        // The generated Omarchy web-app class encodes the site/profile.  It is
        // the only browser identity we can safely use after a focus-launcher
        // hands the request to an existing Chromium process.
        return same_nonempty(&target.class, &observed.client.class)
            || same_nonempty(&target.initial_class, &observed.client.initial_class);
    }

    // Recovery must not declare success from an executable basename and a
    // title alone: another same-app window can legitimately have both.  A
    // terminal CWD plus the owning command and title gives the relaunch proof
    // used here without allowing a generic same-title extra to complete the
    // destructive transaction.
    matches!(
        (launch_cwd(target), observed.cwd.as_ref()),
        (Some(target_cwd), Some(current_cwd)) if target_cwd == *current_cwd
    ) && process_command_matches_target(target, observed)
        && title_identity_agrees(target, observed)
}

fn classes_match(target: &SessionClient, current: &HyprClient) -> bool {
    // Only compare the same identity field.  Cross-field matching creates a
    // transitive false positive when one app's runtime class happens to equal
    // another app's initial class (for example target wrapper/app-a versus
    // current app-a/app-b).  Empty initial-class fields are the legacy format
    // and intentionally fall back to the runtime class comparison above.
    same_nonempty(&target.class, &current.class)
        || same_nonempty(&target.initial_class, &current.initial_class)
}

fn is_brave_client(client: &SessionClient) -> bool {
    is_brave_class(&client.class) || is_brave_class(&client.initial_class)
}

fn brave_client_has_no_safe_profile_identity(client: &SessionClient) -> bool {
    is_brave_client(client)
        && (client.profile_identity_ambiguous
            || client
                .profile_directory
                .as_deref()
                .is_none_or(str::is_empty))
}

/// Ensure a safety snapshot can be used before a destructive replacement
/// starts.  A Brave process can own several windows while exposing several
/// profile flags, but Hyprland does not tell us which flag belongs to which
/// window.  Starting replacement in that state would make recovery guess and
/// could silently lose windows, so fail before closing anything.
pub fn validate_safety_snapshot(session: &Session) -> Result<(), RestoreError> {
    if let Some(client) = session
        .clients
        .iter()
        .find(|client| brave_client_has_no_safe_profile_identity(client))
    {
        return Err(RestoreError::UnsafeRecoverySnapshot {
            reason: format!(
                "Brave window '{}' has no unique profile identity",
                client.title
            ),
        });
    }
    Ok(())
}

/// Validate the complete safety snapshot before replacement closes anything.
/// The Brave-specific check above protects profile identity; this variant also
/// verifies every executable that the recovery pass would need to relaunch.
/// A safety snapshot is captured with recovery filters, so silently accepting a
/// missing or untrusted command here would leave the user with no recoverable
/// copy after a failed replacement.
pub fn validate_safety_snapshot_with_config(
    session: &Session,
    config: &Config,
) -> Result<(), RestoreError> {
    validate_safety_snapshot(session)?;
    if let Some(client) = session.clients.iter().find(|client| {
        client
            .address
            .as_deref()
            .is_none_or(|address| address.is_empty())
    }) {
        return Err(RestoreError::UnsafeRecoverySnapshot {
            reason: format!(
                "captured window '{}' has no Hyprland address for safe replacement",
                client.title
            ),
        });
    }
    if let Some(client) = session.clients.iter().find(|client| {
        client
            .stable_id
            .as_deref()
            .is_none_or(|stable_id| stable_id.is_empty())
    }) {
        return Err(RestoreError::UnsafeRecoverySnapshot {
            reason: format!(
                "captured window '{}' has no Hyprland stable window ID for safe replacement",
                client.title
            ),
        });
    }
    let expected_targets = session
        .clients
        .iter()
        .filter(|client| {
            !is_ignored_class(&client.class, &config.filters.ignore_classes)
                && !is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
        })
        .count();
    let targets = build_reconcile_targets(session, config);
    if targets.len() != expected_targets {
        return Err(RestoreError::UnsafeRecoverySnapshot {
            reason: format!(
                "only {} of {} captured windows have a recoverable launch target",
                targets.len(),
                expected_targets
            ),
        });
    }
    for target in targets {
        let launch_command = build_launch_command(&target.client);
        if !launch_command_is_trusted(&target.client, config) {
            return Err(RestoreError::UnsafeRecoverySnapshot {
                reason: format!(
                    "launch command '{}' for {} is not authorized by app identity or config",
                    launch_command[0], target.label
                ),
            });
        }
        if resolve_launch_binary(&launch_command[0], &target.label).is_err() {
            return Err(RestoreError::UnsafeRecoverySnapshot {
                reason: format!(
                    "binary '{}' for {} is not available",
                    launch_command[0], target.label
                ),
            });
        }
    }
    Ok(())
}

fn is_generic_chromium_client(client: &SessionClient) -> bool {
    (is_generic_chromium_class(&client.class) || is_generic_chromium_class(&client.initial_class))
        && ![client.class.as_str(), client.initial_class.as_str()]
            .iter()
            .any(|class| is_webapp_class_name(class))
}

fn is_browser_target(client: &SessionClient) -> bool {
    is_brave_client(client) || is_generic_chromium_client(client) || is_webapp_class(client)
}

fn browser_handoff_target_is_safe(client: &SessionClient) -> bool {
    is_webapp_class(client)
        || (is_brave_client(client)
            && client
                .profile_directory
                .as_deref()
                .is_some_and(|profile| !profile.is_empty())
            && !client.profile_identity_ambiguous)
}

fn browser_handoff_candidate_is_safe(
    target: &SessionClient,
    candidate: &HyprClient,
    before: &HashSet<String>,
    active_before: Option<&str>,
    active_after: Option<&str>,
    process_info: &dyn ProcessInfoProvider,
) -> bool {
    if !browser_handoff_target_is_safe(target)
        || candidate.stable_id.as_deref().is_none_or(str::is_empty)
        || !same_nonempty(active_after.unwrap_or_default(), &candidate.address)
    {
        return false;
    }
    let active_transition = active_before
        .map(|address| !same_nonempty(address, &candidate.address))
        .unwrap_or(false);
    let is_new_address = !before.contains(&candidate.address);
    if !is_new_address && !active_transition {
        return false;
    }
    if is_brave_client(target) {
        let Some(target_profile) = target.profile_directory.as_deref() else {
            return false;
        };
        let discovery = find_profile_discovery(process_info, candidate.pid);
        return discovery.complete
            && matches!(discovery.profiles.as_slice(), [profile]
                if same_nonempty(profile, target_profile));
    }
    is_webapp_class(target)
        && (same_nonempty(&target.class, &candidate.class)
            || same_nonempty(&target.initial_class, &candidate.initial_class))
}

fn is_brave_hypr_client(client: &HyprClient) -> bool {
    is_brave_class(&client.class) || is_brave_class(&client.initial_class)
}

fn is_brave_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("brave-browser")
}

fn is_generic_chromium_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("chromium")
}

fn generic_chromium_identity_matches_with_policy(
    target: &SessionClient,
    current: &HyprClient,
    policy: MatchPolicy,
) -> bool {
    if target
        .stable_id
        .as_deref()
        .zip(current.stable_id.as_deref())
        .is_some_and(|(target_id, current_id)| {
            target_id.eq_ignore_ascii_case(current_id)
                && target
                    .address
                    .as_deref()
                    .is_some_and(|address| same_nonempty(address, &current.address))
        })
    {
        return true;
    }
    let Some(target_address) = target.address.as_deref() else {
        let _ = policy;
        // focusHistoryID is a recency attribute, not a persisted window
        // identity.  It can be reused by an unrelated Chromium window after
        // a compositor restart, so an addressless legacy browser target must
        // remain unmatched and be handled by the safe launch path.
        return false;
    };
    if !target_address.is_empty() && same_nonempty(target_address, &current.address) {
        return true;
    }
    match policy {
        MatchPolicy::Normal => false,
        MatchPolicy::ReplacementCompletion => false,
    }
}

fn generic_chromium_match_is_safe(
    target: &SessionClient,
    observed: &ObservedClient,
    candidates: &[ObservedClient],
) -> bool {
    generic_chromium_match_is_safe_with_policy(target, observed, candidates, MatchPolicy::Normal)
}

fn generic_chromium_match_is_safe_with_policy(
    target: &SessionClient,
    observed: &ObservedClient,
    candidates: &[ObservedClient],
    policy: MatchPolicy,
) -> bool {
    if !is_generic_chromium_client(target) {
        return true;
    }

    let matching = candidates
        .iter()
        .filter(|candidate| {
            classes_match(target, &candidate.client)
                && generic_chromium_identity_matches_with_policy(target, &candidate.client, policy)
        })
        .collect::<Vec<_>>();
    matching.len() == 1 && matching[0].client.address == observed.client.address
}

fn same_nonempty(left: &str, right: &str) -> bool {
    !left.is_empty() && !right.is_empty() && left.eq_ignore_ascii_case(right)
}

fn titles_similar(left: &str, right: &str) -> bool {
    let left = normalize_title(left);
    let right = normalize_title(right);
    !left.is_empty() && !right.is_empty() && (left.contains(&right) || right.contains(&left))
}

fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn manhattan_distance(left: [i32; 2], right: [i32; 2]) -> i64 {
    (i64::from(left[0]) - i64::from(right[0])).abs()
        + (i64::from(left[1]) - i64::from(right[1])).abs()
}

fn launch_cwd(client: &SessionClient) -> Option<PathBuf> {
    let args = &client.launch.args;
    for (index, arg) in args.iter().enumerate() {
        if arg == "--directory" || arg == "--working-directory" || arg == "-d" {
            if let Some(value) = args.get(index + 1) {
                return Some(PathBuf::from(value));
            }
        }
        if let Some(value) = arg.strip_prefix("--directory=") {
            return Some(PathBuf::from(value));
        }
        if let Some(value) = arg.strip_prefix("--working-directory=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Restore a saved [`Session`] by launching every client and positioning
/// its window via `hyprctl dispatch`.
///
/// When `dry_run` is `true` no processes are spawned and no dispatches are
/// sent; the `details` field of the returned report lists what *would* have
/// been executed.
pub fn restore_session(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<RestoreReport, RestoreError> {
    let process_info = RealProcessInfo;
    restore_session_with_process_info(session, hyprctl, &process_info, config, dry_run, verbose)
}

fn restore_session_with_process_info(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<RestoreReport, RestoreError> {
    let target_count = build_reconcile_targets(session, config).len();
    if target_count > MAX_RECONCILIATION_WINDOWS {
        return Err(RestoreError::TooManyWindows {
            targets: target_count,
            current: 0,
            limit: MAX_RECONCILIATION_WINDOWS,
        });
    }
    let mut report = RestoreReport::default();

    // Fetch current windows once to detect already-running duplicates.
    let mut consumed_existing = ConsumedWindows::default();
    let current_monitors = hyprctl.get_monitors()?;

    // Detect if profile-based Brave restore applies.  An empty profile result
    // can mean an intentionally empty allowlist, but it can also mean that a
    // legacy snapshot contains Brave windows whose profile identity cannot be
    // recovered.  The latter must be reported as a failure, not a successful
    // no-op.
    let brave_profiles = brave_profile_targets(session, config);
    let has_brave_profiles = brave_profiles.is_some();
    let explicit_empty_brave_mapping = app_config_for(config, "brave-browser", "")
        .and_then(|app| app.profile_workspaces.as_ref())
        .is_some_and(HashMap::is_empty);
    let unsafe_empty_brave_profile_mode = has_brave_profiles
        && !explicit_empty_brave_mapping
        && brave_profiles.as_ref().is_some_and(Vec::is_empty)
        && session
            .clients
            .iter()
            .any(brave_client_has_no_safe_profile_identity);

    // Group by workspace (BTreeMap gives us sorted workspace order for free).
    let mut by_workspace: BTreeMap<i32, Vec<&SessionClient>> = BTreeMap::new();
    for client in &session.clients {
        by_workspace
            .entry(client.workspace)
            .or_default()
            .push(client);
    }

    for (ws, mut clients) in by_workspace {
        // Sort within each workspace: top row first, then left-to-right.
        clients.sort_by(|a, b| a.at[1].cmp(&b.at[1]).then(a.at[0].cmp(&b.at[0])));

        for client in clients {
            if unsafe_empty_brave_profile_mode && is_brave_client(client) {
                report.failed += 1;
                if verbose {
                    report.details.push(format!(
                        "FAIL: {} has no safe Brave profile identity to restore",
                        client.title
                    ));
                }
                continue;
            }
            // Skip brave-browser windows when profiles are available (handled after main loop).
            if has_brave_profiles && is_brave_client(client) {
                continue;
            }
            if is_ignored_class(&client.class, &config.filters.ignore_classes)
                || is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
            {
                report.skipped += 1;
                if verbose {
                    report
                        .details
                        .push(format!("SKIP: ignored class '{}'", client.class));
                }
                continue;
            }
            if brave_client_has_no_safe_profile_identity(client) {
                // This is a requested window, not an optional extra.  A
                // plain restore cannot identify its Brave profile safely, so
                // report an actionable failure instead of returning success
                // after silently omitting it.
                report.failed += 1;
                if verbose {
                    report.details.push(format!(
                        "FAIL: {} has no safe Brave profile identity",
                        client.title
                    ));
                }
                continue;
            }
            let target_client = hydrate_webapp_launch(client, config);
            let target_monitor = find_monitor_by_name(&current_monitors, &target_client.monitor);
            let target_monitor_available =
                target_monitor_is_available(&current_monitors, &target_client.monitor);
            let restore_client =
                adapt_client_geometry(&target_client, &session.monitors, target_monitor);

            // An existing target is repaired in place, even when it is on a
            // different workspace or monitor.  This keeps the legacy command
            // useful on its own while sharing the same placement semantics as
            // --reconcile.
            let current_existing =
                observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
            if current_existing.len() > MAX_RECONCILIATION_WINDOWS {
                return Err(RestoreError::TooManyWindows {
                    targets: target_count,
                    current: current_existing.len(),
                    limit: MAX_RECONCILIATION_WINDOWS,
                });
            }
            if let Some(existing_index) = find_existing_restore_match(
                &target_client,
                &current_existing,
                &consumed_existing,
                config,
            ) {
                let current = current_existing[existing_index].clone();
                consumed_existing.insert(&current);
                let commands = build_reconcile_dispatch_commands_with_geometry(
                    &restore_client,
                    &current.client,
                    current.monitor_name.as_deref(),
                    restore_client.at,
                    restore_client.size,
                    target_monitor_available,
                );
                if commands.is_empty() {
                    report.details.push(format!(
                        "{}: {} already on ws={}",
                        if dry_run { "[dry-run]" } else { "SKIP" },
                        current.client.class,
                        target_client.workspace
                    ));
                    report.skipped += 1;
                } else if dry_run {
                    report.details.push(format!(
                        "[dry-run] repair {} at address {}",
                        target_client.class, current.client.address
                    ));
                    for command in &commands {
                        report.details.push(format!("  hyprctl dispatch {command}"));
                    }
                    report.restored += 1;
                } else {
                    match dispatch_existing_repairs(&current, &commands, hyprctl, process_info) {
                        Ok(()) => {
                            report.restored += 1;
                            if verbose {
                                report.details.push(format!(
                                    "OK: repaired {} at address {}",
                                    target_client.class, current.client.address
                                ));
                            }
                        }
                        Err(error) => {
                            report.failed += 1;
                            report.details.push(format!(
                                "FAIL: {} at address {} — {error}",
                                target_client.class, current.client.address
                            ));
                        }
                    }
                }
                continue;
            }

            if has_unavailable_identity_candidate(
                &target_client,
                &current_existing,
                &consumed_existing,
                config,
            ) {
                report.failed += 1;
                report.details.push(format!(
                    "FAIL: {} could not be safely matched because window identity is unavailable",
                    target_client.class
                ));
                continue;
            }

            if has_ambiguous_generic_chromium_candidate(
                &target_client,
                &current_existing,
                &consumed_existing,
                config,
            ) {
                report.failed += 1;
                report.details.push(format!(
                    "FAIL: {} could not be safely matched because generic Chromium window identity is unavailable",
                    target_client.class
                ));
                continue;
            }

            if dry_run {
                let launch_command = build_launch_command(&restore_client);
                if !launch_command_is_trusted(&restore_client, config) {
                    report.details.push(format!(
                        "[dry-run] FAIL: launch command '{}' for {} is not authorized by app identity or config",
                        launch_command[0], target_client.class
                    ));
                    report.failed += 1;
                    continue;
                }
                if resolve_launch_binary(&launch_command[0], &target_client.class).is_err() {
                    report.details.push(format!(
                        "[dry-run] FAIL: binary '{}' not found for {}",
                        launch_command[0], target_client.class
                    ));
                    report.failed += 1;
                    continue;
                }
                let cmds =
                    build_dispatch_commands_for_monitor(&restore_client, target_monitor_available);
                report.details.push(format!(
                    "[dry-run] ws={} {} → {}",
                    ws, target_client.class, target_client.launch.command
                ));
                for cmd in &cmds {
                    report.details.push(format!("  hyprctl dispatch {cmd}"));
                }
                report.restored += 1;
                continue;
            }

            // Validate the effective binary is available before attempting to spawn.
            let launch_command = build_launch_command(&restore_client);
            if !launch_command_is_trusted(&restore_client, config) {
                report.details.push(format!(
                    "FAIL: launch command '{}' for {} is not authorized by app identity or config",
                    launch_command[0], target_client.class
                ));
                report.failed += 1;
                continue;
            }
            if resolve_launch_binary(&launch_command[0], &target_client.class).is_err() {
                report.details.push(format!(
                    "FAIL: binary '{}' not found for {}",
                    launch_command[0], target_client.class
                ));
                report.failed += 1;
                continue;
            }

            match restore_single_client_with_launcher_and_process_info_with_address(
                &restore_client,
                hyprctl,
                process_info,
                config,
                &RealProcessLauncher,
                target_monitor_available,
            ) {
                Ok(restored) => {
                    consumed_existing.insert(&restored.observed);
                    if verbose {
                        report.details.push(restored.message);
                    }
                    report.restored += 1;
                }
                Err(e) => {
                    let msg = format!("FAIL: {} — {e}", client.class);
                    report.details.push(msg);
                    report.failed += 1;
                }
            }
        }
    }

    // Restore Brave profiles (one window per profile) through the same
    // correlation and placement path as every other client.  Chromium may
    // reuse a browser process, so selecting the first newly-created class
    // match is unsafe when two profiles start together.
    if has_brave_profiles {
        let profile_targets: Vec<ReconcileTarget> = build_reconcile_targets(session, config)
            .into_iter()
            .filter(|target| {
                is_brave_client(&target.client) && target.client.profile_directory.is_some()
            })
            .collect();
        let launcher = RealProcessLauncher;

        for mut target in profile_targets {
            let target_monitor = find_monitor_by_name(&current_monitors, &target.client.monitor);
            let target_monitor_available =
                target_monitor_is_available(&current_monitors, &target.client.monitor);
            target.client =
                adapt_client_geometry(&target.client, &session.monitors, target_monitor);

            // Profile-aware normal restore is idempotent too.  Reuse a
            // currently open window whose profile was positively identified
            // before considering a new browser launch; otherwise every plain
            // `restore` would duplicate already-open Brave profiles.
            let current_existing =
                observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
            if current_existing.len() > MAX_RECONCILIATION_WINDOWS {
                return Err(RestoreError::TooManyWindows {
                    targets: target_count,
                    current: current_existing.len(),
                    limit: MAX_RECONCILIATION_WINDOWS,
                });
            }
            if let Some(existing_index) = find_existing_restore_match(
                &target.client,
                &current_existing,
                &consumed_existing,
                config,
            ) {
                let current = current_existing[existing_index].clone();
                consumed_existing.insert(&current);
                let commands = build_reconcile_dispatch_commands_with_geometry(
                    &target.client,
                    &current.client,
                    current.monitor_name.as_deref(),
                    target.client.at,
                    target.client.size,
                    target_monitor_available,
                );
                if commands.is_empty() {
                    report.details.push(format!(
                        "SKIP: {} already on ws={}",
                        current.client.class, target.client.workspace
                    ));
                    report.skipped += 1;
                } else {
                    match dispatch_existing_repairs(&current, &commands, hyprctl, process_info) {
                        Ok(()) => {
                            report.restored += 1;
                            if verbose {
                                report.details.push(format!(
                                    "OK: repaired {} at address {}",
                                    target.label, current.client.address
                                ));
                            }
                        }
                        Err(error) => {
                            report.failed += 1;
                            report.details.push(format!(
                                "FAIL: {} at address {} — {error}",
                                target.label, current.client.address
                            ));
                        }
                    }
                }
                continue;
            }

            if has_unavailable_identity_candidate(
                &target.client,
                &current_existing,
                &consumed_existing,
                config,
            ) {
                report.failed += 1;
                report.details.push(format!(
                    "FAIL: {} could not be safely matched because window identity is unavailable",
                    target.label
                ));
                continue;
            }

            if target.client.profile_identity_ambiguous
                || has_ambiguous_profile_candidate(
                    &target.client,
                    &current_existing,
                    &consumed_existing,
                    config,
                )
            {
                report.details.push(format!(
                    "SKIP: {} could not be safely matched because Brave profile identity is ambiguous",
                    target.label
                ));
                report.skipped += 1;
                continue;
            }

            if dry_run {
                let launch_command = build_launch_command(&target.client);
                if !launch_command_is_trusted(&target.client, config) {
                    report.details.push(format!(
                        "[dry-run] FAIL: launch command '{}' for {} is not authorized by app identity or config",
                        launch_command[0], target.label
                    ));
                    report.failed += 1;
                    continue;
                }
                if resolve_launch_binary(&launch_command[0], &target.label).is_err() {
                    report.details.push(format!(
                        "[dry-run] FAIL: binary '{}' not found for {}",
                        launch_command[0], target.label
                    ));
                    report.failed += 1;
                    continue;
                }
                report.details.push(format!(
                    "[dry-run] {} → ws={}",
                    target.label, target.client.workspace
                ));
                for command in
                    build_dispatch_commands_for_monitor(&target.client, target_monitor_available)
                {
                    report.details.push(format!("  hyprctl dispatch {command}"));
                }
                report.restored += 1;
                continue;
            }

            let launch_command = build_launch_command(&target.client);
            if !launch_command_is_trusted(&target.client, config) {
                report.details.push(format!(
                    "FAIL: launch command '{}' for {} is not authorized by app identity or config",
                    launch_command[0], target.label
                ));
                report.failed += 1;
                continue;
            }
            if resolve_launch_binary(&launch_command[0], &target.label).is_err() {
                report.details.push(format!(
                    "FAIL: binary '{}' not found for {}",
                    launch_command[0], target.label
                ));
                // A profile target that cannot be launched is not an
                // intentional safety skip: the requested window is missing
                // and the caller must receive a failing exit status.
                report.failed += 1;
                continue;
            }

            match restore_single_client_with_launcher_and_process_info_with_address(
                &target.client,
                hyprctl,
                process_info,
                config,
                &launcher,
                target_monitor_available,
            ) {
                Ok(restored) => {
                    consumed_existing.insert(&restored.observed);
                    if verbose {
                        report.details.push(restored.message);
                    }
                    report.restored += 1;
                }
                Err(error) => {
                    report
                        .details
                        .push(format!("FAIL: {} — {error}", target.label));
                    report.failed += 1;
                }
            }
        }
    }

    Ok(report)
}

fn find_existing_restore_match(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
) -> Option<usize> {
    find_existing_restore_match_with_policy(target, existing, consumed, config, true)
}

fn find_existing_restore_match_with_policy(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
    allow_ambiguous_identity: bool,
) -> Option<usize> {
    find_existing_restore_match_with_policy_excluding(
        target,
        existing,
        consumed,
        config,
        allow_ambiguous_identity,
        None,
    )
}

fn find_existing_restore_match_with_policy_excluding(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
    allow_ambiguous_identity: bool,
    protected_addresses: Option<&HashSet<String>>,
) -> Option<usize> {
    let available = existing
        .iter()
        .enumerate()
        .filter(|(_, current)| {
            !consumed.contains(current)
                && !protected_address(&current.client.address, protected_addresses)
                && !is_ignored_class(&current.client.class, &config.filters.ignore_classes)
                && !is_ignored_class(
                    &current.client.initial_class,
                    &config.filters.ignore_classes,
                )
        })
        .collect::<Vec<_>>();
    let available_clients = available
        .iter()
        .map(|(_, current)| (*current).clone())
        .collect::<Vec<_>>();
    available
        .into_iter()
        .filter_map(|(index, current)| {
            if !generic_chromium_match_is_safe(target, current, &available_clients) {
                return None;
            }
            if relaunch_fallback_identity_matches(target, current)
                && available_clients
                    .iter()
                    .filter(|candidate| relaunch_fallback_identity_matches(target, candidate))
                    .count()
                    != 1
            {
                return None;
            }
            allowed_match(target, current, allow_ambiguous_identity)
                .map(|(score, _)| (score, current.client.address.clone(), index))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)))
        .map(|(_, _, index)| index)
}

fn protected_address(address: &str, protected_addresses: Option<&HashSet<String>>) -> bool {
    protected_addresses.is_some_and(|addresses| addresses.contains(address))
}

fn has_ambiguous_profile_candidate(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
) -> bool {
    is_brave_client(target)
        && existing.iter().any(|current| {
            !consumed.contains(current)
                && current.profile_identity_ambiguous
                && !is_ignored_class(&current.client.class, &config.filters.ignore_classes)
                && !is_ignored_class(
                    &current.client.initial_class,
                    &config.filters.ignore_classes,
                )
                && classes_match(target, &current.client)
        })
}

fn has_ambiguous_generic_chromium_candidate(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
) -> bool {
    is_generic_chromium_client(target)
        && existing.iter().any(|current| {
            !consumed.contains(current)
                && !is_ignored_class(&current.client.class, &config.filters.ignore_classes)
                && !is_ignored_class(
                    &current.client.initial_class,
                    &config.filters.ignore_classes,
                )
                && classes_match(target, &current.client)
        })
}

fn has_unmatched_same_class_candidate(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
) -> bool {
    existing.iter().any(|current| {
        !consumed.contains(current)
            && !is_ignored_class(&current.client.class, &config.filters.ignore_classes)
            && !is_ignored_class(
                &current.client.initial_class,
                &config.filters.ignore_classes,
            )
            && classes_match(target, &current.client)
    })
}

/// Return true when a current same-address window may still be the captured
/// target, but the identity evidence needed to prove that is unavailable.
/// Callers must skip it rather than launching a duplicate.  A positive PID,
/// start-time, stable-ID, or profile mismatch is different: that proves the
/// address now belongs to another window and the missing target may be
/// launched normally.
fn identity_unavailable_for_same_address(
    target: &SessionClient,
    observed: &ObservedClient,
) -> bool {
    let Some(saved_address) = target.address.as_deref() else {
        return false;
    };
    if saved_address.is_empty() || !same_nonempty(saved_address, &observed.client.address) {
        return false;
    }

    let target_stable_id = target.stable_id.as_deref().filter(|id| !id.is_empty());
    let current_stable_id = observed
        .client
        .stable_id
        .as_deref()
        .filter(|id| !id.is_empty());
    if target_stable_id.is_some() && current_stable_id.is_some() {
        return false;
    }

    if target.pid.is_some_and(|pid| pid != observed.client.pid) {
        return false;
    }
    if let (Some(target_start), Some(current_start)) =
        (target.process_start_time, observed.process_start_time)
    {
        if target_start != current_start {
            return false;
        }
    }
    if let (Some(target_profile), Some(current_profile)) =
        (&target.profile_directory, &observed.profile_directory)
    {
        if !same_nonempty(target_profile, current_profile) {
            return false;
        }
    }

    if target_stable_id.is_none() && current_stable_id.is_some() {
        // The current compositor has window-specific identity that the saved
        // snapshot lacks.  Address/PID parity cannot distinguish a recycled
        // browser or app window in this state.
        return true;
    }
    if target_stable_id.is_some() && current_stable_id.is_none() {
        return true;
    }
    if window_identity_is_process_only(target, observed) {
        return true;
    }
    target.profile_directory.is_some()
        || target_has_identity_evidence(target)
            && exact_window_identity_matches(target, observed).is_none()
}

fn has_unavailable_identity_candidate(
    target: &SessionClient,
    existing: &[ObservedClient],
    consumed: &ConsumedWindows,
    config: &Config,
) -> bool {
    existing.iter().any(|current| {
        !consumed.contains(current)
            && !is_ignored_class(&current.client.class, &config.filters.ignore_classes)
            && !is_ignored_class(
                &current.client.initial_class,
                &config.filters.ignore_classes,
            )
            && classes_match(target, &current.client)
            && identity_unavailable_for_same_address(target, current)
    })
}

fn dispatch_existing_repairs(
    expected: &ObservedClient,
    commands: &[String],
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
) -> Result<(), RestoreError> {
    let address = &expected.client.address;
    let clients = hyprctl.get_clients()?;
    let Some(current) = clients.iter().find(|client| client.address == *address) else {
        return Err(RestoreError::WindowDisappeared {
            address: address.clone(),
        });
    };
    if !observed_window_identity_matches(expected, current, process_info)
        || identity_is_ambiguous_without_stable_id(expected, current, &clients)
    {
        return Err(RestoreError::WindowIdentityChanged {
            address: address.clone(),
        });
    }
    hyprctl.dispatch_batch(commands)?;
    Ok(())
}

/// A PID and process start time identify a process, not necessarily a window.
/// When two current clients share both and neither exposes Hyprland's
/// window-specific stable ID, address/PID/start-time evidence cannot tell
/// which one a stale observation referred to.  Refuse address-based actions
/// in that case rather than risk repairing or closing the sibling window.
fn identity_is_ambiguous_without_stable_id(
    expected: &ObservedClient,
    current: &HyprClient,
    clients: &[HyprClient],
) -> bool {
    let expected_has_stable_id = expected
        .client
        .stable_id
        .as_deref()
        .is_some_and(|stable_id| !stable_id.is_empty());
    let current_has_stable_id = current
        .stable_id
        .as_deref()
        .is_some_and(|stable_id| !stable_id.is_empty());
    !expected_has_stable_id
        && !current_has_stable_id
        && expected.client.address == current.address
        && expected.client.pid == current.pid
        && clients
            .iter()
            .filter(|client| client.pid == current.pid)
            .count()
            > 1
}

/// Revalidate the window identity immediately before dispatching address-based
/// commands.  Hyprland's address is the dispatch handle, but it can be
/// recycled after a window closes; stable ID, PID, and process start time
/// catch that replacement before a repair is applied to the wrong window.
fn observed_window_identity_matches(
    expected: &ObservedClient,
    current: &HyprClient,
    process_info: &dyn ProcessInfoProvider,
) -> bool {
    if expected.client.address != current.address {
        return false;
    }
    match (
        expected
            .client
            .stable_id
            .as_deref()
            .filter(|id| !id.is_empty()),
        current.stable_id.as_deref().filter(|id| !id.is_empty()),
    ) {
        (Some(expected), Some(current)) => {
            // A stable ID is window-specific and remains authoritative even
            // when a browser hands the window to another process.
            return expected.eq_ignore_ascii_case(current);
        }
        (Some(_), None) | (None, Some(_)) => {
            // Missing parity cannot safely be downgraded to PID identity.
            return false;
        }
        (None, None) if process_info.has_reliable_process_start_time() => {
            // A reliable process provider is available, but the compositor
            // did not provide a window-specific stable ID.  PID/start-time
            // can prove process ownership only, so an address may have been
            // recycled for a sibling window from that same process.
            return false;
        }
        (None, None) => {}
    }
    if expected.client.pid != current.pid {
        return false;
    }
    match expected.process_start_time {
        Some(expected_start) => process_info
            .get_start_time(current.pid)
            .ok()
            .is_some_and(|current_start| current_start == expected_start),
        // PID equality is the best available fallback only for older
        // Hyprland versions or providers which explicitly do not support
        // process start times.  A reliable provider returning no timestamp
        // is partial evidence, so fail closed.
        None => !process_info.has_reliable_process_start_time(),
    }
}

/// Verify that a client at a captured address is still the client that was in
/// the safety snapshot.  The replacement path uses this extra check because
/// its initial client list is taken after the snapshot was written: a window
/// opened in that interval must remain untouched, even if it happens to reuse
/// an address from the captured desktop.
fn safety_snapshot_identity_matches(
    safety_client: &SessionClient,
    current: &HyprClient,
    process_info: &dyn ProcessInfoProvider,
) -> bool {
    if safety_client.address.as_deref() != Some(current.address.as_str())
        || !classes_match(safety_client, current)
    {
        return false;
    }

    let Some(safety_stable_id) = safety_client
        .stable_id
        .as_deref()
        .filter(|stable_id| !stable_id.is_empty())
    else {
        return false;
    };
    let Some(current_stable_id) = current
        .stable_id
        .as_deref()
        .filter(|stable_id| !stable_id.is_empty())
    else {
        return false;
    };
    if !safety_stable_id.eq_ignore_ascii_case(current_stable_id) {
        return false;
    }

    let mut observed = ObservedClient::from_hypr_client(current.clone(), None, None);
    observed.process_start_time = process_info.get_start_time(current.pid).ok();
    if exact_window_identity_matches(safety_client, &observed) != Some(true) {
        return false;
    }

    // Initial title is compositor-owned identity evidence.  It does not
    // change during the normal lifetime of a window, so a mismatch indicates
    // address reuse even when a process was handed the recycled address.
    safety_client.initial_title.is_empty()
        || current.initial_title.is_empty()
        || same_nonempty(&safety_client.initial_title, &current.initial_title)
}

/// Validate every executable that a replacement will need before any current
/// window is closed.  Unlike an ordinary reconcile pass, replacement will
/// make every target missing, so every launch command must be trusted and
/// resolvable up front.
pub fn validate_replacement_targets(
    session: &Session,
    config: &Config,
) -> Result<(), RestoreError> {
    validate_replacement_targets_with_binaries(session, config).map(|_| ())
}

fn validate_replacement_targets_with_binaries(
    session: &Session,
    config: &Config,
) -> Result<Vec<PathBuf>, RestoreError> {
    if let Some(client) = session.clients.iter().find(|client| {
        !is_ignored_class(&client.class, &config.filters.ignore_classes)
            && !is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
            && brave_client_has_no_safe_profile_identity(client)
    }) {
        return Err(RestoreError::UnsafeReplacementTarget {
            reason: format!(
                "Brave window '{}' has no unique profile identity",
                client.title
            ),
        });
    }
    let targets = build_reconcile_targets(session, config);
    if !session.clients.is_empty() && targets.is_empty() {
        return Err(RestoreError::NoRestorableTargets);
    }
    if targets.len() > MAX_RECONCILIATION_WINDOWS {
        return Err(RestoreError::TooManyWindows {
            targets: targets.len(),
            current: 0,
            limit: MAX_RECONCILIATION_WINDOWS,
        });
    }
    let mut binaries = Vec::with_capacity(targets.len());
    for target in targets {
        let launch_command = build_launch_command(&target.client);
        if !launch_command_is_trusted(&target.client, config) {
            return Err(RestoreError::UntrustedLaunch {
                target: target.label,
                command: launch_command[0].clone(),
            });
        }
        let binary = resolve_launch_binary(&launch_command[0], &target.label).map_err(|_| {
            RestoreError::MissingLaunchBinary {
                target: target.label.clone(),
                command: launch_command[0].clone(),
            }
        })?;
        binaries.push(binary);
    }
    Ok(binaries)
}

/// Close the current desktop and reconcile the already-loaded session in the
/// same helper process.  Loading and validating the target before the first
/// close removes the UI's check/use race: deleting or editing the session
/// after this function starts cannot change what will be restored.
#[cfg(test)]
fn replace_session(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<ReconcileReport, RestoreError> {
    replace_session_inner(
        session,
        hyprctl,
        process_info,
        config,
        ReplaceOptions {
            dry_run,
            verbose,
            validate_targets: true,
            marker: None,
            safety_snapshot: None,
        },
    )
}

/// Marker information for a replacement transaction managed by the CLI.  The
/// caller writes a prepared marker before entering this function; the helper
/// records the first close candidate, then advances it to in-progress only
/// after that close dispatch has been accepted.
pub struct ReplaceMarkerContext<'a> {
    pub dry_run: bool,
    pub verbose: bool,
    pub backup_name: &'a str,
    pub target_name: &'a str,
    pub sessions_dir: &'a Path,
    /// The exact snapshot captured immediately before replacement.  The
    /// replacement pass uses it to leave windows that appeared after capture
    /// untouched instead of accidentally closing them.
    pub safety_snapshot: Option<&'a Session>,
}

/// Replacement entry point used by the CLI after it has persisted a safety
/// snapshot and an in-progress marker.  A clean pass advances the marker to a
/// finalizing phase before returning, so a crash between restore completion and
/// marker cleanup cannot replay the old desktop over the new one.
pub fn replace_session_with_marker(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    marker: ReplaceMarkerContext<'_>,
) -> Result<ReconcileReport, RestoreError> {
    if !marker.dry_run && marker.safety_snapshot.is_none() {
        return Err(RestoreError::UnsafeRecoverySnapshot {
            reason: "marker-backed replacement requires a persisted safety snapshot".to_string(),
        });
    }
    replace_session_inner(
        session,
        hyprctl,
        process_info,
        config,
        ReplaceOptions {
            dry_run: marker.dry_run,
            verbose: marker.verbose,
            validate_targets: true,
            marker: Some((marker.backup_name, marker.target_name, marker.sessions_dir)),
            safety_snapshot: marker.safety_snapshot,
        },
    )
}

/// Exact recovery used after an interrupted replacement.  It closes every
/// currently visible client first, including windows created by the failed
/// replacement, then restores the safety snapshot.  This avoids the mixed
/// desktop that an additive reconcile would leave behind.
pub fn recover_session(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<ReconcileReport, RestoreError> {
    replace_session_inner(
        session,
        hyprctl,
        process_info,
        config,
        ReplaceOptions {
            dry_run,
            verbose,
            validate_targets: false,
            marker: None,
            safety_snapshot: None,
        },
    )
}

/// Recover a safety snapshot without closing windows that appeared after the
/// replacement started.  Only strong identity matches are repaired; unknown
/// current windows remain extras, and missing snapshot targets are launched.
/// This is the safe startup path after a crash, when the user may already have
/// continued working on the desktop.
pub fn recover_session_safely(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<ReconcileReport, RestoreError> {
    let launcher = RealProcessLauncher;
    reconcile_session_with_launcher_options(
        session,
        hyprctl,
        process_info,
        config,
        dry_run,
        verbose,
        &launcher,
        false,
    )
}

struct ReplaceOptions<'a> {
    dry_run: bool,
    verbose: bool,
    validate_targets: bool,
    marker: Option<(&'a str, &'a str, &'a Path)>,
    safety_snapshot: Option<&'a Session>,
}

fn replace_session_inner(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    options: ReplaceOptions<'_>,
) -> Result<ReconcileReport, RestoreError> {
    let validated_launch_binaries = options
        .validate_targets
        .then(|| validate_replacement_targets_with_binaries(session, config))
        .transpose()?;
    if options.dry_run {
        return reconcile_session(
            session,
            hyprctl,
            process_info,
            config,
            true,
            options.verbose,
        );
    }

    let current = hyprctl.get_clients()?;
    // Only addresses observed before this helper started can be part of the
    // close plan.  A client opened after the initial list is an extra, not a
    // replacement target, and must not make the close wait time out.
    let mut pending_close_windows: HashMap<String, ObservedClient> = current
        .iter()
        .filter(|client| !client.address.is_empty())
        .map(|client| {
            let mut expected = ObservedClient::from_hypr_client(client.clone(), None, None);
            expected.process_start_time = process_info.get_start_time(client.pid).ok();
            (client.address.clone(), expected)
        })
        .collect();
    let initial_addresses: HashSet<String> = pending_close_windows.keys().cloned().collect();
    let mut protected_addresses = HashSet::new();
    let mut close_dispatched_addresses = HashSet::new();
    let safety_clients_by_address = options.safety_snapshot.map(|snapshot| {
        snapshot
            .clients
            .iter()
            .filter_map(|client| {
                client
                    .address
                    .as_deref()
                    .filter(|address| !address.is_empty())
                    .map(|address| (address.to_string(), client))
            })
            .collect::<HashMap<_, _>>()
    });
    let mut close_started = false;
    for client in current {
        if !client.address.is_empty() {
            if let Some(safety_clients) = &safety_clients_by_address {
                // This client was not in the snapshot captured before
                // replacement.  Remove it from the close/wait plan and leave
                // it as an extra for reconciliation to preserve.
                if !safety_clients.contains_key(&client.address) {
                    pending_close_windows.remove(&client.address);
                    protected_addresses.insert(client.address.clone());
                    continue;
                }
            }

            // The initial client list is only a plan.  Revalidate the
            // address immediately before closing it so a window that
            // disappeared and was replaced cannot receive a destructive
            // close dispatch intended for the old client.
            let mut expected = ObservedClient::from_hypr_client(client.clone(), None, None);
            expected.process_start_time = process_info.get_start_time(client.pid).ok();
            let revalidated_clients = hyprctl.get_clients()?;
            let Some(revalidated) = revalidated_clients
                .iter()
                .find(|current| current.address == client.address)
                .cloned()
            else {
                pending_close_windows.remove(&client.address);
                continue;
            };

            let safety_identity_matches = safety_clients_by_address
                .as_ref()
                .and_then(|safety_clients| safety_clients.get(&client.address))
                .is_none_or(|safety_client| {
                    safety_snapshot_identity_matches(safety_client, &revalidated, process_info)
                });
            let identity_is_safe =
                observed_window_identity_matches(&expected, &revalidated, process_info)
                    && !identity_is_ambiguous_without_stable_id(
                        &expected,
                        &revalidated,
                        &revalidated_clients,
                    );
            if !identity_is_safe || !safety_identity_matches {
                if safety_clients_by_address.is_some() {
                    // Marker-backed replacement has a durable safety copy.
                    // If identity evidence is incomplete or changed, keep
                    // this client rather than risking a close of a different
                    // window.  It is intentionally treated as an extra.
                    pending_close_windows.remove(&client.address);
                    protected_addresses.insert(client.address.clone());
                    continue;
                }
                return Err(RestoreError::WindowIdentityChanged {
                    address: client.address,
                });
            }
            pending_close_windows.insert(client.address.clone(), expected.clone());
            if let Some((backup_name, target_name, sessions_dir)) = options.marker {
                if !close_started {
                    // Keep the prepared marker until the first close is about
                    // to happen. If dispatch fails, startup can inspect this
                    // address before deciding whether recovery is necessary.
                    crate::session::mark_replace_closing_for_target_with_identity_and_stable_id(
                        backup_name,
                        Some(target_name),
                        sessions_dir,
                        &revalidated.address,
                        revalidated.pid,
                        process_info.get_start_time(revalidated.pid).ok(),
                        revalidated.stable_id.as_deref(),
                    )
                    .map_err(|error| RestoreError::Transaction(error.to_string()))?;
                }
            }
            hyprctl.dispatch(&format!("closewindow address:{}", client.address))?;
            close_dispatched_addresses.insert(client.address.clone());
            if !close_started {
                close_started = true;
                if let Some((backup_name, target_name, sessions_dir)) = options.marker {
                    crate::session::mark_replace_in_progress_for_target(
                        backup_name,
                        Some(target_name),
                        sessions_dir,
                    )
                    .map_err(|error| RestoreError::Transaction(error.to_string()))?;
                }
            }
        }
    }

    // An empty desktop has no close dispatch to mark the transition into the
    // destructive phase.  Record that the replacement has nevertheless
    // started before launching any target, so an interruption during launch
    // still recovers from the safety snapshot instead of treating the marker
    // as a harmless preflight.
    if !close_started {
        if let Some((backup_name, target_name, sessions_dir)) = options.marker {
            crate::session::mark_replace_in_progress_for_target(
                backup_name,
                Some(target_name),
                sessions_dir,
            )
            .map_err(|error| RestoreError::Transaction(error.to_string()))?;
        }
    }

    let timeout = Duration::from_millis(config.general.window_detect_timeout_ms.clamp(
        crate::config::MIN_WINDOW_DETECT_TIMEOUT_MS,
        crate::config::MAX_WINDOW_DETECT_TIMEOUT_MS,
    ));
    let started = Instant::now();
    loop {
        let clients = hyprctl.get_clients()?;
        let remaining = pending_close_windows.iter().any(|(address, expected)| {
            clients.iter().any(|current| {
                current.address == *address
                    && observed_window_identity_matches(expected, current, process_info)
            })
        });
        if !remaining {
            break;
        }
        if started.elapsed() >= timeout {
            return Err(RestoreError::ReplaceTimeout);
        }
        thread::sleep(Duration::from_millis(50));
    }

    // The close phase only acts on the initial client list.  Preserve every
    // address that was not in that list, every address excluded by the safety
    // snapshot, and every address that was already dispatched for close but
    // has since reappeared.  The final reconciliation must not claim any of
    // those windows as targets or move them as a side effect.
    for client in hyprctl.get_clients()? {
        if client.address.is_empty()
            || !initial_addresses.contains(&client.address)
            || close_dispatched_addresses.contains(&client.address)
            || safety_clients_by_address
                .as_ref()
                .is_some_and(|safety_clients| !safety_clients.contains_key(&client.address))
        {
            protected_addresses.insert(client.address);
        }
    }

    let report = reconcile_session_with_launcher_options_validated(
        session,
        hyprctl,
        process_info,
        config,
        false,
        options.verbose,
        &RealProcessLauncher,
        true,
        validated_launch_binaries.as_deref(),
        Some(&protected_addresses),
    )?;
    if let Some((backup_name, target_name, sessions_dir)) = options.marker {
        if report.failed == 0 && report.skipped == 0 {
            crate::session::mark_replace_finalizing_for_target(
                backup_name,
                Some(target_name),
                sessions_dir,
            )
            .map_err(|error| RestoreError::TransactionAfterRestore(error.to_string()))?;
        }
    }
    Ok(report)
}

#[derive(Debug, Clone)]
struct ReconcileTarget {
    client: SessionClient,
    label: String,
}

/// Preserve the one-to-one assignment made for later targets while an
/// earlier target is being refreshed.  If the earlier window disappears, a
/// broad fallback must not steal the later window and then launch a duplicate
/// for that later target.
fn reserved_planned_addresses(
    target_index: usize,
    plan: &[Option<ReconcilePair>],
    planned_observed: &[ObservedClient],
    current: &[ObservedClient],
    targets: &[SessionClient],
) -> HashSet<String> {
    let mut reserved = HashSet::new();
    for pair in plan.iter().skip(target_index + 1).flatten() {
        let Some(planned) = planned_observed.get(pair.current_index) else {
            continue;
        };
        let later_target = targets.get(pair.target_index);
        for candidate in current {
            let same_planned_window = observed_windows_are_same(planned, candidate);
            let reopened_planned_window = later_target
                .is_some_and(|target| relaunch_fallback_identity_matches(target, candidate));
            if same_planned_window || reopened_planned_window {
                reserved.insert(candidate.client.address.clone());
            }
        }
    }
    reserved
}

fn protect_windows_created_after(
    baseline: &[ObservedClient],
    current: &[ObservedClient],
    protected_addresses: &mut HashSet<String>,
) {
    for candidate in current {
        if !baseline
            .iter()
            .any(|previous| observed_windows_are_same(previous, candidate))
        {
            protected_addresses.insert(candidate.client.address.clone());
        }
    }
}

/// Reconcile a saved session with the windows that are already open.
///
/// Existing windows are matched one-to-one and repaired in place.  Missing
/// targets are launched, while unmatched windows are intentionally preserved
/// as extras.  Running this command repeatedly is therefore safe and
/// idempotent: once the targets are in place, the next pass emits no
/// compositor commands and launches nothing.
pub fn reconcile_session(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<ReconcileReport, RestoreError> {
    let launcher = RealProcessLauncher;
    reconcile_session_with_launcher(
        session,
        hyprctl,
        process_info,
        config,
        dry_run,
        verbose,
        &launcher,
    )
}

/// Decide whether an interrupted replacement already produced a complete set
/// of target windows.  Placement may have been changed by the user after the
/// last dispatch, so presence of every target is enough to avoid destructively
/// replaying the old safety snapshot; a later normal reconcile can repair any
/// remaining placement differences.
pub fn replacement_target_is_complete(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
) -> Result<bool, RestoreError> {
    replacement_target_is_complete_with_backup(session, None, hyprctl, process_info, config)
}

/// Variant used by startup recovery when the replacement marker also names
/// the safety snapshot.  A target-looking window cannot prove that a
/// replacement completed while any window from the pre-replacement desktop
/// is still present: the old window may share the target's class and title.
pub fn replacement_target_is_complete_with_backup(
    session: &Session,
    safety_snapshot: Option<&Session>,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
) -> Result<bool, RestoreError> {
    if let Some(safety_snapshot) = safety_snapshot {
        if safety_snapshot.clients.iter().any(|client| {
            client
                .address
                .as_deref()
                .is_none_or(|address| address.is_empty())
        }) {
            // Without an address for every pre-replacement window, absence of
            // the known addresses cannot prove that the old desktop is gone.
            // Do not preserve a target-looking window that may still be part
            // of the safety snapshot; the conservative recovery path below
            // is the only safe option for legacy/incomplete snapshots.
            return Ok(false);
        }
        let saved_addresses: HashSet<&str> = safety_snapshot
            .clients
            .iter()
            .map(|client| client.address.as_deref().unwrap_or_default())
            .filter(|address| !address.is_empty())
            .collect();
        if !saved_addresses.is_empty()
            && hyprctl
                .get_clients()?
                .iter()
                .any(|client| saved_addresses.contains(client.address.as_str()))
        {
            return Ok(false);
        }
    }
    if session.clients.iter().any(|client| {
        is_ignored_class(&client.class, &config.filters.ignore_classes)
            || is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
    }) {
        // A changed config cannot tell us which target set the interrupted
        // replace validated.  Keep the conservative recovery path instead.
        return Ok(false);
    }
    if session
        .clients
        .iter()
        .any(brave_client_has_no_safe_profile_identity)
    {
        // The replacement validator rejects this for new transactions.  Keep
        // recovery conservative for an older marker or a hand-edited target;
        // otherwise the profile-aware target builder could silently omit a
        // Brave window and falsely declare the replacement complete.
        return Ok(false);
    }
    let target_count = build_reconcile_targets(session, config).len();
    if target_count == 0 {
        // An intentionally empty replacement is complete once the desktop is
        // empty.  Treating it as incomplete would restore the old safety
        // snapshot after a crash even though the requested target was reached.
        return Ok(hyprctl.get_clients()?.is_empty());
    }
    if target_count > MAX_RECONCILIATION_WINDOWS {
        return Ok(false);
    }

    let current_monitors = hyprctl.get_monitors()?;
    let observed = observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
    if observed.len() > MAX_RECONCILIATION_WINDOWS {
        return Ok(false);
    }
    if let Some(safety_snapshot) = safety_snapshot {
        if observed.iter().any(|current| {
            safety_snapshot
                .clients
                .iter()
                .any(|saved| replacement_safety_identity_matches(saved, current))
        }) {
            // A pre-replacement window can be reopened with a fresh address.
            // If it is still indistinguishable from a saved safety window,
            // do not mistake it for a target and commit the transaction.
            return Ok(false);
        }
    }
    let target_clients: Vec<SessionClient> = build_reconcile_targets(session, config)
        .into_iter()
        .map(|target| {
            adapt_client_geometry(
                &target.client,
                &session.monitors,
                find_monitor_by_name(&current_monitors, &target.client.monitor),
            )
        })
        .collect();
    let plan = plan_reconciliation_with_policy(
        &target_clients,
        &observed,
        MatchPolicy::ReplacementCompletion,
        true,
    );

    // This is a read-only proof used to decide whether replaying the safety
    // snapshot would be destructive.  It deliberately accepts a relaunched
    // target with a new compositor address only when strong identity evidence
    // still exists; it never launches or moves anything itself.
    Ok(plan.iter().all(Option::is_some))
}

/// Testable reconciliation entry point with process creation injected.
pub fn reconcile_session_with_launcher(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
    launcher: &dyn ProcessLauncher,
) -> Result<ReconcileReport, RestoreError> {
    reconcile_session_with_launcher_options(
        session,
        hyprctl,
        process_info,
        config,
        dry_run,
        verbose,
        launcher,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_session_with_launcher_options(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
    launcher: &dyn ProcessLauncher,
    allow_ambiguous_identity: bool,
) -> Result<ReconcileReport, RestoreError> {
    reconcile_session_with_launcher_options_validated(
        session,
        hyprctl,
        process_info,
        config,
        dry_run,
        verbose,
        launcher,
        allow_ambiguous_identity,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_session_with_launcher_options_validated(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
    launcher: &dyn ProcessLauncher,
    allow_ambiguous_identity: bool,
    validated_launch_binaries: Option<&[PathBuf]>,
    protected_addresses: Option<&HashSet<String>>,
) -> Result<ReconcileReport, RestoreError> {
    let targets = build_reconcile_targets(session, config);
    let current_monitors = hyprctl.get_monitors()?;
    let observed = observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
    if targets.len() > MAX_RECONCILIATION_WINDOWS || observed.len() > MAX_RECONCILIATION_WINDOWS {
        return Err(RestoreError::TooManyWindows {
            targets: targets.len(),
            current: observed.len(),
            limit: MAX_RECONCILIATION_WINDOWS,
        });
    }
    let target_clients: Vec<SessionClient> = targets
        .iter()
        .map(|target| {
            adapt_client_geometry(
                &target.client,
                &session.monitors,
                find_monitor_by_name(&current_monitors, &target.client.monitor),
            )
        })
        .collect();
    let plan = plan_reconciliation(&target_clients, &observed);

    let mut report = ReconcileReport::default();
    let mut used_current_windows = ConsumedWindows::default();
    // Keep the newest compositor snapshot for safety decisions.  A window can
    // reopen between the initial plan and the per-target refresh; using only
    // the initial list could launch a duplicate during conservative recovery.
    let mut latest_observed = observed.clone();
    let mut failed_launch_baselines: Vec<Vec<ObservedClient>> = Vec::new();
    let mut protected_failed_launch_addresses = HashSet::new();
    for (target_index, target) in targets.iter().enumerate() {
        let target_client = &target_clients[target_index];
        if brave_client_has_no_safe_profile_identity(target_client) {
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: {} has no safe Brave profile identity",
                target.label
            ));
            continue;
        }
        let mut excluded_addresses = protected_addresses.cloned().unwrap_or_default();
        excluded_addresses.extend(protected_failed_launch_addresses.iter().cloned());
        let matched = if dry_run {
            excluded_addresses.extend(reserved_planned_addresses(
                target_index,
                &plan,
                &observed,
                &observed,
                &target_clients,
            ));
            plan[target_index]
                .and_then(|pair| {
                    observed
                        .get(pair.current_index)
                        .cloned()
                        .filter(|current| !excluded_addresses.contains(&current.client.address))
                        .and_then(|current| {
                            allowed_match(target_client, &current, allow_ambiguous_identity)
                                .map(|(_, kind)| (current, kind))
                        })
                })
                .or_else(|| {
                    find_existing_restore_match_with_policy_excluding(
                        target_client,
                        &observed,
                        &used_current_windows,
                        config,
                        allow_ambiguous_identity,
                        Some(&excluded_addresses),
                    )
                    .and_then(|index| {
                        observed.get(index).cloned().and_then(|current| {
                            allowed_match(target_client, &current, allow_ambiguous_identity)
                                .map(|(_, kind)| (current, kind))
                        })
                    })
                })
        } else {
            // The initial assignment is only a plan.  Re-read the compositor
            // immediately before each target so a window that appeared after
            // planning is matched instead of launching a duplicate.
            let refreshed =
                observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
            if refreshed.len() > MAX_RECONCILIATION_WINDOWS {
                return Err(RestoreError::TooManyWindows {
                    targets: targets.len(),
                    current: refreshed.len(),
                    limit: MAX_RECONCILIATION_WINDOWS,
                });
            }
            latest_observed = refreshed.clone();
            for baseline in &failed_launch_baselines {
                protect_windows_created_after(
                    baseline,
                    &latest_observed,
                    &mut protected_failed_launch_addresses,
                );
            }
            excluded_addresses.extend(protected_failed_launch_addresses.iter().cloned());
            excluded_addresses.extend(reserved_planned_addresses(
                target_index,
                &plan,
                &observed,
                &refreshed,
                &target_clients,
            ));
            let planned_window =
                plan[target_index].and_then(|pair| observed.get(pair.current_index));
            let selected_index = planned_window
                .and_then(|planned| {
                    refreshed.iter().enumerate().find_map(|(index, current)| {
                        (observed_windows_are_same(planned, current)
                            && !used_current_windows.contains(current)
                            && !excluded_addresses.contains(&current.client.address)
                            && allowed_match(target_client, current, allow_ambiguous_identity)
                                .is_some())
                        .then_some(index)
                    })
                })
                .or_else(|| {
                    find_existing_restore_match_with_policy_excluding(
                        target_client,
                        &refreshed,
                        &used_current_windows,
                        config,
                        allow_ambiguous_identity,
                        Some(&excluded_addresses),
                    )
                });
            selected_index.map(|index| {
                let current = refreshed[index].clone();
                let kind = allowed_match(target_client, &current, allow_ambiguous_identity)
                    .map(|(_, kind)| kind)
                    .unwrap_or(MatchKind::ClassFallback);
                (current, kind)
            })
        };

        if let Some((current, match_kind)) = matched {
            used_current_windows.insert(&current);
            report.matched += 1;
            let target_monitor_available =
                target_monitor_is_available(&current_monitors, &target_client.monitor);
            let commands = build_reconcile_dispatch_commands_with_geometry(
                target_client,
                &current.client,
                current.monitor_name.as_deref(),
                target_client.at,
                target_client.size,
                target_monitor_available,
            );

            if commands.is_empty() {
                report.unchanged += 1;
                if dry_run || verbose {
                    report.details.push(format!(
                        "{}: {} already in place (matched {})",
                        if dry_run { "[dry-run]" } else { "OK" },
                        target.label,
                        match_kind_label(match_kind)
                    ));
                }
                continue;
            }

            if dry_run {
                report.moved += 1;
                report.details.push(format!(
                    "[dry-run] repair {} at address {} (matched {})",
                    target.label,
                    current.client.address,
                    match_kind_label(match_kind)
                ));
                for command in &commands {
                    report.details.push(format!("  hyprctl dispatch {command}"));
                }
                continue;
            }

            match dispatch_existing_repairs(&current, &commands, hyprctl, process_info) {
                Ok(()) => {
                    report.moved += 1;
                    if verbose {
                        report.details.push(format!(
                            "OK: repaired {} at address {}",
                            target.label, current.client.address
                        ));
                    }
                }
                Err(error) => {
                    report.failed += 1;
                    report.details.push(format!(
                        "FAIL: {} at address {} — {error}",
                        target.label, current.client.address
                    ));
                }
            }
            continue;
        }

        let eligible_latest = latest_observed
            .iter()
            .filter(|window| !excluded_addresses.contains(&window.client.address))
            .cloned()
            .collect::<Vec<_>>();
        if has_unavailable_identity_candidate(
            target_client,
            &eligible_latest,
            &used_current_windows,
            config,
        ) {
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: {} could not be safely matched because window identity is unavailable",
                target.label
            ));
            continue;
        }

        if has_ambiguous_generic_chromium_candidate(
            target_client,
            &eligible_latest,
            &used_current_windows,
            config,
        ) {
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: {} could not be safely matched because generic Chromium window identity is unavailable",
                target.label
            ));
            continue;
        }

        if !allow_ambiguous_identity
            && has_unmatched_same_class_candidate(
                target_client,
                &eligible_latest,
                &used_current_windows,
                config,
            )
        {
            // A conservative safety recovery must not launch a second copy
            // merely because a formerly captured window was reopened with a
            // new address and no usable CWD/profile evidence.  Leave the
            // existing same-class window untouched for the user to resolve.
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: {} could not be safely matched during safety recovery",
                target.label
            ));
            continue;
        }

        let launch_command = build_launch_command(target_client);
        if !launch_command_is_trusted(target_client, config) {
            report.failed += 1;
            report.details.push(format!(
                "FAIL: launch command '{}' for {} is not authorized by app identity or config",
                launch_command[0], target.label
            ));
            continue;
        }
        if dry_run {
            if resolve_launch_binary(&launch_command[0], &target.label).is_err() {
                report.failed += 1;
                report.details.push(format!(
                    "[dry-run] FAIL: binary '{}' not found for {}",
                    launch_command[0], target.label
                ));
                continue;
            }
            report.launched += 1;
            report.details.push(format!(
                "[dry-run] missing {} → {}",
                target.label,
                launch_command.join(" ")
            ));
            let target_monitor_available =
                target_monitor_is_available(&current_monitors, &target_client.monitor);
            for command in
                build_dispatch_commands_for_monitor(target_client, target_monitor_available)
            {
                report.details.push(format!("  hyprctl dispatch {command}"));
            }
            continue;
        }

        let launch_binary = match validated_launch_binaries {
            Some(binaries) => binaries.get(target_index).cloned().ok_or_else(|| {
                RestoreError::Transaction(
                    "validated replacement launch plan no longer matches its targets".to_string(),
                )
            })?,
            None => match resolve_launch_binary(&launch_command[0], &target.label) {
                Ok(binary) => binary,
                Err(_) => {
                    report.failed += 1;
                    report.details.push(format!(
                        "FAIL: binary '{}' not found for {}",
                        launch_command[0], target.label
                    ));
                    continue;
                }
            },
        };

        if has_ambiguous_profile_candidate(
            target_client,
            &eligible_latest,
            &used_current_windows,
            config,
        ) {
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: {} could not be safely matched because Brave profile identity is ambiguous",
                target.label
            ));
            continue;
        }

        let launch_baseline = latest_observed.clone();
        match restore_single_client_with_launcher_and_process_info_with_address_and_binary(
            target_client,
            hyprctl,
            process_info,
            config,
            launcher,
            target_monitor_is_available(&current_monitors, &target_client.monitor),
            Some(&launch_binary),
        ) {
            Ok(restored) => {
                used_current_windows.insert(&restored.observed);
                report.launched += 1;
                if verbose {
                    report
                        .details
                        .push(format!("OK: launched {}", target.label));
                }
            }
            Err(error) => {
                failed_launch_baselines.push(launch_baseline);
                if let Ok(refreshed_after_failure) =
                    observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)
                {
                    latest_observed = refreshed_after_failure;
                    for baseline in &failed_launch_baselines {
                        protect_windows_created_after(
                            baseline,
                            &latest_observed,
                            &mut protected_failed_launch_addresses,
                        );
                    }
                }
                report.failed += 1;
                report
                    .details
                    .push(format!("FAIL: {} — {error}", target.label));
            }
        }
    }

    if !dry_run {
        latest_observed =
            observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
        for baseline in &failed_launch_baselines {
            protect_windows_created_after(
                baseline,
                &latest_observed,
                &mut protected_failed_launch_addresses,
            );
        }
    }
    report.extras = latest_observed
        .iter()
        .filter(|window| !used_current_windows.contains(window))
        .count();
    if verbose {
        for window in &latest_observed {
            if !used_current_windows.contains(window) {
                report.details.push(format!(
                    "EXTRA: {} '{}' at address {} on ws={} left untouched",
                    window.client.class,
                    window.client.title,
                    window.client.address,
                    window.client.workspace.id
                ));
            }
        }
    }

    Ok(report)
}

fn brave_profile_targets(session: &Session, config: &Config) -> Option<Vec<BraveProfile>> {
    let profile_workspaces =
        app_config_for(config, "brave-browser", "").and_then(|app| app.profile_workspaces.as_ref());

    if let Some(workspaces) = profile_workspaces {
        let has_saved_brave_clients = session.clients.iter().any(is_brave_client);
        let has_known_saved_profile = session.clients.iter().any(|client| {
            is_brave_client(client)
                && client
                    .profile_directory
                    .as_deref()
                    .is_some_and(|directory| !directory.is_empty())
        });
        let inventory = if session.brave_profiles.is_empty() {
            let mut captured = Vec::new();
            for client in session
                .clients
                .iter()
                .filter(|client| is_brave_client(client))
            {
                let Some(directory) = client
                    .profile_directory
                    .as_deref()
                    .filter(|directory| !directory.is_empty())
                else {
                    continue;
                };
                if captured
                    .iter()
                    .any(|profile: &BraveProfile| profile.directory.eq_ignore_ascii_case(directory))
                {
                    continue;
                }
                captured.push(BraveProfile {
                    directory: directory.to_string(),
                    name: client.title.clone(),
                });
            }
            captured
        } else {
            session.brave_profiles.clone()
        };
        if !workspaces.is_empty()
            && has_saved_brave_clients
            && !has_known_saved_profile
            && inventory.is_empty()
        {
            // A legacy snapshot may contain Brave windows but no profile
            // inventory or per-window profile identity.  Do not turn that
            // uncertainty into a successful zero-target restore under a
            // non-empty profile map; retain the raw window targets so the
            // caller reports them as safely skipped instead.
            return None;
        }
        return Some(
            inventory
                .into_iter()
                .filter(|profile| workspaces.contains_key(&profile.directory))
                .collect(),
        );
    }

    if session.brave_profiles.is_empty() {
        return None;
    }
    let brave_client_count = session
        .clients
        .iter()
        .filter(|client| is_brave_client(client))
        .count();
    let active_directories: Vec<&str> = session
        .clients
        .iter()
        .filter(|client| is_brave_client(client))
        .filter_map(|client| client.profile_directory.as_deref())
        .filter(|directory| !directory.is_empty())
        .collect();
    let has_ambiguous_captured_identity = session
        .clients
        .iter()
        .filter(|client| is_brave_client(client))
        .any(|client| client.profile_identity_ambiguous);
    if active_directories.is_empty() {
        // A profile-only snapshot has no window identity to contradict it, so
        // preserve its explicit profile targets.  With captured Brave windows,
        // however, a profile list without per-window identity is ambiguous
        // unless there is exactly one of each.  Do not assign profiles by
        // count or launch a complete Local State inventory by accident.
        return (brave_client_count == 0
            || (!has_ambiguous_captured_identity
                && brave_client_count == 1
                && session.brave_profiles.len() == 1))
            .then(|| session.brave_profiles.clone())
            .or_else(|| Some(Vec::new()));
    }

    let active_profiles: Vec<BraveProfile> = session
        .brave_profiles
        .iter()
        .filter(|profile| {
            active_directories
                .iter()
                .any(|directory| directory.eq_ignore_ascii_case(&profile.directory))
        })
        .cloned()
        .collect();
    // An inventory with a profile identity that does not occur in the saved
    // windows is also not safe to reinterpret as raw Brave targets.  Returning
    // an empty profile mode makes restore/reconcile skip those windows rather
    // than moving an arbitrary shared-process window.
    Some(active_profiles)
}

fn build_reconcile_targets(session: &Session, config: &Config) -> Vec<ReconcileTarget> {
    let brave_profiles = brave_profile_targets(session, config);
    let has_brave_profiles = brave_profiles.is_some();

    let mut targets: Vec<ReconcileTarget> = session
        .clients
        .iter()
        .filter(|client| {
            !is_ignored_class(&client.class, &config.filters.ignore_classes)
                && !is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
        })
        .filter(|client| !(has_brave_profiles && is_brave_client(client)))
        .map(|client| {
            let client = hydrate_webapp_launch(client, config);
            ReconcileTarget {
                label: format!("{} '{}'", client.class, client.title),
                client,
            }
        })
        .collect();

    if has_brave_profiles {
        let brave_config = app_config_for(config, "brave-browser", "");
        let binary = brave_config
            .and_then(|app| app.binary.clone())
            .unwrap_or_else(|| "brave".to_string());
        let default_workspace = brave_config
            .and_then(|app| app.default_workspace)
            .unwrap_or(1);
        let profile_workspaces = brave_config.and_then(|app| app.profile_workspaces.as_ref());

        let mut brave_clients: Vec<SessionClient> = session
            .clients
            .iter()
            .filter(|client| {
                is_brave_client(client)
                    && !is_ignored_class(&client.class, &config.filters.ignore_classes)
                    && !is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
            })
            .cloned()
            .collect();
        brave_clients.sort_by(|left, right| {
            left.workspace
                .cmp(&right.workspace)
                .then(left.at[1].cmp(&right.at[1]))
                .then(left.at[0].cmp(&right.at[0]))
        });

        let mut used_brave_clients = HashSet::new();
        for profile in brave_profiles.expect("Brave profile mode was checked above") {
            if profile_workspaces
                .is_some_and(|workspaces| !workspaces.contains_key(&profile.directory))
            {
                // An explicit profile map is an allowlist.  Do not resurrect
                // profiles that capture or a legacy session may contain but
                // the user intentionally left unmapped.
                continue;
            }
            let matching_index = brave_clients
                .iter()
                .enumerate()
                .find(|(index, client)| {
                    !used_brave_clients.contains(index)
                        && client
                            .profile_directory
                            .as_deref()
                            .map(|directory| directory.eq_ignore_ascii_case(&profile.directory))
                            .unwrap_or(false)
                })
                .map(|(index, _)| index);
            let fallback_index = brave_clients
                .iter()
                .enumerate()
                .find(|(index, _)| !used_brave_clients.contains(index))
                .map(|(index, _)| index);
            let mut client = matching_index
                .or(fallback_index)
                .and_then(|index| {
                    used_brave_clients.insert(index);
                    brave_clients.get(index).cloned()
                })
                .or_else(|| brave_clients.first().cloned())
                .unwrap_or_else(|| SessionClient {
                    class: "brave-browser".to_string(),
                    title: profile.name.clone(),
                    address: None,
                    pid: None,
                    process_start_time: None,
                    stable_id: None,
                    initial_class: "brave-browser".to_string(),
                    initial_title: "Brave".to_string(),
                    workspace: default_workspace,
                    workspace_name: default_workspace.to_string(),
                    monitor: String::new(),
                    at: [0, 0],
                    size: [1280, 800],
                    floating: false,
                    fullscreen: 0,
                    pinned: false,
                    profile_directory: None,
                    profile_identity_ambiguous: false,
                    focus_history_id: 0,
                    launch: crate::session::LaunchInfo {
                        command: binary.clone(),
                        args: vec![],
                        hint: None,
                    },
                });

            client.workspace = profile_workspaces
                .and_then(|workspaces| workspaces.get(&profile.directory))
                .copied()
                .unwrap_or(default_workspace);
            client.workspace_name = client.workspace.to_string();
            client.profile_directory = Some(profile.directory.clone());
            client.profile_identity_ambiguous = false;
            client.launch.command = binary.clone();
            client.launch.args = vec![format!("--profile-directory={}", profile.directory)];
            client.launch.hint = None;

            targets.push(ReconcileTarget {
                label: format!("brave profile '{}'", profile.name),
                client,
            });
        }
    }

    targets.sort_by(|left, right| {
        left.client
            .workspace
            .cmp(&right.client.workspace)
            .then(left.client.at[1].cmp(&right.client.at[1]))
            .then(left.client.at[0].cmp(&right.client.at[0]))
            .then(left.label.cmp(&right.label))
    });
    targets
}

fn hydrate_webapp_launch(client: &SessionClient, config: &Config) -> SessionClient {
    let mut hydrated = client.clone();
    if !is_webapp_class(&hydrated)
        || app_config_for(config, &hydrated.class, &hydrated.initial_class)
            .and_then(|app| app.binary.as_ref())
            .is_some()
    {
        return hydrated;
    }

    let command_is_legacy = hydrated.launch.command.is_empty()
        || [hydrated.class.as_str(), hydrated.initial_class.as_str()]
            .iter()
            .any(|identity| {
                !identity.is_empty() && identity.eq_ignore_ascii_case(&hydrated.launch.command)
            });
    if command_is_legacy {
        if let Some(launch) =
            crate::capture::discover_webapp_launch_info(&hydrated.class, &hydrated.initial_class)
        {
            hydrated.launch = launch;
        }
    }
    hydrated
}

fn observe_clients_with_monitors(
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    monitors: &[HyprMonitor],
) -> Result<Vec<ObservedClient>, RestoreError> {
    let monitor_names: HashMap<i32, String> = monitors
        .iter()
        .cloned()
        .map(|monitor| (monitor.id, monitor.name))
        .collect();

    let mut clients: Vec<ObservedClient> = hyprctl
        .get_clients()?
        .into_iter()
        .filter(|client| {
            !is_ignored_class(&client.class, &config.filters.ignore_classes)
                && !is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
        })
        .map(|client| {
            let monitor_name = monitor_names.get(&client.monitor).cloned();
            let cwd = observe_cwd(&client, process_info);
            let process_start_time = process_info.get_start_time(client.pid).ok();
            let process_command = process_info.get_cmdline(client.pid).ok();
            let mut observed =
                ObservedClient::with_profile_directory(client, monitor_name, cwd, None);
            observed.process_start_time = process_start_time;
            observed.process_command = process_command;
            observed
        })
        .collect();
    let mut brave_window_counts = HashMap::<u32, usize>::new();
    for observed in &clients {
        if is_brave_hypr_client(&observed.client) {
            *brave_window_counts.entry(observed.client.pid).or_default() += 1;
        }
    }
    for observed in &mut clients {
        if is_brave_hypr_client(&observed.client) {
            let window_count = brave_window_counts
                .get(&observed.client.pid)
                .copied()
                .unwrap_or_default();
            let discovery = find_profile_discovery(process_info, observed.client.pid);
            match discovery.profiles.as_slice() {
                [profile] if window_count == 1 => {
                    observed.profile_directory = Some(profile.clone());
                    observed.profile_identity_ambiguous = !discovery.complete;
                }
                [profile] => {
                    observed.profile_directory = Some(profile.clone());
                    observed.profile_identity_ambiguous = true;
                }
                _ => {
                    observed.profile_identity_ambiguous = true;
                }
            }
        }
    }
    clients.sort_by(|left, right| left.client.address.cmp(&right.client.address));
    Ok(clients)
}

fn observe_cwd(client: &HyprClient, process_info: &dyn ProcessInfoProvider) -> Option<PathBuf> {
    select_terminal_process(process_info, client.pid)
        .map(|child| child.cwd)
        .or_else(|| process_info.get_cwd(client.pid).ok())
}

fn find_monitor_by_name<'a>(monitors: &'a [HyprMonitor], name: &str) -> Option<&'a HyprMonitor> {
    if name.is_empty() {
        return None;
    }
    monitors
        .iter()
        .find(|monitor| monitor.name.eq_ignore_ascii_case(name))
}

fn target_monitor_is_available(monitors: &[HyprMonitor], name: &str) -> bool {
    name.is_empty() || find_monitor_by_name(monitors, name).is_some()
}

/// Adapt captured absolute coordinates to a monitor whose origin or
/// resolution changed since the snapshot.  Older sessions have `None` for
/// monitor origins and deliberately keep their original geometry.
fn adapt_client_geometry(
    target: &SessionClient,
    saved_monitors: &[Monitor],
    current_monitor: Option<&HyprMonitor>,
) -> SessionClient {
    let Some(current_monitor) = current_monitor else {
        return target.clone();
    };
    let Some(saved_monitor) = saved_monitors
        .iter()
        .find(|monitor| monitor.name.eq_ignore_ascii_case(&target.monitor))
    else {
        return target.clone();
    };
    let (Some(saved_x), Some(saved_y)) = (saved_monitor.x, saved_monitor.y) else {
        return target.clone();
    };
    let (Some(current_x), Some(current_y)) = (current_monitor.x, current_monitor.y) else {
        return target.clone();
    };
    if saved_monitor.width == 0
        || saved_monitor.height == 0
        || current_monitor.width == 0
        || current_monitor.height == 0
    {
        return target.clone();
    }

    let (relative_at, relative_size) = if supported_rotation(saved_monitor.transform)
        && supported_rotation(current_monitor.transform)
    {
        let saved_relative_at = [
            i64::from(target.at[0]) - i64::from(saved_x),
            i64::from(target.at[1]) - i64::from(saved_y),
        ];
        let (canonical_at, canonical_size) = rotate_rect_to_canonical(
            saved_relative_at,
            target.size,
            saved_monitor.width,
            saved_monitor.height,
            saved_monitor.transform,
        );
        let scale_x = current_monitor.width as f64 / saved_monitor.width as f64;
        let scale_y = current_monitor.height as f64 / saved_monitor.height as f64;
        let scaled_at = [
            scale_coordinate(canonical_at[0], scale_x),
            scale_coordinate(canonical_at[1], scale_y),
        ];
        let scaled_size = [
            scaled_extent(canonical_size[0], scale_x, current_monitor.width),
            scaled_extent(canonical_size[1], scale_y, current_monitor.height),
        ];
        rotate_rect_from_canonical(
            scaled_at,
            scaled_size,
            current_monitor.width,
            current_monitor.height,
            current_monitor.transform,
        )
    } else {
        let scale_x = current_monitor.width as f64 / saved_monitor.width as f64;
        let scale_y = current_monitor.height as f64 / saved_monitor.height as f64;
        (
            [
                scale_coordinate(i64::from(target.at[0]) - i64::from(saved_x), scale_x),
                scale_coordinate(i64::from(target.at[1]) - i64::from(saved_y), scale_y),
            ],
            [
                scaled_extent(target.size[0], scale_x, current_monitor.width),
                scaled_extent(target.size[1], scale_y, current_monitor.height),
            ],
        )
    };
    let (relative_x, relative_y) = (relative_at[0], relative_at[1]);
    let width = relative_size[0];
    let height = relative_size[1];
    let (current_width, current_height) = displayed_monitor_dimensions(current_monitor);
    let proposed_x = i64::from(current_x) + relative_x;
    let proposed_y = i64::from(current_y) + relative_y;
    let at = [
        clamp_coordinate(proposed_x, current_x, current_width, width),
        clamp_coordinate(proposed_y, current_y, current_height, height),
    ];

    let mut adapted = target.clone();
    adapted.at = at;
    adapted.size = [width, height];
    adapted
}

fn supported_rotation(transform: u32) -> bool {
    transform < 4
}

fn displayed_monitor_dimensions(monitor: &HyprMonitor) -> (u32, u32) {
    if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    }
}

fn rotate_rect_to_canonical(
    at: [i64; 2],
    size: [i32; 2],
    base_width: u32,
    base_height: u32,
    transform: u32,
) -> ([i64; 2], [i32; 2]) {
    let x = at[0];
    let y = at[1];
    let width = i64::from(size[0].max(1));
    let height = i64::from(size[1].max(1));
    let base_width = i64::from(base_width);
    let base_height = i64::from(base_height);
    match transform {
        1 => ([y, base_width - x - width], [height as i32, width as i32]),
        2 => (
            [base_width - x - width, base_height - y - height],
            [width as i32, height as i32],
        ),
        3 => ([base_height - y - height, x], [height as i32, width as i32]),
        _ => ([x, y], [width as i32, height as i32]),
    }
}

fn rotate_rect_from_canonical(
    at: [i64; 2],
    size: [i32; 2],
    base_width: u32,
    base_height: u32,
    transform: u32,
) -> ([i64; 2], [i32; 2]) {
    let x = at[0];
    let y = at[1];
    let width = i64::from(size[0].max(1));
    let height = i64::from(size[1].max(1));
    let base_width = i64::from(base_width);
    let base_height = i64::from(base_height);
    match transform {
        1 => ([base_height - y - height, x], [height as i32, width as i32]),
        2 => (
            [base_width - x - width, base_height - y - height],
            [width as i32, height as i32],
        ),
        3 => ([y, base_width - x - width], [height as i32, width as i32]),
        _ => ([x, y], [width as i32, height as i32]),
    }
}

fn scale_coordinate(value: i64, scale: f64) -> i64 {
    (value as f64 * scale).round() as i64
}

fn scaled_extent(value: i32, scale: f64, monitor_extent: u32) -> i32 {
    let scaled = (i64::from(value.max(1)) as f64 * scale).round() as i64;
    scaled.clamp(1, i64::from(monitor_extent.min(i32::MAX as u32))) as i32
}

fn clamp_coordinate(coordinate: i64, origin: i32, monitor_extent: u32, window_extent: i32) -> i32 {
    let origin = i64::from(origin);
    let monitor_end = origin + i64::from(monitor_extent.min(i32::MAX as u32));
    let minimum = origin;
    let maximum = (monitor_end - i64::from(window_extent)).max(minimum);
    coordinate
        .clamp(minimum, maximum)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn match_kind_label(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::ExactIdentity => "exact identity",
        MatchKind::AppIdentity => "app identity",
        MatchKind::ProfileIdentity => "profile identity",
        MatchKind::ClassFallback => "class fallback",
    }
}

fn workspace_matches(target: &SessionClient, current: &HyprClient) -> bool {
    let saved_name = target.workspace_name.trim();
    if saved_name.is_empty() || saved_name.parse::<i32>().is_ok() {
        return target.workspace == current.workspace.id;
    }
    workspace_names_match(saved_name, current.workspace.name.trim())
}

fn workspace_names_match(saved_name: &str, current_name: &str) -> bool {
    if has_workspace_prefix(saved_name, "special:")
        || has_workspace_prefix(current_name, "special:")
    {
        return saved_name.eq_ignore_ascii_case(current_name);
    }
    strip_workspace_prefix(saved_name, "name:")
        .eq_ignore_ascii_case(strip_workspace_prefix(current_name, "name:"))
}

fn workspace_selector(target: &SessionClient) -> String {
    let saved_name = target.workspace_name.trim();
    if saved_name.is_empty() || saved_name.parse::<i32>().is_ok() {
        target.workspace.to_string()
    } else if has_workspace_prefix(saved_name, "special:")
        || has_workspace_prefix(saved_name, "name:")
    {
        saved_name.to_string()
    } else {
        format!("name:{saved_name}")
    }
}

fn has_workspace_prefix(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .map(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .unwrap_or(false)
}

fn strip_workspace_prefix<'a>(value: &'a str, prefix: &str) -> &'a str {
    if has_workspace_prefix(value, prefix) {
        &value[prefix.len()..]
    } else {
        value
    }
}

fn quote_dispatch_token(value: &str) -> String {
    if value.chars().all(|character| {
        !character.is_whitespace() && character != '\'' && character != '"' && character != '\\'
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "'\\''"))
}

fn monitor_move_commands(monitor: &str, address: &str) -> Vec<String> {
    // Hyprland's monitor-aware move dispatcher operates on the active window,
    // while address selectors are supported by focuswindow.  Focus the exact
    // matched client first, then move it silently to the named monitor.
    vec![
        format!("focuswindow address:{address}"),
        format!(
            "movewindow {} silent",
            quote_dispatch_token(&format!("mon:{monitor}"))
        ),
    ]
}

/// Return only the compositor operations needed to make an existing window
/// agree with the saved placement.  An empty result is the important fast
/// path: it means the window is already correct and should be left alone.
pub fn build_reconcile_dispatch_commands(
    target: &SessionClient,
    current: &HyprClient,
    current_monitor: Option<&str>,
) -> Vec<String> {
    build_reconcile_dispatch_commands_with_geometry(
        target,
        current,
        current_monitor,
        target.at,
        target.size,
        true,
    )
}

fn build_reconcile_dispatch_commands_with_geometry(
    target: &SessionClient,
    current: &HyprClient,
    current_monitor: Option<&str>,
    desired_at: [i32; 2],
    desired_size: [i32; 2],
    target_monitor_available: bool,
) -> Vec<String> {
    let monitor_mismatch = target_monitor_available
        && !target.monitor.is_empty()
        && current_monitor
            .map(|monitor| !monitor.eq_ignore_ascii_case(&target.monitor))
            // A successful monitor query can still lack a name for a stale
            // monitor ID.  Leave that case alone; query failures themselves
            // are propagated by the callers rather than being hidden here.
            .unwrap_or(false);
    let workspace_mismatch = !workspace_matches(target, current);
    let leaving_fullscreen = current.fullscreen > 0 && target.fullscreen == 0;
    let entering_or_changing_fullscreen =
        target.fullscreen > 0 && current.fullscreen != target.fullscreen;

    let mut commands = Vec::new();
    if current.pinned && !target.pinned {
        commands.push(format!("pin address:{}", current.address));
    }
    if leaving_fullscreen {
        commands.push(format!("focuswindow address:{}", current.address));
        commands.push("fullscreenstate 0 0".to_string());
    }
    if workspace_mismatch {
        commands.push(format!(
            "movetoworkspacesilent {},address:{}",
            quote_dispatch_token(&workspace_selector(target)),
            current.address
        ));
    }
    // Workspace bindings can move a window to their preferred monitor.  Apply
    // the explicit monitor correction afterwards so the final placement wins.
    if monitor_mismatch {
        commands.extend(monitor_move_commands(&target.monitor, &current.address));
    }
    if current.floating != target.floating {
        commands.push(format!("togglefloating address:{}", current.address));
    }

    // Do not apply stale absolute coordinates when the saved monitor is gone.
    if target.fullscreen == 0 && (target.monitor.is_empty() || target_monitor_available) {
        if current.size != desired_size {
            commands.push(format!(
                "resizewindowpixel exact {} {},address:{}",
                desired_size[0], desired_size[1], current.address
            ));
        }
        if current.at != desired_at {
            commands.push(format!(
                "movewindowpixel exact {} {},address:{}",
                desired_at[0], desired_at[1], current.address
            ));
        }
    }

    if entering_or_changing_fullscreen {
        commands.push(format!("focuswindow address:{}", current.address));
        commands.push(format!(
            "fullscreenstate {} {}",
            target.fullscreen, target.fullscreen
        ));
    }

    if !current.pinned && target.pinned {
        commands.push(format!("pin address:{}", current.address));
    }

    commands
}

// ── Per-client restore logic ────────────────────────────────────────────────

#[cfg(test)]
fn restore_single_client_with_launcher(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
    config: &Config,
    launcher: &dyn ProcessLauncher,
) -> Result<String, RestoreError> {
    let process_info = RealProcessInfo;
    restore_single_client_with_launcher_and_process_info(
        client,
        hyprctl,
        &process_info,
        config,
        launcher,
        false,
    )
}

#[cfg(test)]
fn restore_single_client_with_launcher_and_process_info(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    launcher: &dyn ProcessLauncher,
    target_monitor_available: bool,
) -> Result<String, RestoreError> {
    restore_single_client_with_launcher_and_process_info_with_address(
        client,
        hyprctl,
        process_info,
        config,
        launcher,
        target_monitor_available,
    )
    .map(|restored| restored.message)
}

#[derive(Debug)]
struct RestoredWindow {
    observed: ObservedClient,
    message: String,
}

fn restore_single_client_with_launcher_and_process_info_with_address(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    launcher: &dyn ProcessLauncher,
    target_monitor_available: bool,
) -> Result<RestoredWindow, RestoreError> {
    restore_single_client_with_launcher_and_process_info_with_address_and_binary(
        client,
        hyprctl,
        process_info,
        config,
        launcher,
        target_monitor_available,
        None,
    )
}

fn restore_single_client_with_launcher_and_process_info_with_address_and_binary(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    launcher: &dyn ProcessLauncher,
    target_monitor_available: bool,
    validated_binary: Option<&Path>,
) -> Result<RestoredWindow, RestoreError> {
    // 1. Snapshot existing window addresses before launching.
    let before: HashSet<String> = hyprctl
        .get_clients()?
        .into_iter()
        .map(|client| client.address)
        .collect();
    let active_before = if browser_handoff_target_is_safe(client) {
        hyprctl.get_active_window_address().ok().flatten()
    } else {
        None
    };

    // 2. Build and spawn the launch command.
    let launch_cmd = build_launch_command(client);
    let launch_binary = match validated_binary {
        Some(binary) => binary.to_path_buf(),
        None => resolve_launch_binary(&launch_cmd[0], &client.class)?,
    };
    let launched = launcher
        .spawn(&launch_binary.to_string_lossy(), &launch_cmd[1..])
        .map_err(|e| {
            HyprctlError::CommandFailed(format!("spawn '{}' failed: {e}", launch_cmd[0]))
        })?;

    // 3. Poll for the new window.  A compositor address is usually fresh, but
    // it can be recycled when an old window closes while the new process is
    // starting.  Permit that one edge case only when process correlation
    // proves the reused address belongs to this launch.
    let timeout = Duration::from_millis(config.general.window_detect_timeout_ms.clamp(
        crate::config::MIN_WINDOW_DETECT_TIMEOUT_MS,
        crate::config::MAX_WINDOW_DETECT_TIMEOUT_MS,
    ));
    let poll_interval = Duration::from_millis(100);
    let candidate_settle = Duration::from_millis(250).min(timeout);
    let start = Instant::now();
    let mut first_candidate_at = None;
    let mut first_handoff_candidate_at = None;
    let mut first_handoff_candidate_identity: Option<(String, String)> = None;

    let new_window = loop {
        if start.elapsed() > timeout {
            return Err(RestoreError::Hyprctl(HyprctlError::CommandFailed(format!(
                "timeout waiting for '{}' window to appear",
                client.class
            ))));
        }
        thread::sleep(poll_interval);

        let current = hyprctl.get_clients()?;
        let active_after = if browser_handoff_target_is_safe(client) {
            hyprctl.get_active_window_address().ok().flatten()
        } else {
            None
        };
        let candidates: Vec<HyprClient> = current
            .into_iter()
            .filter(|window| {
                let process_related =
                    launched_process_is_related(&launched, window.pid, process_info);
                let handoff_candidate = browser_handoff_candidate_is_safe(
                    client,
                    window,
                    &before,
                    active_before.as_deref(),
                    active_after.as_deref(),
                    process_info,
                );
                (!before.contains(&window.address) || process_related || handoff_candidate)
                    && classes_match(client, window)
                    && candidate_matches_profile(client, window, process_info)
            })
            .collect();
        if candidates.is_empty() {
            first_candidate_at = None;
            continue;
        }

        let candidate_seen_at = first_candidate_at.get_or_insert_with(Instant::now);
        let profile_metadata_incomplete = is_brave_client(client)
            && client.profile_directory.is_some()
            && candidates
                .iter()
                .any(|candidate| !find_profile_discovery(process_info, candidate.pid).complete);
        if profile_metadata_incomplete {
            // A profile lookup failure is not the same as Brave's normal
            // profile omitting its flag.  Keep polling briefly for a stable
            // process view, then fail closed without moving an unknown
            // profile.
            if candidate_seen_at.elapsed() >= candidate_settle {
                return Err(RestoreError::UncorrelatedWindow {
                    class: client.class.clone(),
                });
            }
            continue;
        }
        let process_related = candidates
            .iter()
            .any(|candidate| launched_process_is_related(&launched, candidate.pid, process_info));
        if launched.pid.is_some() && !process_related {
            if !is_browser_target(client) {
                // Keep the conservative process-tree rule for ordinary apps;
                // a newly visible but uncorrelated window is never accepted.
                continue;
            }
            let handoff_candidates: Vec<HyprClient> = candidates
                .iter()
                .filter(|candidate| {
                    browser_handoff_candidate_is_safe(
                        client,
                        candidate,
                        &before,
                        active_before.as_deref(),
                        active_after.as_deref(),
                        process_info,
                    )
                })
                .cloned()
                .collect();
            if handoff_candidates.len() == 1 {
                let candidate = &handoff_candidates[0];
                let identity = (
                    candidate.address.clone(),
                    candidate.stable_id.clone().unwrap_or_default(),
                );
                if first_handoff_candidate_identity.as_ref() != Some(&identity) {
                    first_handoff_candidate_identity = Some(identity);
                    first_handoff_candidate_at = Some(Instant::now());
                }
                if first_handoff_candidate_at
                    .is_some_and(|seen_at| seen_at.elapsed() >= candidate_settle)
                {
                    break choose_launched_window(
                        client,
                        handoff_candidates,
                        launched,
                        process_info,
                        true,
                    )?;
                }
            } else {
                first_handoff_candidate_identity = None;
                first_handoff_candidate_at = None;
                if handoff_candidates.len() > 1 && candidate_seen_at.elapsed() >= candidate_settle {
                    return Err(RestoreError::AmbiguousWindow {
                        class: client.class.clone(),
                        addresses: handoff_candidates
                            .iter()
                            .map(|candidate| candidate.address.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
            }
            if candidate_seen_at.elapsed() >= candidate_settle {
                return Err(RestoreError::UncorrelatedWindow {
                    class: client.class.clone(),
                });
            }
            continue;
        }
        if candidates.len() > 1 && !process_related {
            return Err(RestoreError::AmbiguousWindow {
                class: client.class.clone(),
                addresses: candidates
                    .iter()
                    .map(|candidate| candidate.address.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        if candidates.len() == 1
            && !process_related
            && candidate_seen_at.elapsed() < candidate_settle
        {
            continue;
        }
        break choose_launched_window(client, candidates, launched, process_info, false)?;
    };

    // 4. Use the same minimal repair logic as reconciliation.  The launch
    // PID/window correlation above prevents this address from belonging to a
    // different same-class window that appeared during startup.
    let mut commands = build_reconcile_dispatch_commands_with_geometry(
        client,
        &new_window,
        None,
        client.at,
        client.size,
        target_monitor_available,
    );
    if target_monitor_available && !client.monitor.is_empty() {
        let monitor_commands = monitor_move_commands(&client.monitor, &new_window.address);
        let insertion = commands
            .iter()
            .position(|command| command.starts_with("movetoworkspacesilent "))
            .map(|index| index + 1)
            .unwrap_or(0);
        commands.splice(insertion..insertion, monitor_commands);
    }
    let mut expected_window = ObservedClient::from_hypr_client(new_window.clone(), None, None);
    expected_window.process_start_time = process_info.get_start_time(new_window.pid).ok();
    expected_window.process_command = process_info.get_cmdline(new_window.pid).ok();
    dispatch_existing_repairs(&expected_window, &commands, hyprctl, process_info)?;

    // 7. Throttle subsequent launches to give the compositor time to settle.
    thread::sleep(Duration::from_millis(
        config
            .general
            .restore_delay_ms
            .min(crate::config::MAX_RESTORE_DELAY_MS),
    ));

    Ok(RestoredWindow {
        observed: expected_window,
        message: format!(
            "OK: {} → ws={} at {:?}",
            client.class, client.workspace, client.at
        ),
    })
}

fn candidate_matches_profile(
    target: &SessionClient,
    candidate: &HyprClient,
    process_info: &dyn ProcessInfoProvider,
) -> bool {
    if is_brave_client(target) && target.profile_identity_ambiguous {
        return false;
    }
    let Some(target_profile) = &target.profile_directory else {
        return true;
    };
    let discovery = find_profile_discovery(process_info, candidate.pid);
    if !discovery.complete {
        // Keep the candidate visible to the correlation loop so it can
        // report an explicit, conservative failure instead of timing out.
        // An incomplete process walk is never accepted as positive profile
        // evidence.
        return true;
    }
    match discovery.profiles.as_slice() {
        [candidate_profile] => candidate_profile.eq_ignore_ascii_case(target_profile),
        // The absence of --profile-directory is not positive evidence for
        // Brave's Default profile: an existing shared browser process can
        // hide the flag for a non-default window.  Only an explicit profile
        // flag can correlate a profile-aware launch.
        [] => false,
        // A shared browser process can advertise several profiles.  This is
        // only compatibility evidence; the caller must still correlate the
        // newly created window using its active address before moving it.
        _ => discovery
            .profiles
            .iter()
            .any(|profile| profile.eq_ignore_ascii_case(target_profile)),
    }
}

fn launched_process_is_related(
    launched: &LaunchedProcess,
    candidate_pid: u32,
    process_info: &dyn ProcessInfoProvider,
) -> bool {
    let Some(root_pid) = launched.pid else {
        return false;
    };
    let root_identity_matches = match (
        launched.start_time,
        process_info.get_start_time(root_pid).ok(),
    ) {
        (Some(expected), Some(actual)) => expected == actual,
        // If either side has an identity timestamp, failing to obtain the
        // other side is not proof that the PID is the same process.  This
        // check also protects descendant correlation when the launch PID was
        // recycled before a child window appeared.
        (Some(_), None) | (None, Some(_)) => false,
        // A provider which explicitly supports start times must fail closed
        // when both reads are unavailable.  Only lightweight providers that
        // opt out of timestamp support retain the historical PID fallback.
        (None, None) => !process_info.has_reliable_process_start_time(),
    };
    if !root_identity_matches {
        return false;
    }
    if root_pid == candidate_pid {
        return true;
    }
    process_info.is_process_related(root_pid, candidate_pid)
}

fn choose_launched_window(
    target: &SessionClient,
    candidates: Vec<HyprClient>,
    launched: LaunchedProcess,
    process_info: &dyn ProcessInfoProvider,
    allow_unrelated_handoff: bool,
) -> Result<HyprClient, RestoreError> {
    let candidates: Vec<HyprClient> = candidates
        .into_iter()
        .filter(|candidate| candidate_matches_profile(target, candidate, process_info))
        .collect();
    let related: Vec<HyprClient> = launched
        .pid
        .map(|_| {
            candidates
                .iter()
                .filter(|candidate| {
                    launched_process_is_related(&launched, candidate.pid, process_info)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if launched.pid.is_some() && related.is_empty() && !allow_unrelated_handoff {
        // A launcher PID is available, so accepting an unrelated candidate
        // would turn a process-correlation failure into a random window move.
        // Browser handoff is handled by the polling layer only when it has a
        // separate, positive identity proof.
        return Err(RestoreError::UncorrelatedWindow {
            class: target.class.clone(),
        });
    }
    let candidates = if related.is_empty() {
        candidates
    } else {
        related
    };

    if candidates.len() == 1 {
        return Ok(candidates
            .into_iter()
            .next()
            .expect("candidate list was checked as non-empty"));
    }

    let mut scored: Vec<(i32, String, HyprClient)> = candidates
        .into_iter()
        .filter_map(|candidate| {
            let discovery = find_profile_discovery(process_info, candidate.pid);
            let profile_directory = match discovery.profiles.as_slice() {
                [profile] => Some(profile.clone()),
                _ => None,
            };
            let mut observed = ObservedClient::with_profile_directory(
                candidate.clone(),
                None,
                None,
                profile_directory,
            );
            if is_brave_hypr_client(&candidate)
                && (observed.profile_directory.is_none() || !discovery.complete)
            {
                observed.profile_identity_ambiguous = true;
            }
            observed.process_command = process_info.get_cmdline(candidate.pid).ok();
            match_score(target, &observed)
                .map(|(score, _)| (score, candidate.address.clone(), candidate))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));

    let Some((best_score, _, best)) = scored.first() else {
        return Err(RestoreError::AmbiguousWindow {
            class: target.class.clone(),
            addresses: "no identifiable candidate".to_string(),
        });
    };
    if scored
        .get(1)
        .map(|second| second.0 == *best_score)
        .unwrap_or(false)
    {
        return Err(RestoreError::AmbiguousWindow {
            class: target.class.clone(),
            addresses: scored
                .iter()
                .map(|(_, address, _)| address.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    Ok(best.clone())
}

// ── Command builders (pure functions, unit-testable) ─────────────────────────

/// Build the argv vector used to spawn `client`'s application.
///
/// For `kitty` windows that carry a `hint` (e.g. the last shell command),
/// we append `-e zsh -c "<hint>; exec zsh"` so the terminal opens with
/// that hint visible and then drops to an interactive shell.
pub fn build_launch_command(client: &SessionClient) -> Vec<String> {
    let mut cmd = vec![effective_binary(client)];
    cmd.extend(client.launch.args.clone());

    if client.class.eq_ignore_ascii_case("kitty") {
        if let Some(hint) = &client.launch.hint {
            // Single-quote-escape the hint so it survives the shell invocation.
            let escaped = hint.replace('\'', "'\\''");
            cmd.push("-e".to_string());
            cmd.push("zsh".to_string());
            cmd.push("-c".to_string());
            cmd.push(format!("echo '{escaped}'; exec zsh"));
        }
    }

    cmd
}

fn effective_binary(client: &SessionClient) -> String {
    if is_ghostty_class(client)
        && (client.launch.command.is_empty()
            || client.launch.command.eq_ignore_ascii_case(&client.class)
            || client
                .launch
                .command
                .eq_ignore_ascii_case(&client.initial_class))
    {
        "ghostty".to_string()
    } else {
        client.launch.command.clone()
    }
}

/// Saved sessions are user-owned state, but automatic restore must not turn a
/// hand-edited launch command into an arbitrary executable hook.  Commands
/// captured from the app identity remain valid; apps whose executable differs
/// from their Hyprland class must explicitly opt in through `apps.<class>.binary`.
fn launch_command_is_trusted(client: &SessionClient, config: &Config) -> bool {
    let command = effective_binary(client);
    if app_config_for(config, &client.class, &client.initial_class)
        .and_then(|app| app.binary.as_deref())
        .map(|configured| configured == command)
        .unwrap_or(false)
    {
        return true;
    }

    if is_ghostty_class(client) && command.eq_ignore_ascii_case("ghostty") {
        return true;
    }

    if is_brave_client(client) && command.eq_ignore_ascii_case("brave") {
        return true;
    }

    // Omarchy Chrome web apps are represented by classes such as
    // `chrome-chatgpt.com__-Default`, while their executable is the
    // user-facing launcher stored in the matching .desktop entry.  The
    // focus wrappers use eval internally, so validate their complete argv
    // before allowing a saved session to run them automatically.
    if is_webapp_class(client) {
        return webapp_launch_is_trusted(client, &command);
    }

    [client.class.as_str(), client.initial_class.as_str()]
        .iter()
        .any(|identity| !identity.is_empty() && identity.eq_ignore_ascii_case(&command))
}

fn resolve_launch_binary(command: &str, target: &str) -> Result<PathBuf, RestoreError> {
    let path = which::which(command).map_err(|_| RestoreError::MissingLaunchBinary {
        target: target.to_string(),
        command: command.to_string(),
    })?;
    let metadata = std::fs::metadata(&path).map_err(|_| RestoreError::MissingLaunchBinary {
        target: target.to_string(),
        command: command.to_string(),
    })?;
    if !metadata.is_file() || {
        #[cfg(unix)]
        {
            metadata.permissions().mode() & 0o111 == 0
        }
        #[cfg(not(unix))]
        {
            false
        }
    } {
        return Err(RestoreError::MissingLaunchBinary {
            target: target.to_string(),
            command: command.to_string(),
        });
    }
    Ok(path)
}

fn is_ghostty_class(client: &SessionClient) -> bool {
    [client.class.as_str(), client.initial_class.as_str()]
        .iter()
        .any(|class| {
            class.eq_ignore_ascii_case("ghostty")
                || class.eq_ignore_ascii_case("com.mitchellh.ghostty")
        })
}

fn is_webapp_class(client: &SessionClient) -> bool {
    [client.class.as_str(), client.initial_class.as_str()]
        .iter()
        .any(|class| is_webapp_class_name(class))
}

fn is_webapp_class_name(class: &str) -> bool {
    class
        .get(.."chrome-".len())
        .map(|prefix| prefix.eq_ignore_ascii_case("chrome-"))
        .unwrap_or(false)
}

fn webapp_launch_is_trusted(client: &SessionClient, command: &str) -> bool {
    let identities = [client.class.as_str(), client.initial_class.as_str()];
    if command.eq_ignore_ascii_case("omarchy-launch-webapp") {
        return client.launch.args.first().is_some_and(|url| {
            identities
                .iter()
                .any(|identity| crate::capture::webapp_class_matches_url(identity, url))
                && client
                    .launch
                    .args
                    .iter()
                    .all(|argument| !contains_shell_metacharacter(argument))
        });
    }

    let Some(class_argument) = client.launch.args.first() else {
        return false;
    };
    let class_matches = identities
        .iter()
        .any(|identity| !identity.is_empty() && identity.eq_ignore_ascii_case(class_argument));
    if !class_matches || contains_shell_metacharacter_or_quote(class_argument) {
        return false;
    }

    if command.eq_ignore_ascii_case("omarchy-launch-or-focus-webapp") {
        return client.launch.args.get(1).is_some_and(|url| {
            identities
                .iter()
                .any(|identity| crate::capture::webapp_class_matches_url(identity, url))
                && client.launch.args[1..]
                    .iter()
                    .all(|argument| !contains_shell_metacharacter(argument))
        });
    }

    if command.eq_ignore_ascii_case("omarchy-launch-or-focus") {
        let Some(nested) = client.launch.args.get(1) else {
            return false;
        };
        let Some(nested_args) = nested.strip_prefix("omarchy-launch-webapp ") else {
            return false;
        };
        let Some(url) = nested_args.split_whitespace().next() else {
            return false;
        };
        return identities
            .iter()
            .any(|identity| crate::capture::webapp_class_matches_url(identity, url))
            && !contains_shell_metacharacter(nested);
    }

    false
}

fn contains_shell_metacharacter(value: &str) -> bool {
    value.chars().any(|character| {
        matches!(
            character,
            ';' | '&' | '|' | '$' | '`' | '(' | ')' | '<' | '>' | '\\' | '\n' | '\r'
        )
    })
}

fn contains_shell_metacharacter_or_quote(value: &str) -> bool {
    contains_shell_metacharacter(value) || value.contains(['\'', '"'])
}

/// Build the list of `hyprctl dispatch` argument strings that would be
/// issued for a given client.  Used both by the dry-run path and by tests.
pub fn build_dispatch_commands(client: &SessionClient) -> Vec<String> {
    build_dispatch_commands_for_monitor(client, true)
}

fn build_dispatch_commands_for_monitor(
    client: &SessionClient,
    target_monitor_available: bool,
) -> Vec<String> {
    let addr = "address:0xNEW";
    let launch = build_launch_command(client);

    let mut cmds = vec![format!("exec {}", launch.join(" "))];
    cmds.push(format!(
        "movetoworkspacesilent {},{}",
        quote_dispatch_token(&workspace_selector(client)),
        addr
    ));
    if target_monitor_available && !client.monitor.is_empty() {
        cmds.extend(monitor_move_commands(&client.monitor, "0xNEW"));
    }
    if client.monitor.is_empty() || target_monitor_available {
        cmds.extend([
            format!(
                "resizewindowpixel exact {} {},{}",
                client.size[0], client.size[1], addr
            ),
            format!(
                "movewindowpixel exact {} {},{}",
                client.at[0], client.at[1], addr
            ),
        ]);
    }
    if client.floating {
        cmds.push(format!("togglefloating {addr}"));
    }
    if client.fullscreen > 0 {
        cmds.push(format!("focuswindow {addr}"));
        cmds.push(format!(
            "fullscreenstate {} {}",
            client.fullscreen, client.fullscreen
        ));
    }
    if client.pinned {
        cmds.push(format!("pin {addr}"));
    }

    cmds
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, FilterConfig, GeneralConfig};
    use crate::hyprctl::{HyprClient, HyprMonitor};
    use crate::process::{ChildProcess, ProcessError, ProcessInfoProvider};
    use crate::session::{
        mark_replace_prepared, replace_marker, BraveProfile, LaunchInfo, ReplacePhase, Session,
        SessionClient,
    };
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // ── Helpers ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn make_client(
        class: &str,
        workspace: i32,
        at: [i32; 2],
        size: [i32; 2],
        floating: bool,
        fullscreen: u8,
        command: &str,
        args: Vec<String>,
        hint: Option<String>,
    ) -> SessionClient {
        SessionClient {
            class: class.to_string(),
            title: class.to_string(),
            address: None,
            pid: None,
            process_start_time: None,
            stable_id: None,
            initial_class: class.to_string(),
            initial_title: class.to_string(),
            workspace,
            workspace_name: workspace.to_string(),
            monitor: "DP-1".to_string(),
            at,
            size,
            floating,
            fullscreen,
            pinned: false,
            profile_directory: None,
            profile_identity_ambiguous: false,
            focus_history_id: 0,
            launch: LaunchInfo {
                command: command.to_string(),
                args,
                hint,
            },
        }
    }

    fn make_session(clients: Vec<SessionClient>) -> Session {
        Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.0".to_string(),
            monitors: vec![],
            clients,
            brave_profiles: vec![],
        }
    }

    #[test]
    fn test_maximum_weight_assignment_is_global_not_greedy() {
        // A greedy pass takes 100 for the first row and leaves only 0 for the
        // second.  The global optimum is 99 + 98.
        let assignment = maximum_weight_assignment(&[vec![100, 99], vec![98, 0]]);

        assert_eq!(assignment, vec![Some(1), Some(0)]);
    }

    #[test]
    fn test_matching_does_not_cross_runtime_and_initial_class_fields() {
        let mut target = make_client(
            "wrapper",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "wrapper",
            vec![],
            None,
        );
        target.initial_class = "app-a".to_string();

        let mut current =
            make_reconcile_window("0xunrelated", "app-a", "app-a", 1, 0, [10, 20], [800, 600]);
        current.initial_class = "app-b".to_string();

        let plan = plan_reconciliation(
            &[target],
            &[ObservedClient::from_hypr_client(current, None, None)],
        );

        assert_eq!(plan, vec![None]);
    }

    #[test]
    fn test_matching_rejects_two_independent_same_class_identity_conflicts() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/project-a".to_string()],
            None,
        );
        target.title = "Project A".to_string();
        let current =
            make_reconcile_window("0xother", "kitty", "Project B", 1, 0, [10, 20], [800, 600]);
        let observed =
            ObservedClient::from_hypr_client(current, None, Some(PathBuf::from("/project-b")));

        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_matching_rejects_same_class_with_conflicting_initial_title() {
        let mut target = make_client(
            "example-app",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "example-app",
            vec![],
            None,
        );
        target.title = "Project A".to_string();
        target.initial_title = "Project A".to_string();

        let current = make_reconcile_window(
            "0xextra",
            "example-app",
            "Project B",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        let observed = ObservedClient::from_hypr_client(current, None, None);

        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_matching_allows_live_title_changes_when_initial_title_is_generic() {
        let mut target = make_client(
            "example-app",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "example-app",
            vec![],
            None,
        );
        target.initial_title = "Example App".to_string();
        target.title = "Project A".to_string();

        let mut current = make_reconcile_window(
            "0xextra",
            "example-app",
            "Project B",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.initial_title = "Example App".to_string();
        let observed = ObservedClient::from_hypr_client(current, None, None);

        let plan = plan_reconciliation(&[target], &[observed]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].map(|pair| pair.kind), Some(MatchKind::AppIdentity));
    }

    #[test]
    fn test_matching_does_not_consume_unrelated_generic_chromium_window() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.title = "ChatGPT".to_string();
        target.initial_title = "Chromium".to_string();

        let mut current = make_reconcile_window(
            "0xunrelated",
            "chromium",
            "YouTube",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.initial_title = "Chromium".to_string();

        let observed = ObservedClient::from_hypr_client(current, None, None);
        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_matching_uses_captured_address_for_generic_chromium_window_after_title_change() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.title = "New Tab".to_string();
        target.initial_title = "Chromium".to_string();
        target.address = Some("0xunrelated-identical-title".to_string());

        let mut current = make_reconcile_window(
            "0xunrelated-identical-title",
            "chromium",
            "Project page",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.initial_title = "Chromium".to_string();

        let observed = ObservedClient::from_hypr_client(current, None, None);
        assert_eq!(
            plan_reconciliation(&[target], &[observed])[0].map(|pair| pair.kind),
            Some(MatchKind::ExactIdentity)
        );
    }

    #[test]
    fn test_matching_rejects_reused_address_when_process_identity_changes() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/project".to_string()],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xreused".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(10);

        let mut current =
            make_reconcile_window("0xreused", "kitty", "Project", 1, 0, [10, 20], [800, 600]);
        current.pid = 2000;
        let mut observed = ObservedClient::from_hypr_client(current, None, None);
        observed.process_start_time = Some(20);

        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_matching_rejects_address_when_process_identity_is_partial() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xreused".to_string());
        target.pid = Some(1000);
        // A capture that obtained a PID but not its start time has only
        // partial process identity and must not trust an exact address.
        target.process_start_time = None;

        let mut current =
            make_reconcile_window("0xreused", "kitty", "Project", 1, 0, [10, 20], [800, 600]);
        current.pid = 1000;
        let mut observed = ObservedClient::from_hypr_client(current, None, None);
        observed.process_start_time = Some(20);

        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_matching_reuses_restarted_terminal_by_unique_cwd_and_command() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/project".to_string()],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xbefore-restart".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(10);

        let mut current = make_reconcile_window(
            "0xafter-restart",
            "kitty",
            "Project",
            7,
            0,
            [900, 100],
            [700, 500],
        );
        current.initial_title = "Project".to_string();
        current.pid = 2000;
        let mut observed =
            ObservedClient::from_hypr_client(current, None, Some(PathBuf::from("/project")));
        observed.process_start_time = Some(20);
        observed.process_command = Some("/usr/bin/kitty --directory /project".to_string());

        let plan = plan_reconciliation(&[target], &[observed]);

        assert_eq!(plan[0].map(|pair| pair.current_index), Some(0));
    }

    #[test]
    fn test_matching_rejects_ambiguous_restarted_terminals_with_same_signature() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/project".to_string()],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xbefore-restart".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(10);

        let mut first = make_reconcile_window(
            "0xafter-restart-a",
            "kitty",
            "Project",
            7,
            0,
            [900, 100],
            [700, 500],
        );
        first.initial_title = "Project".to_string();
        first.pid = 2000;
        let mut first =
            ObservedClient::from_hypr_client(first, None, Some(PathBuf::from("/project")));
        first.process_command = Some("/usr/bin/kitty --directory /project".to_string());

        let mut second = make_reconcile_window(
            "0xafter-restart-b",
            "kitty",
            "Project",
            7,
            0,
            [100, 100],
            [700, 500],
        );
        second.initial_title = "Project".to_string();
        second.pid = 3000;
        let mut second =
            ObservedClient::from_hypr_client(second, None, Some(PathBuf::from("/project")));
        second.process_command = Some("/usr/bin/kitty --directory /project".to_string());

        assert_eq!(plan_reconciliation(&[target], &[first, second]), vec![None]);
    }

    #[test]
    fn test_legacy_matching_rejects_address_when_current_stable_id_is_unpaired() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.address = Some("0xlegacy".to_string());

        let mut current =
            make_reconcile_window("0xlegacy", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);
        current.stable_id = Some("stable-current".to_string());

        assert_eq!(
            plan_reconciliation(
                &[target],
                &[ObservedClient::from_hypr_client(current, None, None)]
            ),
            vec![None]
        );
    }

    #[test]
    fn test_consumed_windows_reject_identity_metadata_loss() {
        let mut first =
            make_reconcile_window("0xsame", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);
        first.stable_id = Some("stable-window".to_string());
        let mut first = ObservedClient::from_hypr_client(first, None, None);
        first.process_start_time = Some(10);

        let mut refreshed =
            make_reconcile_window("0xsame", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);
        refreshed.pid = first.client.pid;
        let refreshed = ObservedClient::from_hypr_client(refreshed, None, None);

        let mut consumed = ConsumedWindows::default();
        consumed.insert(&first);

        assert!(!consumed.contains(&refreshed));
        consumed.insert(&refreshed);
        assert_eq!(consumed.entries.len(), 2);
    }

    #[test]
    fn test_matching_rejects_reused_address_with_same_process_identity_but_new_stable_id() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.address = Some("0xreused".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(20);
        target.stable_id = Some("0xwindow-a".to_string());

        let mut current = make_reconcile_window(
            "0xreused",
            "chromium",
            "New tab",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.pid = 1000;
        current.stable_id = Some("0xwindow-b".to_string());
        let mut observed = ObservedClient::from_hypr_client(current, None, None);
        observed.process_start_time = Some(20);

        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_dispatch_revalidates_window_identity_before_repair() {
        let mut expected_client =
            make_reconcile_window("0xreused", "chromium", "Old tab", 1, 0, [0, 0], [800, 600]);
        expected_client.stable_id = Some("0xwindow-a".to_string());
        let expected = ObservedClient::from_hypr_client(expected_client, None, None);

        let mut reused =
            make_reconcile_window("0xreused", "chromium", "New tab", 1, 0, [0, 0], [800, 600]);
        reused.stable_id = Some("0xwindow-b".to_string());
        let mock = MockHyprctl::new(vec![vec![reused]]);
        let result = dispatch_existing_repairs(
            &expected,
            &["focuswindow address:0xreused".to_string()],
            &mock,
            &EmptyProcessInfo,
        );

        assert!(matches!(
            result,
            Err(RestoreError::WindowIdentityChanged { address }) if address == "0xreused"
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_dispatch_rejects_partial_observation_with_reliable_process_provider() {
        let expected_client =
            make_reconcile_window("0xreused", "kitty", "Old", 1, 0, [0, 0], [800, 600]);
        let expected = ObservedClient::from_hypr_client(expected_client, None, None);
        let current = make_reconcile_window("0xreused", "kitty", "New", 1, 0, [0, 0], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![current]]);

        let result = dispatch_existing_repairs(
            &expected,
            &["focuswindow address:0xreused".to_string()],
            &mock,
            &ReliableStartTimeUnavailableProcessInfo,
        );

        assert!(matches!(
            result,
            Err(RestoreError::WindowIdentityChanged { address }) if address == "0xreused"
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_dispatch_rejects_same_process_sibling_without_stable_ids() {
        let expected_client =
            make_reconcile_window("0xexpected", "kitty", "Old", 1, 0, [0, 0], [800, 600]);
        let expected = ObservedClient::from_hypr_client(expected_client, None, None);
        let current = make_reconcile_window("0xexpected", "kitty", "Old", 1, 0, [0, 0], [800, 600]);
        let sibling =
            make_reconcile_window("0xsibling", "kitty", "Other", 1, 0, [900, 0], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![current, sibling]]);

        let result = dispatch_existing_repairs(
            &expected,
            &["focuswindow address:0xexpected".to_string()],
            &mock,
            &EmptyProcessInfo,
        );

        assert!(matches!(
            result,
            Err(RestoreError::WindowIdentityChanged { address }) if address == "0xexpected"
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_matching_generic_chromium_address_survives_focus_history_change() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.address = Some("0xbrowser".to_string());
        target.focus_history_id = 3;

        let mut current = make_reconcile_window(
            "0xbrowser",
            "chromium",
            "Project page",
            1,
            0,
            [900, 500],
            [800, 600],
        );
        current.focus_history_id = 17;
        let observed = ObservedClient::from_hypr_client(current, None, None);

        assert_eq!(
            plan_reconciliation(&[target], &[observed])[0].map(|pair| pair.kind),
            Some(MatchKind::ExactIdentity)
        );
    }

    #[test]
    fn test_matching_does_not_use_focus_history_for_addressless_chromium() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.address = None;
        target.focus_history_id = 3;

        let mut current = make_reconcile_window(
            "0xreused-focus-id",
            "chromium",
            "Unrelated tab",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.focus_history_id = 3;

        assert_eq!(
            plan_reconciliation(
                &[target],
                &[ObservedClient::from_hypr_client(current, None, None)]
            ),
            vec![None]
        );
    }

    #[test]
    fn test_matching_rejects_process_only_identity_on_reused_address() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.address = Some("0xreused-process".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(20);

        let mut current = make_reconcile_window(
            "0xreused-process",
            "kitty",
            "New window from same process",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.pid = 1000;
        let mut observed = ObservedClient::from_hypr_client(current, None, None);
        observed.process_start_time = Some(20);

        assert_eq!(plan_reconciliation(&[target], &[observed]), vec![None]);
    }

    #[test]
    fn test_matching_does_not_move_same_title_window_when_saved_address_is_gone() {
        let mut target = make_client(
            "com.mitchellh.ghostty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "ghostty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xclosed".to_string());

        let mut current = make_reconcile_window(
            "0xother",
            "com.mitchellh.ghostty",
            "Project",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.initial_class = "com.mitchellh.ghostty".to_string();
        current.initial_title = "Project".to_string();

        assert_eq!(
            plan_reconciliation(
                &[target],
                &[ObservedClient::from_hypr_client(current, None, None)]
            ),
            vec![None]
        );
    }

    #[test]
    fn test_matching_prefers_saved_address_for_same_app_windows_after_one_moves() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xintended".to_string());

        let mut intended = make_reconcile_window(
            "0xintended",
            "kitty",
            "Project",
            1,
            0,
            [900, 500],
            [800, 600],
        );
        intended.initial_title = "Project".to_string();
        let mut same_title =
            make_reconcile_window("0xother", "kitty", "Project", 1, 0, [10, 20], [800, 600]);
        same_title.initial_title = "Project".to_string();

        let observed = vec![
            ObservedClient::from_hypr_client(intended, None, None),
            ObservedClient::from_hypr_client(same_title, None, None),
        ];
        let plan = plan_reconciliation(&[target], &observed);

        assert_eq!(plan[0].map(|pair| pair.current_index), Some(0));
        assert_eq!(
            plan[0].map(|pair| pair.kind),
            Some(MatchKind::ExactIdentity)
        );
    }

    #[test]
    fn test_matching_duplicate_generic_chromium_titles_uses_captured_address() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.title = "New Tab".to_string();
        target.initial_title = "Chromium".to_string();
        target.address = Some("0xin-place".to_string());

        let mut in_place = make_reconcile_window(
            "0xin-place",
            "chromium",
            "New Tab",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        in_place.initial_title = "Chromium".to_string();
        let mut extra = make_reconcile_window(
            "0xextra-same-title",
            "chromium",
            "New Tab",
            1,
            0,
            [900, 20],
            [800, 600],
        );
        extra.initial_title = "Chromium".to_string();

        let observed = vec![
            ObservedClient::from_hypr_client(in_place, None, None),
            ObservedClient::from_hypr_client(extra, None, None),
        ];
        let plan = plan_reconciliation(&[target], &observed);

        assert_eq!(plan[0].map(|pair| pair.current_index), Some(0));
    }

    #[test]
    fn test_matching_legacy_generic_chromium_without_identity_fails_closed() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.title = "New Tab".to_string();
        target.initial_title = "Chromium".to_string();

        let mut first = make_reconcile_window(
            "0xsame-title",
            "chromium",
            "New Tab",
            1,
            0,
            [300, 20],
            [800, 600],
        );
        first.initial_title = "Chromium".to_string();

        let observed = vec![ObservedClient::from_hypr_client(first, None, None)];
        assert_eq!(plan_reconciliation(&[target], &observed), vec![None]);
    }

    #[test]
    fn test_replacement_target_does_not_accept_ambiguous_generic_title_match() {
        let mut target = make_client(
            "example-app",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "example-app",
            vec![],
            None,
        );
        target.initial_title = "Example App".to_string();
        target.title = "Project A".to_string();

        let mut current = make_reconcile_window(
            "0xextra",
            "example-app",
            "Project B",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.initial_title = "Example App".to_string();
        let mock = MockHyprctl::new(vec![vec![current]]);

        assert!(!replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_target_does_not_accept_ambiguous_ghostty_title_match() {
        let mut target = make_client(
            "com.mitchellh.ghostty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "ghostty",
            vec![],
            None,
        );
        target.initial_title = "Ghostty".to_string();
        target.title = "Project A".to_string();

        let mut current = make_reconcile_window(
            "0xextra-ghostty",
            "ghostty",
            "Project B",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        current.initial_class = "com.mitchellh.ghostty".to_string();
        current.initial_title = "Ghostty".to_string();
        let mock = MockHyprctl::new(vec![vec![current]]);

        assert!(!replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_matching_distance_handles_extreme_coordinates_without_overflow() {
        assert_eq!(
            manhattan_distance([i32::MIN, i32::MIN], [i32::MAX, i32::MAX]),
            8_589_934_590
        );
    }

    #[test]
    fn test_geometry_adapts_to_changed_monitor_origin_and_resolution() {
        let target = make_client(
            "kitty",
            1,
            [100, 50],
            [960, 540],
            true,
            0,
            "kitty",
            vec![],
            None,
        );
        let saved_monitor = Monitor {
            name: "DP-1".to_string(),
            width: 1920,
            height: 1080,
            transform: 0,
            x: Some(0),
            y: Some(0),
        };
        let current_monitor = HyprMonitor {
            id: 0,
            name: "DP-1".to_string(),
            width: 2560,
            height: 1440,
            transform: 0,
            x: Some(1920),
            y: Some(0),
        };

        let adapted = adapt_client_geometry(&target, &[saved_monitor], Some(&current_monitor));

        assert_eq!(adapted.at, [2053, 67]);
        assert_eq!(adapted.size, [1280, 720]);
    }

    #[test]
    fn test_geometry_rotates_relative_window_when_monitor_transform_changes() {
        let target = make_client(
            "kitty",
            1,
            [100, 50],
            [400, 300],
            true,
            0,
            "kitty",
            vec![],
            None,
        );
        let saved_monitor = Monitor {
            name: "DP-1".to_string(),
            width: 1920,
            height: 1080,
            transform: 0,
            x: Some(0),
            y: Some(0),
        };
        let current_monitor = HyprMonitor {
            id: 0,
            name: "DP-1".to_string(),
            width: 1920,
            height: 1080,
            transform: 1,
            x: Some(0),
            y: Some(0),
        };

        let adapted = adapt_client_geometry(&target, &[saved_monitor], Some(&current_monitor));

        assert_eq!(adapted.at, [730, 100]);
        assert_eq!(adapted.size, [300, 400]);
    }

    #[test]
    fn test_geometry_keeps_legacy_session_coordinates_without_monitor_origin() {
        let target = make_client(
            "kitty",
            1,
            [100, 50],
            [960, 540],
            true,
            0,
            "kitty",
            vec![],
            None,
        );
        let saved_monitor = Monitor {
            name: "DP-1".to_string(),
            width: 1920,
            height: 1080,
            transform: 0,
            x: None,
            y: None,
        };
        let current_monitor = HyprMonitor {
            id: 0,
            name: "DP-1".to_string(),
            width: 2560,
            height: 1440,
            transform: 0,
            x: Some(1920),
            y: Some(0),
        };

        let adapted = adapt_client_geometry(&target, &[saved_monitor], Some(&current_monitor));

        assert_eq!(adapted.at, target.at);
        assert_eq!(adapted.size, target.size);
    }

    fn make_reconcile_window(
        address: &str,
        class: &str,
        title: &str,
        workspace: i32,
        monitor: i32,
        at: [i32; 2],
        size: [i32; 2],
    ) -> HyprClient {
        HyprClient {
            address: address.to_string(),
            class: class.to_string(),
            title: title.to_string(),
            initial_class: class.to_string(),
            initial_title: title.to_string(),
            workspace: crate::hyprctl::HyprWorkspace {
                id: workspace,
                name: workspace.to_string(),
            },
            monitor,
            at,
            size,
            floating: false,
            fullscreen: 0,
            pinned: false,
            stable_id: None,
            focus_history_id: 0,
            pid: 1000,
        }
    }

    struct EmptyProcessInfo;

    impl ProcessInfoProvider for EmptyProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(
            &self,
            _pid: u32,
        ) -> Result<Vec<crate::process::ChildProcess>, ProcessError> {
            Ok(vec![])
        }
    }

    struct BraveProfileProcessInfo;

    impl ProcessInfoProvider for BraveProfileProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(&self, _pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(vec![])
        }

        fn get_cmdline(&self, pid: u32) -> Result<String, ProcessError> {
            match pid {
                1000 => Ok("brave --profile-directory=Default".to_string()),
                _ => Err(ProcessError::NotFound(pid)),
            }
        }
    }

    struct SharedBraveProfileProcessInfo;

    impl ProcessInfoProvider for SharedBraveProfileProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            if pid == 1000 {
                Ok(vec![ChildProcess {
                    pid: 2000,
                    cwd: PathBuf::from("/tmp"),
                    cmdline: "brave --profile-directory=Default".to_string(),
                }])
            } else {
                Ok(vec![])
            }
        }

        fn get_cmdline(&self, pid: u32) -> Result<String, ProcessError> {
            match pid {
                1000 => Ok("brave --profile-directory=Profile 1".to_string()),
                _ => Err(ProcessError::NotFound(pid)),
            }
        }
    }

    struct CwdProcessInfo {
        cwds: HashMap<u32, PathBuf>,
    }

    impl ProcessInfoProvider for CwdProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            self.cwds
                .get(&pid)
                .cloned()
                .ok_or(ProcessError::NotFound(pid))
        }

        fn get_children(&self, _pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(vec![])
        }
    }

    struct RelaunchedTerminalProcessInfo;

    impl ProcessInfoProvider for RelaunchedTerminalProcessInfo {
        fn get_cwd(&self, _pid: u32) -> Result<PathBuf, ProcessError> {
            Ok(PathBuf::from("/project"))
        }

        fn get_children(&self, _pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(vec![])
        }

        fn get_cmdline(&self, _pid: u32) -> Result<String, ProcessError> {
            Ok("/usr/bin/kitty --directory /project".to_string())
        }
    }

    struct ChildCwdProcessInfo;

    impl ProcessInfoProvider for ChildCwdProcessInfo {
        fn get_cwd(&self, _pid: u32) -> Result<PathBuf, ProcessError> {
            Ok(PathBuf::from("/terminal-start-directory"))
        }

        fn get_children(&self, _pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(vec![ChildProcess {
                pid: 2000,
                cwd: PathBuf::from("/shell-current-directory"),
                cmdline: "zsh".to_string(),
            }])
        }
    }

    struct RelatedProcessInfo {
        children: HashMap<u32, Vec<ChildProcess>>,
    }

    impl ProcessInfoProvider for RelatedProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }
    }

    struct StartTimeProcessInfo {
        start_time: u64,
    }

    impl ProcessInfoProvider for StartTimeProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(&self, _pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(vec![])
        }

        fn get_start_time(&self, _pid: u32) -> Result<u64, ProcessError> {
            Ok(self.start_time)
        }
    }

    struct ReliableStartTimeUnavailableProcessInfo;

    impl ProcessInfoProvider for ReliableStartTimeUnavailableProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(&self, _pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(vec![])
        }

        fn has_reliable_process_start_time(&self) -> bool {
            true
        }
    }

    struct StartTimeRelatedProcessInfo {
        start_time: u64,
        children: HashMap<u32, Vec<ChildProcess>>,
    }

    impl ProcessInfoProvider for StartTimeRelatedProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            Err(ProcessError::NotFound(pid))
        }

        fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }

        fn get_start_time(&self, _pid: u32) -> Result<u64, ProcessError> {
            Ok(self.start_time)
        }
    }

    #[derive(Default)]
    struct RecordingLauncher {
        launches: RefCell<Vec<(String, Vec<String>)>>,
        pid: Option<u32>,
        start_time: Option<u64>,
    }

    impl ProcessLauncher for RecordingLauncher {
        fn spawn(&self, command: &str, args: &[String]) -> Result<LaunchedProcess, std::io::Error> {
            self.launches
                .borrow_mut()
                .push((command.to_string(), args.to_vec()));
            Ok(LaunchedProcess {
                pid: self.pid,
                start_time: self.start_time,
            })
        }
    }

    #[test]
    fn test_launch_correlation_rejects_reused_root_pid() {
        let launched = LaunchedProcess {
            pid: Some(1000),
            start_time: Some(10),
        };
        let reused = StartTimeProcessInfo { start_time: 11 };
        let same_process = StartTimeProcessInfo { start_time: 10 };

        assert!(!launched_process_is_related(&launched, 1000, &reused));
        assert!(launched_process_is_related(&launched, 1000, &same_process));
    }

    #[test]
    fn test_launch_correlation_rejects_descendants_of_a_reused_root_pid() {
        let launched = LaunchedProcess {
            pid: Some(1000),
            start_time: Some(10),
        };
        let child = ChildProcess {
            pid: 2000,
            cwd: PathBuf::new(),
            cmdline: "example-app".to_string(),
        };
        let reused = StartTimeRelatedProcessInfo {
            start_time: 11,
            children: HashMap::from([(1000, vec![child.clone()])]),
        };
        let same_process = StartTimeRelatedProcessInfo {
            start_time: 10,
            children: HashMap::from([(1000, vec![child])]),
        };

        assert!(!launched_process_is_related(&launched, 2000, &reused));
        assert!(launched_process_is_related(&launched, 2000, &same_process));
    }

    #[test]
    fn test_launch_correlation_fails_closed_when_reliable_start_time_is_unavailable() {
        let launched = LaunchedProcess {
            pid: Some(1000),
            start_time: None,
        };

        assert!(!launched_process_is_related(
            &launched,
            1000,
            &ReliableStartTimeUnavailableProcessInfo
        ));
    }

    #[test]
    fn test_replacement_target_is_complete_when_all_windows_are_present() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.address = Some("0xpresent".to_string());
        target.stable_id = Some("stable-target".to_string());
        let current =
            make_reconcile_window("0xpresent", "kitty", "kitty", 7, 0, [900, 100], [700, 500]);
        let mut current = current;
        current.stable_id = Some("stable-target".to_string());
        let mock = MockHyprctl::new(vec![vec![current]]);

        assert!(replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_target_is_not_complete_while_old_window_remains() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xnew-target".to_string());

        let mut old_window = make_reconcile_window(
            "0xold-window",
            "kitty",
            "Project",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        old_window.initial_title = "Project".to_string();
        let mut backup_client = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        backup_client.title = "Project".to_string();
        backup_client.initial_title = "Project".to_string();
        backup_client.address = Some("0xold-window".to_string());

        let current = MockHyprctl::new(vec![vec![old_window]]);
        assert!(!replacement_target_is_complete_with_backup(
            &make_session(vec![target]),
            Some(&make_session(vec![backup_client])),
            &current,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_completion_accepts_relaunched_window_with_new_address() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/project".to_string()],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xtarget-before-crash".to_string());
        target.stable_id = Some("stable-target".to_string());

        let mut relaunched = make_reconcile_window(
            "0xtarget-after-relaunch",
            "kitty",
            "Project",
            1,
            0,
            [900, 100],
            [700, 500],
        );
        relaunched.initial_title = "Project".to_string();
        relaunched.stable_id = Some("stable-target".to_string());

        let mut backup_client = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        backup_client.address = Some("0xold-safety-window".to_string());

        let mock = MockHyprctl::new(vec![vec![relaunched]]);

        assert!(replacement_target_is_complete_with_backup(
            &make_session(vec![target]),
            Some(&make_session(vec![backup_client])),
            &mock,
            &RelaunchedTerminalProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_completion_does_not_trust_stable_id_across_address_change() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xtarget-before-restart".to_string());
        target.stable_id = Some("18000000".to_string());

        let mut unrelated = make_reconcile_window(
            "0xtarget-after-restart",
            "kitty",
            "Unrelated",
            2,
            0,
            [10, 20],
            [800, 600],
        );
        unrelated.stable_id = Some("18000000".to_string());
        let mock = MockHyprctl::new(vec![vec![unrelated]]);

        assert!(!replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_completion_accepts_relaunched_terminal_by_signature() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/project".to_string()],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xtarget-before-crash".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(10);

        let mut relaunched = make_reconcile_window(
            "0xtarget-after-relaunch",
            "kitty",
            "Project",
            1,
            0,
            [900, 100],
            [700, 500],
        );
        relaunched.initial_title = "Project".to_string();
        relaunched.pid = 2000;
        let mock = MockHyprctl::new(vec![vec![relaunched]]);

        assert!(replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &RelaunchedTerminalProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_completion_rejects_same_title_and_binary_without_cwd() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xtarget-before-crash".to_string());

        let mut unrelated = make_reconcile_window(
            "0xunrelated-after-crash",
            "kitty",
            "Project",
            1,
            0,
            [900, 100],
            [700, 500],
        );
        unrelated.initial_title = "Project".to_string();
        let mock = MockHyprctl::new(vec![vec![unrelated]]);

        assert!(!replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &RelaunchedTerminalProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_completion_rejects_reopened_safety_window_as_target() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xtarget-before-crash".to_string());

        let mut safety = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        safety.title = "Project".to_string();
        safety.initial_title = "Project".to_string();
        safety.address = Some("0xold-safety-window".to_string());

        let reopened = make_reconcile_window(
            "0xreopened-safety-window",
            "kitty",
            "Project",
            1,
            0,
            [900, 100],
            [700, 500],
        );
        let mock = MockHyprctl::new(vec![vec![reopened]]);

        assert!(!replacement_target_is_complete_with_backup(
            &make_session(vec![target]),
            Some(&make_session(vec![safety])),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_replacement_completion_rejects_generic_chromium_relaunch_by_title_only() {
        let mut target = make_client(
            "chromium",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.title = "Project dashboard".to_string();
        target.initial_title = "Chromium".to_string();
        target.address = Some("0xbrowser-before-crash".to_string());

        let mut relaunched = make_reconcile_window(
            "0xbrowser-after-relaunch",
            "chromium",
            "Project dashboard",
            1,
            0,
            [900, 100],
            [700, 500],
        );
        relaunched.initial_title = "Chromium".to_string();

        let mock = MockHyprctl::new(vec![vec![relaunched]]);

        assert!(!replacement_target_is_complete(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_reconcile_does_not_launch_when_same_address_identity_is_unavailable() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.address = Some("0xsame".to_string());
        target.pid = Some(1000);
        target.process_start_time = Some(10);

        let current =
            make_reconcile_window("0xsame", "kitty", "kitty", 7, 0, [900, 100], [700, 500]);
        let mock = MockHyprctl::new(vec![vec![current]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher_options(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
            true,
        )
        .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.launched, 0);
        assert!(launcher.launches.borrow().is_empty());
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_safe_recovery_does_not_launch_same_class_window_with_stale_address() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xbefore-reopen".to_string());

        let mut reopened = make_reconcile_window(
            "0xafter-reopen",
            "kitty",
            "Project",
            7,
            0,
            [900, 100],
            [700, 500],
        );
        reopened.initial_title = "Project".to_string();
        let mock = MockHyprctl::new(vec![vec![reopened]]);

        let report = recover_session_safely(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.launched, 0);
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_safe_recovery_checks_reopened_window_before_launching_duplicate() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xbefore-reopen".to_string());

        let mut reopened = make_reconcile_window(
            "0xafter-reopen",
            "kitty",
            "Project",
            7,
            0,
            [900, 100],
            [700, 500],
        );
        reopened.initial_title = "Project".to_string();
        let mock = MockHyprctl::new(vec![vec![], vec![reopened]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher_options(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
            false,
        )
        .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.launched, 0);
        assert!(launcher.launches.borrow().is_empty());
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_replacement_completion_rejects_safety_snapshot_without_addresses() {
        let mut target = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Project".to_string();
        target.initial_title = "Project".to_string();
        target.address = Some("0xnew-target".to_string());

        let mut legacy_backup = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        legacy_backup.title = "Project".to_string();
        legacy_backup.initial_title = "Project".to_string();
        legacy_backup.address = None;

        let current =
            make_reconcile_window("0xcurrent", "kitty", "Project", 2, 0, [10, 20], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![current]]);

        assert!(!replacement_target_is_complete_with_backup(
            &make_session(vec![target]),
            Some(&make_session(vec![legacy_backup])),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    #[test]
    fn test_empty_replacement_target_is_complete_when_desktop_is_empty() {
        let mock = MockHyprctl::new(vec![vec![]]);

        assert!(replacement_target_is_complete_with_backup(
            &make_session(vec![]),
            None,
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
        )
        .unwrap());
    }

    // ── MockHyprctl ───────────────────────────────────────────────────────────

    /// A mock that returns pre-programmed client snapshots on successive
    /// `get_clients()` calls, simulating a new window appearing.
    struct MockHyprctl {
        /// One entry per `get_clients()` call; last entry is repeated if exhausted.
        client_states: RefCell<Vec<Vec<HyprClient>>>,
        state_index: RefCell<usize>,
        active_addresses: RefCell<Vec<Option<String>>>,
        active_index: RefCell<usize>,
        dispatches: RefCell<Vec<String>>,
    }

    impl MockHyprctl {
        fn new(client_states: Vec<Vec<HyprClient>>) -> Self {
            Self {
                client_states: RefCell::new(client_states),
                state_index: RefCell::new(0),
                active_addresses: RefCell::new(Vec::new()),
                active_index: RefCell::new(0),
                dispatches: RefCell::new(Vec::new()),
            }
        }

        fn with_active_addresses(self, active_addresses: Vec<Option<String>>) -> Self {
            *self.active_addresses.borrow_mut() = active_addresses;
            self
        }

        fn dispatches(&self) -> Vec<String> {
            self.dispatches.borrow().clone()
        }
    }

    impl HyprctlClient for MockHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            let idx = *self.state_index.borrow();
            let states = self.client_states.borrow();
            // Clamp to last state once exhausted.
            let effective = idx.min(states.len().saturating_sub(1));
            let result = states.get(effective).cloned().unwrap_or_default();
            drop(states);
            *self.state_index.borrow_mut() = idx + 1;
            Ok(result)
        }

        fn get_active_window_address(&self) -> Result<Option<String>, HyprctlError> {
            let idx = *self.active_index.borrow();
            let addresses = self.active_addresses.borrow();
            let effective = idx.min(addresses.len().saturating_sub(1));
            let result = addresses.get(effective).cloned().unwrap_or(None);
            drop(addresses);
            *self.active_index.borrow_mut() = idx + 1;
            Ok(result)
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![HyprMonitor {
                id: 0,
                name: "DP-1".to_string(),
                width: 1920,
                height: 1080,
                transform: 0,
                x: Some(0),
                y: Some(0),
            }])
        }

        fn dispatch(&self, args: &str) -> Result<(), HyprctlError> {
            self.dispatches.borrow_mut().push(args.to_string());
            Ok(())
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    struct MonitorErrorHyprctl;

    impl HyprctlClient for MonitorErrorHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            Ok(vec![])
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Err(HyprctlError::CommandFailed(
                "monitor query unavailable".to_string(),
            ))
        }

        fn dispatch(&self, _args: &str) -> Result<(), HyprctlError> {
            Ok(())
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    struct ClientErrorHyprctl;

    impl HyprctlClient for ClientErrorHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            Err(HyprctlError::CommandFailed(
                "client query unavailable".to_string(),
            ))
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![])
        }

        fn dispatch(&self, _args: &str) -> Result<(), HyprctlError> {
            Ok(())
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    struct ClosingMockHyprctl {
        clients: RefCell<Vec<HyprClient>>,
        dispatches: RefCell<Vec<String>>,
    }

    impl ClosingMockHyprctl {
        fn new(clients: Vec<HyprClient>) -> Self {
            Self {
                clients: RefCell::new(clients),
                dispatches: RefCell::new(Vec::new()),
            }
        }
    }

    impl HyprctlClient for ClosingMockHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            Ok(self.clients.borrow().clone())
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![])
        }

        fn dispatch(&self, args: &str) -> Result<(), HyprctlError> {
            self.dispatches.borrow_mut().push(args.to_string());
            if args.starts_with("closewindow ") {
                self.clients.borrow_mut().clear();
            }
            Ok(())
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    struct SelectiveClosingMockHyprctl {
        clients: RefCell<Vec<HyprClient>>,
        dispatches: RefCell<Vec<String>>,
    }

    impl SelectiveClosingMockHyprctl {
        fn new(clients: Vec<HyprClient>) -> Self {
            Self {
                clients: RefCell::new(clients),
                dispatches: RefCell::new(Vec::new()),
            }
        }

        fn dispatches(&self) -> Vec<String> {
            self.dispatches.borrow().clone()
        }
    }

    impl HyprctlClient for SelectiveClosingMockHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            Ok(self.clients.borrow().clone())
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![])
        }

        fn dispatch(&self, args: &str) -> Result<(), HyprctlError> {
            self.dispatches.borrow_mut().push(args.to_string());
            if let Some(address) = args.strip_prefix("closewindow address:") {
                self.clients
                    .borrow_mut()
                    .retain(|client| client.address != address);
            }
            Ok(())
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    struct FailingCloseHyprctl {
        client: HyprClient,
    }

    impl HyprctlClient for FailingCloseHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            Ok(vec![self.client.clone()])
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![])
        }

        fn dispatch(&self, _args: &str) -> Result<(), HyprctlError> {
            Err(HyprctlError::CommandFailed(
                "close dispatch failed".to_string(),
            ))
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    // ── Test: dry-run generates commands without dispatching ─────────────────

    #[test]
    fn test_restore_dry_run_generates_commands() {
        let client = make_client(
            "kitty",
            1,
            [100, 200],
            [800, 600],
            false,
            0,
            "true",
            vec!["--directory".to_string(), "/home/user".to_string()],
            Some("claude --continue".to_string()),
        );
        let session = make_session(vec![client]);
        let config = Config {
            apps: HashMap::from([(
                "kitty".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: None,
                    default_workspace: None,
                },
            )]),
            ..Config::default()
        };
        // The mock will never be called for dispatches in dry-run mode.
        let mock = MockHyprctl::new(vec![]);

        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        // Dry-run should count the client as "restored" and emit detail lines.
        assert_eq!(report.restored, 1);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.failed, 0);
        // At minimum: one header line + one or more dispatch lines.
        assert!(
            !report.details.is_empty(),
            "dry-run should produce detail lines"
        );
        // Header line must mention the client class.
        let header = &report.details[0];
        assert!(
            header.contains("kitty"),
            "header should contain class name; got: {header}"
        );
        assert!(
            header.contains("[dry-run]"),
            "header should be tagged [dry-run]; got: {header}"
        );
        // No real dispatches should have been recorded.
        assert!(
            mock.dispatches().is_empty(),
            "dry-run must not send real hyprctl dispatches"
        );
    }

    #[test]
    fn test_restore_propagates_initial_client_query_failures() {
        let client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );

        let result = restore_session(
            &make_session(vec![client]),
            &ClientErrorHyprctl,
            &Config::default(),
            false,
            false,
        );

        assert!(matches!(
            result,
            Err(RestoreError::Hyprctl(HyprctlError::CommandFailed(message)))
                if message == "client query unavailable"
        ));
    }

    #[test]
    fn test_normal_restore_matches_terminal_windows_by_working_directory() {
        let mut first = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/one".to_string()],
            None,
        );
        first.title = "terminal".to_string();
        let mut second = make_client(
            "kitty",
            2,
            [900, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/two".to_string()],
            None,
        );
        second.title = "terminal".to_string();

        let current_one =
            make_reconcile_window("0xone", "kitty", "terminal", 1, 0, [10, 20], [800, 600]);
        let mut current_two =
            make_reconcile_window("0xtwo", "kitty", "terminal", 2, 0, [900, 20], [800, 600]);
        current_two.pid = 102;
        let mut current_one = current_one;
        current_one.pid = 101;
        let mock = MockHyprctl::new(vec![vec![current_one, current_two]]);
        let process_info = CwdProcessInfo {
            cwds: HashMap::from([(101, PathBuf::from("/two")), (102, PathBuf::from("/one"))]),
        };

        let report = restore_session_with_process_info(
            &make_session(vec![first, second]),
            &mock,
            &process_info,
            &Config::default(),
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.restored, 2);
        assert_eq!(report.skipped, 0);
        let dispatches = mock.dispatches();
        assert!(dispatches
            .iter()
            .any(|dispatch| dispatch == "movetoworkspacesilent 1,address:0xtwo"));
        assert!(dispatches
            .iter()
            .any(|dispatch| dispatch == "movetoworkspacesilent 2,address:0xone"));
    }

    #[test]
    fn test_reconcile_dry_run_reports_missing_launch_binary_as_failure() {
        let client = make_client(
            "missing-app",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "missing_binary_for_preflight_xyz",
            vec![],
            None,
        );
        let mock = MockHyprctl::new(vec![vec![]]);

        let report = reconcile_session_with_launcher(
            &make_session(vec![client]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            true,
            true,
            &RecordingLauncher::default(),
        )
        .unwrap();

        assert_eq!(report.launched, 0);
        assert_eq!(report.failed, 1);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("missing_binary_for_preflight_xyz")));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_rejects_unconfigured_launch_command() {
        let client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        let mock = MockHyprctl::new(vec![vec![]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![client]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.failed, 1);
        assert_eq!(report.launched, 0);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("not authorized")));
        assert!(launcher.launches.borrow().is_empty());
    }

    // ── Test: build_launch_command for kitty with hint ───────────────────────

    #[test]
    fn test_build_launch_command_kitty_with_hint() {
        let client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "kitty",
            vec!["--directory".to_string(), "/home/user/project".to_string()],
            Some("claude --continue".to_string()),
        );

        let cmd = build_launch_command(&client);

        // argv[0] is the binary.
        assert_eq!(cmd[0], "kitty");
        // Existing args are preserved before the hint block.
        assert!(
            cmd.contains(&"--directory".to_string()),
            "should keep --directory arg"
        );
        assert!(
            cmd.contains(&"/home/user/project".to_string()),
            "should keep directory value"
        );
        // The hint block must be present.
        let joined = cmd.join(" ");
        assert!(
            joined.contains("-e zsh -c"),
            "kitty hint should inject '-e zsh -c'; got: {joined}"
        );
        assert!(
            joined.contains("claude --continue"),
            "hint content should appear in command; got: {joined}"
        );
        assert!(
            joined.contains("exec zsh"),
            "hint block should drop to interactive zsh; got: {joined}"
        );
    }

    // ── Test: build_launch_command for a generic binary ─────────────────────

    #[test]
    fn test_build_launch_command_generic() {
        let client = make_client(
            "brave-browser",
            1,
            [0, 0],
            [1280, 800],
            false,
            0,
            "brave-browser",
            vec!["--profile-directory=Default".to_string()],
            None,
        );

        let cmd = build_launch_command(&client);

        assert_eq!(cmd[0], "brave-browser");
        assert_eq!(cmd[1], "--profile-directory=Default");
        assert_eq!(
            cmd.len(),
            2,
            "no extra args should be appended for non-kitty"
        );
    }

    // ── Test: build_dispatch_commands produces correct sequence ──────────────

    #[test]
    fn test_build_dispatch_commands() {
        let client = make_client(
            "obsidian",
            3,
            [50, 100],
            [1200, 900],
            true, // floating
            0,
            "obsidian",
            vec![],
            None,
        );

        let cmds = build_dispatch_commands(&client);

        // Must start with exec.
        assert!(cmds[0].starts_with("exec "), "first command must be exec");
        // Workspace move must come before resize/move.
        let ws_idx = cmds
            .iter()
            .position(|c| c.starts_with("movetoworkspacesilent"))
            .unwrap();
        let resize_idx = cmds
            .iter()
            .position(|c| c.starts_with("resizewindowpixel"))
            .unwrap();
        let move_idx = cmds
            .iter()
            .position(|c| c.starts_with("movewindowpixel"))
            .unwrap();
        assert!(ws_idx < resize_idx, "workspace move must precede resize");
        assert!(resize_idx < move_idx, "resize must precede position move");

        // Workspace number must appear in the movetoworkspacesilent command.
        assert!(
            cmds[ws_idx].contains("3"),
            "workspace 3 must appear in dispatch; got: {}",
            cmds[ws_idx]
        );
        // Floating togglefloating must be present.
        let float_cmd = cmds.iter().find(|c| c.starts_with("togglefloating"));
        assert!(
            float_cmd.is_some(),
            "floating client should have togglefloating dispatch"
        );

        // fullscreen=0 means no fullscreen dispatch.
        assert!(
            !cmds.iter().any(|c| c.starts_with("fullscreen")),
            "non-fullscreen client should not have fullscreen dispatch"
        );
        assert!(
            cmds.iter().any(|c| c == "focuswindow address:0xNEW"),
            "dry-run monitor placement should target the placeholder address once"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("address:address:")),
            "dry-run monitor placement must not double-prefix the address"
        );
    }

    #[test]
    fn test_new_fullscreen_window_is_targeted_by_address() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            1,
            "true",
            vec![],
            None,
        );
        let new_window = make_reconcile_window(
            "0xnew-fullscreen",
            "kitty",
            "kitty",
            1,
            0,
            [0, 0],
            [400, 300],
        );
        let mock = MockHyprctl::new(vec![vec![], vec![new_window]]);
        let launcher = RecordingLauncher::default();
        let mut config = Config::default();
        config.general.restore_delay_ms = 0;

        restore_single_client_with_launcher_and_process_info(
            &target,
            &mock,
            &EmptyProcessInfo,
            &config,
            &launcher,
            false,
        )
        .unwrap();

        let dispatches = mock.dispatches();
        assert!(dispatches
            .iter()
            .any(|dispatch| dispatch == "focuswindow address:0xnew-fullscreen"));
        assert!(dispatches
            .iter()
            .any(|dispatch| dispatch == "fullscreenstate 1 1"));
        assert!(!mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch == "fullscreen 1"));
    }

    #[test]
    fn test_launch_accepts_reused_address_when_it_belongs_to_launched_process() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.monitor.clear();

        let mut old_window =
            make_reconcile_window("0xreused", "kitty", "old", 1, 0, [0, 0], [400, 300]);
        old_window.pid = 4000;
        let mut launched_window =
            make_reconcile_window("0xreused", "kitty", "kitty", 1, 0, [0, 0], [400, 300]);
        launched_window.pid = 5000;
        let mock = MockHyprctl::new(vec![vec![old_window], vec![launched_window]]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            start_time: Some(10),
            ..Default::default()
        };
        let process_info = StartTimeProcessInfo { start_time: 10 };
        let mut config = Config::default();
        config.general.restore_delay_ms = 0;

        restore_single_client_with_launcher_and_process_info(
            &target,
            &mock,
            &process_info,
            &config,
            &launcher,
            false,
        )
        .expect("the launched window may reuse the old address");

        assert!(mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch.contains("0xreused")));
    }

    #[test]
    fn test_launch_correlation_prefers_window_from_spawned_process_tree() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.title = "target".to_string();
        target.monitor.clear();
        let related =
            make_reconcile_window("0xrelated", "kitty", "other", 1, 0, [0, 0], [400, 300]);
        let mut related = related;
        related.pid = 6000;
        let unrelated =
            make_reconcile_window("0xunrelated", "kitty", "target", 1, 0, [10, 20], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![], vec![related, unrelated]]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let process_info = RelatedProcessInfo {
            children: HashMap::from([(
                5000,
                vec![ChildProcess {
                    pid: 6000,
                    cwd: PathBuf::from("/tmp"),
                    cmdline: "kitty".to_string(),
                }],
            )]),
        };
        let mut config = Config::default();
        config.general.restore_delay_ms = 0;

        restore_single_client_with_launcher_and_process_info(
            &target,
            &mock,
            &process_info,
            &config,
            &launcher,
            false,
        )
        .unwrap();

        assert!(mock
            .dispatches()
            .iter()
            .all(|dispatch| !dispatch.contains("0xunrelated")));
        assert!(mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch.contains("0xrelated")));
    }

    #[test]
    fn test_launch_correlation_does_not_move_unrelated_same_class_window() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.title = "target".to_string();
        target.monitor.clear();
        let unrelated =
            make_reconcile_window("0xunrelated", "kitty", "target", 1, 0, [10, 20], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![], vec![unrelated]]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info(
            &target,
            &mock,
            &EmptyProcessInfo,
            &config,
            &launcher,
            false,
        );

        assert!(matches!(result, Err(RestoreError::Hyprctl(_))));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_launch_correlation_fails_instead_of_picking_ambiguous_window() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        let first =
            make_reconcile_window("0xambiguous-a", "kitty", "kitty", 1, 0, [0, 0], [400, 300]);
        let second =
            make_reconcile_window("0xambiguous-b", "kitty", "kitty", 1, 0, [0, 0], [400, 300]);
        let mock = MockHyprctl::new(vec![vec![], vec![first, second]]);
        let launcher = RecordingLauncher::default();
        let mut config = Config::default();
        config.general.restore_delay_ms = 0;

        let error = restore_single_client_with_launcher(&target, &mock, &config, &launcher)
            .expect_err("ambiguous candidates must not be targeted arbitrarily");
        assert!(error.to_string().contains("unambiguously identify"));
        assert!(mock.dispatches().is_empty());
    }

    // ── Test: reports client when binary is missing ──────────────────────────

    #[test]
    fn test_restore_reports_missing_binary_as_failure() {
        let client = make_client(
            "nonexistent_app_xyz",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "nonexistent_app_xyz_abc_123", // guaranteed not to exist
            vec![],
            None,
        );
        let session = make_session(vec![client]);
        let mut config = Config::default();
        config.apps.insert(
            "nonexistent_app_xyz".to_string(),
            AppConfig {
                binary: Some("nonexistent_app_xyz_abc_123".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );
        let mock = MockHyprctl::new(vec![]);

        let report = restore_session(&session, &mock, &config, false, true).unwrap();

        assert_eq!(report.skipped, 0);
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 1, "missing binary must be actionable");
        // No dispatches should have been sent.
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_restore_rejects_more_than_the_operational_window_limit() {
        let clients: Vec<SessionClient> = (0..=MAX_RECONCILIATION_WINDOWS)
            .map(|index| {
                make_client(
                    "example-app",
                    1,
                    [index as i32, 0],
                    [800, 600],
                    false,
                    0,
                    "true",
                    vec![],
                    None,
                )
            })
            .collect();
        let mock = MockHyprctl::new(vec![]);

        let error = restore_session(
            &make_session(clients),
            &mock,
            &Config::default(),
            false,
            true,
        )
        .expect_err("normal restore must be bounded");

        assert!(matches!(
            error,
            RestoreError::TooManyWindows {
                targets,
                current: 0,
                limit,
            } if targets == MAX_RECONCILIATION_WINDOWS + 1
                && limit == MAX_RECONCILIATION_WINDOWS
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_restore_dry_run_reports_missing_launch_binary_as_failure() {
        let client = make_client(
            "missing-app",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "missing_binary_for_dry_run_xyz",
            vec![],
            None,
        );
        let mut config = Config::default();
        config.apps.insert(
            "missing-app".to_string(),
            AppConfig {
                binary: Some("missing_binary_for_dry_run_xyz".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let report = restore_session(
            &make_session(vec![client]),
            &MockHyprctl::new(vec![]),
            &config,
            true,
            true,
        )
        .unwrap();

        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 1);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("missing_binary_for_dry_run_xyz")));
    }

    #[test]
    fn test_restore_dry_run_reports_untrusted_launch_command_as_failure() {
        let client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );

        let report = restore_session(
            &make_session(vec![client]),
            &MockHyprctl::new(vec![]),
            &Config::default(),
            true,
            true,
        )
        .unwrap();

        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 1);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("not authorized")));
    }

    // ── Test: skips duplicate class+workspace already running ────────────

    #[test]
    fn test_restore_skips_duplicate_class_workspace() {
        let existing_window = HyprClient {
            address: "0xexisting".to_string(),
            class: "KITTY".to_string(),
            title: "kitty".to_string(),
            initial_class: "TerminalWrapper".to_string(),
            initial_title: "kitty".to_string(),
            workspace: crate::hyprctl::HyprWorkspace {
                id: 1,
                name: "1".to_string(),
            },
            monitor: 0,
            at: [0, 0],
            size: [800, 600],
            floating: false,
            fullscreen: 0,
            pinned: false,
            stable_id: None,
            focus_history_id: 0,
            pid: 9999,
        };

        // First get_clients() call returns the existing window (duplicate check).
        // Subsequent calls would also return it (mock clamps to last state).
        let mock = MockHyprctl::new(vec![vec![existing_window]]);

        let client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let session = make_session(vec![client]);
        let config = Config::default();

        let report = restore_session(&session, &mock, &config, false, true).unwrap();

        assert_eq!(report.skipped, 1, "duplicate should be skipped");
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 0);
        assert!(
            report
                .details
                .iter()
                .any(|d| d.contains("SKIP: KITTY already on ws=1")),
            "details should mention the skipped duplicate; got: {:?}",
            report.details
        );
        // No dispatches should have been sent.
        assert!(mock.dispatches().is_empty());
    }

    // ── Test: dry-run accounts for already-open windows ─────────────────

    #[test]
    fn test_restore_dry_run_accounts_for_duplicates() {
        let existing_window = HyprClient {
            address: "0xexisting".to_string(),
            class: "kitty".to_string(),
            title: "kitty".to_string(),
            initial_class: "kitty".to_string(),
            initial_title: "kitty".to_string(),
            workspace: crate::hyprctl::HyprWorkspace {
                id: 1,
                name: "1".to_string(),
            },
            monitor: 0,
            at: [0, 0],
            size: [800, 600],
            floating: false,
            fullscreen: 0,
            pinned: false,
            stable_id: None,
            focus_history_id: 0,
            pid: 9999,
        };

        // The dry-run should report the existing window instead of planning a
        // duplicate launch.
        let mock = MockHyprctl::new(vec![vec![existing_window]]);

        let client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let session = make_session(vec![client]);
        let config = Config::default();

        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        assert_eq!(report.restored, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.failed, 0);
        // No real dispatches in dry-run.
        assert!(mock.dispatches().is_empty());
    }

    // ── Test: partial duplicates — restore only the missing count ────────

    #[test]
    fn test_restore_partial_duplicate_restores_missing() {
        // 2 existing "testapp" windows on ws=5.
        let existing = vec![
            HyprClient {
                address: "0xaaa".to_string(),
                class: "testapp".to_string(),
                title: "testapp".to_string(),
                initial_class: "testapp".to_string(),
                initial_title: "testapp".to_string(),
                workspace: crate::hyprctl::HyprWorkspace {
                    id: 5,
                    name: "5".to_string(),
                },
                monitor: 0,
                at: [0, 0],
                size: [800, 600],
                floating: false,
                fullscreen: 0,
                pinned: false,
                stable_id: None,
                focus_history_id: 0,
                pid: 1001,
            },
            HyprClient {
                address: "0xbbb".to_string(),
                class: "testapp".to_string(),
                title: "testapp".to_string(),
                initial_class: "testapp".to_string(),
                initial_title: "testapp".to_string(),
                workspace: crate::hyprctl::HyprWorkspace {
                    id: 5,
                    name: "5".to_string(),
                },
                monitor: 0,
                at: [100, 0],
                size: [800, 600],
                floating: false,
                fullscreen: 0,
                pinned: false,
                stable_id: None,
                focus_history_id: 0,
                pid: 1002,
            },
        ];

        let mock = MockHyprctl::new(vec![existing]);

        // Session wants 3 "testapp" on ws=5 with a nonexistent binary.
        let clients: Vec<SessionClient> = (0..3)
            .map(|i| {
                make_client(
                    "testapp",
                    5,
                    [i * 100, 0],
                    [800, 600],
                    false,
                    0,
                    "nonexistent_binary_xyz_123",
                    vec![],
                    None,
                )
            })
            .collect();

        let session = make_session(clients);
        let mut config = Config::default();
        config.apps.insert(
            "testapp".to_string(),
            AppConfig {
                binary: Some("nonexistent_binary_xyz_123".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let report = restore_session(&session, &mock, &config, false, true).unwrap();

        // 2 skipped as duplicates, 1 failed as binary-not-found.
        assert_eq!(
            report.skipped, 2,
            "expected 2 skipped; got {}",
            report.skipped
        );
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 1);

        // Exactly 2 detail lines should mention "already on ws=".
        let dup_msgs: Vec<_> = report
            .details
            .iter()
            .filter(|d| d.contains("SKIP: testapp already on ws=5"))
            .collect();
        assert_eq!(
            dup_msgs.len(),
            2,
            "expected 2 duplicate-skip messages; got {:?}",
            report.details
        );

        // Exactly 1 detail line should mention "binary".
        let bin_msgs: Vec<_> = report
            .details
            .iter()
            .filter(|d| d.contains("binary"))
            .collect();
        assert_eq!(
            bin_msgs.len(),
            1,
            "expected 1 binary-not-found message; got {:?}",
            report.details
        );

        // No dispatches should have been sent.
        assert!(mock.dispatches().is_empty());
    }

    // ── Test: restore brave by profile in dry-run mode ───────────────────

    #[test]
    fn test_restore_brave_by_profile_dry_run() {
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![
                make_client(
                    "brave-browser",
                    1,
                    [0, 0],
                    [800, 600],
                    false,
                    0,
                    "brave",
                    vec![],
                    None,
                ),
                make_client(
                    "brave-browser",
                    8,
                    [0, 0],
                    [800, 600],
                    false,
                    0,
                    "brave",
                    vec![],
                    None,
                ),
                make_client(
                    "kitty",
                    4,
                    [0, 0],
                    [800, 600],
                    false,
                    0,
                    "true",
                    vec![],
                    None,
                ),
            ],
            brave_profiles: vec![
                BraveProfile {
                    directory: "Default".to_string(),
                    name: "Credifit".to_string(),
                },
                BraveProfile {
                    directory: "Profile 1".to_string(),
                    name: "LinkPJ".to_string(),
                },
            ],
        };

        let mut apps = HashMap::new();
        apps.insert(
            "brave-browser".to_string(),
            AppConfig {
                binary: Some("true".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: Some(HashMap::from([
                    ("Default".to_string(), 1),
                    ("Profile 1".to_string(), 6),
                ])),
                default_workspace: Some(1),
            },
        );
        apps.insert(
            "kitty".to_string(),
            AppConfig {
                binary: Some("true".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig::default(),
            apps,
        };

        let mock = MockHyprctl::new(vec![]);
        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        // 2 profiles restored + 1 kitty = 3 restored
        assert_eq!(
            report.restored, 3,
            "should restore 2 profiles + 1 kitty; got details: {:?}",
            report.details
        );

        // Brave individual windows should NOT appear in details (they were skipped)
        let brave_individual: Vec<_> = report
            .details
            .iter()
            .filter(|d| d.contains("[dry-run] ws=") && d.contains("brave"))
            .collect();
        assert!(
            brave_individual.is_empty(),
            "individual brave windows should be skipped; got: {:?}",
            brave_individual
        );

        // Profile entries should appear
        assert!(
            report.details.iter().any(|d| d.contains("Credifit")),
            "should have Credifit profile; got: {:?}",
            report.details
        );
        assert!(
            report.details.iter().any(|d| d.contains("LinkPJ")),
            "should have LinkPJ profile; got: {:?}",
            report.details
        );

        // Kitty should still be present
        assert!(
            report.details.iter().any(|d| d.contains("kitty")),
            "kitty should be restored normally; got: {:?}",
            report.details
        );
    }

    // ── Test: profile without an explicit map uses default_workspace fallback

    #[test]
    fn test_restore_brave_profile_uses_default_workspace() {
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![],
            brave_profiles: vec![BraveProfile {
                directory: "Profile 9".to_string(),
                name: "Unmapped".to_string(),
            }],
        };

        let mut apps = HashMap::new();
        apps.insert(
            "brave-browser".to_string(),
            AppConfig {
                binary: Some("true".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: Some(3),
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps,
        };

        let mock = MockHyprctl::new(vec![]);
        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        assert_eq!(report.restored, 1);
        // With no explicit map, use default_workspace=3.
        assert!(
            report.details.iter().any(|d| d.contains("ws=3")),
            "profile should use default_workspace=3; got: {:?}",
            report.details
        );
    }

    #[test]
    fn test_restore_brave_profile_missing_binary_is_a_failure() {
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![],
            brave_profiles: vec![BraveProfile {
                directory: "Default".to_string(),
                name: "Personal".to_string(),
            }],
        };
        let config = Config {
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("missing_brave_binary_xyz".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: Some(HashMap::from([("Default".to_string(), 1)])),
                    default_workspace: None,
                },
            )]),
            ..Config::default()
        };

        let report = restore_session(
            &session,
            &MockHyprctl::new(vec![vec![]]),
            &config,
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.restored, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.failed, 1);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("missing_brave_binary_xyz")));
    }

    #[test]
    fn test_restore_brave_profile_with_explicit_mapping_skips_unmapped_profile() {
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![],
            brave_profiles: vec![
                BraveProfile {
                    directory: "Default".to_string(),
                    name: "Mapped".to_string(),
                },
                BraveProfile {
                    directory: "Profile 9".to_string(),
                    name: "Unmapped".to_string(),
                },
            ],
        };

        let mut apps = HashMap::new();
        apps.insert(
            "brave-browser".to_string(),
            AppConfig {
                binary: Some("true".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: Some(HashMap::from([("Default".to_string(), 1)])),
                default_workspace: Some(3),
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps,
        };

        let mock = MockHyprctl::new(vec![]);
        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        assert_eq!(report.restored, 1);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("Mapped")));
        assert!(!report
            .details
            .iter()
            .any(|detail| detail.contains("Unmapped")));
    }

    #[test]
    fn test_empty_brave_profile_mapping_does_not_fall_back_to_raw_windows() {
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![make_client(
                "brave-browser",
                1,
                [0, 0],
                [800, 600],
                false,
                0,
                "true",
                vec![],
                None,
            )],
            // A legacy session may not have captured the profile inventory,
            // but an explicit empty map still means "restore no profiles".
            brave_profiles: vec![],
        };
        let config = Config {
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: Some(HashMap::new()),
                    default_workspace: Some(1),
                },
            )]),
            ..Config::default()
        };

        let report = restore_session(&session, &MockHyprctl::new(vec![]), &config, true, true)
            .expect("empty profile map should be a valid no-op");

        assert_eq!(report.restored, 0);
        assert!(report.details.is_empty());
    }

    #[test]
    fn test_reconcile_reports_legacy_brave_without_profile_identity() {
        let session = Session {
            name: "legacy-brave".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![make_client(
                "brave-browser",
                1,
                [0, 0],
                [1280, 800],
                false,
                0,
                "brave",
                vec![],
                None,
            )],
            brave_profiles: vec![],
        };
        let config = Config {
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: Some(HashMap::from([("Default".to_string(), 1)])),
                    default_workspace: None,
                },
            )]),
            ..Config::default()
        };

        let launcher = RecordingLauncher::default();
        let report = reconcile_session_with_launcher(
            &session,
            &MockHyprctl::new(vec![vec![]]),
            &EmptyProcessInfo,
            &config,
            false,
            true,
            &launcher,
        )
        .expect("legacy Brave identity uncertainty should be reported, not dropped");

        assert_eq!(report.skipped, 1);
        assert_eq!(report.launched, 0);
        assert!(launcher.launches.borrow().is_empty());
    }

    #[test]
    fn test_brave_mapping_does_not_create_profiles_absent_from_snapshot() {
        let session = Session {
            name: "without-brave".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![],
            brave_profiles: vec![],
        };
        let config = Config {
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: Some(HashMap::from([("Default".to_string(), 2)])),
                    default_workspace: Some(1),
                },
            )]),
            ..Config::default()
        };

        let report = restore_session(&session, &MockHyprctl::new(vec![]), &config, true, true)
            .expect("a snapshot without Brave should remain a no-op");

        assert_eq!(report.restored, 0);
        assert!(report.details.is_empty());
    }

    #[test]
    fn test_legacy_brave_inventory_without_identity_is_skipped_safely() {
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![make_client(
                "brave-browser",
                1,
                [0, 0],
                [800, 600],
                false,
                0,
                "true",
                vec![],
                None,
            )],
            // Older snapshots could contain every Local State profile even
            // though only one Brave window was captured.
            brave_profiles: vec![
                BraveProfile {
                    directory: "Default".to_string(),
                    name: "Personal".to_string(),
                },
                BraveProfile {
                    directory: "Profile 1".to_string(),
                    name: "Work".to_string(),
                },
            ],
        };
        let config = Config {
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: None,
                    default_workspace: Some(1),
                },
            )]),
            ..Config::default()
        };

        let report = restore_session(&session, &MockHyprctl::new(vec![]), &config, true, true)
            .expect("ambiguous legacy inventory should be skipped safely");

        assert_eq!(report.restored, 0);
        assert!(report
            .details
            .iter()
            .all(|detail| !detail.contains("brave profile")));
    }

    #[test]
    fn test_legacy_brave_without_profile_identity_is_not_matched_or_launched() {
        let target = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        let current = make_reconcile_window(
            "0xprofile",
            "brave-browser",
            "Brave",
            1,
            0,
            [100, 100],
            [800, 600],
        );
        let observed = ObservedClient::with_profile_directory(
            current.clone(),
            None,
            None,
            Some("Profile 1".to_string()),
        );
        assert!(plan_reconciliation(std::slice::from_ref(&target), &[observed])[0].is_none());

        let mock = MockHyprctl::new(vec![vec![current]]);
        let launcher = RecordingLauncher::default();
        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &BraveProfileProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.launched, 0);
        assert!(launcher.launches.borrow().is_empty());
        assert!(mock.dispatches().is_empty());
    }

    // ── Test: without profiles, brave windows restore individually ───────

    #[test]
    fn test_restore_brave_without_profiles_falls_back() {
        // Session WITHOUT brave_profiles can still restore a Brave window when
        // the window itself carries a positive Default-profile identity.
        let mut client = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        client.profile_directory = Some("Default".to_string());
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![client],
            brave_profiles: vec![], // no profiles
        };

        let config = Config {
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: None,
                    default_workspace: None,
                },
            )]),
            ..Config::default()
        };
        let mock = MockHyprctl::new(vec![]);
        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        // Without an inventory, a positively identified Brave window is still
        // restored individually.
        assert_eq!(report.restored, 1);
        assert!(report
            .details
            .iter()
            .any(|d| d.contains("[dry-run]") && d.contains("brave")));
    }

    #[test]
    fn test_normal_restore_reuses_open_brave_profile_instead_of_launching_duplicate() {
        let mut brave = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        brave.title = "Brave".to_string();
        brave.initial_title = "Brave".to_string();
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![brave],
            brave_profiles: vec![BraveProfile {
                directory: "Default".to_string(),
                name: "Personal".to_string(),
            }],
        };
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig::default(),
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("brave".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: Some(HashMap::from([("Default".to_string(), 1)])),
                    default_workspace: Some(1),
                },
            )]),
        };
        let mock = MockHyprctl::new(vec![vec![make_reconcile_window(
            "0xprofile",
            "brave-browser",
            "Brave",
            1,
            0,
            [0, 0],
            [800, 600],
        )]]);

        let report = restore_session_with_process_info(
            &session,
            &mock,
            &BraveProfileProcessInfo,
            &config,
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 0);
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_brave_shared_process_is_not_assumed_to_identify_each_window() {
        let target = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            monitors: vec![],
            hyprland_version: "0.54.1".to_string(),
            clients: vec![target],
            brave_profiles: vec![BraveProfile {
                directory: "Default".to_string(),
                name: "Personal".to_string(),
            }],
        };
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig::default(),
            apps: HashMap::from([(
                "brave-browser".to_string(),
                AppConfig {
                    binary: Some("brave".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: None,
                    default_workspace: Some(1),
                },
            )]),
        };
        let first = make_reconcile_window(
            "0xprofile-a",
            "brave-browser",
            "Personal",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let second = make_reconcile_window(
            "0xprofile-b",
            "brave-browser",
            "Work",
            2,
            0,
            [900, 0],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![first, second]]);

        let report = restore_session_with_process_info(
            &session,
            &mock,
            &BraveProfileProcessInfo,
            &config,
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 0);
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_leaves_conflicting_same_class_extra_unmatched() {
        let mut target = make_client(
            "example-app",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.title = "Project A".to_string();
        target.initial_title = "Project A".to_string();
        let current = make_reconcile_window(
            "0xextra",
            "example-app",
            "Project B",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![current]]);
        let config = Config {
            apps: HashMap::from([(
                "example-app".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: None,
                    default_workspace: None,
                },
            )]),
            ..Config::default()
        };

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &config,
            true,
            true,
            &RecordingLauncher::default(),
        )
        .unwrap();

        assert_eq!(report.matched, 0);
        assert_eq!(report.launched, 1);
        assert_eq!(report.extras, 1);
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_skips_a_saved_brave_target_without_safe_profile_identity() {
        let mut target = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        target.profile_identity_ambiguous = true;
        let current = make_reconcile_window(
            "0xbrave",
            "brave-browser",
            "Brave",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![current]]);

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &RecordingLauncher::default(),
        )
        .unwrap();

        assert_eq!(report.matched, 0);
        assert_eq!(report.launched, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.extras, 1);
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_safety_snapshot_rejects_ambiguous_brave_windows_before_replace() {
        let mut first = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        first.title = "Personal".to_string();
        first.profile_identity_ambiguous = true;
        let mut second = first.clone();
        second.title = "Work".to_string();

        let error = validate_safety_snapshot(&make_session(vec![first, second]))
            .expect_err("an ambiguous Brave safety snapshot must block replacement");

        assert!(matches!(error, RestoreError::UnsafeRecoverySnapshot { .. }));
    }

    #[test]
    fn test_safety_snapshot_rejects_unlaunchable_window_before_replace() {
        let target = make_client(
            "deskloom-test-command-that-does-not-exist",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "deskloom-test-command-that-does-not-exist",
            vec![],
            None,
        );

        let error =
            validate_safety_snapshot_with_config(&make_session(vec![target]), &Config::default())
                .expect_err("replacement must reject a safety snapshot it cannot relaunch");

        assert!(matches!(error, RestoreError::UnsafeRecoverySnapshot { .. }));
    }

    #[test]
    fn test_safety_snapshot_rejects_missing_stable_id_before_replace() {
        let mut target = make_client(
            "true",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.address = Some("0xtrue".to_string());

        let error =
            validate_safety_snapshot_with_config(&make_session(vec![target]), &Config::default())
                .expect_err("replacement must require a stable Hyprland window ID");

        assert!(matches!(
            error,
            RestoreError::UnsafeRecoverySnapshot { reason }
                if reason.contains("stable window ID")
        ));
    }

    #[test]
    fn test_replacement_rejects_ambiguous_brave_target_even_with_other_targets() {
        let mut brave = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        brave.title = "Work".to_string();
        brave.profile_identity_ambiguous = true;
        let other = make_client(
            "kitty",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );

        let error =
            validate_replacement_targets(&make_session(vec![brave, other]), &Config::default())
                .expect_err("replacement must not silently drop an ambiguous Brave target");

        assert!(matches!(
            error,
            RestoreError::UnsafeReplacementTarget { .. }
        ));
    }

    #[test]
    fn test_brave_launch_rejects_unknown_profile_on_reused_browser_pid() {
        let mut target = make_client(
            "brave-browser",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.profile_directory = Some("Default".to_string());
        let existing = make_reconcile_window(
            "0xexisting",
            "brave-browser",
            "Work",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut new_window = make_reconcile_window(
            "0xnew",
            "brave-browser",
            "Personal",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.pid = existing.pid;
        let mock = MockHyprctl::new(vec![vec![existing.clone()], vec![existing, new_window]]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &SharedBraveProfileProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_brave_launch_rejects_reused_pid_even_when_profile_is_positive() {
        let mut target = make_client(
            "brave-browser",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.profile_directory = Some("Default".to_string());
        let existing = make_reconcile_window(
            "0xexisting",
            "brave-browser",
            "Work",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut new_window = make_reconcile_window(
            "0xnew",
            "brave-browser",
            "Personal",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.pid = existing.pid;
        let mock = MockHyprctl::new(vec![vec![existing.clone()], vec![existing, new_window]]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &BraveProfileProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_brave_launch_does_not_trust_active_window_when_browser_reuses_pid() {
        let mut target = make_client(
            "brave-browser",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.profile_directory = Some("Default".to_string());
        let existing = make_reconcile_window(
            "0xexisting",
            "brave-browser",
            "Work",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut new_window = make_reconcile_window(
            "0xnew",
            "brave-browser",
            "Personal",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.pid = existing.pid;
        let mock = MockHyprctl::new(vec![vec![existing.clone()], vec![existing, new_window]])
            .with_active_addresses(vec![Some("0xnew".to_string())]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &SharedBraveProfileProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_webapp_launch_rejects_window_from_existing_chromium_pid() {
        let target = make_client(
            "chrome-chatgpt.com__-Default",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "omarchy-launch-webapp",
            vec!["https://chatgpt.com/".to_string()],
            None,
        );
        let existing = make_reconcile_window(
            "0xexisting",
            "chrome-other.com__-Default",
            "Other app",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut new_window = make_reconcile_window(
            "0xnew",
            "chrome-chatgpt.com__-Default",
            "ChatGPT",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.pid = existing.pid;
        let mock = MockHyprctl::new(vec![vec![existing.clone()], vec![existing, new_window]]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &EmptyProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_webapp_launch_does_not_trust_active_window_when_browser_reuses_pid() {
        let target = make_client(
            "chrome-chatgpt.com__-Default",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "omarchy-launch-webapp",
            vec!["https://chatgpt.com/".to_string()],
            None,
        );
        let existing = make_reconcile_window(
            "0xexisting",
            "chrome-other.com__-Default",
            "Other app",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut new_window = make_reconcile_window(
            "0xnew",
            "chrome-chatgpt.com__-Default",
            "ChatGPT",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.pid = existing.pid;
        let mock = MockHyprctl::new(vec![vec![existing.clone()], vec![existing, new_window]])
            .with_active_addresses(vec![Some("0xnew".to_string())]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &EmptyProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_webapp_launch_accepts_stable_focused_handoff_window() {
        let target = make_client(
            "chrome-chatgpt.com__-Default",
            2,
            [10, 20],
            [800, 600],
            false,
            0,
            "omarchy-launch-webapp",
            vec!["https://chatgpt.com/".to_string()],
            None,
        );
        let existing = make_reconcile_window(
            "0xexisting",
            "chrome-other.com__-Default",
            "Other app",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut new_window = make_reconcile_window(
            "0xnew-webapp",
            "chrome-chatgpt.com__-Default",
            "ChatGPT",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.stable_id = Some("stable-chatgpt-window".to_string());
        new_window.pid = existing.pid;
        let mock = MockHyprctl::new(vec![
            vec![existing.clone()],
            vec![existing.clone(), new_window.clone()],
        ])
        .with_active_addresses(vec![
            Some("0xexisting".to_string()),
            Some("0xnew-webapp".to_string()),
        ]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &EmptyProcessInfo,
            &config,
            &launcher,
            true,
        )
        .expect("a stable, focused web-app handoff should be correlated");

        assert_eq!(result.observed.client.address, "0xnew-webapp");
        assert!(mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch.contains("0xnew-webapp")));
    }

    #[test]
    fn test_browser_launch_rejects_multiple_new_active_candidates() {
        let mut target = make_client(
            "brave-browser",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.profile_directory = Some("Default".to_string());
        let existing = make_reconcile_window(
            "0xexisting",
            "brave-browser",
            "Existing",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut first_new = make_reconcile_window(
            "0xnew-a",
            "brave-browser",
            "First new window",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut second_new = make_reconcile_window(
            "0xnew-b",
            "brave-browser",
            "Second new window",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        first_new.pid = existing.pid;
        second_new.pid = existing.pid;
        let mock = MockHyprctl::new(vec![
            vec![existing.clone()],
            vec![existing, first_new, second_new],
        ])
        .with_active_addresses(vec![Some("0xnew-b".to_string())]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &SharedBraveProfileProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_brave_launch_rejects_reused_pid_with_multiple_existing_windows() {
        let mut target = make_client(
            "brave-browser",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );
        target.profile_directory = Some("Default".to_string());
        let mut first = make_reconcile_window(
            "0xexisting-a",
            "brave-browser",
            "Work",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        let mut second = make_reconcile_window(
            "0xexisting-b",
            "brave-browser",
            "Personal",
            2,
            0,
            [900, 0],
            [800, 600],
        );
        first.pid = 1000;
        second.pid = 1000;
        let mut new_window = make_reconcile_window(
            "0xnew",
            "brave-browser",
            "Unexpected",
            1,
            0,
            [0, 0],
            [800, 600],
        );
        new_window.pid = 1000;
        let mock = MockHyprctl::new(vec![
            vec![first.clone(), second.clone()],
            vec![first, second, new_window],
        ]);
        let launcher = RecordingLauncher {
            pid: Some(5000),
            ..Default::default()
        };
        let mut config = Config::default();
        config.general.window_detect_timeout_ms = 500;
        config.general.restore_delay_ms = 0;

        let result = restore_single_client_with_launcher_and_process_info_with_address(
            &target,
            &mock,
            &BraveProfileProcessInfo,
            &config,
            &launcher,
            true,
        );

        assert!(matches!(
            result,
            Err(RestoreError::UncorrelatedWindow { .. })
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_does_nothing_when_target_is_already_in_place() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let mut target = target;
        target.title = "Project shell".to_string();
        target.initial_class = "kitty".to_string();
        target.initial_title = "kitty".to_string();

        let current = make_reconcile_window(
            "0xexisting",
            "kitty",
            "Project shell",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![current]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.moved, 0);
        assert_eq!(report.launched, 0);
        assert_eq!(report.extras, 0);
        assert!(mock.dispatches().is_empty());
        assert!(launcher.launches.borrow().is_empty());
    }

    #[test]
    fn test_reconcile_does_not_launch_when_generic_chromium_identity_is_ambiguous() {
        let mut target = make_client(
            "chromium",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "chromium",
            vec![],
            None,
        );
        target.address = Some("0xmissing-target".to_string());
        target.title = "New Tab".to_string();
        target.initial_title = "Chromium".to_string();

        let first = make_reconcile_window(
            "0xexisting-a",
            "chromium",
            "New Tab",
            1,
            0,
            [300, 20],
            [800, 600],
        );
        let second = make_reconcile_window(
            "0xexisting-b",
            "chromium",
            "New Tab",
            1,
            0,
            [900, 20],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![first, second]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.launched, 0);
        assert_eq!(report.skipped, 1);
        assert!(launcher.launches.borrow().is_empty());
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_propagates_monitor_query_failures() {
        let error = reconcile_session_with_launcher(
            &make_session(vec![]),
            &MonitorErrorHyprctl,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &RecordingLauncher::default(),
        )
        .expect_err("monitor query failures must not be hidden");

        assert!(matches!(
            error,
            RestoreError::Hyprctl(HyprctlError::CommandFailed(message))
                if message == "monitor query unavailable"
        ));
    }

    #[test]
    fn test_replace_closes_existing_windows_before_reconciling() {
        let current = make_reconcile_window("0xold", "kitty", "kitty", 1, 0, [0, 0], [800, 600]);
        let mock = ClosingMockHyprctl::new(vec![current]);
        let report = replace_session(
            &make_session(vec![]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.launched, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(
            mock.dispatches.borrow().as_slice(),
            &["closewindow address:0xold"]
        );
    }

    #[test]
    fn test_replace_does_not_close_a_reused_address() {
        let mut old_window =
            make_reconcile_window("0xreused", "kitty", "Old", 1, 0, [0, 0], [800, 600]);
        old_window.stable_id = Some("0xwindow-a".to_string());
        let mut reused_window =
            make_reconcile_window("0xreused", "kitty", "New", 1, 0, [0, 0], [800, 600]);
        reused_window.stable_id = Some("0xwindow-b".to_string());
        let mock = MockHyprctl::new(vec![vec![old_window], vec![reused_window]]);
        let target = make_client(
            "true",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "true",
            vec![],
            None,
        );

        let error = replace_session(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
        )
        .expect_err("replace must not close a different window at a reused address");

        assert!(matches!(
            error,
            RestoreError::WindowIdentityChanged { address } if address == "0xreused"
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_replace_does_not_wait_for_a_new_window_at_a_reused_address() {
        let mut old_window =
            make_reconcile_window("0xreused", "kitty", "Old", 1, 0, [0, 0], [800, 600]);
        old_window.stable_id = Some("stable-old".to_string());
        let mut replacement = old_window.clone();
        replacement.title = "New".to_string();
        replacement.initial_title = "New".to_string();
        replacement.stable_id = Some("stable-new".to_string());
        let mock = MockHyprctl::new(vec![
            vec![old_window.clone()],
            vec![old_window],
            vec![replacement.clone()],
        ]);

        let report = replace_session(
            &make_session(vec![]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
        )
        .expect("a new client at the old address must not stall close completion");

        assert_eq!(report.extras, 1);
        assert_eq!(
            mock.dispatches(),
            vec!["closewindow address:0xreused".to_string()]
        );
    }

    #[test]
    fn test_replace_does_not_close_ambiguous_same_process_windows() {
        let first = make_reconcile_window("0xfirst", "kitty", "First", 1, 0, [0, 0], [800, 600]);
        let second =
            make_reconcile_window("0xsecond", "kitty", "Second", 1, 0, [900, 0], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![first, second]]);

        let error = replace_session(
            &make_session(vec![]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
        )
        .expect_err("replace must not close an ambiguous same-process window");

        assert!(matches!(
            error,
            RestoreError::WindowIdentityChanged { address } if address == "0xfirst"
        ));
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_marker_replace_leaves_windows_opened_after_safety_snapshot() {
        let mut captured =
            make_reconcile_window("0xcaptured", "kitty", "Captured", 1, 0, [0, 0], [800, 600]);
        captured.stable_id = Some("stable-captured".to_string());
        let mut safety_client = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        safety_client.title = captured.title.clone();
        safety_client.initial_title = captured.initial_title.clone();
        safety_client.address = Some(captured.address.clone());
        safety_client.pid = Some(captured.pid);
        safety_client.stable_id = captured.stable_id.clone();

        let mut opened_late = make_reconcile_window(
            "0xopened-late",
            "kitty",
            "Opened after snapshot",
            1,
            0,
            [900, 0],
            [800, 600],
        );
        opened_late.stable_id = Some("stable-late".to_string());
        let mock = SelectiveClosingMockHyprctl::new(vec![captured, opened_late]);
        let safety_snapshot = make_session(vec![safety_client]);
        let dir = tempfile::tempdir().unwrap();
        mark_replace_prepared("autosave-recovery", dir.path()).unwrap();

        let report = replace_session_with_marker(
            &make_session(vec![]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            ReplaceMarkerContext {
                dry_run: false,
                verbose: true,
                backup_name: "autosave-recovery",
                target_name: "target",
                sessions_dir: dir.path(),
                safety_snapshot: Some(&safety_snapshot),
            },
        )
        .expect("late windows should not make replacement fail");

        assert_eq!(report.extras, 1);
        assert_eq!(
            mock.dispatches(),
            vec!["closewindow address:0xcaptured".to_string()]
        );
    }

    #[test]
    fn test_replace_validates_targets_before_closing_existing_windows() {
        let current = make_reconcile_window("0xold", "kitty", "kitty", 1, 0, [0, 0], [800, 600]);
        let mock = ClosingMockHyprctl::new(vec![current]);
        let target = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "missing_replace_binary_xyz",
            vec![],
            None,
        );
        let mut config = Config::default();
        config.apps.insert(
            "kitty".to_string(),
            AppConfig {
                binary: Some("missing_replace_binary_xyz".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let error = replace_session(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &config,
            false,
            true,
        )
        .expect_err("missing target binary must abort before close");

        assert!(matches!(error, RestoreError::MissingLaunchBinary { .. }));
        assert!(mock.dispatches.borrow().is_empty());
    }

    #[test]
    fn test_replace_marker_stays_in_closing_phase_when_first_dispatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        mark_replace_prepared("autosave-recovery", dir.path()).unwrap();
        let mut current =
            make_reconcile_window("0xold", "kitty", "kitty", 1, 0, [0, 0], [800, 600]);
        current.stable_id = Some("stable-old".to_string());
        let mut safety_snapshot = make_client(
            "kitty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        safety_snapshot.address = Some(current.address.clone());
        safety_snapshot.pid = Some(current.pid);
        safety_snapshot.stable_id = current.stable_id.clone();
        let safety_snapshot = make_session(vec![safety_snapshot]);
        let error = replace_session_with_marker(
            &make_session(vec![]),
            &FailingCloseHyprctl { client: current },
            &EmptyProcessInfo,
            &Config::default(),
            ReplaceMarkerContext {
                dry_run: false,
                verbose: true,
                backup_name: "autosave-recovery",
                target_name: "target",
                sessions_dir: dir.path(),
                safety_snapshot: Some(&safety_snapshot),
            },
        )
        .expect_err("the failing close dispatch should abort replacement");

        assert!(matches!(error, RestoreError::Hyprctl(_)));
        assert_eq!(
            replace_marker(dir.path())
                .unwrap()
                .map(|marker| marker.phase),
            Some(ReplacePhase::Closing)
        );
    }

    #[test]
    fn test_replace_rejects_a_session_with_no_restorable_targets() {
        let target = make_client(
            "waybar",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "waybar",
            vec![],
            None,
        );
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec!["waybar".to_string()],
            },
            apps: HashMap::new(),
        };

        let error = validate_replacement_targets(&make_session(vec![target]), &config)
            .expect_err("replace must not clear the desktop for an empty target set");
        assert!(matches!(error, RestoreError::NoRestorableTargets));
    }

    #[test]
    fn test_restore_repairs_existing_window_on_the_wrong_workspace() {
        let mut target = make_client(
            "kitty",
            3,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        // Keep this test focused on workspace repair rather than monitor-name
        // resolution, which the mock intentionally does not provide.
        target.monitor.clear();
        let current = make_reconcile_window(
            "0xwrong-workspace",
            "kitty",
            "kitty",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![current]]);

        let report = restore_session_with_process_info(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
        )
        .unwrap();

        assert_eq!(report.restored, 1);
        assert_eq!(report.skipped, 0);
        assert!(mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch.contains("movetoworkspacesilent 3")));
    }

    #[test]
    fn test_reconcile_refreshes_a_match_before_repairing_it() {
        let target = make_client(
            "kitty",
            1,
            [100, 100],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let stale = make_reconcile_window("0xrefresh", "kitty", "kitty", 1, 0, [0, 0], [400, 300]);
        let current =
            make_reconcile_window("0xrefresh", "kitty", "kitty", 1, 0, [100, 100], [800, 600]);
        let mock = MockHyprctl::new(vec![vec![stale], vec![current]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.moved, 0);
        assert_eq!(report.failed, 0);
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_refreshes_before_launch_and_uses_window_that_just_appeared() {
        let mut target = make_client(
            "kitty",
            1,
            [100, 100],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.title = "Just appeared".to_string();
        let appeared = make_reconcile_window(
            "0xappeared",
            "kitty",
            "Just appeared",
            1,
            0,
            [100, 100],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![], vec![appeared]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.launched, 0);
        assert!(launcher.launches.borrow().is_empty());
        assert!(mock.dispatches().is_empty());
    }

    #[test]
    fn test_reconcile_refresh_does_not_steal_a_later_target_window() {
        let mut target_a = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "true",
            vec!["--directory".to_string(), "/project".to_string()],
            None,
        );
        target_a.title = "Project".to_string();
        target_a.initial_title = "Project".to_string();
        target_a.address = Some("0xtarget-a".to_string());

        let mut target_b = target_a.clone();
        target_b.workspace = 2;
        target_b.at = [30, 40];
        target_b.size = [900, 700];
        target_b.address = Some("0xtarget-b".to_string());

        let window_a =
            make_reconcile_window("0xtarget-a", "kitty", "Project", 1, 0, [10, 20], [800, 600]);
        let window_b =
            make_reconcile_window("0xtarget-b", "kitty", "Project", 2, 0, [30, 40], [900, 700]);
        let mut launched =
            make_reconcile_window("0xlaunched-a", "kitty", "Project", 1, 0, [0, 0], [400, 300]);
        launched.pid = 2000;
        let mock = MockHyprctl::new(vec![
            vec![window_a, window_b.clone()],
            vec![window_b.clone()],
            vec![window_b.clone()],
            vec![window_b.clone(), launched.clone()],
        ]);
        let launcher = RecordingLauncher {
            pid: Some(2000),
            ..RecordingLauncher::default()
        };
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig::default(),
            apps: HashMap::from([(
                "kitty".to_string(),
                AppConfig {
                    binary: Some("true".to_string()),
                    capture_cwd: None,
                    capture_last_command: None,
                    hint_template: None,
                    profile_workspaces: None,
                    default_workspace: None,
                },
            )]),
        };
        let process_info = CwdProcessInfo {
            cwds: HashMap::from([(1000, PathBuf::from("/project"))]),
        };

        let report = reconcile_session_with_launcher(
            &make_session(vec![target_a, target_b]),
            &mock,
            &process_info,
            &config,
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.launched, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(launcher.launches.borrow().len(), 1);
        assert_eq!(report.unchanged, 1);
        assert!(!mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch.contains("0xtarget-b")));
    }

    #[test]
    fn test_reconcile_stops_safely_when_a_matched_window_disappears() {
        let target = make_client(
            "kitty",
            1,
            [100, 100],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let existing = make_reconcile_window("0xgone", "kitty", "kitty", 1, 0, [0, 0], [400, 300]);
        let mock = MockHyprctl::new(vec![vec![existing], vec![]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.failed, 1);
        assert_eq!(report.moved, 0);
        assert!(mock.dispatches().is_empty());
        assert!(launcher.launches.borrow().is_empty());
    }

    #[test]
    fn test_reconcile_moves_only_a_target_in_the_wrong_place() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let mut target = target;
        target.title = "Project shell".to_string();

        let current = make_reconcile_window(
            "0xwrong-place",
            "kitty",
            "Project shell",
            1,
            0,
            [110, 220],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![current]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.unchanged, 0);
        assert_eq!(report.moved, 1);
        assert_eq!(report.launched, 0);
        assert!(launcher.launches.borrow().is_empty());
        let dispatches = mock.dispatches();
        assert_eq!(dispatches.len(), 1);
        assert!(dispatches[0].contains("movewindowpixel exact 10 20"));
    }

    #[test]
    fn test_reconcile_launches_only_missing_targets_and_leaves_extras_alone() {
        let mut existing_target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        existing_target.title = "Existing shell".to_string();

        let mut missing_target = make_client(
            "kitty",
            2,
            [30, 40],
            [900, 700],
            false,
            0,
            "true",
            vec!["--new-window".to_string()],
            None,
        );
        missing_target.title = "Missing shell".to_string();

        let existing = make_reconcile_window(
            "0xexisting",
            "kitty",
            "Existing shell",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        let extra = make_reconcile_window(
            "0xextra",
            "obsidian",
            "Unrelated notes",
            8,
            0,
            [500, 500],
            [1000, 800],
        );
        let launched = make_reconcile_window(
            "0xlaunched",
            "kitty",
            "Missing shell",
            1,
            0,
            [0, 0],
            [400, 300],
        );
        let mut launched = launched;
        launched.pid = 2000;
        let initial_state = vec![existing, extra];
        let mock = MockHyprctl::new(vec![
            initial_state.clone(),
            initial_state.clone(),
            initial_state.clone(),
            initial_state,
            vec![
                make_reconcile_window(
                    "0xexisting",
                    "kitty",
                    "Existing shell",
                    1,
                    0,
                    [10, 20],
                    [800, 600],
                ),
                make_reconcile_window(
                    "0xextra",
                    "obsidian",
                    "Unrelated notes",
                    8,
                    0,
                    [500, 500],
                    [1000, 800],
                ),
                launched,
            ],
        ]);
        let launcher = RecordingLauncher::default();
        let mut config = Config::default();
        config.apps.insert(
            "kitty".to_string(),
            AppConfig {
                binary: Some("true".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let report = reconcile_session_with_launcher(
            &make_session(vec![existing_target, missing_target]),
            &mock,
            &EmptyProcessInfo,
            &config,
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.launched, 1);
        assert_eq!(report.extras, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(launcher.launches.borrow().len(), 1);
        let dispatches = mock.dispatches();
        assert!(dispatches
            .iter()
            .any(|dispatch| dispatch.contains("movetoworkspacesilent 2")));
        assert!(
            dispatches
                .iter()
                .any(|dispatch| dispatch.contains("movewindowpixel exact 30 40")),
            "dispatches: {dispatches:?}"
        );
        assert!(!dispatches
            .iter()
            .any(|dispatch| dispatch.contains("0xextra")));
    }

    #[test]
    fn test_reconcile_matches_same_class_windows_one_to_one_by_saved_geometry() {
        let mut left = make_client(
            "com.mitchellh.ghostty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "ghostty",
            vec![],
            None,
        );
        left.title = "Ghostty".to_string();
        let mut right = left.clone();
        right.at = [1000, 20];
        right.title = "Ghostty".to_string();

        let current_right = make_reconcile_window(
            "0xright",
            "com.mitchellh.ghostty",
            "Ghostty",
            1,
            0,
            [1000, 20],
            [800, 600],
        );
        let current_left = make_reconcile_window(
            "0xleft",
            "com.mitchellh.ghostty",
            "Ghostty",
            1,
            0,
            [10, 20],
            [800, 600],
        );
        let plan = plan_reconciliation(
            &[left, right],
            &[
                ObservedClient::from_hypr_client(current_right, None, None),
                ObservedClient::from_hypr_client(current_left, None, None),
            ],
        );

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].map(|pair| pair.current_index), Some(1));
        assert_eq!(plan[1].map(|pair| pair.current_index), Some(0));
    }

    #[test]
    fn test_build_launch_command_maps_ghostty_window_class_to_ghostty_binary() {
        let client = make_client(
            "com.mitchellh.ghostty",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "com.mitchellh.ghostty",
            vec![
                "--working-directory".to_string(),
                "/tmp/project".to_string(),
            ],
            None,
        );

        let command = build_launch_command(&client);

        assert_eq!(command[0], "ghostty");
        assert_eq!(command[1], "--working-directory");
        assert_eq!(command[2], "/tmp/project");
    }

    #[test]
    fn test_default_brave_binary_is_trusted_without_app_config() {
        let client = make_client(
            "brave-browser",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "brave",
            vec![],
            None,
        );

        assert!(launch_command_is_trusted(&client, &Config::default()));
    }

    #[test]
    fn test_omarchy_webapp_launcher_is_trusted_for_matching_class() {
        let client = make_client(
            "chrome-chatgpt.com__-Default",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "omarchy-launch-or-focus-webapp",
            vec![
                "chrome-chatgpt.com__-Default".to_string(),
                "https://chatgpt.com/".to_string(),
            ],
            None,
        );

        assert!(launch_command_is_trusted(&client, &Config::default()));
    }

    #[test]
    fn test_standard_omarchy_webapp_launcher_is_trusted_for_matching_url() {
        let client = make_client(
            "chrome-chatgpt.com__-Default",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "omarchy-launch-webapp",
            vec!["https://chatgpt.com/".to_string()],
            None,
        );

        assert!(launch_command_is_trusted(&client, &Config::default()));
    }

    #[test]
    fn test_omarchy_webapp_focus_launcher_rejects_shell_text() {
        let client = make_client(
            "chrome-chatgpt.com__-Default",
            1,
            [0, 0],
            [800, 600],
            false,
            0,
            "omarchy-launch-or-focus",
            vec![
                "chrome-chatgpt.com__-Default".to_string(),
                "omarchy-launch-webapp https://chatgpt.com/; touch /tmp/proof".to_string(),
            ],
            None,
        );

        assert!(!launch_command_is_trusted(&client, &Config::default()));
    }

    #[test]
    fn test_reconcile_moves_a_window_to_the_saved_workspace_when_monitor_is_wrong() {
        let mut target = make_client(
            "obsidian",
            3,
            [50, 100],
            [1200, 900],
            false,
            0,
            "obsidian",
            vec![],
            None,
        );
        target.monitor = "DP-1".to_string();
        let current = make_reconcile_window(
            "0xwrong-monitor",
            "obsidian",
            "obsidian",
            3,
            1,
            [50, 100],
            [1200, 900],
        );

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-2"));

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0], "focuswindow address:0xwrong-monitor");
        assert_eq!(commands[1], "movewindow mon:DP-1 silent");
    }

    #[test]
    fn test_reconcile_applies_workspace_before_monitor_correction() {
        let mut target = make_client(
            "obsidian",
            2,
            [50, 100],
            [1200, 900],
            false,
            0,
            "obsidian",
            vec![],
            None,
        );
        target.monitor = "DP-1".to_string();
        let current = make_reconcile_window(
            "0xworkspace-and-monitor",
            "obsidian",
            "obsidian",
            3,
            1,
            [50, 100],
            [1200, 900],
        );

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-2"));

        assert_eq!(
            commands,
            vec![
                "movetoworkspacesilent 2,address:0xworkspace-and-monitor",
                "focuswindow address:0xworkspace-and-monitor",
                "movewindow mon:DP-1 silent",
            ]
        );
    }

    #[test]
    fn test_reconcile_skips_stale_geometry_when_saved_monitor_is_missing() {
        let mut target = make_client(
            "obsidian",
            2,
            [50, 100],
            [1200, 900],
            false,
            0,
            "obsidian",
            vec![],
            None,
        );
        target.monitor = "DP-1".to_string();
        let current = make_reconcile_window(
            "0xmissing-monitor",
            "obsidian",
            "obsidian",
            2,
            1,
            [4000, 4000],
            [300, 200],
        );

        let commands = build_reconcile_dispatch_commands_with_geometry(
            &target,
            &current,
            Some("DP-2"),
            target.at,
            target.size,
            false,
        );

        assert!(commands.is_empty());
    }

    #[test]
    fn test_reconcile_uses_named_workspace_and_restores_pinned_state() {
        let mut target = make_client(
            "kitty",
            -99,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.workspace_name = "special:magic".to_string();
        target.pinned = true;
        let mut current =
            make_reconcile_window("0xspecial", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);
        current.workspace.name = "1".to_string();

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-1"));

        assert!(commands
            .iter()
            .any(|command| command == "movetoworkspacesilent special:magic,address:0xspecial"));
        assert_eq!(
            commands.last().map(String::as_str),
            Some("pin address:0xspecial")
        );
    }

    #[test]
    fn test_reconcile_preserves_case_insensitive_named_workspace_prefixes() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.workspace_name = "Name:Writing Desk".to_string();
        let mut current =
            make_reconcile_window("0xnamed", "kitty", "kitty", 99, 0, [10, 20], [800, 600]);
        current.workspace.name = "writing desk".to_string();

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-1"));

        assert!(!commands
            .iter()
            .any(|command| command.starts_with("movetoworkspacesilent")));
    }

    #[test]
    fn test_reconcile_quotes_backslashes_in_named_monitor_tokens() {
        let mut target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        target.monitor = "Desk \\ A".to_string();
        let current =
            make_reconcile_window("0xmonitor", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-1"));

        assert_eq!(
            commands,
            vec![
                "focuswindow address:0xmonitor",
                "movewindow 'mon:Desk \\\\ A' silent"
            ]
        );
    }

    #[test]
    fn test_reconcile_unpins_before_moving_when_saved_state_is_not_pinned() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let mut current =
            make_reconcile_window("0xpinned", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);
        current.pinned = true;

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-1"));

        assert_eq!(commands, vec!["pin address:0xpinned"]);
    }

    #[test]
    fn test_reconcile_exits_fullscreen_before_restoring_saved_geometry() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let mut current =
            make_reconcile_window("0xfullscreen", "kitty", "kitty", 1, 0, [0, 0], [1920, 1080]);
        current.fullscreen = 2;

        let commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-1"));

        assert_eq!(commands[0], "focuswindow address:0xfullscreen");
        assert_eq!(commands[1], "fullscreenstate 0 0");
        assert!(commands
            .iter()
            .any(|command| command.contains("resizewindowpixel exact 800 600")));
        assert!(commands
            .iter()
            .any(|command| command.contains("movewindowpixel exact 10 20")));
    }

    #[test]
    fn test_reconcile_matches_changed_runtime_class_by_initial_class() {
        let target = make_client(
            "com.mitchellh.ghostty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "ghostty",
            vec![],
            None,
        );
        let mut current =
            make_reconcile_window("0xghostty", "ghostty", "shell", 1, 0, [10, 20], [800, 600]);
        current.initial_class = "com.mitchellh.ghostty".to_string();
        current.initial_title = "Ghostty".to_string();

        let plan = plan_reconciliation(
            &[target],
            &[ObservedClient::from_hypr_client(
                current,
                Some("DP-1".to_string()),
                None,
            )],
        );

        assert_eq!(plan[0].map(|pair| pair.kind), Some(MatchKind::AppIdentity));
    }

    #[test]
    fn test_reconcile_uses_profile_identity_when_available() {
        let mut target = make_client(
            "brave-browser",
            1,
            [0, 0],
            [1280, 800],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        target.profile_directory = Some("Profile 1".to_string());

        let client = make_reconcile_window(
            "0xprofile",
            "brave-browser",
            "Brave",
            1,
            0,
            [0, 0],
            [1280, 800],
        );
        let observed = ObservedClient::with_profile_directory(
            client.clone(),
            None,
            None,
            Some("Profile 1".to_string()),
        );
        assert!(plan_reconciliation(&[target.clone()], &[observed])[0].is_some());

        let wrong_profile =
            ObservedClient::with_profile_directory(client, None, None, Some("Default".to_string()));
        assert!(plan_reconciliation(&[target], &[wrong_profile])[0].is_none());
    }

    #[test]
    fn test_reconcile_routes_brave_initial_class_through_profile_targets() {
        let mut brave_window = make_client(
            "browser-wrapper",
            1,
            [0, 0],
            [1280, 800],
            false,
            0,
            "brave",
            vec![],
            None,
        );
        brave_window.initial_class = "BRAVE-BROWSER".to_string();

        let session = Session {
            name: "test".to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![brave_window],
            brave_profiles: vec![BraveProfile {
                directory: "Default".to_string(),
                name: "Default".to_string(),
            }],
        };
        let mut apps = HashMap::new();
        apps.insert(
            "brave-browser".to_string(),
            AppConfig {
                binary: Some("brave".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: Some(1),
            },
        );
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig::default(),
            apps,
        };

        let targets = build_reconcile_targets(&session, &config);
        assert_eq!(targets.len(), 1);
        assert!(targets[0].label.starts_with("brave profile"));
        assert_eq!(
            targets[0].client.profile_directory.as_deref(),
            Some("Default")
        );
    }

    #[test]
    fn test_reconcile_leaves_extra_same_app_windows_untouched() {
        let target = make_client(
            "kitty",
            1,
            [10, 20],
            [800, 600],
            false,
            0,
            "kitty",
            vec![],
            None,
        );
        let matching =
            make_reconcile_window("0xmatching", "kitty", "kitty", 1, 0, [10, 20], [800, 600]);
        let extra = make_reconcile_window(
            "0xextra-kitty",
            "kitty",
            "kitty",
            8,
            0,
            [1000, 20],
            [800, 600],
        );
        let mock = MockHyprctl::new(vec![vec![matching, extra]]);
        let launcher = RecordingLauncher::default();

        let report = reconcile_session_with_launcher(
            &make_session(vec![target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
            false,
            true,
            &launcher,
        )
        .unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.moved, 0);
        assert_eq!(report.launched, 0);
        assert_eq!(report.extras, 1);
        assert!(mock.dispatches().is_empty());
        assert!(launcher.launches.borrow().is_empty());
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("0xextra-kitty") && detail.contains("left untouched")));
    }

    #[test]
    fn test_reconcile_observes_terminal_shell_cwd_before_emulator_cwd() {
        let client =
            make_reconcile_window("0xterminal", "kitty", "kitty", 1, 0, [0, 0], [800, 600]);

        assert_eq!(
            observe_cwd(&client, &ChildCwdProcessInfo),
            Some(PathBuf::from("/shell-current-directory"))
        );
    }
}
