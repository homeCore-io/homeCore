use tracing_subscriber::EnvFilter;

use crate::config::LoggingConfig;

/// Per-target log-level defaults applied before user config.
///
/// These quiet down noisy third-party crates that emit useful info-level
/// events but spam DEBUG. Operators can override any of these via
/// `[logging.targets]` in `homecore.toml` or via `RUST_LOG`, both of which
/// are layered on top.
///
/// Tune carefully — entries here change the "default install" experience
/// and should target genuine noise (per-packet keepalive logs, etc.) not
/// debug events that would actually help an operator diagnose a problem.
pub const NOISE_SUPPRESSION_DEFAULTS: &[(&str, &str)] = &[
    // Pingreq + state-machine bookkeeping — fires every keep-alive
    // (default 30s) on every MQTT client. With many plugins this is the
    // dominant log volume and tells you nothing actionable.
    ("rumqttc::state", "info"),
    // Embedded broker has the same per-packet chatter at DEBUG.
    ("rumqttd", "info"),
];

/// Return the noise-suppression defaults as a CSV directive string,
/// suitable for prepending to a user-supplied filter spec.
///
/// Plugins (which build their `EnvFilter` directly rather than going
/// through [`build_filter`]) call [`with_noise_suppression`] to apply
/// the same defaults the core uses.
pub fn noise_suppression_directives() -> String {
    NOISE_SUPPRESSION_DEFAULTS
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Prepend the noise-suppression defaults to `directives`. The user's
/// directives appear after the defaults so EnvFilter resolves them with
/// higher precedence on conflict.
pub fn with_noise_suppression(directives: &str) -> String {
    let suppression = noise_suppression_directives();
    if directives.is_empty() {
        suppression
    } else {
        format!("{suppression},{directives}")
    }
}

/// Build the filter directive string from config (without parsing).
/// Used to seed the reload handle's initial value.
pub fn build_filter_string(config: &LoggingConfig, level_override: Option<&str>) -> String {
    let base = level_override.unwrap_or(&config.level);
    let mut parts = vec![base.to_string()];
    // Noise-suppression defaults first, so config overrides win.
    for (target, level) in NOISE_SUPPRESSION_DEFAULTS {
        parts.push(format!("{target}={level}"));
    }
    for (target, level) in &config.targets {
        parts.push(format!("{target}={level}"));
    }
    if level_override.is_none() {
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            for part in rust_log.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    parts.push(part.to_string());
                }
            }
        }
    }
    parts.join(",")
}

/// Build an `EnvFilter` from the `[logging]` config section.
///
/// Precedence (lowest → highest):
///   1. `level` — global default
///   2. `targets` map — per-crate overrides from config
///   3. `RUST_LOG` env var — runtime override (only applied when `level_override`
///      is `None`, i.e. for stderr/file layers; syslog uses its own level)
pub fn build_filter(config: &LoggingConfig, level_override: Option<&str>) -> EnvFilter {
    let base = level_override.unwrap_or(&config.level);
    let mut filter = EnvFilter::new(base);

    // Noise-suppression defaults first, so config + RUST_LOG win on conflict.
    for (target, level) in NOISE_SUPPRESSION_DEFAULTS {
        let directive = format!("{target}={level}");
        if let Ok(d) = directive.parse() {
            filter = filter.add_directive(d);
        }
    }

    for (target, level) in &config.targets {
        let directive = format!("{target}={level}");
        if let Ok(d) = directive.parse() {
            filter = filter.add_directive(d);
        }
    }

    // RUST_LOG takes highest precedence for the primary outputs (not syslog
    // which has its own level override independent of env).
    if level_override.is_none() {
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            for part in rust_log.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    if let Ok(d) = part.parse() {
                        filter = filter.add_directive(d);
                    }
                }
            }
        }
    }

    filter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_targets(targets: &[(&str, &str)]) -> LoggingConfig {
        let mut c = LoggingConfig::default();
        c.level = "info".into();
        c.targets = targets
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        c
    }

    #[test]
    fn noise_defaults_in_directive_string() {
        let s = build_filter_string(&cfg_with_targets(&[]), None);
        assert!(
            s.contains("rumqttc::state=info"),
            "expected rumqttc::state default, got: {s}"
        );
        assert!(
            s.contains("rumqttd=info"),
            "expected rumqttd default, got: {s}"
        );
    }

    #[test]
    fn with_noise_suppression_prepends() {
        let combined = with_noise_suppression("hc_thermostat=info");
        assert!(combined.contains("rumqttc::state=info"));
        assert!(combined.contains("rumqttd=info"));
        // User directive after the defaults so it wins on conflict.
        let user_idx = combined.find("hc_thermostat=info").unwrap();
        let suppress_idx = combined.find("rumqttc::state=info").unwrap();
        assert!(user_idx > suppress_idx);
    }

    #[test]
    fn with_noise_suppression_handles_empty() {
        let combined = with_noise_suppression("");
        assert_eq!(combined, noise_suppression_directives());
    }

    #[test]
    fn config_target_overrides_default() {
        // Operator opts in to debug for one of the noisy crates — last
        // matching directive wins in EnvFilter.
        let s = build_filter_string(&cfg_with_targets(&[("rumqttc::state", "debug")]), None);
        // The default appears first, then the override appears after.
        let default_idx = s.find("rumqttc::state=info").expect("default present");
        let override_idx = s.find("rumqttc::state=debug").expect("override present");
        assert!(
            override_idx > default_idx,
            "config override must appear after the default so it wins"
        );
    }
}
