use crate::chaperone::{play_area_rect, play_area_size};
use crate::openxr_data::RealOpenXrData;
use openvr::{self as vr, HmdQuad_t, HmdVector2_t};
use std::os::raw::c_char;
use std::sync::Arc;

/// There's no way to edit the runtime's bounds, so the working copy is always the live one.
#[derive(macros::InterfaceImpl)]
#[interface = "IVRChaperoneSetup"]
#[versions(006, 005, 004)]
pub struct ChaperoneSetup {
    vtables: Vtables,
    openxr: Arc<RealOpenXrData>,
}

/// OpenXR bounds have no height. 8 ft, a common ceiling height.
const WALL_HEIGHT: f32 = 2.4384;

impl ChaperoneSetup {
    pub fn new(openxr: Arc<RealOpenXrData>) -> Self {
        Self {
            vtables: Default::default(),
            openxr,
        }
    }

    /// The walls along the play area's edges, each starting with its bottom corner on the
    /// previous edge.
    fn collision_bounds(&self, buffer: *mut HmdQuad_t, count: *mut u32) -> bool {
        let Some(count) = (unsafe { count.as_mut() }) else {
            return false;
        };
        let Some(rect) = play_area_rect(&self.openxr) else {
            *count = 0;
            return false;
        };
        let walls: [HmdQuad_t; 4] = std::array::from_fn(|i| {
            let [a, b] = [rect.vCorners[i], rect.vCorners[(i + 1) % 4]];
            let top = |c: vr::HmdVector3_t| vr::HmdVector3_t {
                v: [c.v[0], WALL_HEIGHT, c.v[2]],
            };
            HmdQuad_t {
                vCorners: [a, top(a), top(b), b],
            }
        });

        let capacity = std::mem::replace(count, walls.len() as u32);
        if buffer.is_null() {
            return true;
        }
        if (capacity as usize) < walls.len() {
            return false;
        }
        unsafe { std::ptr::copy_nonoverlapping(walls.as_ptr(), buffer, walls.len()) };
        true
    }

    /// Writes `origin`'s zero pose in the raw universe.
    fn zero_pose_to_raw(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        pose: *mut vr::HmdMatrix34_t,
    ) -> bool {
        if pose.is_null() {
            return false;
        }
        let Some(zero) = self
            .openxr
            .locate_origin(origin, vr::ETrackingUniverseOrigin::RawAndUncalibrated)
        else {
            return false;
        };
        unsafe { pose.write(zero.into()) };
        true
    }
}

impl vr::IVRChaperoneSetup006_Interface for ChaperoneSetup {
    fn CommitWorkingCopy(&self, _: vr::EChaperoneConfigFile) -> bool {
        crate::warn_unimplemented!("CommitWorkingCopy");
        false
    }

    fn RevertWorkingCopy(&self) {
        crate::warn_unimplemented!("RevertWorkingCopy");
    }

    fn GetWorkingPlayAreaSize(&self, size_x: *mut f32, size_z: *mut f32) -> bool {
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

    fn GetWorkingPlayAreaRect(&self, rect: *mut HmdQuad_t) -> bool {
        if rect.is_null() {
            return false;
        }
        let quad = play_area_rect(&self.openxr);
        unsafe { rect.write(quad.unwrap_or_default()) };
        quad.is_some()
    }

    fn GetWorkingCollisionBoundsInfo(&self, buffer: *mut HmdQuad_t, quads_count: *mut u32) -> bool {
        self.collision_bounds(buffer, quads_count)
    }

    fn GetLiveCollisionBoundsInfo(&self, buffer: *mut HmdQuad_t, quads_count: *mut u32) -> bool {
        self.collision_bounds(buffer, quads_count)
    }

    fn GetWorkingSeatedZeroPoseToRawTrackingPose(&self, pose: *mut vr::HmdMatrix34_t) -> bool {
        self.zero_pose_to_raw(vr::ETrackingUniverseOrigin::Seated, pose)
    }

    fn GetWorkingStandingZeroPoseToRawTrackingPose(&self, pose: *mut vr::HmdMatrix34_t) -> bool {
        self.zero_pose_to_raw(vr::ETrackingUniverseOrigin::Standing, pose)
    }

    fn SetWorkingPlayAreaSize(&self, _: f32, _: f32) {
        crate::warn_unimplemented!("SetWorkingPlayAreaSize");
    }

    fn SetWorkingCollisionBoundsInfo(&self, _: *mut HmdQuad_t, _: u32) {
        crate::warn_unimplemented!("SetWorkingCollisionBoundsInfo");
    }

    fn SetWorkingPerimeter(&self, _: *const HmdVector2_t, _: u32) {
        crate::warn_unimplemented!("SetWorkingPerimeter");
    }

    fn SetWorkingSeatedZeroPoseToRawTrackingPose(&self, _: *const vr::HmdMatrix34_t) {
        crate::warn_unimplemented!("SetWorkingSeatedZeroPoseToRawTrackingPose");
    }

    fn SetWorkingStandingZeroPoseToRawTrackingPose(&self, _: *const vr::HmdMatrix34_t) {
        crate::warn_unimplemented!("SetWorkingStandingZeroPoseToRawTrackingPose");
    }

    fn ReloadFromDisk(&self, _: vr::EChaperoneConfigFile) {
        crate::warn_unimplemented!("ReloadFromDisk");
    }

    fn GetLiveSeatedZeroPoseToRawTrackingPose(&self, pose: *mut vr::HmdMatrix34_t) -> bool {
        self.zero_pose_to_raw(vr::ETrackingUniverseOrigin::Seated, pose)
    }

    fn ExportLiveToBuffer(&self, _: *mut c_char, buffer_length: *mut u32) -> bool {
        crate::warn_unimplemented!("ExportLiveToBuffer");
        if let Some(buffer_length) = unsafe { buffer_length.as_mut() } {
            *buffer_length = 0;
        }
        false
    }

    fn ImportFromBufferToWorking(&self, _: *const c_char, _: u32) -> bool {
        crate::warn_unimplemented!("ImportFromBufferToWorking");
        false
    }

    fn ShowWorkingSetPreview(&self) {
        crate::warn_unimplemented!("ShowWorkingSetPreview");
    }

    fn HideWorkingSetPreview(&self) {
        crate::warn_unimplemented!("HideWorkingSetPreview");
    }

    fn RoomSetupStarting(&self) {
        crate::warn_unimplemented!("RoomSetupStarting");
    }
}

// The 0.9.14 SDK added these to IVRChaperoneSetup_004 without bumping the version; 005 renamed
// them.
impl vr::IVRChaperoneSetup004On005 for ChaperoneSetup {
    fn SetWorkingWallTagInfo(&self, tags: *mut u8, tag_count: u32) {
        vr::IVRChaperoneSetup005On006::SetWorkingCollisionBoundsTagsInfo(self, tags, tag_count)
    }

    fn GetLiveWallTagInfo(&self, tags: *mut u8, tag_count: *mut u32) -> bool {
        vr::IVRChaperoneSetup005On006::GetLiveCollisionBoundsTagsInfo(self, tags, tag_count)
    }
}

impl vr::IVRChaperoneSetup005On006 for ChaperoneSetup {
    fn SetWorkingCollisionBoundsTagsInfo(&self, _: *mut u8, _: u32) {
        crate::warn_unimplemented!("SetWorkingCollisionBoundsTagsInfo");
    }

    fn GetLiveCollisionBoundsTagsInfo(&self, _: *mut u8, tag_count: *mut u32) -> bool {
        crate::warn_unimplemented!("GetLiveCollisionBoundsTagsInfo");
        if let Some(tag_count) = unsafe { tag_count.as_mut() } {
            *tag_count = 0;
        }
        false
    }

    fn SetWorkingPhysicalBoundsInfo(&self, _: *mut HmdQuad_t, _: u32) -> bool {
        crate::warn_unimplemented!("SetWorkingPhysicalBoundsInfo");
        false
    }

    // OpenXR only knows one boundary, the play area.
    fn GetLivePhysicalBoundsInfo(&self, buffer: *mut HmdQuad_t, quads_count: *mut u32) -> bool {
        self.collision_bounds(buffer, quads_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clientcore::Injector, openxr_data::OpenXrData};
    use openxr as xr;
    use vr::IVRChaperoneSetup006_Interface;

    fn setup(bounds: Option<xr::Extent2Df>) -> ChaperoneSetup {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        fakexr::set_stage_bounds(xr.session_data.get().session.as_raw(), bounds);
        ChaperoneSetup::new(xr)
    }

    #[test]
    fn bounds_match_chaperone() {
        let setup = setup(Some(xr::Extent2Df {
            width: 2.0,
            height: 3.0,
        }));
        let (mut x, mut z) = (0.0, 0.0);
        assert!(setup.GetWorkingPlayAreaSize(&mut x, &mut z));
        assert_eq!((x, z), (2.0, 3.0));
        let mut rect = HmdQuad_t::default();
        assert!(setup.GetWorkingPlayAreaRect(&mut rect));
        let rect = rect.vCorners.map(|c| c.v);
        assert_eq!(rect[0], [1.0, 0.0, -1.5]);

        let mut count = 0;
        assert!(setup.GetLiveCollisionBoundsInfo(std::ptr::null_mut(), &mut count));
        assert_eq!(count, 4);
        let mut walls = [HmdQuad_t::default(); 4];
        count = 3;
        assert!(!setup.GetLiveCollisionBoundsInfo(walls.as_mut_ptr(), &mut count));
        count = 4;
        assert!(setup.GetWorkingCollisionBoundsInfo(walls.as_mut_ptr(), &mut count));
        for (i, wall) in walls.iter().enumerate() {
            let wall = wall.vCorners.map(|c| c.v);
            assert_eq!(wall[0], rect[i]);
            assert_eq!(wall[3], rect[(i + 1) % 4]);
            assert_eq!(wall[1][1], WALL_HEIGHT);
        }
        let mut physical = [HmdQuad_t::default(); 4];
        assert!(vr::IVRChaperoneSetup005On006::GetLivePhysicalBoundsInfo(
            &setup,
            physical.as_mut_ptr(),
            &mut count
        ));
        assert_eq!(
            physical.map(|q| q.vCorners.map(|c| c.v)),
            walls.map(|q| q.vCorners.map(|c| c.v))
        );

        // LOCAL 1.6 m above the floor.
        fakexr::set_reference_space_pose(
            setup.openxr.session_data.get().session.as_raw(),
            xr::ReferenceSpaceType::STAGE,
            xr::Posef {
                position: xr::Vector3f {
                    x: 0.0,
                    y: -1.6,
                    z: 0.0,
                },
                ..xr::Posef::IDENTITY
            },
        );
        let translation = |m: vr::HmdMatrix34_t| m.m.map(|row| row[3]);
        let mut pose = vr::HmdMatrix34_t::default();
        assert!(setup.GetLiveSeatedZeroPoseToRawTrackingPose(&mut pose));
        assert_eq!(translation(pose), [0.0, 1.6, 0.0]);
        assert!(setup.GetWorkingStandingZeroPoseToRawTrackingPose(&mut pose));
        assert_eq!(translation(pose), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn no_bounds() {
        let setup = setup(None);
        let mut count = 4;
        assert!(!setup.GetLiveCollisionBoundsInfo(std::ptr::null_mut(), &mut count));
        assert_eq!(count, 0);
        let mut rect = HmdQuad_t::default();
        assert!(!setup.GetWorkingPlayAreaRect(&mut rect));
    }
}
