use crate::config::MAX_AUTOSAVE_RETAIN;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use crate::platform::current_user_id;

/// Maximum serialized session size accepted by the storage layer.
pub const MAX_SESSION_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum number of clients accepted in one session.
pub const MAX_SESSION_CLIENTS: usize = 4_096;
/// Maximum number of monitors accepted in one session.
pub const MAX_SESSION_MONITORS: usize = 128;
/// Maximum number of Brave profiles accepted in one session.
pub const MAX_SESSION_BRAVE_PROFILES: usize = 1_024;
/// Maximum number of launch arguments accepted for one client.
pub const MAX_SESSION_ARGS: usize = 2_048;
/// Maximum length of any session string, in bytes.
pub const MAX_SESSION_STRING_BYTES: usize = 64 * 1024;

// Older session writers sometimes emitted JSON null for fields that were
// introduced as optional compatibility fields.  Treat null the same as a
// missing value so those snapshots remain usable after an upgrade.
fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(std::option::Option::unwrap_or_default)
}

fn deserialize_nullable_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<bool>::deserialize(deserializer).map(std::option::Option::unwrap_or_default)
}

// === Hyprloom session structs (what we save to disk) ===

/// A Brave profile captured as part of a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BraveProfile {
    /// Profile directory name, such as `Default` or `Profile 1`.
    pub directory: String,
    /// Human-readable profile name shown by Brave.
    pub name: String,
}

/// A named snapshot of the Hyprland desktop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// User-facing snapshot name.
    pub name: String,
    /// Time at which the snapshot was captured.
    pub created_at: DateTime<Utc>,
    /// Hyprland version reported during capture.
    pub hyprland_version: String,
    /// Monitor topology captured with the session.
    pub monitors: Vec<Monitor>,
    /// Saved client windows and their launch/placement state.
    pub clients: Vec<SessionClient>,
    /// Brave profiles observed during capture.
    #[serde(default)]
    pub brave_profiles: Vec<BraveProfile>,
}

/// A monitor's geometry captured with a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    /// Hyprland connector name, such as `DP-1`.
    pub name: String,
    /// Monitor width in pixels.
    pub width: u32,
    /// Monitor height in pixels.
    pub height: u32,
    /// Hyprland transform value.
    pub transform: u32,
    /// Monitor origin in Hyprland's global coordinate space.  `None` keeps
    /// older session files from being treated as if they were captured at
    /// (0, 0), which would make geometry adaptation unsafe.
    /// Global X origin, when available.
    #[serde(default)]
    pub x: Option<i32>,
    /// Global Y origin, when available.
    #[serde(default)]
    pub y: Option<i32>,
}

/// A saved Hyprland client and the information needed to restore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClient {
    /// Runtime Hyprland window class.
    pub class: String,
    /// Window title at capture time.
    pub title: String,
    /// Hyprland's address is a window-level identity while the window remains
    /// open.  Older snapshots do not contain it and deserialize as `None`.
    #[serde(default)]
    pub address: Option<String>,
    /// PID and Linux process-start time identify the process which owned the
    /// window at capture time.  They are supplemental evidence: a window can
    /// legitimately survive a process hand-off, and older sessions do not
    /// contain either field.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Process start timestamp captured from `/proc`, when available.
    #[serde(default)]
    pub process_start_time: Option<u64>,
    /// Hyprland's stable window identifier, when the compositor provides it.
    /// Older snapshots omit this field and continue to use the legacy
    /// identity fallbacks.
    #[serde(default)]
    pub stable_id: Option<String>,
    /// Initial Hyprland app identity.  These fields are optional in spirit:
    /// older session files do not contain them and reconciliation falls back
    /// to `class` and `title` when they are empty.
    /// App class reported when the window was created.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub initial_class: String,
    /// Title reported when the window was created.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub initial_title: String,
    /// Numeric workspace identifier.
    pub workspace: i32,
    /// Workspace name, including named or special-workspace prefixes.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub workspace_name: String,
    /// Monitor connector name at capture time.
    pub monitor: String,
    /// Top-left position in global compositor coordinates.
    pub at: [i32; 2],
    /// Window size in pixels.
    pub size: [i32; 2],
    /// Whether the window was floating.
    pub floating: bool,
    /// Hyprland fullscreen state.
    pub fullscreen: u8,
    /// Whether the window was pinned.
    #[serde(default, deserialize_with = "deserialize_nullable_bool")]
    pub pinned: bool,
    /// Browser profile directory associated with the client, when known.
    #[serde(default)]
    pub profile_directory: Option<String>,
    /// True when the captured browser process did not provide a safe
    /// window-specific profile identity.  Automatic restore must not use such
    /// a client to move or launch a guessed Brave profile.
    /// Whether profile identity was ambiguous during capture.
    #[serde(default, deserialize_with = "deserialize_nullable_bool")]
    pub profile_identity_ambiguous: bool,
    /// Hyprland focus-history sequence at capture time.
    pub focus_history_id: i32,
    /// Command, arguments, and optional desktop-entry hint used to relaunch.
    pub launch: LaunchInfo,
}

/// Launch information captured for a session client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchInfo {
    /// Executable or desktop-entry command.
    pub command: String,
    /// Arguments passed to the command.
    pub args: Vec<String>,
    /// Optional launcher hint for web apps or desktop entries.
    pub hint: Option<String>,
}

// === Session storage ===

use std::path::Path;

/// Errors returned while reading or writing session state.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("IO error: {0}")]
    /// Filesystem operation failed.
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    /// Session JSON was invalid.
    Json(#[from] serde_json::Error),
    #[error("session '{0}' not found")]
    /// The requested session does not exist.
    NotFound(String),
    #[error("session '{0}' already exists")]
    /// A session with the requested name already exists.
    AlreadyExists(String),
    #[error("invalid session name '{0}': use 1-128 ASCII letters, numbers, '.', '_' or '-'")]
    /// The requested name is not safe for use as a file name.
    InvalidName(String),
    #[error("unsafe session path for '{0}'")]
    /// A session path failed an ownership, permission, or symlink check.
    UnsafePath(String),
    #[error("session file '{requested}' contains payload for '{actual}'")]
    /// The filename and serialized session name do not agree.
    NameMismatch {
        /// Name requested by the caller or inferred from the filename.
        requested: String,
        /// Name stored inside the session payload.
        actual: String,
    },
    #[error("session data exceeds safety limits: {0}")]
    /// Session data exceeded one of the storage safety limits.
    TooLarge(String),
}

/// Summary metadata for one stored session.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// Session name.
    pub name: String,
    /// Session capture time.
    pub created_at: DateTime<Utc>,
    /// Number of saved client windows.
    pub client_count: usize,
}

/// Durable phases of a destructive replacement transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacePhase {
    /// Safety snapshot and prepared marker have been written.
    Prepared,
    /// The first current window is about to be closed.
    Closing,
    /// Current windows are being closed or the target is being restored.
    InProgress,
    /// The target reconciliation completed.  This durable phase lets startup
    /// finish cleanup safely if writing the final committed marker is
    /// interrupted after the desktop has already been restored.
    Finalizing,
    /// Replacement completed and its marker can be removed.
    Committed,
}

/// Durable metadata used to recover an interrupted replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceMarker {
    /// Name of the safety snapshot to restore if recovery is required.
    pub backup_name: String,
    /// Current replacement phase.
    pub phase: ReplacePhase,
    /// Address of the first client selected for closing.
    pub closing_address: Option<String>,
    /// Process identity for the first close candidate.  New markers use this
    /// to distinguish a close that actually happened from a different window
    /// which later reused the same Hyprland address.  Older markers leave it
    /// absent and are recovered conservatively.
    pub closing_pid: Option<u32>,
    /// Start timestamp of the first close candidate's process.
    pub closing_start_time: Option<u64>,
    /// Hyprland's window-specific stable ID for the first close candidate.
    /// This protects recovery from address reuse inside one shared browser
    /// process, where PID and process start time remain unchanged.
    pub closing_stable_id: Option<String>,
    /// The session being installed.  Older markers do not contain this and
    /// therefore retain the conservative exact-recovery behavior.
    /// Target session name, when the marker records one.
    pub target_name: Option<String>,
    /// SHA-256 fingerprint of the exact target session used by a replacement.
    /// A changed target must not be used to declare an interrupted transaction
    /// complete, because recovery must reason about the plan that actually
    /// started.
    /// Fingerprint of the target session used by the replacement.
    pub target_digest: Option<String>,
}

/// Process-wide lock for CLI operations that can observe or mutate the
/// desktop/session store.
///
/// Keeping this in the helper means the UI, systemd
/// autosave, and manually invoked commands share one serialization boundary.
#[derive(Debug)]
pub struct OperationLock {
    _file: File,
}

impl OperationLock {
    /// Acquire the process-wide operation lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock directory or lock file is unsafe or
    /// cannot be opened.
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

        file.lock().map_err(SessionError::Io)?;

        Ok(Self { _file: file })
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

/// Save a validated session snapshot in `sessions_dir`.
///
/// # Errors
///
/// Returns an error when validation fails or the snapshot cannot be written
/// safely.
pub fn save_session(session: &Session, sessions_dir: &Path) -> Result<(), SessionError> {
    validate_session_structure(session)?;
    ensure_sessions_dir(sessions_dir)?;
    let path = sessions_dir.join(format!("{}.json", session.name));
    let json = serde_json::to_string_pretty(session)?;
    if u64::try_from(json.len()).unwrap_or(u64::MAX) > MAX_SESSION_FILE_BYTES {
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

/// Load and validate a named session snapshot.
///
/// # Errors
///
/// Returns an error when the name is invalid, the snapshot is missing or
/// unsafe, or its contents fail validation.
pub fn load_session(name: &str, sessions_dir: &Path) -> Result<Session, SessionError> {
    validate_session_name(name)?;
    if !existing_sessions_dir(sessions_dir)? {
        return Err(SessionError::NotFound(name.to_string()));
    }
    let path = sessions_dir.join(format!("{name}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(SessionError::UnsafePath(name.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(SessionError::NotFound(name.to_string())),
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

/// List valid session snapshots in capture-time order.
///
/// # Errors
///
/// Returns an error when the session directory cannot be inspected safely.
// Invalid or unsafe entries are intentionally ignored while valid snapshots
// are collected; the nested checks keep that fail-closed policy local.
#[allow(clippy::excessive_nesting)]
pub fn list_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    if !existing_sessions_dir(sessions_dir)? {
        return Ok(vec![]);
    }
    let mut summaries = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_regular_file = entry.file_type().is_ok_and(|file_type| file_type.is_file());
        if is_regular_file
            && path.extension().is_some_and(|e| e == "json")
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
    summaries.sort_by(|left, right| Reverse(left.created_at).cmp(&Reverse(right.created_at)).then(left.name.cmp(&right.name)));
    Ok(summaries)
}

/// Delete one named session snapshot.
///
/// # Errors
///
/// Returns an error when the name or target path is invalid, missing, or
/// unsafe.
pub fn delete_session(name: &str, sessions_dir: &Path) -> Result<(), SessionError> {
    validate_session_name(name)?;
    if !existing_sessions_dir(sessions_dir)? {
        return Err(SessionError::NotFound(name.to_string()));
    }
    let path = sessions_dir.join(format!("{name}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(SessionError::UnsafePath(name.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(SessionError::NotFound(name.to_string())),
        Err(error) => return Err(SessionError::Io(error)),
    }
    ensure_private_file(&path, name)?;
    std::fs::remove_file(path)?;
    Ok(())
}

/// Check whether a named session exists in the session directory.
#[must_use]
pub fn session_exists(name: &str, sessions_dir: &Path) -> bool {
    if validate_session_name(name).is_err() {
        return false;
    }
    if !matches!(existing_sessions_dir(sessions_dir), Ok(true)) {
        return false;
    }
    let path = sessions_dir.join(format!("{name}.json"));
    std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) && ensure_private_file(&path, name).is_ok()
}

/// Validate a session name for safe use as a storage filename.
///
/// # Errors
///
/// Returns [`SessionError::InvalidName`] when the name is empty, too long, or
/// contains a character outside the supported ASCII set.
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
///
/// # Errors
///
/// Returns [`SessionError::InvalidName`] when the name is invalid or uses the
/// reserved autosave prefix.
pub fn validate_user_session_name(name: &str) -> Result<(), SessionError> {
    validate_session_name(name)?;
    if name.starts_with(AUTOSAVE_PREFIX) {
        return Err(SessionError::InvalidName(format!(
            "{name} (the '{AUTOSAVE_PREFIX}' prefix is reserved for autosaves)"
        )));
    }
    Ok(())
}

/// Copy legacy `HyprFlow` sessions into `Hyprloom` storage without removing or
/// overwriting anything.
///
/// Migration is attempted once and recorded in the current session directory.
/// Keeping the original files as a rollback path must not cause a deliberately
/// deleted Hyprloom snapshot to be copied back on the next command.
///
/// # Errors
///
/// Returns an error when either session directory cannot be inspected or a
/// valid legacy snapshot cannot be copied safely.
// Migration validates every candidate independently so one malformed legacy
// file cannot prevent healthy snapshots from being copied.
#[allow(clippy::excessive_nesting)]
pub fn migrate_legacy_sessions(sessions_dir: &Path, legacy_sessions_dir: &Path) -> Result<usize, SessionError> {
    const MIGRATION_MARKER_NAME: &str = ".hyprflow-migration-complete";

    if sessions_dir == legacy_sessions_dir {
        return Ok(0);
    }

    let migration_marker = sessions_dir.join(MIGRATION_MARKER_NAME);
    match std::fs::symlink_metadata(&migration_marker) {
        Ok(metadata) if metadata.file_type().is_file() => {
            ensure_private_file(&migration_marker, MIGRATION_MARKER_NAME)?;
            return Ok(0);
        }
        Ok(_) => return Err(SessionError::UnsafePath(migration_marker.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(SessionError::Io(error)),
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
        let source_is_regular = entry.file_type().is_ok_and(|file_type| file_type.is_file());
        if source_is_regular && source.extension().is_some_and(|ext| ext == "json") {
            let destination = sessions_dir.join(entry.file_name());
            if std::fs::symlink_metadata(&destination).is_ok() {
                continue;
            }
            let name = source.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
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
    atomic_write(&migration_marker, b"complete\n")?;
    Ok(copied)
}

// === Autosave helpers ===

/// Prefix reserved for automatically captured safety snapshots.
pub const AUTOSAVE_PREFIX: &str = "autosave-";
const REPLACE_MARKER_NAME: &str = ".replace-in-progress";
static AUTOSAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Generate a unique autosave name using the current time and process ID.
pub fn autosave_name_now() -> String {
    let now = Utc::now();
    let sequence = AUTOSAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("autosave-{}-{}-{}", now.format("%Y%m%dT%H%M%S%f"), std::process::id(), sequence)
}

/// Mark a replacement as having started without a named target.
///
/// # Errors
///
/// Returns an error when the backup name or marker path is invalid.
pub fn mark_replace_in_progress(backup_name: &str, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    mark_replace_in_progress_for_target(backup_name, None, sessions_dir)
}

/// Mark a replacement as having started, optionally naming its target.
///
/// # Errors
///
/// Returns an error when the names or marker path are invalid.
pub fn mark_replace_in_progress_for_target(backup_name: &str, target_name: Option<&str>, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    let target_digest = existing_target_digest(backup_name, target_name, sessions_dir)?;
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::InProgress,
            closing_address: None,
            closing_pid: None,
            closing_start_time: None,
            closing_stable_id: None,
            target_name: target_name.map(str::to_string),
            target_digest,
        },
        sessions_dir,
    )
}

/// Record the address of the first window whose close dispatch is about to be
/// sent.
///
/// Startup can use this evidence to distinguish a marker written just
/// before a failed dispatch from a replacement that actually started closing
/// the desktop.
///
/// # Errors
///
/// Returns an error when the names, address, or marker path is invalid.
pub fn mark_replace_closing(backup_name: &str, sessions_dir: &std::path::Path, address: &str) -> Result<(), SessionError> {
    mark_replace_closing_for_target(backup_name, None, sessions_dir, address)
}

/// Record the first client address selected for closing.
///
/// # Errors
///
/// Returns an error when the names, address, or marker path is invalid.
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
    let target_digest = existing_target_digest(backup_name, target_name, sessions_dir)?;
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Closing,
            closing_address: Some(address.to_string()),
            closing_pid: None,
            closing_start_time: None,
            closing_stable_id: None,
            target_name: target_name.map(str::to_string),
            target_digest,
        },
        sessions_dir,
    )
}

/// Record the first close candidate together with its process identity.
///
/// The identity is intentionally optional because Hyprland can report a window
/// whose process has already exited by the time `/proc` is inspected.
///
/// # Errors
///
/// Returns an error when the names, address, or marker path is invalid.
// The marker API keeps each identity field explicit because it is also the
// durable transaction boundary used by startup recovery.
#[allow(clippy::too_many_arguments)]
pub fn mark_replace_closing_for_target_with_identity(
    backup_name: &str,
    target_name: Option<&str>,
    sessions_dir: &std::path::Path,
    address: &str,
    pid: u32,
    start_time: Option<u64>,
) -> Result<(), SessionError> {
    mark_replace_closing_for_target_with_identity_and_stable_id(backup_name, target_name, sessions_dir, address, pid, start_time, None)
}

/// Record the first close candidate with process and compositor identity.
/// The stable ID is optional for compatibility with older Hyprland versions.
///
/// # Errors
///
/// Returns an error when the names, address, stable ID, or marker path is
/// invalid.
// Keep the durable marker API explicit so every identity component is visible
// at the call site and compatible with older recovery callers.
#[allow(clippy::too_many_arguments)]
pub fn mark_replace_closing_for_target_with_identity_and_stable_id(
    backup_name: &str,
    target_name: Option<&str>,
    sessions_dir: &std::path::Path,
    address: &str,
    pid: u32,
    start_time: Option<u64>,
    stable_id: Option<&str>,
) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    validate_replace_address(address)?;
    if let Some(stable_id) = stable_id.filter(|id| !id.is_empty()) {
        validate_replace_stable_id(stable_id)?;
    }
    let target_digest = existing_target_digest(backup_name, target_name, sessions_dir)?;
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Closing,
            closing_address: Some(address.to_string()),
            closing_pid: Some(pid),
            closing_start_time: start_time,
            closing_stable_id: stable_id.filter(|id| !id.is_empty()).map(str::to_string),
            target_name: target_name.map(str::to_string),
            target_digest,
        },
        sessions_dir,
    )
}

/// Record that a replacement has been prepared but has not started closing
/// the desktop yet.  A crash in this phase must not roll back user activity
/// that happened after the safety snapshot was captured.
///
/// # Errors
///
/// Returns an error when the backup name or marker path is invalid.
pub fn mark_replace_prepared(backup_name: &str, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    mark_replace_prepared_for_target(backup_name, None, sessions_dir)
}

/// Record a prepared replacement with an optional target name.
///
/// # Errors
///
/// Returns an error when a name or the marker path is invalid.
pub fn mark_replace_prepared_for_target(backup_name: &str, target_name: Option<&str>, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    mark_replace_prepared_for_target_with_digest(backup_name, target_name, None, sessions_dir)
}

/// Record a prepared replacement and its optional target fingerprint.
///
/// # Errors
///
/// Returns an error when a name, fingerprint, or marker path is invalid.
pub fn mark_replace_prepared_for_target_with_digest(
    backup_name: &str,
    target_name: Option<&str>,
    target_digest: Option<&str>,
    sessions_dir: &std::path::Path,
) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    if let Some(target_digest) = target_digest {
        validate_replace_digest(target_digest)?;
    }
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Prepared,
            closing_address: None,
            closing_pid: None,
            closing_start_time: None,
            closing_stable_id: None,
            target_name: target_name.map(str::to_string),
            target_digest: target_digest.map(str::to_string),
        },
        sessions_dir,
    )
}

/// Mark a replacement as committed without a named target.
///
/// # Errors
///
/// Returns an error when the backup name or marker path is invalid.
pub fn mark_replace_committed(backup_name: &str, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    mark_replace_committed_for_target(backup_name, None, sessions_dir)
}

/// Record that the target desktop is complete and only recovery-marker cleanup
/// remains.  Startup can safely finalize this phase without replaying the old
/// safety snapshot.
///
/// # Errors
///
/// Returns an error when a name or marker path is invalid.
pub fn mark_replace_finalizing_for_target(backup_name: &str, target_name: Option<&str>, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    let target_digest = existing_target_digest(backup_name, target_name, sessions_dir)?;
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Finalizing,
            closing_address: None,
            closing_pid: None,
            closing_start_time: None,
            closing_stable_id: None,
            target_name: target_name.map(str::to_string),
            target_digest,
        },
        sessions_dir,
    )
}

/// Mark a named replacement as committed.
///
/// # Errors
///
/// Returns an error when a name or marker path is invalid.
pub fn mark_replace_committed_for_target(backup_name: &str, target_name: Option<&str>, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    validate_session_name(backup_name)?;
    if let Some(target_name) = target_name {
        validate_session_name(target_name)?;
    }
    let target_digest = existing_target_digest(backup_name, target_name, sessions_dir)?;
    ensure_sessions_dir(sessions_dir)?;
    write_replace_marker(
        &ReplaceMarker {
            backup_name: backup_name.to_string(),
            phase: ReplacePhase::Committed,
            closing_address: None,
            closing_pid: None,
            closing_start_time: None,
            closing_stable_id: None,
            target_name: target_name.map(str::to_string),
            target_digest,
        },
        sessions_dir,
    )
}

/// Read the durable replacement marker, if one exists.
///
/// # Errors
///
/// Returns an error when the marker is malformed, unsafe, or unreadable.
// Marker parsing is a compact compatibility state machine for the historical
// and current on-disk formats.
#[allow(clippy::excessive_nesting)]
#[allow(clippy::too_many_lines)]
pub fn replace_marker(sessions_dir: &std::path::Path) -> Result<Option<ReplaceMarker>, SessionError> {
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
        .map_err(|error| SessionError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())))?
        .trim()
        .to_string();
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.first().copied().unwrap_or_default();
    let (phase, name_index, cursor) = match first {
        "prepared" => (ReplacePhase::Prepared, 1, 2),
        // Older builds wrote only the backup name.  Treat that format as an
        // interrupted replacement so upgrading cannot silently skip recovery.
        "closing" => (ReplacePhase::Closing, 1, 3),
        "in-progress" => (ReplacePhase::InProgress, 1, 2),
        "finalizing" => (ReplacePhase::Finalizing, 1, 2),
        "committed" => (ReplacePhase::Committed, 1, 2),
        _legacy_name => (ReplacePhase::InProgress, 0, 1),
    };
    if lines.len() < cursor {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replacement marker is missing required fields",
        )));
    }
    let name = lines.get(name_index).copied().unwrap_or_default();
    let closing_address = (phase == ReplacePhase::Closing).then(|| lines.get(2).copied().unwrap_or_default().to_string());
    let mut next_index = cursor;
    let mut closing_pid = None;
    let mut closing_start_time = None;
    let mut closing_stable_id = None;
    if phase == ReplacePhase::Closing {
        // New markers add identity records between the closing address and
        // the optional target name.  Their prefixes keep the old three/four-
        // line marker formats unambiguous and readable during upgrades.
        loop {
            if let Some(identity) = lines.get(next_index).filter(|line| line.starts_with("identity:")) {
                let value = identity.strip_prefix("identity:").unwrap_or_default();
                let (pid, start_time) = value.split_once(':').ok_or_else(|| {
                    SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "replacement marker has an invalid process identity",
                    ))
                })?;
                closing_pid = Some(pid.parse::<u32>().map_err(|_| {
                    SessionError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "replacement marker has an invalid process ID",
                    ))
                })?);
                closing_start_time = if start_time.is_empty() {
                    None
                } else {
                    Some(start_time.parse::<u64>().map_err(|_| {
                        SessionError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "replacement marker has an invalid process start time",
                        ))
                    })?)
                };
                next_index += 1;
                continue;
            }
            if let Some(stable) = lines.get(next_index).filter(|line| line.starts_with("stable:")) {
                let value = stable.strip_prefix("stable:").unwrap_or_default();
                validate_replace_stable_id(value)?;
                closing_stable_id = Some(value.to_string());
                next_index += 1;
                continue;
            }
            break;
        }
    }
    let target_name = lines
        .get(next_index)
        .filter(|line| !line.starts_with("target-digest:"))
        .map(|target| (*target).to_string());
    if target_name.is_some() {
        next_index += 1;
    }
    let target_digest = lines
        .get(next_index)
        .and_then(|line| line.strip_prefix("target-digest:"))
        .map(str::to_string);
    if target_digest.is_some() {
        next_index += 1;
    }
    if lines.len() != next_index {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "replacement marker has unexpected extra lines",
        )));
    }
    validate_session_name(name)?;
    if let Some(address) = &closing_address {
        validate_replace_address(address)?;
    }
    if let Some(target_name) = &target_name {
        validate_session_name(target_name)?;
    }
    if let Some(target_digest) = &target_digest {
        validate_replace_digest(target_digest)?;
    }
    Ok(Some(ReplaceMarker {
        backup_name: name.to_string(),
        phase,
        closing_address,
        closing_pid,
        closing_start_time,
        closing_stable_id,
        target_name,
        target_digest,
    }))
}

/// Return the safety snapshot named by a still-pending replacement.
///
/// # Errors
///
/// Returns an error when the marker cannot be read or validated.
pub fn pending_replace_backup(sessions_dir: &std::path::Path) -> Result<Option<String>, SessionError> {
    Ok(replace_marker(sessions_dir)?.map(|marker| marker.backup_name))
}

fn write_replace_marker(marker: &ReplaceMarker, sessions_dir: &std::path::Path) -> Result<(), SessionError> {
    if let Some(target_name) = &marker.target_name {
        validate_session_name(target_name)?;
    }
    if let Some(target_digest) = &marker.target_digest {
        validate_replace_digest(target_digest)?;
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
        ReplacePhase::Finalizing => format_marker_content("finalizing", marker, None),
        ReplacePhase::Committed => format_marker_content("committed", marker, None),
    };
    atomic_write(&sessions_dir.join(REPLACE_MARKER_NAME), content.as_bytes())
}

fn format_marker_content(phase: &str, marker: &ReplaceMarker, closing_address: Option<&str>) -> String {
    let mut content = format!("{phase}\n{}", marker.backup_name);
    if let Some(address) = closing_address {
        content.push('\n');
        content.push_str(address);
    }
    if let (Some(pid), Some(start_time)) = (marker.closing_pid, marker.closing_start_time) {
        content.push('\n');
        let _ = write!(content, "identity:{pid}:{start_time}");
    } else if let Some(pid) = marker.closing_pid {
        content.push('\n');
        let _ = write!(content, "identity:{pid}:");
    }
    if let Some(stable_id) = &marker.closing_stable_id {
        content.push('\n');
        content.push_str("stable:");
        content.push_str(stable_id);
    }
    if let Some(target_name) = &marker.target_name {
        content.push('\n');
        content.push_str(target_name);
    }
    if let Some(target_digest) = &marker.target_digest {
        content.push('\n');
        content.push_str("target-digest:");
        content.push_str(target_digest);
    }
    content
}

fn validate_replace_address(address: &str) -> Result<(), SessionError> {
    if address.is_empty() || address.len() > MAX_SESSION_STRING_BYTES || address.contains(['\r', '\n']) {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement marker has an invalid window address",
        )));
    }
    Ok(())
}

fn validate_replace_stable_id(stable_id: &str) -> Result<(), SessionError> {
    if stable_id.is_empty() || stable_id.len() > MAX_SESSION_STRING_BYTES || stable_id.contains(['\r', '\n']) {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement marker has an invalid stable window ID",
        )));
    }
    Ok(())
}

fn validate_replace_digest(digest: &str) -> Result<(), SessionError> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement marker has an invalid target fingerprint",
        )));
    }
    Ok(())
}

/// Compute the deterministic SHA-256 fingerprint of a session.
///
/// # Errors
///
/// Returns an error when the session cannot be serialized.
pub fn session_fingerprint(session: &Session) -> Result<String, SessionError> {
    let bytes = serde_json::to_vec(session)?;
    let digest = Sha256::digest(bytes);
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(fingerprint, "{byte:02x}");
    }
    Ok(fingerprint)
}

fn existing_target_digest(backup_name: &str, target_name: Option<&str>, sessions_dir: &Path) -> Result<Option<String>, SessionError> {
    let Some(marker) = replace_marker(sessions_dir)? else {
        return Ok(None);
    };
    if marker.backup_name == backup_name && marker.target_name.as_deref() == target_name {
        Ok(marker.target_digest)
    } else {
        Ok(None)
    }
}

/// Remove a completed replacement marker from the session directory.
///
/// # Errors
///
/// Returns an error when the marker path is unsafe or cannot be removed.
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

/// Returns autosave sessions only (name starts with `AUTOSAVE_PREFIX`).
///
/// They are sorted by name descending. The timestamp, process ID, and sequence suffix
/// make names unique even when multiple captures happen in one second.
///
/// # Errors
///
/// Returns an error when the session directory cannot be listed safely.
pub fn list_autosave_sessions(sessions_dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let mut all = list_sessions(sessions_dir)?;
    all.retain(|s| s.name.starts_with(AUTOSAVE_PREFIX));
    // Sort by name descending — autosave-YYYYMMDDTHHMMSS sorts lexicographically
    all.sort_by(|a, b| b.name.cmp(&a.name));
    Ok(all)
}

/// Deletes the oldest autosave sessions, keeping only the `retain` newest.
/// Returns the count of sessions deleted. Non-autosave sessions are untouched.
///
/// # Errors
///
/// Returns an error when the session directory cannot be inspected or a
/// selected autosave cannot be deleted safely.
// Rotation must skip the active recovery backup while tolerating a concurrent
// idempotent deletion, hence the nested guard and match.
#[allow(clippy::excessive_nesting)]
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
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("session.json");

    for _ in 0..100 {
        let sequence = AUTOSAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), sequence));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SessionError::Io(error)),
        };

        let write_result = file.write_all(contents).and_then(|()| file.sync_all());
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
    if metadata.uid() != current_user_id() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(SessionError::UnsafePath(label.to_string()));
    }
    if metadata.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge(format!("'{label}' is larger than {MAX_SESSION_FILE_BYTES} bytes")));
    }

    let capacity = usize::try_from(metadata.len().min(MAX_SESSION_FILE_BYTES)).unwrap_or(usize::MAX);
    let mut content = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(MAX_SESSION_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut content)?;
    if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::TooLarge(format!("'{label}' is larger than {MAX_SESSION_FILE_BYTES} bytes")));
    }
    Ok(content)
}

fn ensure_private_directory(path: &Path) -> Result<(), SessionError> {
    ensure_private_path(path, true)
}

fn ensure_private_file(path: &Path, label: &str) -> Result<(), SessionError> {
    ensure_private_path(path, false).map_err(|error| {
        if matches!(&error, SessionError::UnsafePath(_)) {
            SessionError::UnsafePath(label.to_string())
        } else {
            error
        }
    })
}

fn ensure_private_path(path: &Path, directory: bool) -> Result<(), SessionError> {
    #[cfg(unix)]
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.uid() != current_user_id() {
            return Err(SessionError::UnsafePath(path.display().to_string()));
        }
        let expected_mode = if directory { 0o700 } else { 0o600 };
        if metadata.permissions().mode() & 0o777 != expected_mode {
            let mut permissions = metadata.permissions();
            permissions.set_mode(expected_mode);
            std::fs::set_permissions(path, permissions)?;
        }
        let verified = std::fs::symlink_metadata(path)?;
        if verified.uid() != current_user_id() || verified.permissions().mode() & 0o077 != 0 {
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
        return Err(SessionError::TooLarge(format!("more than {MAX_SESSION_MONITORS} monitors")));
    }
    if session.clients.len() > MAX_SESSION_CLIENTS {
        return Err(SessionError::TooLarge(format!("more than {MAX_SESSION_CLIENTS} clients")));
    }
    if session.brave_profiles.len() > MAX_SESSION_BRAVE_PROFILES {
        return Err(SessionError::TooLarge(format!("more than {MAX_SESSION_BRAVE_PROFILES} browser profiles")));
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
        if let Some(address) = &client.address {
            validate_text("client address", address)?;
        }
        if let Some(stable_id) = &client.stable_id {
            validate_text("client stable window ID", stable_id)?;
        }
        if let Some(profile) = &client.profile_directory {
            validate_text("client profile directory", profile)?;
        }
        if let Some(hint) = &client.launch.hint {
            validate_text("launch hint", hint)?;
        }
        if client.launch.args.len() > MAX_SESSION_ARGS {
            return Err(SessionError::TooLarge(format!("more than {MAX_SESSION_ARGS} launch arguments")));
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
        return Err(SessionError::TooLarge(format!("{label} is longer than {MAX_SESSION_STRING_BYTES} bytes")));
    }
    Ok(())
}

/// Parses a human-readable duration string into a `chrono::Duration`.
///
/// Supported suffixes: `m` (minutes), `h` (hours), `d` (days).
/// Examples: `"30m"`, `"24h"`, `"7d"`.
/// Parse a duration such as `30m`, `24h`, or `7d`.
///
/// # Errors
///
/// Returns an error when the value has no supported suffix or overflows.
pub fn parse_max_age(s: &str) -> Result<chrono::Duration, String> {
    let Some((unit_index, unit)) = s.char_indices().next_back() else {
        return Err(format!("invalid duration: '{s}'"));
    };
    if unit_index == 0 {
        return Err(format!("invalid duration: '{s}'"));
    }
    let num_str = &s[..unit_index];
    let num: i64 = num_str.parse().map_err(|_| format!("invalid duration: '{s}'"))?;
    if num <= 0 {
        return Err(format!("duration must be greater than zero: '{s}'"));
    }
    let duration = match unit {
        'm' => chrono::Duration::try_minutes(num),
        'h' => chrono::Duration::try_hours(num),
        'd' => chrono::Duration::try_days(num),
        _ => return Err(format!("invalid duration unit '{unit}' in '{s}'. Use m, h, or d.")),
    };
    duration.ok_or_else(|| format!("duration is out of range: '{s}'"))
}

// === Raw hyprctl JSON structs (what hyprctl returns) ===

/// Raw Hyprland client JSON returned by `hyprctl clients -j`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprClient {
    /// Hyprland window address.
    pub address: String,
    /// Runtime window class.
    pub class: String,
    /// Current window title.
    pub title: String,
    /// Stable window identifier, when provided by Hyprland.
    #[serde(default, rename = "stableId")]
    pub stable_id: Option<String>,
    /// Class assigned when the window was created.
    #[serde(default, rename = "initialClass")]
    pub initial_class: String,
    /// Title assigned when the window was created.
    #[serde(default, rename = "initialTitle")]
    pub initial_title: String,
    /// Current workspace.
    pub workspace: HyprWorkspace,
    /// Numeric monitor identifier.
    pub monitor: i32,
    /// Top-left position in global compositor coordinates.
    pub at: [i32; 2],
    /// Window size in pixels.
    pub size: [i32; 2],
    /// Whether the window is floating.
    pub floating: bool,
    /// Hyprland fullscreen state.
    pub fullscreen: u8,
    /// Whether the window is pinned.
    #[serde(default)]
    pub pinned: bool,
    /// Focus-history sequence.
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
    /// Owning process ID.
    pub pid: u32,
}

/// Workspace data embedded in raw Hyprland client JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprWorkspace {
    /// Numeric workspace identifier.
    pub id: i32,
    /// Workspace name.
    pub name: String,
}

/// Raw Hyprland monitor JSON returned by `hyprctl monitors -j`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyprMonitor {
    /// Monitor connector name.
    pub name: String,
    /// Monitor width in pixels.
    pub width: u32,
    /// Monitor height in pixels.
    pub height: u32,
    /// Hyprland transform value.
    pub transform: u32,
    /// Global X origin, when reported.
    #[serde(default)]
    pub x: Option<i32>,
    /// Global Y origin, when reported.
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
            created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z").unwrap().with_timezone(&Utc),
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
                address: None,
                pid: None,
                process_start_time: None,
                stable_id: None,
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
                address: None,
                pid: None,
                process_start_time: None,
                stable_id: None,
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
            created_at: DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z").unwrap().with_timezone(&Utc),
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
        assert!(client.address.is_none());
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

        let session: Session = serde_json::from_str(json).expect("null compatibility fields must load");
        let client = &session.clients[0];
        assert_eq!(client.initial_class, "");
        assert_eq!(client.initial_title, "");
        assert_eq!(client.workspace_name, "");
        assert!(!client.pinned);
        assert!(client.address.is_none());
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

        assert_eq!(migrate_legacy_sessions(current.path(), legacy.path()).unwrap(), 1);
        assert_eq!(load_session("work", current.path()).unwrap().name, "work");

        // Existing fork data is never overwritten, and a second pass copies nothing.
        let existing = make_test_session("work");
        save_session(&existing, current.path()).unwrap();
        assert_eq!(migrate_legacy_sessions(current.path(), legacy.path()).unwrap(), 0);
        assert_eq!(load_session("work", current.path()).unwrap().name, "work");
    }

    #[test]
    fn test_migrate_legacy_sessions_does_not_resurrect_deleted_sessions() {
        let legacy = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        save_session(&make_test_session("work"), legacy.path()).unwrap();

        assert_eq!(migrate_legacy_sessions(current.path(), legacy.path()).unwrap(), 1);
        delete_session("work", current.path()).unwrap();

        assert_eq!(migrate_legacy_sessions(current.path(), legacy.path()).unwrap(), 0);
        assert!(!current.path().join("work.json").exists());
    }

    #[test]
    fn test_migrate_legacy_sessions_skips_bad_files_and_keeps_valid_files() {
        let legacy = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        let malformed = legacy.path().join("malformed.json");
        std::fs::write(&malformed, b"not json").unwrap();
        ensure_private_file(&malformed, "malformed").unwrap();

        let oversized = legacy.path().join("oversized.json");
        let oversized_len = usize::try_from(MAX_SESSION_FILE_BYTES).unwrap_or(usize::MAX).saturating_add(1);
        std::fs::write(&oversized, vec![b'x'; oversized_len]).unwrap();
        ensure_private_file(&oversized, "oversized").unwrap();

        save_session(&make_test_session("healthy"), legacy.path()).unwrap();

        assert_eq!(migrate_legacy_sessions(current.path(), legacy.path()).unwrap(), 1);
        assert_eq!(load_session("healthy", current.path()).unwrap().name, "healthy");
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
    #[allow(clippy::too_many_lines)]
    fn test_replace_recovery_marker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        mark_replace_prepared("autosave-recovery", dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Prepared,
                closing_address: None,
                closing_pid: None,
                closing_start_time: None,
                closing_stable_id: None,
                target_name: None,
                target_digest: None,
            })
        );
        mark_replace_closing("autosave-recovery", dir.path(), "0xclosing").unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Closing,
                closing_address: Some("0xclosing".to_string()),
                closing_pid: None,
                closing_start_time: None,
                closing_stable_id: None,
                target_name: None,
                target_digest: None,
            })
        );
        mark_replace_in_progress("autosave-recovery", dir.path()).unwrap();
        assert_eq!(pending_replace_backup(dir.path()).unwrap().as_deref(), Some("autosave-recovery"));
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::InProgress,
                closing_address: None,
                closing_pid: None,
                closing_start_time: None,
                closing_stable_id: None,
                target_name: None,
                target_digest: None,
            })
        );
        mark_replace_committed("autosave-recovery", dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Committed,
                closing_address: None,
                closing_pid: None,
                closing_start_time: None,
                closing_stable_id: None,
                target_name: None,
                target_digest: None,
            })
        );
        mark_replace_finalizing_for_target("autosave-recovery", Some("target"), dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Finalizing,
                closing_address: None,
                closing_pid: None,
                closing_start_time: None,
                closing_stable_id: None,
                target_name: Some("target".to_string()),
                target_digest: None,
            })
        );
        mark_replace_prepared_for_target("autosave-recovery", Some("target"), dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Prepared,
                closing_address: None,
                closing_pid: None,
                closing_start_time: None,
                closing_stable_id: None,
                target_name: Some("target".to_string()),
                target_digest: None,
            })
        );
        clear_replace_marker(dir.path()).unwrap();
        assert_eq!(pending_replace_backup(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_replace_marker_preserves_target_fingerprint_across_phases() {
        let dir = tempfile::tempdir().unwrap();
        let target = make_test_session("target");
        let digest = session_fingerprint(&target).unwrap();

        mark_replace_prepared_for_target_with_digest("autosave-recovery", Some("target"), Some(&digest), dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap().and_then(|marker| marker.target_digest),
            Some(digest.clone())
        );

        mark_replace_in_progress_for_target("autosave-recovery", Some("target"), dir.path()).unwrap();
        assert_eq!(
            replace_marker(dir.path()).unwrap().and_then(|marker| marker.target_digest),
            Some(digest.clone())
        );

        mark_replace_finalizing_for_target("autosave-recovery", Some("target"), dir.path()).unwrap();
        assert_eq!(replace_marker(dir.path()).unwrap().and_then(|marker| marker.target_digest), Some(digest));
    }

    #[test]
    fn test_replace_closing_marker_preserves_process_identity() {
        let dir = tempfile::tempdir().unwrap();
        mark_replace_closing_for_target_with_identity("autosave-recovery", Some("target"), dir.path(), "0xclosing", 1234, Some(5678)).unwrap();

        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Closing,
                closing_address: Some("0xclosing".to_string()),
                closing_pid: Some(1234),
                closing_start_time: Some(5678),
                closing_stable_id: None,
                target_name: Some("target".to_string()),
                target_digest: None,
            })
        );
    }

    #[test]
    fn test_replace_closing_marker_preserves_stable_window_identity() {
        let dir = tempfile::tempdir().unwrap();
        mark_replace_closing_for_target_with_identity_and_stable_id(
            "autosave-recovery",
            Some("target"),
            dir.path(),
            "0xclosing",
            1234,
            Some(5678),
            Some("18000001"),
        )
        .unwrap();

        assert_eq!(
            replace_marker(dir.path()).unwrap(),
            Some(ReplaceMarker {
                backup_name: "autosave-recovery".to_string(),
                phase: ReplacePhase::Closing,
                closing_address: Some("0xclosing".to_string()),
                closing_pid: Some(1234),
                closing_start_time: Some(5678),
                closing_stable_id: Some("18000001".to_string()),
                target_name: Some("target".to_string()),
                target_digest: None,
            })
        );
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
        assert!(matches!(load_session("autosave-old", dir.path()), Err(SessionError::NameMismatch { .. })));
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

        assert!(matches!(save_session(&session, dir.path()), Err(SessionError::TooLarge(_))));
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

    #[test]
    fn test_parse_duration_rejects_unicode_without_panicking() {
        assert!(parse_max_age("1🙂").is_err());
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
        assert!(!std::fs::symlink_metadata(dir.path().join("work.json")).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn test_save_rejects_symlinked_sessions_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions_dir = root.path().join("sessions");
        symlink(outside.path(), &sessions_dir).unwrap();

        let error = save_session(&make_test_session("work"), &sessions_dir).expect_err("symlinked sessions directory must be rejected");
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

        assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(dir.path().join("work.json")).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn test_oversized_session_file_is_rejected_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.json");
        let oversized_len = usize::try_from(MAX_SESSION_FILE_BYTES).unwrap_or(usize::MAX).saturating_add(1);
        std::fs::write(&path, vec![b' '; oversized_len]).unwrap();

        let error = load_session("work", dir.path()).expect_err("oversized session must fail");
        assert!(matches!(error, SessionError::TooLarge(_)));
    }

    #[test]
    fn test_structurally_oversized_session_is_rejected_before_save() {
        let mut session = make_test_session("work");
        session.clients = vec![session.clients[0].clone(); MAX_SESSION_CLIENTS + 1];

        let error = save_session(&session, tempfile::tempdir().unwrap().path()).expect_err("too many clients must fail");
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
        assert_eq!(kitty.stable_id.as_deref(), Some("18000001"));
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
