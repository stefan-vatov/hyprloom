use crate::config::Config;
use crate::hyprctl::{HyprClient, HyprctlClient, HyprctlError};
use crate::process::ProcessInfoProvider;
use crate::session::{Session, SessionClient};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    fn spawn(&self, command: &str, args: &[String]) -> Result<(), std::io::Error>;
}

pub struct RealProcessLauncher;

impl ProcessLauncher for RealProcessLauncher {
    fn spawn(&self, command: &str, args: &[String]) -> Result<(), std::io::Error> {
        Command::new(command)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    ExactIdentity,
    AppIdentity,
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
    let mut candidates = Vec::new();

    for (target_index, target) in targets.iter().enumerate() {
        for (current_index, observed) in current.iter().enumerate() {
            if let Some((score, kind)) = match_score(target, observed) {
                candidates.push(ReconcilePair {
                    target_index,
                    current_index,
                    score,
                    kind,
                });
            }
        }
    }

    // This is a greedy maximum-weight assignment.  It is intentionally small
    // and predictable for desktop-sized window sets: the best identity match
    // is committed first, then the remaining windows are assigned by the
    // same rules without ever reusing an existing address.
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.target_index.cmp(&b.target_index))
            .then(a.current_index.cmp(&b.current_index))
    });

    let mut assigned_targets = HashSet::new();
    let mut assigned_current = HashSet::new();
    let mut plan = vec![None; targets.len()];

    for candidate in candidates {
        if assigned_targets.contains(&candidate.target_index)
            || assigned_current.contains(&candidate.current_index)
        {
            continue;
        }

        assigned_targets.insert(candidate.target_index);
        assigned_current.insert(candidate.current_index);
        plan[candidate.target_index] = Some(candidate);
    }

    plan
}

fn match_score(target: &SessionClient, observed: &ObservedClient) -> Option<(i32, MatchKind)> {
    let current = &observed.client;
    if !classes_match(target, current) {
        return None;
    }

    let mut score = 1_000;
    let mut kind = MatchKind::ClassFallback;

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

    if target.workspace == current.workspace.id {
        score += 180;
    }
    if let Some(monitor_name) = &observed.monitor_name {
        if !target.monitor.is_empty() && target.monitor == *monitor_name {
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
    let target_classes = [&target.class, &target.initial_class];
    let current_classes = [&current.class, &current.initial_class];

    target_classes.iter().any(|target_class| {
        !target_class.is_empty()
            && current_classes.iter().any(|current_class| {
                !current_class.is_empty() && target_class.eq_ignore_ascii_case(current_class)
            })
    })
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
    let mut report = RestoreReport::default();

    // Fetch current windows once to detect already-running duplicates.
    let mut existing_counts: HashMap<(String, i32), usize> = HashMap::new();
    if !dry_run {
        if let Ok(current) = hyprctl.get_clients() {
            for c in &current {
                *existing_counts
                    .entry((c.class.clone(), c.workspace.id))
                    .or_insert(0) += 1;
            }
        }
    }

    // Detect if profile-based Brave restore applies.
    let has_brave_profiles =
        !session.brave_profiles.is_empty() && config.apps.contains_key("brave-browser");

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
            if has_brave_profiles && client.class == "brave-browser" {
                continue;
            }

            if dry_run {
                let cmds = build_dispatch_commands(client);
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

            // Count-based duplicate detection: skip if enough instances already exist.
            let key = (client.class.clone(), client.workspace);
            if let Some(count) = existing_counts.get_mut(&key) {
                if *count > 0 {
                    let msg = format!("SKIP: {} already on ws={}", client.class, client.workspace);
                    report.details.push(msg);
                    report.skipped += 1;
                    *count -= 1;
                    continue;
                }
            }

            // Validate the effective binary is available before attempting to spawn.
            let launch_command = build_launch_command(client);
            if which::which(&launch_command[0]).is_err() {
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

            match restore_single_client(client, hyprctl, config, verbose) {
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

    // Restore Brave profiles (one window per profile).
    if has_brave_profiles {
        let brave_config = config.apps.get("brave-browser");
        let binary = brave_config
            .and_then(|c| c.binary.clone())
            .unwrap_or_else(|| "brave".to_string());
        let default_ws = brave_config.and_then(|c| c.default_workspace).unwrap_or(1);
        let profile_ws = brave_config.and_then(|c| c.profile_workspaces.as_ref());

        if !dry_run && which::which(&binary).is_err() {
            let msg = format!("SKIP: binary '{}' not found for Brave profiles", binary);
            report.details.push(msg);
            report.skipped += session.brave_profiles.len();
        } else {
            for profile in &session.brave_profiles {
                let ws = profile_ws
                    .and_then(|m| m.get(&profile.directory))
                    .copied()
                    .unwrap_or(default_ws);

                if dry_run {
                    report.details.push(format!(
                        "[dry-run] brave profile \"{}\" ({}) → ws={}",
                        profile.name, profile.directory, ws
                    ));
                    report.details.push(format!(
                        "  {} --profile-directory={}",
                        binary, profile.directory
                    ));
                    report.details.push(format!(
                        "  hyprctl dispatch movetoworkspacesilent {},address:0xNEW",
                        ws
                    ));
                    report.restored += 1;
                    continue;
                }

                // Snapshot existing addresses BEFORE spawning (avoid race condition).
                let before: HashSet<String> = hyprctl
                    .get_clients()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| c.address)
                    .collect();

                // Launch brave with profile directory.
                let spawn_result = Command::new(&binary)
                    .arg(format!("--profile-directory={}", profile.directory))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();

                match spawn_result {
                    Ok(_) => {
                        let timeout =
                            Duration::from_millis(config.general.window_detect_timeout_ms);
                        let poll_interval = Duration::from_millis(100);
                        let start = Instant::now();

                        let new_addr = loop {
                            if start.elapsed() > timeout {
                                report.details.push(format!(
                                    "FAIL: timeout waiting for brave profile \"{}\"",
                                    profile.name
                                ));
                                report.failed += 1;
                                break None;
                            }
                            thread::sleep(poll_interval);

                            if let Ok(current) = hyprctl.get_clients() {
                                if let Some(w) = current.into_iter().find(|c| {
                                    !before.contains(&c.address) && c.class == "brave-browser"
                                }) {
                                    break Some(w.address);
                                }
                            }
                        };

                        if let Some(addr) = new_addr {
                            // Move to target workspace (no pixel positioning for Brave).
                            let _ = hyprctl.dispatch(&format!(
                                "movetoworkspacesilent {},address:{}",
                                ws, addr
                            ));

                            if verbose {
                                report.details.push(format!(
                                    "OK: brave profile \"{}\" ({}) → ws={}",
                                    profile.name, profile.directory, ws
                                ));
                            }
                            report.restored += 1;
                        }

                        // Throttle between launches.
                        thread::sleep(Duration::from_millis(config.general.restore_delay_ms));
                    }
                    Err(e) => {
                        report.details.push(format!(
                            "FAIL: brave profile \"{}\" — spawn error: {}",
                            profile.name, e
                        ));
                        report.failed += 1;
                    }
                }
            }
        }
    }

    Ok(report)
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
    let observed = observe_clients(hyprctl, process_info, config)?;
    let target_clients: Vec<SessionClient> =
        targets.iter().map(|target| target.client.clone()).collect();
    let plan = plan_reconciliation(&target_clients, &observed);

    let mut report = ReconcileReport::default();
    let mut used_current = HashSet::new();

    for (target_index, target) in targets.iter().enumerate() {
        if let Some(pair) = plan[target_index] {
            let current = &observed[pair.current_index];
            used_current.insert(pair.current_index);
            report.matched += 1;

            let commands = build_reconcile_dispatch_commands(
                &target.client,
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

        let launch_command = build_launch_command(&target.client);
        if dry_run {
            report.launched += 1;
            report.details.push(format!(
                "[dry-run] missing {} → {}",
                target.label,
                launch_command.join(" ")
            ));
            for command in build_dispatch_commands(&target.client) {
                report.details.push(format!("  hyprctl dispatch {command}"));
            }
            continue;
        }

        if which::which(&launch_command[0]).is_err() {
            report.skipped += 1;
            report.details.push(format!(
                "SKIP: binary '{}' not found for {}",
                launch_command[0], target.label
            ));
            continue;
        }

        match restore_single_client_with_launcher(&target.client, hyprctl, config, launcher) {
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
        !session.brave_profiles.is_empty() && config.apps.contains_key("brave-browser");

    let mut targets: Vec<ReconcileTarget> = session
        .clients
        .iter()
        .filter(|client| !(has_brave_profiles && client.class == "brave-browser"))
        .cloned()
        .map(|client| ReconcileTarget {
            label: format!("{} '{}'", client.class, client.title),
            client,
        })
        .collect();

    if has_brave_profiles {
        let brave_config = config.apps.get("brave-browser");
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
            .filter(|client| client.class == "brave-browser")
            .cloned()
            .collect();
        brave_clients.sort_by(|left, right| {
            left.workspace
                .cmp(&right.workspace)
                .then(left.at[1].cmp(&right.at[1]))
                .then(left.at[0].cmp(&right.at[0]))
        });

        for (index, profile) in session.brave_profiles.iter().enumerate() {
            let mut client = brave_clients
                .get(index)
                .cloned()
                .or_else(|| brave_clients.first().cloned())
                .unwrap_or_else(|| SessionClient {
                    class: "brave-browser".to_string(),
                    title: profile.name.clone(),
                    initial_class: "brave-browser".to_string(),
                    initial_title: "Brave".to_string(),
                    workspace: default_workspace,
                    monitor: String::new(),
                    at: [0, 0],
                    size: [1280, 800],
                    floating: false,
                    fullscreen: 0,
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
    let monitor_names: HashMap<i32, String> = hyprctl
        .get_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|monitor| (monitor.id, monitor.name))
        .collect();

    Ok(hyprctl
        .get_clients()?
        .into_iter()
        .filter(|client| {
            !config.filters.ignore_classes.contains(&client.class)
                && !config
                    .filters
                    .ignore_classes
                    .contains(&client.initial_class)
        })
        .map(|client| {
            let monitor_name = monitor_names.get(&client.monitor).cloned();
            let cwd = observe_cwd(&client, process_info);
            ObservedClient::from_hypr_client(client, monitor_name, cwd)
        })
        .collect())
}

fn observe_cwd(client: &HyprClient, process_info: &dyn ProcessInfoProvider) -> Option<PathBuf> {
    const SKIP_COMMANDS: &[&str] = &["kitten", "/usr/bin/kitten"];

    process_info
        .get_children(client.pid)
        .ok()
        .and_then(|children| {
            children
                .into_iter()
                .filter(|child| !child.cwd.as_os_str().is_empty())
                .find(|child| {
                    !SKIP_COMMANDS
                        .iter()
                        .any(|skip| child.cmdline.starts_with(skip))
                })
                .map(|child| child.cwd)
        })
        .or_else(|| process_info.get_cwd(client.pid).ok())
}

fn match_kind_label(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::ExactIdentity => "exact identity",
        MatchKind::AppIdentity => "app identity",
        MatchKind::ClassFallback => "class fallback",
    }
}

/// Return only the compositor operations needed to make an existing window
/// agree with the saved placement.  An empty result is the important fast
/// path: it means the window is already correct and should be left alone.
pub fn build_reconcile_dispatch_commands(
    target: &SessionClient,
    current: &HyprClient,
    current_monitor: Option<&str>,
) -> Vec<String> {
    let monitor_mismatch = !target.monitor.is_empty()
        && current_monitor
            .map(|monitor| monitor != target.monitor)
            .unwrap_or(false);
    let workspace_mismatch = current.workspace.id != target.workspace || monitor_mismatch;
    let leaving_fullscreen = current.fullscreen > 0 && target.fullscreen == 0;
    let entering_or_changing_fullscreen =
        target.fullscreen > 0 && current.fullscreen != target.fullscreen;

    let mut commands = Vec::new();
    if leaving_fullscreen {
        commands.push(format!("fullscreenstate 0 0,address:{}", current.address));
    }
    if workspace_mismatch {
        commands.push(format!(
            "movetoworkspacesilent {},address:{}",
            target.workspace, current.address
        ));
    }
    if current.floating != target.floating {
        commands.push(format!("togglefloating address:{}", current.address));
    }

    if target.fullscreen == 0 {
        if current.size != target.size {
            commands.push(format!(
                "resizewindowpixel exact {} {},address:{}",
                target.size[0], target.size[1], current.address
            ));
        }
        if current.at != target.at {
            commands.push(format!(
                "movewindowpixel exact {} {},address:{}",
                target.at[0], target.at[1], current.address
            ));
        }
    }

    if entering_or_changing_fullscreen {
        commands.push(format!(
            "fullscreenstate {} {},address:{}",
            target.fullscreen, target.fullscreen, current.address
        ));
    }

    commands
}

// ── Per-client restore logic ────────────────────────────────────────────────

fn restore_single_client(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
    config: &Config,
    _verbose: bool,
) -> Result<String, RestoreError> {
    let launcher = RealProcessLauncher;
    restore_single_client_with_launcher(client, hyprctl, config, &launcher)
}

fn restore_single_client_with_launcher(
    client: &SessionClient,
    hyprctl: &dyn HyprctlClient,
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
    launcher
        .spawn(&launch_cmd[0], &launch_cmd[1..])
        .map_err(|e| {
            HyprctlError::CommandFailed(format!("spawn '{}' failed: {e}", launch_cmd[0]))
        })?;

    // 3. Poll for the new window (address not in snapshot + class match).
    let timeout = Duration::from_millis(config.general.window_detect_timeout_ms);
    let poll_interval = Duration::from_millis(100);
    let start = Instant::now();

    let new_addr = loop {
        if start.elapsed() > timeout {
            return Err(RestoreError::Hyprctl(HyprctlError::CommandFailed(format!(
                "timeout waiting for '{}' window to appear",
                client.class
            ))));
        }
        thread::sleep(poll_interval);

        let current = hyprctl.get_clients()?;
        if let Some(w) = current
            .into_iter()
            .find(|c| !before.contains(&c.address) && classes_match(client, c))
        {
            break w.address;
        }
    };

    // 4. Move to target workspace (silently, without switching).
    hyprctl.dispatch(&format!(
        "movetoworkspacesilent {},address:{}",
        client.workspace, new_addr
    ))?;

    // 5. Resize then position (order matters: resize first, then move).
    hyprctl.dispatch(&format!(
        "resizewindowpixel exact {} {},address:{}",
        client.size[0], client.size[1], new_addr
    ))?;
    hyprctl.dispatch(&format!(
        "movewindowpixel exact {} {},address:{}",
        client.at[0], client.at[1], new_addr
    ))?;

    // 6. Apply floating / fullscreen state.
    if client.floating {
        hyprctl.dispatch(&format!("togglefloating address:{}", new_addr))?;
    }
    if client.fullscreen > 0 {
        hyprctl.dispatch(&format!(
            "fullscreenstate {} {},address:{}",
            client.fullscreen, client.fullscreen, new_addr
        ))?;
    }

    // 7. Throttle subsequent launches to give the compositor time to settle.
    thread::sleep(Duration::from_millis(config.general.restore_delay_ms));

    Ok(format!(
        "OK: {} → ws={} at {:?}",
        client.class, client.workspace, client.at
    ))
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

    if client.class == "kitty" {
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
            || client.launch.command == client.class
            || client.launch.command == client.initial_class)
    {
        "ghostty".to_string()
    } else {
        client.launch.command.clone()
    }
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

    let mut cmds = vec![
        format!("exec {}", launch.join(" ")),
        format!("movetoworkspacesilent {},{}", client.workspace, addr),
        format!(
            "resizewindowpixel exact {} {},{}",
            client.size[0], client.size[1], addr
        ),
        format!(
            "movewindowpixel exact {} {},{}",
            client.at[0], client.at[1], addr
        ),
    ];

    if client.floating {
        cmds.push(format!("togglefloating {addr}"));
    }
    if client.fullscreen > 0 {
        cmds.push(format!(
            "fullscreenstate {} {},{}",
            client.fullscreen, client.fullscreen, addr
        ));
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
            monitor: "DP-1".to_string(),
            at,
            size,
            floating,
            fullscreen,
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

    #[derive(Default)]
    struct RecordingLauncher {
        launches: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl ProcessLauncher for RecordingLauncher {
        fn spawn(&self, command: &str, args: &[String]) -> Result<(), std::io::Error> {
            self.launches
                .borrow_mut()
                .push((command.to_string(), args.to_vec()));
            Ok(())
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
        let config = Config::default();
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
                .any(|d| d.contains("SKIP: kitty already on ws=1")),
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
        let config = Config::default();

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

        let report = reconcile_session_with_launcher(
            &make_session(vec![existing_target, missing_target]),
            &mock,
            &EmptyProcessInfo,
            &Config::default(),
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
        assert_eq!(
            commands[0],
            "movetoworkspacesilent 3,address:0xwrong-monitor"
        );
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
