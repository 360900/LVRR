use crate::openxr_data::RealOpenXrData;
use openvr as vr;
use openxr as xr;
use std::sync::Arc;

#[derive(macros::InterfaceImpl)]
#[interface = "IVRChaperone"]
#[versions(004, 003)]
pub struct Chaperone {
    vtables: Vtables,
    openxr: Arc<RealOpenXrData>,
}

impl Chaperone {
    pub fn new(openxr: Arc<RealOpenXrData>) -> Self {
        Self {
            vtables: Default::default(),
            openxr,
        }
    }

    /// Play area extents from the runtime's stage bounds. `height` is the Z extent.
    fn stage_bounds(&self) -> Option<xr::Extent2Df> {
        let bounds = self
            .openxr
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
}

impl vr::IVRChaperone004_Interface for Chaperone {
    fn ResetZeroPose(&self, origin: vr::ETrackingUniverseOrigin) {
        self.openxr.reset_tracking_space(origin);
    }

    fn ForceBoundsVisible(&self, _: bool) {
        crate::warn_unimplemented!("ForceBoundsVisible");
    }
    fn AreBoundsVisible(&self) -> bool {
        crate::warn_unimplemented!("AreBoundsVisible");
        false
    }
    fn GetBoundsColor(
        &self,
        color_array: *mut vr::HmdColor_t,
        count: std::ffi::c_int,
        _collision_bounds_fade_distance: f32,
        camera_color: *mut vr::HmdColor_t,
    ) {
        crate::warn_unimplemented!("GetBoundsColor");
        if color_array.is_null() || camera_color.is_null() || count <= 0 {
            return;
        }
        let color_array = unsafe { std::slice::from_raw_parts_mut(color_array, count as usize) };
        color_array.fill(vr::HmdColor_t::default());
        unsafe {
            camera_color.write(vr::HmdColor_t::default());
        }
    }
    fn SetSceneColor(&self, _: vr::HmdColor_t) {
        crate::warn_unimplemented!("SetSceneColor");
    }
    fn ReloadInfo(&self) {
        crate::warn_unimplemented!("ReloadInfo");
    }
    fn GetPlayAreaRect(&self, rect: *mut vr::HmdQuad_t) -> bool {
        if rect.is_null() {
            return false;
        }
        let Some(bounds) = self.stage_bounds() else {
            unsafe { rect.write(Default::default()) };
            return false;
        };
        let (x, z) = (bounds.width / 2.0, bounds.height / 2.0);
        // Counter-clockwise seen from above, centered on the stage origin, on the floor.
        let corner = |x, z| vr::HmdVector3_t { v: [x, 0.0, z] };
        unsafe {
            rect.write(vr::HmdQuad_t {
                vCorners: [corner(x, -z), corner(-x, -z), corner(-x, z), corner(x, z)],
            })
        };
        true
    }
    fn GetPlayAreaSize(&self, size_x: *mut f32, size_z: *mut f32) -> bool {
        if size_x.is_null() || size_z.is_null() {
            return false;
        }
        let bounds = self.stage_bounds().unwrap_or(xr::Extent2Df {
            width: 1.0,
            height: 1.0,
        });
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
