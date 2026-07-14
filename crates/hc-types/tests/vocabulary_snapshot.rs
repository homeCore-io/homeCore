//! A committed snapshot of the rule vocabulary.
//!
//! `docs/rule-vocabulary.json` is the file every client checks itself against.
//! This test is what makes it trustworthy: change the rule types and it fails,
//! so the snapshot cannot silently fall behind the code that generates it.
//!
//!     cargo test -p hc-types --features schema
//!     UPDATE_VOCABULARY=1 cargo test -p hc-types --features schema   # regenerate
//!
//! The chain this closes:
//!
//!   1. someone adds a `Trigger` variant
//!   2. THIS test fails — the snapshot is stale       <- core cannot be silent
//!   3. they regenerate it
//!   4. hc-web's conformance test fails — its table is missing the variant
//!   5. they add it
//!
//! Before this, step 2 and step 4 did not exist. The client's own tripwire
//! asserted that ITS table had 18 triggers in it, which measures the mirror and
//! not the thing being mirrored, and passes happily while core grows a 19th.

#![cfg(feature = "schema")]

use std::path::PathBuf;

use hc_types::vocabulary::Vocabulary;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/rule-vocabulary.json")
}

#[test]
fn the_committed_vocabulary_matches_the_types() {
    let derived = Vocabulary::derive();
    let json = serde_json::to_string_pretty(&derived).unwrap() + "\n";
    let path = snapshot_path();

    if std::env::var("UPDATE_VOCABULARY").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &json).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing.\n\
             Generate it with:\n  \
             UPDATE_VOCABULARY=1 cargo test -p hc-types --features schema",
            path.display()
        )
    });

    if committed != json {
        let old: Vocabulary = serde_json::from_str(&committed).unwrap();

        let names = |v: &[hc_types::vocabulary::VariantSpec]| {
            v.iter().map(|s| s.tag.clone()).collect::<Vec<_>>()
        };
        let diff = |was: Vec<String>, now: Vec<String>| {
            let added: Vec<_> = now.iter().filter(|t| !was.contains(t)).cloned().collect();
            let gone: Vec<_> = was.iter().filter(|t| !now.contains(t)).cloned().collect();
            (added, gone)
        };

        let (t_add, t_gone) = diff(names(&old.triggers), names(&derived.triggers));
        let (c_add, c_gone) = diff(names(&old.conditions), names(&derived.conditions));
        let (a_add, a_gone) = diff(names(&old.actions), names(&derived.actions));

        panic!(
            "the rule vocabulary changed and the snapshot is stale.\n\
             \n\
             triggers   added {t_add:?}  removed {t_gone:?}\n\
             conditions added {c_add:?}  removed {c_gone:?}\n\
             actions    added {a_add:?}  removed {a_gone:?}\n\
             \n\
             (field-level changes do not show above, but also fail this test.)\n\
             \n\
             Regenerate:\n  \
             UPDATE_VOCABULARY=1 cargo test -p hc-types --features schema\n\
             \n\
             Then update every client's descriptor table — hc-web's conformance\n\
             test will tell you exactly what it is missing."
        );
    }
}
