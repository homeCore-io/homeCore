//! Conformance fixtures for the plugin widget descriptor.
//!
//! `docs/dashboard-widget-fixtures.json` is what a plugin SDK — and any client
//! that previews a registration before sending it — checks itself against. Each
//! case is a descriptor and the verdict [`hc_types::widget_descriptor::validate`]
//! reaches, so an SDK in Rust, Python or TypeScript can assert it agrees with
//! core rather than with its own belief about what is registrable.
//!
//!     cargo test -p hc-types
//!     UPDATE_WIDGET_FIXTURES=1 cargo test -p hc-types   # regenerate
//!
//! The verdict is *derived*, never typed out: a case states a descriptor and the
//! reference answers it. Hand-writing the rejections would make this a second
//! implementation to keep in step, which is the problem it exists to solve.
//!
//! Why fixtures rather than only prose: a registration is rejected whole, and
//! the rejection a plugin author sees is the entire diagnostic they get. Pinning
//! the *reason*, not just the pass/fail, is what keeps that message from
//! degrading into "invalid descriptor" three refactors from now.

use std::path::PathBuf;

use hc_types::widget_descriptor::{
    validate, Binding, CodeAttachment, Portability, RenderElement, WidgetDescriptor,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Case {
    name: String,
    /// What this case is actually asserting, kept in the artifact so an SDK
    /// author reading a failure knows the rule and not just the verdict.
    why: String,
    descriptor: WidgetDescriptor,
    accepted: bool,
    /// The exact message, when rejected. This is the contract too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Derived, and stated so a client can see what the render/code split buys.
    portability: Portability,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Fixtures {
    reference: String,
    cases: Vec<Case>,
}

fn el(kind: &str, fields: Value) -> RenderElement {
    RenderElement {
        kind: kind.to_string(),
        children: Vec::new(),
        fields: match fields {
            Value::Object(m) => m,
            _ => Map::new(),
        },
    }
}

fn container(kind: &str, children: Vec<RenderElement>) -> RenderElement {
    RenderElement {
        kind: kind.to_string(),
        children,
        fields: Map::new(),
    }
}

fn descriptor(widget_id: &str, render: Option<RenderElement>) -> WidgetDescriptor {
    WidgetDescriptor {
        widget_id: widget_id.to_string(),
        title: "Boiler flow".to_string(),
        icon: Some("water".to_string()),
        config_schema: Vec::new(),
        bindings: Vec::new(),
        render,
        code: None,
    }
}

fn binding(name: &str) -> Binding {
    Binding {
        name: name.to_string(),
        device: "{{config.device_id}}".to_string(),
        key: "flow_lpm".to_string(),
        in_from: None,
        in_to: None,
        out_from: None,
        out_to: None,
        decimals: None,
    }
}

fn gauge() -> RenderElement {
    el(
        "gauge",
        json!({"value": "flow", "shape": "radial", "max": 30.0}),
    )
}

/// Every case, as `(name, why, descriptor)`. The verdict is computed.
fn situations() -> Vec<(&'static str, &'static str, WidgetDescriptor)> {
    vec![
        (
            "a portable widget is accepted",
            "The ordinary case: bindings plus instruments core knows. Every \
             client draws this, hc-tui included, and no core release was needed \
             to add it.",
            WidgetDescriptor {
                bindings: vec![binding("flow")],
                ..descriptor("boiler_flow", Some(gauge()))
            },
        ),
        (
            "code registered against a render is accepted",
            "The Lovelace case. hc-web draws the sandboxed document; every \
             other client draws the gauge. The code is an attachment to the \
             portable description, not a replacement for it.",
            WidgetDescriptor {
                bindings: vec![binding("flow")],
                code: Some(CodeAttachment {
                    entry: "boiler_flow.html".to_string(),
                    grant: vec!["{{config.device_id}}".to_string()],
                }),
                ..descriptor("boiler_flow", Some(gauge()))
            },
        ),
        (
            "code without a render is rejected",
            "THE portability guarantee. A widget that only a browser can draw \
             would make hc-web the only viable homeCore UI, which is exactly \
             what the two-class split exists to prevent — so it is refused \
             where the declaration arrives, not where it fails to draw.",
            WidgetDescriptor {
                code: Some(CodeAttachment {
                    entry: "boiler_flow.html".to_string(),
                    grant: Vec::new(),
                }),
                ..descriptor("boiler_flow", None)
            },
        ),
        (
            "no render at all is rejected",
            "A widget nothing can draw. Read differently from the case above, \
             because a plugin that has not started is not making the same \
             mistake as one that shipped code and stopped.",
            descriptor("boiler_flow", None),
        ),
        (
            "an unknown element kind is rejected",
            "The opposite of an unknown widget `type`, which core accepts on \
             purpose. A type core has never seen is a card some client may \
             still draw; an element kind no client knows is a hole in the page.",
            descriptor("boiler_flow", Some(el("sparkline", json!({})))),
        ),
        (
            "an unknown field on a known element is rejected",
            "Strict, where a widget config is not. An unknown config key is a \
             client's own drawing preference riding along; an unknown render \
             field is a plugin asking every client to draw something none of \
             them agreed to.",
            descriptor(
                "boiler_flow",
                Some(el("gauge", json!({"value": "flow", "glow": 0.4}))),
            ),
        ),
        (
            "a half-specified range is rejected",
            "All four bounds or none. A mapping missing one bound is the case \
             where a gauge quietly reads 0-1 — a card that looks like it works \
             until somebody checks it against the boiler.",
            WidgetDescriptor {
                bindings: vec![Binding {
                    in_from: Some(0.0),
                    in_to: Some(30.0),
                    out_from: Some(0.0),
                    ..binding("flow")
                }],
                ..descriptor("boiler_flow", Some(gauge()))
            },
        ),
        (
            "a complete range is accepted",
            "The same binding with the fourth bound. Lifted out of hc-web's \
             SvgBinding so a mapping means one thing whether it feeds a \
             portable gauge or a sandboxed drawing.",
            WidgetDescriptor {
                bindings: vec![Binding {
                    in_from: Some(0.0),
                    in_to: Some(30.0),
                    out_from: Some(0.0),
                    out_to: Some(1.0),
                    decimals: Some(1),
                    ..binding("flow")
                }],
                ..descriptor("boiler_flow", Some(gauge()))
            },
        ),
        (
            "a missing required field is rejected",
            "A gauge with nothing to show is not a gauge.",
            descriptor("boiler_flow", Some(el("gauge", json!({"shape": "bar"})))),
        ),
        (
            "a value outside one_of is rejected, and the message says what would work",
            "Arriving here is nearly always a typo, so the error names the \
             offending value and the legal ones.",
            descriptor(
                "boiler_flow",
                Some(el("gauge", json!({"value": "flow", "shape": "dial"}))),
            ),
        ),
        (
            "a container holds children",
            "row, column and stack are the only kinds that may. Everything \
             else draws itself.",
            descriptor(
                "boiler_flow",
                Some(container(
                    "row",
                    vec![gauge(), el("text", json!({"content": "Flow"}))],
                )),
            ),
        ),
        (
            "children on an instrument are rejected",
            "A text with children is a mistake worth naming rather than \
             ignoring — ignoring it means one client draws them and the rest \
             do not.",
            descriptor(
                "boiler_flow",
                Some(RenderElement {
                    children: vec![gauge()],
                    ..el("text", json!({"content": "Flow"}))
                }),
            ),
        ),
        (
            "a bad field inside a child is rejected",
            "The tree is validated all the way down. A render that is legal at \
             the root and broken two levels in is the shape that would \
             otherwise reach a client.",
            descriptor(
                "boiler_flow",
                Some(container(
                    "column",
                    vec![el("icon", json!({"name": "water", "size": 24}))],
                )),
            ),
        ),
        (
            "a negative thickness is rejected",
            "Geometry has bounds, and `number` is spelled in this table and \
             nowhere in the widget-config vocabulary, which has no float field.",
            descriptor(
                "boiler_flow",
                Some(el("gauge", json!({"value": "flow", "thickness": -2.0}))),
            ),
        ),
        (
            "a widget with no title is rejected",
            "The title is human-facing and owned by the plugin. Core's own \
             widgets have none here because every client already has labels \
             for those; a plugin card has nowhere else to get one.",
            WidgetDescriptor {
                title: String::new(),
                ..descriptor("boiler_flow", Some(gauge()))
            },
        ),
    ]
}

fn build() -> Fixtures {
    let cases = situations()
        .into_iter()
        .map(|(name, why, descriptor)| {
            let verdict = validate(&descriptor);
            Case {
                name: name.to_string(),
                why: why.to_string(),
                portability: descriptor.portability(),
                accepted: verdict.is_ok(),
                error: verdict.err(),
                descriptor,
            }
        })
        .collect();

    Fixtures {
        reference: "hc_types::widget_descriptor::validate".to_string(),
        cases,
    }
}

fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/dashboard-widget-fixtures.json")
}

#[test]
fn the_committed_fixtures_match_the_reference() {
    let built = build();
    let json = serde_json::to_string_pretty(&built).unwrap() + "\n";
    let path = fixtures_path();

    if std::env::var("UPDATE_WIDGET_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &json).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing.\n\
             Generate it with:\n  \
             UPDATE_WIDGET_FIXTURES=1 cargo test -p hc-types",
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
            "the descriptor reference changed and the fixtures are stale.\n\
             \n\
             cases that differ: {changed:?}\n\
             \n\
             If this was deliberate, regenerate:\n  \
             UPDATE_WIDGET_FIXTURES=1 cargo test -p hc-types\n\
             \n\
             Then every SDK that validates a registration locally has to change\n\
             with it. A rejection message is not decoration: it is the whole\n\
             diagnostic a plugin author gets when core refuses their widget."
        );
    }
}

/// A rejection must say something a plugin author can act on.
///
/// Checked rather than trusted, because the failure mode is gradual: each
/// refactor shortens a message a little, and nothing fails until the day the
/// only thing core says is "invalid descriptor".
#[test]
fn every_rejection_names_the_widget_and_a_reason() {
    for case in build().cases.iter().filter(|c| !c.accepted) {
        let error = case.error.as_deref().unwrap_or_default();
        assert!(
            error.contains(&case.descriptor.widget_id) || case.descriptor.widget_id.is_empty(),
            "'{}' rejected without naming the widget: {error}",
            case.name
        );
        assert!(
            error.split_whitespace().count() >= 5,
            "'{}' rejected without a usable reason: {error}",
            case.name
        );
    }
}

/// Accepting a descriptor must not depend on having just built it in Rust.
///
/// A registration arrives as JSON, so the round trip is the real path and the
/// struct is the convenience.
#[test]
fn every_case_survives_a_json_round_trip() {
    for case in build().cases {
        let text = serde_json::to_string(&case.descriptor).unwrap();
        let back: WidgetDescriptor = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("'{}' did not round-trip: {e}", case.name));
        assert_eq!(back, case.descriptor, "'{}' changed in transit", case.name);
        assert_eq!(
            validate(&back).is_ok(),
            case.accepted,
            "'{}' got a different verdict after a round trip",
            case.name
        );
    }
}

/// The served element list and the table the validator walks must be the same
/// table.
///
/// They are, by construction — `derive` copies `elements()` — which is exactly
/// why this is worth asserting rather than assuming. The failure it guards
/// against is somebody advertising a kind the validator rejects, which would
/// hand every client a contract core refuses to honour.
#[test]
fn the_vocabulary_advertises_exactly_the_elements_the_validator_knows() {
    let vocabulary = hc_types::dashboard_vocabulary::DashboardVocabulary::derive();
    assert!(
        !vocabulary.elements.is_empty(),
        "the vocabulary describes no elements, so no client can know what to draw"
    );

    for spec in &vocabulary.elements {
        assert!(
            hc_types::widget_descriptor::element(&spec.kind).is_some(),
            "the vocabulary advertises '{}', which the validator has never heard of",
            spec.kind
        );
    }
    for spec in hc_types::widget_descriptor::elements() {
        assert!(
            vocabulary.elements.iter().any(|e| e.kind == spec.kind),
            "'{}' is drawable and unadvertised, so no client will implement it",
            spec.kind
        );
    }
}

/// Every advertised element must be usable — not merely known.
///
/// Builds the smallest legal node of each kind by filling exactly its required
/// fields, and asserts core accepts it. This is what catches a required field
/// with no legal value: a kind that appears in the vocabulary, is enumerable, is
/// documented, and cannot be written down.
#[test]
fn every_advertised_element_can_actually_be_used() {
    for spec in hc_types::widget_descriptor::elements() {
        let mut fields = Map::new();
        for f in spec.fields.iter().filter(|f| f.required) {
            let value = match (f.one_of.first(), f.r#type.as_str()) {
                (Some(first), _) => json!(first),
                (None, "string") => json!("x"),
                (None, "number") => json!(f.min.unwrap_or(0) as f64),
                (None, "integer") => json!(f.min.unwrap_or(0)),
                (None, "boolean") => json!(true),
                (None, other) => panic!(
                    "'{}' requires '{}' of type '{other}', which nothing can supply",
                    spec.kind, f.name
                ),
            };
            fields.insert(f.name.clone(), value);
        }

        let probe = WidgetDescriptor {
            render: Some(RenderElement {
                kind: spec.kind.clone(),
                children: Vec::new(),
                fields,
            }),
            ..descriptor("probe", None)
        };
        assert_eq!(
            validate(&probe),
            Ok(()),
            "'{}' is advertised but its minimal form is rejected",
            spec.kind
        );
    }
}
