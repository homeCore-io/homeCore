//! Conformance fixtures for the dashboard layout engine.
//!
//! `docs/dashboard-layout-fixtures.json` is what a client checks its own layout
//! engine against. Each case is an input layout, the column count and flow it is
//! read under, and the placements [`hc_types::dashboard_layout`] produces — so a
//! Dart, TypeScript or Rust client can assert it agrees with core's reference
//! rather than with its own belief about what packing means.
//!
//!     cargo test -p hc-types
//!     UPDATE_LAYOUT_FIXTURES=1 cargo test -p hc-types   # regenerate
//!
//! The `expected` side is *derived*, never typed out: a case states a situation
//! and the reference answers it. Hand-writing expectations would make this a
//! second implementation to keep in step, which is the problem it exists to
//! solve.
//!
//! Why fixtures rather than only prose: `normalize` runs before every save, and
//! core rejects the whole dashboard on the first bad placement. A client that
//! normalises differently does not draw a page differently — it loses the
//! user's edit. That is worth pinning case by case.

use std::path::PathBuf;

use hc_types::dashboard::{DashboardFlow, DashboardRect};
use hc_types::dashboard_layout::{Engine, GridItem};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Case {
    name: String,
    /// What this case is actually asserting. Kept in the artifact so a client
    /// author reading a failure knows what the rule is, not just that two
    /// numbers differ.
    why: String,
    columns: i32,
    flow: String,
    input: Vec<GridItem>,
    expected: Vec<GridItem>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Fixtures {
    /// What produced `expected`, so a client knows what it is conforming to.
    reference: String,
    cases: Vec<Case>,
}

fn item(id: &str, x: i32, y: i32, w: i32, h: i32) -> GridItem {
    GridItem {
        id: id.to_string(),
        x,
        y,
        w,
        h,
        section_id: None,
        floating: false,
        rect: None,
    }
}

fn floating(id: &str, x: i32, y: i32, w: i32, h: i32) -> GridItem {
    GridItem {
        floating: true,
        ..item(id, x, y, w, h)
    }
}

fn composed(id: &str, x: i32, y: i32, w: i32, h: i32) -> GridItem {
    GridItem {
        rect: Some(DashboardRect {
            x: 10.0,
            y: 200.0,
            w: 100.0,
            h: 40.0,
        }),
        ..item(id, x, y, w, h)
    }
}

fn in_section(section: &str, mut i: GridItem) -> GridItem {
    i.section_id = Some(section.to_string());
    i
}

/// Every case, as `(name, why, columns, flow, input)`. `expected` is computed.
fn situations() -> Vec<(
    &'static str,
    &'static str,
    i32,
    DashboardFlow,
    Vec<GridItem>,
)> {
    vec![
        (
            "packed closes a gap",
            "Under `packed`, gravity pulls every card up into the space above it. \
             This is what every dashboard written before `flow` existed meant.",
            12,
            DashboardFlow::Packed,
            vec![item("a", 0, 0, 2, 1), item("b", 0, 5, 2, 1)],
        ),
        (
            "free keeps a gap",
            "Under `free`, a gap is content: the card stays at row 5 because \
             somebody put it there. This is the whole point of the flag.",
            12,
            DashboardFlow::Free,
            vec![item("a", 0, 0, 2, 1), item("b", 0, 5, 2, 1)],
        ),
        (
            "an overlap is resolved under packed",
            "Overlapping cards are a layout to be fixed, never a design.",
            12,
            DashboardFlow::Packed,
            vec![item("a", 0, 0, 2, 2), item("b", 1, 1, 2, 2)],
        ),
        (
            "an overlap is resolved under free too",
            "A gap being content does not make an overlap acceptable. `free` \
             changes gravity and nothing else.",
            12,
            DashboardFlow::Free,
            vec![item("a", 0, 0, 2, 2), item("b", 1, 1, 2, 2)],
        ),
        (
            "a cascade pushes each card past the one above it",
            "Three cards stacked on the same cells resolve in reading order, \
             each dropping to the bottom edge of what blocked it.",
            12,
            DashboardFlow::Free,
            vec![
                item("a", 0, 0, 4, 2),
                item("b", 0, 0, 4, 2),
                item("c", 0, 0, 4, 2),
            ],
        ),
        (
            "a card wider than the grid is clamped, not rejected",
            "Core would reject `x + w > columns` outright and the user would \
             lose the whole edit, so the client trims it first.",
            4,
            DashboardFlow::Packed,
            vec![item("a", 3, 0, 99, 1)],
        ),
        (
            "negative coordinates come back inside",
            "A hand-edited or buggy document must open, approximately right \
             rather than broken.",
            12,
            DashboardFlow::Packed,
            vec![item("a", -5, -5, 2, 1)],
        ),
        (
            "zero and negative sizes become one cell",
            "`w` and `h` are clamped to at least 1 — a zero-size card is \
             invisible and unselectable, which is a card you cannot delete.",
            12,
            DashboardFlow::Free,
            vec![item("a", 0, 0, 0, -3)],
        ),
        (
            "items never collide across sections",
            "Sections partition a layout. Two cards on the same cell in \
             different sections are both legal and neither moves.",
            12,
            DashboardFlow::Free,
            vec![
                in_section("top", item("a", 0, 0, 2, 1)),
                in_section("bottom", item("b", 0, 0, 2, 1)),
            ],
        ),
        (
            "a floating element is neither pushed nor pulled",
            "It sits above the grid: nothing pushes it, it pushes nothing, and \
             gravity does not pull it. The position is the design.",
            12,
            DashboardFlow::Packed,
            vec![item("a", 0, 0, 2, 1), floating("float", 0, 4, 2, 1)],
        ),
        (
            "a grid card ignores a floating one above it",
            "The floating element competes for nothing, so the grid card rises \
             straight through the cells it occupies.",
            12,
            DashboardFlow::Packed,
            vec![floating("float", 0, 0, 4, 2), item("a", 0, 6, 2, 1)],
        ),
        (
            "a composed element rises under packed",
            "DOCUMENTED, NOT ENDORSED. `normalize` skips floating elements \
             explicitly and composed ones implicitly (they compete for nothing), \
             but gravity skips only floating — so a composed element never \
             breaks the rise loop and lands at y=0. Usually invisible, because a \
             composed element is drawn from its `rect`; not harmless, because \
             the cells are what core validates and what a frame-unaware client \
             draws. Pinned so a change here is a decision, not a slip.",
            12,
            DashboardFlow::Packed,
            vec![composed("c", 0, 6, 2, 1)],
        ),
        (
            "a composed element stays put under free",
            "Which is the flow a composed layout actually uses: it was placed \
             on a canvas, and packing a composition would be the engine \
             overruling the design.",
            12,
            DashboardFlow::Free,
            vec![composed("c", 0, 6, 2, 1)],
        ),
        (
            "reading order decides who wins a contested cell",
            "Placement runs in `(y, x)` order, not document order, so a corrupt \
             layout resolves the same way whatever order the JSON happened to \
             be in. Note the input here is deliberately reversed.",
            12,
            DashboardFlow::Free,
            vec![item("later", 4, 0, 2, 1), item("first", 0, 0, 2, 1)],
        ),
        (
            "an already-legal layout is left alone",
            "Idempotence, which is what makes it safe to run before every save: \
             a document must not drift a little on each round trip.",
            12,
            DashboardFlow::Free,
            vec![
                item("a", 0, 0, 3, 2),
                item("b", 3, 0, 2, 1),
                item("c", 0, 7, 4, 1),
            ],
        ),
    ]
}

fn flow_name(flow: DashboardFlow) -> String {
    match serde_json::to_value(flow) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("DashboardFlow is a unit enum"),
    }
}

fn build() -> Fixtures {
    let cases = situations()
        .into_iter()
        .map(|(name, why, columns, flow, input)| {
            let engine = Engine::new(columns, flow);
            let expected = engine.normalize(&input);
            Case {
                name: name.to_string(),
                why: why.to_string(),
                columns,
                flow: flow_name(flow),
                input,
                expected,
            }
        })
        .collect();

    Fixtures {
        reference: "hc_types::dashboard_layout::Engine::normalize".to_string(),
        cases,
    }
}

fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/dashboard-layout-fixtures.json")
}

#[test]
fn the_committed_fixtures_match_the_reference() {
    let built = build();
    let json = serde_json::to_string_pretty(&built).unwrap() + "\n";
    let path = fixtures_path();

    if std::env::var("UPDATE_LAYOUT_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &json).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing.\n\
             Generate it with:\n  \
             UPDATE_LAYOUT_FIXTURES=1 cargo test -p hc-types",
            path.display()
        )
    });

    if committed != json {
        let old: Fixtures = serde_json::from_str(&committed).unwrap();
        let changed: Vec<_> = built
            .cases
            .iter()
            .filter(|c| {
                old.cases
                    .iter()
                    .find(|o| o.name == c.name)
                    .is_none_or(|o| o != *c)
            })
            .map(|c| c.name.as_str())
            .collect();

        panic!(
            "the layout reference changed and the fixtures are stale.\n\
             \n\
             cases that differ: {changed:?}\n\
             \n\
             If this was deliberate, regenerate:\n  \
             UPDATE_LAYOUT_FIXTURES=1 cargo test -p hc-types\n\
             \n\
             Then every client's layout engine has to change with it — that is\n\
             what these fixtures are for. A layout that normalises differently\n\
             in two clients does not draw differently, it LOSES the edit: core\n\
             rejects the whole dashboard on the first bad placement."
        );
    }
}

/// Every case must survive a second pass unchanged.
///
/// Checked here rather than trusted, because it is the property that makes
/// `normalize` safe to run before every save.
#[test]
fn every_fixture_is_a_fixed_point() {
    for (name, _, columns, flow, input) in situations() {
        let engine = Engine::new(columns, flow);
        let once = engine.normalize(&input);
        let twice = engine.normalize(&once);
        assert_eq!(once, twice, "'{name}' is not idempotent");
    }
}

/// Every case's output must be one core would accept.
#[test]
fn every_fixture_produces_a_legal_layout() {
    for (name, _, columns, flow, input) in situations() {
        let engine = Engine::new(columns, flow);
        let out = engine.normalize(&input);
        assert!(engine.is_legal(&out), "'{name}' produced an illegal layout");
    }
}
