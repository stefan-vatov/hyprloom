use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("process {0} not found")]
    NotFound(u32),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ChildProcess {
    pub pid: u32,
    pub cwd: PathBuf,
    pub cmdline: String,
}

pub trait ProcessInfoProvider {
    fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError>;
    fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError>;

    /// Return all descendants in deterministic PID order.  Process trees can
    /// contain wrappers, multiplexers, and helper processes between a
    /// terminal and its interactive shell, so callers should not assume that
    /// the shell is an immediate child.
    fn get_descendants(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
        let mut descendants = Vec::new();
        let mut queue = VecDeque::from([pid]);
        let mut visited = HashSet::from([pid]);

        while let Some(parent_pid) = queue.pop_front() {
            let Ok(mut children) = self.get_children(parent_pid) else {
                continue;
            };
            children.sort_by_key(|child| child.pid);
            for child in children {
                if visited.insert(child.pid) {
                    queue.push_back(child.pid);
                    descendants.push(child);
                }
            }
        }

        descendants.sort_by_key(|child| child.pid);
        Ok(descendants)
    }

    /// Return the complete process command line when the provider can see it.
    /// Test doubles that only model child processes may keep the default.
    fn get_cmdline(&self, pid: u32) -> Result<String, ProcessError> {
        Err(ProcessError::NotFound(pid))
    }

    /// Return whether `candidate_pid` is the root process or one of its
    /// descendants.  This is used to correlate a newly launched process with
    /// the window it eventually creates.
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
            for child in children {
                if child.pid == candidate_pid {
                    return true;
                }
                if visited.insert(child.pid) {
                    queue.push_back(child.pid);
                }
            }
        }
        false
    }
}

pub struct RealProcessInfo;

impl ProcessInfoProvider for RealProcessInfo {
    fn get_cwd(&self, pid: u32) -> Result<PathBuf, ProcessError> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).map_err(|_| ProcessError::NotFound(pid))
    }

    fn get_children(&self, pid: u32) -> Result<Vec<ChildProcess>, ProcessError> {
        let mut children_by_pid = std::collections::BTreeMap::new();
        let tasks_dir = format!("/proc/{pid}/task");
        let tasks = std::fs::read_dir(&tasks_dir).map_err(|_| ProcessError::NotFound(pid))?;

        for task in tasks.flatten() {
            let children_file = task.path().join("children");
            if let Ok(content) = std::fs::read_to_string(&children_file) {
                for child_pid_str in content.split_whitespace() {
                    if let Ok(child_pid) = child_pid_str.parse::<u32>() {
                        let cwd = self.get_cwd(child_pid).unwrap_or_default();
                        let cmdline = read_cmdline(child_pid);
                        children_by_pid.insert(
                            child_pid,
                            ChildProcess {
                                pid: child_pid,
                                cwd,
                                cmdline,
                            },
                        );
                    }
                }
            }
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

/// Whether a process command line represents an interactive shell itself,
/// rather than a shell running a command.  This intentionally accepts paths
/// such as `/usr/bin/zsh` and rejects `zsh -c ...`.
pub fn is_plain_shell(cmdline: &str) -> bool {
    let mut parts = cmdline.split_whitespace();
    let Some(command) = parts.next() else {
        return false;
    };
    if !matches!(
        Path::new(command)
            .file_name()
            .and_then(|name| name.to_str()),
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
        if argument.starts_with("--command=")
            || (argument.starts_with('-') && argument.len() > 1 && argument[1..].contains('c'))
        {
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
/// This returns an owned value because the provider's descendant list is
/// temporary and may have been assembled from several `/proc` reads.
pub fn select_terminal_process(
    process_info: &dyn ProcessInfoProvider,
    terminal_pid: u32,
) -> Option<ChildProcess> {
    process_info
        .get_descendants(terminal_pid)
        .ok()
        .and_then(|children| select_terminal_child(&children).cloned())
}

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

/// Search a process and its descendants for an application profile flag.
/// Chromium puts the profile flag on either the browser process or a helper,
/// depending on how an existing browser process was reused.  If the tree
/// advertises more than one profile, the root PID cannot identify a specific
/// top-level window and the result is intentionally ambiguous.
pub fn find_profile_directory(
    process_info: &dyn ProcessInfoProvider,
    root_pid: u32,
) -> Option<String> {
    let mut queue = VecDeque::from([root_pid]);
    let mut visited = HashSet::from([root_pid]);
    let mut profiles = HashMap::<String, String>::new();
    while let Some(pid) = queue.pop_front() {
        if let Ok(cmdline) = process_info.get_cmdline(pid) {
            if let Some(profile) = profile_directory_from_cmdline(&cmdline) {
                profiles
                    .entry(profile.to_ascii_lowercase())
                    .or_insert(profile);
            }
        }

        let Ok(mut children) = process_info.get_children(pid) else {
            continue;
        };
        children.sort_by_key(|child| child.pid);
        for child in children {
            if let Some(profile) = profile_directory_from_cmdline(&child.cmdline) {
                profiles
                    .entry(profile.to_ascii_lowercase())
                    .or_insert(profile);
            }
            if visited.insert(child.pid) {
                queue.push_back(child.pid);
            }
        }
    }
    (profiles.len() == 1).then(|| profiles.into_values().next().expect("one profile"))
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
            self.cwds
                .get(&pid)
                .cloned()
                .ok_or(ProcessError::NotFound(pid))
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
        let result = mock
            .get_children(parent_pid)
            .expect("should return children");
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
            other => panic!("expected NotFound, got {other}"),
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
    }
}
