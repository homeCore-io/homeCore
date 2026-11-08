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
  - `MODE=camera` makes it a camera on its own (type IPC, no channel
    list): the PTZ camera, or with `LENSES=2` a two-lens one like a Duo.
    Give each its own `PORT`.
- `cam-go2rtc.yaml`: go2rtc playing the cameras' RTSP on `127.0.0.1:18554`,
  test patterns under Reolink's stream names (needs ffmpeg).
- `house-go2rtc.yaml`: go2rtc on `127.0.0.1:11984`, standing in for the
  house's, which the plugin adds its streams to.

```sh
node standin.mjs &                              # the NVR, 18996
MODE=camera PORT=18997 node standin.mjs &       # a camera on its own
MODE=camera LENSES=2 PORT=18998 node standin.mjs &  # a two-lens camera
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
