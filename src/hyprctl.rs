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
    /// Hyprland's stable window identifier.  Unlike `address`, it is not
    /// recycled when a different window is created at the same address.
    #[serde(default, rename = "stableId")]
    pub stable_id: Option<String>,
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
    #[serde(default)]
    pub pinned: bool,
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
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
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
    /// Return the address of the currently focused window, when Hyprland has
    /// one.  This is a window-level correlation signal for applications such
    /// as Chromium and Brave that may hand a launch request to an existing
    /// browser process.
    fn get_active_window_address(&self) -> Result<Option<String>, HyprctlError> {
        Ok(None)
    }
    fn get_monitors(&self) -> Result<Vec<HyprMonitor>, HyprctlError>;
    /// Dispatch a Hyprland command; `args` is the full argument string
    /// (e.g. `"workspace 2"`).
    fn dispatch(&self, args: &str) -> Result<(), HyprctlError>;
    /// Dispatch several commands in one compositor request.  The default
    /// implementation preserves test-double behavior; the real client
    /// overrides it with Hyprland's atomic `--batch` request so a focus-based
    /// sequence cannot be interrupted by another focus change.
    fn dispatch_batch(&self, commands: &[String]) -> Result<(), HyprctlError> {
        for command in commands {
            self.dispatch(command)?;
        }
        Ok(())
    }
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

    fn get_active_window_address(&self) -> Result<Option<String>, HyprctlError> {
        let output = Command::new("hyprctl")
            .args(["activewindow", "-j"])
            .output()?;
        if !output.status.success() {
            return Err(HyprctlError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(parse_active_window_address(&output.stdout)?)
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
        let parsed_args = parse_dispatch_args(args).map_err(HyprctlError::CommandFailed)?;
        let output = Command::new("hyprctl")
            .arg("dispatch")
            .args(parsed_args)
            .output()?;
        if !output.status.success() {
            return Err(HyprctlError::CommandFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(())
    }

    fn dispatch_batch(&self, commands: &[String]) -> Result<(), HyprctlError> {
        if commands.is_empty() {
            return Ok(());
        }
        let mut batch = String::new();
        for (index, command) in commands.iter().enumerate() {
            // Validate each command with the same parser used by the single
            // dispatch path before handing it to Hyprland's batch parser.
            parse_dispatch_args(command).map_err(HyprctlError::CommandFailed)?;
            if index > 0 {
                batch.push_str(" ; ");
            }
            batch.push_str("dispatch ");
            batch.push_str(&escape_batch_command(command));
        }
        let output = Command::new("hyprctl").args(["--batch", &batch]).output()?;
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

fn escape_batch_command(command: &str) -> String {
    command.replace('\\', "\\\\").replace(';', "\\;")
}

fn parse_active_window_address(payload: &[u8]) -> Result<Option<String>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_slice(payload)?;
    Ok(value
        .get("address")
        .and_then(serde_json::Value::as_str)
        .filter(|address| !address.is_empty() && *address != "0x0")
        .map(str::to_string))
}

/// Parse the argument string accepted by `HyprctlClient::dispatch` without
/// destroying quoted workspace or monitor names.  The old whitespace split
/// made a named workspace such as `name:Writing Desk` impossible to dispatch.
fn parse_dispatch_args(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut token_started = false;

    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            token_started = true;
            continue;
        }

        if character == '\\' {
            escaped = true;
            token_started = true;
            continue;
        }

        if let Some(quote_character) = quote {
            if character == quote_character {
                quote = None;
            } else {
                current.push(character);
            }
            token_started = true;
            continue;
        }

        match character {
            '\'' | '"' => {
                quote = Some(character);
                token_started = true;
            }
            character if character.is_whitespace() => {
                if token_started {
                    args.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            character => {
                current.push(character);
                token_started = true;
            }
        }
    }

    if escaped {
        return Err("dispatch arguments end with an escape".to_string());
    }
    if quote.is_some() {
        return Err("dispatch arguments contain an unterminated quote".to_string());
    }
    if token_started {
        args.push(current);
    }

    Ok(args)
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

    #[test]
    fn test_parse_dispatch_args_preserves_quoted_names_and_escapes() {
        let args =
            parse_dispatch_args("movetoworkspacesilent 'name:Writing Desk',address:0x1").unwrap();

        assert_eq!(
            args,
            vec!["movetoworkspacesilent", "name:Writing Desk,address:0x1"]
        );

        assert_eq!(
            parse_dispatch_args("movetomonitor DP-1,address:0x1").unwrap(),
            vec!["movetomonitor", "DP-1,address:0x1"]
        );
    }

    #[test]
    fn test_parse_dispatch_args_rejects_unterminated_quotes() {
        assert!(parse_dispatch_args("workspace 'name:Writing Desk").is_err());
    }

    #[test]
    fn test_monitor_origin_stays_unknown_when_hyprland_omits_it() {
        let monitor: HyprMonitor = serde_json::from_str(
            r#"{"id":0,"name":"DP-1","width":1920,"height":1080,"transform":0}"#,
        )
        .expect("monitor without optional origin should still parse");

        assert_eq!(monitor.x, None);
        assert_eq!(monitor.y, None);
    }

    #[test]
    fn test_parse_active_window_address() {
        assert_eq!(
            parse_active_window_address(br#"{"address":"0x123"}"#).unwrap(),
            Some("0x123".to_string())
        );
        assert_eq!(
            parse_active_window_address(br#"{"address":"0x0"}"#).unwrap(),
            None
        );
        assert_eq!(parse_active_window_address(br#"{}"#).unwrap(), None);
    }
}
