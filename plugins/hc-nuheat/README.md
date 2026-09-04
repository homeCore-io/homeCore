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

NuHeat's API is **OAuth2-only**. The session endpoint that older third-party
NuHeat integrations used (`POST /api/authenticate/user`, returning a
`SessionId`) no longer exists — it answers 404. Which OAuth2 flow you can use
depends entirely on your client id, and the identity server is stricter than
its own documentation suggests. Measured against the live server:

| client | grant | result |
|---|---|---|
| `swagger` | implicit (`token`, `id_token token`), scope `openapi` | accepted |
| `swagger` | implicit **+ `offline_access`** | rejected |
| `swagger` | `code`, hybrid | rejected |
| `swagger` | any redirect_uri but NuHeat's own | rejected |
| `swagger`, `js` | password, device_code | `unauthorized_client` |

`swagger` is the client id NuHeat's own Swagger UI ships, registered and usable
by anyone. So there are two modes, and the difference between them is whether
this plugin can stay signed in on its own.

### `mode = "access_token"` — works today, expires hourly

No application to NuHeat. Press **Link NuHeat account**, open the link it
shows, sign in, and paste back the URL you land on. The plugin checks the token
against `GET /api/v2/Account` before saying you are linked, so a bad paste
fails immediately rather than showing up later as thermostats that never
appear.

The catch is structural, not an implementation gap: **implicit tokens last one
hour and cannot be renewed.** There is no refresh token to have. This mode is
for evaluating the plugin, and it raises a notice telling you so.

### `mode = "oauth"` — unattended

Ask NuHeat support for a client id ("Request access to the API" on
<https://api.mynuheat.com/>). Set `client_id` and a `redirect_uri` registered
against it, then link the same way — you paste the `code` from the redirect
instead of a token. With `offline_access` the plugin gets a refresh token (15
days, rolling) and keeps itself signed in from then on.

PKCE (S256) is used, and the `state` returned in the redirect is checked
against the one sent. A client secret is only needed if NuHeat issued you a
confidential client.

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
