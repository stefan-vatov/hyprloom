use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Errors returned while inspecting a process or its descendants.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("process {0} not found")]
    /// The requested process no longer exists or is not visible.
    NotFound(u32),
    #[error("IO error: {0}")]
    /// Process metadata could not be read.
    IoError(#[from] std::io::Error),
}

/// Process metadata used to identify terminal shells and browser instances.
#[derive(Debug, Clone)]
pub struct ChildProcess {
    /// Process ID.
    pub pid: u32,
    /// Current working directory, when available.
    pub cwd: PathBuf,
    /// Space-separated process command line.
    pub cmdline: String,
}

/// The profile flags found while walking a Chromium process tree, together
/// with whether every process lookup needed for that conclusion succeeded.
///
/// An empty profile list is only evidence of the normal `Default` profile when
/// the walk completed; an incomplete walk could simply have missed a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDiscovery {
    /// Distinct browser profile directories found in the process tree.
    pub profiles: Vec<String>,
    /// Whether every process lookup needed for the result succeeded.
    pub complete: bool,
}

/// Provider for process metadata used during capture and restore.
pub trait ProcessInfoProvider {
    /// Return the current working directory for `pid`.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be inspected.
    fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError>;
    /// Return direct child processes for `pid`.
    ///
    /// # Errors
    ///
    /// Returns an error when the process tree cannot be inspected.
    fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError>;

    /// Return all descendants in deterministic PID order.  Process trees can
    /// contain wrappers, multiplexers, and helper processes between a
    /// terminal and its interactive shell, so callers should not assume that
    /// the shell is an immediate child.
    ///
    /// # Errors
    ///
    /// Returns an error when one of the process-tree reads fails.
    // Breadth-first traversal deliberately keeps queue, visited, and output
    // updates in one place so process identity cannot be recorded twice.
    #[allow(clippy::excessive_nesting)]
    fn get_descendants(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
        let mut descendants = Vec::new();
        let mut queue = VecDeque::from([pid]);
        let mut visited = HashSet::from([pid]);

        while let Some(parent_pid) = queue.pop_front() {
            let mut children = self.get_children(parent_pid)?;
            children.sort_by_key(|child| child.pid);
            for child in children.into_iter().filter(|child| visited.insert(child.pid)) {
                queue.push_back(child.pid);
                descendants.push(child);
            }
        }

        descendants.sort_by_key(|child| child.pid);
        Ok(descendants)
    }

    /// Return the complete process command line when the provider can see it.
    /// Test doubles that only model child processes may keep the default.
    ///
    /// # Errors
    ///
    /// Returns an error when the command line is unavailable.
    fn get_cmdline(&self, pid: u32) -> Result<String, ProcessError> {
        Err(ProcessError::NotFound(pid))
    }

    /// Return the kernel process-start timestamp when the provider can read
    /// one.  The default keeps lightweight test doubles compatible.
    ///
    /// # Errors
    ///
    /// Returns an error when the timestamp is unavailable.
    fn get_start_time(&self, pid: u32) -> Result<u64, ProcessError> {
        Err(ProcessError::NotFound(pid))
    }

    /// Return whether a failed start-time read is meaningful for this
    /// provider.  Real `/proc` inspection is reliable enough to fail closed;
    /// small test doubles and integrations that do not model timestamps can
    /// retain the historical PID-only fallback.
    fn has_reliable_process_start_time(&self) -> bool {
        false
    }

    /// Return whether `candidate_pid` is the root process or one of its
    /// descendants.  This is used to correlate a newly launched process with
    /// the window it eventually creates.
    // This is the same bounded process-tree walk as `get_descendants`, with an
    // early match; keeping the traversal explicit avoids recursive stack use.
    #[allow(clippy::excessive_nesting)]
    fn is_process_related(&self, root_pid: u32, candidate_pid: u32) -> bool {
        if root_pid == candidate_pid {
            return true;
        }

        let mut queue = VecDeque::from([root_pid]);
        let mut visited = HashSet::from([root_pid]);
        while let Some(pid) = queue.pop_front() {
            let Ok(mut children) = self.get_children(pid) else {
                continue;
            };
            children.sort_by_key(|child| child.pid);
            if children.iter().any(|child| child.pid == candidate_pid) {
                return true;
            }
            for child in children.into_iter().filter(|child| visited.insert(child.pid)) {
                queue.push_back(child.pid);
            }
        }
        false
    }
}

/// Reads process metadata from Linux `/proc` entries.
#[derive(Debug)]
pub struct RealProcessInfo;

impl ProcessInfoProvider for RealProcessInfo {
    fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).map_err(|_| ProcessError::NotFound(pid))
    }

    // `/proc/<pid>/task` can change while it is read, so the loop records
    // completeness and aggregates thread results in one defensive pass.
    #[allow(clippy::excessive_nesting)]
    fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
        let mut children_by_pid = std::collections::BTreeMap::new();
        let tasks_dir = format!("/proc/{pid}/task");
        let tasks = std::fs::read_dir(&tasks_dir).map_err(|_| ProcessError::NotFound(pid))?;
        let mut complete = true;

        for task in tasks {
            let Ok(task) = task else {
                complete = false;
                continue;
            };
            let Ok(children) = read_task_children(self, &task) else {
                complete = false;
                continue;
            };
            children_by_pid.extend(children.into_iter().map(|child| (child.pid, child)));
        }
        if !complete {
            return Err(ProcessError::IoError(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("process tree for {pid} changed while it was being inspected"),
            )));
        }
        Ok(children_by_pid.into_values().collect())
    }

    fn get_cmdline(&self, pid: u32) -> Result<String, ProcessError> {
        let cmdline = read_cmdline(pid);
        if cmdline.is_empty() {
            Err(ProcessError::NotFound(pid))
        } else {
            Ok(cmdline)
        }
    }

    fn get_start_time(&self, pid: u32) -> Result<u64, ProcessError> {
        read_process_start_time(pid)
    }

    fn has_reliable_process_start_time(&self) -> bool {
        true
    }
}

fn read_task_children(process_info: &RealProcessInfo, task: &std::fs::DirEntry) -> Result<Vec<ChildProcess>, ()> {
    let content = std::fs::read_to_string(task.path().join("children")).map_err(|_| ())?;
    let child_pids = content
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    child_pids
        .into_iter()
        .map(|pid| {
            let cmdline = read_cmdline(pid);
            if cmdline.is_empty() {
                return Err(());
            }
            Ok(ChildProcess {
                pid,
                cwd: process_info.get_cwd(pid).unwrap_or_default(),
                cmdline,
            })
        })
        .collect()
}

fn read_cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            bytes
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn read_process_start_time(pid: u32) -> Result<u64, ProcessError> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| ProcessError::NotFound(pid))?;
    let (_, fields) = stat.rsplit_once(')').ok_or(ProcessError::NotFound(pid))?;
    fields
        .split_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(ProcessError::NotFound(pid))
}

/// Whether a process command line represents an interactive shell itself,
/// rather than a shell running a command.  This intentionally accepts paths
/// such as `/usr/bin/zsh` and rejects `zsh -c ...`.
#[must_use]
pub fn is_plain_shell(cmdline: &str) -> bool {
    let mut parts = cmdline.split_whitespace();
    let Some(command) = parts.next() else {
        return false;
    };
    if !matches!(
        Path::new(command).file_name().and_then(|name| name.to_str()),
        Some("zsh" | "bash" | "fish" | "sh")
    ) {
        return false;
    }

    // Login and interactive flags still describe the shell itself.  Any
    // command-string flag, positional argument, or argument after `--` means
    // this process is being used as a command runner rather than the user's
    // terminal shell.
    for argument in parts {
        if argument == "--" || argument == "-c" || argument == "--command" {
            return false;
        }
        if argument.starts_with("--command=") || (argument.starts_with('-') && argument.len() > 1 && argument[1..].contains('c')) {
            return false;
        }
        if argument.starts_with('-') || argument.starts_with('+') {
            continue;
        }
        return false;
    }
    true
}

fn is_terminal_wrapper(cmdline: &str) -> bool {
    matches!(
        Path::new(cmdline.split_whitespace().next().unwrap_or(""))
            .file_name()
            .and_then(|name| name.to_str()),
        Some("tmux" | "screen" | "zellij")
    )
}

#[must_use]
/// Return whether `cmdline` names an internal helper process rather than an
/// interactive application.
pub fn is_helper_process(cmdline: &str) -> bool {
    matches!(
        Path::new(cmdline.split_whitespace().next().unwrap_or(""))
            .file_name()
            .and_then(|name| name.to_str()),
        Some("kitten")
    )
}

/// Pick a likely terminal shell deterministically.  A plain shell is
/// preferred over application commands, and terminal multiplexers are kept
/// as a last resort; PID is the stable tie-breaker.
#[must_use]
pub fn select_terminal_child(children: &[ChildProcess]) -> Option<&ChildProcess> {
    children
        .iter()
        .filter(|child| !child.cwd.as_os_str().is_empty() && !is_helper_process(&child.cmdline))
        .min_by_key(|child| {
            (
                if is_plain_shell(&child.cmdline) {
                    0
                } else if is_terminal_wrapper(&child.cmdline) {
                    2
                } else {
                    1
                },
                child.pid,
            )
        })
}

/// Select a likely interactive shell from the complete terminal process tree.
///
/// This returns an owned value because the provider's descendant list is
/// temporary and may have been assembled from several `/proc` reads.
pub fn select_terminal_process(process_info: &dyn ProcessInfoProvider, terminal_pid: u32) -> Option<ChildProcess> {
    let descendants = process_info.get_descendants(terminal_pid).ok()?;
    let shell_count = descendants
        .iter()
        .filter(|child| !child.cwd.as_os_str().is_empty() && is_plain_shell(&child.cmdline))
        .count();
    if shell_count != 1 {
        // A terminal server, multiplexer, or single-instance terminal can
        // expose several shell descendants for one Hyprland client PID.  An
        // unrecognised shell leaves no trustworthy shell candidate.  In both
        // cases no process-level signal tells us which child belongs to that
        // window, so returning an arbitrary descendant would capture another
        // window's CWD or command and later use it as false identity evidence.
        return None;
    }
    select_terminal_child(&descendants).cloned()
}

#[must_use]
/// Extract a Chromium `--profile-directory` value from a command line.
pub fn profile_directory_from_cmdline(cmdline: &str) -> Option<String> {
    let marker = "--profile-directory=";
    let start = cmdline.find(marker)? + marker.len();
    let value = cmdline[start..]
        .split_whitespace()
        .take_while(|part| !part.starts_with('-'))
        .collect::<Vec<_>>()
        .join(" ");
    (!value.is_empty()).then_some(value)
}

/// Search a process and its descendants for all application profile flags.
///
/// Chromium puts the profile flag on either the browser process or a helper,
/// depending on how an existing browser process was reused.  Returning the
/// complete set lets capture preserve the active profile inventory even when
/// one shared process cannot identify a particular top-level window.
pub fn find_profile_directories(process_info: &dyn ProcessInfoProvider, root_pid: u32) -> Vec<String> {
    find_profile_discovery(process_info, root_pid).profiles
}

/// Search a process and its descendants for profile flags without losing
/// whether process metadata was unavailable during the walk.
// Profile discovery combines process-tree traversal with completeness
// tracking; the branches are the observable states of that walk.
#[allow(clippy::excessive_nesting)]
pub fn find_profile_discovery(process_info: &dyn ProcessInfoProvider, root_pid: u32) -> ProfileDiscovery {
    let mut queue = VecDeque::from([(root_pid, None::<String>)]);
    let mut visited = HashSet::from([root_pid]);
    let mut profiles = HashMap::<String, String>::new();
    let mut complete = true;
    while let Some((pid, known_cmdline)) = queue.pop_front() {
        let cmdline = known_cmdline.unwrap_or_else(|| {
            process_info.get_cmdline(pid).unwrap_or_else(|_| {
                complete = false;
                String::new()
            })
        });
        if cmdline.is_empty() {
            complete = false;
        } else if let Some(profile) = profile_directory_from_cmdline(&cmdline) {
            profiles.entry(profile.to_ascii_lowercase()).or_insert(profile);
        }

        let Ok(mut children) = process_info.get_children(pid) else {
            complete = false;
            continue;
        };
        children.sort_by_key(|child| child.pid);
        for child in children {
            if child.cmdline.is_empty() {
                complete = false;
            }
            if visited.insert(child.pid) {
                queue.push_back((child.pid, Some(child.cmdline)));
            }
        }
    }
    let mut profiles: Vec<String> = profiles.into_values().collect();
    profiles.sort_by_key(|profile| profile.to_ascii_lowercase());
    ProfileDiscovery { profiles, complete }
}

/// Return a single profile only when the process tree provides an
/// unambiguous identity for it.
pub fn find_profile_directory(process_info: &dyn ProcessInfoProvider, root_pid: u32) -> Option<String> {
    let mut profiles = find_profile_directories(process_info, root_pid);
    if profiles.len() != 1 {
        return None;
    }
    profiles.pop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockProcessInfo {
        cwds: HashMap<u32, PathBuf>,
        children: HashMap<u32, Vec<ChildProcess>>,
    }

    impl ProcessInfoProvider for MockProcessInfo {
        fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
            self.cwds.get(&pid).cloned().ok_or(ProcessError::NotFound(pid))
        }

        fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
            Ok(self.children.get(&pid).cloned().unwrap_or_default())
        }
    }

    #[test]
    fn test_mock_process_info() {
        let parent_pid: u32 = 1000;
        let child_pid: u32 = 1001;
        let child_cwd = PathBuf::from("/home/user/projects");

        let mut cwds = HashMap::new();
        cwds.insert(parent_pid, PathBuf::from("/home/user"));
        cwds.insert(child_pid, child_cwd.clone());

        let child = ChildProcess {
            pid: child_pid,
            cwd: child_cwd.clone(),
            cmdline: "bash".to_string(),
        };

        let mut children_map = HashMap::new();
        children_map.insert(parent_pid, vec![child]);

        let mock = MockProcessInfo {
            cwds,
            children: children_map,
        };

        // Verify get_cwd returns the correct path for the parent
        let parent_cwd = mock.get_cwd(parent_pid).expect("parent cwd should exist");
        assert_eq!(parent_cwd, PathBuf::from("/home/user"));

        // Verify get_children returns correct child data
        let result = mock.get_children(parent_pid).expect("should return children");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].pid, child_pid);
        assert_eq!(result[0].cwd, child_cwd);
        assert_eq!(result[0].cmdline, "bash");
    }

    #[test]
    fn test_mock_cwd_not_found() {
        let mock = MockProcessInfo {
            cwds: HashMap::new(),
            children: HashMap::new(),
        };

        let result = mock.get_cwd(99999);
        assert!(result.is_err());

        match result.unwrap_err() {
            ProcessError::NotFound(pid) => assert_eq!(pid, 99999),
            ProcessError::IoError(error) => panic!("expected NotFound, got IO error {error}"),
        }
    }

    #[test]
    fn test_mock_children_empty_for_unknown_pid() {
        let mock = MockProcessInfo {
            cwds: HashMap::new(),
            children: HashMap::new(),
        };

        // get_children returns Ok(empty) for unknown PIDs (not an error)
        let result = mock.get_children(99999).expect("should return empty vec");
        assert!(result.is_empty());
    }

    #[test]
    fn test_plain_shell_detection_and_deterministic_child_selection() {
        let children = vec![
            ChildProcess {
                pid: 20,
                cwd: PathBuf::from("/work"),
                cmdline: "some-command".to_string(),
            },
            ChildProcess {
                pid: 10,
                cwd: PathBuf::from("/work"),
                cmdline: "/usr/bin/zsh".to_string(),
            },
        ];

        assert!(is_plain_shell("/usr/bin/zsh"));
        assert!(is_plain_shell("zsh -l"));
        assert!(is_plain_shell("bash --login"));
        assert!(!is_plain_shell("bash -lc pwd"));
        assert!(!is_plain_shell("zsh -c pwd"));
        assert_eq!(select_terminal_child(&children).unwrap().pid, 10);
    }

    #[test]
    fn test_terminal_selection_walks_nested_process_tree() {
        let mut children = HashMap::new();
        children.insert(
            1,
            vec![ChildProcess {
                pid: 20,
                cwd: PathBuf::from("/work"),
                cmdline: "tmux".to_string(),
            }],
        );
        children.insert(
            20,
            vec![ChildProcess {
                pid: 30,
                cwd: PathBuf::from("/work/project"),
                cmdline: "/usr/bin/zsh".to_string(),
            }],
        );
        let process_info = MockProcessInfo {
            cwds: HashMap::new(),
            children,
        };

        let shell = select_terminal_process(&process_info, 1).expect("nested shell");
        assert_eq!(shell.pid, 30);
        assert_eq!(shell.cwd, PathBuf::from("/work/project"));
    }

    #[test]
    fn test_terminal_selection_rejects_multiple_shells_for_one_terminal_pid() {
        let mut children = HashMap::new();
        children.insert(
            1,
            vec![
                ChildProcess {
                    pid: 20,
                    cwd: PathBuf::from("/work/one"),
                    cmdline: "zsh".to_string(),
                },
                ChildProcess {
                    pid: 30,
                    cwd: PathBuf::from("/work/two"),
                    cmdline: "bash".to_string(),
                },
            ],
        );
        let process_info = MockProcessInfo {
            cwds: HashMap::new(),
            children,
        };

        assert!(select_terminal_process(&process_info, 1).is_none());
    }

    #[test]
    fn test_terminal_selection_rejects_unknown_shell_fallback() {
        let mut children = HashMap::new();
        children.insert(
            1,
            vec![ChildProcess {
                pid: 20,
                cwd: PathBuf::from("/work"),
                cmdline: "nu".to_string(),
            }],
        );
        let process_info = MockProcessInfo {
            cwds: HashMap::new(),
            children,
        };

        assert!(select_terminal_process(&process_info, 1).is_none());
    }

    #[test]
    fn test_profile_directory_parser_accepts_names_with_spaces() {
        assert_eq!(
            profile_directory_from_cmdline("brave --profile-directory=Profile 1"),
            Some("Profile 1".to_string())
        );
    }

    #[test]
    fn test_profile_detection_rejects_process_trees_with_multiple_profiles() {
        let mut children = HashMap::new();
        children.insert(
            1,
            vec![ChildProcess {
                pid: 2,
                cwd: PathBuf::from("/tmp"),
                cmdline: "brave --profile-directory=Default".to_string(),
            }],
        );
        children.insert(
            2,
            vec![ChildProcess {
                pid: 3,
                cwd: PathBuf::from("/tmp"),
                cmdline: "brave --profile-directory=Profile 1".to_string(),
            }],
        );
        let process_info = MockProcessInfo {
            cwds: HashMap::new(),
            children,
        };

        assert_eq!(find_profile_directory(&process_info, 1), None);
        assert_eq!(
            find_profile_directories(&process_info, 1),
            vec!["Default".to_string(), "Profile 1".to_string()]
        );
    }

    #[test]
    fn test_profile_discovery_marks_unreadable_process_metadata_incomplete() {
        let process_info = MockProcessInfo {
            cwds: HashMap::new(),
            children: HashMap::new(),
        };

        let discovery = find_profile_discovery(&process_info, 1);

        assert!(discovery.profiles.is_empty());
        assert!(!discovery.complete);
    }
}
