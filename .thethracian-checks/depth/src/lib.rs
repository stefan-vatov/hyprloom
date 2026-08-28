//! Dylint library: `thethracian_max_directory_depth`
//!
//! Enforces a maximum directory depth for source files in Rust projects.
//! Files nested deeper than `max` levels (default 4) from the project root
//! are flagged. Prefer feature-based flat modules over layered deep hierarchies.
//!
//! ## Consumer setup
//!
//! Add to `Cargo.toml` or `dylint.toml`:
//!
//! ```toml
//! [workspace.metadata.dylint.libraries]
//! thethracian-depth = { path = "vendor/thethracian-rust-lint-config/checks/depth" }
//! ```
//!
//! Run with: `cargo dylint --all`

use std::collections::HashSet;
use std::path::Path;

use rustc_lint::{LateContext, LateLintPass};
use rustc_session::Session;
use rustc_span::Span;

dylint_linting::impl_late_lint! {
    /// ### What it does
    /// Checks that Rust source files are not nested deeper than a configurable
    /// directory depth threshold.
    ///
    /// ### Why is this bad?
    /// Deep directory structures make code harder to navigate and trace
    /// dependencies. AI agents struggle with deeply nested module hierarchies.
    ///
    /// ### Configuration
    /// Default max depth: 4
    ///
    /// ### Example
    /// ```rust,ignore
    /// // BAD: src/features/auth/domain/models/user.rs (5 levels)
    /// // GOOD: src/auth/models.rs (3 levels)
    /// ```
    pub MAX_DIRECTORY_DEPTH,
    Warn,
    "source file is nested deeper than the configured maximum directory depth",
    max_directory_depth::check_item
}

fn max_directory_depth<'tcx>(cx: &LateContext<'tcx>, _item: &'tcx rustc_hir::Item<'tcx>) {
    // Check only once per file — use session-global tracking
    let sess = cx.sess();
    let source_map = sess.source_map();
    let span = _item.span;
    let loc = source_map.lookup_char_pos(span.lo());
    let filename = loc.file.name.to_string();

    // Skip non-local files (stdlib, deps)
    if is_dependency_file(&filename) {
        return;
    }

    // Depth check — run exactly once per file per session
    if already_checked(sess, &filename) {
        return;
    }

    let depth = count_depth(&filename);

    // Default max depth is 4
    if depth > 4 {
        rustc_lint::span_lint_and_help(
            cx,
            MAX_DIRECTORY_DEPTH,
            span,
            &format!(
                "source file is nested {} levels deep (maximum allowed: 4). \
                 Use a flatter module structure.",
                depth
            ),
            None,
            "prefer feature-based flat modules over layered deep hierarchies",
        );
    }
}

fn is_dependency_file(path: &str) -> bool {
    // Paths from registry/cargo home are dependencies
    path.contains("/registry/") || path.contains("/.cargo/")
}

fn count_depth(path: &str) -> usize {
    let path = Path::new(path);

    // Find project root: walk up until we find Cargo.toml
    let root = find_project_root(path);

    // Strip root prefix and count directory components
    let relative = path
        .strip_prefix(&root)
        .unwrap_or(path);

    relative
        .parent() // strip filename
        .map(|p| p.components().count())
        .unwrap_or(0)
}

fn find_project_root(path: &Path) -> std::path::PathBuf {
    let mut current = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for _ in 0..10 {
        if current.join("Cargo.toml").exists() {
            return current;
        }
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }
    // Fallback to current directory
    std::env::current_dir().unwrap_or_default()
}

fn already_checked(sess: &Session, filename: &str) -> bool {
    // Use session diagnostic-context-like tracking if available,
    // otherwise use a thread-local set
    std::thread_local! {
        static CHECKED_FILES: std::cell::RefCell<HashSet<String>> =
            std::cell::RefCell::new(HashSet::new());
    }
    CHECKED_FILES.with(|set| {
        let mut files = set.borrow_mut();
        if files.contains(filename) {
            true
        } else {
            files.insert(filename.to_string());
            false
        }
    })
}
