//! Drive a Bevy app's 3D camera from outside over the Bevy Remote Protocol (BRP):
//! pose, roll, field of view and depth of field, and frame-exact PNG capture that
//! replies only once the file is saved.
//!
//! Add [`CameraRemotePlugin`] next to Bevy's `RemotePlugin` and `RemoteHttpPlugin`; it
//! serves four JSON-RPC methods on the same server (by default `http://127.0.0.1:15702`):
//!
//! - `camera/set`: set any of `eye`, `target`, `up`, `vfov_deg` and `dof`, and hold the
//!   camera there (the default) so the app's own camera control cannot drift it.
//! - `camera/get`: the camera's state, in the same shape as the `camera/set` reply.
//! - `camera/release`: stop holding; the app's own camera control resumes.
//! - `camera/capture`: apply a camera and a time, render offscreen at an exact size,
//!   write a PNG, then reply.
//!
//! Positions are Z-up, right-handed metres unless the plugin's [`WorldFrame`] says
//! otherwise. See the README for the full protocol.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy::remote::{RemotePlugin, http::RemoteHttpPlugin};
//! use bevy_camera_remote::{CameraRemotePlugin, RemoteCamera, RemoteSeek};
//!
//! App::new()
//!     .add_plugins(DefaultPlugins)
//!     .add_plugins((RemotePlugin::default(), RemoteHttpPlugin::default()))
//!     .add_plugins(CameraRemotePlugin::default())
//!     .add_systems(Startup, |mut commands: Commands| {
//!         commands.spawn((Camera3d::default(), RemoteCamera, Name::new("Main Camera")));
//!     })
//!     .add_systems(Update, |mut seeks: MessageReader<RemoteSeek>| {
//!         for seek in seeks.read() {
//!             // Show the app's content at `seek.seconds`.
//!         }
//!     })
//!     .run();
//! ```

mod camera;
mod capture;
mod pose;

pub use pose::WorldFrame;

use bevy::camera::CameraUpdateSystems;
use bevy::prelude::*;
use bevy::remote::{BrpError, BrpResult, RemoteMethodSystemId, RemoteMethods, error_codes};
use bevy::render::{ExtractSchedule, RenderApp};
use bevy::transform::TransformSystems;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Method names, as clients call them.
pub mod methods {
    /// Set the camera; replies with its state.
    pub const SET: &str = "camera/set";
    /// The camera's state.
    pub const GET: &str = "camera/get";
    /// Stop holding the camera.
    pub const RELEASE: &str = "camera/release";
    /// Render the camera to a PNG; replies once it is written.
    pub const CAPTURE: &str = "camera/capture";
}

/// Serves the `camera/*` methods on the app's Bevy Remote Protocol server.
///
/// The app adds `RemotePlugin` (and a transport such as `RemoteHttpPlugin`) itself, in
/// any order relative to this plugin: the methods are registered once every plugin is
/// built, in the `RemoteMethods` resource that `RemotePlugin` creates.
#[derive(Clone, Copy, Debug, Default)]
pub struct CameraRemotePlugin {
    /// The frame positions are given in. Default: the protocol's Z-up frame.
    pub frame: WorldFrame,
}

/// Marks the camera the methods drive when a request names none. Without a marked
/// camera they drive the app's only `Camera3d`.
#[derive(Component, Reflect, Clone, Copy, Debug, Default)]
#[reflect(Component, Default)]
pub struct RemoteCamera;

/// Sent by `camera/capture` when a request gives `time_s`. Apps with content that changes
/// over time read it and show that content at `seconds`; others can ignore it. The
/// capture waits its `settle_frames` after sending it, so a seek may take that long.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct RemoteSeek {
    /// Seconds into the app's timeline.
    pub seconds: f64,
}

impl Plugin for CameraRemotePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.frame)
            .init_resource::<capture::Capture>()
            .add_message::<RemoteSeek>()
            .register_type::<RemoteCamera>()
            .add_systems(First, capture::tick)
            .add_systems(
                PostUpdate,
                hold_systems()
                    .before(TransformSystems::Propagate)
                    .before(CameraUpdateSystems),
            )
            .add_systems(Last, capture::advance);
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.add_systems(ExtractSchedule, capture::probe);
        }
        let world = app.world_mut();
        if !world.contains_resource::<RemoteMethods>() {
            warn!("CameraRemotePlugin needs RemotePlugin; the camera methods are not served");
            return;
        }
        let set = world.register_system(camera::set);
        let get = world.register_system(camera::get);
        let release = world.register_system(camera::release);
        let capture = world.register_system(capture::capture);
        let mut remote = world.resource_mut::<RemoteMethods>();
        remote.insert(methods::SET, RemoteMethodSystemId::Instant(set));
        remote.insert(methods::GET, RemoteMethodSystemId::Instant(get));
        remote.insert(methods::RELEASE, RemoteMethodSystemId::Instant(release));
        remote.insert(methods::CAPTURE, RemoteMethodSystemId::Watching(capture));
    }
}

#[cfg(feature = "dof")]
fn hold_systems() -> bevy::ecs::schedule::ScheduleConfigs<bevy::ecs::system::ScheduleSystem> {
    (camera::hold, camera::hold_dof).into_configs()
}

#[cfg(not(feature = "dof"))]
fn hold_systems() -> bevy::ecs::schedule::ScheduleConfigs<bevy::ecs::system::ScheduleSystem> {
    camera::hold.into_configs()
}

/// Reads JSON-RPC params as `T`; no params reads as `{}`.
fn parse<T: DeserializeOwned>(params: Option<Value>) -> BrpResult<T> {
    bevy::remote::builtin_methods::parse(params.unwrap_or_else(|| Value::Object(default())))
}

/// An invalid-params error with this message.
fn invalid(message: impl Into<String>) -> BrpError {
    BrpError {
        code: error_codes::INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::remote::RemotePlugin;
    use serde_json::json;

    fn app(frame: WorldFrame) -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            CameraRemotePlugin { frame },
            RemotePlugin::default(),
        ));
        app.finish();
        app.cleanup();
        app
    }

    fn call(app: &mut App, method: &str, params: Value) -> BrpResult {
        let id = *app.world().resource::<RemoteMethods>().get(method).unwrap();
        match id {
            RemoteMethodSystemId::Instant(id) => {
                app.world_mut().run_system_with(id, Some(params)).unwrap()
            }
            RemoteMethodSystemId::Watching(id) => app
                .world_mut()
                .run_system_with(id, Some(params))
                .unwrap()
                .map(|v| v.unwrap_or(Value::Null)),
        }
    }

    fn vec(v: &Value) -> Vec3 {
        let a: [f32; 3] = serde_json::from_value(v.clone()).unwrap();
        Vec3::from(a)
    }

    fn near(a: &Value, b: [f32; 3]) -> bool {
        vec(a).distance(Vec3::from(b)) < 1e-5
    }

    fn camera(app: &mut App, name: &str) -> Entity {
        app.world_mut()
            .spawn((
                Camera3d::default(),
                RemoteCamera,
                Name::new(name.to_owned()),
            ))
            .id()
    }

    /// The protocol's example `camera/set`, without depth of field when it is not built.
    fn shot() -> Value {
        let mut shot = json!({
            "eye": [4.5, 0.05, 1.6], "target": [0.5, 0.05, 0.9], "up": [0, 0, 1],
            "vfov_deg": 40.0,
        });
        if cfg!(feature = "dof") {
            shot["dof"] = json!({"f_stop": 2.8, "focus_m": 3.6, "sensor_height_mm": 20.25,
                                 "max_coc_px": 100, "mode": "bokeh"});
        }
        shot
    }

    #[test]
    fn methods_are_registered() {
        let app = app(WorldFrame::ZUp);
        let methods = app.world().resource::<RemoteMethods>();
        for m in [
            methods::SET,
            methods::GET,
            methods::RELEASE,
            methods::CAPTURE,
        ] {
            assert!(methods.get(m).is_some(), "{m}");
        }
    }

    #[test]
    fn set_replies_with_the_state_and_get_round_trips() {
        let mut app = app(WorldFrame::ZUp);
        let entity = camera(&mut app, "Main Camera");
        let reply = call(&mut app, methods::SET, shot()).unwrap();
        assert_eq!(reply["camera"], "Main Camera");
        assert!(near(&reply["eye"], [4.5, 0.05, 1.6]));
        // The target lies along the view, at the focus distance when there is depth of field.
        let (eye, target) = (vec(&reply["eye"]), vec(&reply["target"]));
        let sent = Vec3::new(0.5, 0.05, 0.9);
        assert!(
            (target - eye)
                .normalize()
                .distance((sent - eye).normalize())
                < 1e-5
        );
        let distance = if cfg!(feature = "dof") {
            3.6
        } else {
            sent.distance(eye)
        };
        assert!((target.distance(eye) - distance).abs() < 1e-5);
        assert!((reply["vfov_deg"].as_f64().unwrap() - 40.0).abs() < 1e-4);
        if cfg!(feature = "dof") {
            assert_eq!(reply["dof"]["mode"], "bokeh");
        }
        assert_eq!(reply["hold"], true);
        // Bevy's frame is Y-up: the eye's height is its y.
        let t = app.world().get::<Transform>(entity).unwrap().translation;
        assert!(t.distance(Vec3::new(4.5, 1.6, -0.05)) < 1e-6);
        // A reply sent back as a request changes nothing.
        let got = call(&mut app, methods::GET, json!({})).unwrap();
        assert_eq!(got, reply);
        let again = call(&mut app, methods::SET, got.clone()).unwrap();
        for key in ["eye", "target", "up"] {
            assert!(vec(&again[key]).distance(vec(&got[key])) < 1e-5, "{key}");
        }
        assert_eq!(again["dof"], got["dof"]);
    }

    #[test]
    fn up_rolls_and_y_up_passes_through() {
        let mut app = app(WorldFrame::YUp);
        camera(&mut app, "c");
        let up = [0.5f32.sin(), 0.5f32.cos(), 0.0];
        let reply = call(
            &mut app,
            methods::SET,
            json!({"eye": [0, 1, 5], "target": [0, 1, 0], "up": up}),
        )
        .unwrap();
        assert!(near(&reply["eye"], [0.0, 1.0, 5.0]));
        assert!(near(&reply["up"], up));
        assert!(near(&reply["target"], [0.0, 1.0, 0.0]));
    }

    #[test]
    fn hold_beats_the_app_until_released() {
        let mut app = app(WorldFrame::ZUp);
        let entity = camera(&mut app, "c");
        call(&mut app, methods::SET, shot()).unwrap();
        let drift = Transform::from_xyz(9.0, 9.0, 9.0);
        *app.world_mut().get_mut::<Transform>(entity).unwrap() = drift;
        app.update();
        let got = call(&mut app, methods::GET, json!({})).unwrap();
        assert!(near(&got["eye"], [4.5, 0.05, 1.6]));
        let t = app.world().get::<Transform>(entity).unwrap().translation;
        assert!(t.distance(Vec3::new(4.5, 1.6, -0.05)) < 1e-6);

        let released = call(&mut app, methods::RELEASE, json!({})).unwrap();
        assert_eq!(released, json!({"hold": false}));
        *app.world_mut().get_mut::<Transform>(entity).unwrap() = drift;
        app.update();
        let got = call(&mut app, methods::GET, json!({})).unwrap();
        assert!(near(&got["eye"], [9.0, -9.0, 9.0]));
        assert_eq!(got["hold"], false);
    }

    #[test]
    fn dof_is_kept_unless_null() {
        let mut app = app(WorldFrame::ZUp);
        camera(&mut app, "c");
        call(&mut app, methods::SET, shot()).unwrap();
        let kept = call(&mut app, methods::SET, json!({"vfov_deg": 30})).unwrap();
        assert!(kept["dof"].is_object() == cfg!(feature = "dof"));
        let removed = call(&mut app, methods::SET, json!({"dof": null})).unwrap();
        assert!(removed["dof"].is_null());
        // Without depth of field the target is the one sent.
        assert!(near(&removed["target"], [0.5, 0.05, 0.9]));
    }

    #[cfg(not(feature = "dof"))]
    #[test]
    fn dof_needs_the_feature() {
        let mut app = app(WorldFrame::ZUp);
        camera(&mut app, "c");
        let dof = json!({"dof": {"f_stop": 2.8, "focus_m": 3.6}});
        assert!(
            call(&mut app, methods::SET, dof)
                .unwrap_err()
                .message
                .contains("dof")
        );
        assert!(call(&mut app, methods::SET, json!({"dof": null})).is_ok());
    }

    #[test]
    fn bad_requests_are_refused_without_changes() {
        let mut app = app(WorldFrame::ZUp);
        let entity = camera(&mut app, "c");
        let before = *app.world().get::<Transform>(entity).unwrap();
        for params in [
            json!({"eye": [1, 2, 3]}),
            json!({"up": [0, 0, 1]}),
            json!({"eye": [0, 0, 5], "target": [0, 0, 0], "up": [0, 0, 1]}),
            json!({"eye": [1, 0, 0], "target": [1, 0, 0]}),
            json!({"vfov_deg": 180}),
            json!({"eye": [1, 0, 0], "target": [0, 0, 0], "dof": {"f_stop": 0, "focus_m": 1}}),
            json!({"camera": "nobody"}),
            json!({"zoom": 2}),
        ] {
            assert!(
                call(&mut app, methods::SET, params.clone()).is_err(),
                "{params}"
            );
        }
        assert_eq!(*app.world().get::<Transform>(entity).unwrap(), before);
    }

    #[test]
    fn cameras_are_found_by_marker_name_or_entity() {
        let mut app = app(WorldFrame::ZUp);
        let a = app
            .world_mut()
            .spawn((Camera3d::default(), Name::new("a")))
            .id();
        app.world_mut().spawn((Camera3d::default(), Name::new("b")));
        // Two cameras and no marker: ambiguous.
        assert!(call(&mut app, methods::GET, json!({})).is_err());
        assert_eq!(
            call(&mut app, methods::GET, json!({"camera": "b"})).unwrap()["camera"],
            "b"
        );
        let by_entity = json!({"camera": a.to_bits()});
        assert_eq!(
            call(&mut app, methods::GET, by_entity).unwrap()["camera"],
            "a"
        );
        app.world_mut().entity_mut(a).insert(RemoteCamera);
        assert_eq!(
            call(&mut app, methods::GET, json!({})).unwrap()["camera"],
            "a"
        );
    }

    #[test]
    fn capture_without_a_renderer_is_an_error() {
        let mut app = app(WorldFrame::ZUp);
        camera(&mut app, "c");
        let path = std::env::temp_dir().join("bevy_camera_remote_test.png");
        let reply = call(&mut app, methods::CAPTURE, json!({"path": path}));
        assert!(reply.unwrap_err().message.contains("renderer"));
        let relative = call(&mut app, methods::CAPTURE, json!({"path": "x.png"}));
        assert!(relative.unwrap_err().message.contains("absolute"));
    }
}
