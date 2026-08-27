use clap::{Parser, Subcommand};
use hyprloom::capture::capture_session;
use hyprloom::config::{
    app_config_for, config_path, legacy_sessions_dir, load_config, sessions_dir,
};
use hyprloom::hyprctl::{HyprctlClient, RealHyprctl};
use hyprloom::process::RealProcessInfo;
use hyprloom::restore::{
    recover_session, replace_session_with_marker, restore_session, validate_replacement_targets,
    ReplaceMarkerContext,
};
use hyprloom::session::{
    autosave_name_now, clear_replace_marker, delete_session, list_sessions, load_session,
    mark_replace_prepared, migrate_legacy_sessions, replace_marker, save_session, session_exists,
    validate_user_session_name, OperationLock, ReplacePhase,
};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "hyprloom",
    version,
    about = "Save, restore, and reconcile Hyprland sessions"
)]
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

fn main() {
    let cli = Cli::parse();
    let config = load_config();
    let sessions_dir = sessions_dir();
    let _operation_lock = match OperationLock::acquire() {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("Error acquiring hyprloom operation lock: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = migrate_legacy_sessions(&sessions_dir, &legacy_sessions_dir()) {
        eprintln!("Warning: could not migrate legacy hyprflow sessions: {error}");
    }
    let requires_clean_recovery = command_requires_clean_recovery(&cli.command);
    let recovery_ready = if requires_clean_recovery {
        recover_pending_replace(&sessions_dir, &config)
    } else {
        true
    };
    if !recovery_ready && requires_clean_recovery {
        eprintln!(
            "A previous desktop replacement still needs recovery; retry after Hyprland is available."
        );
        std::process::exit(1);
    }

    match cli.command {
        Commands::Save { name, force } => {
            let name = name.unwrap_or_else(|| config.general.default_session.clone());

            if let Err(error) = validate_user_session_name(&name) {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }

            if !force && session_exists(&name, &sessions_dir) && name != "latest" {
                eprintln!(
                    "Session '{}' already exists. Use --force to overwrite.",
                    name
                );
                std::process::exit(1);
            }

            let hyprctl = RealHyprctl;
            let process_info = RealProcessInfo;

            match capture_session(&name, &hyprctl, &process_info, &config) {
                Ok(session) => {
                    let client_count = session.clients.len();
                    match save_session(&session, &sessions_dir) {
                        Ok(()) => {
                            println!("Saved session '{}' ({} windows)", name, client_count);
                            if cli.verbose {
                                for c in &session.clients {
                                    println!("  ws={} {} — {}", c.workspace, c.class, c.title);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Error saving session: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error capturing session: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Commands::Restore {
            name,
            dry_run,
            reconcile,
            max_age,
            on_login,
        } => {
            if on_login {
                println!("Add this line to ~/.config/hypr/hyprland.conf:");
                println!();
                println!("  exec-once = hyprloom restore --reconcile --max-age 24h");
                println!();
                println!("This will reconcile your last saved session on login.");
                println!("Sessions older than 24h will be skipped.");
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
                                println!(
                                    "Session '{}' not found. Falling back to '{}'.",
                                    name, fallback_name
                                );
                                match load_session(fallback_name, &sessions_dir) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        eprintln!("Error loading fallback session: {}", e);
                                        std::process::exit(1);
                                    }
                                }
                            }
                            _ => {
                                eprintln!(
                                    "Session '{}' not found and no autosave sessions available.",
                                    name
                                );
                                std::process::exit(1);
                            }
                        }
                    } else {
                        eprintln!("Error: session '{}' not found", name);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };

            if let Some(ref age_str) = max_age {
                match hyprloom::session::parse_max_age(age_str) {
                    Ok(max_duration) => {
                        let age = chrono::Utc::now() - session.created_at;
                        if age < chrono::Duration::zero() {
                            println!(
                                "Session '{}' has a future timestamp (created {}).",
                                session.name,
                                session.created_at.format("%Y-%m-%d %H:%M")
                            );
                            println!("Skipping restore (max age: {}).", age_str);
                            return;
                        }
                        if age > max_duration {
                            println!(
                                "Session '{}' is too old (created {}).",
                                session.name,
                                session.created_at.format("%Y-%m-%d %H:%M")
                            );
                            println!("Skipping restore (max age: {}).", age_str);
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            let hyprctl = RealHyprctl;
            if reconcile {
                let process_info = RealProcessInfo;
                match hyprloom::restore::reconcile_session(
                    &session,
                    &hyprctl,
                    &process_info,
                    &config,
                    dry_run,
                    cli.verbose,
                ) {
                    Ok(report) => {
                        if dry_run {
                            println!("Dry run for reconciliation of '{}':", session.name);
                        } else {
                            println!("Reconciled session '{}':", session.name);
                        }
                        println!(
                            "  {} unchanged, {} moved, {} launched, {} extra left alone, {} skipped, {} failed",
                            report.unchanged,
                            report.moved,
                            report.launched,
                            report.extras,
                            report.skipped,
                            report.failed
                        );
                        for detail in &report.details {
                            println!("  {}", detail);
                        }
                        if report.failed > 0 || report.skipped > 0 {
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error reconciling session: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                match restore_session(&session, &hyprctl, &config, dry_run, cli.verbose) {
                    Ok(report) => {
                        if dry_run {
                            println!("Dry run for session '{}':", session.name);
                        } else {
                            println!("Restored session '{}':", session.name);
                        }
                        println!(
                            "  {} restored, {} skipped, {} failed",
                            report.restored, report.skipped, report.failed
                        );
                        for detail in &report.details {
                            println!("  {}", detail);
                        }
                        if report.failed > 0 {
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error restoring session: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }

        Commands::Replace { name } => {
            let session = match load_session(&name, &sessions_dir) {
                Ok(session) => session,
                Err(error) => {
                    eprintln!("Error loading session: {error}");
                    std::process::exit(1);
                }
            };

            // Validate the complete target before capturing or closing the
            // current desktop.  The replace implementation loads this same
            // session in memory and performs the close/restore sequence in a
            // single process.
            if let Err(error) = validate_replacement_targets(&session, &config) {
                eprintln!("Replace cancelled: {error}");
                std::process::exit(1);
            }

            let hyprctl = RealHyprctl;
            let process_info = RealProcessInfo;
            let backup_name = autosave_name_now();
            let recovery_config = safety_recovery_config(&config);
            let backup =
                match capture_session(&backup_name, &hyprctl, &process_info, &recovery_config) {
                    Ok(session) => session,
                    Err(error) => {
                        eprintln!("Replace cancelled: safety backup failed: {error}");
                        std::process::exit(1);
                    }
                };
            if let Err(error) = save_session(&backup, &sessions_dir) {
                eprintln!("Replace cancelled: safety backup could not be saved: {error}");
                std::process::exit(1);
            }
            if let Err(error) = mark_replace_prepared(&backup_name, &sessions_dir) {
                eprintln!("Replace cancelled: could not record recovery marker: {error}");
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
                    sessions_dir: &sessions_dir,
                },
            ) {
                Ok(report) => {
                    println!(
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
                        println!("  {detail}");
                    }
                    if report.failed > 0 || report.skipped > 0 {
                        eprintln!(
                            "Replacement was incomplete; attempting safety recovery from '{}'.",
                            backup_name
                        );
                        if report_safety_recovery(
                            &backup,
                            &hyprctl,
                            &process_info,
                            &config,
                            cli.verbose,
                        ) {
                            clear_recovery_marker(&sessions_dir);
                        }
                        std::process::exit(1);
                    } else {
                        if !clear_recovery_marker(&sessions_dir) {
                            eprintln!(
                                "Replacement completed, but its recovery marker could not be cleared; retry after checking the session store."
                            );
                            std::process::exit(1);
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Error replacing desktop: {error}");
                    match replacement_has_started(&sessions_dir, &hyprctl, &config) {
                        Some(true) => {
                            eprintln!(
                                "Attempting safety recovery from '{backup_name}' before giving up."
                            );
                            if report_safety_recovery(
                                &backup,
                                &hyprctl,
                                &process_info,
                                &config,
                                cli.verbose,
                            ) {
                                clear_recovery_marker(&sessions_dir);
                            }
                        }
                        Some(false) => {
                            eprintln!(
                                "Replacement did not start closing windows; leaving the desktop unchanged."
                            );
                            clear_recovery_marker(&sessions_dir);
                        }
                        None => {
                            eprintln!(
                                "Could not determine whether replacement started; leaving the recovery marker for manual recovery."
                            );
                        }
                    }
                    std::process::exit(1);
                }
            }
        }

        Commands::List => match list_sessions(&sessions_dir) {
            Ok(sessions) => {
                if sessions.is_empty() {
                    println!("No saved sessions.");
                } else {
                    println!("Saved sessions:");
                    for s in sessions {
                        let tag = if s.name.starts_with("autosave-") {
                            " [auto]"
                        } else {
                            ""
                        };
                        println!(
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
                eprintln!("Error listing sessions: {}", e);
                std::process::exit(1);
            }
        },

        Commands::Delete { name } => match delete_session(&name, &sessions_dir) {
            Ok(()) => println!("Deleted session '{}'", name),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },

        Commands::Config => {
            println!("Config path: {}", config_path().display());
            println!("Sessions dir: {}", sessions_dir.display());
            println!("Default session: {}", config.general.default_session);
            println!("Restore delay: {}ms", config.general.restore_delay_ms);
            println!(
                "Window detect timeout: {}ms",
                config.general.window_detect_timeout_ms
            );
            println!("Ignored classes: {:?}", config.filters.ignore_classes);
            if !config.apps.is_empty() {
                println!("App configs:");
                for (name, app) in &config.apps {
                    println!(
                        "  {}: binary={:?} capture_cwd={:?}",
                        name, app.binary, app.capture_cwd
                    );
                }
            }

            // Show Brave profile mappings
            println!();
            match hyprloom::brave::read_profiles() {
                Ok(profiles) if !profiles.is_empty() => {
                    println!("Brave profiles detected:");
                    let profile_ws = app_config_for(&config, "brave-browser", "")
                        .and_then(|c| c.profile_workspaces.as_ref());
                    for profile in &profiles {
                        if let Some(ws) = profile_ws.and_then(|m| m.get(&profile.directory)) {
                            println!("  ✓ {} ({}) → ws={}", profile.directory, profile.name, ws);
                        } else {
                            println!(
                                "  · {} ({}) — not mapped, will be skipped",
                                profile.directory, profile.name
                            );
                        }
                    }
                    if profile_ws.is_none() {
                        println!(
                            "  (no profile_workspaces configured — all profiles will be captured)"
                        );
                    }
                }
                Ok(_) => println!("No Brave profiles detected."),
                Err(e) => println!("Could not read Brave profiles: {e}"),
            }
        }

        Commands::Autosave {
            now,
            install,
            uninstall,
        } => {
            let flag_count = [now, install, uninstall].iter().filter(|&&f| f).count();
            if flag_count > 1 {
                eprintln!(
                    "Error: only one of --now, --install, --uninstall may be specified at a time."
                );
                std::process::exit(1);
            }

            let systemd_dir = hyprloom::autosave::systemd_user_dir();

            if install {
                match hyprloom::autosave::install(&systemd_dir) {
                    Ok((service_path, timer_path)) => {
                        println!("Created:");
                        println!("  {}", service_path.display());
                        println!("  {}", timer_path.display());
                        println!();
                        println!("To enable and start:");
                        println!("  systemctl --user enable --now hyprloom-autosave.timer");
                        println!();
                        println!("To check status:");
                        println!("  systemctl --user status hyprloom-autosave.timer");
                        println!("  journalctl --user -u hyprloom-autosave.service");
                    }
                    Err(e) => {
                        eprintln!("Error installing autosave timer: {}", e);
                        std::process::exit(1);
                    }
                }
            } else if uninstall {
                match hyprloom::autosave::uninstall(&systemd_dir) {
                    Ok(()) => println!("Autosave timer removed."),
                    Err(e) => {
                        eprintln!("Error uninstalling autosave timer: {}", e);
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
                            eprintln!("Error saving autosave session: {}", e);
                            std::process::exit(1);
                        }

                        let retain = config.general.autosave_retain;
                        let total_before =
                            match hyprloom::session::list_autosave_sessions(&sessions_dir) {
                                Ok(sessions) => sessions.len(),
                                Err(error) => {
                                    eprintln!("Error listing autosaves for rotation: {error}");
                                    std::process::exit(1);
                                }
                            };
                        let pruned =
                            match hyprloom::session::rotate_autosaves(&sessions_dir, retain) {
                                Ok(pruned) => pruned,
                                Err(error) => {
                                    eprintln!("Error rotating autosaves: {error}");
                                    std::process::exit(1);
                                }
                            };
                        let retained = total_before.saturating_sub(pruned);

                        println!(
                            "Autosaved '{}' ({} windows). Retained {}, pruned {}.",
                            name, client_count, retained, pruned
                        );
                    }
                    Err(e) => {
                        eprintln!("Error capturing session: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                // Status mode (no flags)
                let installed = hyprloom::autosave::is_installed(&systemd_dir);
                let active = hyprloom::autosave::is_active();

                if !installed {
                    println!("Autosave is not configured.");
                    println!("Run 'hyprloom autosave --install' to set up the systemd timer.");
                } else if !active {
                    println!("Autosave timer is installed but not active.");
                    println!("  systemctl --user enable --now hyprloom-autosave.timer");
                } else {
                    println!("Autosave is active (every 10min).");
                    match hyprloom::session::list_autosave_sessions(&sessions_dir) {
                        Ok(sessions) if !sessions.is_empty() => {
                            let latest = &sessions[0];
                            println!(
                                "Last: {} — {} windows",
                                latest.created_at.format("%Y-%m-%d %H:%M:%S"),
                                latest.client_count
                            );
                            println!(
                                "Retained: {} sessions (oldest: {})",
                                sessions.len(),
                                sessions.last().unwrap().name
                            );
                        }
                        Ok(_) => println!("No autosave sessions yet."),
                        Err(e) => println!("Could not list sessions: {e}"),
                    }
                    println!("To disable: hyprloom autosave --uninstall");
                }
            }
        }
    }
}

fn clear_recovery_marker(sessions_dir: &Path) -> bool {
    if let Err(error) = clear_replace_marker(sessions_dir) {
        eprintln!("Warning: could not clear replacement recovery marker: {error}");
        return false;
    }
    true
}

fn recover_pending_replace(sessions_dir: &Path, config: &hyprloom::config::Config) -> bool {
    let marker = match replace_marker(sessions_dir) {
        Ok(Some(marker)) => marker,
        Ok(None) => return true,
        Err(error) => {
            eprintln!("Warning: could not inspect replacement recovery marker: {error}");
            return false;
        }
    };
    if marker.phase == ReplacePhase::Committed {
        eprintln!(
            "Found a completed desktop replacement; finalizing its recovery marker without replaying the old snapshot."
        );
        return clear_recovery_marker(sessions_dir);
    }
    if marker.phase == ReplacePhase::Prepared {
        eprintln!(
            "Found a replacement that was prepared but never started; leaving the current desktop unchanged."
        );
        return clear_recovery_marker(sessions_dir);
    }
    if marker.phase == ReplacePhase::Closing {
        let Some(closing_address) = marker.closing_address.as_deref() else {
            eprintln!(
                "Warning: replacement marker is missing the window address for its closing phase."
            );
            return false;
        };
        let hyprctl = RealHyprctl;
        match replacement_close_started(closing_address, &hyprctl, config) {
            Some(false) => {
                eprintln!(
                    "Found a replacement that had not confirmed its first close; leaving the current desktop unchanged."
                );
                return clear_recovery_marker(sessions_dir);
            }
            None => {
                eprintln!(
                    "Warning: could not determine whether the first replacement close was applied; leaving the recovery marker for manual recovery."
                );
                return false;
            }
            Some(true) => {}
        }
    }
    let backup_name = marker.backup_name;
    eprintln!(
        "Found an interrupted desktop replacement; attempting recovery from '{backup_name}'."
    );
    let backup = match load_session(&backup_name, sessions_dir) {
        Ok(backup) => backup,
        Err(error) => {
            eprintln!("Warning: could not load replacement recovery snapshot: {error}");
            return false;
        }
    };
    let hyprctl = RealHyprctl;
    let process_info = RealProcessInfo;
    if report_safety_recovery(&backup, &hyprctl, &process_info, config, false) {
        return clear_recovery_marker(sessions_dir);
    }
    false
}

fn replacement_has_started(
    sessions_dir: &Path,
    hyprctl: &dyn HyprctlClient,
    config: &hyprloom::config::Config,
) -> Option<bool> {
    match replace_marker(sessions_dir) {
        Ok(Some(marker)) => match marker.phase {
            ReplacePhase::InProgress => Some(true),
            ReplacePhase::Closing => marker
                .closing_address
                .as_deref()
                .and_then(|address| replacement_close_started(address, hyprctl, config)),
            ReplacePhase::Prepared | ReplacePhase::Committed => Some(false),
        },
        Ok(None) => Some(false),
        Err(error) => {
            eprintln!("Warning: could not inspect replacement recovery marker: {error}");
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
fn replacement_close_started(
    address: &str,
    hyprctl: &dyn HyprctlClient,
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
                eprintln!(
                    "Warning: could not determine whether the first replacement close was applied: {error}"
                );
                return None;
            }
        };
        if !clients.iter().any(|client| client.address == address) {
            return Some(true);
        }
        if started.elapsed() >= timeout {
            return Some(false);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn command_requires_clean_recovery(command: &Commands) -> bool {
    match command {
        Commands::List | Commands::Config => false,
        Commands::Restore {
            dry_run, on_login, ..
        } => !dry_run && !on_login,
        Commands::Autosave {
            now: false,
            install: false,
            uninstall: false,
        } => false,
        _ => true,
    }
}

fn report_safety_recovery(
    backup: &hyprloom::session::Session,
    hyprctl: &RealHyprctl,
    process_info: &RealProcessInfo,
    config: &hyprloom::config::Config,
    verbose: bool,
) -> bool {
    let recovery_config = safety_recovery_config(config);
    match recover_session(
        backup,
        hyprctl,
        process_info,
        &recovery_config,
        false,
        verbose,
    ) {
        Ok(report) if report.failed == 0 && report.skipped == 0 => {
            eprintln!(
                "Safety recovery pass completed: {} unchanged, {} moved, {} launched, {} skipped.",
                report.unchanged, report.moved, report.launched, report.skipped
            );
            for detail in &report.details {
                eprintln!("  recovery: {detail}");
            }
            true
        }
        Ok(report) => {
            eprintln!(
                "Safety recovery was partial: {} unchanged, {} moved, {} launched, {} skipped, {} failed.",
                report.unchanged,
                report.moved,
                report.launched,
                report.skipped,
                report.failed
            );
            for detail in &report.details {
                eprintln!("  recovery: {detail}");
            }
            eprintln!("The safety backup remains available for another retry.");
            false
        }
        Err(error) => {
            eprintln!("Safety recovery could not run: {error}");
            eprintln!("The safety backup remains available for another retry.");
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
