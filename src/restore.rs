use crate::config::{app_config_for, is_ignored_class, Config};
use crate::hyprctl::{HyprClient, HyprMonitor, HyprctlClient, HyprctlError};
use crate::process::{
    find_profile_directory, select_terminal_process, ProcessInfoProvider, RealProcessInfo,
};
use crate::session::{Monitor, Session, SessionClient};
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
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
    #[error("window {address} disappeared before reconciliation completed")]
    WindowDisappeared { address: String },
    #[error("launch command '{command}' for {target} is not authorized by app identity or config")]
    UntrustedLaunch { target: String, command: String },
    #[error("binary '{command}' for {target} was not found")]
    MissingLaunchBinary { target: String, command: String },
    #[error("timed out waiting for existing windows to close")]
    ReplaceTimeout,
}

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
}

pub struct RealProcessLauncher;

impl ProcessLauncher for RealProcessLauncher {
    fn spawn(&self, command: &str, args: &[String]) -> Result<LaunchedProcess, std::io::Error> {
        Command::new(command)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|child| LaunchedProcess {
                pid: Some(child.id()),
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
        }
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
    if targets.is_empty() {
        return vec![];
    }

    let mut candidates = vec![vec![None; current.len()]; targets.len()];

    for (target_index, target) in targets.iter().enumerate() {
        for (current_index, observed) in current.iter().enumerate() {
            if let Some((score, kind)) = match_score(target, observed) {
                candidates[target_index][current_index] = Some(ReconcilePair {
                    target_index,
                    current_index,
                    score,
                    kind,
                });
            }
        }
    }

    // Add dummy rows and columns so a target may remain unmatched when no
    // suitable window exists, and an existing window may remain an extra.  A
    // valid match always has a positive score; dummy edges have weight zero.
    let size = targets.len() + current.len();
    let mut weights = vec![vec![0_i64; size]; size];
    let impossible = -1_000_000_000_000_i64;
    for (target_index, row) in candidates.iter().enumerate() {
        for (current_index, candidate) in row.iter().enumerate() {
            weights[target_index][current_index] = candidate
                .map(|pair| i64::from(pair.score))
                .unwrap_or(impossible);
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
    let current = &observed.client;
    if !classes_match(target, current) {
        return None;
    }

    let mut score = 1_000;
    let mut kind = MatchKind::ClassFallback;

    if let Some(target_profile) = &target.profile_directory {
        if let Some(current_profile) = &observed.profile_directory {
            if !same_nonempty(target_profile, current_profile) {
                return None;
            }
            score += 1_200;
            kind = MatchKind::ProfileIdentity;
        } else {
            // Profile flags are not always visible from a browser window's
            // PID.  Keep the candidate usable, but make a known profile beat
            // an unknown one whenever another piece of identity agrees.
            score -= 250;
        }
    }

    if same_nonempty(&target.class, &current.class) {
        score += 150;
    } else {
        score += 50;
    }

    if same_nonempty(&target.initial_class, &current.initial_class) {
        score += 350;
        kind = MatchKind::AppIdentity;
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
        score -= manhattan_distance(target.at, current.at).min(120);
    }
    if target.size == current.size {
        score += 90;
    } else {
        score -= manhattan_distance(target.size, current.size).min(90);
    }

    Some((score, kind))
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

fn is_brave_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("brave-browser")
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

fn manhattan_distance(left: [i32; 2], right: [i32; 2]) -> i32 {
    (left[0] - right[0]).abs() + (left[1] - right[1]).abs()
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
    let mut report = RestoreReport::default();

    // Fetch current windows once to detect already-running duplicates.
    let mut consumed_existing = HashSet::new();
    let current_monitors = hyprctl.get_monitors()?;
    let existing_clients = if dry_run {
        Vec::new()
    } else {
        observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?
    };

    // Detect if profile-based Brave restore applies.
    let has_brave_profiles =
        !session.brave_profiles.is_empty() && app_config_for(config, "brave-browser", "").is_some();

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
            let restore_client = adapt_client_geometry(
                client,
                &session.monitors,
                find_monitor_by_name(&current_monitors, &client.monitor),
            );

            if dry_run {
                let cmds = build_dispatch_commands(&restore_client);
                report.details.push(format!(
                    "[dry-run] ws={} {} → {}",
                    ws, client.class, client.launch.command
                ));
                for cmd in &cmds {
                    report.details.push(format!("  hyprctl dispatch {cmd}"));
                }
                report.restored += 1;
                continue;
            }

            // An existing target is repaired in place, even when it is on a
            // different workspace or monitor.  This keeps the legacy command
            // useful on its own while sharing the same placement semantics as
            // --reconcile.
            if let Some(existing_index) =
                find_existing_restore_match(client, &existing_clients, &consumed_existing, config)
            {
                let current = existing_clients[existing_index].clone();
                consumed_existing.insert(current.client.address.clone());
                let commands = build_reconcile_dispatch_commands(
                    &restore_client,
                    &current.client,
                    current.monitor_name.as_deref(),
                );
                if commands.is_empty() {
                    report.details.push(format!(
                        "SKIP: {} already on ws={}",
                        current.client.class, client.workspace
                    ));
                    report.skipped += 1;
                } else {
                    match dispatch_existing_repairs(&current.client, &commands, hyprctl) {
                        Ok(()) => {
                            report.restored += 1;
                            if verbose {
                                report.details.push(format!(
                                    "OK: repaired {} at address {}",
                                    client.class, current.client.address
                                ));
                            }
                        }
                        Err(error) => {
                            report.failed += 1;
                            report.details.push(format!(
                                "FAIL: {} at address {} — {error}",
                                client.class, current.client.address
                            ));
                        }
                    }
                }
                continue;
            }

            // Validate the effective binary is available before attempting to spawn.
            let launch_command = build_launch_command(&restore_client);
            if !launch_command_is_trusted(&restore_client, config) {
                report.details.push(format!(
                    "FAIL: launch command '{}' for {} is not authorized by app identity or config",
                    launch_command[0], client.class
                ));
                report.failed += 1;
                continue;
            }
            if resolve_launch_binary(&launch_command[0], &client.class).is_err() {
                let msg = format!(
                    "SKIP: binary '{}' not found for {}",
                    launch_command[0], client.class
                );
                if verbose {
                    report.details.push(msg);
                }
                report.skipped += 1;
                continue;
            }

            match restore_single_client_with_launcher_and_process_info(
                &restore_client,
                hyprctl,
                process_info,
                config,
                &RealProcessLauncher,
            ) {
                Ok(msg) => {
                    if verbose {
                        report.details.push(msg);
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
            target.client = adapt_client_geometry(
                &target.client,
                &session.monitors,
                find_monitor_by_name(&current_monitors, &target.client.monitor),
            );
            if dry_run {
                report.details.push(format!(
                    "[dry-run] {} → ws={}",
                    target.label, target.client.workspace
                ));
                for command in build_dispatch_commands(&target.client) {
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
                    "SKIP: binary '{}' not found for {}",
                    launch_command[0], target.label
                ));
                report.skipped += 1;
                continue;
            }

            match restore_single_client_with_launcher_and_process_info(
                &target.client,
                hyprctl,
                process_info,
                config,
                &launcher,
            ) {
                Ok(message) => {
                    if verbose {
                        report.details.push(message);
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
    consumed: &HashSet<String>,
    config: &Config,
) -> Option<usize> {
    existing
        .iter()
        .enumerate()
        .filter(|(_, current)| {
            !consumed.contains(&current.client.address)
                && !is_ignored_class(&current.client.class, &config.filters.ignore_classes)
                && !is_ignored_class(
                    &current.client.initial_class,
                    &config.filters.ignore_classes,
                )
        })
        .filter_map(|(index, current)| {
            match_score(target, current)
                .map(|(score, _)| (score, current.client.address.clone(), index))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)))
        .map(|(_, _, index)| index)
}

fn dispatch_existing_repairs(
    current: &HyprClient,
    commands: &[String],
    hyprctl: &dyn HyprctlClient,
) -> Result<(), RestoreError> {
    for command in commands {
        if !window_is_present(&current.address, hyprctl)? {
            return Err(RestoreError::WindowDisappeared {
                address: current.address.clone(),
            });
        }
        hyprctl.dispatch(command)?;
    }
    Ok(())
}

/// Validate every executable that a replacement will need before any current
/// window is closed.  Unlike an ordinary reconcile pass, replacement will
/// make every target missing, so every launch command must be trusted and
/// resolvable up front.
pub fn validate_replacement_targets(
    session: &Session,
    config: &Config,
) -> Result<(), RestoreError> {
    for target in build_reconcile_targets(session, config) {
        let launch_command = build_launch_command(&target.client);
        if !launch_command_is_trusted(&target.client, config) {
            return Err(RestoreError::UntrustedLaunch {
                target: target.label,
                command: launch_command[0].clone(),
            });
        }
        if resolve_launch_binary(&launch_command[0], &target.label).is_err() {
            return Err(RestoreError::MissingLaunchBinary {
                target: target.label,
                command: launch_command[0].clone(),
            });
        }
    }
    Ok(())
}

/// Close the current desktop and reconcile the already-loaded session in the
/// same helper process.  Loading and validating the target before the first
/// close removes the UI's check/use race: deleting or editing the session
/// after this function starts cannot change what will be restored.
pub fn replace_session(
    session: &Session,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    dry_run: bool,
    verbose: bool,
) -> Result<ReconcileReport, RestoreError> {
    validate_replacement_targets(session, config)?;
    if dry_run {
        return reconcile_session(session, hyprctl, process_info, config, true, verbose);
    }

    let current = hyprctl.get_clients()?;
    for client in current {
        if !client.address.is_empty() {
            hyprctl.dispatch(&format!("closewindow address:{}", client.address))?;
        }
    }

    let timeout = Duration::from_millis(config.general.window_detect_timeout_ms.clamp(
        crate::config::MIN_WINDOW_DETECT_TIMEOUT_MS,
        crate::config::MAX_WINDOW_DETECT_TIMEOUT_MS,
    ));
    let started = Instant::now();
    loop {
        if hyprctl.get_clients()?.is_empty() {
            break;
        }
        if started.elapsed() >= timeout {
            return Err(RestoreError::ReplaceTimeout);
        }
        thread::sleep(Duration::from_millis(50));
    }

    reconcile_session(session, hyprctl, process_info, config, false, verbose)
}

#[derive(Debug, Clone)]
struct ReconcileTarget {
    client: SessionClient,
    label: String,
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
    let targets = build_reconcile_targets(session, config);
    let current_monitors = hyprctl.get_monitors()?;
    let observed = observe_clients_with_monitors(hyprctl, process_info, config, &current_monitors)?;
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
    let mut used_current = HashSet::new();

    for (target_index, target) in targets.iter().enumerate() {
        if let Some(pair) = plan[target_index] {
            let current = if dry_run {
                observed[pair.current_index].clone()
            } else {
                match observe_client_by_address(
                    &observed[pair.current_index].client.address,
                    hyprctl,
                    process_info,
                    config,
                ) {
                    Ok(Some(current)) => current,
                    Ok(None) => {
                        report.failed += 1;
                        report.details.push(format!(
                            "FAIL: {} — window {} disappeared before repair",
                            target.label, observed[pair.current_index].client.address
                        ));
                        continue;
                    }
                    Err(error) => {
                        report.failed += 1;
                        report
                            .details
                            .push(format!("FAIL: {} — {error}", target.label));
                        continue;
                    }
                }
            };
            used_current.insert(pair.current_index);
            report.matched += 1;

            let current_target_monitor =
                find_monitor_by_name(&current_monitors, &target.client.monitor).cloned();
            let command_target = adapt_client_geometry(
                &target.client,
                &session.monitors,
                current_target_monitor.as_ref(),
            );
            let commands = build_reconcile_dispatch_commands(
                &command_target,
                &current.client,
                current.monitor_name.as_deref(),
            );

            if commands.is_empty() {
                report.unchanged += 1;
                if dry_run || verbose {
                    report.details.push(format!(
                        "{}: {} already in place (matched {})",
                        if dry_run { "[dry-run]" } else { "OK" },
                        target.label,
                        match_kind_label(pair.kind)
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
                    match_kind_label(pair.kind)
                ));
                for command in &commands {
                    report.details.push(format!("  hyprctl dispatch {command}"));
                }
                continue;
            }

            let mut applied = true;
            for command in &commands {
                match window_is_present(&current.client.address, hyprctl) {
                    Ok(true) => {}
                    Ok(false) => {
                        report.details.push(format!(
                            "FAIL: {} at address {} — window disappeared before '{command}'",
                            target.label, current.client.address
                        ));
                        report.failed += 1;
                        applied = false;
                        break;
                    }
                    Err(error) => {
                        report.details.push(format!(
                            "FAIL: {} at address {} — could not verify window before '{command}': {error}",
                            target.label, current.client.address
                        ));
                        report.failed += 1;
                        applied = false;
                        break;
                    }
                }
                if let Err(error) = hyprctl.dispatch(command) {
                    report.details.push(format!(
                        "FAIL: {} at address {} — {error}",
                        target.label, current.client.address
                    ));
                    report.failed += 1;
                    applied = false;
                    break;
                }
            }

            if applied {
                report.moved += 1;
                if verbose {
                    report.details.push(format!(
                        "OK: repaired {} at address {}",
                        target.label, current.client.address
                    ));
                }
            }
            continue;
        }

        let target_client = &target_clients[target_index];
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
            for command in build_dispatch_commands(target_client) {
                report.details.push(format!("  hyprctl dispatch {command}"));
            }
            continue;
        }

        if resolve_launch_binary(&launch_command[0], &target.label).is_err() {
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: binary '{}' not found for {}",
                launch_command[0], target.label
            ));
            continue;
        }

        match restore_single_client_with_launcher_and_process_info(
            target_client,
            hyprctl,
            process_info,
            config,
            launcher,
        ) {
            Ok(_) => {
                report.launched += 1;
                if verbose {
                    report
                        .details
                        .push(format!("OK: launched {}", target.label));
                }
            }
            Err(error) => {
                report.failed += 1;
                report
                    .details
                    .push(format!("FAIL: {} — {error}", target.label));
            }
        }
    }

    report.extras = observed.len().saturating_sub(used_current.len());
    if verbose {
        for (index, window) in observed.iter().enumerate() {
            if !used_current.contains(&index) {
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

fn build_reconcile_targets(session: &Session, config: &Config) -> Vec<ReconcileTarget> {
    let has_brave_profiles =
        !session.brave_profiles.is_empty() && app_config_for(config, "brave-browser", "").is_some();

    let mut targets: Vec<ReconcileTarget> = session
        .clients
        .iter()
        .filter(|client| {
            !is_ignored_class(&client.class, &config.filters.ignore_classes)
                && !is_ignored_class(&client.initial_class, &config.filters.ignore_classes)
        })
        .filter(|client| !(has_brave_profiles && is_brave_client(client)))
        .cloned()
        .map(|client| ReconcileTarget {
            label: format!("{} '{}'", client.class, client.title),
            client,
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
        for profile in &session.brave_profiles {
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

fn observe_clients(
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
) -> Result<Vec<ObservedClient>, RestoreError> {
    let monitors = hyprctl.get_monitors()?;
    observe_clients_with_monitors(hyprctl, process_info, config, &monitors)
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
            let profile_directory = find_profile_directory(process_info, client.pid);
            ObservedClient::with_profile_directory(client, monitor_name, cwd, profile_directory)
        })
        .collect();
    clients.sort_by(|left, right| left.client.address.cmp(&right.client.address));
    Ok(clients)
}

fn observe_client_by_address(
    address: &str,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
) -> Result<Option<ObservedClient>, RestoreError> {
    Ok(observe_clients(hyprctl, process_info, config)?
        .into_iter()
        .find(|observed| observed.client.address == address))
}

fn window_is_present(address: &str, hyprctl: &dyn HyprctlClient) -> Result<bool, RestoreError> {
    Ok(hyprctl
        .get_clients()?
        .into_iter()
        .any(|client| client.address == address))
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

    let scale_x = current_monitor.width as f64 / saved_monitor.width as f64;
    let scale_y = current_monitor.height as f64 / saved_monitor.height as f64;
    let width = scaled_extent(target.size[0], scale_x, current_monitor.width);
    let height = scaled_extent(target.size[1], scale_y, current_monitor.height);
    let relative_x = i64::from(target.at[0]) - i64::from(saved_x);
    let relative_y = i64::from(target.at[1]) - i64::from(saved_y);
    let proposed_x = i64::from(current_x) + scale_coordinate(relative_x, scale_x);
    let proposed_y = i64::from(current_y) + scale_coordinate(relative_y, scale_y);
    let at = [
        clamp_coordinate(proposed_x, current_x, current_monitor.width, width),
        clamp_coordinate(proposed_y, current_y, current_monitor.height, height),
    ];

    let mut adapted = target.clone();
    adapted.at = at;
    adapted.size = [width, height];
    adapted
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
    )
}

fn build_reconcile_dispatch_commands_with_geometry(
    target: &SessionClient,
    current: &HyprClient,
    current_monitor: Option<&str>,
    desired_at: [i32; 2],
    desired_size: [i32; 2],
) -> Vec<String> {
    let monitor_mismatch = !target.monitor.is_empty()
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
        commands.push(format!("fullscreenstate 0 0,address:{}", current.address));
    }
    if monitor_mismatch {
        commands.push(format!(
            "movetomonitor {},address:{}",
            quote_dispatch_token(&target.monitor),
            current.address
        ));
    }
    if workspace_mismatch {
        commands.push(format!(
            "movetoworkspacesilent {},address:{}",
            quote_dispatch_token(&workspace_selector(target)),
            current.address
        ));
    }
    if current.floating != target.floating {
        commands.push(format!("togglefloating address:{}", current.address));
    }

    if target.fullscreen == 0 {
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
        commands.push(format!(
            "fullscreenstate {} {},address:{}",
            target.fullscreen, target.fullscreen, current.address
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
    )
}

fn restore_single_client_with_launcher_and_process_info(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    launcher: &dyn ProcessLauncher,
) -> Result<String, RestoreError> {
    // 1. Snapshot existing window addresses before launching.
    let before: HashSet<String> = hyprctl
        .get_clients()?
        .into_iter()
        .map(|c| c.address)
        .collect();

    // 2. Build and spawn the launch command.
    let launch_cmd = build_launch_command(client);
    let launch_binary = resolve_launch_binary(&launch_cmd[0], &client.class)?;
    let launched = launcher
        .spawn(&launch_binary.to_string_lossy(), &launch_cmd[1..])
        .map_err(|e| {
            HyprctlError::CommandFailed(format!("spawn '{}' failed: {e}", launch_cmd[0]))
        })?;

    // 3. Poll for the new window (address not in snapshot + class match).
    let timeout = Duration::from_millis(config.general.window_detect_timeout_ms.clamp(
        crate::config::MIN_WINDOW_DETECT_TIMEOUT_MS,
        crate::config::MAX_WINDOW_DETECT_TIMEOUT_MS,
    ));
    let poll_interval = Duration::from_millis(100);
    let candidate_settle = Duration::from_millis(250).min(timeout);
    let start = Instant::now();
    let mut first_candidate_at = None;

    let new_window = loop {
        if start.elapsed() > timeout {
            return Err(RestoreError::Hyprctl(HyprctlError::CommandFailed(format!(
                "timeout waiting for '{}' window to appear",
                client.class
            ))));
        }
        thread::sleep(poll_interval);

        let current = hyprctl.get_clients()?;
        let candidates: Vec<HyprClient> = current
            .into_iter()
            .filter(|window| {
                !before.contains(&window.address)
                    && classes_match(client, window)
                    && candidate_matches_profile(client, window, process_info)
            })
            .collect();
        if !candidates.is_empty() {
            let candidate_seen_at = first_candidate_at.get_or_insert_with(Instant::now);
            let process_related = launched.pid.is_some()
                && candidates.iter().any(|candidate| {
                    process_info.is_process_related(launched.pid.unwrap(), candidate.pid)
                });
            if candidates.len() == 1
                && !process_related
                && candidate_seen_at.elapsed() < candidate_settle
            {
                continue;
            }
            break choose_launched_window(client, candidates, launched, process_info)?;
        }
    };

    // 4. Use the same minimal repair logic as reconciliation.  The launch
    // PID/window correlation above prevents this address from belonging to a
    // different same-class window that appeared during startup.
    let mut commands = build_reconcile_dispatch_commands(client, &new_window, None);
    if !client.monitor.is_empty() {
        commands.insert(
            0,
            format!(
                "movetomonitor {},address:{}",
                quote_dispatch_token(&client.monitor),
                new_window.address
            ),
        );
    }
    for command in commands {
        if !window_is_present(&new_window.address, hyprctl)? {
            return Err(RestoreError::WindowDisappeared {
                address: new_window.address.clone(),
            });
        }
        hyprctl.dispatch(&command)?;
    }

    // 7. Throttle subsequent launches to give the compositor time to settle.
    thread::sleep(Duration::from_millis(
        config
            .general
            .restore_delay_ms
            .min(crate::config::MAX_RESTORE_DELAY_MS),
    ));

    Ok(format!(
        "OK: {} → ws={} at {:?}",
        client.class, client.workspace, client.at
    ))
}

fn candidate_matches_profile(
    target: &SessionClient,
    candidate: &HyprClient,
    process_info: &dyn ProcessInfoProvider,
) -> bool {
    let Some(target_profile) = &target.profile_directory else {
        return true;
    };
    find_profile_directory(process_info, candidate.pid)
        .map(|candidate_profile| candidate_profile.eq_ignore_ascii_case(target_profile))
        .unwrap_or(true)
}

fn choose_launched_window(
    target: &SessionClient,
    candidates: Vec<HyprClient>,
    launched: LaunchedProcess,
    process_info: &dyn ProcessInfoProvider,
) -> Result<HyprClient, RestoreError> {
    let candidates: Vec<HyprClient> = candidates
        .into_iter()
        .filter(|candidate| candidate_matches_profile(target, candidate, process_info))
        .collect();
    let related: Vec<HyprClient> = launched
        .pid
        .map(|pid| {
            candidates
                .iter()
                .filter(|candidate| process_info.is_process_related(pid, candidate.pid))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
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
            let profile_directory = find_profile_directory(process_info, candidate.pid);
            let observed = ObservedClient::with_profile_directory(
                candidate.clone(),
                None,
                None,
                profile_directory,
            );
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

/// Build the list of `hyprctl dispatch` argument strings that would be
/// issued for a given client.  Used both by the dry-run path and by tests.
pub fn build_dispatch_commands(client: &SessionClient) -> Vec<String> {
    let addr = "address:0xNEW";
    let launch = build_launch_command(client);

    let mut cmds = vec![format!("exec {}", launch.join(" "))];
    if !client.monitor.is_empty() {
        cmds.push(format!(
            "movetomonitor {},{}",
            quote_dispatch_token(&client.monitor),
            addr
        ));
    }
    cmds.extend([
        format!(
            "movetoworkspacesilent {},{}",
            quote_dispatch_token(&workspace_selector(client)),
            addr
        ),
        format!(
            "resizewindowpixel exact {} {},{}",
            client.size[0], client.size[1], addr
        ),
        format!(
            "movewindowpixel exact {} {},{}",
            client.at[0], client.at[1], addr
        ),
    ]);

    if client.floating {
        cmds.push(format!("togglefloating {addr}"));
    }
    if client.fullscreen > 0 {
        cmds.push(format!(
            "fullscreenstate {} {},{}",
            client.fullscreen, client.fullscreen, addr
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
    use crate::session::{BraveProfile, LaunchInfo, Session, SessionClient};
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

    #[derive(Default)]
    struct RecordingLauncher {
        launches: RefCell<Vec<(String, Vec<String>)>>,
        pid: Option<u32>,
    }

    impl ProcessLauncher for RecordingLauncher {
        fn spawn(&self, command: &str, args: &[String]) -> Result<LaunchedProcess, std::io::Error> {
            self.launches
                .borrow_mut()
                .push((command.to_string(), args.to_vec()));
            Ok(LaunchedProcess { pid: self.pid })
        }
    }

    // ── MockHyprctl ───────────────────────────────────────────────────────────

    /// A mock that returns pre-programmed client snapshots on successive
    /// `get_clients()` calls, simulating a new window appearing.
    struct MockHyprctl {
        /// One entry per `get_clients()` call; last entry is repeated if exhausted.
        client_states: RefCell<Vec<Vec<HyprClient>>>,
        state_index: RefCell<usize>,
        dispatches: RefCell<Vec<String>>,
    }

    impl MockHyprctl {
        fn new(client_states: Vec<Vec<HyprClient>>) -> Self {
            Self {
                client_states: RefCell::new(client_states),
                state_index: RefCell::new(0),
                dispatches: RefCell::new(Vec::new()),
            }
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

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![])
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
            "kitty",
            vec!["--directory".to_string(), "/home/user".to_string()],
            Some("claude --continue".to_string()),
        );
        let session = make_session(vec![client]);
        let config = Config::default();
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

        restore_single_client_with_launcher(&target, &mock, &config, &launcher).unwrap();

        assert!(mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch == "fullscreenstate 1 1,address:0xnew-fullscreen"));
        assert!(!mock
            .dispatches()
            .iter()
            .any(|dispatch| dispatch == "fullscreen 1"));
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

    // ── Test: skips client when binary is missing ────────────────────────────

    #[test]
    fn test_restore_skips_missing_binary() {
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

        assert_eq!(report.skipped, 1, "missing binary should be skipped");
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 0);
        // No dispatches should have been sent.
        assert!(mock.dispatches().is_empty());
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

    // ── Test: dry-run does NOT skip duplicates ──────────────────────────

    #[test]
    fn test_restore_dry_run_ignores_duplicates() {
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
            focus_history_id: 0,
            pid: 9999,
        };

        // Even though the existing window matches, dry-run should ignore it.
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

        assert_eq!(report.restored, 1, "dry-run should not skip duplicates");
        assert_eq!(report.skipped, 0);
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

        // 2 skipped as duplicates, 1 skipped as binary-not-found → total 3.
        assert_eq!(
            report.skipped, 3,
            "expected 3 skipped; got {}",
            report.skipped
        );
        assert_eq!(report.restored, 0);
        assert_eq!(report.failed, 0);

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
                    "kitty",
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
                binary: Some("brave".to_string()),
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

    // ── Test: unmapped profile uses default_workspace fallback ──────────

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
                binary: Some("brave".to_string()),
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
        // Should use default_workspace=3 since "Profile 9" has no mapping
        assert!(
            report.details.iter().any(|d| d.contains("ws=3")),
            "unmapped profile should use default_workspace=3; got: {:?}",
            report.details
        );
    }

    // ── Test: without profiles, brave windows restore individually ───────

    #[test]
    fn test_restore_brave_without_profiles_falls_back() {
        // Session WITHOUT brave_profiles — should restore brave windows normally
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
                "brave",
                vec![],
                None,
            )],
            brave_profiles: vec![], // no profiles
        };

        let config = Config::default();
        let mock = MockHyprctl::new(vec![]);
        let report = restore_session(&session, &mock, &config, true, true).unwrap();

        // Without profiles, brave windows are restored individually
        assert_eq!(report.restored, 1);
        assert!(report
            .details
            .iter()
            .any(|d| d.contains("[dry-run]") && d.contains("brave")));
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

        let report = restore_session(
            &make_session(vec![target]),
            &mock,
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
        let initial_state = vec![existing, extra];
        let mock = MockHyprctl::new(vec![
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
        assert!(dispatches
            .iter()
            .any(|dispatch| dispatch.contains("movewindowpixel exact 30 40")));
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

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], "movetomonitor DP-1,address:0xwrong-monitor");
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
            vec!["movetomonitor 'Desk \\\\ A',address:0xmonitor"]
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

        assert_eq!(commands[0], "fullscreenstate 0 0,address:0xfullscreen");
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
