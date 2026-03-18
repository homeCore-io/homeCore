//! Profile directory loader.
//!
//! Reads all `*.toml` files from a directory and deserializes them into
//! `EcosystemProfile` values. Files that fail to parse are logged and skipped.
//! The `device-types.toml` file is handled separately by `DeviceTypeRegistry`.

use crate::profile::{EcosystemProfile, ProfileFile};
use anyhow::{Context, Result};
use tracing::{info, warn};

/// Load a single profile from a TOML string (used in tests and for inline profiles).
pub fn load_profile_str(src: &str) -> Result<EcosystemProfile> {
    let file: ProfileFile = toml::from_str(src).context("Failed to parse ecosystem profile")?;
    Ok(file.ecosystem)
}

/// Load all `*.toml` ecosystem profiles from `dir`.
/// `device-types.toml` is skipped (handled separately).
/// Files that cannot be parsed are logged and skipped without aborting.
pub fn load_profiles_from_dir(dir: &str) -> Result<Vec<EcosystemProfile>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Cannot read profiles directory: {dir}"))?;

    let mut profiles = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => { warn!(error = %e, "Skipping unreadable dir entry"); continue; }
        };

        let path = entry.path();

        // Only process .toml files.
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // device-types.toml is loaded separately.
        if filename == "device-types.toml" {
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
