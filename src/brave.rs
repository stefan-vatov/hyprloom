use crate::session::BraveProfile;
use serde_json::Value;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::PathBuf;

/// Errors returned while reading Brave's profile inventory.
#[derive(Debug, thiserror::Error)]
pub enum BraveError {
    #[error("IO error: {0}")]
    /// The Local State file could not be read.
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    /// The Local State file was not valid JSON.
    Json(#[from] serde_json::Error),
}

/// Default path to Brave's Local State file on Linux.
#[must_use]
pub fn local_state_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("BraveSoftware/Brave-Browser/Local State")
}

/// Read and parse profiles from the Local State file.
///
/// # Errors
///
/// Returns an error when the Local State file exists but cannot be read or
/// parsed as JSON.
pub fn read_profiles() -> Result<Vec<BraveProfile>, BraveError> {
    let path = local_state_path();
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(path)?;
    Ok(parse_profiles_from_local_state(&content)?)
}

/// Parse profile info from Local State JSON content.
///
/// # Errors
///
/// Returns the JSON parser error when `json_str` is malformed.
pub fn parse_profiles_from_local_state(json_str: &str) -> Result<Vec<BraveProfile>, serde_json::Error> {
    let value: Value = serde_json::from_str(json_str)?;
    let profiles = value
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
        .map(|cache| {
            cache
                .iter()
                .map(|(dir, info)| BraveProfile {
                    directory: dir.clone(),
                    name: info
                        .get("name")
                        .and_then(|n| n.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(dir)
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(profiles)
}

/// Filter profiles to only include those with a workspace mapping.
///
/// When `profile_workspaces` is `None`, the inventory is returned unchanged;
/// callers that do not have an explicit mapping should apply
/// [`filter_profiles_by_active_directories`] instead of restoring every
/// profile listed in Local State.
#[must_use]
pub fn filter_profiles_by_config<S: BuildHasher>(
    profiles: Vec<BraveProfile>,
    profile_workspaces: Option<&HashMap<String, i32, S>>,
) -> Vec<BraveProfile> {
    match profile_workspaces {
        Some(mappings) => profiles.into_iter().filter(|p| mappings.contains_key(&p.directory)).collect(),
        None => profiles,
    }
}

/// Keep only profiles that were positively observed in the current session.
///
/// Local State contains profiles that may never have an open window, so using
/// the complete inventory as a restore target would unexpectedly launch all
/// of them when no workspace mapping was configured.
#[must_use]
pub fn filter_profiles_by_active_directories(profiles: Vec<BraveProfile>, active_directories: &[String]) -> Vec<BraveProfile> {
    profiles
        .into_iter()
        .filter(|profile| {
            active_directories
                .iter()
                .any(|directory| directory.eq_ignore_ascii_case(&profile.directory))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_state_profiles() {
        let json = r#"{
            "profile": {
                "info_cache": {
                    "Default": {"name": "Credifit"},
                    "Profile 1": {"name": "LinkPJ"},
                    "Profile 2": {"name": "ABRH Bahia"}
                }
            }
        }"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert_eq!(profiles.len(), 3);
        assert!(profiles.iter().any(|p| p.directory == "Default" && p.name == "Credifit"));
        assert!(profiles.iter().any(|p| p.directory == "Profile 1" && p.name == "LinkPJ"));
    }

    #[test]
    fn test_parse_local_state_empty() {
        let json = r#"{"profile": {"info_cache": {}}}"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn test_parse_local_state_empty_name_falls_back_to_directory() {
        let json = r#"{"profile": {"info_cache": {"Default": {"name": ""}}}}"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Default", "empty name should fall back to directory");
    }

    #[test]
    fn test_parse_local_state_missing_field() {
        let json = r#"{"other": "data"}"#;
        let profiles = parse_profiles_from_local_state(json).unwrap();
        assert!(profiles.is_empty());
    }

    #[test]
    fn test_filter_profiles_by_config_with_mappings() {
        let profiles = vec![
            BraveProfile {
                directory: "Default".to_string(),
                name: "Credifit".to_string(),
            },
            BraveProfile {
                directory: "Profile 1".to_string(),
                name: "LinkPJ".to_string(),
            },
            BraveProfile {
                directory: "Profile 2".to_string(),
                name: "ABRH".to_string(),
            },
        ];
        let mappings = HashMap::from([("Default".to_string(), 1), ("Profile 1".to_string(), 6)]);
        let filtered = filter_profiles_by_config(profiles, Some(&mappings));
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|p| p.directory == "Default"));
        assert!(filtered.iter().any(|p| p.directory == "Profile 1"));
        assert!(!filtered.iter().any(|p| p.directory == "Profile 2"));
    }

    #[test]
    fn test_filter_profiles_by_config_without_mappings_keeps_all() {
        let profiles = vec![
            BraveProfile {
                directory: "Default".to_string(),
                name: "Credifit".to_string(),
            },
            BraveProfile {
                directory: "Profile 1".to_string(),
                name: "LinkPJ".to_string(),
            },
        ];
        let filtered = filter_profiles_by_config::<std::collections::hash_map::RandomState>(profiles, None);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_profiles_by_active_directories_does_not_restore_closed_profiles() {
        let profiles = vec![
            BraveProfile {
                directory: "Default".to_string(),
                name: "Credifit".to_string(),
            },
            BraveProfile {
                directory: "Profile 1".to_string(),
                name: "LinkPJ".to_string(),
            },
        ];
        let active = vec!["default".to_string()];

        let filtered = filter_profiles_by_active_directories(profiles, &active);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].directory, "Default");
    }
}
