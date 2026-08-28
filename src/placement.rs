//! Hyprland placement and dispatch command generation.
//!
//! The reconciliation engine decides which target belongs to which current
//! window.  This module owns the separate concern of translating that plan
//! into monitor, workspace, geometry, and state operations.

use crate::hyprctl::{HyprClient, HyprMonitor};
use crate::session::{Monitor, SessionClient};

pub fn find_monitor_by_name<'a>(monitors: &'a [HyprMonitor], name: &str) -> Option<&'a HyprMonitor> {
    if name.is_empty() {
        return None;
    }
    monitors.iter().find(|monitor| monitor.name.eq_ignore_ascii_case(name))
}

pub fn target_monitor_is_available(monitors: &[HyprMonitor], name: &str) -> bool {
    name.is_empty() || find_monitor_by_name(monitors, name).is_some()
}

/// Adapt captured absolute coordinates to a monitor whose origin or
/// resolution changed since the snapshot. Older sessions have `None` for
/// monitor origins and deliberately keep their original geometry.
pub fn adapt_client_geometry(target: &SessionClient, saved_monitors: &[Monitor], current_monitor: Option<&HyprMonitor>) -> SessionClient {
    let Some(current_monitor) = current_monitor else {
        return target.clone();
    };
    let Some(saved_monitor) = saved_monitors.iter().find(|monitor| monitor.name.eq_ignore_ascii_case(&target.monitor)) else {
        return target.clone();
    };
    let (Some(saved_x), Some(saved_y)) = (saved_monitor.x, saved_monitor.y) else {
        return target.clone();
    };
    let (Some(current_x), Some(current_y)) = (current_monitor.x, current_monitor.y) else {
        return target.clone();
    };
    if saved_monitor.width == 0 || saved_monitor.height == 0 || current_monitor.width == 0 || current_monitor.height == 0 {
        return target.clone();
    }

    let (relative_at, relative_size) = if supported_rotation(saved_monitor.transform) && supported_rotation(current_monitor.transform) {
        let saved_relative_at = [i64::from(target.at[0]) - i64::from(saved_x), i64::from(target.at[1]) - i64::from(saved_y)];
        let (canonical_at, canonical_size) = rotate_rect_to_canonical(
            saved_relative_at,
            target.size,
            saved_monitor.width,
            saved_monitor.height,
            saved_monitor.transform,
        );
        let scaled_at = [
            scale_coordinate(canonical_at[0], current_monitor.width, saved_monitor.width),
            scale_coordinate(canonical_at[1], current_monitor.height, saved_monitor.height),
        ];
        let scaled_size = [
            scaled_extent(canonical_size[0], current_monitor.width, saved_monitor.width, current_monitor.width),
            scaled_extent(canonical_size[1], current_monitor.height, saved_monitor.height, current_monitor.height),
        ];
        rotate_rect_from_canonical(
            scaled_at,
            scaled_size,
            current_monitor.width,
            current_monitor.height,
            current_monitor.transform,
        )
    } else {
        (
            [
                scale_coordinate(i64::from(target.at[0]) - i64::from(saved_x), current_monitor.width, saved_monitor.width),
                scale_coordinate(i64::from(target.at[1]) - i64::from(saved_y), current_monitor.height, saved_monitor.height),
            ],
            [
                scaled_extent(target.size[0], current_monitor.width, saved_monitor.width, current_monitor.width),
                scaled_extent(target.size[1], current_monitor.height, saved_monitor.height, current_monitor.height),
            ],
        )
    };
    let (relative_x, relative_y) = (relative_at[0], relative_at[1]);
    let width = relative_size[0];
    let height = relative_size[1];
    let (current_width, current_height) = displayed_monitor_dimensions(current_monitor);
    let proposed_x = i64::from(current_x) + relative_x;
    let proposed_y = i64::from(current_y) + relative_y;
    let at = [
        clamp_coordinate(proposed_x, current_x, current_width, width),
        clamp_coordinate(proposed_y, current_y, current_height, height),
    ];

    let mut adapted = target.clone();
    adapted.at = at;
    adapted.size = [width, height];
    adapted
}

const fn supported_rotation(transform: u32) -> bool {
    transform < 4
}

const fn displayed_monitor_dimensions(monitor: &HyprMonitor) -> (u32, u32) {
    if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    }
}

fn rotate_rect_to_canonical(at: [i64; 2], size: [i32; 2], base_width: u32, base_height: u32, transform: u32) -> ([i64; 2], [i32; 2]) {
    let x = at[0];
    let y = at[1];
    let width = i64::from(size[0].max(1));
    let height = i64::from(size[1].max(1));
    let base_width = i64::from(base_width);
    let base_height = i64::from(base_height);
    match transform {
        1 => ([y, base_width - x - width], [to_i32(height), to_i32(width)]),
        2 => ([base_width - x - width, base_height - y - height], [to_i32(width), to_i32(height)]),
        3 => ([base_height - y - height, x], [to_i32(height), to_i32(width)]),
        _ => ([x, y], [to_i32(width), to_i32(height)]),
    }
}

fn rotate_rect_from_canonical(at: [i64; 2], size: [i32; 2], base_width: u32, base_height: u32, transform: u32) -> ([i64; 2], [i32; 2]) {
    let x = at[0];
    let y = at[1];
    let width = i64::from(size[0].max(1));
    let height = i64::from(size[1].max(1));
    let base_width = i64::from(base_width);
    let base_height = i64::from(base_height);
    match transform {
        1 => ([base_height - y - height, x], [to_i32(height), to_i32(width)]),
        2 => ([base_width - x - width, base_height - y - height], [to_i32(width), to_i32(height)]),
        3 => ([y, base_width - x - width], [to_i32(height), to_i32(width)]),
        _ => ([x, y], [to_i32(width), to_i32(height)]),
    }
}

fn to_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or_else(|_| if value.is_negative() { i32::MIN } else { i32::MAX })
}

fn scale_coordinate(value: i64, numerator: u32, denominator: u32) -> i64 {
    round_ratio(value, i128::from(numerator), i128::from(denominator))
}

fn scaled_extent(value: i32, numerator: u32, denominator: u32, monitor_extent: u32) -> i32 {
    let scaled = round_ratio(i64::from(value.max(1)), i128::from(numerator), i128::from(denominator));
    let maximum = i64::from(monitor_extent_for_i32(monitor_extent));
    to_i32(scaled.clamp(1, maximum))
}

fn round_ratio(value: i64, numerator: i128, denominator: i128) -> i64 {
    if denominator == 0 {
        return value;
    }
    let product = i128::from(value) * numerator;
    let adjustment = denominator / 2;
    let rounded = if product.is_negative() {
        (product - adjustment) / denominator
    } else {
        (product + adjustment) / denominator
    };
    i64::try_from(rounded).unwrap_or_else(|_| if rounded.is_negative() { i64::MIN } else { i64::MAX })
}

fn monitor_extent_for_i32(extent: u32) -> u32 {
    extent.min(u32::try_from(i32::MAX).unwrap_or(u32::MAX))
}

fn clamp_coordinate(coordinate: i64, origin: i32, monitor_extent: u32, window_extent: i32) -> i32 {
    let origin = i64::from(origin);
    let monitor_end = origin + i64::from(monitor_extent_for_i32(monitor_extent));
    let minimum = origin;
    let maximum = (monitor_end - i64::from(window_extent)).max(minimum);
    to_i32(coordinate.clamp(minimum, maximum))
}

pub fn workspace_matches(target: &SessionClient, current: &HyprClient) -> bool {
    let saved_name = target.workspace_name.trim();
    if saved_name.is_empty() || saved_name.parse::<i32>().is_ok() {
        return target.workspace == current.workspace.id;
    }
    workspace_names_match(saved_name, current.workspace.name.trim())
}

fn workspace_names_match(saved_name: &str, current_name: &str) -> bool {
    if has_workspace_prefix(saved_name, "special:") || has_workspace_prefix(current_name, "special:") {
        return saved_name.eq_ignore_ascii_case(current_name);
    }
    strip_workspace_prefix(saved_name, "name:").eq_ignore_ascii_case(strip_workspace_prefix(current_name, "name:"))
}

pub fn workspace_selector(target: &SessionClient) -> String {
    let saved_name = target.workspace_name.trim();
    if saved_name.is_empty() || saved_name.parse::<i32>().is_ok() {
        target.workspace.to_string()
    } else if has_workspace_prefix(saved_name, "special:") || has_workspace_prefix(saved_name, "name:") {
        saved_name.to_string()
    } else {
        format!("name:{saved_name}")
    }
}

fn has_workspace_prefix(value: &str, prefix: &str) -> bool {
    value.get(..prefix.len()).is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn strip_workspace_prefix<'a>(value: &'a str, prefix: &str) -> &'a str {
    if has_workspace_prefix(value, prefix) {
        &value[prefix.len()..]
    } else {
        value
    }
}

pub fn quote_dispatch_token(value: &str) -> String {
    if value
        .chars()
        .all(|character| !character.is_whitespace() && character != '\'' && character != '"' && character != '\\')
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "'\\''"))
}

pub fn monitor_move_commands(monitor: &str, address: &str) -> Vec<String> {
    // Hyprland's monitor-aware move dispatcher operates on the active window,
    // while address selectors are supported by focuswindow. Focus the exact
    // matched client first, then move it silently to the named monitor.
    vec![
        format!("focuswindow address:{address}"),
        format!("movewindow {} silent", quote_dispatch_token(&format!("mon:{monitor}"))),
    ]
}

/// Return only the compositor operations needed to make an existing window
/// agree with the saved placement.
///
/// An empty result is the important fast path:
/// it means the window is already correct and should be left alone.
#[must_use]
pub fn build_reconcile_dispatch_commands(target: &SessionClient, current: &HyprClient, current_monitor: Option<&str>) -> Vec<String> {
    build_reconcile_dispatch_commands_with_geometry(target, current, current_monitor, ReconcileGeometry::new(target.at, target.size, true))
}

/// Geometry inputs for a reconciliation dispatch plan.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileGeometry {
    desired_at: [i32; 2],
    desired_size: [i32; 2],
    target_monitor_available: bool,
}

impl ReconcileGeometry {
    pub const fn new(desired_at: [i32; 2], desired_size: [i32; 2], target_monitor_available: bool) -> Self {
        Self {
            desired_at,
            desired_size,
            target_monitor_available,
        }
    }
}

pub fn build_reconcile_dispatch_commands_with_geometry(
    target: &SessionClient,
    current: &HyprClient,
    current_monitor: Option<&str>,
    geometry: ReconcileGeometry,
) -> Vec<String> {
    let ReconcileGeometry {
        desired_at,
        desired_size,
        target_monitor_available,
    } = geometry;
    let monitor_mismatch = target_monitor_available
        && !target.monitor.is_empty()
        && current_monitor.is_some_and(|monitor| !monitor.eq_ignore_ascii_case(&target.monitor));
    let workspace_mismatch = !workspace_matches(target, current);
    let leaving_fullscreen = current.fullscreen > 0 && target.fullscreen == 0;
    let entering_or_changing_fullscreen = target.fullscreen > 0 && current.fullscreen != target.fullscreen;

    let mut commands = Vec::new();
    if current.pinned && !target.pinned {
        commands.push(format!("pin address:{}", current.address));
    }
    if leaving_fullscreen {
        commands.push(format!("focuswindow address:{}", current.address));
        commands.push("fullscreenstate 0 0".to_string());
    }
    if workspace_mismatch {
        commands.push(format!(
            "movetoworkspacesilent {},address:{}",
            quote_dispatch_token(&workspace_selector(target)),
            current.address
        ));
    }
    // Workspace bindings can move a window to their preferred monitor. Apply
    // the explicit monitor correction afterwards so the final placement wins.
    if monitor_mismatch {
        commands.extend(monitor_move_commands(&target.monitor, &current.address));
    }
    if current.floating != target.floating {
        commands.push(format!("togglefloating address:{}", current.address));
    }

    // Do not apply stale absolute coordinates when the saved monitor is gone.
    if target.fullscreen == 0 && (target.monitor.is_empty() || target_monitor_available) {
        if current.size != desired_size {
            commands.push(format!(
                "resizewindowpixel exact {} {},address:{}",
                desired_size[0], desired_size[1], current.address
            ));
        }
        if current.at != desired_at {
            commands.push(format!(
                "movewindowpixel exact {} {},address:{}",
                desired_at[0], desired_at[1], current.address
            ));
        }
    }

    if entering_or_changing_fullscreen {
        commands.push(format!("focuswindow address:{}", current.address));
        commands.push(format!("fullscreenstate {} {}", target.fullscreen, target.fullscreen));
    }

    if !current.pinned && target.pinned {
        commands.push(format!("pin address:{}", current.address));
    }

    commands
}
