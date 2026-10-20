#!/usr/bin/env python3
"""Move stored automations to the device vocabulary (October 2026).

The plugins moved to one name, one meaning and one unit per idea
(`crates/hc-types/src/device_vocabulary.rs`). This rewrites the automations
that still use an old name or command shape, in three phases, because each
change only works once the version that understands it is installed:

  now      Works with the versions already running: Lutron/Caseta keypad and
           scene steps written as keys become `{"action": ...}`; WLED's
           `preset`, `effect`, `palette` become `preset_id`, `effect_id`,
           `palette_id`.
  plugins  Run right after installing the plugin releases: renamed attributes
           (`humidity_pct` -> `humidity`, a Lutron scene's `on` -> `active`,
           `battery` -> `battery_pct`, Sonos media names, ...).
  core     Run after installing the core release that takes `{"action": ...}`
           on its own devices: timers, modes, counters ... `{"command": x}`
           -> `{"action": x}`.

It only shows what it would change unless given --apply. Every change is
printed before and after; temperature thresholds are never converted, only
reported, because the old unit depended on settings.

    vocabulary-rewrite.py --url http://HOST:PORT --phase now
    vocabulary-rewrite.py --url http://HOST:PORT --phase now --apply

Sign-in: HC_TOKEN in the environment, or --user (the password is asked for
and never echoed).
"""

import argparse
import copy
import getpass
import json
import os
import sys
import urllib.request

# ── What changed, per plugin ─────────────────────────────────────────────────

# (plugin, device_type or None for any) -> {old attribute: new attribute}
RENAMES = {
    ("plugin.yolink", None): {
        "humidity_pct": "humidity",
        "battery": "battery_pct",
        "door_open": "open",
    },
    ("plugin.zwave", None): {
        "battery": "battery_pct",
        "contact_open": "open",
        "tamper": "tampered",
        "co2_ppm": "co2",
        "mode": "hvac_mode",
    },
    ("plugin.lutron", "scene"): {"on": "active"},
    ("plugin.lutron", None): {"occupancy": "occupied"},
    ("plugin.caseta", None): {"occupancy": "occupied"},
    ("plugin.hue", None): {
        "illuminance_lux": "illuminance",
        "duration_ms": "transition_ms",
    },
    ("plugin.sonos", None): {
        "title": "media_title",
        "artist": "media_artist",
        "album": "media_album",
        "position_secs": "media_position",
        "duration_secs": "media_duration",
    },
    ("plugin.thermostat", None): {
        "current_temperature": "temperature",
        "mode": "hvac_mode",
        "call_for": "hvac_action",
    },
}

# Renamed for reading only: a scene is activated, never written `active`.
READ_ONLY = {("plugin.lutron", "on")}

# Old values that changed with the name: (plugin, new attribute) -> {old: new}
VALUE_MAPS = {
    ("plugin.thermostat", "hvac_action"): {"heat": "heating", "cool": "cooling"},
}

# Names a person must look at: the value's unit or scale changed.
REVIEW = {
    "temperature", "current_temperature", "setpoint", "target_temp",
    "brightness", "color_temp_mirek", "contact_state", "cci_",
}

WLED_KEYS = {"preset": "preset_id", "apply_preset": "preset_id",
             "effect": "effect_id", "palette": "palette_id"}
KEY_ACTIONS = {"activate", "raise", "lower", "stop", "execute"}


# ── The API ──────────────────────────────────────────────────────────────────

class Api:
    def __init__(self, url, token):
        self.url = url.rstrip("/") + "/api/v1"
        self.token = token

    def call(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.url + path, data=data, method=method)
        req.add_header("content-type", "application/json")
        if self.token:
            req.add_header("authorization", "Bearer " + self.token)
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
        return json.loads(raw) if raw else None


def sign_in(url, user):
    token = os.environ.get("HC_TOKEN")
    if token:
        return token
    if not user:
        sys.exit("Give --user (the password is asked for) or set HC_TOKEN.")
    password = getpass.getpass(f"Password for {user}: ")
    out = Api(url, None).call("POST", "/auth/login",
                              {"username": user, "password": password})
    return out["token"]


def listed(v, key):
    return v if isinstance(v, list) else v.get(key, [])


# ── The rewrite ──────────────────────────────────────────────────────────────

class Rewriter:
    def __init__(self, devices, phase):
        self.devices = {d["device_id"]: d for d in devices}
        self.phase = phase
        self.notes = []

    def device(self, device_id):
        d = self.devices.get(device_id) or {}
        return d.get("plugin_id", ""), d.get("device_type", "")

    def renames(self, device_id):
        plugin, dtype = self.device(device_id)
        out = {}
        for (p, t), m in RENAMES.items():
            if p == plugin and t is None:
                out.update(m)
        for (p, t), m in RENAMES.items():
            if p == plugin and t == dtype:
                out.update(m)
        return plugin, out

    # A command written to a device: {"set_led": {...}}, {"command": "start"}, ...
    def command(self, device_id, state):
        plugin, dtype = self.device(device_id)
        new = dict(state)
        if self.phase == "now" and plugin in ("plugin.lutron", "plugin.caseta"):
            if isinstance(new.get("set_led"), dict) and "action" not in new:
                led = new.pop("set_led")
                new = {"action": "set_led", **led, **new}
            for verb in KEY_ACTIONS:
                if new.get(verb) is True and "action" not in new:
                    new.pop(verb)
                    new = {"action": verb, **new}
        if self.phase == "now" and plugin == "plugin.wled":
            for old, cur in WLED_KEYS.items():
                if old in new and cur not in new:
                    new[cur] = new.pop(old)
        if self.phase == "plugins":
            _, ren = self.renames(device_id)
            for old, cur in ren.items():
                if (plugin, old) in READ_ONLY:
                    continue
                if old in new and cur not in new:
                    new[cur] = new.pop(old)
        if self.phase == "core" and plugin == "core.glue" and "command" in new \
                and "action" not in new:
            new["action"] = new.pop("command")
        return new

    def read(self, node):
        device_ids = [node["device_id"]] if isinstance(node.get("device_id"), str) \
            else node.get("device_ids") or []
        attr = node.get("attribute")
        if not device_ids or not isinstance(attr, str):
            return
        if any(attr == r or attr.startswith(r) for r in REVIEW if r.endswith("_")) \
                or attr in REVIEW:
            self.notes.append(f"reads `{attr}` on {device_ids[0]}: its unit or "
                              "scale changed; check the number by hand")
        if self.phase != "plugins":
            return
        plugin, ren = self.renames(device_ids[0])
        if attr in ren:
            node["attribute"] = ren[attr]
            vmap = VALUE_MAPS.get((plugin, ren[attr]), {})
            for k in ("to", "from", "value"):
                if isinstance(node.get(k), str) and node[k] in vmap:
                    node[k] = vmap[node[k]]

    def walk(self, o):
        if isinstance(o, dict):
            self.read(o)
            if isinstance(o.get("device_id"), str):
                for k in ("state", "target", "default_state"):
                    if isinstance(o.get(k), dict):
                        o[k] = self.command(o["device_id"], o[k])
            for v in o.values():
                self.walk(v)
        elif isinstance(o, list):
            for v in o:
                self.walk(v)


def lines(v):
    return json.dumps(v, indent=2, sort_keys=True).splitlines()


def show(before, after):
    import difflib
    for line in difflib.unified_diff(lines(before), lines(after), lineterm="", n=1):
        if not line.startswith(("---", "+++")):
            print("    " + line)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", required=True, help="homeCore, e.g. http://10.0.10.150:8080")
    ap.add_argument("--user")
    ap.add_argument("--phase", required=True, choices=["now", "plugins", "core"])
    ap.add_argument("--apply", action="store_true", help="write the changes")
    args = ap.parse_args()

    api = Api(args.url, sign_in(args.url, args.user))
    devices = listed(api.call("GET", "/devices"), "devices")
    autos = listed(api.call("GET", "/automations"), "automations")

    changed = 0
    for auto in autos:
        rw = Rewriter(devices, args.phase)
        after = copy.deepcopy(auto)
        for k in ("trigger", "conditions", "actions"):
            rw.walk(after.get(k))
        if after == auto and not rw.notes:
            continue
        print(f"\n{auto['name']}  ({auto['id']})")
        for n in dict.fromkeys(rw.notes):
            print("  ! " + n)
        if after == auto:
            continue
        changed += 1
        show(auto, after)
        if args.apply:
            api.call("PUT", f"/automations/{auto['id']}", after)
            print("  written")

    verb = "Changed" if args.apply else "Would change"
    print(f"\n{verb} {changed} of {len(autos)} automations (phase {args.phase}).")
    if changed and not args.apply:
        print("Nothing was written. Run again with --apply to write.")


if __name__ == "__main__":
    main()
