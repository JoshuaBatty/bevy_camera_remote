//! The protocol's world frame and the camera pose math, kept free of the ECS so it is
//! easy to test.

use bevy::prelude::*;

/// How positions in the protocol map to the app's Bevy world.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorldFrame {
    /// The protocol's frame: Z-up, right-handed, metres. A protocol point `(x, y, z)` is
    /// the Bevy point `(x, z, -y)`.
    #[default]
    ZUp,
    /// The app's own Bevy frame (Y-up); positions pass through unchanged.
    YUp,
}

impl WorldFrame {
    /// A protocol position or direction in Bevy's frame.
    pub fn to_bevy(self, v: Vec3) -> Vec3 {
        match self {
            Self::ZUp => Vec3::new(v.x, v.z, -v.y),
            Self::YUp => v,
        }
    }

    /// A Bevy position or direction in the protocol's frame.
    pub fn from_bevy(self, v: Vec3) -> Vec3 {
        match self {
            Self::ZUp => Vec3::new(v.x, -v.z, v.y),
            Self::YUp => v,
        }
    }
}

/// The camera transform at `eye` looking at `target`, its top turned toward `up`
/// (all in Bevy's frame). `up` need not be square to the view: only its tilt about the
/// view direction counts, so any up other than world up rolls the camera.
pub(crate) fn look(eye: Vec3, target: Vec3, up: Vec3) -> Result<Transform, &'static str> {
    if !(eye.is_finite() && target.is_finite() && up.is_finite()) {
        return Err("eye, target and up must be finite");
    }
    let forward = (target - eye)
        .try_normalize()
        .ok_or("eye and target must differ")?;
    let up = up.try_normalize().ok_or("up must not be zero")?;
    if forward.cross(up).length() < 1e-6 {
        return Err("up must not be parallel to the view direction");
    }
    Ok(Transform::from_translation(eye).looking_to(forward, up))
}

/// The eye, a target `distance` ahead, and the camera's own up of a pose; feeding them
/// back to [`look`] gives the same pose.
pub(crate) fn unlook(pose: &Transform, distance: f32) -> (Vec3, Vec3, Vec3) {
    let eye = pose.translation;
    (eye, eye + pose.forward() * distance, *pose.up())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::FRAC_PI_6;

    const POINTS: [Vec3; 4] = [
        Vec3::new(4.5, 0.05, 1.6),
        Vec3::new(-3.0, 7.25, -0.5),
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(1e3, -2e2, 3.5),
    ];

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-5 * (1.0 + a.length().max(b.length()))
    }

    #[test]
    fn frames_round_trip() {
        for frame in [WorldFrame::ZUp, WorldFrame::YUp] {
            for p in POINTS {
                assert_eq!(frame.from_bevy(frame.to_bevy(p)), p);
                assert_eq!(frame.to_bevy(frame.from_bevy(p)), p);
            }
        }
    }

    #[test]
    fn z_up_maps_to_bevy_axes() {
        let f = WorldFrame::ZUp;
        assert_eq!(f.to_bevy(Vec3::X), Vec3::X);
        assert_eq!(f.to_bevy(Vec3::Y), Vec3::NEG_Z);
        assert_eq!(f.to_bevy(Vec3::Z), Vec3::Y);
        assert_eq!(
            f.to_bevy(Vec3::new(4.5, 0.05, 1.6)),
            Vec3::new(4.5, 1.6, -0.05)
        );
        // A rotation, so handedness survives: x × y = z in both frames.
        assert_eq!(
            f.to_bevy(Vec3::X).cross(f.to_bevy(Vec3::Y)),
            f.to_bevy(Vec3::Z)
        );
    }

    #[test]
    fn look_points_the_camera_at_the_target() {
        let f = WorldFrame::ZUp;
        let (eye, target) = (f.to_bevy(POINTS[0]), f.to_bevy(Vec3::new(0.5, 0.05, 0.9)));
        let pose = look(eye, target, f.to_bevy(Vec3::Z)).unwrap();
        assert!(close(pose.translation, eye));
        assert!(close(*pose.forward(), (target - eye).normalize()));
        // Level: the camera's right stays horizontal and its top leans up.
        assert!(pose.right().dot(Vec3::Y).abs() < 1e-6);
        assert!(pose.up().dot(Vec3::Y) > 0.0);
        assert!((pose.scale - Vec3::ONE).length() < 1e-6);
    }

    #[test]
    fn up_sets_roll() {
        let (eye, target) = (Vec3::new(0.0, 1.0, 5.0), Vec3::new(0.0, 1.0, 0.0));
        // Looking down -Z, an up tilted 30 degrees toward +X rolls the top to the right.
        let up = Vec3::new(FRAC_PI_6.sin(), FRAC_PI_6.cos(), 0.0);
        let pose = look(eye, target, up).unwrap();
        assert!(close(*pose.up(), up));
        assert!(close(*pose.forward(), Vec3::NEG_Z));
        // An up that is not square to the view gives the same roll as its square part.
        let leaning = look(eye, target, up + Vec3::new(0.0, 0.0, 0.7)).unwrap();
        assert!(leaning.rotation.dot(pose.rotation).abs() > 1.0 - 1e-6);
    }

    #[test]
    fn pose_round_trips() {
        let ups = [Vec3::Y, Vec3::new(0.3, 1.0, 0.2), Vec3::new(-1.0, 0.1, 0.0)];
        for (i, eye) in POINTS.into_iter().enumerate() {
            let target = eye + Vec3::new(1.0, -0.5, 2.0) * (i as f32 + 0.5);
            let up = ups[i % ups.len()];
            let pose = look(eye, target, up).unwrap();
            let distance = (target - eye).length();
            let (e, t, u) = unlook(&pose, distance);
            assert!(
                close(e, eye) && close(t, target),
                "{e} {t} vs {eye} {target}"
            );
            let again = look(e, t, u).unwrap();
            assert!(again.rotation.dot(pose.rotation).abs() > 1.0 - 1e-6);
            assert!(close(again.translation, pose.translation));
        }
    }

    #[test]
    fn rejects_degenerate_poses() {
        assert!(look(Vec3::ONE, Vec3::ONE, Vec3::Y).is_err());
        assert!(look(Vec3::ZERO, Vec3::NEG_Y, Vec3::Y).is_err());
        assert!(look(Vec3::ZERO, Vec3::X, Vec3::ZERO).is_err());
        assert!(look(Vec3::ZERO, Vec3::splat(f32::NAN), Vec3::Y).is_err());
    }
}
