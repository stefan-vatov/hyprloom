//! Small operating-system adapters shared by the storage and autosave code.

/// Return the effective Unix user ID without exposing unsafe FFI in the
/// application modules that perform filesystem ownership checks.
#[cfg(unix)]
pub fn current_user_id() -> u32 {
    nix::unistd::geteuid().as_raw()
}
