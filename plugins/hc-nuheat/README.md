# hc-nuheat

NuHeat Signature floor-heating thermostats in [homeCore](https://github.com/homeCore-io/homeCore),
through NuHeat's cloud OpenAPI.

One `thermostat` device per thermostat on the account: floor temperature,
target, whether it is heating, and whether it follows its schedule. A
Signature talks to NuHeat and nothing else, so the cloud is the device here,
and everything is polled.

```
src/
├── main.rs      connect → logs → manage → describe → run → poll → consume
├── config.rs    the config core hands you as argv[1], and how it renders
├── auth.rs      homeCore's NuHeat client, PKCE, and where tokens live
├── api.rs       the NuHeat OpenAPI, from its live swagger documents
├── device.rs    thermostat ⇄ homeCore device: state, schema, commands
├── link.rs      signing in: the page's actions and a streaming fallback
├── runtime.rs   the poll loop, the command path, notices and alerts
└── units.rs     NuHeat's integer temperatures ⇄ °C
ui/              its plugin page (Svelte, built to ui/dist)
```

## Signing in

Press **Link NuHeat account** on the plugin's page and sign in with your
NuHeat account, the one the NuHeat app uses. You type your password on
NuHeat's own site; homeCore never sees it. Nothing needs configuring first.

The plugin signs in as **homeCore's NuHeat client**, which NuHeat registered
for homeCore in November 2026: authorization code with PKCE, scopes `openid
openapi offline_access`, no client secret. Everyone signs in under it with
their own account, as with Hue Remote or Ecobee. A PKCE client has no secret,
so shipping its id in an open-source plugin is what it is for.

How the browser gets back:

1. The page asks the plugin to start (`begin_link`) and goes to NuHeat.
2. After you sign in, NuHeat sends the browser to
   `https://homecore.io/oauth/nuheat/callback`, a static page that sends it
   straight back to your own homeCore's plugin page with the `code`. The
   address of your homeCore travels in `state`. It goes back by itself only
   to a home-network address (localhost, a private IP, a `.local` or
   single-word name); anywhere else it shows the address and asks first.
3. The page hands the code to the plugin (`complete_link`), which exchanges it
   with its PKCE verifier and reads your account before saying it is linked.

The code is useless without the verifier, which never leaves the plugin, so
passing it through a public page gives nothing away. `state` is random, single
use, good for ten minutes, and checked.

**If the browser can't get back** (say, you sign in from a phone on another
network): under "Can't come back to this page from NuHeat?" the page opens
NuHeat in a new tab using the other registered address,
`http://127.0.0.1:8127/nuheat/callback`. Nothing answers there, so the tab
shows an error; copy its whole address and paste it into the page. The
generic action list's **Link NuHeat account** does the same as a streaming
action.

**NuHeat ends a sign-in 30 days after it is made**, however often it is used
(an absolute refresh-token lifetime). The page says when ("Sign in again by
December 1"), and five days ahead the plugin raises a `relink_due` notice.
Signing in again takes a minute and starts another 30 days.

**Your own client.** If NuHeat issued you a client of your own, set
`[nuheat.auth] client_id` (and `client_secret` for a confidential one, and a
`redirect_uri` registered against it). The homecore.io page is registered only
against homeCore's client.

### Where the tokens go

**Core's durable learned state** (`homecore/plugins/plugin.nuheat/state`), not
the config file, as hc-hue keeps its bridge key. The config file is core-owned
and watched, so writing a token into it would restart the plugin mid-flow. A
restart picks the sign-in back up.

NuHeat's refresh tokens are one-time use: each refresh returns a new one and
retires the old. The plugin refreshes one at a time and keeps the new token
before using it, so a restart never finds a spent one.

## What it publishes

On `homecore/devices/nuheat_<serial>/state`, in the shared names and units
(`hc_types::device_vocabulary`). The app shows each person their own units.

| key | |
|---|---|
| `temperature` | floor temperature, °C |
| `setpoint` | target, °C |
| `hvac_mode` | always `heat`: a floor only heats |
| `hvac_action` | `heating` or `idle`, whether the relay is on now |
| `schedule_mode` | `auto` (following its schedule), `hold` (for now), or `permanent_hold` |
| `hold_until` | when a temporary hold ends |
| `error_state` | the thermostat's own fault text, or null |
| `serial_number` | |

Availability is its own topic: a thermostat NuHeat can't reach is unavailable.

## What it accepts

Attribute writes and action calls both, because the device schema declares
both:

```sh
# Set a target: a temporary hold by default; permanent if the thermostat is
# already on a permanent hold, or if configured so
curl -X PATCH localhost:8080/api/v1/devices/nuheat_12345678/state \
  -H 'Content-Type: application/json' -d '{"setpoint": 22.5}'

# Back to the thermostat's own schedule
curl -X PATCH localhost:8080/api/v1/devices/nuheat_12345678/state \
  -H 'Content-Type: application/json' -d '{"schedule_mode": "auto"}'

# Hold 24 °C for three hours
curl -X PATCH localhost:8080/api/v1/devices/nuheat_12345678/state \
  -H 'Content-Type: application/json' \
  -d '{"action": "hold_temperature", "setpoint": 24, "hours": 3}'
```

`heat_setpoint` and `target_temperature` are accepted as spellings of
`setpoint`, and `mode` (this plugin's name for `schedule_mode` before the
shared names) still works. A hold longer than 23 hours is refused before it is
sent, because NuHeat's own refusal is a bare 400.

Nothing is published from a command directly. The plugin performs the write,
re-reads the thermostat, and publishes what it reports, so a command NuHeat
refuses leaves the app showing what the floor is really doing.

## Alerts and notices

| | kind | |
|---|---|---|
| `thermostat_fault` | alert, error, on by default | the thermostat reports a fault (a floor sensor, most often), in its own words |
| `not_linked` | notice | not signed in, or NuHeat ended the sign-in |
| `relink_due` | notice | the 30 days end within five |
| `auth_rejected` | notice | NuHeat refused the sign-in |
| `api_unreachable`, `rate_limited` | notice | NuHeat's cloud can't be reached, or is slowing the plugin down |
| `no_thermostats` | notice | the account has none, or none match `only_serials` |
| `implausible_reading` | notice | a reading that decodes to no believable temperature (see below) |

## The floor-covering limit

`max_setpoint` is worth setting. A NuHeat will drive a slab to 30 °C / 86 °F,
and floor coverings generally won't survive it: engineered hardwood is
usually rated to about 27 °C / 80 °F. NuHeat enforces nothing, so without a
limit a rule with a units mistake in it damages the floor. Anything above the
limit is held at it, and the device schema advertises the narrowed range so
the app's slider stops there.

## A note on temperature units

NuHeat carries temperatures as integer **hundredths of a degree Celsius**:
their documented hold example is `"setPointTemp": 3000`, 30.00 °C / 86 °F.
The RTI control driver for these thermostats did the same arithmetic against
the live service (`JCtoC` is `input / 100`). The same driver confirms the
schedule modes: 1 Auto, 2 Hold until, 3 Permanent.

Their prose still says "1/10 °C" in one place. Being wrong here would be wrong
by a factor of ten, silently, so `units::decode_celsius` refuses a reading that
decodes to something no floor reaches, and the plugin raises an error notice
instead of publishing it.

## Actions

| action | |
|---|---|
| **Link NuHeat account** | streaming; open NuHeat, paste the address back (admin) |
| `begin_link`, `complete_link` | the plugin page's two steps of signing in (admin) |
| **Sign out** | forget the sign-in; thermostats stay registered (admin) |
| **Refresh now** | poll now instead of waiting out the interval |
| **Show status** | signed in, as whom, until when, and the thermostats (read-only; the page reads it) |

## Its page

`ui/` builds the plugin page (`plugin.page`): whether it is signed in, the
account, the date to sign in again by, the thermostats with their reading,
target and any fault, and Link, Refresh, Sign out and the paste fallback. Its
settings stay the app's generic form below it. Its thermostats have no card of
their own: the app draws them from their schema (`temperature` and `setpoint`
on the card, the target slider and the schedule actions in their details).

## Not covered

v1's `EnergyLog` (heating minutes and estimated kWh per hour/day/month) and
`Group` (away mode across several thermostats) are real endpoints this plugin
doesn't use yet. NuHeat's `Schedule` endpoints are disabled server-side (each
answers 405), so no client can edit a schedule.

Change notifications exist over SignalR (`/v2/notificationsHost`) and would
replace polling, but there is no mature SignalR client for Rust, and NuHeat's
rate limits make polling cost nothing.

## Development

```sh
cp config/config.toml.example config/config.toml
cd ui && npm ci && npm run build && cd ..
cargo run -p hc-nuheat -- config/config.toml
```

To test without NuHeat, point `[nuheat.endpoints]` and
`[nuheat.auth] redirect_uri` at a local stand-in (never the real service, and
never a real house). `cargo fmt`, `clippy` and `test` at the repo root cover
this crate along with core and the SDK.

## License

Dual-licensed under **MIT** or **Apache-2.0**, at your option.
