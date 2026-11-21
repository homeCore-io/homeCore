// Rhai transforms for the example profiles.
//
// homeCore loads every *.rhai file in the profiles directory (config/profiles)
// at startup, beside the *.toml profiles. Copy this file there with the
// profiles that use it. A topic's `transform = "name"` calls the function of
// that name with the raw payload as a string.
//
// Return a string to publish it as it is, or a map to publish it as JSON. On a
// state topic the result then goes through the profile's field_map and coerce
// like any other payload. parse_json(text) reads JSON into a map; print() and
// debug() write to homeCore's log.

// tasmota.toml, dimmer commands: one Backlog message for the whole command.
//   {"on": true, "brightness_pct": 80, "color_temp": 3000}
//   -> "POWER ON; Dimmer 80; CT 333"
// Tasmota's CT is in mireds (1,000,000 / kelvin), 153 to 500.
fn homecore_cmd_to_tasmota_backlog(payload) {
    let p = parse_json(payload);
    let parts = [];
    if "on" in p {
        parts.push(if p.on { "POWER ON" } else { "POWER OFF" });
    }
    if "brightness_pct" in p {
        parts.push("Dimmer " + p.brightness_pct.to_int());
    }
    if "color_temp" in p && p.color_temp > 0 {
        let ct = (1000000 / p.color_temp).to_int();
        parts.push("CT " + ct.max(153).min(500));
    }
    let out = "";
    for (part, i) in parts {
        if i > 0 { out += "; "; }
        out += part;
    }
    out
}
