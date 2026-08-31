use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use crate::platform::current_user_id;

const SERVICE_NAME: &str = "hyprloom-autosave.service";
const TIMER_NAME: &str = "hyprloom-autosave.timer";
const LEGACY_SERVICE_NAME: &str = "hyprflow-autosave.service";
const LEGACY_TIMER_NAME: &str = "hyprflow-autosave.timer";
const TRANSACTION_MARKER_NAME: &str = ".hyprloom-autosave.transaction";
const SERVICE_BACKUP_NAME: &str = ".hyprloom-autosave.service.backup";
const TIMER_BACKUP_NAME: &str = ".hyprloom-autosave.timer.backup";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[must_use]
/// Return the per-user systemd unit directory used by Hyprloom.
pub fn systemd_user_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd")
        .join("user")
}

fn current_executable() -> std::io::Result<PathBuf> {
    let binary = std::env::current_exe()?;
    if !binary.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "current executable path is not absolute",
        ));
    }
    let binary = std::fs::canonicalize(binary)?;
    let metadata = std::fs::metadata(&binary)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "current executable is not a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "current executable is not executable",
        ));
    }
    Ok(binary)
}

fn service_content(binary: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Hyprloom autosave session\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} autosave --now\n",
        systemd_quote_path(binary)
    )
}

fn systemd_quote_path(path: &Path) -> String {
    let mut escaped = String::from("\"");
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn timer_content() -> String {
    "[Unit]\n\
     Description=Hyprloom autosave timer\n\
     \n\
     [Timer]\n\
     OnUnitActiveSec=10min\n\
     OnBootSec=1min\n\
     \n\
     [Install]\n\
     WantedBy=timers.target\n"
        .to_string()
}

fn ensure_no_symlink_ancestors(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("refusing symlinked autosave path: {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn ensure_safe_parent_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("autosave parent is not a directory: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o022 != 0 && metadata.permissions().mode() & 0o1000 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("autosave parent is writable by another user: {}", path.display()),
        ));
    }
    Ok(())
}

fn ensure_owned_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("autosave path is not a directory: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != current_user_id() || metadata.permissions().mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("autosave directory is not user-owned and private: {}", path.display()),
        ));
    }
    Ok(())
}

fn ensure_systemd_directory(systemd_dir: &Path) -> std::io::Result<()> {
    ensure_no_symlink_ancestors(systemd_dir)?;
    if let Some(parent) = systemd_dir.parent() {
        ensure_no_symlink_ancestors(parent)?;
        if parent.exists() {
            ensure_safe_parent_directory(parent)?;
        }
    }
    std::fs::create_dir_all(systemd_dir)?;
    ensure_no_symlink_ancestors(systemd_dir)?;
    ensure_owned_directory(systemd_dir)
}

fn ensure_existing_systemd_directory(systemd_dir: &Path) -> std::io::Result<bool> {
    ensure_no_symlink_ancestors(systemd_dir)?;
    match std::fs::symlink_metadata(systemd_dir) {
        Ok(_metadata) => {
            ensure_owned_directory(systemd_dir)?;
            if let Some(parent) = systemd_dir.parent() {
                ensure_safe_parent_directory(parent)?;
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_private_marker(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("autosave transaction marker is not a regular file: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        if metadata.uid() != current_user_id() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("autosave transaction marker is not user-owned: {}", path.display()),
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(path, permissions)?;
        }
        let verified = std::fs::symlink_metadata(path)?;
        if verified.uid() != current_user_id() || verified.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("autosave transaction marker is not private: {}", path.display()),
            ));
        }
    }
    Ok(())
}

/// Install or update the Hyprloom autosave service and timer.
///
/// # Errors
///
/// Returns an error when the unit directory is unsafe or the service files
/// cannot be written atomically.
pub fn install(systemd_dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    ensure_systemd_directory(systemd_dir)?;
    recover_install_transaction(systemd_dir)?;
    let binary = current_executable()?;

    let service_path = systemd_dir.join(SERVICE_NAME);
    let timer_path = systemd_dir.join(TIMER_NAME);
    let previous_service = read_optional_unit(&service_path)?;
    let previous_timer = read_optional_unit(&timer_path)?;
    let service_backup = systemd_dir.join(SERVICE_BACKUP_NAME);
    let timer_backup = systemd_dir.join(TIMER_BACKUP_NAME);

    // Persist the transaction before touching either unit.  If the process is
    // killed between the two renames, the next invocation can restore the
    // exact previous pair instead of leaving systemd with half an install.
    write_transaction_marker(systemd_dir, "prepared")?;
    if let Err(error) = write_optional_backup(&service_backup, previous_service.as_deref())
        .and_then(|()| write_optional_backup(&timer_backup, previous_timer.as_deref()))
        .and_then(|()| write_transaction_marker(systemd_dir, "backed-up"))
        .and_then(|()| atomic_write(&service_path, service_content(&binary).as_bytes()))
        .and_then(|()| write_transaction_marker(systemd_dir, "service-written"))
        .and_then(|()| atomic_write(&timer_path, timer_content().as_bytes()))
        .and_then(|()| write_transaction_marker(systemd_dir, "timer-written"))
        .and_then(|()| write_transaction_marker(systemd_dir, "committed"))
    {
        // Best-effort immediate rollback keeps an ordinary write error just as
        // safe as a process crash.  If rollback itself fails, the marker and
        // backups remain for the next install attempt to recover.
        let _ = recover_install_transaction(systemd_dir);
        return Err(error);
    }

    std::fs::remove_file(service_backup).or_else(ignore_not_found)?;
    std::fs::remove_file(timer_backup).or_else(ignore_not_found)?;
    std::fs::remove_file(systemd_dir.join(TRANSACTION_MARKER_NAME)).or_else(ignore_not_found)?;
    sync_directory(systemd_dir)?;

    migrate_legacy_units(systemd_dir)?;
    Ok((service_path, timer_path))
}

fn read_optional_unit(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            if metadata.uid() != current_user_id() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("systemd unit is not user-owned: {}", path.display()),
                ));
            }
            std::fs::read(path).map(Some)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("systemd unit is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_optional_backup(path: &Path, contents: Option<&[u8]>) -> std::io::Result<()> {
    contents.map_or_else(
        || std::fs::remove_file(path).or_else(ignore_not_found),
        |contents| atomic_write(path, contents),
    )
}

fn write_transaction_marker(systemd_dir: &Path, phase: &str) -> std::io::Result<()> {
    let path = systemd_dir.join(TRANSACTION_MARKER_NAME);
    atomic_write(&path, format!("{phase}\n").as_bytes())?;
    ensure_private_marker(&path)
}

fn recover_install_transaction(systemd_dir: &Path) -> std::io::Result<()> {
    if !ensure_existing_systemd_directory(systemd_dir)? {
        return Ok(());
    }
    let marker_path = systemd_dir.join(TRANSACTION_MARKER_NAME);
    let marker = match std::fs::symlink_metadata(&marker_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            ensure_private_marker(&marker_path)?;
            std::fs::read_to_string(&marker_path)?.trim().to_string()
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("autosave transaction marker is not a regular file: {}", marker_path.display()),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let service_path = systemd_dir.join(SERVICE_NAME);
    let timer_path = systemd_dir.join(TIMER_NAME);
    let service_backup = systemd_dir.join(SERVICE_BACKUP_NAME);
    let timer_backup = systemd_dir.join(TIMER_BACKUP_NAME);

    match marker.as_str() {
        "prepared" | "committed" => {
            // Either phase is safe to finish by removing the marker and any
            // backup files left by an interrupted install.
        }
        "backed-up" | "service-written" | "timer-written" => {
            restore_optional_unit(&service_path, &service_backup)?;
            restore_optional_unit(&timer_path, &timer_backup)?;
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "autosave transaction marker has an unknown phase",
            ));
        }
    }

    std::fs::remove_file(service_backup).or_else(ignore_not_found)?;
    std::fs::remove_file(timer_backup).or_else(ignore_not_found)?;
    std::fs::remove_file(marker_path).or_else(ignore_not_found)?;
    sync_directory(systemd_dir)
}

fn restore_optional_unit(unit: &Path, backup: &Path) -> std::io::Result<()> {
    read_optional_unit(backup)?.map_or_else(
        || std::fs::remove_file(unit).or_else(ignore_not_found),
        |contents| atomic_write(unit, &contents),
    )
}

fn ignore_not_found(error: std::io::Error) -> std::io::Result<()> {
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

fn sync_directory(directory: &Path) -> std::io::Result<()> {
    OpenOptions::new().read(true).open(directory)?.sync_all()
}

fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "systemd unit path has no parent directory"))?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("unit");

    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), sequence));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o644);

        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let write_result = file.write_all(contents).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        let directory = OpenOptions::new().read(true).open(parent)?;
        directory.sync_all()?;
        return Ok(());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary systemd unit path",
    ))
}

/// Remove current and legacy Hyprloom autosave units from `systemd_dir`.
///
/// # Errors
///
/// Returns an error when the directory or one of its unit files fails the
/// ownership and regular-file safety checks.
pub fn uninstall(systemd_dir: &Path) -> std::io::Result<()> {
    uninstall_with(systemd_dir, &RealSystemctl)
}

/// Result of one systemctl invocation: spawn and exit success plus bounded
/// stderr for diagnostics.
#[derive(Debug, Clone)]
pub struct SystemctlOutcome {
    /// Whether the command spawned and exited successfully.
    pub success: bool,
    /// The process exit status, when it exited (None on spawn failure).
    /// State queries distinguish outcomes by code: `0` enabled/active, `1`
    /// disabled/inactive, anything else is a query error.
    pub code: Option<i32>,
    /// Bounded stderr text from the command, for stage-and-unit errors.
    pub stderr: String,
}

/// Injectable systemctl boundary so uninstall transitions can be scripted.
pub trait SystemctlRunner {
    /// Run one systemctl command with the given arguments.
    fn run(&self, args: &[&str]) -> SystemctlOutcome;
}

/// Calls the real `systemctl` binary.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealSystemctl;

const SYSTEMCTL_STDERR_LIMIT: usize = 400;

impl SystemctlRunner for RealSystemctl {
    fn run(&self, args: &[&str]) -> SystemctlOutcome {
        match std::process::Command::new("systemctl").args(args).output() {
            Ok(output) => SystemctlOutcome {
                success: output.status.success(),
                code: output.status.code(),
                stderr: bounded_stderr(&output.stderr),
            },
            Err(error) => SystemctlOutcome {
                success: false,
                code: None,
                stderr: bounded_stderr(error.to_string().as_bytes()),
            },
        }
    }
}

fn bounded_stderr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut bounded: String = text.chars().take(SYSTEMCTL_STDERR_LIMIT).collect();
    if text.chars().count() > SYSTEMCTL_STDERR_LIMIT {
        bounded.push('…');
    }
    bounded.trim().to_owned()
}

/// Whether the timer's unit file exists on disk as a regular file.
fn unit_file_exists(systemd_dir: &Path, timer: &str) -> bool {
    std::fs::symlink_metadata(systemd_dir.join(timer)).is_ok_and(|metadata| metadata.file_type().is_file())
}

/// Uninstall with a confirmed transition order.
///
/// Both timers must disable and stop before any unit file is removed, so a
/// refused disable never leaves a live timer without its definition.
/// Failures retain every unit and are retryable.
///
/// # Errors
///
/// Returns an error when any required timer disable fails or is uncertain,
/// or when a unit file cannot be removed safely.
pub fn uninstall_with(systemd_dir: &Path, systemctl: &dyn SystemctlRunner) -> std::io::Result<()> {
    // If an earlier install was interrupted, settle that transaction before
    // deleting units.  This keeps backup files and the transaction marker from
    // being stranded by an uninstall performed during recovery.
    if !ensure_existing_systemd_directory(systemd_dir)? {
        return Ok(());
    }
    recover_install_transaction(systemd_dir)?;
    for timer in [TIMER_NAME, LEGACY_TIMER_NAME] {
        // A timer whose unit file is absent locally cannot stay loaded after
        // the removal phase, so no disable is required for it.
        if !unit_file_exists(systemd_dir, timer) {
            continue;
        }
        let outcome = systemctl.run(&["--user", "disable", "--now", timer]);
        if !outcome.success {
            return Err(std::io::Error::other(format!(
                "could not disable autosave timer '{timer}' (units retained): {}",
                outcome.stderr
            )));
        }
    }

    for unit in [SERVICE_NAME, TIMER_NAME, LEGACY_SERVICE_NAME, LEGACY_TIMER_NAME] {
        let path = systemd_dir.join(unit);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(path)?,
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("refusing to remove non-regular systemd unit: {}", path.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[must_use]
/// Return whether a Hyprloom autosave timer file is installed.
pub fn is_installed(systemd_dir: &Path) -> bool {
    [TIMER_NAME, LEGACY_TIMER_NAME]
        .iter()
        .any(|name| std::fs::symlink_metadata(systemd_dir.join(name)).is_ok_and(|metadata| metadata.file_type().is_file()))
}

#[must_use]
/// Return whether the current or legacy autosave timer is active.
pub fn is_active() -> bool {
    [TIMER_NAME, LEGACY_TIMER_NAME].iter().any(|name| unit_state_is(name, "is-active"))
}

#[must_use]
/// Return whether the current or legacy autosave timer is enabled.
pub fn is_enabled() -> bool {
    [TIMER_NAME, LEGACY_TIMER_NAME].iter().any(|name| unit_state_is(name, "is-enabled"))
}

fn unit_state_is(unit: &str, state: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", state, "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

fn migrate_legacy_units(systemd_dir: &Path) -> std::io::Result<()> {
    migrate_legacy_units_with(systemd_dir, &RealSystemctl)
}

/// Whether a legacy `HyprFlow` timer still carries the autosave schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyTimerState {
    /// `is-enabled` exited zero: the legacy timer preserves a schedule.
    Enabled,
    /// `is-enabled` exited one: the legacy timer is disabled.
    Disabled,
    /// Spawn failure or an unexpected status: the state is unknown.
    QueryError,
}

const fn classify_legacy_state(outcome: &SystemctlOutcome) -> LegacyTimerState {
    match outcome.code {
        Some(0) => LegacyTimerState::Enabled,
        Some(1) => LegacyTimerState::Disabled,
        _ => LegacyTimerState::QueryError,
    }
}

fn migrate_legacy_units_with(systemd_dir: &Path, systemctl: &dyn SystemctlRunner) -> std::io::Result<()> {
    let legacy_service = systemd_dir.join(LEGACY_SERVICE_NAME);
    let legacy_timer = systemd_dir.join(LEGACY_TIMER_NAME);
    if !legacy_service.exists() && !legacy_timer.exists() {
        return Ok(());
    }

    let legacy_state = classify_legacy_state(&systemctl.run(&["--user", "is-enabled", "--quiet", LEGACY_TIMER_NAME]));
    if legacy_state == LegacyTimerState::QueryError {
        return Err(std::io::Error::other(
            "could not query the legacy autosave timer state (legacy units retained)",
        ));
    }
    let was_enabled = legacy_state == LegacyTimerState::Enabled;

    // An enabled legacy schedule must live on in the replacement timer
    // before the legacy units are touched: establish and verify first.
    if was_enabled {
        let enabled = systemctl.run(&["--user", "enable", "--now", TIMER_NAME]);
        if !enabled.success {
            return Err(std::io::Error::other(format!(
                "could not enable the Hyprloom autosave timer during migration (legacy units retained): {}",
                enabled.stderr
            )));
        }
        let verified = systemctl.run(&["--user", "is-enabled", "--quiet", TIMER_NAME]);
        if !verified.success {
            return Err(std::io::Error::other(format!(
                "could not verify the Hyprloom autosave timer during migration (legacy units retained): {}",
                verified.stderr
            )));
        }
    }

    let disabled = systemctl.run(&["--user", "disable", "--now", LEGACY_TIMER_NAME]);
    if !disabled.success {
        return Err(std::io::Error::other(format!(
            "could not disable the legacy autosave timer (legacy units retained): {}",
            disabled.stderr
        )));
    }
    for path in [legacy_service, legacy_timer] {
        if let Err(error) = std::fs::remove_file(&path).or_else(ignore_not_found) {
            return Err(std::io::Error::other(format!(
                "could not remove legacy autosave unit '{}': {error}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted systemctl runner: fails specific (verb, ordinal) calls
    /// with a configurable exit status.
    struct ScriptedSystemctl {
        failures: Vec<(String, usize)>,
        fail_code: i32,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl ScriptedSystemctl {
        fn failing(verb: &str, ordinal: usize) -> Self {
            Self::failing_with_code(verb, ordinal, 1)
        }

        fn failing_with_code(verb: &str, ordinal: usize, code: i32) -> Self {
            Self {
                failures: vec![(verb.to_owned(), ordinal)],
                fail_code: code,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn always_succeeding() -> Self {
            Self {
                failures: Vec::new(),
                fail_code: 1,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn recorded_commands(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }

        fn first_failure(&self, verb: &str) -> Option<(String, usize)> {
            self.failures.iter().find(|(failed_verb, _)| failed_verb == verb).cloned()
        }
    }

    impl SystemctlRunner for ScriptedSystemctl {
        fn run(&self, args: &[&str]) -> SystemctlOutcome {
            let call: Vec<String> = args.iter().map(|argument| (*argument).to_owned()).collect();
            let verb = scripted_call_verb(&call);
            let ordinal = self.record_verb_call(&call);
            match self.first_failure(&verb) {
                Some((_, target)) if target == ordinal => SystemctlOutcome {
                    success: false,
                    code: Some(self.fail_code),
                    stderr: format!("fake systemctl: {verb} refused"),
                },
                _ => SystemctlOutcome {
                    success: true,
                    code: Some(0),
                    stderr: String::new(),
                },
            }
        }
    }

    impl ScriptedSystemctl {
        fn record_verb_call(&self, call: &[String]) -> usize {
            let verb = scripted_call_verb(call);
            let mut calls = self.calls.borrow_mut();
            calls.push(call.to_owned());
            calls.iter().filter(|recorded| scripted_call_verb(recorded) == verb).count()
        }
    }

    fn scripted_call_verb(call: &[String]) -> String {
        call.iter().find(|argument| !argument.starts_with('-')).cloned().unwrap_or_default()
    }

    fn seed_units(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for name in [SERVICE_NAME, TIMER_NAME, LEGACY_SERVICE_NAME, LEGACY_TIMER_NAME] {
            std::fs::write(dir.join(name), "[Unit]\nDescription=seeded\n").unwrap();
        }
    }

    fn unit_paths(dir: &Path) -> Vec<std::path::PathBuf> {
        [SERVICE_NAME, TIMER_NAME, LEGACY_SERVICE_NAME, LEGACY_TIMER_NAME]
            .iter()
            .map(|name| dir.join(name))
            .collect()
    }

    fn seed_legacy_units_only(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for name in [LEGACY_SERVICE_NAME, LEGACY_TIMER_NAME] {
            std::fs::write(dir.join(name), "[Unit]\nDescription=legacy\n").unwrap();
        }
    }

    #[test]
    fn migration_of_enabled_legacy_establishes_then_verifies_then_disables() {
        let dir = tempfile::tempdir().unwrap();
        seed_legacy_units_only(dir.path());
        let systemctl = ScriptedSystemctl::always_succeeding();

        migrate_legacy_units_with(dir.path(), &systemctl).expect("enabled migration must succeed");

        let verbs: Vec<String> = systemctl.recorded_commands().iter().map(|call| scripted_call_verb(call)).collect();
        assert_eq!(
            verbs,
            vec![
                "is-enabled".to_owned(),
                "enable".to_owned(),
                "is-enabled".to_owned(),
                "disable".to_owned()
            ],
            "query, establish, verify, then retire the legacy timer"
        );
        assert!(!dir.path().join(LEGACY_TIMER_NAME).exists(), "legacy units are removed last");
    }

    #[test]
    fn migration_of_disabled_legacy_never_enables_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        seed_legacy_units_only(dir.path());
        // is-enabled exits one: the legacy timer is disabled.
        let systemctl = ScriptedSystemctl::failing_with_code("is-enabled", 1, 1);

        migrate_legacy_units_with(dir.path(), &systemctl).expect("disabled migration must succeed");

        let verbs: Vec<String> = systemctl.recorded_commands().iter().map(|call| scripted_call_verb(call)).collect();
        assert_eq!(
            verbs,
            vec!["is-enabled".to_owned(), "disable".to_owned()],
            "a disabled legacy timer must not activate the replacement"
        );
        assert!(!dir.path().join(LEGACY_TIMER_NAME).exists());
    }

    #[test]
    fn migration_query_error_fails_and_retains_legacy_units() {
        let dir = tempfile::tempdir().unwrap();
        seed_legacy_units_only(dir.path());
        // Exit two is a query error, not a disabled answer.
        let systemctl = ScriptedSystemctl::failing_with_code("is-enabled", 1, 2);

        let error = migrate_legacy_units_with(dir.path(), &systemctl).expect_err("query error must refuse migration");

        assert!(error.to_string().contains("could not query"), "{error}");
        assert!(dir.path().join(LEGACY_TIMER_NAME).exists(), "legacy units are retained");
        assert_eq!(systemctl.recorded_commands().len(), 1, "no transitions may follow an unknown state");
    }

    #[test]
    fn migration_enable_failure_retains_the_legacy_schedule() {
        let dir = tempfile::tempdir().unwrap();
        seed_legacy_units_only(dir.path());
        let systemctl = ScriptedSystemctl::failing("enable", 1);

        let error = migrate_legacy_units_with(dir.path(), &systemctl).expect_err("enable failure must refuse migration");

        assert!(error.to_string().contains("legacy units retained"), "{error}");
        assert!(dir.path().join(LEGACY_TIMER_NAME).exists(), "the legacy schedule survives");
        let legacy_retired = systemctl.recorded_commands().iter().any(|call| scripted_call_verb(call) == "disable");
        assert!(!legacy_retired, "the legacy timer is retired only after success");
    }

    #[test]
    fn migration_legacy_disable_failure_retains_the_legacy_schedule() {
        let dir = tempfile::tempdir().unwrap();
        seed_legacy_units_only(dir.path());
        let systemctl = ScriptedSystemctl::failing("disable", 1);

        let error = migrate_legacy_units_with(dir.path(), &systemctl).expect_err("legacy disable failure must refuse");

        assert!(error.to_string().contains("legacy units retained"), "{error}");
        assert!(dir.path().join(LEGACY_TIMER_NAME).exists(), "the legacy timer file survives");
    }

    #[test]
    fn migration_without_legacy_units_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let systemctl = ScriptedSystemctl::always_succeeding();
        migrate_legacy_units_with(dir.path(), &systemctl).expect("no legacy units: no-op");
        assert!(systemctl.recorded_commands().is_empty());
    }

    #[test]
    fn uninstall_fails_and_retains_every_unit_when_current_disable_fails() {
        let dir = tempfile::tempdir().unwrap();
        seed_units(dir.path());
        let systemctl = ScriptedSystemctl::failing("disable", 1);

        let error = uninstall_with(dir.path(), &systemctl).expect_err("current disable failure must refuse");

        assert!(
            error.to_string().contains(TIMER_NAME),
            "the stage-and-unit error must name the timer: {error}"
        );
        for path in unit_paths(dir.path()) {
            assert!(path.exists(), "unit must be retained on failure: {}", path.display());
        }
    }

    #[test]
    fn uninstall_fails_and_retains_every_unit_when_legacy_disable_fails() {
        let dir = tempfile::tempdir().unwrap();
        seed_units(dir.path());
        // The current timer disables fine; the legacy timer is the second call.
        let systemctl = ScriptedSystemctl::failing("disable", 2);

        let error = uninstall_with(dir.path(), &systemctl).expect_err("legacy disable failure must refuse");

        assert!(
            error.to_string().contains(LEGACY_TIMER_NAME),
            "the stage-and-unit error must name the legacy timer: {error}"
        );
        for path in unit_paths(dir.path()) {
            assert!(path.exists(), "unit must be retained on failure: {}", path.display());
        }
    }

    #[test]
    fn uninstall_disables_both_timers_in_order_before_removing_units() {
        let dir = tempfile::tempdir().unwrap();
        seed_units(dir.path());
        let systemctl = ScriptedSystemctl::always_succeeding();

        uninstall_with(dir.path(), &systemctl).expect("confirmed disables must allow removal");

        for path in unit_paths(dir.path()) {
            assert!(!path.exists(), "every unit must be removed on success: {}", path.display());
        }
        let commands = systemctl.recorded_commands();
        assert_eq!(
            commands,
            vec![
                vec!["--user".to_owned(), "disable".to_owned(), "--now".to_owned(), TIMER_NAME.to_owned()],
                vec![
                    "--user".to_owned(),
                    "disable".to_owned(),
                    "--now".to_owned(),
                    LEGACY_TIMER_NAME.to_owned()
                ],
            ],
            "disable order must be current then legacy, before any deletion"
        );
    }

    #[test]
    fn uninstall_retries_idempotently_after_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        seed_units(dir.path());
        let systemctl = ScriptedSystemctl::failing("disable", 1);
        assert!(uninstall_with(dir.path(), &systemctl).is_err());

        let retry = ScriptedSystemctl::always_succeeding();
        uninstall_with(dir.path(), &retry).expect("retry after cleared failure must succeed");
        for path in unit_paths(dir.path()) {
            assert!(!path.exists(), "retry must complete the removal: {}", path.display());
        }
    }

    #[test]
    fn uninstall_without_units_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let systemctl = ScriptedSystemctl::always_succeeding();
        uninstall_with(dir.path(), &systemctl).expect("absent install must stay a no-op");
        assert!(systemctl.recorded_commands().is_empty(), "nothing to disable when nothing is installed");
    }

    #[test]
    fn uninstall_refuses_non_regular_units_but_confirms_disables_first() {
        let dir = tempfile::tempdir().unwrap();
        seed_units(dir.path());
        let symlink = dir.path().join(SERVICE_NAME);
        std::fs::remove_file(&symlink).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &symlink).unwrap();
        let systemctl = ScriptedSystemctl::always_succeeding();

        let error = uninstall_with(dir.path(), &systemctl).expect_err("non-regular units must be refused");

        assert!(
            error.to_string().contains("non-regular"),
            "the error must describe the unsafe unit: {error}"
        );
        assert_eq!(systemctl.recorded_commands().len(), 2, "disables are confirmed before the removal phase");
        assert!(symlink.symlink_metadata().is_ok(), "the unsafe entry is retained for the operator");
    }

    #[test]
    fn test_install_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let (service, timer) = install(dir.path()).unwrap();
        assert!(service.exists());
        assert!(timer.exists());

        let service_text = std::fs::read_to_string(&service).unwrap();
        assert!(service_text.contains("autosave --now"));
        assert!(service_text.contains("[Service]"));

        let timer_text = std::fs::read_to_string(&timer).unwrap();
        assert!(timer_text.contains("OnUnitActiveSec=10min"));
        assert!(timer_text.contains("[Install]"));
    }

    #[test]
    fn test_is_installed_checks_timer_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_installed(dir.path()));
        install(dir.path()).unwrap();
        assert!(is_installed(dir.path()));
    }

    #[test]
    fn test_uninstall_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        assert!(is_installed(dir.path()));
        let systemctl = ScriptedSystemctl::always_succeeding();
        uninstall_with(dir.path(), &systemctl).unwrap();
        assert!(!is_installed(dir.path()));
        assert_eq!(
            systemctl.recorded_commands().len(),
            1,
            "only the installed current timer requires a disable; the absent legacy timer cannot stay loaded"
        );
    }

    #[test]
    fn test_uninstall_noop_when_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        uninstall(dir.path()).unwrap();
    }

    #[test]
    fn test_service_content_format() {
        let content = service_content(Path::new("/tmp/hyprloom"));
        assert!(content.contains("Type=oneshot"));
        assert!(content.contains("autosave --now"));
        assert!(content.contains("ExecStart=\"/tmp/hyprloom\" autosave --now"));
    }

    #[test]
    fn test_systemd_quote_path_protects_spaces_and_specifiers() {
        assert_eq!(
            systemd_quote_path(Path::new("/tmp/Desk Loom/100%/hyprloom")),
            "\"/tmp/Desk Loom/100%%/hyprloom\""
        );
    }

    #[test]
    fn test_timer_content_format() {
        let content = timer_content();
        assert!(content.contains("OnBootSec=1min"));
        assert!(content.contains("WantedBy=timers.target"));
    }

    #[test]
    fn test_recovery_restores_both_units_after_partial_install() {
        let dir = tempfile::tempdir().unwrap();
        let service = dir.path().join(SERVICE_NAME);
        let timer = dir.path().join(TIMER_NAME);
        let service_backup = dir.path().join(SERVICE_BACKUP_NAME);
        let timer_backup = dir.path().join(TIMER_BACKUP_NAME);

        std::fs::write(&service, "old service\n").unwrap();
        std::fs::write(&timer, "old timer\n").unwrap();
        atomic_write(&service_backup, b"old service\n").unwrap();
        atomic_write(&timer_backup, b"old timer\n").unwrap();
        std::fs::write(&service, "new service\n").unwrap();
        std::fs::write(&timer, "new timer\n").unwrap();
        write_transaction_marker(dir.path(), "service-written").unwrap();

        recover_install_transaction(dir.path()).unwrap();

        assert_eq!(std::fs::read_to_string(service).unwrap(), "old service\n");
        assert_eq!(std::fs::read_to_string(timer).unwrap(), "old timer\n");
        assert!(!service_backup.exists());
        assert!(!timer_backup.exists());
        assert!(!dir.path().join(TRANSACTION_MARKER_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_install_rejects_symlinked_systemd_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let real_dir = root.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let linked_dir = root.path().join("linked");
        symlink(&real_dir, &linked_dir).unwrap();

        assert!(install(&linked_dir).is_err());
        assert!(!real_dir.join(SERVICE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_recovery_rejects_symlinked_transaction_marker() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "prepared\n").unwrap();
        symlink(outside.path(), dir.path().join(TRANSACTION_MARKER_NAME)).unwrap();

        assert!(recover_install_transaction(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(outside.path()).unwrap(), "prepared\n");
    }
}
