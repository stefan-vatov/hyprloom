//! Integration tests for the reconciliation planning and dispatch boundary.

#![allow(unused_crate_dependencies)]

use chrono::Utc;
use hyprloom::config::Config;
use hyprloom::hyprctl::{build_batch_command, parse_dispatch_args, HyprClient, HyprMonitor, HyprWorkspace, HyprctlClient, HyprctlError};
use hyprloom::matching::MatchingStrategy;
use hyprloom::process::{ChildProcess, ProcessError, ProcessInfoProvider};
use hyprloom::restore::{build_reconcile_dispatch_commands, reconcile_session_with_launcher_strategy, ProcessLauncher};
use hyprloom::session::{LaunchInfo, Monitor, Session, SessionClient};
use std::cell::RefCell;
use std::path::PathBuf;

struct NoProcessInfo;

impl ProcessInfoProvider for NoProcessInfo {
    fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
        Err(ProcessError::NotFound(pid))
    }

    fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
        Err(ProcessError::NotFound(pid))
    }
}

struct NeverLaunch;

impl ProcessLauncher for NeverLaunch {
    fn spawn(&self, _command: &str, _args: &[String]) -> Result<hyprloom::restore::LaunchedProcess, std::io::Error> {
        Err(std::io::Error::other("the integration scenario should not launch"))
    }
}

struct FixtureHyprctl {
    clients: Vec<HyprClient>,
    monitors: Vec<HyprMonitor>,
    dispatches: RefCell<Vec<String>>,
}

impl FixtureHyprctl {
    fn new(clients: Vec<HyprClient>) -> Self {
        Self {
            clients,
            monitors: vec![HyprMonitor {
                id: 0,
                name: "DP-1".to_string(),
                width: 1920,
                height: 1080,
                transform: 0,
                x: Some(0),
                y: Some(0),
            }],
            dispatches: RefCell::new(Vec::new()),
        }
    }
}

impl HyprctlClient for FixtureHyprctl {
    fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
        Ok(self.clients.clone())
    }

    fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
        Ok(self.monitors.clone())
    }

    fn dispatch(&self, args: &str) -> Result<(), HyprctlError> {
        self.dispatches.borrow_mut().push(args.to_string());
        Ok(())
    }

    fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
        Ok("0.54.1-fixture".to_string())
    }
}

fn target_client() -> SessionClient {
    SessionClient {
        class: "kitty".to_string(),
        title: "Project terminal".to_string(),
        address: Some("0xsaved".to_string()),
        pid: Some(100),
        process_start_time: None,
        stable_id: Some("stable-window-1".to_string()),
        initial_class: "kitty".to_string(),
        initial_title: "kitty".to_string(),
        workspace: 1,
        workspace_name: "1".to_string(),
        monitor: "DP-1".to_string(),
        at: [20, 30],
        size: [800, 600],
        floating: false,
        fullscreen: 0,
        pinned: false,
        profile_directory: None,
        profile_identity_ambiguous: false,
        focus_history_id: 0,
        launch: LaunchInfo {
            command: "kitty".to_string(),
            args: vec![],
            hint: None,
            terminal_shell: None,
        },
    }
}

fn current_client() -> HyprClient {
    HyprClient {
        address: "0xsaved".to_string(),
        class: "kitty".to_string(),
        title: "Project terminal".to_string(),
        stable_id: Some("stable-window-1".to_string()),
        initial_class: "kitty".to_string(),
        initial_title: "kitty".to_string(),
        workspace: HyprWorkspace {
            id: 4,
            name: "4".to_string(),
        },
        monitor: 0,
        at: [500, 300],
        size: [600, 400],
        floating: false,
        fullscreen: 0,
        pinned: false,
        focus_history_id: 0,
        pid: 100,
    }
}

fn session_with_target(target: SessionClient) -> Session {
    Session {
        name: "integration".to_string(),
        created_at: Utc::now(),
        hyprland_version: "0.54.1-fixture".to_string(),
        monitors: vec![Monitor {
            name: "DP-1".to_string(),
            width: 1920,
            height: 1080,
            transform: 0,
            x: Some(0),
            y: Some(0),
        }],
        clients: vec![target],
        brave_profiles: vec![],
    }
}

#[test]
fn reconciliation_repairs_a_realistic_fixture_through_the_dispatch_boundary() {
    let target = target_client();
    let current = current_client();
    let standalone_commands = build_reconcile_dispatch_commands(&target, &current, Some("DP-1"));
    assert!(!standalone_commands.is_empty());
    for command in &standalone_commands {
        parse_dispatch_args(command).expect("generated dispatch must have valid argv syntax");
    }
    let batch = build_batch_command(&standalone_commands).expect("generated batch must be valid");
    assert!(batch.contains("dispatch movetoworkspacesilent 1,address:0xsaved"));

    let hyprctl = FixtureHyprctl::new(vec![current]);
    let report = reconcile_session_with_launcher_strategy(
        &session_with_target(target),
        &hyprctl,
        &NoProcessInfo,
        &Config::default(),
        false,
        true,
        &NeverLaunch,
        MatchingStrategy::Greedy,
    )
    .expect("a matched window should be repaired without launching");

    assert_eq!(report.matched, 1);
    assert_eq!(report.moved, 1);
    assert_eq!(report.launched, 0);
    assert_eq!(hyprctl.dispatches.borrow().as_slice(), standalone_commands.as_slice());
    build_batch_command(&hyprctl.dispatches.borrow()).expect("recorded dispatches must batch");
}

#[test]
fn reconciliation_leaves_an_already_correct_window_untouched() {
    let mut target = target_client();
    target.workspace = 4;
    target.workspace_name = "4".to_string();
    target.at = [500, 300];
    target.size = [600, 400];
    let current = current_client();
    let hyprctl = FixtureHyprctl::new(vec![current]);

    let report = reconcile_session_with_launcher_strategy(
        &session_with_target(target),
        &hyprctl,
        &NoProcessInfo,
        &Config::default(),
        false,
        true,
        &NeverLaunch,
        MatchingStrategy::Greedy,
    )
    .expect("an already-correct window should be a no-op");

    assert_eq!(report.matched, 1);
    assert_eq!(report.unchanged, 1);
    assert_eq!(report.moved, 0);
    assert_eq!(report.launched, 0);
    assert!(hyprctl.dispatches.borrow().is_empty());
}

#[test]
fn malformed_dispatch_commands_are_rejected_before_batching() {
    let commands = vec!["workspace 'unfinished".to_string()];
    assert!(build_batch_command(&commands).is_err());
}
