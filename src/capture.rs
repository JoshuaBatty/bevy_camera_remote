//! `camera/capture`: applies a camera and a time, renders the camera offscreen at an
//! exact size, saves a PNG, and only then replies.
//!
//! BRP replies to a method once its handler returns a value, so capture is a watching
//! method: BRP polls it every frame (in `RemoteLast`) until it has the reply. The work
//! itself runs in [`advance`] and the screenshot observer, so a capture whose client
//! hangs up still finishes and gives the camera back its own target.

use crate::camera::{self, SetParams};
use crate::{RemoteSeek, invalid, parse};
use bevy::camera::{ImageRenderTarget, RenderTarget, Viewport};
use bevy::diagnostic::FrameCount;
use bevy::prelude::*;
use bevy::remote::{BrpError, BrpResult};
use bevy::render::MainWorld;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{PipelineCache, TextureFormat};
use bevy::render::renderer::RenderDevice;
use bevy::render::texture::GpuImage;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Consecutive rendered frames with the image on the GPU and no shader still compiling
/// before the shot, so a first capture is not missing anything.
const READY_FRAMES: u32 = 2;

/// How long to wait for shaders, and then for the image, before going ahead or failing.
/// Both together stay well inside a client's two-minute request timeout.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most frames a request may ask to settle for.
const MAX_SETTLE_FRAMES: u32 = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    path: PathBuf,
    size: Option<[u32; 2]>,
    camera: Option<SetParams>,
    time_s: Option<f64>,
    settle_frames: Option<u32>,
}

/// The capture in progress, and enough memory of the last reply to tell its leftover
/// polls from a new request.
#[derive(Resource, Default)]
pub(crate) struct Capture {
    /// Counts app frames, from `First`.
    tick: u32,
    job: Option<Job>,
    /// The params and tick of the last reply. BRP polls an answered request until its
    /// connection closes, usually once more; those polls must not start a new capture.
    answered: Option<(Option<Value>, u32)>,
}

struct Job {
    /// The request's params, which identify it when BRP polls.
    request: Option<Value>,
    camera: Entity,
    image: Handle<Image>,
    /// What to give the camera back once the shot is taken.
    restore: Option<Restore>,
    path: PathBuf,
    size: UVec2,
    /// The first tick the shot may be taken in.
    shoot_at: u32,
    /// Consecutive extractions that found the image on the GPU and no pipeline waiting.
    ready: u32,
    phase: Phase,
}

enum Phase {
    /// Waiting out the settle frames.
    Settling,
    /// Settled; waiting, since then, for the renderer to be ready.
    Warming(Instant),
    /// The screenshot entity, the app frame it was taken in, and when.
    Shooting {
        shot: Entity,
        frame: u32,
        since: Instant,
    },
    /// The reply, and the tick it was ready in.
    Done { tick: u32, result: BrpResult },
}

/// What the capture changed on the camera.
struct Restore {
    target: RenderTarget,
    viewport: Option<Viewport>,
    active: bool,
}

/// `camera/capture`, polled every frame until it returns the reply.
pub(crate) fn capture(
    In(params): In<Option<Value>>,
    world: &mut World,
) -> BrpResult<Option<Value>> {
    let mut state = world.resource_mut::<Capture>();
    let tick = state.tick;
    if let Some(job) = state.job.take() {
        let mine = job.request == params;
        match job.phase {
            Phase::Done { result, .. } if mine => {
                state.answered = Some((params, tick));
                return result.map(Some);
            }
            // Done in an earlier frame and never collected: its client has gone.
            Phase::Done { tick: done, .. } if done != tick => {}
            // Running, or another request's reply not yet collected: wait.
            _ => {
                state.job = Some(job);
                return Ok(None);
            }
        }
    }
    if let Some((answered, at)) = &state.answered
        && *answered == params
        && tick.wrapping_sub(*at) <= 1
    {
        return Ok(None);
    }
    match start(world, params.clone(), tick) {
        Ok(job) => {
            world.resource_mut::<Capture>().job = Some(job);
            Ok(None)
        }
        Err(error) => {
            world.resource_mut::<Capture>().answered = Some((params, tick));
            Err(error)
        }
    }
}

/// Checks the request, applies its camera and time, and points the camera at a new image.
fn start(world: &mut World, request: Option<Value>, tick: u32) -> BrpResult<Job> {
    let p: Params = parse(request.clone())?;
    if !p.path.is_absolute() {
        return Err(invalid("path must be absolute"));
    }
    if !p
        .path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    {
        return Err(invalid("path must end in .png"));
    }
    if !p.path.parent().is_some_and(Path::is_dir) {
        return Err(invalid(format!(
            "The folder for {} does not exist",
            p.path.display()
        )));
    }
    let max = world
        .get_resource::<RenderDevice>()
        .ok_or_else(|| BrpError::internal("Capture needs a renderer, and this app has none"))?
        .limits()
        .max_texture_dimension_2d;
    let [w, h] = p.size.unwrap_or([1920, 1080]);
    if w == 0 || h == 0 || w > max || h > max {
        return Err(invalid(format!("size must be 1 to {max} pixels a side")));
    }
    if p.time_s.is_some_and(|t| !t.is_finite()) {
        return Err(invalid("time_s must be finite"));
    }
    let settle = p.settle_frames.unwrap_or(3);
    if settle > MAX_SETTLE_FRAMES {
        return Err(invalid(format!(
            "settle_frames must be at most {MAX_SETTLE_FRAMES}"
        )));
    }
    let camera = match p.camera {
        Some(params) => camera::apply(world, params)?,
        None => camera::resolve(world, None)?,
    };
    if let Some(seconds) = p.time_s {
        world.write_message(RemoteSeek { seconds });
    }
    let image = Image::new_target_texture(w, h, TextureFormat::Rgba8UnormSrgb, None);
    let image = world.resource_mut::<Assets<Image>>().add(image);
    let target = ImageRenderTarget {
        handle: image.clone(),
        scale_factor: 1.0,
    };
    let to = Restore {
        target: target.into(),
        viewport: None,
        active: true,
    };
    let restore = aim(world, camera, to);
    Ok(Job {
        request,
        camera,
        image,
        restore,
        path: p.path,
        size: UVec2::new(w, h),
        shoot_at: tick.wrapping_add(1 + settle),
        ready: 0,
        phase: Phase::Settling,
    })
}

/// Counts frames, in `First`.
pub(crate) fn tick(mut state: ResMut<Capture>) {
    state.tick = state.tick.wrapping_add(1);
}

/// Keeps a capture's camera whole and active until the shot, in `PostUpdate`, against an
/// app that sets the camera's viewport or activity every frame (from a UI pane, say).
pub(crate) fn keep(state: Res<Capture>, mut cameras: Query<&mut Camera>) {
    let Some(job) = state.job.as_ref().filter(|j| j.restore.is_some()) else {
        return;
    };
    let Ok(mut camera) = cameras.get_mut(job.camera) else {
        return;
    };
    if camera.viewport.is_some() {
        camera.viewport = None;
    }
    if !camera.is_active {
        camera.is_active = true;
    }
}

/// Moves the capture along, in `Last`: settle, wait for the renderer, then shoot.
pub(crate) fn advance(world: &mut World) {
    let frame = world.get_resource::<FrameCount>().map_or(0, |f| f.0);
    let mut state = world.resource_mut::<Capture>();
    let tick = state.tick;
    let Some(job) = state.job.as_mut() else {
        return;
    };
    if matches!(job.phase, Phase::Settling) && tick >= job.shoot_at {
        job.phase = Phase::Warming(Instant::now());
    }
    match job.phase {
        Phase::Warming(since) if job.ready >= READY_FRAMES || since.elapsed() > TIMEOUT => {
            if job.ready < READY_FRAMES {
                warn!("Shaders still compiling after {TIMEOUT:?}; capturing anyway");
            }
            let image = job.image.clone();
            let shot = world.spawn(Screenshot::image(image)).observe(saved).id();
            if let Some(job) = world.resource_mut::<Capture>().job.as_mut() {
                job.phase = Phase::Shooting {
                    shot,
                    frame,
                    since: Instant::now(),
                };
            }
        }
        Phase::Shooting { since, .. } if since.elapsed() > TIMEOUT => {
            let error = BrpError::internal("The renderer sent no image back");
            finish(world, Err(error));
        }
        _ => {}
    }
}

/// Saves the shot and readies the reply.
fn saved(captured: On<ScreenshotCaptured>, mut commands: Commands) {
    let (entity, image) = (captured.entity, captured.image.clone());
    commands.queue(move |world: &mut World| {
        let Some(job) = world.resource::<Capture>().job.as_ref() else {
            return;
        };
        let Phase::Shooting { shot, frame, .. } = job.phase else {
            return;
        };
        if shot != entity {
            return;
        }
        let (path, size) = (job.path.clone(), job.size);
        let result = save(&image, &path, size)
            .map(|()| json!({ "path": path, "frame": frame, "size": [size.x, size.y] }));
        finish(world, result);
    });
}

/// Writes the image as an opaque 8-bit PNG.
fn save(image: &Image, path: &Path, size: UVec2) -> BrpResult<()> {
    if image.size() != size {
        return Err(BrpError::internal(format!(
            "The image came back {} by {}, not {} by {}",
            image.width(),
            image.height(),
            size.x,
            size.y
        )));
    }
    let rgb = image
        .clone()
        .try_into_dynamic()
        .map_err(BrpError::internal)?
        .into_rgb8();
    rgb.save(path)
        .map_err(|e| BrpError::internal(format!("Could not write {}: {e}", path.display())))
}

/// Gives the camera back its own target and records the reply.
fn finish(world: &mut World, result: BrpResult) {
    let mut state = world.resource_mut::<Capture>();
    let tick = state.tick;
    let Some(job) = state.job.as_mut() else {
        return;
    };
    job.phase = Phase::Done { tick, result };
    let (camera, restore) = (job.camera, job.restore.take());
    if let Some(restore) = restore {
        aim(world, camera, restore);
    }
}

/// Sets the camera's target, viewport and activity, returning what they were. A changed
/// projection makes Bevy re-read the target's size (and so the aspect ratio) this frame.
fn aim(world: &mut World, camera: Entity, to: Restore) -> Option<Restore> {
    let mut entity = world.get_entity_mut(camera).ok()?;
    let target = std::mem::replace(&mut *entity.get_mut::<RenderTarget>()?, to.target);
    let mut cam = entity.get_mut::<Camera>()?;
    let viewport = std::mem::replace(&mut cam.viewport, to.viewport);
    let active = std::mem::replace(&mut cam.is_active, to.active);
    if let Some(mut projection) = entity.get_mut::<Projection>() {
        projection.set_changed();
    }
    Some(Restore {
        target,
        viewport,
        active,
    })
}

/// Counts, at each extraction, whether the renderer has the capture image and no
/// pipeline waiting to compile. Runs in the render world's `ExtractSchedule`.
pub(crate) fn probe(
    mut main: ResMut<MainWorld>,
    cache: Option<Res<PipelineCache>>,
    images: Option<Res<RenderAssets<GpuImage>>>,
) {
    let (Some(cache), Some(images)) = (cache, images) else {
        return;
    };
    let Some(mut state) = main.get_resource_mut::<Capture>() else {
        return;
    };
    let Some(job) = state.job.as_mut() else {
        return;
    };
    if matches!(job.phase, Phase::Settling | Phase::Warming(_)) {
        let ready = images.get(&job.image).is_some() && cache.waiting_pipelines().next().is_none();
        job.ready = if ready {
            job.ready.saturating_add(1)
        } else {
            0
        };
    }
}
