use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

#[cfg(unix)]
use crate::platform::current_user_id;

/// Safety limits for values that control sleeps and polling loops.  They keep
/// a damaged config from making automatic restore appear hung indefinitely.
///
/// The limits are public so callers that expose configuration controls can
/// validate values using the same bounds as the restore engine.
///
/// The maximum delay accepted from configuration, in milliseconds.
pub const MAX_RESTORE_DELAY_MS: u64 = 60_000;
/// The minimum window-detection timeout accepted from configuration.
pub const MIN_WINDOW_DETECT_TIMEOUT_MS: u64 = 500;
/// The maximum window-detection timeout accepted from configuration.
pub const MAX_WINDOW_DETECT_TIMEOUT_MS: u64 = 120_000;
/// The maximum number of bytes read from the configuration file.
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
/// The maximum number of configured applications.
pub const MAX_CONFIG_APPS: usize = 1_024;
/// The maximum number of ignored window classes.
pub const MAX_CONFIG_FILTERS: usize = 4_096;
/// The maximum number of browser profile workspace mappings.
pub const MAX_CONFIG_PROFILE_WORKSPACES: usize = 4_096;
/// The maximum length of a configuration string, in bytes.
pub const MAX_CONFIG_STRING_BYTES: usize = 64 * 1024;

/// Complete user configuration loaded from the current or legacy config path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    /// General timing, default-session, and autosave settings.
    pub general: GeneralConfig,
    #[serde(default)]
    /// Window classes excluded from capture and restore.
    pub filters: FilterConfig,
    #[serde(default)]
    /// Per-application launch and browser-profile settings, keyed by class.
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
            .clamp(MIN_WINDOW_DETECT_TIMEOUT_MS, MAX_WINDOW_DETECT_TIMEOUT_MS);
    }
}

/// General restore and autosave settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    #[serde(default = "default_session_name")]
    /// Session name used when a command does not specify one explicitly.
    pub default_session: String,
    #[serde(default = "default_restore_delay")]
    /// Delay between launch operations, in milliseconds.
    pub restore_delay_ms: u64,
    #[serde(default = "default_detect_timeout")]
    /// Maximum time spent waiting for a launched window, in milliseconds.
    pub window_detect_timeout_ms: u64,
    #[serde(default = "default_autosave_retain")]
    /// Number of autosave snapshots retained after rotation.
    pub autosave_retain: usize,
}

/// Window classes that capture should ignore.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    #[serde(default = "default_ignore_classes")]
    /// Case-insensitive Hyprland classes excluded from snapshots.
    pub ignore_classes: Vec<String>,
}

/// Compare compositor classes case-insensitively.  Hyprland normally reports
/// stable casing, but app wrappers and older sessions do not always agree.
#[must_use]
pub fn is_ignored_class(class: &str, ignored_classes: &[String]) -> bool {
    ignored_classes
        .iter()
        .any(|ignored| !ignored.is_empty() && ignored.eq_ignore_ascii_case(class))
}

/// Find the most specific per-app configuration while tolerating compositor
/// class casing and wrappers that expose a different runtime class from their
/// initial class.
#[must_use]
pub fn app_config_for<'a>(config: &'a Config, class: &str, initial_class: &str) -> Option<&'a AppConfig> {
    let exact = |name: &str| config.apps.get(name);
    let fold = |name: &str| config.apps.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, app)| app);
    exact(class)
        .or_else(|| exact(initial_class))
        .or_else(|| fold(class))
        .or_else(|| fold(initial_class))
}

/// Optional launch and placement overrides for one application class.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Explicit executable used when restoring this application.
    pub binary: Option<String>,
    /// Whether to capture and restore the terminal working directory.
    pub capture_cwd: Option<bool>,
    /// Whether to capture and restore the terminal's last command.
    pub capture_last_command: Option<bool>,
    /// Optional launch hint template for desktop/web-app entries.
    pub hint_template: Option<String>,
    /// Browser profile directory to workspace mappings.
    pub profile_workspaces: Option<HashMap<String, i32>>,
    /// Default workspace for windows of this application.
    pub default_workspace: Option<i32>,
}

fn default_session_name() -> String {
    "latest".to_string()
}

const fn default_restore_delay() -> u64 {
    500
}

const fn default_detect_timeout() -> u64 {
    5000
}

const fn default_autosave_retain() -> usize {
    5
}

fn default_ignore_classes() -> Vec<String> {
    vec!["waybar", "wofi", "mako", "polkit", "nm-applet", "xdg-desktop-portal"]
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

/// The outcome of configuration discovery.
///
/// Only a genuinely absent configuration file selects defaults, because then
/// defaults are the operator's authoritative choice. A present-but-invalid
/// file means the retention and safety policy is unknown, so mutating
/// callers must refuse instead of proceeding with fallback values.
#[derive(Debug, Clone)]
pub enum LoadedConfig {
    /// No configuration file exists; defaults are authoritative.
    Absent(Config),
    /// The configuration file was read, parsed, and validated.
    Valid(Config),
    /// A configuration file exists but could not be loaded. The context is
    /// actionable and never contains file contents.
    Invalid(String),
}

/// Load configuration as an explicit tri-state for callers that must
/// distinguish absent defaults from present-but-invalid policy.
#[must_use]
pub fn load_config_state() -> LoadedConfig {
    for path in [config_path(), legacy_config_path()] {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return load_config_file_state(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return LoadedConfig::Invalid(format!("could not inspect config '{}': {error}", path.display()));
            }
        }
    }
    LoadedConfig::Absent(Config::default())
}

#[must_use]
/// Load configuration, falling back to defaults when it is absent or invalid.
pub fn load_config() -> Config {
    match load_config_state() {
        LoadedConfig::Valid(config) | LoadedConfig::Absent(config) => config,
        LoadedConfig::Invalid(context) => {
            crate::output::warning(format!("Warning: {context}; using defaults"));
            Config::default()
        }
    }
}

fn load_config_file_state(path: &std::path::Path) -> LoadedConfig {
    let content = match read_config_file(path) {
        Ok(content) => content,
        Err(error) => {
            return LoadedConfig::Invalid(format!("could not read config '{}': {error}", path.display()));
        }
    };
    let mut config: Config = match toml::from_str(&content) {
        Ok(config) => config,
        Err(error) => {
            return LoadedConfig::Invalid(format!("could not parse config '{}': {error}", path.display()));
        }
    };
    if let Err(error) = validate_config_structure(&config) {
        return LoadedConfig::Invalid(format!("config '{}' exceeds safety limits: {error}", path.display()));
    }
    config.normalize();
    LoadedConfig::Valid(config)
}

fn read_config_file(path: &std::path::Path) -> Result<String, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("config path is not a regular file".to_string());
    }
    #[cfg(unix)]
    {
        let user_id = current_user_id();
        let parent = path.parent().ok_or_else(|| "config path has no parent directory".to_string())?;
        let parent_metadata = std::fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
        if metadata.uid() != user_id
            || metadata.permissions().mode() & 0o022 != 0
            || !parent_metadata.is_dir()
            || parent_metadata.uid() != user_id
            || parent_metadata.permissions().mode() & 0o022 != 0
        {
            return Err("config file or its directory is not user-owned and non-writable by others".to_string());
        }
    }
    if metadata.len() > MAX_CONFIG_FILE_BYTES {
        return Err(format!("file is larger than {MAX_CONFIG_FILE_BYTES} bytes"));
    }
    let capacity = usize::try_from(metadata.len().min(MAX_CONFIG_FILE_BYTES)).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_CONFIG_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONFIG_FILE_BYTES {
        return Err(format!("file is larger than {MAX_CONFIG_FILE_BYTES} bytes"));
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

// Validation keeps the limits for each nested configuration section adjacent
// to the section it protects.
#[allow(clippy::excessive_nesting)]
fn validate_config_structure(config: &Config) -> Result<(), String> {
    validate_config_text("default session", &config.general.default_session)?;
    if config.filters.ignore_classes.len() > MAX_CONFIG_FILTERS {
        return Err(format!("more than {MAX_CONFIG_FILTERS} ignored classes"));
    }
    for class in &config.filters.ignore_classes {
        validate_config_text("ignored class", class)?;
    }
    if config.apps.len() > MAX_CONFIG_APPS {
        return Err(format!("more than {MAX_CONFIG_APPS} app configurations"));
    }
    // App keys are matched case-insensitively as a fallback for compositor
    // class casing, so two keys that fold to the same ASCII case would make
    // that fallback nondeterministic. Reject the ambiguous namespace with
    // the conflicting spellings in sorted order.
    let mut by_fold: HashMap<String, Vec<String>> = HashMap::new();
    for name in config.apps.keys() {
        let fold = name.to_ascii_lowercase();
        by_fold.entry(fold).or_default().push(name.clone());
    }
    let mut collisions: Vec<Vec<String>> = by_fold
        .into_values()
        .filter(|spellings| spellings.len() > 1)
        .map(|mut spellings| {
            spellings.sort();
            spellings
        })
        .collect();
    collisions.sort();
    if let Some(spellings) = collisions.first() {
        let listed = spellings.iter().map(|spelling| format!("'{spelling}'")).collect::<Vec<_>>().join(", ");
        return Err(format!("ambiguous app names that differ only by ASCII case: {listed}"));
    }

    for (name, app) in &config.apps {
        validate_config_text("app name", name)?;
        for (label, value) in [("app binary", app.binary.as_deref()), ("app hint template", app.hint_template.as_deref())] {
            if let Some(value) = value {
                validate_config_text(label, value)?;
            }
        }
        if let Some(workspaces) = &app.profile_workspaces {
            if workspaces.len() > MAX_CONFIG_PROFILE_WORKSPACES {
                return Err(format!("more than {MAX_CONFIG_PROFILE_WORKSPACES} profile workspace mappings"));
            }
            for profile in workspaces.keys() {
                validate_config_text("profile workspace name", profile)?;
            }
        }
    }
    Ok(())
}

fn validate_config_text(label: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_CONFIG_STRING_BYTES {
        return Err(format!("{label} is longer than {MAX_CONFIG_STRING_BYTES} bytes"));
    }
    Ok(())
}

#[must_use]
/// Return the current Hyprloom configuration file path.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("hyprloom")
        .join("config.toml")
}

#[must_use]
/// Return the legacy Hyprflow configuration file path.
pub fn legacy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("hyprflow")
        .join("config.toml")
}

#[must_use]
/// Return the current Hyprloom session directory.
pub fn sessions_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("hyprloom")
        .join("sessions")
}

#[must_use]
/// Return the legacy Hyprflow session directory.
pub fn legacy_sessions_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("hyprflow")
        .join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_apps(names: &[&str]) -> Config {
        let apps: HashMap<String, AppConfig> = names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    AppConfig {
                        binary: None,
                        capture_cwd: None,
                        capture_last_command: None,
                        hint_template: None,
                        profile_workspaces: None,
                        default_workspace: None,
                    },
                )
            })
            .collect();
        Config { apps, ..Config::default() }
    }

    #[test]
    fn validation_rejects_case_fold_collisions_with_sorted_names() {
        let config = config_with_apps(&["Foo", "foo"]);
        let error = validate_config_structure(&config).expect_err("case-fold collision must be rejected");
        assert!(error.contains("'Foo'"), "error must name the conflicting spellings: {error}");
        assert!(error.contains("'foo'"), "error must name the conflicting spellings: {error}");
    }

    #[test]
    fn validation_rejects_three_way_collisions_deterministically() {
        let config = config_with_apps(&["foo", "FOO", "Foo"]);
        let error = validate_config_structure(&config).expect_err("three-way collision must be rejected");
        assert!(error.contains("'FOO', 'Foo', 'foo'"), "names must be sorted: {error}");
    }

    #[test]
    fn validation_allows_unique_and_non_ascii_keys() {
        let config = config_with_apps(&["foot", "kitty", "Émile"]);
        validate_config_structure(&config).expect("unique keys and non-ASCII folds must stay valid");
    }

    #[test]
    fn app_lookup_falls_back_deterministically_for_unique_folds() {
        // Insertion order in a HashMap is randomized per process: the
        // case-insensitive fallback must resolve to the single spelling.
        let config = config_with_apps(&["foot"]);
        for class in ["FOOT", "Foot", "foot"] {
            assert!(app_config_for(&config, class, class).is_some(), "fallback must find {class}");
        }
    }

    fn assert_unknown_field_rejected(path: &std::path::Path, content: &str) {
        let LoadedConfig::Invalid(diagnostic) = load_config_file_state(path) else {
            panic!("unknown field must be Invalid for {content:?}");
        };
        assert!(diagnostic.contains("unknown field"), "case {content:?}: {diagnostic}");
        assert!(diagnostic.contains(path.to_string_lossy().as_ref()), "case {content:?}: {diagnostic}");
    }

    #[test]
    fn load_config_file_state_rejects_unknown_fields_with_path_context() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            "[autosaave]\nretain = 10\n".to_owned(),
            "[general]\nautosave_reten = 10\n".to_owned(),
            "[filters]\nignored_classses = []\n".to_owned(),
            "[apps.foot]\ncapture_last_comand = true\n".to_owned(),
        ];
        for content in cases {
            let path = dir.path().join("config.toml");
            std::fs::write(&path, &content).unwrap();
            assert_unknown_field_rejected(&path, &content);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn load_config_file_state_keeps_dynamic_profile_names_and_omitted_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[apps.brave-browser]\nprofile_workspaces = { \"Some Profile\" = 3 }\n").unwrap();

        match load_config_file_state(&path) {
            LoadedConfig::Valid(config) => {
                let app = config.apps.get("brave-browser").expect("dynamic app entry must parse");
                assert_eq!(app.profile_workspaces.as_ref().unwrap().get("Some Profile"), Some(&3));
            }
            LoadedConfig::Absent(_) | LoadedConfig::Invalid(_) => {
                panic!("dynamic profile names must stay valid")
            }
        }
    }

    #[test]
    fn load_config_file_state_accepts_valid_and_preserves_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\nautosave_retain = 1001\n").unwrap();

        match load_config_file_state(&path) {
            LoadedConfig::Valid(config) => assert_eq!(config.general.autosave_retain, 1001),
            LoadedConfig::Absent(_) | LoadedConfig::Invalid(_) => panic!("valid config must load"),
        }
    }

    #[test]
    fn load_config_file_state_reports_malformed_toml_as_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not [valid toml ===").unwrap();

        match load_config_file_state(&path) {
            LoadedConfig::Invalid(context) => {
                assert!(context.contains("could not parse config"), "context: {context}");
                assert!(context.contains(path.to_string_lossy().as_ref()), "context: {context}");
            }
            LoadedConfig::Absent(_) | LoadedConfig::Valid(_) => panic!("malformed toml must be Invalid"),
        }
    }

    #[test]
    fn load_config_file_state_reports_non_regular_paths_as_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();

        assert!(matches!(load_config_file_state(&path), LoadedConfig::Invalid(_)));
    }

    #[test]
    fn load_config_file_state_reports_oversized_files_as_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let padding = usize::try_from(MAX_CONFIG_FILE_BYTES).unwrap_or(0) / 6 + 8;
        let body = "# pad\n".repeat(padding);
        std::fs::write(&path, body).unwrap();

        assert!(matches!(load_config_file_state(&path), LoadedConfig::Invalid(_)));
    }

    #[test]
    fn load_config_file_state_reports_non_utf8_files_as_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x01]).unwrap();

        assert!(matches!(load_config_file_state(&path), LoadedConfig::Invalid(_)));
    }

    #[test]
    fn load_config_file_state_normalizes_valid_configs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\nwindow_detect_timeout_ms = 999_999\n").unwrap();

        match load_config_file_state(&path) {
            LoadedConfig::Valid(config) => {
                assert_eq!(config.general.window_detect_timeout_ms, MAX_WINDOW_DETECT_TIMEOUT_MS);
            }
            LoadedConfig::Absent(_) | LoadedConfig::Invalid(_) => panic!("valid config must load"),
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();

        assert_eq!(config.general.default_session, "latest");
        assert_eq!(config.general.restore_delay_ms, 500);
        assert_eq!(config.general.window_detect_timeout_ms, 5000);
        assert!(
            config.filters.ignore_classes.contains(&"waybar".to_string()),
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

        let kitty = config.apps.get("kitty").expect("apps.kitty should be present");
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
            config.filters.ignore_classes.contains(&"waybar".to_string()),
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
        assert_eq!(config.general.window_detect_timeout_ms, MAX_WINDOW_DETECT_TIMEOUT_MS);
        // Retention is a documented policy value: valid parsed values must
        // survive normalization untouched instead of being clamped.
        assert_eq!(config.general.autosave_retain, usize::MAX);

        config.general.autosave_retain = 1001;
        config.normalize();
        assert_eq!(config.general.autosave_retain, 1001);

        config.general.autosave_retain = 0;
        config.normalize();
        assert_eq!(config.general.autosave_retain, 0);

        config.general.window_detect_timeout_ms = 100;
        config.normalize();
        assert_eq!(config.general.window_detect_timeout_ms, MIN_WINDOW_DETECT_TIMEOUT_MS);
    }

    #[test]
    fn test_config_file_and_structure_limits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let oversized_len = usize::try_from(MAX_CONFIG_FILE_BYTES).unwrap_or(usize::MAX).saturating_add(1);
        std::fs::write(&path, vec![b' '; oversized_len]).unwrap();
        assert!(read_config_file(&path).is_err());

        let mut config = Config::default();
        config.general.default_session = "x".repeat(MAX_CONFIG_STRING_BYTES + 1);
        assert!(validate_config_structure(&config).is_err());

        config.general.default_session = "latest".to_string();
        config.filters.ignore_classes = vec!["class".to_string(); MAX_CONFIG_FILTERS + 1];
        assert!(validate_config_structure(&config).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_config_rejects_group_writable_config_or_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[general]\nrestore_delay_ms = 1\n").unwrap();

        let mut file_permissions = std::fs::metadata(&path).unwrap().permissions();
        file_permissions.set_mode(0o664);
        std::fs::set_permissions(&path, file_permissions).unwrap();
        assert!(read_config_file(&path).is_err());

        let mut file_permissions = std::fs::metadata(&path).unwrap().permissions();
        file_permissions.set_mode(0o600);
        std::fs::set_permissions(&path, file_permissions).unwrap();
        let mut directory_permissions = std::fs::metadata(directory.path()).unwrap().permissions();
        directory_permissions.set_mode(0o770);
        std::fs::set_permissions(directory.path(), directory_permissions).unwrap();
        assert!(read_config_file(&path).is_err());
    }

    #[test]
    fn test_config_autosave_retain_from_toml() {
        let toml_str = r"
[general]
autosave_retain = 10
";
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
