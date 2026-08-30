//! Hyprloom command-line application.

#![allow(unused_crate_dependencies)]

use clap::{Parser, Subcommand};
use hyprloom::capture::capture_session;
use hyprloom::config::{app_config_for, config_path, legacy_sessions_dir, load_config, sessions_dir};
use hyprloom::hyprctl::{HyprctlClient, RealHyprctl};
use hyprloom::matching::MatchingStrategy;
use hyprloom::process::{ProcessInfoProvider, RealProcessInfo};
use hyprloom::restore::{
    recover_session_safely, replace_session_with_marker, replacement_target_is_complete_with_backup, restore_session, validate_replacement_targets,
    validate_safety_snapshot_with_config, ReplaceMarkerContext,
};
use hyprloom::session::{
    autosave_name_now, clear_replace_marker, delete_session, list_sessions, load_session, migrate_legacy_sessions, replace_marker, rotate_autosaves,
    save_session, session_exists, session_fingerprint, validate_user_session_name, OperationLock, ReplacePhase,
};
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

macro_rules! cli_println {
    ($($arg:tt)*) => {{
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, $($arg)*);
    }};
}

macro_rules! cli_eprintln {
    ($($arg:tt)*) => {{
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, $($arg)*);
    }};
}

#[derive(Parser)]
#[command(name = "hyprloom", version, about = "Save, restore, and reconcile Hyprland sessions")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Show detailed output
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Save current session
    Save {
        /// Session name (default: "latest")
        name: Option<String>,
        /// Overwrite without prompt
        #[arg(short, long)]
        force: bool,
    },
    /// Restore a saved session
    Restore {
        /// Session name (default: "latest")
        name: Option<String>,
        /// Preview without executing
        #[arg(short, long)]
        dry_run: bool,
        /// Match existing windows, repair only mismatches, and leave extras alone
        #[arg(long)]
        reconcile: bool,
        /// Emit a versioned per-window JSON report instead of human-readable results
        #[arg(long, requires = "reconcile", conflicts_with = "on_login")]
        report_json: bool,
        /// Use deterministic greedy matching instead of global assignment
        #[arg(long, requires = "reconcile")]
        greedy: bool,
        /// Skip restore if session is older than duration (e.g., 24h, 7d, 30m)
        #[arg(long)]
        max_age: Option<String>,
        /// Print exec-once line for Hyprland config
        #[arg(long)]
        on_login: bool,
    },
    /// Replace the current desktop and automatically attempt safety recovery.
    Replace {
        /// Session name to replace the current desktop with
        name: String,
        /// Emit a versioned per-window JSON report after replacement and recovery
        #[arg(long)]
        report_json: bool,
    },
    /// List saved sessions
    List,
    /// Delete a saved session
    Delete {
        /// Session name to delete
        name: String,
    },
    /// Show config info
    Config,
    /// Recover an interrupted desktop replacement without starting another restore
    Recover,
    /// Manage autosave (status, run, install/uninstall timer)
    Autosave {
        /// Run autosave now (capture + rotate)
        #[arg(long)]
        now: bool,
        /// Install systemd timer
        #[arg(long)]
        install: bool,
        /// Uninstall systemd timer
        #[arg(long)]
        uninstall: bool,
    },
}

// The CLI dispatch keeps startup recovery and all subcommands in one ordered
// transaction boundary; moving those branches apart would obscure when the
// lock, migration, and recovery checks run.
#[allow(clippy::excessive_nesting)]
#[allow(clippy::too_many_lines)]
fn main() {
    let cli = Cli::parse();
    let config = load_config();
    let sessions_dir = sessions_dir();
    let _operation_lock = match OperationLock::acquire() {
        Ok(lock) => lock,
        Err(error) => {
            cli_eprintln!("Error acquiring hyprloom operation lock: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = migrate_legacy_sessions(&sessions_dir, &legacy_sessions_dir()) {
        cli_eprintln!("Warning: could not migrate legacy HyprFlow sessions: {error}");
    }
    let requires_clean_recovery = command_requires_clean_recovery(&cli.command);
    let recovery_ready = if requires_clean_recovery {
        recover_pending_replace(&sessions_dir, &config)
    } else {
        true
    };
    if !recovery_ready && requires_clean_recovery {
        cli_eprintln!("A previous desktop replacement still needs recovery; retry after Hyprland is available.");
        std::process::exit(1);
    }

    match cli.command {
        Commands::Save { name, force } => {
            let name = name.unwrap_or_else(|| config.general.default_session.clone());

            if let Err(error) = validate_user_session_name(&name) {
                cli_eprintln!("Error: {error}");
                std::process::exit(1);
            }

            if !force && session_exists(&name, &sessions_dir) && name != "latest" {
                cli_eprintln!("Session '{}' already exists. Use --force to overwrite.", name);
                std::process::exit(1);
            }

            let hyprctl = RealHyprctl;
            let process_info = RealProcessInfo;

            match capture_session(&name, &hyprctl, &process_info, &config) {
                Ok(session) => {
                    let client_count = session.clients.len();
                    match save_session(&session, &sessions_dir) {
                        Ok(()) => {
                            cli_println!("Saved session '{}' ({} windows)", name, client_count);
                            if cli.verbose {
                                for c in &session.clients {
                                    cli_println!("  ws={} {} — {}", c.workspace, c.class, c.title);
                                }
                            }
                        }
                        Err(e) => {
                            cli_eprintln!("Error saving session: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    cli_eprintln!("Error capturing session: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Commands::Restore {
            name,
            dry_run,
            reconcile,
            report_json,
            greedy,
            max_age,
            on_login,
        } => {
            if on_login {
                cli_println!("Add this line to ~/.config/hypr/hyprland.conf:");
                cli_println!();
                cli_println!("  exec-once = hyprloom restore --reconcile --max-age 24h");
                cli_println!();
                cli_println!("This will reconcile your last saved session on login.");
                cli_println!("Sessions older than 24h will be skipped.");
                return;
            }

            let name = name.unwrap_or_else(|| config.general.default_session.clone());

            let session = match load_session(&name, &sessions_dir) {
                Ok(s) => s,
                Err(hyprloom::session::SessionError::NotFound(_)) => {
                    // If requesting the default session and it doesn't exist,
                    // fall back to the most recent autosave
                    if name == config.general.default_session {
                        match hyprloom::session::list_autosave_sessions(&sessions_dir) {
                            Ok(autosaves) if !autosaves.is_empty() => {
                                let fallback_name = &autosaves[0].name;
                                restore_notice(report_json, &format!("Session '{name}' not found. Falling back to '{fallback_name}'."));
                                match load_session(fallback_name, &sessions_dir) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        cli_eprintln!("Error loading fallback session: {}", e);
                                        std::process::exit(1);
                                    }
                                }
                            }
                            _ => {
                                cli_eprintln!("Session '{}' not found and no autosave sessions available.", name);
                                std::process::exit(1);
                            }
                        }
                    } else {
                        cli_eprintln!("Error: session '{}' not found", name);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    cli_eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };

            if let Some(ref age_str) = max_age {
                match hyprloom::session::parse_max_age(age_str) {
                    Ok(max_duration) => {
                        let age = chrono::Utc::now() - session.created_at;
                        if age < chrono::Duration::zero() {
                            restore_notice(
                                report_json,
                                &format!(
                                    "Session '{}' has a future timestamp (created {}).",
                                    session.name,
                                    session.created_at.format("%Y-%m-%d %H:%M")
                                ),
                            );
                            restore_notice(report_json, &format!("Skipping restore (max age: {age_str})."));
                            return;
                        }
                        if age > max_duration {
                            restore_notice(
                                report_json,
                                &format!(
                                    "Session '{}' is too old (created {}).",
                                    session.name,
                                    session.created_at.format("%Y-%m-%d %H:%M")
                                ),
                            );
                            restore_notice(report_json, &format!("Skipping restore (max age: {age_str})."));
                            return;
                        }
                    }
                    Err(e) => {
                        cli_eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            let hyprctl = RealHyprctl;
            if reconcile {
                let process_info = RealProcessInfo;
                let strategy = if greedy { MatchingStrategy::Greedy } else { MatchingStrategy::Global };
                match hyprloom::restore::reconcile_session_with_strategy(&session, &hyprctl, &process_info, &config, dry_run, cli.verbose, strategy) {
                    Ok(report) => {
                        if report_json {
                            print_json_report(&session.name, "reconcile", dry_run, &report, None);
                        } else {
                            if dry_run {
                                cli_println!("Dry run for reconciliation of '{}':", session.name);
                            } else {
                                cli_println!("Reconciled session '{}':", session.name);
                            }
                            cli_println!(
                                "  {} unchanged, {} moved, {} launched, {} extra left alone, {} skipped, {} failed",
                                report.unchanged,
                                report.moved,
                                report.launched,
                                report.extras,
                                report.skipped,
                                report.failed
                            );
                            for detail in &report.details {
                                cli_println!("  {}", detail);
                            }
                        }
                        if report.failed > 0 || report.skipped > 0 {
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        cli_eprintln!("Error reconciling session: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                match restore_session(&session, &hyprctl, &config, dry_run, cli.verbose) {
                    Ok(report) => {
                        if dry_run {
                            cli_println!("Dry run for session '{}':", session.name);
                        } else {
                            cli_println!("Restored session '{}':", session.name);
                        }
                        cli_println!("  {} restored, {} skipped, {} failed", report.restored, report.skipped, report.failed);
                        for detail in &report.details {
                            cli_println!("  {}", detail);
                        }
                        if report.failed > 0 {
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        cli_eprintln!("Error restoring session: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }

        Commands::Replace { name, report_json } => {
            let session = match load_session(&name, &sessions_dir) {
                Ok(session) => session,
                Err(error) => {
                    cli_eprintln!("Error loading session: {error}");
                    std::process::exit(1);
                }
            };
            let target_digest = match session_fingerprint(&session) {
                Ok(digest) => digest,
                Err(error) => {
                    cli_eprintln!("Replace cancelled: could not fingerprint target session: {error}");
                    std::process::exit(1);
                }
            };

            // Validate the complete target before capturing or closing the
            // current desktop.  The replace implementation loads this same
            // session in memory and performs the close/restore sequence in a
            // single process.
            if let Err(error) = validate_replacement_targets(&session, &config) {
                cli_eprintln!("Replace cancelled: {error}");
                std::process::exit(1);
            }

            let hyprctl = RealHyprctl;
            let process_info = RealProcessInfo;
            let backup_name = autosave_name_now();
            let recovery_config = safety_recovery_config(&config);
            let backup = match capture_session(&backup_name, &hyprctl, &process_info, &recovery_config) {
                Ok(session) => session,
                Err(error) => {
                    cli_eprintln!("Replace cancelled: safety backup failed: {error}");
                    std::process::exit(1);
                }
            };
            if let Err(error) = validate_safety_snapshot_with_config(&backup, &recovery_config) {
                cli_eprintln!("Replace cancelled: {error}");
                std::process::exit(1);
            }
            if let Err(error) = save_session(&backup, &sessions_dir) {
                cli_eprintln!("Replace cancelled: safety backup could not be saved: {error}");
                std::process::exit(1);
            }
            if let Err(error) =
                hyprloom::session::mark_replace_prepared_for_target_with_digest(&backup_name, Some(&name), Some(&target_digest), &sessions_dir)
            {
                cli_eprintln!("Replace cancelled: could not record recovery marker: {error}");
                std::process::exit(1);
            }
            if let Err(error) = rotate_autosaves(&sessions_dir, config.general.autosave_retain) {
                cli_eprintln!("Replace cancelled: could not rotate autosaves: {error}");
                clear_recovery_marker(&sessions_dir);
                std::process::exit(1);
            }

            match replace_session_with_marker(
                &session,
                &hyprctl,
                &process_info,
                &config,
                ReplaceMarkerContext {
                    dry_run: false,
                    verbose: cli.verbose,
                    backup_name: &backup_name,
                    target_name: &name,
                    sessions_dir: &sessions_dir,
                    safety_snapshot: Some(&backup),
                },
            ) {
                Ok(report) => {
                    if !report_json {
                        cli_println!(
                            "Replaced desktop with '{}' ({} unchanged, {} moved, {} launched, {} extra left alone, {} failed). Safety backup: '{}'.",
                            session.name,
                            report.unchanged,
                            report.moved,
                            report.launched,
                            report.extras,
                            report.failed,
                            backup_name
                        );
                        for detail in &report.details {
                            cli_println!("  {detail}");
                        }
                    }
                    if report.failed > 0 || report.skipped > 0 {
                        cli_eprintln!("Replacement was incomplete; attempting safety recovery from '{}'.", backup_name);
                        let recovered = report_non_destructive_safety_recovery(&backup, &hyprctl, &process_info, &config, cli.verbose);
                        if recovered {
                            clear_recovery_marker_and_rotate(&sessions_dir, config.general.autosave_retain);
                        }
                        if report_json {
                            print_json_report(
                                &session.name,
                                "replace",
                                false,
                                &report,
                                Some(if recovered { "succeeded" } else { "failed" }),
                            );
                        }
                        std::process::exit(1);
                    } else if !clear_recovery_marker_and_rotate(&sessions_dir, config.general.autosave_retain) {
                        cli_eprintln!("Replacement completed, but its recovery marker could not be cleared; retry after checking the session store.");
                        if report_json {
                            print_json_report(&session.name, "replace", false, &report, None);
                        }
                        std::process::exit(1);
                    }
                    if report_json {
                        print_json_report(&session.name, "replace", false, &report, None);
                    }
                }
                Err(error) => {
                    cli_eprintln!("Error replacing desktop: {error}");
                    if let hyprloom::restore::RestoreError::TransactionAfterRestore(_) = &error {
                        cli_eprintln!("The target desktop was restored, so safety recovery is not being replayed.");
                        if !clear_recovery_marker_and_rotate(&sessions_dir, config.general.autosave_retain) {
                            cli_eprintln!(
                                "The recovery marker could not be cleared; leave the desktop in place and retry cleanup after checking the session store."
                            );
                        }
                        std::process::exit(1);
                    }
                    match replacement_has_started(&sessions_dir, &hyprctl, &config) {
                        Some(true) => {
                            cli_eprintln!("Attempting safety recovery from '{backup_name}' before giving up.");
                            if report_non_destructive_safety_recovery(&backup, &hyprctl, &process_info, &config, cli.verbose) {
                                clear_recovery_marker_and_rotate(&sessions_dir, config.general.autosave_retain);
                            }
                        }
                        Some(false) => {
                            cli_eprintln!("Replacement did not start closing windows; leaving the desktop unchanged.");
                            clear_recovery_marker_and_rotate(&sessions_dir, config.general.autosave_retain);
                        }
                        None => {
                            cli_eprintln!("Could not determine whether replacement started; leaving the recovery marker for manual recovery.");
                        }
                    }
                    std::process::exit(1);
                }
            }
        }

        Commands::List => match list_sessions(&sessions_dir) {
            Ok(sessions) => {
                if sessions.is_empty() {
                    cli_println!("No saved sessions.");
                } else {
                    cli_println!("Saved sessions:");
                    for s in sessions {
                        let tag = if s.name.starts_with("autosave-") { " [auto]" } else { "" };
                        cli_println!(
                            "  {} — {} windows ({}){}",
                            s.name,
                            s.client_count,
                            s.created_at.format("%Y-%m-%d %H:%M"),
                            tag
                        );
                    }
                }
            }
            Err(e) => {
                cli_eprintln!("Error listing sessions: {}", e);
                std::process::exit(1);
            }
        },

        Commands::Delete { name } => match delete_session(&name, &sessions_dir) {
            Ok(()) => cli_println!("Deleted session '{}'", name),
            Err(e) => {
                cli_eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },

        Commands::Config => {
            cli_println!("Config path: {}", config_path().display());
            cli_println!("Sessions dir: {}", sessions_dir.display());
            cli_println!("Default session: {}", config.general.default_session);
            cli_println!("Restore delay: {}ms", config.general.restore_delay_ms);
            cli_println!("Window detect timeout: {}ms", config.general.window_detect_timeout_ms);
            cli_println!("Ignored classes: {:?}", config.filters.ignore_classes);
            if !config.apps.is_empty() {
                cli_println!("App configs:");
                for (name, app) in &config.apps {
                    cli_println!("  {}: binary={:?} capture_cwd={:?}", name, app.binary, app.capture_cwd);
                }
            }

            // Show Brave profile mappings
            cli_println!();
            match hyprloom::brave::read_profiles() {
                Ok(profiles) if !profiles.is_empty() => {
                    cli_println!("Brave profiles detected:");
                    let profile_ws = app_config_for(&config, "brave-browser", "").and_then(|c| c.profile_workspaces.as_ref());
                    for profile in &profiles {
                        if let Some(ws) = profile_ws.and_then(|m| m.get(&profile.directory)) {
                            cli_println!("  ✓ {} ({}) → ws={}", profile.directory, profile.name, ws);
                        } else {
                            cli_println!("  · {} ({}) — not mapped, will be skipped", profile.directory, profile.name);
                        }
                    }
                    if profile_ws.is_none() {
                        cli_println!("  (no profile_workspaces configured — all profiles will be captured)");
                    }
                }
                Ok(_) => cli_println!("No Brave profiles detected."),
                Err(e) => cli_println!("Could not read Brave profiles: {e}"),
            }
        }

        Commands::Recover => {
            cli_println!("Replacement recovery check completed.");
        }

        Commands::Autosave { now, install, uninstall } => {
            let flag_count = [now, install, uninstall].iter().filter(|&&f| f).count();
            if flag_count > 1 {
                cli_eprintln!("Error: only one of --now, --install, --uninstall may be specified at a time.");
                std::process::exit(1);
            }

            let systemd_dir = hyprloom::autosave::systemd_user_dir();

            if install {
                match hyprloom::autosave::install(&systemd_dir) {
                    Ok((service_path, timer_path)) => {
                        cli_println!("Created:");
                        cli_println!("  {}", service_path.display());
                        cli_println!("  {}", timer_path.display());
                        cli_println!();
                        cli_println!("To enable and start:");
                        cli_println!("  systemctl --user enable --now hyprloom-autosave.timer");
                        cli_println!();
                        cli_println!("To check status:");
                        cli_println!("  systemctl --user status hyprloom-autosave.timer");
                        cli_println!("  journalctl --user -u hyprloom-autosave.service");
                    }
                    Err(e) => {
                        cli_eprintln!("Error installing autosave timer: {}", e);
                        std::process::exit(1);
                    }
                }
            } else if uninstall {
                match hyprloom::autosave::uninstall(&systemd_dir) {
                    Ok(()) => cli_println!("Autosave timer removed."),
                    Err(e) => {
                        cli_eprintln!("Error uninstalling autosave timer: {}", e);
                        std::process::exit(1);
                    }
                }
            } else if now {
                let hyprctl = RealHyprctl;
                let process_info = RealProcessInfo;
                let name = hyprloom::session::autosave_name_now();

                match capture_session(&name, &hyprctl, &process_info, &config) {
                    Ok(session) => {
                        let client_count = session.clients.len();
                        if let Err(e) = save_session(&session, &sessions_dir) {
                            cli_eprintln!("Error saving autosave session: {}", e);
                            std::process::exit(1);
                        }

                        let retain = config.general.autosave_retain;
                        let total_before = match hyprloom::session::list_autosave_sessions(&sessions_dir) {
                            Ok(sessions) => sessions.len(),
                            Err(error) => {
                                cli_eprintln!("Error listing autosaves for rotation: {error}");
                                std::process::exit(1);
                            }
                        };
                        let pruned = match hyprloom::session::rotate_autosaves(&sessions_dir, retain) {
                            Ok(pruned) => pruned,
                            Err(error) => {
                                cli_eprintln!("Error rotating autosaves: {error}");
                                std::process::exit(1);
                            }
                        };
                        let retained = total_before.saturating_sub(pruned);

                        cli_println!(
                            "Autosaved '{}' ({} windows). Retained {}, pruned {}.",
                            name,
                            client_count,
                            retained,
                            pruned
                        );
                    }
                    Err(e) => {
                        cli_eprintln!("Error capturing session: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                // Status mode (no flags)
                let installed = hyprloom::autosave::is_installed(&systemd_dir);
                let active = hyprloom::autosave::is_active();

                if !installed {
                    cli_println!("Autosave is not configured.");
                    cli_println!("Run 'hyprloom autosave --install' to set up the systemd timer.");
                } else if !active {
                    cli_println!("Autosave timer is installed but not active.");
                    cli_println!("  systemctl --user enable --now hyprloom-autosave.timer");
                } else {
                    cli_println!("Autosave is active (every 10min).");
                    match hyprloom::session::list_autosave_sessions(&sessions_dir) {
                        Ok(sessions) if !sessions.is_empty() => {
                            let latest = &sessions[0];
                            cli_println!(
                                "Last: {} — {} windows",
                                latest.created_at.format("%Y-%m-%d %H:%M:%S"),
                                latest.client_count
                            );
                            if let Some(oldest) = sessions.last() {
                                cli_println!("Retained: {} sessions (oldest: {})", sessions.len(), oldest.name);
                            }
                        }
                        Ok(_) => cli_println!("No autosave sessions yet."),
                        Err(e) => cli_println!("Could not list sessions: {e}"),
                    }
                    cli_println!("To disable: hyprloom autosave --uninstall");
                }
            }
        }
    }
}

fn clear_recovery_marker(sessions_dir: &Path) -> bool {
    if let Err(error) = clear_replace_marker(sessions_dir) {
        cli_eprintln!("Warning: could not clear replacement recovery marker: {error}");
        return false;
    }
    true
}

fn clear_recovery_marker_and_rotate(sessions_dir: &Path, retain: usize) -> bool {
    if !clear_recovery_marker(sessions_dir) {
        return false;
    }
    if let Err(error) = rotate_autosaves(sessions_dir, retain) {
        cli_eprintln!("Warning: could not rotate autosaves after replacement: {error}");
        return false;
    }
    true
}

// Startup recovery is a deliberately ordered state machine over the durable
// marker phases; keeping those branches together makes crash behavior clear.
#[allow(clippy::excessive_nesting)]
#[allow(clippy::too_many_lines)]
fn recover_pending_replace(sessions_dir: &Path, config: &hyprloom::config::Config) -> bool {
    let marker = match replace_marker(sessions_dir) {
        Ok(Some(marker)) => marker,
        Ok(None) => return true,
        Err(error) => {
            cli_eprintln!("Warning: could not inspect replacement recovery marker: {error}");
            return false;
        }
    };
    if matches!(marker.phase, ReplacePhase::Finalizing | ReplacePhase::Committed) {
        cli_eprintln!("Found a completed desktop replacement; finalizing its recovery marker without replaying the old snapshot.");
        return clear_recovery_marker_and_rotate(sessions_dir, config.general.autosave_retain);
    }
    if marker.phase == ReplacePhase::Prepared {
        cli_eprintln!("Found a replacement that was prepared but never started; leaving the current desktop unchanged.");
        return clear_recovery_marker_and_rotate(sessions_dir, config.general.autosave_retain);
    }
    if marker.phase == ReplacePhase::Closing {
        let Some(closing_address) = marker.closing_address.as_deref() else {
            cli_eprintln!("Warning: replacement marker is missing the window address for its closing phase.");
            return false;
        };
        let hyprctl = RealHyprctl;
        let process_info = RealProcessInfo;
        match replacement_close_started(
            closing_address,
            marker.closing_pid,
            marker.closing_start_time,
            marker.closing_stable_id.as_deref(),
            &hyprctl,
            &process_info,
            config,
        ) {
            Some(false) => {
                cli_eprintln!("Found a replacement that had not confirmed its first close; leaving the current desktop unchanged.");
                return clear_recovery_marker_and_rotate(sessions_dir, config.general.autosave_retain);
            }
            None => {
                cli_eprintln!(
                    "Warning: could not determine whether the first replacement close was applied; leaving the recovery marker for manual recovery."
                );
                return false;
            }
            Some(true) => {}
        }
    }
    if marker.phase == ReplacePhase::InProgress {
        if let Some(target_name) = marker.target_name.as_deref() {
            let hyprctl = RealHyprctl;
            let process_info = RealProcessInfo;
            let backup_for_completion = load_session(&marker.backup_name, sessions_dir).ok();
            match load_session(target_name, sessions_dir) {
                Ok(target) => {
                    let target_matches_marker = marker
                        .target_digest
                        .as_deref()
                        .is_some_and(|expected_digest| session_fingerprint(&target).is_ok_and(|actual_digest| actual_digest == expected_digest));
                    if target_matches_marker {
                        match backup_for_completion
                            .as_ref()
                            .map(|backup| replacement_target_is_complete_with_backup(&target, Some(backup), &hyprctl, &process_info, config))
                        {
                            Some(Ok(true)) => {
                                cli_eprintln!(
                                    "Found a replacement whose target windows are already present; preserving the current desktop and finalizing its recovery marker."
                                );
                                return clear_recovery_marker_and_rotate(sessions_dir, config.general.autosave_retain);
                            }
                            Some(Ok(false)) | None => {}
                            Some(Err(error)) => {
                                cli_eprintln!("Warning: could not verify the replacement target; attempting safety recovery: {error}");
                            }
                        }
                    } else if marker.target_digest.is_some() {
                        cli_eprintln!(
                            "Warning: replacement target changed since the transaction started; attempting safety recovery instead of accepting the edited target."
                        );
                    }
                }
                Err(error) => cli_eprintln!("Warning: could not load the replacement target; attempting safety recovery: {error}"),
            }
        }
    }
    let backup_name = marker.backup_name;
    cli_eprintln!("Found an interrupted desktop replacement; attempting recovery from '{backup_name}'.");
    let backup = match load_session(&backup_name, sessions_dir) {
        Ok(backup) => backup,
        Err(error) => {
            cli_eprintln!("Warning: could not load replacement recovery snapshot: {error}");
            return false;
        }
    };
    let hyprctl = RealHyprctl;
    let process_info = RealProcessInfo;
    // Marker-backed recovery is deliberately non-destructive.  A legacy
    // marker has no target identity or safety snapshot metadata, so the old
    // exact-replace path could close the current desktop before discovering
    // that the backup could not be replayed.  Leaving current windows alone
    // preserves user work and still repairs/launches only proven targets.
    let recovered = report_non_destructive_safety_recovery(&backup, &hyprctl, &process_info, config, false);
    if recovered {
        return clear_recovery_marker_and_rotate(sessions_dir, config.general.autosave_retain);
    }
    false
}

fn replacement_has_started(sessions_dir: &Path, hyprctl: &dyn HyprctlClient, config: &hyprloom::config::Config) -> Option<bool> {
    match replace_marker(sessions_dir) {
        Ok(Some(marker)) => match marker.phase {
            ReplacePhase::InProgress => Some(true),
            ReplacePhase::Closing => marker.closing_address.as_deref().and_then(|address| {
                let process_info = RealProcessInfo;
                replacement_close_started(
                    address,
                    marker.closing_pid,
                    marker.closing_start_time,
                    marker.closing_stable_id.as_deref(),
                    hyprctl,
                    &process_info,
                    config,
                )
            }),
            ReplacePhase::Prepared | ReplacePhase::Finalizing | ReplacePhase::Committed => Some(false),
        },
        Ok(None) => Some(false),
        Err(error) => {
            cli_eprintln!("Warning: could not inspect replacement recovery marker: {error}");
            None
        }
    }
}

/// A close dispatch is asynchronous: `hyprctl dispatch closewindow` can
/// succeed before the client disappears from `hyprctl clients -j`.  When a
/// replacement marker is in its first-close phase, wait for that address to
/// disappear before deciding whether recovery is necessary.  If it remains
/// for the normal detection window, the close was not confirmed and it is
/// safer to leave the user's desktop alone than to replay a full backup.
// Close confirmation combines compositor polling with process identity; the
// nested checks represent the available levels of evidence for one address.
#[allow(clippy::excessive_nesting)]
#[allow(clippy::too_many_arguments)]
fn replacement_close_started(
    address: &str,
    expected_pid: Option<u32>,
    expected_start_time: Option<u64>,
    expected_stable_id: Option<&str>,
    hyprctl: &dyn HyprctlClient,
    process_info: &dyn ProcessInfoProvider,
    config: &hyprloom::config::Config,
) -> Option<bool> {
    let timeout = Duration::from_millis(config.general.window_detect_timeout_ms.clamp(
        hyprloom::config::MIN_WINDOW_DETECT_TIMEOUT_MS,
        hyprloom::config::MAX_WINDOW_DETECT_TIMEOUT_MS,
    ));
    let started = Instant::now();
    loop {
        let clients = match hyprctl.get_clients() {
            Ok(clients) => clients,
            Err(error) => {
                cli_eprintln!("Warning: could not determine whether the first replacement close was applied: {error}");
                return None;
            }
        };
        let Some(current) = clients.iter().find(|client| client.address == address) else {
            return Some(true);
        };

        let mut stable_identity_confirmed = false;
        if let Some(expected_stable_id) = expected_stable_id {
            match current.stable_id.as_deref() {
                Some(current_stable_id) if expected_stable_id.eq_ignore_ascii_case(current_stable_id) => {
                    // Hyprland's stable ID is window-specific and proves
                    // that this is still the original client.  It still does
                    // not prove that an asynchronous close dispatch has not
                    // merely been queued, so the timeout remains unknown.
                    stable_identity_confirmed = true;
                }
                Some(_) => {
                    // The old address now belongs to a different window.
                    return Some(true);
                }
                None => {
                    // A compositor response without the stable ID cannot
                    // prove that a same-process window is the original one.
                    // Fall through to process evidence only for the case in
                    // which it provides positive proof of a different owner.
                }
            }
        }

        if !stable_identity_confirmed {
            if let Some(expected_pid) = expected_pid {
                if current.pid != expected_pid {
                    // The old window is gone and this address now belongs to a
                    // different process.  Treat the close as started instead of
                    // waiting for (or deleting) the replacement backup.
                    return Some(true);
                }
                if let Some(expected_start_time) = expected_start_time {
                    if let Ok(current_start_time) = process_info.get_start_time(current.pid) {
                        if current_start_time != expected_start_time {
                            return Some(true);
                        }
                    }
                }
            }
        }
        if started.elapsed() >= timeout {
            // A close dispatch is asynchronous.  Even positive identity
            // evidence cannot prove that a successful dispatch has not merely
            // been queued behind this poll.  Never clear the marker while the
            // address remains visible; the next startup can retry safely once
            // the compositor has settled.
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn restore_notice(report_json: bool, message: &str) {
    if report_json {
        cli_eprintln!("{message}");
    } else {
        cli_println!("{message}");
    }
}

fn print_json_report(name: &str, operation: &str, dry_run: bool, report: &hyprloom::restore::ReconcileReport, recovery: Option<&str>) {
    let document = serde_json::json!({
        "schema_version": 1,
        "operation": operation,
        "session": name,
        "dry_run": dry_run,
        "report": report,
        "recovery": recovery,
    });
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = serde_json::to_writer(&mut stdout, &document).and_then(|()| writeln!(stdout).map_err(serde_json::Error::io)) {
        cli_eprintln!("Error writing restore report: {error}");
        std::process::exit(1);
    }
}

fn command_requires_clean_recovery(command: &Commands) -> bool {
    match command {
        Commands::List | Commands::Config => false,
        Commands::Restore { dry_run, on_login, .. } => !dry_run && !on_login,
        Commands::Autosave { now, .. } => *now,
        Commands::Save { .. } | Commands::Replace { .. } | Commands::Delete { .. } | Commands::Recover => true,
    }
}

fn report_non_destructive_safety_recovery(
    backup: &hyprloom::session::Session,
    hyprctl: &RealHyprctl,
    process_info: &RealProcessInfo,
    config: &hyprloom::config::Config,
    verbose: bool,
) -> bool {
    let recovery_config = safety_recovery_config(config);
    report_recovery_result(
        recover_session_safely(backup, hyprctl, process_info, &recovery_config, false, verbose),
        "Non-destructive safety recovery",
    )
}

fn report_recovery_result(result: Result<hyprloom::restore::ReconcileReport, hyprloom::restore::RestoreError>, label: &str) -> bool {
    match result {
        Ok(report) if report.failed == 0 && report.skipped == 0 => {
            cli_eprintln!(
                "{label} pass completed: {} unchanged, {} moved, {} launched, {} skipped.",
                report.unchanged,
                report.moved,
                report.launched,
                report.skipped
            );
            for detail in &report.details {
                cli_eprintln!("  recovery: {detail}");
            }
            true
        }
        Ok(report) => {
            cli_eprintln!(
                "{label} was partial: {} unchanged, {} moved, {} launched, {} skipped, {} failed.",
                report.unchanged,
                report.moved,
                report.launched,
                report.skipped,
                report.failed
            );
            for detail in &report.details {
                cli_eprintln!("  recovery: {detail}");
            }
            cli_eprintln!("The safety backup remains available for another retry.");
            false
        }
        Err(error) => {
            cli_eprintln!("{label} could not run: {error}");
            cli_eprintln!("The safety backup remains available for another retry.");
            false
        }
    }
}

fn safety_recovery_config(config: &hyprloom::config::Config) -> hyprloom::config::Config {
    let mut recovery_config = config.clone();
    // Replace closes every current client, including classes excluded from
    // ordinary snapshots.  The recovery copy must therefore include those
    // clients too; otherwise a failed replacement cannot restore them.
    recovery_config.filters.ignore_classes.clear();
    // A safety snapshot must preserve every currently open Brave profile, not
    // only the profiles selected for ordinary restore.  Otherwise an active
    // unmapped profile can disappear when recovery reconstructs the snapshot
    // through the profile-aware path.
    for (class, app) in &mut recovery_config.apps {
        if class.eq_ignore_ascii_case("brave-browser") {
            app.profile_workspaces = None;
        }
    }
    recovery_config
}
