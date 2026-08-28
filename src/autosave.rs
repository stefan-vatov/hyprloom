use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SERVICE_NAME: &str = "hyprloom-autosave.service";
const TIMER_NAME: &str = "hyprloom-autosave.timer";
const LEGACY_SERVICE_NAME: &str = "hyprflow-autosave.service";
const LEGACY_TIMER_NAME: &str = "hyprflow-autosave.timer";
const TRANSACTION_MARKER_NAME: &str = ".hyprloom-autosave.transaction";
const SERVICE_BACKUP_NAME: &str = ".hyprloom-autosave.service.backup";
const TIMER_BACKUP_NAME: &str = ".hyprloom-autosave.timer.backup";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
            format!(
                "autosave parent is writable by another user: {}",
                path.display()
            ),
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
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "autosave directory is not user-owned and private: {}",
                path.display()
            ),
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
            format!(
                "autosave transaction marker is not a regular file: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "autosave transaction marker is not user-owned: {}",
                    path.display()
                ),
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(path, permissions)?;
        }
        let verified = std::fs::symlink_metadata(path)?;
        if verified.uid() != unsafe { libc::geteuid() }
            || verified.permissions().mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "autosave transaction marker is not private: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

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
        .and_then(|_| write_optional_backup(&timer_backup, previous_timer.as_deref()))
        .and_then(|_| write_transaction_marker(systemd_dir, "backed-up"))
        .and_then(|_| atomic_write(&service_path, service_content(&binary).as_bytes()))
        .and_then(|_| write_transaction_marker(systemd_dir, "service-written"))
        .and_then(|_| atomic_write(&timer_path, timer_content().as_bytes()))
        .and_then(|_| write_transaction_marker(systemd_dir, "timer-written"))
        .and_then(|_| write_transaction_marker(systemd_dir, "committed"))
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

    migrate_legacy_units(systemd_dir);
    Ok((service_path, timer_path))
}

fn read_optional_unit(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            if metadata.uid() != unsafe { libc::geteuid() } {
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
    match contents {
        Some(contents) => atomic_write(path, contents),
        None => std::fs::remove_file(path).or_else(ignore_not_found),
    }
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
                format!(
                    "autosave transaction marker is not a regular file: {}",
                    marker_path.display()
                ),
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
        "prepared" => {}
        "committed" => {
            // The new pair is complete.  Only cleanup was interrupted.
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
    match read_optional_unit(backup)? {
        Some(contents) => atomic_write(unit, &contents),
        None => std::fs::remove_file(unit).or_else(ignore_not_found),
    }
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
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "systemd unit path has no parent directory",
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unit");

    for _ in 0..100 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o644);

        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let write_result = file.write_all(contents).and_then(|_| file.sync_all());
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

pub fn uninstall(systemd_dir: &Path) -> std::io::Result<()> {
    // If an earlier install was interrupted, settle that transaction before
    // deleting units.  This keeps backup files and the transaction marker from
    // being stranded by an uninstall performed during recovery.
    if !ensure_existing_systemd_directory(systemd_dir)? {
        return Ok(());
    }
    recover_install_transaction(systemd_dir)?;
    for timer in [TIMER_NAME, LEGACY_TIMER_NAME] {
        disable_timer(timer);
    }

    for unit in [
        SERVICE_NAME,
        TIMER_NAME,
        LEGACY_SERVICE_NAME,
        LEGACY_TIMER_NAME,
    ] {
        let path = systemd_dir.join(unit);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => std::fs::remove_file(path)?,
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing to remove non-regular systemd unit: {}",
                        path.display()
                    ),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn is_installed(systemd_dir: &Path) -> bool {
    [TIMER_NAME, LEGACY_TIMER_NAME].iter().any(|name| {
        std::fs::symlink_metadata(systemd_dir.join(name))
            .map(|metadata| metadata.file_type().is_file())
            .unwrap_or(false)
    })
}

pub fn is_active() -> bool {
    [TIMER_NAME, LEGACY_TIMER_NAME]
        .iter()
        .any(|name| unit_state_is(name, "is-active"))
}

pub fn is_enabled() -> bool {
    [TIMER_NAME, LEGACY_TIMER_NAME]
        .iter()
        .any(|name| unit_state_is(name, "is-enabled"))
}

fn unit_state_is(unit: &str, state: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", state, "--quiet", unit])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn disable_timer(timer: &str) {
    let result = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", timer])
        .output();
    if let Ok(output) = result {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            if !message.is_empty() {
                eprintln!("Warning: systemctl disable {timer} failed: {message}");
            }
        }
    }
}

fn migrate_legacy_units(systemd_dir: &Path) {
    let legacy_service = systemd_dir.join(LEGACY_SERVICE_NAME);
    let legacy_timer = systemd_dir.join(LEGACY_TIMER_NAME);
    if !legacy_service.exists() && !legacy_timer.exists() {
        return;
    }

    let was_enabled = unit_state_is(LEGACY_TIMER_NAME, "is-enabled");
    disable_timer(LEGACY_TIMER_NAME);
    for path in [legacy_service, legacy_timer] {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Warning: could not remove legacy autosave unit '{}': {error}",
                    path.display()
                );
            }
        }
    }
    if was_enabled {
        let output = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", TIMER_NAME])
            .output();
        if let Ok(output) = output {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "Warning: could not activate migrated autosave timer: {}",
                    stderr.trim()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        uninstall(dir.path()).unwrap();
        assert!(!is_installed(dir.path()));
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
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "prepared\n"
        );
    }
}
