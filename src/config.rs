use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Safety limits for values that control sleeps and polling loops.  They keep
/// a damaged config from making automatic restore appear hung indefinitely.
pub const MAX_RESTORE_DELAY_MS: u64 = 60_000;
pub const MAX_WINDOW_DETECT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_AUTOSAVE_RETAIN: usize = 1_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub filters: FilterConfig,
    #[serde(default)]
    pub apps: HashMap<String, AppConfig>,
}

impl Config {
    /// Clamp operational settings to bounded values before they reach a
    /// restore or autosave loop.  Configuration files are user-editable, so
    /// this is deliberately best-effort and keeps valid settings intact.
    pub fn normalize(&mut self) {
        self.general.restore_delay_ms = self.general.restore_delay_ms.min(MAX_RESTORE_DELAY_MS);
        self.general.window_detect_timeout_ms = self
            .general
            .window_detect_timeout_ms
            .min(MAX_WINDOW_DETECT_TIMEOUT_MS);
        self.general.autosave_retain = self.general.autosave_retain.min(MAX_AUTOSAVE_RETAIN);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_session_name")]
    pub default_session: String,
    #[serde(default = "default_restore_delay")]
    pub restore_delay_ms: u64,
    #[serde(default = "default_detect_timeout")]
    pub window_detect_timeout_ms: u64,
    #[serde(default = "default_autosave_retain")]
    pub autosave_retain: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    #[serde(default = "default_ignore_classes")]
    pub ignore_classes: Vec<String>,
}

/// Compare compositor classes case-insensitively.  Hyprland normally reports
/// stable casing, but app wrappers and older sessions do not always agree.
pub fn is_ignored_class(class: &str, ignored_classes: &[String]) -> bool {
    ignored_classes
        .iter()
        .any(|ignored| !ignored.is_empty() && ignored.eq_ignore_ascii_case(class))
}

/// Find the most specific per-app configuration while tolerating compositor
/// class casing and wrappers that expose a different runtime class from their
/// initial class.
pub fn app_config_for<'a>(
    config: &'a Config,
    class: &str,
    initial_class: &str,
) -> Option<&'a AppConfig> {
    config
        .apps
        .get(class)
        .or_else(|| config.apps.get(initial_class))
        .or_else(|| {
            config.apps.iter().find_map(|(name, app)| {
                (name.eq_ignore_ascii_case(class) || name.eq_ignore_ascii_case(initial_class))
                    .then_some(app)
            })
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub binary: Option<String>,
    pub capture_cwd: Option<bool>,
    pub capture_last_command: Option<bool>,
    pub hint_template: Option<String>,
    pub profile_workspaces: Option<HashMap<String, i32>>,
    pub default_workspace: Option<i32>,
}

fn default_session_name() -> String {
    "latest".to_string()
}

fn default_restore_delay() -> u64 {
    500
}

fn default_detect_timeout() -> u64 {
    5000
}

fn default_autosave_retain() -> usize {
    5
}

fn default_ignore_classes() -> Vec<String> {
    vec![
        "waybar",
        "wofi",
        "mako",
        "polkit",
        "nm-applet",
        "xdg-desktop-portal",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_session: default_session_name(),
            restore_delay_ms: default_restore_delay(),
            window_detect_timeout_ms: default_detect_timeout(),
            autosave_retain: default_autosave_retain(),
        }
    }
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            ignore_classes: default_ignore_classes(),
        }
    }
}

pub fn load_config() -> Config {
    for path in [config_path(), legacy_config_path()] {
        if path.exists() {
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    eprintln!(
                        "Warning: could not read config '{}': {error}; using defaults",
                        path.display()
                    );
                    return Config::default();
                }
            };
            let mut config: Config = match toml::from_str(&content) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!(
                        "Warning: could not parse config '{}': {error}; using defaults",
                        path.display()
                    );
                    return Config::default();
                }
            };
            config.normalize();
            return config;
        }
    }
    Config::default()
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("hyprloom")
        .join("config.toml")
}

pub fn legacy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("hyprflow")
        .join("config.toml")
}

pub fn sessions_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("hyprloom")
        .join("sessions")
}

pub fn legacy_sessions_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("hyprflow")
        .join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();

        assert_eq!(config.general.default_session, "latest");
        assert_eq!(config.general.restore_delay_ms, 500);
        assert_eq!(config.general.window_detect_timeout_ms, 5000);
        assert!(
            config
                .filters
                .ignore_classes
                .contains(&"waybar".to_string()),
            "ignore_classes should contain 'waybar'"
        );
        assert!(config.apps.is_empty());
    }

    #[test]
    fn test_config_from_toml() {
        let toml_str = r#"
[general]
default_session = "work"
restore_delay_ms = 1000
window_detect_timeout_ms = 8000

[filters]
ignore_classes = ["waybar", "dunst"]

[apps.kitty]
binary = "/usr/bin/kitty"
capture_cwd = true
capture_last_command = false
hint_template = "{cwd}"
"#;

        let config: Config = toml::from_str(toml_str).expect("should parse valid TOML");

        assert_eq!(config.general.default_session, "work");
        assert_eq!(config.general.restore_delay_ms, 1000);
        assert_eq!(config.general.window_detect_timeout_ms, 8000);
        assert_eq!(config.filters.ignore_classes, vec!["waybar", "dunst"]);

        let kitty = config
            .apps
            .get("kitty")
            .expect("apps.kitty should be present");
        assert_eq!(kitty.binary.as_deref(), Some("/usr/bin/kitty"));
        assert_eq!(kitty.capture_cwd, Some(true));
        assert_eq!(kitty.capture_last_command, Some(false));
        assert_eq!(kitty.hint_template.as_deref(), Some("{cwd}"));
    }

    #[test]
    fn test_empty_toml_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty TOML should parse successfully");

        assert_eq!(config.general.default_session, "latest");
        assert_eq!(config.general.restore_delay_ms, 500);
        assert_eq!(config.general.window_detect_timeout_ms, 5000);
        assert!(
            config
                .filters
                .ignore_classes
                .contains(&"waybar".to_string()),
            "ignore_classes should contain 'waybar' by default"
        );
        assert!(
            config.filters.ignore_classes.contains(&"wofi".to_string()),
            "ignore_classes should contain 'wofi' by default"
        );
        assert!(config.apps.is_empty());
    }

    #[test]
    fn test_config_autosave_retain_default() {
        let config = Config::default();
        assert_eq!(config.general.autosave_retain, 5);
    }

    #[test]
    fn test_config_normalizes_operational_limits() {
        let mut config = Config::default();
        config.general.restore_delay_ms = u64::MAX;
        config.general.window_detect_timeout_ms = u64::MAX;
        config.general.autosave_retain = usize::MAX;

        config.normalize();

        assert_eq!(config.general.restore_delay_ms, MAX_RESTORE_DELAY_MS);
        assert_eq!(
            config.general.window_detect_timeout_ms,
            MAX_WINDOW_DETECT_TIMEOUT_MS
        );
        assert_eq!(config.general.autosave_retain, MAX_AUTOSAVE_RETAIN);
    }

    #[test]
    fn test_config_autosave_retain_from_toml() {
        let toml_str = r#"
[general]
autosave_retain = 10
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.autosave_retain, 10);
    }

    #[test]
    fn test_ignored_classes_are_case_insensitive() {
        assert!(is_ignored_class("WayBar", &["waybar".to_string()]));
        assert!(!is_ignored_class("kitty", &["waybar".to_string()]));
    }

    #[test]
    fn test_app_config_lookup_is_case_insensitive() {
        let config: Config = toml::from_str(
            r#"
[apps."com.mitchellh.ghostty"]
binary = "ghostty"
capture_cwd = true
"#,
        )
        .unwrap();

        let app = app_config_for(&config, "Com.MitchellH.Ghostty", "").unwrap();
        assert_eq!(app.binary.as_deref(), Some("ghostty"));
        assert_eq!(app.capture_cwd, Some(true));
    }

    #[test]
    fn test_config_brave_profile_workspaces() {
        let toml_str = r#"
[apps.brave-browser]
binary = "brave"
default_workspace = 1
profile_workspaces = { "Default" = 1, "Profile 1" = 6 }
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let brave = config.apps.get("brave-browser").unwrap();
        assert_eq!(brave.default_workspace, Some(1));
        let pw = brave.profile_workspaces.as_ref().unwrap();
        assert_eq!(pw.get("Default"), Some(&1));
        assert_eq!(pw.get("Profile 1"), Some(&6));
    }
}
