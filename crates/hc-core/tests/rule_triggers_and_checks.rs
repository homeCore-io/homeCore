//! Several triggers per rule (E2) and checks inside if/otherwise (E3) must
//! survive the round trips a rule makes: JSON from the API, RON to disk and
//! back — and rules written before either keep reading and writing as they
//! did.

use hc_types::rule::{Action, IfCheck, Rule};

fn ron_round_trip(rule: &Rule) -> Rule {
    let cfg = ron::ser::PrettyConfig::default().struct_names(true);
    let text = ron::ser::to_string_pretty(rule, cfg).expect("serialize");
    ron::from_str(&text).unwrap_or_else(|e| panic!("RON no longer parses: {e}\n{text}"))
}

fn rule(json: serde_json::Value) -> Rule {
    serde_json::from_value(json).expect("a rule")
}

#[test]
fn several_triggers_and_a_checked_if_round_trip() {
    let r = rule(serde_json::json!({
        "id": "6f2f1a3e-6b1f-4f0c-9a7e-3b6b0a2d1c44",
        "name": "LED follows lamp",
        "enabled": true,
        "priority": 0,
        "trigger": { "DeviceStateChanged": { "device_id": "lamp", "attribute": "on", "to": true } },
        "triggers": [
            { "DeviceStateChanged": { "device_id": "lamp", "attribute": "on", "to": false } },
            "SystemStarted"
        ],
        "actions": [{ "enabled": true, "action": { "Conditional": {
            "condition": { "Or": { "conditions": [
                { "DeviceState": { "device_id": "lamp", "attribute": "on", "op": "Eq", "value": true } },
                { "TimeWindow": { "start": "06:00:00", "end": "12:00:00" } }
            ] } },
            "then_actions": [{ "SetDeviceState": { "device_id": "keypad", "state": { "led": 1 }, "track_event_value": false } }],
            "else_if": [{ "condition": { "ModeIs": { "mode_id": "mode_night", "on": true } }, "actions": [] }],
            "else_actions": []
        } } }]
    }));
    assert_eq!(r.all_triggers().count(), 3);
    assert_eq!(ron_round_trip(&r), r);
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(rule(json), r);
    let Action::Conditional { condition, .. } = &r.actions[0].action else {
        panic!("a conditional")
    };
    assert!(matches!(condition, IfCheck::Check(_)));
}

#[test]
fn rules_from_before_keep_their_shape() {
    let r = rule(serde_json::json!({
        "id": "6f2f1a3e-6b1f-4f0c-9a7e-3b6b0a2d1c45",
        "name": "Old style",
        "enabled": true,
        "priority": 0,
        "trigger": "SystemStarted",
        "actions": [{ "enabled": true, "action": { "Conditional": {
            "condition": "device_state(\"door\")[\"open\"] == true",
            "then_actions": [],
            "else_actions": []
        } } }]
    }));
    let json = serde_json::to_value(&r).unwrap();
    assert!(
        json.get("triggers").is_none(),
        "no triggers field is written for one"
    );
    assert_eq!(
        json["actions"][0]["action"]["Conditional"]["condition"],
        "device_state(\"door\")[\"open\"] == true"
    );
    assert_eq!(ron_round_trip(&r), r);
    let cfg = ron::ser::PrettyConfig::default().struct_names(true);
    let text = ron::ser::to_string_pretty(&r, cfg).unwrap();
    assert!(!text.contains(" triggers:"), "{text}");
    assert!(text.contains(r#"condition: "device_state"#), "{text}");
}
