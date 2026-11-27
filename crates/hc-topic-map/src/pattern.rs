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
                // Longer than prefix and suffix together, or they overlap
                // (`tasmota_relay1` against `tasmota_{device}_relay1`) and the
                // slice below runs backwards and panics. An empty capture
                // names no device.
                if part.len() > prefix.len() + suffix.len()
                    && part.starts_with(prefix.as_str())
                    && part.ends_with(suffix.as_str())
                {
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

/// Whether some topic matches both patterns.
pub fn patterns_overlap(a: &[Segment], b: &[Segment]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| segments_overlap(x, y))
}

fn segments_overlap(a: &Segment, b: &Segment) -> bool {
    use Segment::*;
    match (a, b) {
        (Literal(x), Literal(y)) => x == y,
        (Capture(_), _) | (_, Capture(_)) => true,
        (PartialCapture { prefix, suffix, .. }, Literal(l))
        | (Literal(l), PartialCapture { prefix, suffix, .. }) => {
            l.len() > prefix.len() + suffix.len() && l.starts_with(prefix) && l.ends_with(suffix)
        }
        (
            PartialCapture {
                prefix: p1,
                suffix: s1,
                ..
            },
            PartialCapture {
                prefix: p2,
                suffix: s2,
                ..
            },
        ) => {
            (p1.starts_with(p2.as_str()) || p2.starts_with(p1.as_str()))
                && (s1.ends_with(s2.as_str()) || s2.ends_with(s1.as_str()))
        }
    }
}

/// Whether a device id fits `template`, where a `{…}` left in it stands for
/// one or more characters: `shelly_sh1_relay{relay}` fits `shelly_sh1_relay0`
/// and not `shelly_sh1_2_relay0`.
pub fn id_matches(template: &str, id: &str) -> bool {
    let Some(open) = template.find('{') else {
        return template == id;
    };
    let Some(close) = template[open..].find('}') else {
        return template == id;
    };
    let rest = &template[open + close + 1..];
    let Some(after) = id.strip_prefix(&template[..open]) else {
        return false;
    };
    // The placeholder takes at least one character, then the rest must fit.
    after
        .char_indices()
        .skip(1)
        .map(|(i, _)| i)
        .chain((!after.is_empty()).then_some(after.len()))
        .any(|i| id_matches(rest, &after[i..]))
}

/// Sanitize a string for use in a homeCore device ID.
/// Replaces characters that are awkward in MQTT topics or HTTP paths.
pub fn sanitize_id(s: &str) -> String {
    s.replace([':', ' ', '/'], "_")
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
    fn prefix_and_suffix_that_overlap_do_not_match() {
        let segs = parse_pattern("homecore/devices/tasmota_{device}_relay1/cmd");
        assert!(match_segments(&segs, "homecore/devices/tasmota_relay1/cmd").is_none());
        assert!(match_segments(&segs, "homecore/devices/tasmota__relay1/cmd").is_none());
        let vars = match_segments(&segs, "homecore/devices/tasmota_x_relay1/cmd").unwrap();
        assert_eq!(vars["device"], "x");
    }

    #[test]
    fn patterns_that_share_a_topic_overlap() {
        let p = parse_pattern;
        assert!(patterns_overlap(
            &p("stat/{device}/POWER"),
            &p("stat/{x}/{y}")
        ));
        assert!(patterns_overlap(&p("a/dev_{d}"), &p("a/{x}_1")));
        assert!(!patterns_overlap(
            &p("stat/{device}/POWER"),
            &p("tele/{device}/POWER")
        ));
        assert!(!patterns_overlap(&p("a/{d}"), &p("a/{d}/b")));
        assert!(!patterns_overlap(&p("a/dev_{d}"), &p("a/x")));
    }

    #[test]
    fn ids_fit_a_template() {
        assert!(id_matches("shelly_sh1_relay{relay}", "shelly_sh1_relay0"));
        assert!(!id_matches(
            "shelly_sh1_relay{relay}",
            "shelly_sh1_2_relay0"
        ));
        assert!(!id_matches("shelly_sh1_relay{relay}", "shelly_sh1_relay"));
        assert!(id_matches("shelly_sh1_sensor", "shelly_sh1_sensor"));
        assert!(id_matches("a_{x}_b_{y}", "a_1_b_2"));
        assert!(!id_matches("a_{x}_b_{y}", "a_1_c_2"));
    }

    #[test]
    fn sanitize_replaces_colon() {
        assert_eq!(sanitize_id("switch:0"), "switch_0");
    }
}
