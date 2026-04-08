use tracing_subscriber::EnvFilter;

use crate::config::LoggingConfig;

/// Build the filter directive string from config (without parsing).
/// Used to seed the reload handle's initial value.
pub fn build_filter_string(config: &LoggingConfig, level_override: Option<&str>) -> String {
    let base = level_override.unwrap_or(&config.level);
    let mut parts = vec![base.to_string()];
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
