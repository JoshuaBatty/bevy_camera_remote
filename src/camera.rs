//! `camera/set`, `camera/get` and `camera/release`, and the hold that keeps a set
//! camera where it was put.

use crate::pose::{WorldFrame, look, unlook};
use crate::{RemoteCamera, invalid, parse};
use bevy::prelude::*;
use bevy::remote::BrpResult;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

#[cfg(feature = "dof")]
use bevy::post_process::dof::{DepthOfField, DepthOfFieldMode};

/// The reply's `target` distance for a camera that was never sent one and has no depth
/// of field: the target is then the point one metre ahead.
const DEFAULT_DISTANCE: f32 = 1.0;

/// Which camera a request is for: its `Name`, or its entity bits as a reply gives them.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub(crate) enum CameraRef {
    Name(String),
    Entity(u64),
}

/// `camera/set` params, and the `camera` object of `camera/capture`.
#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetParams {
    camera: Option<CameraRef>,
    eye: Option<[f32; 3]>,
    target: Option<[f32; 3]>,
    up: Option<[f32; 3]>,
    vfov_deg: Option<f32>,
    /// Absent: unchanged. `null`: removed. An object: inserted or replaced.
    #[serde(default, deserialize_with = "present")]
    dof: Option<Option<DofParams>>,
    hold: Option<bool>,
}

/// A lens for depth of field. Every request gives the whole lens, so the same request
/// always gives the same picture; omitted fields take these defaults.
#[derive(Deserialize, Clone, Copy, Debug)]
#[cfg_attr(not(feature = "dof"), allow(dead_code))]
#[serde(deny_unknown_fields)]
struct DofParams {
    f_stop: f32,
    focus_m: f32,
    /// 20.25 mm: a 36 mm wide sensor at 16:9.
    #[serde(default = "sensor_height_mm")]
    sensor_height_mm: f32,
    #[serde(default = "max_coc_px")]
    max_coc_px: f32,
    #[serde(default)]
    mode: Mode,
}

fn sensor_height_mm() -> f32 {
    20.25
}

fn max_coc_px() -> f32 {
    64.0
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(not(feature = "dof"), allow(dead_code))]
enum Mode {
    #[default]
    Bokeh,
    Gaussian,
}

/// Reads a field that is present, `null` included, as `Some`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// `camera/get` and `camera/release` params.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Which {
    camera: Option<CameraRef>,
}

/// Keeps a camera as the remote left it, re-applied every frame until released, so the
/// app's own camera control cannot drift it.
#[derive(Component, Clone)]
pub(crate) struct Hold {
    /// World-space pose in Bevy's frame.
    pose: Transform,
    /// Vertical field of view in radians, for a perspective camera.
    fov: Option<f32>,
    #[cfg(feature = "dof")]
    dof: Option<DepthOfField>,
}

/// The target distance last sent with a pose.
#[derive(Component, Clone, Copy)]
struct Focus(f32);

/// A checked `camera/set`, ready to apply.
struct Change {
    /// World pose in Bevy's frame, and the distance to the target that was sent.
    pose: Option<(Transform, f32)>,
    fov: Option<f32>,
    #[cfg(feature = "dof")]
    dof: Option<Option<DepthOfField>>,
    hold: bool,
}

impl Change {
    fn new(p: &SetParams, frame: WorldFrame) -> BrpResult<Self> {
        let pose = match (p.eye, p.target) {
            (Some(eye), Some(target)) => {
                let (eye, target) = (Vec3::from(eye), Vec3::from(target));
                let up = p.up.map_or(Vec3::Y, |up| frame.to_bevy(up.into()));
                let (eye, target) = (frame.to_bevy(eye), frame.to_bevy(target));
                Some((
                    look(eye, target, up).map_err(invalid)?,
                    eye.distance(target),
                ))
            }
            (None, None) if p.up.is_some() => return Err(invalid("up needs eye and target")),
            (None, None) => None,
            _ => return Err(invalid("eye and target go together")),
        };
        let fov = match p.vfov_deg {
            Some(deg) if deg.is_finite() && deg > 0.0 && deg < 180.0 => Some(deg.to_radians()),
            Some(_) => return Err(invalid("vfov_deg must be between 0 and 180")),
            None => None,
        };
        #[cfg(not(feature = "dof"))]
        if let Some(Some(_)) = p.dof {
            return Err(invalid(
                "this app was built without depth of field (the `dof` feature)",
            ));
        }
        Ok(Self {
            pose,
            fov,
            #[cfg(feature = "dof")]
            dof: p.dof.map(dof).transpose()?,
            hold: p.hold.unwrap_or(true),
        })
    }
}

#[cfg(feature = "dof")]
fn dof(p: Option<DofParams>) -> BrpResult<Option<DepthOfField>> {
    let Some(p) = p else { return Ok(None) };
    let positive = |v: f32| v.is_finite() && v > 0.0;
    if !(positive(p.f_stop)
        && positive(p.focus_m)
        && positive(p.sensor_height_mm)
        && positive(p.max_coc_px))
    {
        return Err(invalid(
            "f_stop, focus_m, sensor_height_mm and max_coc_px must be above zero",
        ));
    }
    Ok(Some(DepthOfField {
        mode: match p.mode {
            Mode::Bokeh => DepthOfFieldMode::Bokeh,
            Mode::Gaussian => DepthOfFieldMode::Gaussian,
        },
        focal_distance: p.focus_m,
        aperture_f_stops: p.f_stop,
        sensor_height: p.sensor_height_mm / 1000.0,
        max_circle_of_confusion_diameter: p.max_coc_px,
        // Finite, unlike the default, so the component survives a reflection round trip.
        max_depth: 1000.0,
    }))
}

/// `camera/set`: changes the given parts of the camera, then replies with all of it.
pub(crate) fn set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let entity = apply(world, parse(params)?)?;
    Ok(state(world, entity))
}

/// `camera/get`: the camera's state.
pub(crate) fn get(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let Which { camera } = parse(params)?;
    let entity = resolve(world, camera.as_ref())?;
    Ok(state(world, entity))
}

/// `camera/release`: stops holding the named camera, or every held camera.
pub(crate) fn release(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let Which { camera } = parse(params)?;
    let held: Vec<Entity> = match camera {
        Some(camera) => vec![resolve(world, Some(&camera))?],
        None => world
            .query_filtered::<Entity, With<Hold>>()
            .iter(world)
            .collect(),
    };
    for entity in held {
        world.entity_mut(entity).remove::<Hold>();
    }
    Ok(json!({ "hold": false }))
}

/// Checks a change, then applies it to its camera; nothing changes if it fails.
pub(crate) fn apply(world: &mut World, params: SetParams) -> BrpResult<Entity> {
    let change = Change::new(&params, *world.resource::<WorldFrame>())?;
    let entity = resolve(world, params.camera.as_ref())?;
    let parent = parent_transform(world, entity);
    let mut camera = world.entity_mut(entity);
    if let Some((pose, distance)) = change.pose {
        let local = parent.map_or(pose, |p| GlobalTransform::from(pose).reparented_to(&p));
        if let Some(mut transform) = camera.get_mut::<Transform>() {
            transform.set_if_neq(local);
        }
        camera.insert(Focus(distance));
    }
    if let (Some(fov), Some(mut projection)) = (change.fov, camera.get_mut::<Projection>()) {
        set_fov(&mut projection, fov);
    }
    #[cfg(feature = "dof")]
    match change.dof {
        Some(Some(dof)) => {
            camera.insert(dof);
        }
        Some(None) => {
            camera.remove::<DepthOfField>();
        }
        None => {}
    }
    if change.hold {
        let pose = change
            .pose
            .map_or_else(|| world_pose(world, entity), |(pose, _)| pose);
        let fov = match world.get::<Projection>(entity) {
            Some(Projection::Perspective(p)) => Some(p.fov),
            _ => None,
        };
        let hold = Hold {
            pose,
            fov,
            #[cfg(feature = "dof")]
            dof: world.get::<DepthOfField>(entity).copied(),
        };
        world.entity_mut(entity).insert(hold);
    } else {
        world.entity_mut(entity).remove::<Hold>();
    }
    Ok(entity)
}

/// The camera a request names, else the one marked [`RemoteCamera`], else the only 3D camera.
pub(crate) fn resolve(world: &mut World, which: Option<&CameraRef>) -> BrpResult<Entity> {
    let one = |found: Vec<Entity>, none: String, many: String| match found[..] {
        [entity] => Ok(entity),
        [] => Err(invalid(none)),
        _ => Err(invalid(many)),
    };
    match which {
        Some(CameraRef::Name(name)) => {
            let found = world
                .query_filtered::<(Entity, &Name), With<Camera3d>>()
                .iter(world)
                .filter(|(_, n)| n.as_str() == name)
                .map(|(e, _)| e)
                .collect();
            one(
                found,
                format!("No 3D camera is named {name:?}"),
                format!("Several 3D cameras are named {name:?}"),
            )
        }
        Some(CameraRef::Entity(bits)) => Entity::try_from_bits(*bits)
            .filter(|&e| world.get::<Camera3d>(e).is_some())
            .ok_or_else(|| invalid(format!("Entity {bits} is not a 3D camera"))),
        None => {
            let marked: Vec<Entity> = world
                .query_filtered::<Entity, (With<RemoteCamera>, With<Camera3d>)>()
                .iter(world)
                .collect();
            match marked[..] {
                [entity] => Ok(entity),
                [] => {
                    let all = world
                        .query_filtered::<Entity, With<Camera3d>>()
                        .iter(world)
                        .collect();
                    one(
                        all,
                        "No 3D camera".into(),
                        "Several 3D cameras: mark one with RemoteCamera or name it".into(),
                    )
                }
                _ => Err(invalid("Several cameras have RemoteCamera: name one")),
            }
        }
    }
}

/// The camera's state in the protocol's frame, as every camera method replies.
pub(crate) fn state(world: &World, entity: Entity) -> Value {
    let frame = *world.resource::<WorldFrame>();
    let hold = world.get::<Hold>(entity);
    let pose = hold.map_or_else(|| world_pose(world, entity), |h| h.pose);
    let fov = match world.get::<Projection>(entity) {
        Some(Projection::Perspective(p)) => Some(p.fov.to_degrees()),
        _ => None,
    };
    #[cfg(feature = "dof")]
    let (dof, focus) = match world.get::<DepthOfField>(entity) {
        Some(d) => (
            json!({
                "f_stop": d.aperture_f_stops,
                "focus_m": d.focal_distance,
                "sensor_height_mm": d.sensor_height * 1000.0,
                "max_coc_px": d.max_circle_of_confusion_diameter,
                "mode": if d.mode == DepthOfFieldMode::Bokeh { "bokeh" } else { "gaussian" },
            }),
            Some(d.focal_distance),
        ),
        None => (Value::Null, None),
    };
    #[cfg(not(feature = "dof"))]
    let (dof, focus) = (Value::Null, None::<f32>);
    let distance = focus
        .or_else(|| world.get::<Focus>(entity).map(|f| f.0))
        .unwrap_or(DEFAULT_DISTANCE);
    let (eye, target, up) = unlook(&pose, distance);
    let name = world.get::<Name>(entity);
    json!({
        "camera": name.map_or_else(|| json!(entity.to_bits()), |n| json!(n.as_str())),
        "eye": frame.from_bevy(eye).to_array(),
        "target": frame.from_bevy(target).to_array(),
        "up": frame.from_bevy(up).to_array(),
        "vfov_deg": fov,
        "dof": dof,
        "hold": hold.is_some(),
    })
}

/// Re-applies each held pose and field of view, in `PostUpdate` before transforms propagate.
pub(crate) fn hold(
    mut cameras: Query<(&Hold, &mut Transform, &mut Projection, Option<&ChildOf>)>,
    parents: Query<&GlobalTransform>,
) {
    for (hold, mut transform, mut projection, child_of) in &mut cameras {
        let parent = child_of.and_then(|c| parents.get(c.parent()).ok());
        let local = parent.map_or(hold.pose, |p| {
            GlobalTransform::from(hold.pose).reparented_to(p)
        });
        transform.set_if_neq(local);
        if let Some(fov) = hold.fov {
            set_fov(&mut projection, fov);
        }
    }
}

/// Re-applies each held depth of field, or its absence.
#[cfg(feature = "dof")]
pub(crate) fn hold_dof(
    mut commands: Commands,
    mut cameras: Query<(Entity, &Hold, Option<&mut DepthOfField>)>,
) {
    for (entity, hold, dof) in &mut cameras {
        match (hold.dof, dof) {
            (Some(want), Some(mut have)) => {
                let same = have.mode == want.mode
                    && have.focal_distance == want.focal_distance
                    && have.aperture_f_stops == want.aperture_f_stops
                    && have.sensor_height == want.sensor_height
                    && have.max_circle_of_confusion_diameter
                        == want.max_circle_of_confusion_diameter
                    && have.max_depth == want.max_depth;
                if !same {
                    *have = want;
                }
            }
            (Some(want), None) => {
                commands.entity(entity).insert(want);
            }
            (None, Some(_)) => {
                commands.entity(entity).remove::<DepthOfField>();
            }
            (None, None) => {}
        }
    }
}

/// Sets a perspective field of view, switching an orthographic camera to perspective.
/// Leaves the projection unchanged (and unmarked) when it already matches.
fn set_fov(projection: &mut Mut<Projection>, fov: f32) {
    match projection.as_ref() {
        Projection::Perspective(p) if p.fov == fov => {}
        Projection::Perspective(_) => {
            if let Projection::Perspective(p) = projection.as_mut() {
                p.fov = fov;
            }
        }
        _ => {
            **projection = Projection::Perspective(PerspectiveProjection { fov, ..default() });
        }
    }
}

/// The parent's world transform, for a camera with a parent.
fn parent_transform(world: &World, entity: Entity) -> Option<GlobalTransform> {
    let parent = world.get::<ChildOf>(entity)?.parent();
    world.get::<GlobalTransform>(parent).copied()
}

/// The camera's world pose from its own transform, which may be newer than its
/// `GlobalTransform`.
fn world_pose(world: &World, entity: Entity) -> Transform {
    let local = world.get::<Transform>(entity).copied().unwrap_or_default();
    parent_transform(world, entity).map_or(local, |p| p.mul_transform(local).compute_transform())
}
