//! Device-id inclusion filter.
//!
//! Supports a tiny glob syntax sufficient for "match by device-id":
//!   - `*`              → match anything
//!   - `prefix.*`       → starts-with match (including the trailing `.`)
//!   - `*.suffix`       → ends-with match
//!   - `prefix.*.mid`   → both ends fixed, anything in the middle
//!   - exact strings    → equality
//!
//! Anything more complex (regex, character classes) is out of scope —
//! device IDs are dotted-namespace strings and these patterns cover
//! the realistic cases.

#[derive(Debug, Clone)]
pub struct DeviceFilter {
    patterns: Vec<Pattern>,
}

#[derive(Debug, Clone)]
enum Pattern {
    /// Matches any device id.
    Any,
    /// Exact match.
    Exact(String),
    /// `prefix*` — must start with this (literal, no inner wildcard).
    StartsWith(String),
    /// `*suffix` — must end with this.
    EndsWith(String),
    /// `prefix*suffix` — must start with prefix AND end with suffix.
    StartsAndEnds(String, String),
}

impl DeviceFilter {
    /// Build from raw config strings. Empty list → matches nothing.
    pub fn from_patterns<S: AsRef<str>>(raw: &[S]) -> Self {
        let patterns = raw
            .iter()
            .map(|s| Pattern::compile(s.as_ref()))
            .collect::<Vec<_>>();
        Self { patterns }
    }

    /// Empty filter — matches nothing. Used as the default.
    pub fn empty() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    pub fn matches(&self, device_id: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(device_id))
    }
}

impl Pattern {
    fn compile(raw: &str) -> Self {
        if raw == "*" {
            return Pattern::Any;
        }
        match raw.split_once('*') {
            None => Pattern::Exact(raw.to_string()),
            Some(("", "")) => Pattern::Any,
            Some((prefix, "")) => Pattern::StartsWith(prefix.to_string()),
            Some(("", suffix)) => Pattern::EndsWith(suffix.to_string()),
            Some((prefix, suffix)) => {
                Pattern::StartsAndEnds(prefix.to_string(), suffix.to_string())
            }
        }
    }

    fn matches(&self, s: &str) -> bool {
        match self {
            Pattern::Any => true,
            Pattern::Exact(want) => s == want,
            Pattern::StartsWith(p) => s.starts_with(p),
            Pattern::EndsWith(p) => s.ends_with(p),
            Pattern::StartsAndEnds(p, q) => {
                s.starts_with(p) && s.ends_with(q) && s.len() >= p.len() + q.len()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_anything() {
        let f = DeviceFilter::from_patterns(&["*"]);
        assert!(f.matches("anything"));
        assert!(f.matches("light.kitchen"));
        assert!(f.matches(""));
    }

    #[test]
    fn empty_filter_matches_nothing() {
        let f = DeviceFilter::empty();
        assert!(!f.matches("light.kitchen"));
    }

    #[test]
    fn exact_match() {
        let f = DeviceFilter::from_patterns(&["light.kitchen"]);
        assert!(f.matches("light.kitchen"));
        assert!(!f.matches("light.kitchen_main"));
        assert!(!f.matches("light.living"));
    }

    #[test]
    fn prefix_glob() {
        let f = DeviceFilter::from_patterns(&["sensor.*"]);
        assert!(f.matches("sensor.temp"));
        assert!(f.matches("sensor."));
        assert!(!f.matches("light.kitchen"));
    }

    #[test]
    fn suffix_glob() {
        let f = DeviceFilter::from_patterns(&["*_battery"]);
        assert!(f.matches("kitchen_battery"));
        assert!(!f.matches("battery_kitchen"));
    }

    #[test]
    fn middle_glob() {
        let f = DeviceFilter::from_patterns(&["sensor.*.temp"]);
        assert!(f.matches("sensor.kitchen.temp"));
        assert!(f.matches("sensor..temp"));
        assert!(!f.matches("sensor.kitchen.humidity"));
    }

    #[test]
    fn multiple_patterns_or() {
        let f = DeviceFilter::from_patterns(&["light.*", "sensor.*"]);
        assert!(f.matches("light.kitchen"));
        assert!(f.matches("sensor.outdoor"));
        assert!(!f.matches("thermostat.upstairs"));
    }
}
