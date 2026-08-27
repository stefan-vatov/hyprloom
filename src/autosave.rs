use std::path::{Path, PathBuf};

const SERVICE_NAME: &str = "hyprloom-autosave.service";
const TIMER_NAME: &str = "hyprloom-autosave.timer";

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
        binary.display()
    )
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
    std::fs::write(&service_path, service_content())?;
    std::fs::write(&timer_path, timer_content())?;
    Ok((service_path, timer_path))
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
    fn test_timer_content_format() {
        let content = timer_content();
        assert!(content.contains("OnBootSec=1min"));
        assert!(content.contains("WantedBy=timers.target"));
    }
}
