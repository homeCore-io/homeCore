# The Reolink stand-in

For trying hc-reolink on a sandbox homeCore without a camera: a stand-in NVR
with two cameras behind one address, answering Reolink's HTTP API in its
shapes, and two go2rtc instances.

- `standin.mjs` (Node): the NVR on `127.0.0.1:18996`, user `admin`, password
  `standin-pass`. Channel 1 is a doorbell (person, package, the button);
  channel 2 a PTZ camera with spotlight, siren, infrared and presets. It
  limits sessions to four and locks after five refused sign-ins, as a real
  one does.
  - `POST /standin/set {"channel": 1, "md": 1, "people": 1}`: what a camera
    sees (`md`, `people`, `vehicle`, `dog_cat`, `package`, `visitor`,
    `online`).
  - `GET /standin/log`: the commands it was sent.
  - `GET /standin/siren`: each channel's siren, sounding or not and how
    (`manul` or `times`). It takes `AudioAlarmPlay` as a Reolink does, and
    pushes a `SirenStatus` when one starts or stops. `SIREN_PLAY_MS` is one
    "times" play (5000); `SIREN_PERMIT=4` is an account that may only watch
    (no siren offered); `SIREN_MANUAL=0` refuses a manual play, as some
    firmware does. A siren test against it:
    `PORT=18980 PUSH_PORT=18981 SIREN_PLAY_MS=300 node standin.mjs`, then
    `REOLINK_STANDIN=127.0.0.1:18980 REOLINK_STANDIN_PUSH=18981 cargo test -p hc-reolink -- --ignored standin`.
  - `MODE=camera` makes it a camera on its own (type IPC, no channel
    list): the PTZ camera. `LENSES=2` makes it report two channels, as a
    Duo may; the plugin should still show one camera.
    Give each its own `PORT`.
- `cam-go2rtc.yaml`: go2rtc playing the cameras' RTSP on `127.0.0.1:18554`,
  test patterns under Reolink's stream names (needs ffmpeg).
- `house-go2rtc.yaml`: go2rtc on `127.0.0.1:11984`, standing in for the
  house's, which the plugin adds its streams to.

```sh
node standin.mjs &                              # the NVR, 18996
MODE=camera PORT=18997 node standin.mjs &       # a camera on its own
MODE=camera LENSES=2 PORT=18998 node standin.mjs &  # reports two channels, like a Duo
go2rtc -config cam-go2rtc.yaml &
go2rtc -config house-go2rtc.yaml &
```

And the plugin's config on the sandbox:

```toml
[reolink.go2rtc]
url = "http://127.0.0.1:11984"

[[reolink.hosts]]
id        = "nvr"
address   = "127.0.0.1:18996"
password  = "standin-pass"
https     = false
rtsp_port = 18554
```
