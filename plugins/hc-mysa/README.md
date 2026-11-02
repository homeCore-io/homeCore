# hc-mysa

Mysa thermostats in [homeCore](https://github.com/homeCore-io/homeCore),
through the [Mysa Consumer API](https://consumer-api.mysa.cloud/v1/reference).

It signs in as homeCore's Mysa client (issued
2026-11-01; Mysa's consent page names it "homeCore", published by Simplex
Technology). From a home-network address the browser comes back to homeCore by
itself after you sign in; from anywhere else you paste an address back
(below).

One `thermostat` device per thermostat the customer shared: its temperature,
humidity, mode and targets, and whichever of fan, swing, keypad lock and
display brightness the hardware has. Everything is polled; Mysa's API has no
push.

```
src/
├── main.rs      connect → logs → manage → describe → run → poll → consume
├── config.rs    the config core hands you as argv[1], and how it renders
├── auth.rs      homeCore's Mysa client, PKCE, and where tokens live
├── api.rs       the Mysa Consumer API, from its OpenAPI document
├── device.rs    thermostat ⇄ homeCore device: state, schema, commands
├── link.rs      signing in: the page's actions and a streaming fallback
├── runtime.rs   the poll loop, the command path, and the notices
└── units.rs     landing on the values a thermostat accepts
ui/              its plugin page (Svelte, built to ui/dist)
```

## Signing in

Press **Link Mysa account** on the plugin's page, sign in with your Mysa
account and choose **Allow**. You type your password on Mysa's own site;
homeCore never sees it.

The plugin uses OAuth 2.0 authorization code with PKCE (S256) and no client
secret, as Mysa requires, and asks for:

| Scope | For |
|---|---|
| `consumer-api/devices.read` | thermostats, what they can do, their state |
| `consumer-api/devices.control` | mode, targets, fan, swing, keypad lock, display |
| `consumer-api/schedules.read` | reading schedules |
| `consumer-api/schedules.write` | hold and follow-the-schedule (it never edits a schedule program) |
| `consumer-api/energy.read` | energy history (not shown yet) |

When the plugin's page is open at a home-network address, the browser comes
back by itself:

1. The page asks the plugin to start (`begin_link`) and goes to Mysa.
2. After you sign in, Mysa sends the browser to
   `https://homecore.io/oauth/mysa/callback`, a static page that sends it
   straight back to your own homeCore's plugin page with the `code`. The
   address of your homeCore travels in `state`. It forwards only to a
   home-network address (localhost, a private IPv4, an IPv6 unique local
   address, or a `.local`, `.home.arpa` or `.internal` name), and refuses
   anything else; the plugin won't put any other address in `state` either.
   Otherwise someone could start a sign-in with their own verifier and have
   a customer's code sent to them.
3. The page hands the code to the plugin (`complete_link`), which exchanges
   it with its PKCE verifier and lists your thermostats before saying it is
   linked. Mysa allows 60 seconds for this.

The code is useless without the verifier, which never leaves the plugin.
`state` is random, single use, good for ten minutes, and checked.

A homeCore reached any other way (a public name, a VPN address) signs in by
pasting. Link Mysa account opens Mysa in a new tab; after you choose Allow,
Mysa sends that tab to `http://127.0.0.1:8127/mysa/callback`, where nothing
answers. Copy the whole address from the address bar and paste it into the
page within a minute.

Mysa registered both return pages after reviewing them (2026-11-02), and
asked to be told before either page changes how it handles the code or where
it sends the browser.

**Mysa ends a sign-in 30 days after it is made**, however often it is used.
The page shows the date, and the plugin raises a notice five days before.
Refreshing doesn't give a new refresh token or extend it (Mysa confirmed
this, 2026-11-01). Signing in again anywhere retires the previous sign-in, so
two homeCores on one Mysa account sign each other out.

Tokens live in homeCore's own plugin state, never in the config file, and are
never logged.

## What it publishes

The shared names (`hc_types::device_vocabulary`), so the app's thermostat card
draws it with no plugin code: `temperature`, `humidity`, `hvac_mode`,
`heat_setpoint` and `cool_setpoint` in °C. In auto they are auto's band. Also
`fan_mode`, `fan_speed`, `swing_vertical`, `swing_horizontal`,
`keypad_lockout`, `display_brightness` and `display_idle_brightness` where the
hardware has them, and `model`.

Mysa doesn't report whether a thermostat is heating right now or whether it is
holding, so `hvac_action` and `schedule_mode` are not published.

Every control comes from `GET /v1/capabilities`. Mysa accepts only the exact
setpoints it lists for each thermostat, so every target is moved onto that
list before it is sent; a press of + or − always moves at least one step.
Setting a heat target switches a thermostat to heat, and a cool target to
cool, as Mysa does.

Actions: **Hold until the next change** and **Follow the schedule**, for
thermostats with a schedule.

## Limits

Mysa's limits, per 60 seconds (as Mysa gave them, 2026-11-01): per customer,
300 reads and 30 writes, at most 15 of the writes through any one app; per
app across all customers, 6,000 reads and 180 writes. One `POST
/v1/state/batch` a minute per home is what Mysa expects; a batch takes up to
100 thermostats and counts as one request.

So one poll a minute lists the thermostats and reads every state in one
batch, and capabilities are read again every six hours. Writes are at most 12
a minute across the account (a scene that changes every thermostat is spread
over the minute rather than refused), at least 1.5 s apart to one thermostat,
and commands queued while one is out are merged. A `429` or `503` stops
everything for its `Retry-After`. Mysa has no push or webhook, so polling is
the supported way.

## Testing

`cargo test -p hc-mysa` runs against answers shaped as Mysa's OpenAPI document
describes them; Mysa publishes no examples. For a sandbox, point
`[mysa.endpoints]` and `[mysa.auth]` at a local stand-in and give it a client
id. Never test against the real house.

## Licence

MIT or Apache-2.0, at your option. Mysa is a trademark of Empowered Homes Inc.
