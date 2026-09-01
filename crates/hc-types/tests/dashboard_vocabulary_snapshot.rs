//! A committed snapshot of the dashboard vocabulary.
//!
//! `docs/dashboard-vocabulary.json` is the file every client that edits
//! dashboards checks itself against, the way `docs/rule-vocabulary.json` is for
//! rules. This test is what makes it trustworthy: change what core validates and
//! it fails, so the snapshot cannot silently fall behind.
//!
//!     cargo test -p hc-types
//!     UPDATE_DASHBOARD_VOCABULARY=1 cargo test -p hc-types   # regenerate
//!
//! The chain this closes:
//!
//!   1. someone adds a field to `validate_widget_config`
//!   2. they have to add it to `dashboard_vocabulary::catalogue` — because the
//!      validator EXECUTES the catalogue, there is nowhere else to put it
//!   3. THIS test fails — the snapshot is stale        <- core cannot be silent
//!   4. they regenerate it
//!   5. every client diffs the served vocabulary against its own table
//!
//! Step 2 is the difference from the rule vocabulary. There, a `Trigger`
//! variant is reflected out of the type. Here, widget types are deliberately an
//! open string set, so there is no type to reflect — instead the table IS the
//! validator, which gets the same property by a different route.

use std::path::PathBuf;

use hc_types::dashboard_vocabulary::DashboardVocabulary;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/dashboard-vocabulary.json")
}

#[test]
fn the_committed_vocabulary_matches_the_validator() {
    let derived = DashboardVocabulary::derive();
    let json = serde_json::to_string_pretty(&derived).unwrap() + "\n";
    let path = snapshot_path();

    if std::env::var("UPDATE_DASHBOARD_VOCABULARY").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &json).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing.\n\
             Generate it with:\n  \
             UPDATE_DASHBOARD_VOCABULARY=1 cargo test -p hc-types",
            path.display()
        )
    });

    if committed != json {
        let old: DashboardVocabulary = serde_json::from_str(&committed).unwrap();

        let types = |v: &DashboardVocabulary| {
            v.widgets
                .iter()
                .map(|w| w.r#type.clone())
                .collect::<Vec<_>>()
        };
        let was = types(&old);
        let now = types(&derived);
        let added: Vec<_> = now.iter().filter(|t| !was.contains(t)).collect();
        let gone: Vec<_> = was.iter().filter(|t| !now.contains(t)).collect();

        panic!(
            "the dashboard vocabulary changed and the snapshot is stale.\n\
             \n\
             widget types  added {added:?}  removed {gone:?}\n\
             \n\
             (field-level changes do not show above, but also fail this test.)\n\
             \n\
             Regenerate:\n  \
             UPDATE_DASHBOARD_VOCABULARY=1 cargo test -p hc-types\n\
             \n\
             Then update every client that edits dashboards — the served\n\
             vocabulary at GET /api/v1/dashboards/vocabulary is what they\n\
             check themselves against."
        );
    }
}
