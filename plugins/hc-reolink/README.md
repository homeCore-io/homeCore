# hc-reolink

Reolink cameras, NVRs and Home Hubs in homeCore.

- **Cameras on their own and NVRs or Home Hubs** alike: each address is
  either. A camera with two lenses (a Duo) is a camera per lens, with one
  spotlight and one siren.
- **Cameras** are `camera` devices reporting what they see in homeCore's
  shared names: `motion`, `person_detected`, `vehicle_detected`,
  `animal_detected`, `package_detected`. A doorbell's press is a button press
  (`pushed`), so the button trigger reads it.
- **Spotlights** are `light` devices and **sirens** `siren` devices of their
  own, so cards, rules and scenes treat them like any other.
- **PTZ** cameras have `ptz_move` and `goto_preset` actions; `ir_mode` turns
  infrared on or off.
- **Snapshots** are taken when a detection starts and kept in the plugin's
  files folder (`snap/<camera>-<ms>.jpg`), the newest few per camera.
- **Live video** is go2rtc's: the plugin adds `reolink_<camera>` (sub stream,
  H.264) and `reolink_<camera>_hd` (main stream) to it. It never touches a
  stream without the `reolink_` prefix.

Each address is signed in to once, with one session kept, renewed, and given
back when the plugin stops: a Reolink device allows only a few sessions.
What each camera can do is read from it (`GetAbility`, `GetEvents`), never
guessed.

Detections are polled every `poll_secs` (`GetEvents`, or `GetMdState` and
`GetAiState` on older firmware). Reolink's push on port 9000 comes next.

The protocol is Reolink's HTTP API (`/cgi-bin/api.cgi`). No AGPL code is used;
the Python library reolink_aio (MIT) was the reference for its quirks.
