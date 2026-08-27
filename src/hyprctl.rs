use serde::Deserialize;
use std::process::Command;

// ── Hyprland data types ────────────────────────────────────────────────────
// These mirror the JSON shapes returned by `hyprctl -j`.
// Once crate::session is fully implemented they should be re-exported from
// there and these definitions removed (or aliased).

#[derive(Debug, Clone, Deserialize)]
pub struct HyprWorkspace {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HyprClient {
    pub address: String,
    pub class: String,
    pub title: String,
    /// The app identity reported when the window was first created.  Unlike
    /// `class`/`title`, these values stay useful when an app changes its
    /// title or class after launch.
    #[serde(default, rename = "initialClass")]
    pub initial_class: String,
    #[serde(default, rename = "initialTitle")]
    pub initial_title: String,
    pub workspace: HyprWorkspace,
    pub monitor: i32,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub floating: bool,
    pub fullscreen: u8,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
    pub pid: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HyprMonitor {
    pub id: i32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub transform: u32,
}

// ── Error type ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum HyprctlError {
    #[error("hyprctl command failed: {0}")]
    CommandFailed(String),
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

// ── Trait ──────────────────────────────────────────────────────────────────

pub trait HyprctlClient {
    fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError>;
    fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError>;
    /// Dispatch a Hyprland command; `args` is the full argument string
    /// (e.g. `"workspace 2"`).
    fn dispatch(&self, args: &str) -> Result<(), HyprctlError>;
    /// Return the Hyprland version string (e.g. `"0.54.1"`).
    fn get_hyprland_version(&self) -> Result<String, HyprctlError>;
}

// ── Real implementation ────────────────────────────────────────────────────

/// Calls the real `hyprctl` binary via `std::process::Command`.
pub struct RealHyprctl;

impl HyprctlClient for RealHyprctl {
    fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
        let output = Command::new("hyprctl").args(["clients", "-j"]).output()?;
        if !output.status.success() {
            return Err(HyprctlError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
        let output = Command::new("hyprctl").args(["monitors", "-j"]).output()?;
        if !output.status.success() {
            return Err(HyprctlError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn dispatch(&self, args: &str) -> Result<(), HyprctlError> {
        let output = Command::new("hyprctl")
            .arg("dispatch")
            .args(args.split_whitespace())
            .output()?;
        if !output.status.success() {
            return Err(HyprctlError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(())
    }

    fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
        let output = Command::new("hyprctl").arg("version").output()?;
        if !output.status.success() {
            return Err(HyprctlError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        // Output format: "Hyprland 0.54.1 built from ..."
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .split_whitespace()
            .nth(1)
            .unwrap_or("unknown")
            .to_string())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A test double that records every `dispatch` call and returns canned
    /// empty data for the query methods.
    struct MockHyprctl {
        dispatched: RefCell<Vec<String>>,
    }

    impl MockHyprctl {
        fn new() -> Self {
            Self {
                dispatched: RefCell::new(Vec::new()),
            }
        }

        fn dispatched_calls(&self) -> Vec<String> {
            self.dispatched.borrow().clone()
        }
    }

    impl HyprctlClient for MockHyprctl {
        fn get_clients(&self) -> Result<Vec<HyprClient>, HyprctlError> {
            Ok(vec![])
        }

        fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError> {
            Ok(vec![])
        }

        fn dispatch(&self, args: &str) -> Result<(), HyprctlError> {
            self.dispatched.borrow_mut().push(args.to_string());
            Ok(())
        }

        fn get_hyprland_version(&self) -> Result<String, HyprctlError> {
            Ok("0.0.0-mock".to_string())
        }
    }

    #[test]
    fn test_mock_records_dispatches() {
        let mock = MockHyprctl::new();

        mock.dispatch("workspace 1").unwrap();
        mock.dispatch("workspace 2").unwrap();
        mock.dispatch("movetoworkspace 3").unwrap();

        let calls = mock.dispatched_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "workspace 1");
        assert_eq!(calls[1], "workspace 2");
        assert_eq!(calls[2], "movetoworkspace 3");
    }

    #[test]
    fn test_mock_get_clients_returns_empty() {
        let mock = MockHyprctl::new();
        let clients = mock.get_clients().unwrap();
        assert!(clients.is_empty());
    }

    #[test]
    fn test_mock_get_monitors_returns_empty() {
        let mock = MockHyprctl::new();
        let monitors = mock.get_monitors().unwrap();
        assert!(monitors.is_empty());
    }

    #[test]
    fn test_mock_version_returns_sentinel() {
        let mock = MockHyprctl::new();
        let version = mock.get_hyprland_version().unwrap();
        assert_eq!(version, "0.0.0-mock");
    }

    #[test]
    fn test_dispatch_records_preserve_order() {
        let mock = MockHyprctl::new();
        let commands = ["exec kitty", "killactive", "togglefloating"];
        for cmd in &commands {
            mock.dispatch(cmd).unwrap();
        }
        let calls = mock.dispatched_calls();
        for (i, cmd) in commands.iter().enumerate() {
            assert_eq!(&calls[i], cmd);
        }
    }
}
