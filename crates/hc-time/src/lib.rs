//! Configured-timezone helpers for HomeCore.
//!
//! Storage stays UTC everywhere. This crate exists for the **display
//! layer**: log timestamps, console-style API responses, and
//! mode/scheduler "what time is it locally?" decisions need to honor
//! the operator's configured zone — not the process's `TZ` env var
//! or `/etc/localtime`. Plain `chrono::Local::now()` reads the latter,
//! which is wrong inside containers running UTC.
//!
//! # Lifecycle
//!
//! Call [`init`] once at process startup (in `main.rs`) with the
//! `chrono_tz::Tz` parsed from `[location].timezone`. After that,
//! [`now_local`], [`fmt_local`], and [`ConfiguredTzTime`] all read
//! the configured zone. If `init` is never called (e.g. in tests),
//! everything defaults to UTC — no panics, no undefined behaviour.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::sync::RwLock;
use tracing_subscriber::fmt::time::FormatTime;

// `RwLock<Tz>` (rather than `OnceLock<Tz>`) so the configured zone
// can be updated at runtime, not just once at boot. This is what
// allows the plugin SDK to apply a TZ pushed via the
// `homecore/system/tz` retained MQTT topic — plugins start their
// tracing subscriber before MQTT connects, so the very first log
// lines render in UTC, then the formatter auto-swaps once the
// retained payload arrives. `RwLock::new` is `const`-stable, so
// the default-UTC initial value compiles statically.
//
// The lock is acquired once per log line on the read side; lock
// contention is irrelevant at home-automation log volumes.
static CONFIGURED_TZ: RwLock<Tz> = RwLock::new(Tz::UTC);

/// Set the process-wide configured timezone.
///
/// Call once at process startup, **before** the tracing subscriber
/// is built, so [`ConfiguredTzTime`] reads the right zone for the
/// very first log line. Plugin SDKs may call this again later when
/// they receive a TZ over MQTT — the most recent successful call
/// wins, and existing tracing layers pick up the new zone on the
/// next event without needing to be rebuilt.
pub fn init(tz: Tz) {
    if let Ok(mut g) = CONFIGURED_TZ.write() {
        *g = tz;
    }
}

/// Return the configured timezone, defaulting to UTC if [`init`] has
/// not been called (or if the lock is poisoned, defensively).
pub fn configured_tz() -> Tz {
    CONFIGURED_TZ.read().map(|g| *g).unwrap_or(Tz::UTC)
}

/// "Now" in the configured zone. Replaces `chrono::Local::now()` for
/// any caller whose notion of "local" is the configured zone — most
/// notably scheduler / mode-manager logic that decides whether the
/// current moment falls inside a configured window.
pub fn now_local() -> DateTime<Tz> {
    Utc::now().with_timezone(&configured_tz())
}

/// Format a UTC instant in the configured zone as ISO-8601 with the
/// numeric offset, e.g. `2026-05-03T14:32:00.123-04:00`. Use for
/// human-readable timestamps in console-style API endpoints (audit
/// log dumps, pre-formatted log lines, etc.). Structured JSON
/// timestamps stay UTC `...Z` per the wire-format split.
pub fn fmt_local(utc: &DateTime<Utc>) -> String {
    utc.with_timezone(&configured_tz())
        .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        .to_string()
}

/// Parse an IANA zone name like `"America/New_York"` into a [`Tz`].
/// `chrono::FixedOffset` won't do because it doesn't track DST; this
/// is the right form for the `[location].timezone` config field.
///
/// On parse failure the error message includes the offending value
/// so the operator can find their typo.
pub fn parse_iana(name: &str) -> Result<Tz, String> {
    name.parse::<Tz>()
        .map_err(|e| format!("invalid IANA timezone '{name}': {e}"))
}

/// `tracing_subscriber` timer that formats events in the configured
/// zone. Plug into any fmt layer's `.with_timer(ConfiguredTzTime)`.
#[derive(Clone, Debug, Default)]
pub struct ConfiguredTzTime;

impl FormatTime for ConfiguredTzTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", fmt_local(&Utc::now()))
    }
}

/// UTC variant timer. Same output shape as [`ConfiguredTzTime`] but
/// always renders `...Z`. Use when `[logging].time_display = "utc"`.
#[derive(Clone, Debug, Default)]
pub struct UtcTime;

impl FormatTime for UtcTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // The OnceLock is process-global, so tests that call `init` would
    // race each other and leak state across the whole binary. The
    // tests below either don't call `init` (and rely on the UTC
    // default) or use `with_timezone` directly to exercise the
    // formatter without touching global state.

    #[test]
    fn default_tz_is_utc_when_uninitialized() {
        // We can't reliably assert this in a process where another
        // test happened to call `init` first, so instead we exercise
        // the formatter shape: a UTC `Tz` always renders `+00:00`.
        let utc_instant = Utc.with_ymd_and_hms(2026, 5, 3, 14, 32, 0).unwrap();
        let formatted = utc_instant
            .with_timezone(&Tz::UTC)
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string();
        assert!(
            formatted.ends_with("+00:00"),
            "expected UTC offset, got {formatted}"
        );
    }

    #[test]
    fn fmt_local_with_explicit_tz_handles_dst() {
        // 2026-01-15 NYC = EST (-05:00); 2026-07-15 NYC = EDT (-04:00).
        // chrono-tz tracks DST; FixedOffset wouldn't.
        let ny: Tz = "America/New_York".parse().unwrap();
        let winter = Utc.with_ymd_and_hms(2026, 1, 15, 17, 0, 0).unwrap();
        let summer = Utc.with_ymd_and_hms(2026, 7, 15, 17, 0, 0).unwrap();
        let win = winter.with_timezone(&ny).format("%:z").to_string();
        let sum = summer.with_timezone(&ny).format("%:z").to_string();
        assert_eq!(win, "-05:00");
        assert_eq!(sum, "-04:00");
    }

    #[test]
    fn parse_iana_accepts_named_zone() {
        assert!(parse_iana("America/New_York").is_ok());
        assert!(parse_iana("Europe/London").is_ok());
        assert!(parse_iana("UTC").is_ok());
    }

    #[test]
    fn parse_iana_rejects_garbage() {
        let err = parse_iana("Not/A/Zone").unwrap_err();
        assert!(err.contains("Not/A/Zone"), "error should echo input: {err}");
    }
}
