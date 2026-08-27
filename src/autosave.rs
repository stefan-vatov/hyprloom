use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SERVICE_NAME: &str = "hyprloom-autosave.service";
const TIMER_NAME: &str = "hyprloom-autosave.timer";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn systemd_user_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd")
        .join("user")
}

fn service_content() -> String {
    let binary = which::which("hyprloom").unwrap_or_else(|_| PathBuf::from("hyprloom"));
    format!(
        "[Unit]\n\
         Description=Hyprloom autosave session\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} autosave --now\n",
        systemd_quote_path(&binary)
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

pub fn install(systemd_dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(systemd_dir)?;

    if which::which("hyprloom").is_err() {
        eprintln!("Warning: hyprloom not found in PATH. Edit the generated .service file with the full path.");
    }

    let service_path = systemd_dir.join(SERVICE_NAME);
    let timer_path = systemd_dir.join(TIMER_NAME);
    let previous_service = match std::fs::read(&service_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    atomic_write(&service_path, service_content().as_bytes())?;
    if let Err(error) = atomic_write(&timer_path, timer_content().as_bytes()) {
        // Do not leave a newly-written service paired with a stale or
        // truncated timer when the second replacement fails.
        match previous_service {
            Some(contents) => {
                let _ = atomic_write(&service_path, &contents);
            }
            None => {
                let _ = std::fs::remove_file(&service_path);
            }
        }
        return Err(error);
    }
    Ok((service_path, timer_path))
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
        if let Ok(directory) = OpenOptions::new().read(true).open(parent) {
            let _ = directory.sync_all();
        }
        return Ok(());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary systemd unit path",
    ))
}

pub fn uninstall(systemd_dir: &Path) -> std::io::Result<()> {
    let result = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "hyprloom-autosave.timer"])
        .output();
    if let Ok(output) = result {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Warning: systemctl disable failed: {}", stderr.trim());
        }
    }

    let service_path = systemd_dir.join(SERVICE_NAME);
    let timer_path = systemd_dir.join(TIMER_NAME);
    if timer_path.exists() {
        std::fs::remove_file(&timer_path)?;
    }
    if service_path.exists() {
        std::fs::remove_file(&service_path)?;
    }
    Ok(())
}

pub fn is_installed(systemd_dir: &Path) -> bool {
    systemd_dir.join(TIMER_NAME).exists()
}

pub fn is_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "hyprloom-autosave.timer"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "hyprloom-autosave.timer"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
        let content = service_content();
        assert!(content.contains("Type=oneshot"));
        assert!(content.contains("autosave --now"));
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
}
