//! Profile directory loader.
//!
//! Reads all `*.toml` files from a directory and deserializes them into
//! `EcosystemProfile` values. Files that fail to parse are logged and skipped.
//! The `device-types.toml` file is handled separately by `DeviceTypeRegistry`.

use crate::profile::{EcosystemProfile, ProfileFile};
use anyhow::{Context, Result};
use tracing::{info, warn};

/// Files in the profiles folder that aren't profiles: the device type catalog,
/// and the logins devices use to reach the broker.
pub const NOT_PROFILES: &[&str] = &["device-types.toml", "logins.toml"];

/// Load a single profile from a TOML string (used in tests and for inline profiles).
pub fn load_profile_str(src: &str) -> Result<EcosystemProfile> {
    let file: ProfileFile = toml::from_str(src).context("Failed to parse ecosystem profile")?;
    Ok(file.ecosystem)
}

/// Load all `*.toml` ecosystem profiles from `dir`.
/// `device-types.toml` is skipped (handled separately).
/// Files that cannot be parsed are logged and skipped without aborting.
pub fn load_profiles_from_dir(dir: &str) -> Result<Vec<EcosystemProfile>> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("Cannot read profiles directory: {dir}"))?;

    let mut profiles = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "Skipping unreadable dir entry");
                continue;
            }
        };

        let path = entry.path();

        // Only process .toml files.
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // The catalog and the device logins live here too; they aren't profiles.
        if NOT_PROFILES.contains(&filename) {
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(file = %path.display(), error = %e, "Cannot read profile file; skipping");
                continue;
            }
        };

        match load_profile_str(&text) {
            Ok(profile) => {
                info!(name = %profile.name, file = %path.display(), "Loaded ecosystem profile");
                profiles.push(profile);
            }
            Err(e) => {
                warn!(file = %path.display(), error = %e, "Failed to parse profile; skipping");
            }
        }
    }

    Ok(profiles)
}

/// Read every `*.rhai` transform script in `dir`, as `(file name, source)`
/// sorted by file name, for [`crate::EcosystemRouter::with_scripts`].
///
/// Sorted so that when two scripts define the same function, which one wins
/// does not depend on the order the directory happens to list them in.
/// A missing directory has no scripts; an unreadable file is logged and skipped.
pub fn load_transforms_from_dir(dir: &str) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rhai"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            match std::fs::read_to_string(&path) {
                Ok(src) => Some((name, src)),
                Err(e) => {
                    warn!(file = %path.display(), error = %e, "Cannot read Rhai script; skipping");
                    None
                }
            }
        })
        .collect()
}

/// One profile file in a directory, read: its text, and the profile or why
/// it didn't load.
#[derive(Debug)]
pub struct ProfileRead {
    /// The file's name, e.g. `tasmota.toml`.
    pub file: String,
    pub text: String,
    /// The profile, or what is wrong with the file, said as TOML says it
    /// (with the line and column).
    pub result: std::result::Result<EcosystemProfile, String>,
}

/// Every `*.toml` profile file in `dir` except `device-types.toml`, sorted by
/// name, each with the profile or the reason it didn't load. Unlike
/// [`load_profiles_from_dir`], a file that doesn't parse is kept, with its
/// error, so the person can be shown what is wrong.
pub fn read_profiles_from_dir(dir: &str) -> Result<Vec<ProfileRead>> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("Cannot read profiles directory: {dir}"))?;
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !NOT_PROFILES.contains(&n))
        })
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let result = toml::from_str::<ProfileFile>(&text)
                        .map(|f| f.ecosystem)
                        .map_err(|e| e.to_string());
                    ProfileRead { file, text, result }
                }
                Err(e) => ProfileRead {
                    file,
                    text: String::new(),
                    result: Err(format!("can't be read: {e}")),
                },
            }
        })
        .collect())
}
