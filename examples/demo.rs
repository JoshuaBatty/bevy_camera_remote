//! A small scene to drive over the Bevy Remote Protocol.
//!
//! `cargo run --example demo`, then send JSON-RPC to `http://127.0.0.1:15702`, e.g.
//!
//! ```sh
//! curl -s localhost:15702 -d '{"jsonrpc":"2.0","id":1,"method":"camera/get"}'
//! ```
//!
//! Positions below are in the protocol's Z-up frame. Cubes stand on the ground at known
//! places: red at the origin, green 2 m along +x, blue 2 m along +y, and a white cube
//! that `camera/capture`'s `time_s` moves along +x at 1 m/s from x = -3. Left alone the
//! camera slowly orbits, standing in for an app's own camera control; a held camera
//! ignores it.

use bevy::camera::Hdr;
use bevy::prelude::*;
use bevy::remote::{RemotePlugin, http::RemoteHttpPlugin};
use bevy_camera_remote::{CameraRemotePlugin, RemoteCamera, RemoteSeek, WorldFrame};

/// The protocol's Z-up position in Bevy's frame.
fn z_up(x: f32, y: f32, z: f32) -> Vec3 {
    WorldFrame::ZUp.to_bevy(Vec3::new(x, y, z))
}

/// The cube `RemoteSeek` moves.
#[derive(Component)]
struct Mover;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "bevy_camera_remote demo".into(),
                resolution: (1280, 720).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            RemotePlugin::default(),
            RemoteHttpPlugin::default(),
            CameraRemotePlugin::default(),
        ))
        .insert_resource(ClearColor(Color::srgb(0.55, 0.65, 0.8)))
        .add_systems(Startup, setup)
        .add_systems(Update, (orbit, seek))
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(20.0, 20.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.35, 0.37, 0.33))),
    ));
    let cube = meshes.add(Cuboid::from_length(0.8));
    for (color, at) in [
        (Color::srgb(0.85, 0.15, 0.12), z_up(0.0, 0.0, 0.4)),
        (Color::srgb(0.15, 0.7, 0.2), z_up(2.0, 0.0, 0.4)),
        (Color::srgb(0.15, 0.3, 0.9), z_up(0.0, 2.0, 0.4)),
    ] {
        commands.spawn((
            Mesh3d(cube.clone()),
            MeshMaterial3d(materials.add(color)),
            Transform::from_translation(at),
        ));
    }
    commands.spawn((
        Mover,
        Mesh3d(cube),
        MeshMaterial3d(materials.add(Color::srgb(0.95, 0.95, 0.9))),
        Transform::from_translation(z_up(-3.0, -2.0, 0.4)),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 8_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_translation(z_up(4.0, -3.0, 8.0)).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((
        Camera3d::default(),
        // Bright out-of-focus highlights keep their energy with HDR.
        Hdr,
        RemoteCamera,
        Name::new("Main Camera"),
        Transform::from_translation(z_up(7.0, -7.0, 4.0)).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// The app's own camera control: a slow orbit. A held camera is put back every frame.
fn orbit(time: Res<Time>, mut camera: Single<&mut Transform, With<RemoteCamera>>) {
    let a = time.elapsed_secs() * 0.2 - core::f32::consts::FRAC_PI_4;
    let eye = z_up(10.0 * a.cos(), 10.0 * a.sin(), 4.0);
    **camera = Transform::from_translation(eye).looking_at(Vec3::ZERO, Vec3::Y);
}

/// Shows the scene at the time a capture asks for.
fn seek(mut seeks: MessageReader<RemoteSeek>, mut mover: Single<&mut Transform, With<Mover>>) {
    if let Some(seek) = seeks.read().last() {
        mover.translation = z_up(-3.0 + seek.seconds as f32, -2.0, 0.4);
    }
}
