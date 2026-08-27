use crate::config::MAX_AUTOSAVE_RETAIN;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::cmp::Reverse;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_SESSION_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SESSION_CLIENTS: usize = 4_096;
pub const MAX_SESSION_MONITORS: usize = 128;
pub const MAX_SESSION_BRAVE_PROFILES: usize = 1_024;
pub const MAX_SESSION_ARGS: usize = 2_048;
pub const MAX_SESSION_STRING_BYTES: usize = 64 * 1024;

// Older session writers sometimes emitted JSON null for fields that were
// introduced as optional compatibility fields.  Treat null the same as a
// missing value so those snapshots remain usable after an upgrade.
fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|value| value.unwrap_or_default())
}

fn deserialize_nullable_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<bool>::deserialize(deserializer).map(|value| value.unwrap_or_default())
}

// === Hyprloom session structs (what we save to disk) ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveProfile {
    pub directory: String, // "Default", "Profile 1", etc.
    pub name: String,      // "Credifit", "LinkPJ", etc.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub hyprland_version: String,
    pub monitors: Vec<Monitor>,
    pub clients: Vec<SessionClient>,
    #[serde(default)]
    pub brave_profiles: Vec<BraveProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub transform: u32,
    /// Monitor origin in Hyprland's global coordinate space.  `None` keeps
    /// older session files from being treated as if they were captured at
    /// (0, 0), which would make geometry adaptation unsafe.
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClient {
    pub class: String,
    pub title: String,
    /// Initial Hyprland app identity.  These fields are optional in spirit:
    /// older session files do not contain them and reconciliation falls back
    /// to `class` and `title` when they are empty.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub initial_class: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub initial_title: String,
    pub workspace: i32,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub workspace_name: String,
    pub monitor: String,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub floating: bool,
    pub fullscreen: u8,
    #[serde(default, deserialize_with = "deserialize_nullable_bool")]
    pub pinned: bool,
    #[serde(default)]
    pub profile_directory: Option<String>,
    /// True when the captured browser process did not provide a safe
    /// window-specific profile identity.  Automatic restore must not use such
    /// a client to move or launch a guessed Brave profile.
    #[serde(default, deserialize_with = "deserialize_nullable_bool")]
    pub profile_identity_ambiguous: bool,
    pub focus_history_id: i32,
    pub launch: LaunchInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchInfo {
    pub command: String,
    pub args: Vec<String>,
    pub hint: Option<String>,
}

// === Session storage ===

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session '{0}' not found")]
    NotFound(String),
    #[error("session '{0}' already exists")]
    AlreadyExists(String),
    #[error("invalid session name '{0}': use 1-128 ASCII letters, numbers, '.', '_' or '-'")]
    InvalidName(String),
    #[error("unsafe session path for '{0}'")]
    UnsafePath(String),
    #[error("session file '{requested}' contains payload for '{actual}'")]
    NameMismatch { requested: String, actual: String },
    #[error("session data exceeds safety limits: {0}")]
    TooLarge(String),
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub client_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacePhase {
    Prepared,
    Closing,
    InProgress,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceMarker {
    pub backup_name: String,
    pub phase: ReplacePhase,
    pub closing_address: Option<String>,
    /// The session being installed.  Older markers do not contain this and
    /// therefore retain the conservative exact-recovery behavior.
    pub target_name: Option<String>,
}

/// Process-wide lock for all CLI operations that can observe or mutate the
/// desktop/session store.  Keeping this in the helper means the UI, systemd
/// autosave, and manually invoked commands share one serialization boundary.
pub struct OperationLock {
    file: File,
}

impl OperationLock {
    pub fn acquire() -> Result<Self, SessionError> {
        let lock_dir = operation_lock_dir()?;
        match std::fs::symlink_metadata(&lock_dir) {
            Ok(metadata) if metadata.is_dir() => ensure_private_directory(&lock_dir)?,
            Ok(_) => return Err(SessionError::UnsafePath(lock_dir.display().to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&lock_dir)?;
                ensure_private_directory(&lock_dir)?;
            }
            Err(error) => return Err(SessionError::Io(error)),
        }

        let path = lock_dir.join("operation.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let file = options.open(&path)?;
        ensure_private_file(&path, "operation.lock")?;

        #[cfg(unix)]
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(SessionError::Io(std::io::Error::last_os_error()));
        }

        Ok(Self { file })
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn operation_lock_dir() -> Result<std::path::PathBuf, SessionError> {
    dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")))
        .map(|root| root.join("hyprloom"))
        .ok_or_else(|| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine a user state directory for the operation lock",
            ))
        })
}

pub fn save_session(session: &Session, sessions_dir: &Path) -> Result<(), SessionError> {
    validate_session_structure(session)?;
    ensure_sessions_dir(sessions_dir)?;
    let path = sessions_dir.join(format!("{}.json", session.name));
    let json = serde_json::to_string_pretty(session)?;
    if json.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge(format!(
            "serialized session is larger than {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    atomic_write(&path, json.as_bytes())?;
    Ok(())
}

fn ensure_sessions_dir(sessions_dir: &Path) -> Result<(), SessionError> {
    match std::fs::symlink_metadata(sessions_dir) {
        Ok(metadata) if metadata.is_dir() => ensure_private_directory(sessions_dir),
        Ok(_) => Err(SessionError::UnsafePath(sessions_dir.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(sessions_dir)?;
            match std::fs::symlink_metadata(sessions_dir) {
                Ok(metadata) if metadata.is_dir() => ensure_private_directory(sessions_dir),
                Ok(_) => Err(SessionError::UnsafePath(sessions_dir.display().to_string())),
                Err(error) => Err(SessionError::Io(error)),
            }
        }
        Err(error) => Err(SessionError::Io(error)),
    }
}

fn existing_sessions_dir(sessions_dir: &Path) -> Result<bool, SessionError> {
    match std::fs::symlink_metadata(sessions_dir) {
        Ok(metadata) if metadata.is_dir() => {
            ensure_private_directory(sessions_dir)?;
            Ok(true)
        }
        Ok(_) => Err(SessionError::UnsafePath(sessions_dir.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(SessionError::Io(error)),
    }
}

pub fn load_session(name: &str, sessions_dir: &Path) -> Result<Session, SessionError> {
    validate_session_name(name)?;
    if !existing_sessions_dir(sessions_dir)? {
        return Err(SessionError::NotFound(name.to_string()));
    }
    let path = sessions_dir.join(format!("{name}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(SessionError::UnsafePath(name.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionError::NotFound(name.to_string()))
        }
        Err(error) => return Err(SessionError::Io(error)),
    }
    ensure_private_file(&path, name)?;
    let content = read_limited_file(&path, name)?;
    let session = serde_json::from_slice(&content)?;
    validate_session_structure(&session)?;
    if session.name != name {
        return Err(SessionError::NameMismatch {
            requested: name.to_string(),
            actual: session.name,
        });
    }
    Ok(session)
}

pub fn list_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    if !existing_sessions_dir(sessions_dir)? {
        return Ok(vec![]);
    }
    let mut summaries = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_regular_file = entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false);
        if is_regular_file
            && path.extension().map(|e| e == "json").unwrap_or(false)
            && ensure_private_file(&path, &entry.file_name().to_string_lossy()).is_ok()
        {
            if let Ok(content) = read_limited_file(&path, &entry.file_name().to_string_lossy()) {
                if let Ok(session) = serde_json::from_slice::<Session>(&content) {
                    if validate_session_structure(&session).is_err() {
                        continue;
                    }
                    let Some(file_stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    if session.name != file_stem {
                        // A payload name must not redirect rotation or deletion
                        // to a different filename in the session directory.
                        continue;
                    }
                    summaries.push(SessionSummary {
                        name: file_stem.to_string(),
                        created_at: session.created_at,
                        client_count: session.clients.len(),
                    });
                }
            }
        }
    }
    summaries.sort_by(|left, right| {
        Reverse(left.created_at)
            .cmp(&Reverse(right.created_at))
            .then(left.name.cmp(&right.name))
    });
    Ok(summaries)
}

pub fn delete_session(name: &str, sessions_dir: &Path) -> Result<(), SessionError> {
    validate_session_name(name)?;
    if !existing_sessions_dir(sessions_dir)? {
        return Err(SessionError::NotFound(name.to_string()));
    }
    let path = sessions_dir.join(format!("{name}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(SessionError::UnsafePath(name.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionError::NotFound(name.to_string()))
        }
        Err(error) => return Err(SessionError::Io(error)),
    }
    ensure_private_file(&path, name)?;
    std::fs::remove_file(path)?;
    Ok(())
}

pub fn session_exists(name: &str, sessions_dir: &Path) -> bool {
    if validate_session_name(name).is_err() {
        return false;
    }
    if !matches!(existing_sessions_dir(sessions_dir), Ok(true)) {
        return false;
    }
    let path = sessions_dir.join(format!("{name}.json"));
    std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
        && ensure_private_file(&path, name).is_ok()
}

pub fn validate_session_name(name: &str) -> Result<(), SessionError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(SessionError::InvalidName(name.to_string()));
    }
    Ok(())
}

/// Validate a name supplied by a user.  Autosave names are reserved for the
/// helper so retention can never prune a manually saved session by accident.
pub fn validate_user_session_name(name: &str) -> Result<(), SessionError> {
    validate_session_name(name)?;
    if name.starts_with(AUTOSAVE_PREFIX) {
        return Err(SessionError::InvalidName(format!(
            "{name} (the '{AUTOSAVE_PREFIX}' prefix is reserved for autosaves)"
        )));
    }
    Ok(())
}

/// Copy legacy hyprflow sessions into the fork's storage without removing or
/// overwriting anything.  This is intentionally idempotent so a user can run
/// the fork repeatedly while keeping the original files as a rollback path.
pub fn migrate_legacy_sessions(
    sessions_dir: &Path,
    legacy_sessions_dir: &Path,
) -> Result<usize, SessionError> {
    if sessions_dir == legacy_sessions_dir {
        return Ok(0);
    }
    match std::fs::symlink_metadata(legacy_sessions_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(SessionError::Io(error)),
    }

    ensure_sessions_dir(sessions_dir)?;
    ensure_private_directory(legacy_sessions_dir)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(legacy_sessions_dir)? {
        let entry = entry?;
        let source = entry.path();
        let source_is_regular = entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false);
        if source_is_regular && source.extension().map(|ext| ext == "json").unwrap_or(false) {
            let destination = sessions_dir.join(entry.file_name());
            if std::fs::symlink_metadata(&destination).is_ok() {
                continue;
            }
            let name = source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if validate_session_name(name).is_err() {
                continue;
            }
            if ensure_private_file(&source, name).is_err() {
                // A single unsafe legacy file must not prevent healthy
                // snapshots beside it from being migrated.
                continue;
            }
            let Ok(content) = read_limited_file(&source, name) else {
                continue;
            };
            let Ok(session) = serde_json::from_slice::<Session>(&content) else {
                continue;
            };
            if validate_session_structure(&session).is_err() {
                continue;
            }
            if session.name != name {
                // Never copy a payload under a filename that names a
                // different session.  This also keeps a later load from
                // interpreting the migrated file as the requested preset.
                continue;
            }
            atomic_write(&destination, &content)?;
            copied += 1;
        }
    }
    Ok(copied)
}

// === Autosave helpers ===

pub const AUTOSAVE_PREFIX: &str = "autosave-";
const REPLACE_MARKER_NAME: &str = ".replace-in-progress";
static AUTOSAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn autosave_name_now() -> String {
    let now = Utc::now();
    let sequence = AUTOSAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "autosave-{}-{}-{}",
        now.format("%Y%m%dT%H%M%S%f"),
        std::process::id(),
        sequence
    )
}

/// Record that a replace operation has a saved desktop which can be used for
/// recovery if the helper is interrupted after it starts closing windows.
pub fn mark_replace_in_progress(
    backup_name: &str,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    mark_replace_in_progress_for_target(backup_name, None, sessions_dir)
}

pub fn mark_replace_in_progress_for_target(
    backup_name: &str,
    target_name: Option<&str>,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::InProgress,
            closing_address: None,
            target_name: target_name.map(str::to_string),
        },
        sessions_dir,
    )
}

/// Record the address of the first window whose close dispatch is about to be
/// sent. Startup can use this evidence to distinguish a marker written just
/// before a failed dispatch from a replacement that actually started closing
/// the desktop.
pub fn mark_replace_closing(
    backup_name: &str,
    sessions_dir: &std::path::Path,
    address: &str,
) -> Result<(), SessionError> {
    mark_replace_closing_for_target(backup_name, None, sessions_dir, address)
}

pub fn mark_replace_closing_for_target(
    backup_name: &str,
    target_name: Option<&str>,
    sessions_dir: &std::path::Path,
    address: &str,
) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    validate_replace_address(address)?;
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Closing,
            closing_address: Some(address.to_string()),
            target_name: target_name.map(str::to_string),
        },
        sessions_dir,
    )
}

/// Record that a replacement has been prepared but has not started closing
/// the desktop yet.  A crash in this phase must not roll back user activity
/// that happened after the safety snapshot was captured.
pub fn mark_replace_prepared(
    backup_name: &str,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    mark_replace_prepared_for_target(backup_name, None, sessions_dir)
}

pub fn mark_replace_prepared_for_target(
    backup_name: &str,
    target_name: Option<&str>,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Prepared,
            closing_address: None,
            target_name: target_name.map(str::to_string),
        },
        sessions_dir,
    )
}

pub fn mark_replace_committed(
    backup_name: &str,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    mark_replace_committed_for_target(backup_name, None, sessions_dir)
}

pub fn mark_replace_committed_for_target(
    backup_name: &str,
    target_name: Option<&str>,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Committed,
            closing_address: None,
            target_name: target_name.map(str::to_string),
        },
        sessions_dir,
    )
}

pub fn replace_marker(
    sessions_dir: &std::path::Path,
) -> Result<Option<ReplaceMarker>, SessionError> {
    if !existing_sessions_dir(sessions_dir)? {
        return Ok(None);
    }
    let path = sessions_dir.join(REPLACE_MARKER_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(SessionError::UnsafePath(REPLACE_MARKER_NAME.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SessionError::Io(error)),
    }
    ensure_private_file(&path, REPLACE_MARKER_NAME)?;
    let content = read_limited_file(&path, REPLACE_MARKER_NAME)?;
    let text = std::str::from_utf8(&content)
        .map_err(|error| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            ))
        })?
        .trim()
        .to_string();
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.first().copied().unwrap_or_default();
    let (phase, name_index, base_len, closing_index) = match first {
        "prepared" => (ReplacePhase::Prepared, 1, 2, None),
        // Older builds wrote only the backup name.  Treat that format as an
        // interrupted replacement so upgrading cannot silently skip recovery.
        "closing" => (ReplacePhase::Closing, 1, 3, Some(2)),
        "in-progress" => (ReplacePhase::InProgress, 1, 2, None),
        "committed" => (ReplacePhase::Committed, 1, 2, None),
        _legacy_name => (ReplacePhase::InProgress, 0, 1, None),
    };
    if lines.len() < base_len || lines.len() > base_len + 1 {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replacement marker has unexpected extra lines",
        )));
    }
    let name = lines.get(name_index).copied().unwrap_or_default();
    let closing_address =
        closing_index.map(|index| lines.get(index).copied().unwrap_or_default().to_string());
    let target_name = lines.get(base_len).map(|target| (*target).to_string());
    validate_session_name(name)?;
    if let Some(address) = &closing_address {
        validate_replace_address(address)?;
    }
    if let Some(target_name) = &target_name {
        validate_session_name(target_name)?;
    }
    Ok(Some(ReplaceMarker {
        backup_name: name.to_string(),
        phase,
        closing_address,
        target_name,
    }))
}

pub fn pending_replace_backup(
    sessions_dir: &std::path::Path,
) -> Result<Option<String>, SessionError> {
    Ok(replace_marker(sessions_dir)?.map(|marker| marker.backup_name))
}

fn write_replace_marker(
    marker: &ReplaceMarker,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    if let Some(target_name) = &marker.target_name {
        validate_session_name(target_name)?;
    }
    let content = match marker.phase {
        ReplacePhase::Prepared => format_marker_content("prepared", marker, None),
        ReplacePhase::Closing => {
            let address = marker.closing_address.as_deref().ok_or_else(|| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "closing replacement marker has no window address",
                ))
            })?;
            validate_replace_address(address)?;
            format_marker_content("closing", marker, Some(address))
        }
        ReplacePhase::InProgress => format_marker_content("in-progress", marker, None),
        ReplacePhase::Committed => format_marker_content("committed", marker, None),
    };
    atomic_write(&sessions_dir.join(REPLACE_MARKER_NAME), content.as_bytes())
}

fn format_marker_content(
    phase: &str,
    marker: &ReplaceMarker,
    closing_address: Option<&str>,
) -> String {
    let mut content = format!("{phase}\n{}", marker.backup_name);
    if let Some(address) = closing_address {
        content.push('\n');
        content.push_str(address);
    }
    if let Some(target_name) = &marker.target_name {
        content.push('\n');
        content.push_str(target_name);
    }
    content
}

fn validate_replace_address(address: &str) -> Result<(), SessionError> {
    if address.is_empty()
        || address.len() > MAX_SESSION_STRING_BYTES
        || address.contains(['\r', '\n'])
    {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement marker has an invalid window address",
        )));
    }
    Ok(())
}

pub fn clear_replace_marker(sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    if !existing_sessions_dir(sessions_dir)? {
        return Ok(());
    }
    let path = sessions_dir.join(REPLACE_MARKER_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            ensure_private_file(&path, REPLACE_MARKER_NAME)?;
            std::fs::remove_file(path)?;
        }
        Ok(_) => return Err(SessionError::UnsafePath(REPLACE_MARKER_NAME.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(SessionError::Io(error)),
    }
    Ok(())
}

/// Returns autosave sessions only (name starts with `AUTOSAVE_PREFIX`),
/// sorted by name descending. The timestamp, process ID, and sequence suffix
/// make names unique even when multiple captures happen in one second.
pub fn list_autosave_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let mut all = list_sessions(sessions_dir)?;
    all.retain(|s| s.name.starts_with(AUTOSAVE_PREFIX));
    // Sort by name descending — autosave-YYYYMMDDTHHMMSS sorts lexicographically
    all.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(all)
}

/// Deletes the oldest autosave sessions, keeping only the `retain` newest.
/// Returns the count of sessions deleted. Non-autosave sessions are untouched.
pub fn rotate_autosaves(sessions_dir: &Path, retain: usize) -> Result<usize, SessionError> {
    let retain = retain.min(MAX_AUTOSAVE_RETAIN);
    let pending_backup = pending_replace_backup(sessions_dir)?;
    let autosaves = list_autosave_sessions(sessions_dir)?;
    let mut pruned = 0;
    if autosaves.len() > retain {
        for session in &autosaves[retain..] {
            if pending_backup.as_deref() == Some(session.name.as_str()) {
                continue;
            }
            match delete_session(&session.name, sessions_dir) {
                Ok(()) => pruned += 1,
                // Another autosave rotation may have removed it already.
                // Rotation is intentionally idempotent in that case.
                Err(SessionError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(pruned)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), SessionError> {
    let parent = path.parent().ok_or_else(|| {
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path has no parent directory",
        ))
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");

    for _ in 0..100 {
        let sequence = AUTOSAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SessionError::Io(error)),
        };

        let write_result = file.write_all(contents).and_then(|_| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temporary);
            return Err(SessionError::Io(error));
        }

        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(SessionError::Io(error));
        }
        #[cfg(unix)]
        {
            let directory = OpenOptions::new().read(true).open(parent)?;
            directory.sync_all()?;
        }
        return Ok(());
    }

    Err(SessionError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary session path",
    )))
}

fn read_limited_file(path: &Path, label: &str) -> Result<Vec<u8>, SessionError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SessionError::UnsafePath(label.to_string()));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o077 != 0 {
        return Err(SessionError::UnsafePath(label.to_string()));
    }
    if metadata.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge(format!(
            "'{label}' is larger than {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }

    let mut content = Vec::with_capacity(metadata.len().min(MAX_SESSION_FILE_BYTES) as usize);
    Read::by_ref(&mut file)
        .take(MAX_SESSION_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut content)?;
    if content.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge(format!(
            "'{label}' is larger than {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    Ok(content)
}

fn ensure_private_directory(path: &Path) -> Result<(), SessionError> {
    ensure_private_path(path, true)
}

fn ensure_private_file(path: &Path, label: &str) -> Result<(), SessionError> {
    ensure_private_path(path, false).map_err(|error| match error {
        SessionError::UnsafePath(_) => SessionError::UnsafePath(label.to_string()),
        other => other,
    })
}

fn ensure_private_path(path: &Path, directory: bool) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(SessionError::UnsafePath(path.display().to_string()));
        }
        let expected_mode = if directory { 0o700 } else { 0o600 };
        if metadata.permissions().mode() & 0o777 != expected_mode {
            let mut permissions = metadata.permissions();
            permissions.set_mode(expected_mode);
            std::fs::set_permissions(path, permissions)?;
        }
        let verified = std::fs::symlink_metadata(path)?;
        if verified.uid() != unsafe { libc::geteuid() }
            || verified.permissions().mode() & 0o077 != 0
        {
            return Err(SessionError::UnsafePath(path.display().to_string()));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}

fn validate_session_structure(session: &Session) -> Result<(), SessionError> {
    validate_session_name(&session.name)?;
    validate_text("session version", &session.hyprland_version)?;
    if session.monitors.len() > MAX_SESSION_MONITORS {
        return Err(SessionError::TooLarge(format!(
            "more than {MAX_SESSION_MONITORS} monitors"
        )));
    }
    if session.clients.len() > MAX_SESSION_CLIENTS {
        return Err(SessionError::TooLarge(format!(
            "more than {MAX_SESSION_CLIENTS} clients"
        )));
    }
    if session.brave_profiles.len() > MAX_SESSION_BRAVE_PROFILES {
        return Err(SessionError::TooLarge(format!(
            "more than {MAX_SESSION_BRAVE_PROFILES} browser profiles"
        )));
    }

    for monitor in &session.monitors {
        validate_text("monitor name", &monitor.name)?;
    }
    for client in &session.clients {
        for (label, value) in [
            ("client class", &client.class),
            ("client title", &client.title),
            ("client initial class", &client.initial_class),
            ("client initial title", &client.initial_title),
            ("client workspace name", &client.workspace_name),
            ("client monitor", &client.monitor),
            ("launch command", &client.launch.command),
        ] {
            validate_text(label, value)?;
        }
        if let Some(profile) = &client.profile_directory {
            validate_text("client profile directory", profile)?;
        }
        if let Some(hint) = &client.launch.hint {
            validate_text("launch hint", hint)?;
        }
        if client.launch.args.len() > MAX_SESSION_ARGS {
            return Err(SessionError::TooLarge(format!(
                "more than {MAX_SESSION_ARGS} launch arguments"
            )));
        }
        for argument in &client.launch.args {
            validate_text("launch argument", argument)?;
        }
    }
    for profile in &session.brave_profiles {
        validate_text("browser profile directory", &profile.directory)?;
        validate_text("browser profile name", &profile.name)?;
    }
    Ok(())
}

fn validate_text(label: &str, value: &str) -> Result<(), SessionError> {
    if value.len() > MAX_SESSION_STRING_BYTES {
        return Err(SessionError::TooLarge(format!(
            "{label} is longer than {MAX_SESSION_STRING_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Parses a human-readable duration string into a `chrono::Duration`.
///
/// Supported suffixes: `m` (minutes), `h` (hours), `d` (days).
/// Examples: `"30m"`, `"24h"`, `"7d"`.
pub fn parse_max_age(s: &str) -> Result<chrono::Duration, String> {
    if s.len() < 2 {
        return Err(format!("invalid duration: '{s}'"));
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration: '{s}'"))?;
    if num <= 0 {
        return Err(format!("duration must be greater than zero: '{s}'"));
    }
    let duration = match unit {
        "m" => chrono::Duration::try_minutes(num),
        "h" => chrono::Duration::try_hours(num),
        "d" => chrono::Duration::try_days(num),
        _ => {
            return Err(format!(
                "invalid duration unit '{unit}' in '{s}'. Use m, h, or d."
            ))
        }
    };
    duration.ok_or_else(|| format!("duration is out of range: '{s}'"))
}

// === Raw hyprctl JSON structs (what hyprctl returns) ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprClient {
    pub address: String,
    pub class: String,
    pub title: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprWorkspace {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprMonitor {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub transform: u32,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    #[test]
    fn test_session_roundtrip() {
        let session = Session {
            name: "work".to_string(),
            created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            hyprland_version: "0.47.0".to_string(),
            monitors: vec![Monitor {
                name: "DP-4".to_string(),
                width: 2560,
                height: 1440,
                transform: 0,
                x: None,
                y: None,
            }],
            clients: vec![SessionClient {
                class: "kitty".to_string(),
                title: "Claude Code".to_string(),
                initial_class: "kitty".to_string(),
                initial_title: "kitty".to_string(),
                workspace: 4,
                workspace_name: "4".to_string(),
                monitor: "DP-4".to_string(),
                at: [12, 50],
                size: [842, 1378],
                floating: false,
                fullscreen: 0,
                pinned: false,
                profile_directory: None,
                profile_identity_ambiguous: false,
                focus_history_id: 3,
                launch: LaunchInfo {
                    command: "kitty".to_string(),
                    args: vec![],
                    hint: None,
                },
            }],
            brave_profiles: vec![],
        };

        let json = serde_json::to_string(&session).expect("serialization failed");
        let restored: Session = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(restored.name, session.name);
        assert_eq!(restored.hyprland_version, session.hyprland_version);
        assert_eq!(restored.created_at, session.created_at);

        assert_eq!(restored.monitors.len(), 1);
        let mon = &restored.monitors[0];
        assert_eq!(mon.name, "DP-4");
        assert_eq!(mon.width, 2560);
        assert_eq!(mon.height, 1440);
        assert_eq!(mon.transform, 0);

        assert_eq!(restored.clients.len(), 1);
        let client = &restored.clients[0];
        assert_eq!(client.class, "kitty");
        assert_eq!(client.title, "Claude Code");
        assert_eq!(client.workspace, 4);
        assert_eq!(client.monitor, "DP-4");
        assert_eq!(client.at, [12, 50]);
        assert_eq!(client.size, [842, 1378]);
        assert!(!client.floating);
        assert_eq!(client.fullscreen, 0);
        assert_eq!(client.focus_history_id, 3);
        assert_eq!(client.launch.command, "kitty");
        assert!(client.launch.args.is_empty());
        assert!(client.launch.hint.is_none());
    }

    fn make_test_session(name: &str) -> Session {
        Session {
            name: name.to_string(),
            created_at: Utc::now(),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![SessionClient {
                class: "kitty".to_string(),
                title: "test".to_string(),
                initial_class: "kitty".to_string(),
                initial_title: "kitty".to_string(),
                workspace: 1,
                workspace_name: "1".to_string(),
                monitor: "DP-4".to_string(),
                at: [0, 0],
                size: [800, 600],
                floating: false,
                fullscreen: 0,
                pinned: false,
                profile_directory: None,
                profile_identity_ambiguous: false,
                focus_history_id: 0,
                launch: LaunchInfo {
                    command: "kitty".to_string(),
                    args: vec![],
                    hint: None,
                },
            }],
            brave_profiles: vec![],
        }
    }

    #[test]
    fn test_session_roundtrip_with_brave_profiles() {
        let session = Session {
            name: "brave-test".to_string(),
            created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            hyprland_version: "0.54.1".to_string(),
            monitors: vec![],
            clients: vec![],
            brave_profiles: vec![
                BraveProfile {
                    directory: "Default".to_string(),
                    name: "Credifit".to_string(),
                },
                BraveProfile {
                    directory: "Profile 1".to_string(),
                    name: "LinkPJ".to_string(),
                },
            ],
        };

        let json = serde_json::to_string(&session).expect("serialization failed");
        let restored: Session = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(restored.brave_profiles.len(), 2);
        assert_eq!(restored.brave_profiles[0].directory, "Default");
        assert_eq!(restored.brave_profiles[0].name, "Credifit");
        assert_eq!(restored.brave_profiles[1].directory, "Profile 1");
        assert_eq!(restored.brave_profiles[1].name, "LinkPJ");
    }

    #[test]
    fn test_session_backward_compat_no_brave_profiles() {
        // A session JSON without the brave_profiles field (as saved by older versions).
        let json = r#"{
            "name": "old-session",
            "created_at": "2026-03-08T10:00:00Z",
            "hyprland_version": "0.54.0",
            "monitors": [],
            "clients": []
        }"#;

        let session: Session = serde_json::from_str(json).expect("deserialization must succeed");
        assert_eq!(
            session.brave_profiles.len(),
            0,
            "missing brave_profiles field should default to empty vec"
        );
    }

    #[test]
    fn test_session_backward_compat_defaults_new_client_fields() {
        let json = r#"{
            "name": "old-client",
            "created_at": "2026-03-08T10:00:00Z",
            "hyprland_version": "0.54.0",
            "monitors": [],
            "clients": [{
                "class": "kitty",
                "title": "kitty",
                "workspace": 1,
                "monitor": "DP-1",
                "at": [0, 0],
                "size": [800, 600],
                "floating": false,
                "fullscreen": 0,
                "focus_history_id": 0,
                "launch": {"command": "kitty", "args": [], "hint": null}
            }]
        }"#;

        let session: Session = serde_json::from_str(json).expect("old session must load");
        let client = &session.clients[0];
        assert_eq!(client.workspace_name, "");
        assert!(!client.pinned);
        assert!(client.profile_directory.is_none());
    }

    #[test]
    fn test_session_backward_compat_accepts_null_new_client_fields() {
        let json = r#"{
            "name": "old-client-null-fields",
            "created_at": "2026-03-08T10:00:00Z",
            "hyprland_version": "0.54.0",
            "monitors": [],
            "clients": [{
                "class": "kitty",
                "title": "kitty",
                "initial_class": null,
                "initial_title": null,
                "workspace": 1,
                "workspace_name": null,
                "monitor": "DP-1",
                "at": [0, 0],
                "size": [800, 600],
                "floating": false,
                "fullscreen": 0,
                "pinned": null,
                "profile_directory": null,
                "profile_identity_ambiguous": null,
                "focus_history_id": 0,
                "launch": {"command": "kitty", "args": [], "hint": null}
            }]
        }"#;

        let session: Session =
            serde_json::from_str(json).expect("null compatibility fields must load");
        let client = &session.clients[0];
        assert_eq!(client.initial_class, "");
        assert_eq!(client.initial_title, "");
        assert_eq!(client.workspace_name, "");
        assert!(!client.pinned);
        assert!(client.profile_directory.is_none());
        assert!(!client.profile_identity_ambiguous);
    }

    #[test]
    fn test_save_and_load_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = make_test_session("work");
        save_session(&session, dir.path()).unwrap();
        let loaded = load_session("work", dir.path()).unwrap();
        assert_eq!(loaded.name, "work");
        assert_eq!(loaded.clients.len(), 1);
    }

    #[test]
    fn test_migrate_legacy_sessions_is_idempotent_and_non_destructive() {
        let legacy = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        save_session(&make_test_session("work"), legacy.path()).unwrap();

        assert_eq!(
            migrate_legacy_sessions(current.path(), legacy.path()).unwrap(),
            1
        );
        assert_eq!(load_session("work", current.path()).unwrap().name, "work");

        // Existing fork data is never overwritten, and a second pass copies nothing.
        let existing = make_test_session("work");
        save_session(&existing, current.path()).unwrap();
        assert_eq!(
            migrate_legacy_sessions(current.path(), legacy.path()).unwrap(),
            0
        );
        assert_eq!(load_session("work", current.path()).unwrap().name, "work");
    }

    #[test]
    fn test_migrate_legacy_sessions_skips_bad_files_and_keeps_valid_files() {
        let legacy = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        let malformed = legacy.path().join("malformed.json");
        std::fs::write(&malformed, b"not json").unwrap();
        ensure_private_file(&malformed, "malformed").unwrap();

        let oversized = legacy.path().join("oversized.json");
        std::fs::write(&oversized, vec![b'x'; MAX_SESSION_FILE_BYTES as usize + 1]).unwrap();
        ensure_private_file(&oversized, "oversized").unwrap();

        save_session(&make_test_session("healthy"), legacy.path()).unwrap();

        assert_eq!(
            migrate_legacy_sessions(current.path(), legacy.path()).unwrap(),
            1
        );
        assert_eq!(
            load_session("healthy", current.path()).unwrap().name,
            "healthy"
        );
        assert!(!current.path().join("malformed.json").exists());
        assert!(!current.path().join("oversized.json").exists());
    }

    #[test]
    fn test_list_sessions() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("a"), dir.path()).unwrap();
        save_session(&make_test_session("b"), dir.path()).unwrap();
        let list = list_sessions(dir.path()).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_delete_session() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("old"), dir.path()).unwrap();
        delete_session("old", dir.path()).unwrap();
        assert!(load_session("old", dir.path()).is_err());
    }

    #[test]
    fn test_load_nonexistent_session() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_session("nope", dir.path()).is_err());
    }

    #[test]
    fn test_list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let list = list_sessions(dir.path()).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_session_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!session_exists("x", dir.path()));
        save_session(&make_test_session("x"), dir.path()).unwrap();
        assert!(session_exists("x", dir.path()));
    }

    #[test]
    fn test_session_names_cannot_escape_storage_directory() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../escape", "nested/name", "..", "", "bad name"] {
            let mut session = make_test_session("safe");
            session.name = name.to_string();

            assert!(save_session(&session, dir.path()).is_err());
            assert!(load_session(name, dir.path()).is_err());
            assert!(delete_session(name, dir.path()).is_err());
            assert!(!session_exists(name, dir.path()));
        }
    }

    #[test]
    fn test_autosave_name_format() {
        let name = autosave_name_now();
        assert!(name.starts_with("autosave-"));
        let parts: Vec<&str> = name[9..].split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 24);
        assert_eq!(&parts[0][8..9], "T");
    }

    #[test]
    fn test_autosave_names_are_unique_within_a_process() {
        assert_ne!(autosave_name_now(), autosave_name_now());
    }

    #[test]
    fn test_replace_recovery_marker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        mark_replace_prepared("autosave-recovery", dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Prepared,
                closing_address: None,
                target_name: None,
            })
        );
        mark_replace_closing("autosave-recovery", dir.path(), "0xclosing").unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Closing,
                closing_address: Some("0xclosing".to_string()),
                target_name: None,
            })
        );
        mark_replace_in_progress("autosave-recovery", dir.path()).unwrap();
        assert_eq!(
            pending_replace_backup(dir.path()).unwrap().as_deref(),
            Some("autosave-recovery")
        );
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::InProgress,
                closing_address: None,
                target_name: None,
            })
        );
        mark_replace_committed("autosave-recovery", dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Committed,
                closing_address: None,
                target_name: None,
            })
        );
        mark_replace_prepared_for_target("autosave-recovery", Some("target"), dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Prepared,
                closing_address: None,
                target_name: Some("target".to_string()),
            })
        );
        clear_replace_marker(dir.path()).unwrap();
        assert_eq!(pending_replace_backup(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_list_ignores_payload_filename_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("target"), dir.path()).unwrap();
        let mismatched = serde_json::to_vec(&make_test_session("target")).unwrap();
        let path = dir.path().join("autosave-old.json");
        std::fs::write(&path, mismatched).unwrap();
        ensure_private_file(&path, "autosave-old").unwrap();

        let sessions = list_sessions(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "target");
        assert!(matches!(
            load_session("autosave-old", dir.path()),
            Err(SessionError::NameMismatch { .. })
        ));
    }

    #[test]
    fn test_save_rejects_serialized_session_over_file_limit() {
        let dir = tempfile::tempdir().unwrap();
        let base = make_test_session("large");
        let mut session = base.clone();
        session.clients = (0..300)
            .map(|_| {
                let mut client = base.clients[0].clone();
                client.title = "x".repeat(MAX_SESSION_STRING_BYTES);
                client
            })
            .collect();

        assert!(matches!(
            save_session(&session, dir.path()),
            Err(SessionError::TooLarge(_))
        ));
        assert!(!dir.path().join("large.json").exists());
    }

    #[test]
    fn test_list_autosave_sessions_filters_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("work"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T110000"), dir.path()).unwrap();

        let autosaves = list_autosave_sessions(dir.path()).unwrap();
        assert_eq!(autosaves.len(), 2);
        assert_eq!(autosaves[0].name, "autosave-20260309T110000");
        assert_eq!(autosaves[1].name, "autosave-20260309T100000");
    }

    #[test]
    fn test_rotate_autosaves_keeps_n() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T110000"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T120000"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T130000"), dir.path()).unwrap();
        save_session(&make_test_session("work"), dir.path()).unwrap();

        let pruned = rotate_autosaves(dir.path(), 2).unwrap();
        assert_eq!(pruned, 2);

        let remaining = list_autosave_sessions(dir.path()).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].name, "autosave-20260309T130000");
        assert_eq!(remaining[1].name, "autosave-20260309T120000");

        assert!(session_exists("work", dir.path()));
    }

    #[test]
    fn test_rotate_autosaves_keeps_pending_replace_backup() {
        let dir = tempfile::tempdir().unwrap();
        let oldest = "autosave-20260309T100000";
        save_session(&make_test_session(oldest), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T110000"), dir.path()).unwrap();
        save_session(&make_test_session("autosave-20260309T120000"), dir.path()).unwrap();
        mark_replace_in_progress(oldest, dir.path()).unwrap();

        let pruned = rotate_autosaves(dir.path(), 1).unwrap();

        assert_eq!(pruned, 1);
        assert!(session_exists(oldest, dir.path()));
        assert!(session_exists("autosave-20260309T120000", dir.path()));
        assert!(!session_exists("autosave-20260309T110000", dir.path()));
    }

    #[test]
    fn test_rotate_autosaves_noop_when_under_limit() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();

        let pruned = rotate_autosaves(dir.path(), 5).unwrap();
        assert_eq!(pruned, 0);
        assert_eq!(list_autosave_sessions(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn test_rotate_autosaves_retain_zero_removes_all_autosaves() {
        let dir = tempfile::tempdir().unwrap();
        save_session(&make_test_session("autosave-20260309T100000"), dir.path()).unwrap();

        let pruned = rotate_autosaves(dir.path(), 0).unwrap();
        assert_eq!(pruned, 1);
        assert!(list_autosave_sessions(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_max_age("30m").unwrap(), chrono::Duration::minutes(30));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_max_age("24h").unwrap(), chrono::Duration::hours(24));
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_max_age("7d").unwrap(), chrono::Duration::days(7));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_max_age("abc").is_err());
        assert!(parse_max_age("10x").is_err());
        assert!(parse_max_age("").is_err());
        assert!(parse_max_age("0m").is_err());
        assert!(parse_max_age("-1h").is_err());
        assert!(parse_max_age("9223372036854775807d").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_save_replaces_destination_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let original = b"must remain untouched";
        std::fs::write(outside.path(), original).unwrap();
        symlink(outside.path(), dir.path().join("work.json")).unwrap();

        save_session(&make_test_session("work"), dir.path()).unwrap();

        assert_eq!(std::fs::read(outside.path()).unwrap(), original);
        assert_eq!(load_session("work", dir.path()).unwrap().name, "work");
        assert!(!std::fs::symlink_metadata(dir.path().join("work.json"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn test_save_rejects_symlinked_sessions_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions_dir = root.path().join("sessions");
        symlink(outside.path(), &sessions_dir).unwrap();

        let error = save_session(&make_test_session("work"), &sessions_dir)
            .expect_err("symlinked sessions directory must be rejected");
        assert!(matches!(error, SessionError::UnsafePath(_)));
        assert!(!outside.path().join("work.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_session_storage_is_restricted_to_user_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut directory_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        directory_permissions.set_mode(0o755);
        std::fs::set_permissions(dir.path(), directory_permissions).unwrap();

        save_session(&make_test_session("work"), dir.path()).unwrap();

        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("work.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn test_oversized_session_file_is_rejected_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.json");
        std::fs::write(&path, vec![b' '; MAX_SESSION_FILE_BYTES as usize + 1]).unwrap();

        let error = load_session("work", dir.path()).expect_err("oversized session must fail");
        assert!(matches!(error, SessionError::TooLarge(_)));
    }

    #[test]
    fn test_structurally_oversized_session_is_rejected_before_save() {
        let mut session = make_test_session("work");
        session.clients = vec![session.clients[0].clone(); MAX_SESSION_CLIENTS + 1];

        let error = save_session(&session, tempfile::tempdir().unwrap().path())
            .expect_err("too many clients must fail");
        assert!(matches!(error, SessionError::TooLarge(_)));
    }

    #[test]
    fn test_parse_hyprctl_clients_fixture() {
        let raw = include_str!("../tests/fixtures/sample_clients.json");
        let clients: Vec<HyprClient> = serde_json::from_str(raw).expect("fixture parse failed");

        assert_eq!(clients.len(), 3);

        // First client: kitty
        let kitty = &clients[0];
        assert_eq!(kitty.address, "0x55c46f7e1350");
        assert_eq!(kitty.class, "kitty");
        assert_eq!(kitty.initial_class, "kitty");
        assert_eq!(kitty.title, "Claude Code");
        assert_eq!(kitty.workspace.id, 4);
        assert_eq!(kitty.workspace.name, "4");
        assert_eq!(kitty.monitor, 0);
        assert_eq!(kitty.at, [12, 50]);
        assert_eq!(kitty.size, [842, 1378]);
        assert!(!kitty.floating);
        assert_eq!(kitty.fullscreen, 0);
        assert_eq!(kitty.focus_history_id, 3);
        assert_eq!(kitty.pid, 9537);

        // Second client: brave-browser
        let brave = &clients[1];
        assert_eq!(brave.class, "brave-browser");
        assert_eq!(brave.workspace.id, 1);
        assert_eq!(brave.focus_history_id, 1);

        // Third client: obsidian
        let obsidian = &clients[2];
        assert_eq!(obsidian.class, "obsidian");
        assert_eq!(obsidian.title, "smart notes - Obsidian");
        assert_eq!(obsidian.workspace.id, 3);
        assert_eq!(obsidian.focus_history_id, 2);
        assert_eq!(obsidian.pid, 5000);
    }
}
