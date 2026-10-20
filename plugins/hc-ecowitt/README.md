# hc-ecowitt

[![CI](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-ecowitt/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges Ecowitt weather station sensors into HomeCore. Devices are dynamically discovered from incoming data — no manual sensor configuration needed.

## Data ingestion

- **HTTP POST** (primary) — configure the Ecowitt gateway to POST to this plugin
- **HTTP GET polling** (optional) — plugin polls the gateway's `/get_livedata_info` endpoint

## Supported sensors

All sensors discovered from gateway data are auto-registered: the outdoor
station, the indoor sensor, rain gauges (tipping bucket and piezo), WH31
thermo-hygrometers, WN34 probes, soil moisture and soil EC, leaf wetness, leak
detectors, water level, PM2.5, CO2 and lightning.

## Names and units

Both ways of hearing the gateway — its upload and the poll — publish **the same
device ids, the same attribute names and the same units**, so an automation
does not care which one is configured. Readings follow homeCore's shared
device vocabulary and are converted in the plugin, whatever the gateway's
display is set to: **°C, hPa, m/s, mm, km, W/m²**. There is no unit setting;
showing °F or inches is the app's job. A value the gateway cannot read (`--`)
is left out, never published as 0.

| Device id | Attributes |
|---|---|
| `{prefix}_weather` | `temperature`, `humidity`, `dewpoint`, `windchill`, `feels_like`, `heat_index` (°C / %), `pressure` (relative), `pressure_absolute` (hPa), `wind_speed`, `wind_speed_avg10m`, `wind_gust`, `wind_gust_daily_max` (m/s), `wind_direction`, `wind_direction_avg10m` (°), `solar_radiation` (W/m²), `uvi`, `vpd` (kPa), `datetime` |
| `{prefix}_indoor` | `temperature`, `humidity` |
| `{prefix}_rain`, `{prefix}_piezo_rain` | `rain_rate_mm_hr` (mm/h), `rain_event_mm`, `rain_hourly_mm`, `rain_24h_mm`, `rain_daily_mm`, `rain_weekly_mm`, `rain_monthly_mm`, `rain_yearly_mm`, `rain_total_mm`; the piezo gauge also `raining` |
| `{prefix}_temp_{n}` (WH31) | `temperature`, `humidity` |
| `{prefix}_tf_{n}` (WN34 probe) | `temperature` |
| `{prefix}_soil_{n}` (WH51) | `soil_moisture_pct`, `soil_moisture_raw` |
| `{prefix}_ec_{n}` (WH52) | `temperature`, `soil_moisture_pct`, `ec` (µS/cm) |
| `{prefix}_leaf_{n}` | `leaf_wetness_pct` |
| `{prefix}_leak_{n}` | `water_detected` |
| `{prefix}_lds_{n}` (WH54) | `air_gap_mm`, `depth_mm` |
| `{prefix}_pm25_{n}` | `pm25`, `pm25_24h`, `pm25_aqi`, `pm25_24h_aqi` (µg/m³) |
| `{prefix}_co2` | `co2`, `co2_24h` (ppm), `pm25`, `pm10`, …, `temperature`, `humidity` |
| `{prefix}_lightning` | `distance_km`, `strike_count`, `last_strike` |

Plus the battery attributes below. Nothing is writable: Ecowitt gateways are
receivers.

## Battery fields

Ecowitt sensors do **not** report a uniform "battery percentage" — the
semantics depend on the sensor family. hc-ecowitt classifies each
incoming value and publishes only what it really is:

| field            | type    | when                                       |
| ---------------- | ------- | ------------------------------------------ |
| `battery_pct`    | f64 %   | a level sensor (0..=5 or 0..=6), converted |
| `voltage`        | f64 V   | a sensor that reports its cell voltage     |
| `battery_low`    | bool    | whenever classified — true when low        |
| `battery_kind`   | string  | `"binary"`, `"voltage"`, or `"level"`      |
| `battery_raw`    | f64     | a value whose scale is unknown             |

A 0/1 flag publishes only `battery_low` — `1` means "replace me", not "1%".
`battery_low` is what rules should trigger off.

Taxonomy (per Ecowitt's family-by-family spec, cross-checked against
[`aioecowitt`](https://github.com/home-assistant-libs/aioecowitt)):

| kind                   | raw range | low when     | sensors                                                |
| ---------------------- | --------- | ------------ | ------------------------------------------------------ |
| **binary**             | 0 or 1    | `>= 1`       | WH25, WH26, WH65, WH31 (`batt1..8`), PM2.5 ch 5–8      |
| **voltage** (AA-class) | volts     | `< 1.2`      | WH40, WH68, WH51 soil, WN34 soil temp, WN35 leaf, LDS  |
| **voltage** (supercap) | volts     | `< 2.4`      | WH80, WH85, WH90, WS90                                 |
| **level** (0..=5)      | 0 to 5    | `<= 1`       | WH57 lightning, PM2.5 ch 1–4, leak detectors           |
| **level** (0..=6)      | 0 to 6    | `<= 1`       | WH45 / CO2 5-in-1                                      |

The poll (`get_livedata_info`) doesn't always tag the sensor model in its
outdoor / rain blocks — those publish `battery_raw` and omit `battery_low`
and `battery_kind` rather than guess.

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
