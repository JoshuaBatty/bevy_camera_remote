# bevy_camera_remote

A small Bevy 0.20 plugin for driving an app's 3D camera from outside, over the
[Bevy Remote Protocol](https://docs.rs/bevy_remote/0.20.0/bevy_remote/) (BRP, JSON-RPC 2.0
over HTTP). The app adds one plugin, and an external tool can then:

- set the pose (eye, target, up for roll), vertical field of view and depth of field;
- hold that state against the app's own camera controls;
- render the camera offscreen at an exact size and get the reply only once the PNG is
  written, with the app moved to a given time first.

It is built so a script or camera-move tool can render a Bevy app frame by frame: one
`camera/capture` per frame, then the PNGs go into a video. The same request always gives
the same bytes.

## App integration

```toml
[dependencies]
bevy = { version = "0.20", features = ["bevy_remote"] }
bevy_camera_remote = { git = "<repository url>" }
```

```rust
use bevy::prelude::*;
use bevy::remote::{RemotePlugin, http::RemoteHttpPlugin};
use bevy_camera_remote::{CameraRemotePlugin, RemoteCamera, RemoteSeek};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins((RemotePlugin::default(), RemoteHttpPlugin::default()))
        .add_plugins(CameraRemotePlugin::default())
        .add_systems(Startup, setup)
        .add_systems(Update, seek)
        .run();
}

fn setup(mut commands: Commands) {
    // RemoteCamera is optional when the app has only one Camera3d.
    commands.spawn((Camera3d::default(), RemoteCamera, Name::new("Main Camera")));
}

// Optional: show the app's content at the time a capture asks for.
fn seek(mut seeks: MessageReader<RemoteSeek>) {
    for seek in seeks.read() {
        // e.g. set the app's playhead to seek.seconds
    }
}
```

The plugin adds its methods to the `RemoteMethods` resource in `Plugin::finish`, once every
plugin is built. That means it works whether `RemotePlugin` is added before or after it, and
the methods share whatever transport and port the app gave BRP (by default
`http://127.0.0.1:15702`). Without `RemotePlugin` it logs a warning and serves nothing. Method
names are also exported as constants in `bevy_camera_remote::methods`.

## Coordinates

All positions are in the protocol's frame: **Z-up, right-handed, metres**. The plugin
converts to Bevy's Y-up frame as `bevy = (x, z, -y)` and back as `(x, -z, y)`. An app whose
scene already uses Bevy's frame for its numbers sets
`CameraRemotePlugin { frame: WorldFrame::YUp }`, and positions then pass through unchanged.
The default `up` is world up in either case.

## Methods

Numbers in replies are `f32` values, so `0.05` comes back as `0.05000000074505806`.

### `camera/set`

```json
{"jsonrpc": "2.0", "id": 1, "method": "camera/set", "params": {
  "camera": "Main Camera",
  "eye": [4.5, 0.05, 1.6], "target": [0.5, 0.05, 0.9], "up": [0, 0.25, 1],
  "vfov_deg": 40.0,
  "dof": {"f_stop": 2.8, "focus_m": 3.6, "sensor_height_mm": 20.25, "max_coc_px": 100, "mode": "bokeh"},
  "hold": true}}
```

| Field | Meaning |
|---|---|
| `camera` | Optional. The camera's `Name`, or the entity number a reply gave. Default: the camera with `RemoteCamera`, else the only `Camera3d`. Ambiguity is an error. |
| `eye`, `target` | Optional, but only together. Without them the pose is unchanged. |
| `up` | Optional, default world up. Only its tilt about the view direction counts, so any other up rolls the camera. It must not be parallel to the view. |
| `vfov_deg` | Optional vertical field of view in degrees. Switches an orthographic camera to perspective. |
| `dof` | An object inserts or replaces `DepthOfField`; `null` removes it; absent leaves it alone. `f_stop` and `focus_m` are required. Defaults: `sensor_height_mm` 20.25 (a 36 mm wide sensor at 16:9), `max_coc_px` 64, `mode` `"bokeh"` (or `"gaussian"`). Each request gives the whole lens; fields are not merged with the previous one. |
| `hold` | Default `true`. The plugin re-applies the camera's state (pose, field of view, depth of field) every frame until `camera/release`, so the app's own camera control cannot drift it. `false` applies once and drops any hold. |

The whole request is checked before anything changes, so a bad request changes nothing.

Reply: the camera's state after the change.

```json
{"camera": "Main Camera",
 "eye": [4.5, 0.05, 1.6], "target": [0.954, 0.05, 0.979], "up": [-0.167, 0.246, 0.955],
 "vfov_deg": 40.0,
 "dof": {"f_stop": 2.8, "focus_m": 3.6, "sensor_height_mm": 20.25, "max_coc_px": 100.0, "mode": "bokeh"},
 "hold": true}
```

- `target` is `eye + forward × distance`. The distance is `focus_m` when depth of field is
  set, otherwise the eye-to-target distance last sent, otherwise 1 m.
- `up` is the camera's own up, square to the view.
- `vfov_deg` is `null` for a camera that is not perspective.

Because of this, a reply can be sent back to `camera/set` as it is, and the camera does not
change.

### `camera/get`

Params `{}`, none, or `{"camera": ...}`. Reply as for `camera/set`.

```sh
curl -s localhost:15702 -d '{"jsonrpc":"2.0","id":1,"method":"camera/get"}'
```

### `camera/release`

Stops holding, and the app's own camera control resumes. Without params it releases every
held camera; `{"camera": ...}` releases one. Reply: `{"hold": false}`.

### `camera/capture`

```json
{"jsonrpc": "2.0", "id": 1, "method": "camera/capture", "params": {
  "path": "/abs/path/f_000123.png", "size": [1920, 1080],
  "camera": {"eye": [6, -1, 1.5], "target": [0, 0, 0.4], "up": [0, 0.3, 1], "vfov_deg": 30, "dof": null},
  "time_s": 2.16, "settle_frames": 3}}
```

| Field | Meaning |
|---|---|
| `path` | Required: an absolute `.png` path whose folder exists. |
| `size` | Optional `[w, h]`, default `[1920, 1080]`. The camera renders offscreen at exactly this size, whatever the window's size. |
| `camera` | Optional: the same fields as `camera/set`, applied first (with hold, unless it says `"hold": false`). |
| `time_s` | Optional. The plugin sends a `RemoteSeek { seconds }` message for the app to show its content at that time. Apps without time can ignore it. |
| `settle_frames` | Optional, default 3, at most 1000: frames to wait after applying, before rendering. |

The plugin works in this order:

1. It applies the camera and sends the seek.
2. It points the camera at a new offscreen image of `size`. This clears any viewport and
   activates the camera, and keeps them so every frame until the shot.
3. It waits `settle_frames`, and also until the renderer has the image and no shader is
   still compiling. This keeps the first capture complete.
4. It takes the image, writes an opaque 8-bit sRGB PNG, and gives the camera back its own
   target, viewport and active flag.
5. Only then does it reply:

```json
{"path": "/abs/path/f_000123.png", "frame": 6299, "size": [1920, 1080]}
```

`frame` is the app's `FrameCount` when the image was taken. It is for logs only.

Send captures one at a time. If a second capture arrives while one is running, it waits its
turn and is not rejected. With the same app state, camera and time, a capture gives the same
bytes.

## Depth of field

- On by default via the `dof` cargo feature, which enables `bevy/bevy_post_process`. Apps
  that do not want post-processing build with `default-features = false`. A `dof` object is
  then refused with an error, and `null` is accepted.
- The component is `DepthOfField { mode, focal_distance: focus_m, aperture_f_stops: f_stop,
  sensor_height: sensor_height_mm / 1000, max_circle_of_confusion_diameter: max_coc_px,
  max_depth: 1000.0 }`. `max_depth` is finite on purpose: Bevy's default of infinity becomes
  `null` in JSON and cannot be sent back through reflection.
- Bevy derives the focal length from the vertical field of view and `sensor_height`. The
  blur therefore follows the thin-lens circle of confusion for that lens, measured on planar
  depth, and is capped at `max_coc_px`.
- **Add `Hdr` to the camera if bright out-of-focus highlights matter.** Without it,
  highlights clip before the blur and bokeh comes out as dull grey discs.
- Depth of field only applies to perspective cameras, and needs depth textures (it is off on
  WebGL2).

## Demo

```sh
cargo run --example demo
```

The demo is a ground plane, a light, and a camera marked `RemoteCamera` with `Hdr`. There
are red, green and blue cubes at (0, 0), (2, 0) and (0, 2), and a white cube that `time_s`
moves along +x at 1 m/s from x = -3. Left alone, the camera slowly orbits the scene, which
stands in for an app's own camera control; a held camera ignores it.

```sh
curl -s localhost:15702 -d '{"jsonrpc":"2.0","id":1,"method":"camera/capture","params":{
  "path":"/tmp/shot.png","camera":{"eye":[6,-1,1.5],"target":[0,0,0.4],"vfov_deg":30,
  "dof":{"f_stop":0.5,"focus_m":4.27}},"time_s":2}}'
```

Ctrl-C in its terminal quits cleanly. So does BRP's own
`{"method":"world.write_message","params":{"message":"bevy_app::app::AppExit","value":"Success"}}`.

## Limits

- **One capture at a time.** Any others queue.
- **Only the driven camera is in the image.** Other cameras that draw to the window, such as
  a UI camera, are not captured.
- **The window loses that camera during a capture**, while it renders offscreen.
- **The hold runs in `PostUpdate`**, in `CameraRemoteSystems`, before transform propagation
  and the camera update. Camera control in `Update` (the usual place) is overridden. Camera
  control that writes the camera in `PostUpdate` must run before the set, or it wins:
  `app.configure_sets(PostUpdate, CameraRemoteSystems.after(MyCameraSystems))`.
- **Parented cameras** are held in world space. The plugin uses the parent's
  `GlobalTransform` from the previous frame, so a moving parent lags one frame.
- **Setting a pose resets the camera's scale to 1.**
- **Time is only as exact as the app's `RemoteSeek` handler.** Content animated by
  wall-clock time, and not by the seek, will differ between captures.
- **Captures run at the app's frame rate.** Each capture takes its settle frames, two
  renderer-ready frames, the GPU readback and the PNG write. On an Apple M5 Max, a debug
  build with optimised dependencies took 125–230 ms per 1920×1080 capture (about 65 ms at
  1280×720) with the window visible, and 250–300 ms with it hidden.
- **BRP has no authentication.** Keep its default loopback address; to reach the app from
  another machine, tunnel over SSH.
- **Desktop only.** BRP's HTTP transport is not available on the web.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed
as above, without any additional terms or conditions.
