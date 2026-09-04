# hc-nuheat

NuHeat Signature floor-heating thermostats in [homeCore](https://github.com/homeCore-io/homeCore),
through NuHeat's cloud OpenAPI.

One `thermostat` device per thermostat on the account: floor temperature,
target, mode, and whether it is currently heating. There is no local protocol
to speak — a Signature talks to NuHeat and nothing else — so the cloud *is* the
device here, and everything is polled.

```
src/
├── main.rs      connect → logs → manage → describe → run → poll → consume
├── config.rs    the config core hands you as argv[1], and how it renders
├── auth.rs      OAuth2 against identity.mynuheat.com, and where tokens live
├── api.rs       the NuHeat OpenAPI, from its live swagger documents
├── device.rs    thermostat ⇄ homeCore device: state, schema, commands
├── link.rs      the streaming "Link NuHeat account" action
├── runtime.rs   the poll loop, the command path, and the notices
└── units.rs     NuHeat's integer temperatures ⇄ °C
```

## Signing in — read this before installing

**You supply your own NuHeat API credentials.** Request them from NuHeat
support — <https://api.mynuheat.com/> has the link — and enter the client id
and redirect URI under **NuHeat account** in the plugin's configuration.

This plugin deliberately ships no client id of its own. A client id identifies
*an application* to NuHeat: it is what their rate limits are counted against,
what appears in their logs, and what a user's consent is granted to. Shipping
one would put every homeCore installation behind a single identity that nobody
here controls.

NuHeat's API is OAuth2-only. The session endpoint that older third-party NuHeat
integrations used (`POST /api/authenticate/user`, returning a `SessionId`) no
longer exists — it answers 404.

### Which flow — `[nuheat.auth] mode`

NuHeat decides per client id which grants it may use, so the mode has to match
what they enabled for yours.

- **`oauth`** (default) — authorization code + PKCE, requesting
  `offline_access`. Returns a refresh token (15 days, rolling), so the plugin
  keeps itself signed in and runs unattended. **This is the one to ask NuHeat
  for.** The `state` returned in the redirect is verified. A client secret is
  only needed if they issued you a confidential client; a public client uses
  PKCE alone.
- **`access_token`** — the implicit flow, for a client id that only permits it.
  Returns a **one-hour token with no refresh token**, so you re-paste every
  hour. Useful for a first look; the plugin raises a notice saying as much.

Either way, press **Link NuHeat account**, open the link it shows, sign in, and
paste back the address you land on. The plugin checks the result against
`GET /api/v2/Account` before saying you are linked, so a bad paste fails
immediately rather than showing up later as thermostats that never appear.

### What the identity server actually permits

Worth knowing when NuHeat tells you which flow your client has. Measured
against the live server rather than read off the documentation, which
contradicts itself:

| client | grant | result |
|---|---|---|
| a client with implicit enabled | `token`, `id_token token`, scope `openapi` | accepted |
| the same | implicit **+ `offline_access`** | rejected |
| `swagger` (NuHeat's own docs client) | `code`, hybrid | rejected |
| `swagger` | any redirect_uri but NuHeat's own | rejected |
| `swagger`, `js` | password, device_code | `unauthorized_client` |

The practical consequences: implicit can never give you a refresh token, and a
redirect URI has to be one actually registered against your client id —
`localhost` included, if that is what you register.

### Where the tokens go

**Core's durable learned state** (`homecore/plugins/plugin.nuheat/state`), not
the config file — the same place hc-hue keeps its bridge `app_key`, and for the
same reason: the config file is core-owned and watched, so writing a token into
it would trip the hot-reload watcher and restart the plugin mid-flow. A restart
picks the tokens back up without asking you to sign in again.

## What it publishes

On `homecore/devices/nuheat_<serial>/state`:

| key | |
|---|---|
| `current_temperature` | floor temperature, °C |
| `setpoint` | target, °C |
| `current_temperature_f`, `setpoint_f` | the same two in °F, derived |
| `mode` | `auto`, `hold`, or `permanent_hold` |
| `heating` | whether the relay is on right now |
| `online` | also published as availability |
| `hold_until` | when a temporary hold ends |
| `error_state` | the thermostat's own fault text |

`current_temperature` and `setpoint` are spelled the way hc-thermostat spells
them, so a rule or dashboard written against one works against the other, and
the built-in `thermostat` dashboard widget binds without configuration.

## What it accepts

Attribute writes and action calls both, because the device schema declares
both:

```sh
# Set a target — a temporary hold by default, permanent if configured so
curl -X PATCH localhost:8080/api/v1/devices/nuheat_12345678/state \
  -H 'Content-Type: application/json' -d '{"setpoint": 22.5}'

# Back to the thermostat's own schedule
curl -X PATCH localhost:8080/api/v1/devices/nuheat_12345678/state \
  -H 'Content-Type: application/json' -d '{"mode": "auto"}'

# Hold 24 °C for three hours
curl -X PATCH localhost:8080/api/v1/devices/nuheat_12345678/state \
  -H 'Content-Type: application/json' \
  -d '{"action": "hold_temperature", "temperature": 24, "hours": 3}'
```

`target_temperature` and `temperature` are accepted as spellings of `setpoint`;
`manual` and `resume` as spellings of the modes. A hold longer than 23 hours is
refused *before* it is sent, because NuHeat's own rejection is a bare 400.

Nothing is published from a command directly. The plugin performs the write,
re-reads the thermostat, and publishes what it actually reports — so a command
NuHeat refuses leaves the UI showing what the floor is really doing.

## The floor-covering limit

`max_setpoint` is worth setting. A NuHeat will drive a slab to 30 °C / 86 °F,
and floor coverings generally will not survive it — engineered hardwood is
usually rated to about 27 °C / 80 °F. NuHeat enforces nothing, so without a
limit a rule with a units mistake in it damages the floor rather than tripping
something. Anything above the limit is held at it, and the device schema
advertises the narrowed range so clients draw sliders that stop there.

## A note on temperature units

NuHeat carries temperatures as integer **hundredths of a degree Celsius** —
their documented hold example is `"setPointTemp": 3000`, i.e. 30.00 °C / 86 °F.
Their prose says "1/10 °C" in one place and every worked example contradicts
it, so this is inferred rather than stated.

Being wrong about that would be wrong by a factor of ten, silently. So
`units::decode_celsius` refuses to publish a reading that decodes to something
no floor reaches, and the plugin raises an error notice instead. If NuHeat ever
changes the scale, this plugin stops rather than publishing plausible nonsense.

## Actions

| action | |
|---|---|
| **Link NuHeat account** | streaming; sign in and store the credentials (admin) |
| **Sign out** | forget them; devices stay registered (admin) |
| **Refresh now** | poll immediately instead of waiting out the interval |
| **Show status** | signed in?, token lifetime, thermostat count, setpoint limits |

## Not covered

v1's `EnergyLog` (heating minutes and estimated kWh per hour/day/month) and
`Group` (away mode across several thermostats) are real endpoints this plugin
does not use yet. NuHeat's `Schedule` endpoints are disabled server-side —
every one answers 405 — so schedule editing is not available to any client.

Change notifications exist over SignalR (`/v2/notificationsHost`) and would
replace polling, but there is no mature SignalR client for Rust and NuHeat's
rate limits are generous enough that polling costs nothing.

## Development

```sh
cp config/config.toml.example config/config.toml
cargo run -p hc-nuheat -- config/config.toml
```

`cargo fmt`, `clippy` and `test` at the repo root cover this crate along with
core and the SDK, so there is no per-plugin workflow.

## License

Dual-licensed under **MIT** or **Apache-2.0**, at your option.
