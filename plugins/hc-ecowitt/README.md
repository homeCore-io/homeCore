# hc-ecowitt

[![CI](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Ecowitt weather station sensors into HomeCore. Devices are dynamically discovered from incoming data — no manual sensor configuration needed.

## Data ingestion

- **HTTP POST** (primary) — configure the Ecowitt gateway to POST to this plugin
- **HTTP GET polling** (optional) — plugin polls the gateway's `/get_livedata_info` endpoint

## Supported sensors

All sensors discovered from gateway data are auto-registered. Common types include:

- Temperature (indoor/outdoor/soil/water)
- Humidity
- Barometric pressure
- Wind speed and direction
- Rainfall
- UV index
- Solar radiation
- CO2 / PM2.5

## Battery fields

Ecowitt sensors do **not** report a uniform "battery percentage" — the
semantics depend on the sensor family. hc-ecowitt classifies each
incoming value and emits three fields per device:

| field            | type    | meaning                                   |
| ---------------- | ------- | ----------------------------------------- |
| `battery`        | f64     | raw value from the sensor (unchanged)     |
| `battery_low`    | bool    | derived per-kind — true when sensor is low |
| `battery_kind`   | string  | `"binary"`, `"voltage"`, or `"level"`      |

`battery_low` is what rules should trigger off; `battery + battery_kind`
is what dashboards should display.

Taxonomy (per Ecowitt's family-by-family spec, cross-checked against
[`aioecowitt`](https://github.com/home-assistant-libs/aioecowitt)):

| kind                   | raw range | low when     | sensors                                                |
| ---------------------- | --------- | ------------ | ------------------------------------------------------ |
| **binary**             | 0 or 1    | `>= 1`       | WH25, WH26, WH65, WH31 (`batt1..8`), PM2.5 ch 5–8      |
| **voltage** (AA-class) | volts     | `< 1.2`      | WH40, WH68, WH51 soil, WN34 soil temp, WN35 leaf, LDS  |
| **voltage** (supercap) | volts     | `< 2.4`      | WH80, WH85, WH90, WS90                                 |
| **level** (0..=5)      | 0 to 5    | `<= 1`       | WH57 lightning, PM2.5 ch 1–4, leak detectors           |
| **level** (0..=6)      | 0 to 6    | `<= 1`       | WH45 / CO2 5-in-1                                      |

The cloud-API path (`get_livedata_info`) doesn't always tag the sensor
model in its outdoor / rain blocks — those keep the raw `battery`
value but omit `battery_low` and `battery_kind` rather than guess.

## Setup

1. Install it from the web UI — **Plugins → Add** — and open its
   **Configuration** tab. homeCore records the install itself, so there is no
   `[[plugins]]` block to write; it owns the config file
   (`config/plugins/plugin.ecowitt.toml`) and restarts the plugin when it
   changes.

2. **Let the gateway reach the receiver.** `bind_addr` defaults to `127.0.0.1`,
   so out of the box the plugin only accepts POSTs originating on its own host.
   A gateway is a separate box on your network, so unless you change this its
   uploads are dropped by the kernel — the gateway reports no error, the plugin
   sees no request, and the sensors simply never update. Nothing fails loudly.

   ```toml
   [ecowitt]
   bind_addr = "0.0.0.0"
   allowed_source_ips = ["10.0.0.42"]   # the gateway's IP
   ```

   The loopback default is not an accident: Ecowitt's upload protocol has no
   real authentication (PASSKEY is just the gateway's MAC, in cleartext), so an
   open `0.0.0.0` bind would let any host on the LAN forge weather readings.
   `allowed_source_ips` is what buys that protection back — give the gateway a
   static DHCP lease so the entry doesn't go stale.

   Prefer no inbound listener at all? Set `gateway_ip` and the plugin will poll
   the gateway instead, leaving `bind_addr` on loopback.

3. Configure the Ecowitt gateway's "Customized" upload: Protocol=Ecowitt,
   Server=this machine's IP, Path=/data/report/, Port=8888.

   Or skip it: run `set_custom_server` from the plugin's Actions and it pushes
   its own listen URL into the gateway's settings for you.

### In a container

A Docker **bridge** network defeats two of the three paths. Gateway discovery
is a UDP broadcast to `255.255.255.255:45000`, which a bridge does not forward,
and the upload port is inbound — `compose.yml` does not publish `8888`, so
`bind_addr = "0.0.0.0"` alone is not enough there, though it would be on bare
metal. Use host networking, or set `gateway_ip` and let the plugin poll over
ordinary outbound HTTP.

## Notices

Problems are reported as **notices**, shown on the plugin's card in the web
UI. They are state rather than log lines — each clears when the condition
stops being true, so the card reflects the gateway's situation now rather than
what it was at startup.

| Code | Means |
|---|---|
| `receiver_unreachable` | Bound to loopback with no poller configured, so nothing can reach it — the silent failure above. |
| `no_reports_received` | Listening, but no gateway has ever uploaded. |
| `gateway_silent` | Reports were arriving and then stopped. |
