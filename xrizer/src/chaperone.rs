use crate::openxr_data::RealOpenXrData;
use glam::{Quat, Vec3};
use openvr as vr;
use openxr as xr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(macros::InterfaceImpl)]
#[interface = "IVRChaperone"]
#[versions(004, 003)]
pub struct Chaperone {
    vtables: Vtables,
    openxr: Arc<RealOpenXrData>,
    /// Whether the app forced the bounds visible. The runtime draws its own bounds as it sees
    /// fit, so this is all AreBoundsVisible can report.
    bounds_forced: AtomicBool,
}

impl Chaperone {
    pub fn new(openxr: Arc<RealOpenXrData>) -> Self {
        Self {
            vtables: Default::default(),
            openxr,
            bounds_forced: false.into(),
        }
    }
}

/// The bounds color from SteamVR's collisionBounds settings, for apps that draw their own.
fn bounds_color() -> vr::HmdColor_t {
    let channel = |key| crate::settings::get_f32("collisionBounds", key).unwrap_or(0.0) / 255.0;
    vr::HmdColor_t {
        r: channel("CollisionBoundsColorGammaR"),
        g: channel("CollisionBoundsColorGammaG"),
        b: channel("CollisionBoundsColorGammaB"),
        a: channel("CollisionBoundsColorGammaA"),
    }
}

/// Play area extents from the runtime's stage bounds. `height` is the Z extent.
fn stage_bounds(openxr: &RealOpenXrData) -> Option<xr::Extent2Df> {
    let bounds = openxr
        .session_data
        .get()
        .session
        .reference_space_bounds_rect(xr::ReferenceSpaceType::STAGE);
    match bounds {
        Ok(Some(bounds)) if bounds.width > 0.0 && bounds.height > 0.0 => Some(bounds),
        Ok(_) => {
            crate::warn_once!("Stage bounds unavailable, play area is unknown");
            None
        }
        Err(e) => {
            crate::warn_once!("Failed to get stage bounds: {:?}", e);
            None
        }
    }
}

/// The play area size, 1x1 m if the runtime doesn't know it.
pub(crate) fn play_area_size(openxr: &RealOpenXrData) -> xr::Extent2Df {
    stage_bounds(openxr).unwrap_or(xr::Extent2Df {
        width: 1.0,
        height: 1.0,
    })
}

/// The play area corners in the standing universe, counter-clockwise seen from above starting at
/// the front right, on the floor.
pub(crate) fn play_area_rect(openxr: &RealOpenXrData) -> Option<vr::HmdQuad_t> {
    let bounds = stage_bounds(openxr)?;
    // The bounds are centered on the runtime's stage, which a standing zero pose reset moves the
    // standing universe away from.
    let xr::Posef {
        position: p,
        orientation: o,
    } = openxr
        .locate_origin(
            vr::ETrackingUniverseOrigin::RawAndUncalibrated,
            vr::ETrackingUniverseOrigin::Standing,
        )
        .unwrap_or(xr::Posef::IDENTITY);
    let (rotation, translation) = (
        Quat::from_xyzw(o.x, o.y, o.z, o.w),
        Vec3::new(p.x, p.y, p.z),
    );
    let (x, z) = (bounds.width / 2.0, bounds.height / 2.0);
    let corner = |x, z| {
        let corner = translation + rotation * Vec3::new(x, 0.0, z);
        vr::HmdVector3_t {
            v: [corner.x, 0.0, corner.z],
        }
    };
    Some(vr::HmdQuad_t {
        vCorners: [corner(x, -z), corner(-x, -z), corner(-x, z), corner(x, z)],
    })
}

impl vr::IVRChaperone004_Interface for Chaperone {
    fn ResetZeroPose(&self, origin: vr::ETrackingUniverseOrigin) {
        self.openxr.reset_tracking_space(origin);
    }

    fn ForceBoundsVisible(&self, force: bool) {
        if self.bounds_forced.swap(force, Ordering::Relaxed) != force {
            crate::warn_once!("The runtime draws its own bounds, ForceBoundsVisible is only recorded");
        }
    }
    fn AreBoundsVisible(&self) -> bool {
        self.bounds_forced.load(Ordering::Relaxed)
    }
    /// The color SteamVR's settings give the bounds, at every fade distance: the runtime's
    /// bounds fade on their own terms.
    fn GetBoundsColor(
        &self,
        color_array: *mut vr::HmdColor_t,
        count: std::ffi::c_int,
        _collision_bounds_fade_distance: f32,
        camera_color: *mut vr::HmdColor_t,
    ) {
        let color = bounds_color();
        if !color_array.is_null() && count > 0 {
            let color_array =
                unsafe { std::slice::from_raw_parts_mut(color_array, count as usize) };
            color_array.fill(color);
        }
        if !camera_color.is_null() {
            unsafe { camera_color.write(color) };
        }
    }
    /// SteamVR tints its grid with this between scenes; the runtime shows its own.
    fn SetSceneColor(&self, _: vr::HmdColor_t) {}
    /// The bounds come from the runtime whenever asked, nothing is cached.
    fn ReloadInfo(&self) {}
    fn GetPlayAreaRect(&self, rect: *mut vr::HmdQuad_t) -> bool {
        if rect.is_null() {
            return false;
        }
        let quad = play_area_rect(&self.openxr);
        unsafe { rect.write(quad.unwrap_or_default()) };
        quad.is_some()
    }
    fn GetPlayAreaSize(&self, size_x: *mut f32, size_z: *mut f32) -> bool {
        if size_x.is_null() || size_z.is_null() {
            return false;
        }
        let bounds = play_area_size(&self.openxr);
        unsafe {
            size_x.write(bounds.width);
            size_z.write(bounds.height);
        }
        true
    }
    fn GetCalibrationState(&self) -> vr::ChaperoneCalibrationState {
        vr::ChaperoneCalibrationState::OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clientcore::Injector, openxr_data::OpenXrData};
    use vr::IVRChaperone004_Interface;

    fn chaperone() -> Chaperone {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        Chaperone::new(xr)
    }

    fn set_bounds(chaperone: &Chaperone, bounds: Option<xr::Extent2Df>) {
        let session = chaperone.openxr.session_data.get().session.as_raw();
        fakexr::set_stage_bounds(session, bounds);
    }

    /// Forcing the bounds visible is recorded, and the bounds color is SteamVR's setting.
    #[test]
    fn bounds_visibility_and_color() {
        let chaperone = chaperone();
        assert!(!chaperone.AreBoundsVisible());
        chaperone.ForceBoundsVisible(true);
        assert!(chaperone.AreBoundsVisible());
        chaperone.ForceBoundsVisible(false);
        assert!(!chaperone.AreBoundsVisible());

        let mut colors = [vr::HmdColor_t::default(); 3];
        let mut camera = vr::HmdColor_t::default();
        chaperone.GetBoundsColor(colors.as_mut_ptr(), colors.len() as i32, 1.0, &mut camera);
        // SteamVR's default: cyan at 60 %.
        for color in colors.iter().chain([&camera]) {
            assert_eq!((color.r, color.g, color.b), (0.0, 1.0, 1.0));
            assert!((color.a - 0.6).abs() < 0.01, "{}", color.a);
        }
        // Null outputs are fine.
        chaperone.GetBoundsColor(std::ptr::null_mut(), 0, 1.0, std::ptr::null_mut());
    }

    #[test]
    fn play_area_from_stage_bounds() {
        let chaperone = chaperone();
        set_bounds(
            &chaperone,
            Some(xr::Extent2Df {
                width: 2.0,
                height: 3.0,
            }),
        );

        let (mut x, mut z) = (0.0, 0.0);
        assert!(chaperone.GetPlayAreaSize(&mut x, &mut z));
        assert_eq!((x, z), (2.0, 3.0));

        let mut rect = vr::HmdQuad_t::default();
        assert!(chaperone.GetPlayAreaRect(&mut rect));
        let corners = rect.vCorners.map(|c| c.v);
        assert_eq!(
            corners,
            [
                [1.0, 0.0, -1.5],
                [-1.0, 0.0, -1.5],
                [-1.0, 0.0, 1.5],
                [1.0, 0.0, 1.5]
            ]
        );
    }

    /// The rect is in the standing universe, which a standing zero pose reset moves away from
    /// the stage the bounds are centered on.
    #[test]
    fn play_area_follows_standing_zero_pose() {
        let chaperone = chaperone();
        set_bounds(
            &chaperone,
            Some(xr::Extent2Df {
                width: 2.0,
                height: 3.0,
            }),
        );
        let rot = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        fakexr::set_reference_space_pose(
            chaperone.openxr.session_data.get().session.as_raw(),
            xr::ReferenceSpaceType::VIEW,
            xr::Posef {
                position: xr::Vector3f {
                    x: 1.0,
                    y: 1.7,
                    z: 0.0,
                },
                orientation: xr::Quaternionf {
                    x: rot.x,
                    y: rot.y,
                    z: rot.z,
                    w: rot.w,
                },
            },
        );
        chaperone.ResetZeroPose(vr::ETrackingUniverseOrigin::Standing);

        let mut rect = vr::HmdQuad_t::default();
        assert!(chaperone.GetPlayAreaRect(&mut rect));
        // Facing -x from (1, 0): the stage's front right corner (1, -1.5) is 1.5 m to the right.
        let expected = [
            [1.5, 0.0, 0.0],
            [1.5, 0.0, -2.0],
            [-1.5, 0.0, -2.0],
            [-1.5, 0.0, 0.0],
        ];
        for (corner, expected) in rect.vCorners.iter().zip(expected) {
            assert!(
                Vec3::from(corner.v).abs_diff_eq(Vec3::from(expected), 1e-5),
                "{:?}",
                rect.vCorners.map(|c| c.v)
            );
        }
    }

    #[test]
    fn play_area_without_stage_bounds() {
        let chaperone = chaperone();
        set_bounds(&chaperone, None);

        let (mut x, mut z) = (0.0, 0.0);
        assert!(chaperone.GetPlayAreaSize(&mut x, &mut z));
        assert_eq!((x, z), (1.0, 1.0));

        let mut rect = vr::HmdQuad_t::default();
        assert!(!chaperone.GetPlayAreaRect(&mut rect));
    }
}
