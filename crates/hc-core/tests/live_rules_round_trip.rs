//! The committed rule files must parse, and must survive a rewrite.
//!
//! Rules live on disk as RON and core rewrites them — assigning a UUID on
//! first load, and on every edit through the API. So a RON version change
//! risks two distinct failures: files that no longer parse (automations
//! silently disabled with an `error` field set), and files that parse but
//! re-serialize differently (churn at best, an unreadable file at worst).
//!
//! `rules/` in this repository is a real, working set covering the trigger,
//! condition and action shapes actually in use, so it is the right corpus to
//! hold a serialization library to.

use std::path::{Path, PathBuf};

use hc_types::rule::Rule;

fn rules_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../rules")
}

fn rule_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(rules_dir())
        .expect("rules/ must exist")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ron"))
        .collect();
    v.sort();
    v
}

/// Exactly what rule_loader and rule_file_store write.
fn serialize(rule: &Rule) -> String {
    let cfg = ron::ser::PrettyConfig::default().struct_names(true);
    ron::ser::to_string_pretty(rule, cfg).expect("serializing a rule that just parsed")
}

fn parse(path: &Path) -> Rule {
    let content = std::fs::read_to_string(path).unwrap();
    ron::from_str(&content).unwrap_or_else(|e| panic!("{} no longer parses: {e}", path.display()))
}

#[test]
fn every_committed_rule_parses() {
    let files = rule_files();
    assert!(
        files.len() >= 30,
        "expected the real rule set, found {} files",
        files.len()
    );
    for path in files {
        let rule = parse(&path);
        assert!(!rule.name.is_empty(), "{} has no name", path.display());
    }
}

/// Parse → serialize → parse must reach the same rule. This is what core does
/// to a file when it assigns an id or an edit arrives over the API; if it is
/// lossy, the rewrite quietly changes what the automation does.
#[test]
fn every_committed_rule_survives_a_rewrite() {
    for path in rule_files() {
        let original = parse(&path);
        let written = serialize(&original);
        let reparsed: Rule = ron::from_str(&written)
            .unwrap_or_else(|e| panic!("{} did not survive a rewrite: {e}", path.display()));
        assert_eq!(
            original,
            reparsed,
            "{} changed meaning when rewritten",
            path.display()
        );
    }
}

/// Rewriting is idempotent: a file core has already written is not written
/// differently next time. Without this, every restart could churn the rules
/// directory and every edit would produce a spurious diff.
#[test]
fn rewriting_is_stable() {
    for path in rule_files() {
        let once = serialize(&parse(&path));
        let twice = serialize(&ron::from_str::<Rule>(&once).unwrap());
        assert_eq!(
            once,
            twice,
            "{} serializes differently each pass",
            path.display()
        );
    }
}

/// Names the distinct trigger and action variants in the corpus. If a RON
/// change breaks one shape only, this says which — a bare "33 files parsed"
/// would not.
#[test]
fn the_corpus_covers_a_real_spread_of_shapes() {
    let head = |s: String| s.split(['(', ' ']).next().unwrap_or("?").to_string();
    let mut triggers = std::collections::BTreeSet::new();
    let mut actions = std::collections::BTreeSet::new();
    for path in rule_files() {
        let rule = parse(&path);
        triggers.insert(head(format!("{:?}", rule.trigger)));
        for a in &rule.actions {
            actions.insert(head(format!("{:?}", a.action)));
        }
    }
    eprintln!("triggers: {triggers:?}");
    eprintln!("actions:  {actions:?}");
    assert!(triggers.len() >= 3, "corpus is too narrow: {triggers:?}");
    assert!(actions.len() >= 3, "corpus is too narrow: {actions:?}");
}
