# hc-weather

The weather for a place, as a homeCore device: the weather now, today's
figures, seven days and the next 24 hours, and the air, from
[Open-Meteo](https://open-meteo.com/); and the national weather service's
warnings, as homeCore alerts. No account or key.

With no places in its settings it reports **the house**, at the coordinates
homeCore already has (`[location]` in `homecore.toml`, the same place sunrise
and sunset come from). Add a place for anywhere else, by its coordinates, a
postal code and country, or its name.

## What it publishes

One device per place, `weather_<id>` (`weather_home` for the house), of type
`weather`. Nothing is writable. Names and units are homeCore's shared ones,
and Ecowitt's where the app already reads them, so the app's **Weather card**
shows this device like a weather station, with no screen of the plugin's own.

| Attribute | Unit | What |
|---|---|---|
| `temperature`, `feels_like` | °C | now |
| `humidity` | % | now |
| `dewpoint` | °C | now |
| `pressure` | hPa | at sea level |
| `wind_speed`, `wind_gust` | m/s | now |
| `wind_direction` | ° | where the wind comes from |
| `precipitation_mm` | mm | the last 15 minutes |
| `rain_next_hour_mm` | mm | expected in the next hour |
| `rain_starts_in_min` | min | until rain starts, within the next two hours; null if none is expected |
| `rain_soon` | | rain expected within 30 minutes |
| `cloud_cover_pct` | % | now |
| `is_day` | | day or night |
| `condition` | | `clear`, `partly_cloudy`, `cloudy`, `fog`, `drizzle`, `rain`, `freezing_rain`, `snow`, `showers`, `thunderstorm` (each with a label and an icon in the schema) |
| `today_high`, `today_low` | °C | today |
| `rain_chance_pct` | % | today |
| `snow_today_mm` | mm | snow expected today |
| `tonight_low` | °C | the coldest hour from now to 9 tomorrow morning |
| `freeze_tonight` | | `tonight_low` at or below 0 °C |
| `tomorrow_high`, `tomorrow_low` | °C | tomorrow |
| `uvi` | | today's highest UV index |
| `aqi` | | US AQI now |
| `aqi_category` | | `good`, `moderate`, `unhealthy_sensitive`, `unhealthy`, `very_unhealthy`, `hazardous` (the EPA's bands) |
| `pm25_ugm3`, `ozone_ugm3` | µg/m³ | now |
| `forecast` | | seven days, today first (below) |
| `forecast_hourly` | | the next 24 hours, for automations |
| `alert_count` | | warnings in force |
| `alert_level` | | 0 none, 1 advisory, 2 watch, 3 warning |
| `alert_headline` | | the most serious one's headline |

`forecast` is a list of days:

```json
{ "time": "2026-11-02T00:00:00-05:00", "high": 17.1, "low": 8.2,
  "condition": "cloudy", "precipitation_probability": 1, "precipitation_mm": 0.0,
  "snowfall_mm": 0.0, "wind_gust": 5.1 }
```

Each `time` is the place's midnight with that day's offset (from its time
zone, so the days after a clock change are right too), and a day is never
shown under the day before. `snowfall_mm` is snow, not its water; `wind_gust`
is the day's strongest. `forecast_hourly` entries have `time`, `temperature`,
`condition`, `precipitation_probability`, `precipitation_mm` and `wind_gust`.

Only what changed is published. Readings it can't get are null.

## Warnings

In the US they come from the National Weather Service; in Europe from
MeteoAlarm. Each becomes a homeCore alert on the place's device, and clears
by itself when the service withdraws it or it expires.

| Alert | From | Severity | On by default |
|---|---|---|---|
| `weather_warning` | a warning ("Flood Warning", "Tornado Warning"); MeteoAlarm orange or red | error | yes |
| `weather_watch` | a watch ("Flood Watch"); MeteoAlarm yellow | warning | yes |
| `weather_advisory` | an advisory or statement ("Wind Advisory") | info | no |

A warning's kind comes from what the service calls it: the NWS rates a Flood
Watch "Severe", and it is still a watch. Automations can start on, check and
act on these alerts, or use `alert_level`. The plugin's page shows each one in
full, with when it ends and what the service says to do; an NWS update replaces
the message it updates rather than showing both.

`alerts = "auto"` takes a US place's warnings from the NWS (by its country, or
by where it is when no country is given). MeteoAlarm lists warnings by
region, with no shapes to place a point in, so a European place needs its
region named in `meteoalarm_area`, by code (`DE106`) or part of its name
(`Nordfriesland`); without one, it gets none.

The NWS asks every program that reads its warnings for a contact. Set
`contact` to give one; it is sent only to api.weather.gov.

## Actions

| Action | Who | What |
|---|---|---|
| `status` | anyone who can see devices | each place, where it is, whose warnings it gets, and when each reading was last fetched or why it wasn't |
| `refresh` | users | read everything now |

## Notices

On its page in Settings › Plugins, when Open-Meteo or a warning service can't
be reached, a place can't be found, a place's warnings can't be had (MeteoAlarm
without a region), or there are no places and homeCore didn't say where the
house is (a homeCore too old to share its location).

## Settings

Places (each with an id, a name, and coordinates, a postal code and country,
or a name; where its warnings come from; MeteoAlarm's region), the contact
for the NWS, how often each reading is fetched (the weather now every 15
minutes, the forecast every 30, the air every 15, warnings every 2), and the
services' addresses, for a self-hosted Open-Meteo or a stand-in. A postal code
or name is looked up once and kept in `.weather-places.json` beside the
settings file.

## Its screen

A page in Settings › Plugins: each place with the weather there now and any
warnings, when each reading was last fetched, Refresh now, and the data's
credit. Its devices have no card of their own: the app's Weather card draws
them, and their panel is the app's generic one.

## Data

Weather data by [Open-Meteo.com](https://open-meteo.com/), under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Its free service is
for non-commercial use, about 10,000 calls a day; this plugin makes about 240
a day for each place. Warnings: the US National Weather Service (public
domain) and [MeteoAlarm](https://meteoalarm.org/).

## Building

```sh
cargo test -p hc-weather
cd ui && npm ci && npm run build
```
