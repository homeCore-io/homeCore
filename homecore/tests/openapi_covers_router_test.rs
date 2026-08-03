//! The OpenAPI spec must describe exactly the routes the router serves.
//!
//! `docs/openapi.yaml` is hand-maintained, and nothing checked it, so it drifted:
//! by core 0.1.18 it was stamped `0.1.5` and had missed thirteen endpoints,
//! including whole features (`/plugins/status`, `/registry/plugins`). A spec that
//! is quietly wrong is worse than no spec, because callers believe it.
//!
//! Deliberately string-matching rather than parsing YAML: adding a YAML crate to
//! core's dependency tree to check a doc file is a poor trade, and the two things
//! compared here — `.route("…")` calls and top-level `  /path:` keys — are both
//! simple enough to read directly.

use std::collections::BTreeSet;

/// Normalise a path so `/plugins/:id` and `/plugins/{plugin_id}` compare equal.
/// The spec names its parameters; the router only positions them.
fn normalise(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ':' => {
                while chars
                    .peek()
                    .is_some_and(|n| n.is_alphanumeric() || *n == '_')
                {
                    chars.next();
                }
                out.push_str("{}");
            }
            '{' => {
                while chars.peek().is_some_and(|n| *n != '}') {
                    chars.next();
                }
                chars.next(); // consume '}'
                out.push_str("{}");
            }
            _ => out.push(c),
        }
    }
    out
}

fn router_paths() -> BTreeSet<String> {
    let src = include_str!("../../crates/hc-api/src/lib.rs");
    let mut found = BTreeSet::new();
    for (idx, _) in src.match_indices(".route(") {
        // The path literal is the first string after `.route(`, which may sit
        // on the following line when rustfmt has wrapped the call.
        let rest = &src[idx..];
        let Some(open) = rest.find('"') else { continue };
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            continue;
        };
        let path = &after[..close];
        if path.starts_with('/') {
            found.insert(normalise(path));
        }
    }
    found
}

fn spec_paths() -> BTreeSet<String> {
    let spec = include_str!("../../docs/openapi.yaml");
    let mut found = BTreeSet::new();
    let mut in_paths = false;
    for line in spec.lines() {
        if line.starts_with("paths:") {
            in_paths = true;
            continue;
        }
        // A new top-level key ends the paths block.
        if in_paths && !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            break;
        }
        if !in_paths {
            continue;
        }
        // Exactly two spaces of indent, a leading slash, a trailing colon.
        if let Some(rest) = line.strip_prefix("  /") {
            if !line.starts_with("   ") {
                if let Some(path) = rest.strip_suffix(':') {
                    found.insert(normalise(&format!("/{path}")));
                }
            }
        }
    }
    found
}

#[test]
fn every_route_is_documented() {
    let undocumented: Vec<_> = router_paths().difference(&spec_paths()).cloned().collect();
    assert!(
        undocumented.is_empty(),
        "these routes exist but are absent from docs/openapi.yaml:\n  {}\n\n\
         Add them to the spec, or the API's own documentation is lying about \
         what the API does.",
        undocumented.join("\n  ")
    );
}

#[test]
fn nothing_is_documented_that_does_not_exist() {
    let phantom: Vec<_> = spec_paths().difference(&router_paths()).cloned().collect();
    assert!(
        phantom.is_empty(),
        "docs/openapi.yaml describes routes the router does not serve:\n  {}\n\n\
         A caller who trusts the spec gets a 404. Remove them, or restore the route.",
        phantom.join("\n  ")
    );
}

#[test]
fn the_spec_is_stamped_with_the_crate_version() {
    // The spec said 0.1.5 while core shipped 0.1.18 — thirteen releases of
    // silent drift, and the version is the first thing a reader trusts.
    let spec = include_str!("../../docs/openapi.yaml");
    let want = format!("version: \"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(
        spec.contains(&want),
        "docs/openapi.yaml is not stamped {}; found: {:?}",
        want,
        spec.lines()
            .find(|l| l.trim_start().starts_with("version:"))
            .unwrap_or("<no version line>")
    );
}

#[test]
fn path_normalisation_is_symmetric() {
    // The comparison above is only as good as this.
    assert_eq!(
        normalise("/plugins/:id/config"),
        normalise("/plugins/{plugin_id}/config")
    );
    assert_eq!(normalise("/a/:x/b/:y"), normalise("/a/{one}/b/{two}"));
    assert_ne!(normalise("/plugins/status"), normalise("/plugins/{id}"));
    assert_eq!(normalise("/health"), "/health");
}
