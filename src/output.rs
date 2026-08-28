//! Small terminal-output helpers shared by the library and command-line app.

use std::fmt::Display;
use std::io::Write;

/// Write a warning to the process's standard error stream.
pub fn warning(message: impl Display) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}
