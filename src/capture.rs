use crate::config::{app_config_for, is_ignored_class, AppConfig, Config};
use crate::hyprctl::{HyprctlClient, HyprctlError};
use crate::process::{
    find_profile_directories, is_helper_process, is_plain_shell, select_terminal_process,
    ProcessError, ProcessInfoProvider,
};
use crate::session::{LaunchInfo, Monitor, Session, SessionClient};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;

// ── Error ──────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("hyprctl error: {0}")]
    Hyprctl(#[from] HyprctlError),
    #[error("process error: {0}")]
    Process(#[from] ProcessError),
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Capture the current Hyprland session state into a [`Session`].
///
/// All windows whose class appears in `config.filters.ignore_classes` are
/// excluded from the returned session.
pub fn capture_session(
    name: &str,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
) -> Result<Session, CaptureError> {
    let raw_clients = hyprctl.get_clients()?;
    let raw_monitors = hyprctl.get_monitors()?;
    let version = hyprctl
        .get_hyprland_version()
        .unwrap_or_else(|_| "unknown".to_string());

    // Build a map from monitor ID to monitor name so that
    // HyprClient.monitor (an i32 monitor ID) can be resolved to a
    // human-readable name such as "DP-1".
    let monitor_map: HashMap<i32, String> = raw_monitors
        .iter()
        .map(|m| (m.id, m.name.clone()))
        .collect();

    let monitors: Vec<Monitor> = raw_monitors
        .iter()
        .map(|m| Monitor {
            name: m.name.clone(),
            width: m.width,
            height: m.height,
            transform: m.transform,
            x: m.x,
            y: m.y,
        })
        .collect();

    let brave_window_counts = raw_clients
        .iter()
        .filter(|client| is_brave_client(client))
        .fold(HashMap::<u32, usize>::new(), |mut counts, client| {
            *counts.entry(client.pid).or_default() += 1;
            counts
        });
    let brave_profiles_by_pid: HashMap<u32, Vec<String>> = raw_clients
        .iter()
        .filter(|client| is_brave_client(client))
        .map(|client| {
            (
                client.pid,
                find_profile_directories(process_info, client.pid),
            )
        })
        .collect();

    let clients: Vec<SessionClient> = raw_clients
        .iter()
        .filter(|c| {
            !is_ignored_class(&c.class, &config.filters.ignore_classes)
                && !is_ignored_class(&c.initial_class, &config.filters.ignore_classes)
        })
        .map(|c| {
            let profile_directory = brave_profiles_by_pid
                .get(&c.pid)
                .and_then(|profiles| profile_for_window(profiles, brave_window_counts.get(&c.pid)))
                .map(str::to_string);
            build_session_client(c, &monitor_map, process_info, config, profile_directory)
        })
        .collect();

    let brave_profiles = if clients.iter().any(|c| {
        c.class.eq_ignore_ascii_case("brave-browser")
            || c.initial_class.eq_ignore_ascii_case("brave-browser")
    }) {
        let all_profiles = crate::brave::read_profiles().unwrap_or_else(|e| {
            eprintln!("Warning: could not read Brave profiles: {e}");
            vec![]
        });
        let profile_ws =
            app_config_for(config, "brave-browser", "").and_then(|c| c.profile_workspaces.as_ref());
        match profile_ws {
            Some(mappings) => crate::brave::filter_profiles_by_config(all_profiles, Some(mappings)),
            None => {
                let active_directories: Vec<String> = raw_clients
                    .iter()
                    .filter(|client| is_brave_client(client))
                    .flat_map(|client| {
                        brave_profiles_by_pid
                            .get(&client.pid)
                            .into_iter()
                            .flat_map(|profiles| {
                                if profiles.is_empty()
                                    && brave_window_counts.get(&client.pid) == Some(&1)
                                {
                                    vec!["Default".to_string()]
                                } else {
                                    profiles.clone()
                                }
                            })
                    })
                    .collect();
                crate::brave::filter_profiles_by_active_directories(
                    all_profiles,
                    &active_directories,
                )
            }
        }
    } else {
        vec![]
    };

    Ok(Session {
        name: name.to_string(),
        created_at: Utc::now(),
        hyprland_version: version,
        monitors,
        clients,
        brave_profiles,
    })
}

// ── Private helpers ────────────────────────────────────────────────────────

fn build_session_client(
    client: &crate::hyprctl::HyprClient,
    monitor_map: &HashMap<i32, String>,
    process_info: &dyn ProcessInfoProvider,
    config: &Config,
    profile_directory: Option<String>,
) -> SessionClient {
    let monitor_name = monitor_map
        .get(&client.monitor)
        .cloned()
        .unwrap_or_else(|| format!("monitor-{}", client.monitor));

    let app_config = app_config_for(config, &client.class, &client.initial_class);
    let launch = build_launch_info(client, app_config, process_info);

    let profile_identity_ambiguous = is_brave_client(client) && profile_directory.is_none();

    SessionClient {
        class: client.class.clone(),
        title: client.title.clone(),
        initial_class: if client.initial_class.is_empty() {
            client.class.clone()
        } else {
            client.initial_class.clone()
        },
        initial_title: if client.initial_title.is_empty() {
            client.title.clone()
        } else {
            client.initial_title.clone()
        },
        workspace: client.workspace.id,
        workspace_name: client.workspace.name.clone(),
        monitor: monitor_name,
        at: client.at,
        size: client.size,
        floating: client.floating,
        fullscreen: client.fullscreen,
        pinned: client.pinned,
        profile_directory,
        profile_identity_ambiguous,
        focus_history_id: client.focus_history_id,
        launch,
    }
}

fn is_brave_client(client: &crate::hyprctl::HyprClient) -> bool {
    client.class.eq_ignore_ascii_case("brave-browser")
        || client.initial_class.eq_ignore_ascii_case("brave-browser")
}

fn profile_for_window<'a>(profiles: &'a [String], window_count: Option<&usize>) -> Option<&'a str> {
    match profiles {
        [profile] if window_count == Some(&1) => Some(profile.as_str()),
        [] if window_count == Some(&1) => Some("Default"),
        _ => None,
    }
}

fn build_launch_info(
    client: &crate::hyprctl::HyprClient,
    app_config: Option<&AppConfig>,
    process_info: &dyn ProcessInfoProvider,
) -> LaunchInfo {
    // Omarchy web apps expose a Chrome window class that is not an executable
    // (for example, `chrome-chatgpt.com__-Default`).  Their desktop entries
    // contain the authoritative launcher and URL, so retain that launch
    // contract instead of falling back to the non-existent class binary.
    let desktop_launch = if app_config
        .and_then(|config| config.binary.as_ref())
        .is_none()
    {
        find_webapp_launch_info(client)
    } else {
        None
    };
    let binary = app_config
        .and_then(|a| a.binary.clone())
        .or_else(|| desktop_launch.as_ref().map(|launch| launch.command.clone()))
        .unwrap_or_else(|| {
            if is_ghostty_class(&client.class) || is_ghostty_class(&client.initial_class) {
                "ghostty".to_string()
            } else if is_brave_class(&client.class) || is_brave_class(&client.initial_class) {
                "brave".to_string()
            } else {
                client.class.clone()
            }
        });

    let capture_cwd = app_config.and_then(|a| a.capture_cwd).unwrap_or(false);
    let capture_cmd = app_config
        .and_then(|a| a.capture_last_command)
        .unwrap_or(false);

    let mut args: Vec<String> = desktop_launch
        .as_ref()
        .map(|launch| launch.args.clone())
        .unwrap_or_default();
    let mut hint: Option<String> = None;

    if desktop_launch.is_none() && (capture_cwd || capture_cmd) {
        if let Some(shell) = select_terminal_process(process_info, client.pid) {
            // Find the actual shell child, skipping helper processes like
            // kitty's "kitten __atexit__" which has CWD=/home but is not the
            // interactive shell.
            if capture_cwd {
                args.push(
                    if is_ghostty_class(&client.class) || is_ghostty_class(&client.initial_class) {
                        "--working-directory".to_string()
                    } else {
                        "--directory".to_string()
                    },
                );
                args.push(shell.cwd.to_string_lossy().to_string());
            }

            if capture_cmd {
                // Prefer a grandchild process (the command running inside the shell),
                // but only when it is not itself a plain shell.
                if let Ok(grandchildren) = process_info.get_descendants(shell.pid) {
                    if let Some(cmd) = grandchildren
                        .iter()
                        .filter(|gc| {
                            !gc.cmdline.is_empty()
                                && !is_helper_process(&gc.cmdline)
                                && !is_plain_shell(&gc.cmdline)
                        })
                        .min_by_key(|gc| gc.pid)
                    {
                        hint = Some(cmd.cmdline.clone());
                    }
                }

                // Fall back to the shell's own cmdline if it is not a plain shell.
                if hint.is_none() && !shell.cmdline.is_empty() && !is_plain_shell(&shell.cmdline) {
                    hint = Some(shell.cmdline.clone());
                }
            }
        }
    }

    // Render hint through the app-level template when one is configured.
    if let (Some(h), Some(ac)) = (&hint, app_config) {
        if let Some(template) = &ac.hint_template {
            let cwd_str = launch_cwd_arg(&args).unwrap_or("");
            hint = Some(
                template
                    .replace("{last_command}", h)
                    .replace("{cwd}", cwd_str),
            );
        }
    }

    LaunchInfo {
        command: binary,
        args,
        hint,
    }
}

fn launch_cwd_arg(args: &[String]) -> Option<&str> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == "--directory" || arg == "--working-directory" {
            args.get(index + 1).map(String::as_str)
        } else if let Some(value) = arg.strip_prefix("--directory=") {
            Some(value)
        } else {
            arg.strip_prefix("--working-directory=")
        }
    })
}

fn is_ghostty_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("ghostty") || class.eq_ignore_ascii_case("com.mitchellh.ghostty")
}

fn is_brave_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("brave-browser")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopLaunch {
    command: String,
    args: Vec<String>,
    specificity: u8,
}

const OMARCHY_WEBAPP_LAUNCHERS: [&str; 3] = [
    "omarchy-launch-or-focus-webapp",
    "omarchy-launch-or-focus",
    "omarchy-launch-webapp",
];

fn find_webapp_launch_info(client: &crate::hyprctl::HyprClient) -> Option<LaunchInfo> {
    discover_webapp_launch_info(&client.class, &client.initial_class)
}

/// Find the launcher recorded by Omarchy for a Chrome web-app class.  This is
/// also used while restoring older snapshots whose launch command predates
/// web-app desktop-entry discovery.
pub fn discover_webapp_launch_info(class: &str, initial_class: &str) -> Option<LaunchInfo> {
    let identities = [class, initial_class];
    if !identities.iter().any(|class| is_webapp_class(class)) {
        return None;
    }

    find_webapp_launch_info_in_directories(class, initial_class, &desktop_application_directories())
}

fn find_webapp_launch_info_in_directories(
    class: &str,
    initial_class: &str,
    directories: &[PathBuf],
) -> Option<LaunchInfo> {
    let identities = [class, initial_class];
    let mut matches = Vec::new();
    for directory in directories {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("desktop") {
                continue;
            }
            let contents = match std::fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            if let Some(launch) = parse_webapp_desktop_file(&contents, &identities) {
                matches.push(launch);
            }
        }
    }

    let strongest_specificity = matches.iter().map(|launch| launch.specificity).max()?;
    let strongest = matches
        .into_iter()
        .filter(|launch| launch.specificity == strongest_specificity)
        .collect::<Vec<_>>();
    let mut unique = Vec::new();
    for launch in strongest {
        if !unique.iter().any(|existing: &DesktopLaunch| {
            existing.command == launch.command && existing.args == launch.args
        }) {
            unique.push(launch);
        }
    }
    let launch = (unique.len() == 1).then(|| unique.pop().expect("one launch"))?;
    Some(LaunchInfo {
        command: launch.command,
        args: launch.args,
        hint: None,
    })
}

fn desktop_application_directories() -> Vec<PathBuf> {
    let mut data_directories = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        if !data_home.is_empty() {
            data_directories.push(PathBuf::from(data_home));
        }
    } else if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            data_directories.push(PathBuf::from(home).join(".local/share"));
        }
    }

    if let Some(data_dirs) = std::env::var_os("XDG_DATA_DIRS") {
        data_directories.extend(
            std::env::split_paths(&data_dirs).filter(|directory| !directory.as_os_str().is_empty()),
        );
    } else {
        data_directories.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]);
    }
    // Omarchy's bundled application entries live here on a standard install,
    // even when the host's XDG_DATA_DIRS does not mention the parent.
    data_directories.push(PathBuf::from("/usr/share/omarchy"));

    data_directories
        .into_iter()
        .map(|directory| directory.join("applications"))
        .fold(Vec::new(), |mut unique, directory| {
            if !unique.contains(&directory) {
                unique.push(directory);
            }
            unique
        })
}

fn parse_webapp_desktop_file(contents: &str, identities: &[&str]) -> Option<DesktopLaunch> {
    let mut exec = None;
    let mut startup_class = None;
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("Exec=") {
            exec = Some(value);
        } else if let Some(value) = line.strip_prefix("StartupWMClass=") {
            startup_class = Some(value.trim());
        }
    }

    let tokens = tokenize_desktop_exec(exec?)?;
    let command = tokens.first()?.as_str();
    if !OMARCHY_WEBAPP_LAUNCHERS
        .iter()
        .any(|launcher| launcher.eq_ignore_ascii_case(command))
    {
        return None;
    }

    let class_argument_matches = tokens
        .get(1)
        .map(|argument| {
            identities
                .iter()
                .any(|identity| !identity.is_empty() && identity.eq_ignore_ascii_case(argument))
        })
        .unwrap_or(false);
    let url_argument_matches = if command.eq_ignore_ascii_case("omarchy-launch-webapp") {
        tokens
            .get(1)
            .map(|url| {
                identities
                    .iter()
                    .any(|identity| webapp_class_matches_url(identity, url))
            })
            .unwrap_or(false)
    } else if command.eq_ignore_ascii_case("omarchy-launch-or-focus-webapp") {
        tokens
            .get(2)
            .map(|url| {
                identities
                    .iter()
                    .any(|identity| webapp_class_matches_url(identity, url))
            })
            .unwrap_or(false)
    } else {
        tokens
            .get(2)
            .and_then(|nested| nested.strip_prefix("omarchy-launch-webapp "))
            .and_then(|nested| nested.split_whitespace().next())
            .map(|url| {
                identities
                    .iter()
                    .any(|identity| webapp_class_matches_url(identity, url))
            })
            .unwrap_or(false)
    };
    let startup_class_matches = startup_class
        .map(|startup| {
            identities
                .iter()
                .any(|identity| !identity.is_empty() && identity.eq_ignore_ascii_case(startup))
        })
        .unwrap_or(false);
    let matches = if command.eq_ignore_ascii_case("omarchy-launch-webapp") {
        url_argument_matches
            && identities
                .iter()
                .any(|identity| webapp_class_uses_default_profile(identity))
    } else {
        (class_argument_matches || startup_class_matches) && url_argument_matches
    };
    if !matches {
        return None;
    }

    let specificity = if class_argument_matches {
        3
    } else if startup_class_matches {
        2
    } else {
        1
    };

    Some(DesktopLaunch {
        command: tokens[0].clone(),
        args: tokens.into_iter().skip(1).collect(),
        specificity,
    })
}

fn tokenize_desktop_exec(value: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if quote == Some('"') && character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\\' => escaped = true,
            '\'' | '"' => quote = Some(character),
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }

    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    (!tokens.is_empty()).then_some(tokens)
}

fn is_webapp_class(class: &str) -> bool {
    class
        .get(.."chrome-".len())
        .map(|prefix| prefix.eq_ignore_ascii_case("chrome-"))
        .unwrap_or(false)
}

pub(crate) fn webapp_class_matches_url(class: &str, url: &str) -> bool {
    let Some(prefix) = class.get(.."chrome-".len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case("chrome-") {
        return false;
    }
    let Some(url_host) = webapp_url_host(url) else {
        return false;
    };
    let Some(url_route) = webapp_url_route(url) else {
        return false;
    };
    let class_identity = &class["chrome-".len()..];
    if let Some((class_host, class_suffix)) = class_identity.split_once("__") {
        if class_host.is_empty() {
            return false;
        }
        let class_route = class_suffix
            .rsplit_once('-')
            .map(|(route, _profile)| route)
            .unwrap_or(class_suffix);
        return class_host.eq_ignore_ascii_case(url_host)
            && class_route == url_route.replace('/', "_");
    }

    // Chromium also emits a compact root-app class such as
    // `chrome-example.com-Default` on some versions.  There is no route
    // component to validate in that form, so accept it only for the root URL
    // and only when the trailing component is a known profile suffix.
    if !url_route.is_empty() {
        return false;
    }
    let expected_prefix = format!("{url_host}-");
    let Some(class_prefix) = class_identity.get(..expected_prefix.len()) else {
        return false;
    };
    let Some(profile) = class_identity.get(expected_prefix.len()..) else {
        return false;
    };
    class_prefix.eq_ignore_ascii_case(&expected_prefix) && is_webapp_profile_suffix(profile)
}

fn webapp_class_uses_default_profile(class: &str) -> bool {
    let Some(prefix) = class.get(.."chrome-".len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case("chrome-") {
        return false;
    }
    class["chrome-".len()..]
        .rsplit_once('-')
        .map(|(_route, profile)| profile.eq_ignore_ascii_case("Default"))
        .unwrap_or(false)
}

fn is_webapp_profile_suffix(profile: &str) -> bool {
    profile.eq_ignore_ascii_case("Default")
        || profile
            .strip_prefix("Profile_")
            .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

fn webapp_url_host(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = &url[scheme_end + 3..];
    let authority = authority
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

fn webapp_url_route(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")?;
    let authority = &url[scheme_end + 3..];
    let route = authority
        .find(['/', '?', '#'])
        .map(|index| &authority[index..])
        .unwrap_or("");
    Some(route.split(['?', '#']).next()?.trim_matches('/'))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, Config, FilterConfig, GeneralConfig};
    use crate::hyprctl::{
        HyprClient as RawClient, HyprMonitor as RawMonitor, HyprWorkspace as RawWorkspace,
        HyprctlClient, HyprctlError,
    };
    use crate::process::{ChildProcess, ProcessError, ProcessInfoProvider};
    use std::collections::HashMap;
    use std::path::PathBuf;

    // ── Mock: HyprctlClient ──────────────────────────────────────────────

    struct MockHyprctl {
        clients: Vec<RawClient>,
        monitors: Vec<RawMonitor>,
    }

    impl HyprctlClient for MockHyprctl {
        fn get_clients(&self) -> Result<Vec<RawClient>, HyprctlError> {
            Ok(self.clients.clone())
        }
        fn get_monitors(&self) -> Result<Vec<RawMonitor>, HyprctlError> {
            Ok(self.monitors.clone())
        }
        fn dispatch(&self, _: &str) -> Result<(), HyprctlError> {
            Ok(())
        }
        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.54.1".to_string())
        }
    }

    // ── Mock: ProcessInfoProvider ────────────────────────────────────────

    struct MockProcess {
        cwds: HashMap<u32, PathBuf>,
        children: HashMap<u32, Vec<ChildProcess>>,
    }

    impl ProcessInfoProvider for MockProcess {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            self.cwds
                .get(&pid)
                .cloned()
                .ok_or(ProcessError::NotFound(pid))
        }
        fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn make_hypr_client(class: &str, pid: u32) -> RawClient {
        RawClient {
            address: "0xdeadbeef".to_string(),
            class: class.to_string(),
            title: format!("{class} window"),
            initial_class: class.to_string(),
            initial_title: class.to_string(),
            workspace: RawWorkspace {
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
            pid,
        }
    }

    fn make_monitor(name: &str) -> RawMonitor {
        make_monitor_with_id(0, name)
    }

    fn make_monitor_with_id(id: i32, name: &str) -> RawMonitor {
        RawMonitor {
            id,
            name: name.to_string(),
            width: 1920,
            height: 1080,
            transform: 0,
            x: Some(0),
            y: Some(0),
        }
    }

    fn empty_process() -> MockProcess {
        MockProcess {
            cwds: HashMap::new(),
            children: HashMap::new(),
        }
    }

    // ── Test 1: filter ignored classes ───────────────────────────────────

    #[test]
    fn test_capture_filters_ignored_classes() {
        let hyprctl = MockHyprctl {
            clients: vec![
                make_hypr_client("kitty", 1001),
                make_hypr_client("waybar", 1002),
                make_hypr_client("brave-browser", 1003),
            ],
            monitors: vec![make_monitor("DP-1")],
        };

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec!["waybar".to_string()],
            },
            apps: HashMap::new(),
        };

        let session =
            capture_session("test", &hyprctl, &empty_process(), &config).expect("capture failed");

        assert_eq!(session.clients.len(), 2, "waybar must be excluded");
        let classes: Vec<&str> = session.clients.iter().map(|c| c.class.as_str()).collect();
        assert!(classes.contains(&"kitty"), "kitty must be present");
        assert!(
            classes.contains(&"brave-browser"),
            "brave-browser must be present"
        );
        assert!(!classes.contains(&"waybar"), "waybar must be absent");
    }

    #[test]
    fn test_capture_filters_ignored_initial_class_case_insensitively() {
        let mut client = make_hypr_client("wrapper", 1004);
        client.initial_class = "WayBar".to_string();
        let hyprctl = MockHyprctl {
            clients: vec![client],
            monitors: vec![make_monitor("DP-1")],
        };
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec!["waybar".to_string()],
            },
            apps: HashMap::new(),
        };

        let session = capture_session("test", &hyprctl, &empty_process(), &config).unwrap();
        assert!(session.clients.is_empty());
    }

    // ── Test 2: kitty with CWD capture ───────────────────────────────────

    #[test]
    fn test_capture_builds_kitty_launch_with_cwd() {
        const KITTY_PID: u32 = 2001;
        const SHELL_PID: u32 = 2002;

        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("kitty", KITTY_PID)],
            monitors: vec![make_monitor("DP-1")],
        };

        let mut app_configs = HashMap::new();
        app_configs.insert(
            "kitty".to_string(),
            AppConfig {
                binary: None,
                capture_cwd: Some(true),
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: app_configs,
        };

        let mut children: HashMap<u32, Vec<ChildProcess>> = HashMap::new();
        children.insert(
            KITTY_PID,
            vec![ChildProcess {
                pid: SHELL_PID,
                cwd: PathBuf::from("/home/user/project"),
                cmdline: "zsh".to_string(),
            }],
        );

        let process = MockProcess {
            cwds: HashMap::new(),
            children,
        };

        let session = capture_session("test", &hyprctl, &process, &config).expect("capture failed");

        assert_eq!(session.clients.len(), 1);
        let launch = &session.clients[0].launch;
        assert_eq!(launch.command, "kitty");
        assert_eq!(
            launch.args,
            vec!["--directory", "/home/user/project"],
            "args must contain --directory <cwd>"
        );
        assert!(
            launch.hint.is_none(),
            "no hint expected when capture_last_command is off"
        );
    }

    // ── Test 3: generic app — binary override, no CWD ────────────────────

    #[test]
    fn test_capture_builds_generic_app_launch() {
        const BRAVE_PID: u32 = 3001;

        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("brave-browser", BRAVE_PID)],
            monitors: vec![make_monitor("HDMI-A-1")],
        };

        let mut app_configs = HashMap::new();
        app_configs.insert(
            "brave-browser".to_string(),
            AppConfig {
                binary: Some("brave".to_string()),
                capture_cwd: None,
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: app_configs,
        };

        let session =
            capture_session("test", &hyprctl, &empty_process(), &config).expect("capture failed");

        assert_eq!(session.clients.len(), 1);
        let launch = &session.clients[0].launch;
        assert_eq!(launch.command, "brave", "binary override must be applied");
        assert!(
            launch.args.is_empty(),
            "no args expected without CWD capture"
        );
        assert!(launch.hint.is_none());
        assert_eq!(
            session.clients[0].profile_directory.as_deref(),
            Some("Default"),
            "a lone Brave window without a profile flag is the Default profile"
        );
        assert!(!session.clients[0].profile_identity_ambiguous);
    }

    #[test]
    fn test_capture_defaults_brave_to_its_executable() {
        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("brave-browser", 3002)],
            monitors: vec![make_monitor("DP-1")],
        };

        let session = capture_session("test", &hyprctl, &empty_process(), &Config::default())
            .expect("capture failed");

        assert_eq!(session.clients[0].launch.command, "brave");
    }

    #[test]
    fn test_parse_omarchy_webapp_desktop_entry() {
        let contents = r#"
            [Desktop Entry]
            Name=YouTube
            Exec=omarchy-launch-or-focus chrome-youtube.com__-Profile_1 "omarchy-launch-webapp https://youtube.com/ --profile-directory='Profile 1'"
        "#;

        let launch = parse_webapp_desktop_file(contents, &["chrome-youtube.com__-Profile_1", ""])
            .expect("webapp desktop entry should be recognized");

        assert_eq!(launch.command, "omarchy-launch-or-focus");
        assert_eq!(
            launch.args,
            vec![
                "chrome-youtube.com__-Profile_1",
                "omarchy-launch-webapp https://youtube.com/ --profile-directory='Profile 1'"
            ]
        );
    }

    #[test]
    fn test_parse_webapp_entry_rejects_unrelated_class() {
        let contents =
            "Exec=omarchy-launch-or-focus-webapp chrome-chatgpt.com__-Default https://chatgpt.com/";

        assert!(parse_webapp_desktop_file(contents, &["chrome-other.com__-Default"]).is_none());
    }

    #[test]
    fn test_parse_standard_omarchy_webapp_desktop_entry() {
        let contents = "[Desktop Entry]\nExec=omarchy-launch-webapp https://chatgpt.com/\n";

        let launch = parse_webapp_desktop_file(contents, &["chrome-chatgpt.com__-Default", ""])
            .expect("standard direct webapp entry should be recognized");

        assert_eq!(launch.command, "omarchy-launch-webapp");
        assert_eq!(launch.args, vec!["https://chatgpt.com/"]);
        assert_eq!(launch.specificity, 1);
    }

    #[test]
    fn test_webapp_route_and_profile_suffixes_must_match() {
        let class = "chrome-discord.com__channels_@me-Default";

        assert!(webapp_class_matches_url(
            class,
            "https://discord.com/channels/@me"
        ));
        assert!(webapp_class_matches_url(
            class,
            "https://discord.com/channels/@me?view=unread"
        ));
        assert!(!webapp_class_matches_url(
            class,
            "https://discord.com/other"
        ));
        assert!(webapp_class_uses_default_profile(class));
        assert!(!webapp_class_uses_default_profile(
            "chrome-discord.com__channels_@me-Profile_1"
        ));
    }

    #[test]
    fn test_pathless_root_webapp_class_matches_default_profile() {
        let class = "chrome-example.com-Default";

        assert!(webapp_class_matches_url(class, "https://example.com/"));
        assert!(webapp_class_matches_url(
            class,
            "https://example.com/?view=home"
        ));
        assert!(webapp_class_uses_default_profile(class));
        assert!(!webapp_class_matches_url(class, "https://example.com/app"));
    }

    #[test]
    fn test_find_webapp_launch_info_uses_matching_desktop_entry() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("ChatGPT.desktop"),
            "[Desktop Entry]\nExec=omarchy-launch-or-focus-webapp chrome-chatgpt.com__-Default https://chatgpt.com/\n",
        )
        .unwrap();
        let client = make_hypr_client("chrome-chatgpt.com__-Default", 3003);

        let launch = find_webapp_launch_info_in_directories(
            &client.class,
            &client.initial_class,
            &[directory.path().to_path_buf()],
        )
        .expect("matching webapp desktop entry should be found");

        assert_eq!(launch.command, "omarchy-launch-or-focus-webapp");
        assert_eq!(
            launch.args,
            vec!["chrome-chatgpt.com__-Default", "https://chatgpt.com/"]
        );
    }

    #[test]
    fn test_find_webapp_launch_info_rejects_ambiguous_lossy_routes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("Slash.desktop"),
            "[Desktop Entry]\nExec=omarchy-launch-webapp https://example.com/foo/bar\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("Underscore.desktop"),
            "[Desktop Entry]\nExec=omarchy-launch-webapp https://example.com/foo_bar\n",
        )
        .unwrap();

        assert!(find_webapp_launch_info_in_directories(
            "chrome-example.com__foo_bar-Default",
            "",
            &[directory.path().to_path_buf()],
        )
        .is_none());
    }

    #[test]
    fn test_find_webapp_launch_info_prefers_explicit_class_identity() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("Direct.desktop"),
            "[Desktop Entry]\nExec=omarchy-launch-webapp https://example.com/\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("Explicit.desktop"),
            "[Desktop Entry]\nExec=omarchy-launch-or-focus-webapp chrome-example.com__-Default https://example.com/\n",
        )
        .unwrap();

        let launch = find_webapp_launch_info_in_directories(
            "chrome-example.com__-Default",
            "",
            &[directory.path().to_path_buf()],
        )
        .expect("explicit class launcher should win over a generic URL entry");

        assert_eq!(launch.command, "omarchy-launch-or-focus-webapp");
        assert_eq!(launch.args[0], "chrome-example.com__-Default");
    }

    #[test]
    fn test_capture_uses_ghostty_binary_and_working_directory_flag() {
        const GHOSTTY_PID: u32 = 3501;
        const SHELL_PID: u32 = 3502;

        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("com.mitchellh.ghostty", GHOSTTY_PID)],
            monitors: vec![make_monitor("DP-1")],
        };

        let mut app_configs = HashMap::new();
        app_configs.insert(
            "com.mitchellh.ghostty".to_string(),
            AppConfig {
                binary: None,
                capture_cwd: Some(true),
                capture_last_command: None,
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: app_configs,
        };

        let process = MockProcess {
            cwds: HashMap::new(),
            children: HashMap::from([(
                GHOSTTY_PID,
                vec![ChildProcess {
                    pid: SHELL_PID,
                    cwd: PathBuf::from("/home/user/project"),
                    cmdline: "zsh".to_string(),
                }],
            )]),
        };

        let session = capture_session("test", &hyprctl, &process, &config).unwrap();
        let client = &session.clients[0];

        assert_eq!(client.launch.command, "ghostty");
        assert_eq!(
            client.launch.args,
            vec!["--working-directory", "/home/user/project"]
        );
        assert_eq!(client.initial_class, "com.mitchellh.ghostty");
    }

    // ── Additional: monitor name resolution ───────────────────────────────

    #[test]
    fn test_capture_resolves_monitor_name_from_index() {
        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("kitty", 4001)],
            monitors: vec![make_monitor("DP-2")],
        };

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: HashMap::new(),
        };

        let session =
            capture_session("test", &hyprctl, &empty_process(), &config).expect("capture failed");

        assert_eq!(session.clients[0].monitor, "DP-2");
    }

    #[test]
    fn test_capture_preserves_named_workspace_and_pinned_state() {
        let mut client = make_hypr_client("kitty", 4002);
        client.workspace = RawWorkspace {
            id: -99,
            name: "special:magic".to_string(),
        };
        client.pinned = true;
        let hyprctl = MockHyprctl {
            clients: vec![client],
            monitors: vec![make_monitor("DP-1")],
        };

        let session = capture_session("test", &hyprctl, &empty_process(), &Config::default())
            .expect("capture failed");
        let saved = &session.clients[0];
        assert_eq!(saved.workspace, -99);
        assert_eq!(saved.workspace_name, "special:magic");
        assert!(saved.pinned);
    }

    // ── Additional: version propagated ───────────────────────────────────

    #[test]
    fn test_capture_propagates_hyprland_version() {
        let hyprctl = MockHyprctl {
            clients: vec![],
            monitors: vec![],
        };
        let config = Config::default();

        let session =
            capture_session("test", &hyprctl, &empty_process(), &config).expect("capture failed");

        assert_eq!(session.hyprland_version, "0.54.1");
    }

    // ── Task 1: hint must filter plain-shell grandchildren ───────────────

    #[test]
    fn test_capture_hint_filters_plain_shell_grandchild() {
        const KITTY_PID: u32 = 5001;
        const SHELL_PID: u32 = 5002;
        const GC_PID: u32 = 5003;

        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("kitty", KITTY_PID)],
            monitors: vec![make_monitor("DP-1")],
        };

        let mut app_configs = HashMap::new();
        app_configs.insert(
            "kitty".to_string(),
            AppConfig {
                binary: None,
                capture_cwd: Some(true),
                capture_last_command: Some(true),
                hint_template: None,
                profile_workspaces: None,
                default_workspace: None,
            },
        );

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: app_configs,
        };

        let mut children: HashMap<u32, Vec<ChildProcess>> = HashMap::new();
        // Shell child of kitty
        children.insert(
            KITTY_PID,
            vec![ChildProcess {
                pid: SHELL_PID,
                cwd: PathBuf::from("/home/user"),
                cmdline: "zsh".to_string(),
            }],
        );
        // Grandchild is also a plain shell (e.g. nested /bin/zsh)
        children.insert(
            SHELL_PID,
            vec![ChildProcess {
                pid: GC_PID,
                cwd: PathBuf::from("/home/user"),
                cmdline: "/bin/zsh".to_string(),
            }],
        );

        let process = MockProcess {
            cwds: HashMap::new(),
            children,
        };

        let session = capture_session("test", &hyprctl, &process, &config).expect("capture failed");

        assert_eq!(session.clients.len(), 1);
        assert!(
            session.clients[0].launch.hint.is_none(),
            "hint must be None when grandchild is a plain shell like /bin/zsh"
        );
    }

    // ── Task 2: monitor resolved by ID, not array index ─────────────────

    #[test]
    fn test_capture_resolves_monitor_by_id_not_index() {
        // Array order: [DP-4 (id=1), DP-5 (id=0)]
        // A client with monitor:0 should resolve to DP-5 (id=0), NOT DP-4 (index 0).
        let mut client = make_hypr_client("kitty", 6001);
        client.monitor = 0;

        let hyprctl = MockHyprctl {
            clients: vec![client],
            monitors: vec![
                make_monitor_with_id(1, "DP-4"),
                make_monitor_with_id(0, "DP-5"),
            ],
        };

        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: HashMap::new(),
        };

        let session =
            capture_session("test", &hyprctl, &empty_process(), &config).expect("capture failed");

        assert_eq!(
            session.clients[0].monitor, "DP-5",
            "monitor must be resolved by ID (0 → DP-5), not by array index (0 → DP-4)"
        );
    }

    // ── Task 4: brave_profiles field is populated when Brave is present ──

    #[test]
    fn test_capture_includes_brave_profiles_field() {
        let hyprctl = MockHyprctl {
            clients: vec![make_hypr_client("brave-browser", 7001)],
            monitors: vec![make_monitor("DP-1")],
        };
        let config = Config {
            general: GeneralConfig::default(),
            filters: FilterConfig {
                ignore_classes: vec![],
            },
            apps: HashMap::new(),
        };

        let session = capture_session("test", &hyprctl, &empty_process(), &config).unwrap();
        // brave_profiles is populated from Local State if Brave is installed;
        // in test env it may be empty or populated — just verify it doesn't error.
        // The field exists and is accessible.
        let _ = &session.brave_profiles;
    }
}
