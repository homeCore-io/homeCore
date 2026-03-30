//! MQTT topic pattern matching and template rendering.
//!
//! Patterns use `{var}` to capture single-segment variables (whole segment)
//! or `prefix_{var}` / `{var}_suffix` for partial-segment captures.
//! Captured variables are available for template rendering.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Segment types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Segment {
    /// Literal segment — must match exactly.
    Literal(String),
    /// Whole-segment capture: `{name}`.
    Capture(String),
    /// Partial capture with optional literal prefix and/or suffix: `shelly_{device}`.
    PartialCapture {
        prefix: String,
        name: String,
        suffix: String,
    },
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse an MQTT topic pattern into segments.
/// Supports `{var}`, `prefix_{var}`, `{var}_suffix`, and `prefix_{var}_suffix`.
pub fn parse_pattern(pattern: &str) -> Vec<Segment> {
    pattern
        .split('/')
        .map(|seg| {
            if let (Some(open), Some(close)) = (seg.find('{'), seg.find('}')) {
                if open < close {
                    let prefix = seg[..open].to_string();
                    let name = seg[open + 1..close].to_string();
                    let suffix = seg[close + 1..].to_string();
                    if prefix.is_empty() && suffix.is_empty() {
                        return Segment::Capture(name);
                    } else {
                        return Segment::PartialCapture {
                            prefix,
                            name,
                            suffix,
                        };
                    }
                }
            }
            Segment::Literal(seg.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Attempt to match `topic` against `segments`.
/// Returns captured variables on success, `None` if the topic does not match.
pub fn match_segments(segments: &[Segment], topic: &str) -> Option<HashMap<String, String>> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut vars = HashMap::new();
    for (seg, part) in segments.iter().zip(parts.iter()) {
        match seg {
            Segment::Literal(lit) if lit == part => {}
            Segment::Literal(_) => return None,
            Segment::Capture(name) => {
                vars.insert(name.clone(), (*part).to_string());
            }
            Segment::PartialCapture {
                prefix,
                name,
                suffix,
            } => {
                if part.starts_with(prefix.as_str()) && part.ends_with(suffix.as_str()) {
                    let inner_end = part.len() - suffix.len();
                    let value = &part[prefix.len()..inner_end];
                    vars.insert(name.clone(), value.to_string());
                } else {
                    return None;
                }
            }
        }
    }
    Some(vars)
}

// ---------------------------------------------------------------------------
// Template rendering
// ---------------------------------------------------------------------------

/// Substitute `{var}` placeholders in `template` with values from `vars`.
pub fn render_template(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (k, v) in vars {
        result = result.replace(&format!("{{{k}}}"), v);
    }
    result
}

/// Sanitize a string for use in a HomeCore device ID.
/// Replaces characters that are awkward in MQTT topics or HTTP paths.
pub fn sanitize_id(s: &str) -> String {
    s.replace(':', "_").replace(' ', "_").replace('/', "_")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_segment_capture() {
        let segs = parse_pattern("shellies/{device}/relay/0");
        let vars = match_segments(&segs, "shellies/myplug/relay/0").unwrap();
        assert_eq!(vars["device"], "myplug");
    }

    #[test]
    fn partial_capture_prefix() {
        let segs = parse_pattern("homecore/devices/shelly_{device}/cmd");
        let vars = match_segments(&segs, "homecore/devices/shelly_myplug/cmd").unwrap();
        assert_eq!(vars["device"], "myplug");
    }

    #[test]
    fn literal_mismatch_returns_none() {
        let segs = parse_pattern("shellies/{device}/relay/0");
        assert!(match_segments(&segs, "shellies/myplug/relay/1").is_none());
    }

    #[test]
    fn length_mismatch_returns_none() {
        let segs = parse_pattern("a/b/c");
        assert!(match_segments(&segs, "a/b").is_none());
    }

    #[test]
    fn render_substitutes_vars() {
        let mut vars = HashMap::new();
        vars.insert("device".into(), "myplug".into());
        assert_eq!(
            render_template("shellies/{device}/relay/0/command", &vars),
            "shellies/myplug/relay/0/command"
        );
    }

    #[test]
    fn sanitize_replaces_colon() {
        assert_eq!(sanitize_id("switch:0"), "switch_0");
    }
}
