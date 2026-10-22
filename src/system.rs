use crate::{
    clientcore::{Injected, Injector},
    input::Input,
    openxr_data::{Hand, RealOpenXrData, SessionData},
    overlay::OverlayMan,
    tracy_span,
};

#[cfg(feature = "monado")]
use libmonado::BatteryStatus;

use glam::{Mat3, Quat, Vec3};
use log::{debug, error, info, trace, warn};
use openvr as vr;
use openxr as xr;
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

#[derive(Copy, Clone)]
pub struct ViewData {
    pub flags: xr::ViewStateFlags,
    pub views: [xr::View; 2],
}

#[derive(Copy, Clone)]
struct ViewDataViewSpace {
    data: ViewData,
    original_orientations: [Quat; 2],
}

impl ViewData {
    /// Stand-in for views the runtime never located: a typical IPD and a 90 degree FOV.
    fn fallback() -> Self {
        use std::f32::consts::FRAC_PI_4;
        let view = |x| xr::View {
            pose: xr::Posef {
                orientation: xr::Quaternionf::IDENTITY,
                position: xr::Vector3f { x, y: 0.0, z: 0.0 },
            },
            fov: xr::Fovf {
                angle_left: -FRAC_PI_4,
                angle_right: FRAC_PI_4,
                angle_up: FRAC_PI_4,
                angle_down: -FRAC_PI_4,
            },
        };
        Self {
            flags: xr::ViewStateFlags::EMPTY,
            views: [view(-0.0315), view(0.0315)],
        }
    }
}

#[derive(Default, Copy, Clone)]
struct ViewCache {
    view: Option<ViewDataViewSpace>,
    local: Option<ViewData>,
    stage: Option<ViewData>,
}

/// PRIMARY_STEREO always has two views, but a runtime getting that wrong shouldn't panic.
fn stereo_views(views: Vec<xr::View>) -> xr::Result<[xr::View; 2]> {
    views.try_into().map_err(|v: Vec<_>| {
        crate::warn_once!("Expected 2 views, got {}", v.len());
        xr::sys::Result::ERROR_RUNTIME_FAILURE
    })
}

impl ViewCache {
    fn cached(&self, ty: xr::ReferenceSpaceType) -> Option<ViewData> {
        match ty {
            xr::ReferenceSpaceType::VIEW => self.view.map(|view| view.data),
            xr::ReferenceSpaceType::LOCAL => self.local,
            _ => self.stage,
        }
    }

    fn get_views(
        &mut self,
        session: &SessionData,
        display_time: xr::Time,
        ty: xr::ReferenceSpaceType,
    ) -> xr::Result<ViewData> {
        let view = match self.view {
            Some(view) => view,
            None => *self
                .view
                .insert(Self::get_views_view_space(session, display_time)?),
        };
        let cached = match ty {
            xr::ReferenceSpaceType::VIEW => return Ok(view.data),
            xr::ReferenceSpaceType::LOCAL => &mut self.local,
            xr::ReferenceSpaceType::STAGE => &mut self.stage,
            other => panic!("unexpected reference space type: {other:?}"),
        };
        if let Some(data) = *cached {
            return Ok(data);
        }
        let data =
            Self::get_views_other_space(session, display_time, ty, view.original_orientations)?;
        Ok(*cached.insert(data))
    }

    fn get_views_view_space(
        session: &SessionData,
        display_time: xr::Time,
    ) -> xr::Result<ViewDataViewSpace> {
        let (flags, views) = session.session.locate_views(
            xr::ViewConfigurationType::PRIMARY_STEREO,
            display_time,
            session.get_space_from_type(xr::ReferenceSpaceType::VIEW),
        )?;
        let mut views = stereo_views(views)?;

        // Canted-view handling.
        //
        // Runtimes for HMDs with canted displays (Pimax etc.) report per-eye
        // rotations in the VIEW reference space to describe the tilt of each
        // panel. By default xrizer normalises those away by forcing VIEW-space
        // views to identity ("parallel views") and multiplying the inverse back
        // into LOCAL/STAGE views. On canted HMDs that also flattens the FOV,
        // which is not what the user wants.
        //
        // Opt-in via XRIZER_PRESERVE_VIEW_CANTING=1 to preserve the canted
        // orientations end-to-end (fixes Pimax FOV; may confuse games that
        // assume parallel views).
        static PRESERVE_CANTING: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            std::env::var("XRIZER_PRESERVE_VIEW_CANTING")
                .ok()
                .filter(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
                .is_some()
        });

        let original_orientations: [Quat; 2] = if *PRESERVE_CANTING {
            [Quat::IDENTITY, Quat::IDENTITY]
        } else {
            views
                .iter_mut()
                .map(
                    |xr::View {
                         pose: xr::Posef { orientation: o, .. },
                         ..
                     }| {
                        let ret = Quat::from_xyzw(o.x, o.y, o.z, o.w).inverse();
                        *o = xr::Quaternionf::IDENTITY;
                        ret
                    },
                )
                .collect::<Vec<_>>()
                .try_into()
                .unwrap()
        };

        Ok(ViewDataViewSpace {
            data: ViewData { flags, views },
            original_orientations,
        })
    }

    fn get_views_other_space(
        session: &SessionData,
        display_time: xr::Time,
        ty: xr::ReferenceSpaceType,
        view_data_orientations_inverse: [Quat; 2],
    ) -> xr::Result<ViewData> {
        let (flags, views) = session.session.locate_views(
            xr::ViewConfigurationType::PRIMARY_STEREO,
            display_time,
            session.get_space_from_type(ty),
        )?;
        let mut views = stereo_views(views)?;

        for (
            xr::View {
                pose: xr::Posef {
                    orientation: rot, ..
                },
                ..
            },
            view_rot,
        ) in views.iter_mut().zip(view_data_orientations_inverse)
        {
            let quat = Quat::from_xyzw(rot.x, rot.y, rot.z, rot.w);
            // rotate the inverse of the view space view rotation by this space's
            // view orientation to remove the canting from the displays in this space
            let adjusted_rot = quat * view_rot;
            *rot = xr::Quaternionf {
                x: adjusted_rot.x,
                y: adjusted_rot.y,
                z: adjusted_rot.z,
                w: adjusted_rot.w,
            };
        }

        Ok(ViewData { flags, views })
    }
}

#[derive(Default)]
struct Views {
    /// Located for the current frame.
    frame: ViewCache,
    /// The last views located in each space, for when locating fails.
    last: ViewCache,
    failing: bool,
}

impl Views {
    fn get(
        &mut self,
        session: &SessionData,
        openxr: &RealOpenXrData,
        ty: xr::ReferenceSpaceType,
    ) -> Option<ViewData> {
        let result = self
            .frame
            .get_views(session, openxr.get_display_time(), ty)
            .or_else(|e| match openxr.now() {
                // Before the first frame the display time can be long gone.
                Some(now) => self.frame.get_views(session, now, ty),
                None => Err(e),
            });
        match result {
            Ok(data) => {
                if std::mem::take(&mut self.failing) {
                    info!("Locating views works again");
                }
                let (frame, last) = (self.frame, self.last);
                self.last = ViewCache {
                    view: frame.view.or(last.view),
                    local: frame.local.or(last.local),
                    stage: frame.stage.or(last.stage),
                };
                Some(data)
            }
            Err(e) => {
                if !std::mem::replace(&mut self.failing, true) {
                    warn!("Couldn't locate views, using the last ones: {e}");
                }
                self.last.cached(ty)
            }
        }
    }
}

#[derive(macros::InterfaceImpl)]
#[interface = "IVRSystem"]
#[versions(026, 023, 022, 021, 020, 019, 017, 016, 015, 014, 012, 011, 009)]
pub struct System {
    openxr: Arc<RealOpenXrData>, // We don't need to test session restarting.
    events: Mutex<VecDeque<vr::VREvent_t>>,
    input: Injected<Input<crate::compositor::Compositor>>,
    overlay: Injected<OverlayMan>,
    vtables: Vtables,
    views: Mutex<Views>,
}

mod log_tags {
    pub const TRACKED_PROP: &str = "tracked_property";
}

struct HmdIdentity {
    tracking_system: CString,
    manufacturer: CString,
    model: CString,
}

impl System {
    /// Identity strings the HMD reports, derived from the OpenXR system name.
    ///
    /// Games that sniff these match against the handful of device families
    /// SteamVR shipped, so map the system name to the closest family rather
    /// than inventing new strings. An unrecognized (or empty) system name
    /// falls back to Oculus-style strings, since touch-style controllers are
    /// the modern common case.
    fn hmd_identity(&self) -> &'static HmdIdentity {
        static IDENTITY: std::sync::OnceLock<HmdIdentity> = std::sync::OnceLock::new();
        IDENTITY.get_or_init(|| {
            let name = self
                .openxr
                .instance
                .system_properties(self.openxr.system_id)
                .map(|properties| properties.system_name)
                .unwrap_or_default();
            let lower = name.to_lowercase();
            let is_any = |words: &[&str]| words.iter().any(|w| lower.contains(w));

            let (tracking_system, manufacturer, family_model) =
                if is_any(&["quest", "oculus", "meta", "rift"]) {
                    ("oculus", "Oculus", "Oculus Quest")
                // "frame" covers the Steam Frame, whose controllers games
                // know nothing about; Valve-family strings get sniffing games
                // onto their Index scheme, the closest match for its
                // controllers.
                } else if is_any(&["index", "valve", "lighthouse", "frame"]) {
                    ("lighthouse", "Valve", "Index")
                } else if is_any(&["vive", "htc"]) {
                    ("lighthouse", "HTC", "Vive")
                } else if is_any(&["reverb", "holographic", "mixed reality", "wmr"]) {
                    ("holographic", "WindowsMR", "WindowsMR")
                } else {
                    ("oculus", "Oculus", "Oculus Quest")
                };

            // Report the real system name where we have one - games only
            // substring match on it, and it's more informative in their logs.
            let model = if name.is_empty() {
                family_model.to_string()
            } else {
                name
            };
            log::info!("Reporting HMD identity: {tracking_system}/{manufacturer}/{model:?}");

            HmdIdentity {
                tracking_system: CString::new(tracking_system).unwrap(),
                manufacturer: CString::new(manufacturer).unwrap(),
                model: CString::new(model).unwrap(),
            }
        })
    }

    pub fn new(openxr: Arc<RealOpenXrData>, injector: &Injector) -> Self {
        Self {
            openxr,
            events: Mutex::default(),
            input: injector.inject(),
            overlay: injector.inject(),
            vtables: Default::default(),
            views: Mutex::default(),
        }
    }

    pub fn reset_views(&self) {
        std::mem::take(&mut self.views.lock().unwrap().frame);
        let session = self.openxr.session_data.get();
        let mut views = self.views.lock().unwrap();
        views.get(&session, &self.openxr, xr::ReferenceSpaceType::VIEW);
        views.get(
            &session,
            &self.openxr,
            session.current_origin_as_reference_space(),
        );
    }

    /// The views for this frame, falling back to the last ones located. `None` if the runtime
    /// never located them.
    pub fn located_views(&self, ty: xr::ReferenceSpaceType) -> Option<ViewData> {
        tracy_span!();
        let session = self.openxr.session_data.get();
        let mut views = self.views.lock().unwrap();
        views.get(&session, &self.openxr, ty)
    }

    /// Like [Self::located_views], with made up views if there are none - apps can't handle
    /// failure here.
    pub fn get_views(&self, ty: xr::ReferenceSpaceType) -> ViewData {
        self.located_views(ty).unwrap_or_else(ViewData::fallback)
    }

    /// Copies an event into an app provided buffer, which may be an older, smaller VREvent_t.
    pub fn copy_event(src: &vr::VREvent_t, dst: *mut vr::VREvent_t, size: u32) {
        let len = (size as usize).min(std::mem::size_of::<vr::VREvent_t>());
        unsafe {
            std::ptr::copy_nonoverlapping(
                (src as *const vr::VREvent_t).cast::<u8>(),
                dst.cast(),
                len,
            )
        };
    }

    pub fn push_event(&self, event: vr::VREvent_t) {
        let mut events = self.events.lock().unwrap();
        events.push_back(event);
    }

    #[cfg(feature = "monado")]
    pub fn mnd_get_device_battery(&self, device_index: u32) -> Option<BatteryStatus> {
        let session_data = self.openxr.session_data.get();

        session_data
            .monado
            .as_ref()?
            .get_device_from_vr_index(device_index)
            .and_then(|device| device.battery_status().ok())
    }
}

impl vr::IVRSystem026_Interface for System {
    fn GetRecommendedRenderTargetSize(&self, width: *mut u32, height: *mut u32) {
        let views = self
            .openxr
            .instance
            .enumerate_view_configuration_views(
                self.openxr.system_id,
                xr::ViewConfigurationType::PRIMARY_STEREO,
            )
            .unwrap();

        if !width.is_null() {
            unsafe { *width = views[0].recommended_image_rect_width };
        }

        if !height.is_null() {
            unsafe { *height = views[0].recommended_image_rect_height };
        }
    }
    fn GetProjectionMatrix(&self, eye: vr::EVREye, near_z: f32, far_z: f32) -> vr::HmdMatrix44_t {
        // https://github.com/ValveSoftware/openvr/wiki/IVRSystem::GetProjectionRaw
        let [mut left, mut right, mut up, mut down] = [0.0; 4];
        self.GetProjectionRaw(eye, &mut left, &mut right, &mut down, &mut up);

        let idx = 1.0 / (right - left);
        let idy = 1.0 / (up - down);
        let idz = 1.0 / (far_z - near_z);
        let sx = right + left;
        let sy = up + down;

        vr::HmdMatrix44_t {
            m: [
                [2.0 * idx, 0.0, sx * idx, 0.0],
                [0.0, 2.0 * idy, sy * idy, 0.0],
                [0.0, 0.0, -far_z * idz, -far_z * near_z * idz],
                [0.0, 0.0, -1.0, 0.0],
            ],
        }
    }
    fn GetProjectionRaw(
        &self,
        eye: vr::EVREye,
        left: *mut f32,
        right: *mut f32,
        top: *mut f32,
        bottom: *mut f32,
    ) {
        let ty = self
            .openxr
            .session_data
            .get()
            .current_origin_as_reference_space();
        let view = self.get_views(ty).views[eye as usize];

        // Top and bottom are flipped, for some reason
        unsafe {
            *left = view.fov.angle_left.tan();
            *right = view.fov.angle_right.tan();
            *bottom = view.fov.angle_up.tan();
            *top = view.fov.angle_down.tan();
        }
    }
    // The OpenXR runtime applies lens distortion itself, so the app's view is undistorted.
    fn ComputeDistortion(
        &self,
        _: vr::EVREye,
        u: f32,
        v: f32,
        coords: *mut vr::DistortionCoordinates_t,
    ) -> bool {
        if coords.is_null() {
            return false;
        }
        unsafe {
            coords.write(vr::DistortionCoordinates_t {
                rfRed: [u, v],
                rfGreen: [u, v],
                rfBlue: [u, v],
            })
        };
        true
    }
    fn ComputeDistortionSet(
        &self,
        _: openvr::EVREye,
        _: openvr::EVRDistortionChannel,
        _: bool,
        count: u32,
        input: *const openvr::DistortionCoordinate_t,
        output: *mut openvr::DistortionCoordinate_t,
    ) -> bool {
        if count == 0 {
            return true;
        }
        if input.is_null() || output.is_null() {
            return false;
        }
        unsafe { std::ptr::copy(input, output, count as usize) };
        true
    }
    fn GetEyeToHeadTransform(&self, eye: vr::EVREye) -> vr::HmdMatrix34_t {
        let views = self.get_views(xr::ReferenceSpaceType::VIEW).views;
        let view = views[eye as usize];
        let view_rot = view.pose.orientation;

        {
            tracy_span!("conversion");
            let rot = Mat3::from_quat(Quat::from_xyzw(
                view_rot.x, view_rot.y, view_rot.z, view_rot.w,
            ))
            .transpose();

            let gen_array = |translation, rot_axis: Vec3| {
                std::array::from_fn(|i| if i == 3 { translation } else { rot_axis[i] })
            };
            vr::HmdMatrix34_t {
                m: [
                    gen_array(view.pose.position.x, rot.x_axis),
                    gen_array(view.pose.position.y, rot.y_axis),
                    gen_array(view.pose.position.z, rot.z_axis),
                ],
            }
        }
    }
    fn GetTimeSinceLastVsync(&self, seconds: *mut f32, frame_counter: *mut u64) -> bool {
        let Some((since, count)) = self.openxr.vsync_timing() else {
            return false;
        };
        unsafe {
            if let Some(seconds) = seconds.as_mut() {
                *seconds = since.as_secs_f32();
            }
            if let Some(frame_counter) = frame_counter.as_mut() {
                *frame_counter = count;
            }
        }
        true
    }
    fn GetRuntimeVersion(&self) -> *const std::os::raw::c_char {
        static VERSION: &CStr = c"2.15.6";
        VERSION.as_ptr()
    }
    fn SetSDKVersion(&self, _: u32, _: u32, _: u32) -> vr::EVRInitError {
        vr::EVRInitError::None
    }
    fn GetAppContainerFilePaths(&self, _: *mut std::os::raw::c_char, _: u32) -> u32 {
        crate::warn_unimplemented!("GetAppContainerFilePaths");
        0
    }
    fn AcknowledgeQuit_Exiting(&self) {}
    fn PerformFirmwareUpdate(&self, _: vr::TrackedDeviceIndex_t) -> vr::EVRFirmwareError {
        crate::warn_unimplemented!("PerformFirmwareUpdate");
        vr::EVRFirmwareError::Fail
    }
    fn ShouldApplicationReduceRenderingWork(&self) -> bool {
        false
    }
    fn ShouldApplicationPause(&self) -> bool {
        false
    }
    fn IsSteamVRDrawingControllers(&self) -> bool {
        false
    }
    fn IsInputAvailable(&self) -> bool {
        true
    }
    fn GetControllerAxisTypeNameFromEnum(
        &self,
        _: vr::EVRControllerAxisType,
    ) -> *const std::os::raw::c_char {
        crate::warn_unimplemented!("GetControllerAxisTypeNameFromEnum");
        static NAME: &CStr = c"Unknown";
        NAME.as_ptr()
    }
    fn GetButtonIdNameFromEnum(&self, _: vr::EVRButtonId) -> *const std::os::raw::c_char {
        crate::warn_unimplemented!("GetButtonIdNameFromEnum");
        static NAME: &CStr = c"Unknown";
        NAME.as_ptr()
    }
    fn TriggerHapticPulse(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        axis_id: u32,
        duration_us: std::ffi::c_ushort,
    ) {
        self.input
            .force(|_| Input::new(self.openxr.clone()))
            .legacy_haptic(device_index, axis_id, duration_us);
    }
    fn GetControllerStateWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        state_size: u32,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        let input = self.input.force(|_| Input::new(self.openxr.clone()));

        let Some(hand) = input.device_index_to_hand(device_index) else {
            return false;
        };

        if self.GetControllerState(device_index, state, state_size) {
            unsafe {
                *pose.as_mut().unwrap() = self
                    .input
                    .get()
                    .unwrap()
                    .get_controller_pose(hand, Some(origin), self.openxr.display_time.get(), None)
                    .unwrap_or_default();
            }
            true
        } else {
            false
        }
    }
    fn GetControllerState(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        state_size: u32,
    ) -> bool {
        self.input
            .force(|_| Input::new(self.openxr.clone()))
            .get_legacy_controller_state(device_index, state, state_size)
    }
    fn GetHiddenAreaMesh(
        &self,
        eye: vr::EVREye,
        ty: vr::EHiddenAreaMeshType,
    ) -> vr::HiddenAreaMesh_t {
        if !self.openxr.enabled_extensions.khr_visibility_mask {
            return Default::default();
        }

        debug!("GetHiddenAreaMesh: area mesh type: {ty:?}");
        let mask_ty = match ty {
            vr::EHiddenAreaMeshType::Standard => xr::VisibilityMaskTypeKHR::HIDDEN_TRIANGLE_MESH,
            vr::EHiddenAreaMeshType::Inverse => xr::VisibilityMaskTypeKHR::VISIBLE_TRIANGLE_MESH,
            vr::EHiddenAreaMeshType::LineLoop => xr::VisibilityMaskTypeKHR::LINE_LOOP,
            vr::EHiddenAreaMeshType::Max => {
                warn!("Unexpectedly got EHiddenAreaMeshType::Max - returning default area mesh");
                return Default::default();
            }
        };

        let Ok(mask) = self
            .openxr
            .session_data
            .get()
            .session
            .get_visibility_mask_khr(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                eye as u32,
                mask_ty,
            )
            .inspect_err(|e| warn!("Failed to get {ty:?} hidden area mesh for {eye:?}: {e}"))
        else {
            return Default::default();
        };

        trace!("openxr mask: {:#?} {:#?}", mask.indices, mask.vertices);

        let [mut left, mut right, mut top, mut bottom] = [0.0; 4];
        self.GetProjectionRaw(eye, &mut left, &mut right, &mut top, &mut bottom);

        // convert from indices + vertices to just vertices
        let vertices: Vec<_> = mask
            .indices
            .into_iter()
            .map(|i| {
                let v = mask.vertices[i as usize];

                // It is unclear to me why this scaling is necessary, but OpenComposite does it and
                // it seems to get games to use the mask correctly.
                let x_scaled = (v.x - left) / (right - left);
                let y_scaled = (v.y - top) / (bottom - top);
                vr::HmdVector2_t {
                    v: [x_scaled, y_scaled],
                }
            })
            .collect();

        trace!("vertices: {vertices:#?}");
        let count = vertices.len() / 3;
        // XXX: what are we supposed to do here? pVertexData is a random pointer and there's no
        // clear way for the application to deallocate it
        // fortunately it seems like applications don't call this often, so this leakage isn't a
        // huge deal.
        let vertices = Vec::leak(vertices).as_ptr();

        vr::HiddenAreaMesh_t {
            pVertexData: vertices,
            unTriangleCount: count as u32,
        }
    }

    fn GetEyeTrackedFoveationCenter(
        &self,
        _: *mut openvr::HmdVector2_t,
        _: *mut openvr::HmdVector2_t,
    ) -> bool {
        crate::warn_unimplemented!("GetEyeTrackedFoveationCenter");
        false
    }
    fn GetEyeTrackedFoveationCenterForProjection(
        &self,
        _: *const openvr::HmdMatrix44_t,
        _: *mut openvr::HmdVector2_t,
    ) -> bool {
        crate::warn_unimplemented!("GetEyeTrackedFoveationCenterForProjection");
        false
    }

    fn GetEventTypeNameFromEnum(&self, _: vr::EVREventType) -> *const std::os::raw::c_char {
        crate::warn_unimplemented!("GetEventTypeNameFromEnum");
        c"Unknown".as_ptr()
    }

    fn PollNextEventWithPoseAndOverlays(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::VREvent_t,
        size: u32,
        pose: *mut vr::TrackedDevicePose_t,
        overlay_handle: *mut vr::VROverlayHandle_t,
    ) -> bool {
        if self.PollNextEventWithPose(origin, event, size, pose) {
            return true;
        }
        let Some(overlay) = self.overlay.get() else {
            return false;
        };

        if let Some(handle) = overlay.get_next_overlay_event(event, size) {
            if !overlay_handle.is_null() {
                unsafe {
                    overlay_handle.write(handle);
                }
            }
            true
        } else {
            false
        }
    }

    fn PollNextEventWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::VREvent_t,
        size: u32,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        if let Some(queued) = self.events.lock().unwrap().pop_front() {
            unsafe {
                (&raw mut (*event).eventType).write(queued.eventType);
                (&raw mut (*event).eventAgeSeconds).write(queued.eventAgeSeconds);
                match vr::EVREventType::try_from(queued.eventType).unwrap() {
                    vr::EVREventType::Quit => {
                        const CONNECTION_LOST_OFFSET: usize =
                            std::mem::offset_of!(vr::VREvent_t, data)
                                + std::mem::offset_of!(vr::VREvent_Process_t, bConnectionLost);

                        (&raw mut (*event).data.process.pid).write(queued.data.process.pid);
                        (&raw mut (*event).data.process.oldPid).write(queued.data.process.oldPid);
                        (&raw mut (*event).data.process.bForced).write(queued.data.process.bForced);
                        if size > CONNECTION_LOST_OFFSET as u32 {
                            (&raw mut (*event).data.process.bConnectionLost)
                                .write(queued.data.process.bConnectionLost);
                        }

                        return true;
                    }
                    vr::EVREventType::KeyboardDone => {
                        Self::copy_event(&queued, event, size);
                        return true;
                    }
                    _ => return true,
                }
            }
        }

        let Some(input) = self.input.get() else {
            return false;
        };

        let got_event = input.get_next_event(size, event);
        if got_event && !pose.is_null() {
            unsafe {
                let index = (&raw const (*event).trackedDeviceIndex).read();
                pose.write(
                    input
                        .get_device_pose(index, Some(origin), self.openxr.display_time.get(), None)
                        .unwrap_or_default(),
                );
            }
        }
        got_event
    }

    fn PollNextEvent(&self, event: *mut vr::VREvent_t, size: u32) -> bool {
        self.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Seated,
            event,
            size,
            std::ptr::null_mut(),
        )
    }

    fn GetPropErrorNameFromEnum(
        &self,
        _: vr::ETrackedPropertyError,
    ) -> *const std::os::raw::c_char {
        c"Unknown error".as_ptr()
    }
    fn GetStringTrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        value: *mut std::os::raw::c_char,
        size: u32,
        error: *mut vr::ETrackedPropertyError,
    ) -> u32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting string property: {prop:?} ({device_index})");

        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0;
        }

        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::ETrackedPropertyError::Success;
        }

        let buf = if !value.is_null() && size > 0 {
            unsafe { std::slice::from_raw_parts_mut(value, size as usize) }
        } else {
            &mut []
        };

        // The HMD's identity is derived from the controller interaction profile, so make sure
        // the input module exists to answer for it.
        let input = if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            Some(self.input.force(|_| Input::new(self.openxr.clone())))
        } else {
            self.input.get()
        };

        let data = input
            .and_then(|input| input.get_device_string_tracked_property(device_index, prop))
            .or_else(|| match (device_index, prop) {
                // If the interaction profile isn't known we still need to return *something*
                // here: the Unity OpenVR sample has a hard requirement on these three properties
                // returning a value to even recognize the HMD's location.
                (
                    vr::k_unTrackedDeviceIndex_Hmd,
                    vr::ETrackedDeviceProperty::SerialNumber_String
                    | vr::ETrackedDeviceProperty::ControllerType_String,
                ) => Some(CString::new("<unknown>").unwrap()),
                // Some games sniff the HMD's tracking system/model/vendor
                // strings to decide which motion controller scheme to enable.
                // If these are empty, such games fall back to "no supported
                // controllers" or a scheme the actual controllers can't drive
                // (e.g. SUPERHOT VR's Vive scheme drops items with a trackpad
                // button) - and they typically sniff once, at startup, before
                // any controller has connected, so the HMD is the only device
                // that can answer.
                (
                    vr::k_unTrackedDeviceIndex_Hmd,
                    vr::ETrackedDeviceProperty::TrackingSystemName_String,
                ) => Some(self.hmd_identity().tracking_system.clone()),
                (
                    vr::k_unTrackedDeviceIndex_Hmd,
                    vr::ETrackedDeviceProperty::ModelNumber_String,
                ) => Some(self.hmd_identity().model.clone()),
                (
                    vr::k_unTrackedDeviceIndex_Hmd,
                    vr::ETrackedDeviceProperty::ManufacturerName_String,
                ) => Some(self.hmd_identity().manufacturer.clone()),
                _ => None,
            });

        let Some(data) = data else {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::UnknownProperty;
            }
            return 0;
        };

        let data =
            unsafe { std::slice::from_raw_parts(data.as_ptr(), data.to_bytes_with_nul().len()) };
        if buf.len() < data.len() {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::BufferTooSmall;
            }
        } else {
            buf[0..data.len()].copy_from_slice(data);
        }

        data.len() as u32
    }
    fn GetArrayTrackedDeviceProperty(
        &self,
        _: vr::TrackedDeviceIndex_t,
        _: vr::ETrackedDeviceProperty,
        _: vr::PropertyTypeTag_t,
        _: *mut std::os::raw::c_void,
        _: u32,
        error: *mut vr::ETrackedPropertyError,
    ) -> u32 {
        crate::warn_unimplemented!("GetArrayTrackedDeviceProperty");
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::ETrackedPropertyError::UnknownProperty;
        }
        0
    }
    fn GetMatrix34TrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> vr::HmdMatrix34_t {
        debug!(target: log_tags::TRACKED_PROP, "requesting matrix property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return Default::default();
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::UnknownProperty;
        }
        Default::default()
    }
    fn GetUint64TrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> u64 {
        debug!(target: log_tags::TRACKED_PROP, "requesting uint64 property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0;
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::Success;
        }

        self.input
            .get()
            .and_then(|input| input.get_device_uint_tracked_property(device_index, prop))
            .unwrap_or_else(|| {
                if let Some(err) = unsafe { err.as_mut() } {
                    *err = vr::ETrackedPropertyError::UnknownProperty;
                }
                0
            })
    }
    fn GetInt32TrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> i32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting int32 property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0;
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::Success;
        }
        self.input
            .get()
            .and_then(|input| input.get_device_int_tracked_property(device_index, prop))
            .unwrap_or_else(|| {
                if let Some(err) = unsafe { err.as_mut() } {
                    *err = vr::ETrackedPropertyError::UnknownProperty;
                }
                0
            })
    }
    fn GetFloatTrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        error: *mut vr::ETrackedPropertyError,
    ) -> f32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting float property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0.0;
        }

        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::ETrackedPropertyError::Success;
        }

        match prop {
            #[cfg(feature = "monado")]
            vr::ETrackedDeviceProperty::DeviceBatteryPercentage_Float => {
                let Some(battery) = self.mnd_get_device_battery(device_index) else {
                    if let Some(error) = unsafe { error.as_mut() } {
                        *error = vr::ETrackedPropertyError::ValueNotProvidedByDevice;
                    }
                    return 0.0;
                };

                if !battery.present {
                    if let Some(error) = unsafe { error.as_mut() } {
                        *error = vr::ETrackedPropertyError::ValueNotProvidedByDevice;
                    }
                    return 0.0;
                }

                return battery.charge;
            }
            _ => {}
        }

        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            match prop {
                vr::ETrackedDeviceProperty::UserIpdMeters_Float => {
                    let views = self.get_views(xr::ReferenceSpaceType::VIEW).views;
                    return views[1].pose.position.x - views[0].pose.position.x;
                }
                vr::ETrackedDeviceProperty::DisplayFrequency_Float => {
                    return self.openxr.get_refresh_rate();
                }
                _ => {}
            }
        }

        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::ETrackedPropertyError::UnknownProperty;
        }
        0.0
    }

    fn GetBoolTrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> bool {
        debug!(target: log_tags::TRACKED_PROP, "requesting bool property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return false;
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::Success;
        }

        match prop {
            #[cfg(feature = "monado")]
            vr::ETrackedDeviceProperty::DeviceProvidesBatteryStatus_Bool => {
                let Some(d) = self.mnd_get_device_battery(device_index) else {
                    if let Some(err) = unsafe { err.as_mut() } {
                        *err = vr::ETrackedPropertyError::ValueNotProvidedByDevice;
                    }
                    return false;
                };

                return d.present;
            }
            #[cfg(feature = "monado")]
            vr::ETrackedDeviceProperty::DeviceIsCharging_Bool => {
                let Some(d) = self.mnd_get_device_battery(device_index) else {
                    if let Some(err) = unsafe { err.as_mut() } {
                        *err = vr::ETrackedPropertyError::ValueNotProvidedByDevice;
                    }
                    return false;
                };

                if !d.present {
                    if let Some(err) = unsafe { err.as_mut() } {
                        *err = vr::ETrackedPropertyError::ValueNotProvidedByDevice;
                    }
                    return false;
                }

                return d.charging;
            }
            _ => {}
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::UnknownProperty;
        }
        false
    }

    fn IsTrackedDeviceConnected(&self, device_index: vr::TrackedDeviceIndex_t) -> bool {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => true,
            _ => self
                .input
                .get()
                .is_some_and(|input| input.is_device_connected(device_index)),
        }
    }

    fn GetTrackedDeviceClass(&self, index: vr::TrackedDeviceIndex_t) -> vr::ETrackedDeviceClass {
        match index {
            vr::k_unTrackedDeviceIndex_Hmd => vr::ETrackedDeviceClass::HMD,
            _ => self
                .input
                .get()
                .and_then(|input| input.device_index_to_tracked_device_class(index))
                .unwrap_or(vr::ETrackedDeviceClass::Invalid),
        }
    }

    fn GetControllerRoleForTrackedDeviceIndex(
        &self,
        index: vr::TrackedDeviceIndex_t,
    ) -> vr::ETrackedControllerRole {
        let Some(input) = self.input.get() else {
            return vr::ETrackedControllerRole::Invalid;
        };
        input
            .device_index_to_hand(index)
            .map_or(vr::ETrackedControllerRole::Invalid, |hand| hand.into())
    }

    fn GetTrackedDeviceIndexForControllerRole(
        &self,
        role: vr::ETrackedControllerRole,
    ) -> vr::TrackedDeviceIndex_t {
        let Some(input) = self.input.get() else {
            return vr::k_unTrackedDeviceIndexInvalid;
        };

        Hand::try_from(role).map_or(vr::k_unTrackedDeviceIndexInvalid, |hand| {
            input
                .get_controller_device_index(hand)
                .unwrap_or(vr::k_unTrackedDeviceIndexInvalid)
        })
    }
    fn ApplyTransform(
        &self,
        output: *mut vr::TrackedDevicePose_t,
        input: *const vr::TrackedDevicePose_t,
        transform: *const vr::HmdMatrix34_t,
    ) {
        if output.is_null() || input.is_null() || transform.is_null() {
            return;
        }

        // copy first: the caller may pass the same pointer for input and output
        let input_pose = unsafe { *input };
        let transform = xr::Posef::from(unsafe { *transform });
        let pose = xr::Posef::from(input_pose.mDeviceToAbsoluteTracking);

        let rot = Quat::from_xyzw(
            transform.orientation.x,
            transform.orientation.y,
            transform.orientation.z,
            transform.orientation.w,
        );
        let position = rot * Vec3::new(pose.position.x, pose.position.y, pose.position.z);
        let orientation = rot
            * Quat::from_xyzw(
                pose.orientation.x,
                pose.orientation.y,
                pose.orientation.z,
                pose.orientation.w,
            );

        let result = xr::Posef {
            position: xr::Vector3f {
                x: transform.position.x + position.x,
                y: transform.position.y + position.y,
                z: transform.position.z + position.z,
            },
            orientation: xr::Quaternionf {
                x: orientation.x,
                y: orientation.y,
                z: orientation.z,
                w: orientation.w,
            },
        };

        unsafe {
            (*output) = input_pose;
            (*output).mDeviceToAbsoluteTracking = result.into();
        }
    }
    fn GetTrackedDeviceActivityLevel(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
    ) -> vr::EDeviceActivityLevel {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => vr::EDeviceActivityLevel::UserInteraction,
            x if self
                .input
                .get()
                .is_some_and(|input| input.device_index_to_hand(x).is_some()) =>
            {
                if self.IsTrackedDeviceConnected(x) {
                    vr::EDeviceActivityLevel::UserInteraction
                } else {
                    vr::EDeviceActivityLevel::Unknown
                }
            }
            _ => vr::EDeviceActivityLevel::Unknown,
        }
    }
    fn GetSortedTrackedDeviceIndicesOfClass(
        &self,
        _: vr::ETrackedDeviceClass,
        _: *mut vr::TrackedDeviceIndex_t,
        _: u32,
        _: vr::TrackedDeviceIndex_t,
    ) -> u32 {
        0
    }
    fn GetRawZeroPoseToStandingAbsoluteTrackingPose(&self) -> vr::HmdMatrix34_t {
        xr::Posef::IDENTITY.into()
    }
    fn GetSeatedZeroPoseToStandingAbsoluteTrackingPose(&self) -> vr::HmdMatrix34_t {
        xr::Posef::IDENTITY.into()
    }
    fn GetDeviceToAbsoluteTrackingPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        seconds_to_photon_from_now: f32,
        pose_array: *mut vr::TrackedDevicePose_t,
        pose_count: u32,
    ) {
        if pose_array.is_null() || pose_count == 0 {
            return;
        }
        self.input
            .force(|_| Input::new(self.openxr.clone()))
            .get_poses(
                unsafe { std::slice::from_raw_parts_mut(pose_array, pose_count as usize) },
                Some(origin),
                self.openxr.time_from_now(seconds_to_photon_from_now),
            );
    }
    fn SetDisplayVisibility(&self, _: bool) -> bool {
        // Act as if we're limited to direct mode
        false
    }
    fn IsDisplayOnDesktop(&self) -> bool {
        // Direct mode
        false
    }
    fn GetOutputDevice(
        &self,
        device: *mut u64,
        texture_type: vr::ETextureType,
        instance: *mut vr::VkInstance_T,
    ) {
        if texture_type != vr::ETextureType::Vulkan {
            // Proton doesn't seem to properly translate this function, but it doesn't appear to
            // actually matter.
            log::error!("Unsupported texture type: {texture_type:?}");
            return;
        }

        unsafe {
            *device = self
                .openxr
                .instance
                .vulkan_graphics_device(self.openxr.system_id, instance as _)
                .expect("Failed to get vulkan physical device") as _;
        }
    }
    fn GetDXGIOutputInfo(&self, output_info: *mut i32) {
        crate::warn_unimplemented!("GetDXGIOutputInfo");
        if !output_info.is_null() {
            unsafe { output_info.write(0) };
        }
    }
    fn GetD3D9AdapterIndex(&self) -> i32 {
        crate::warn_unimplemented!("GetD3D9AdapterIndex");
        0
    }
}

impl vr::IVRSystem021On022 for System {
    fn ResetSeatedZeroPose(&self) {
        self.openxr
            .reset_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    }
}

impl vr::IVRSystem020On021 for System {
    fn AcknowledgeQuit_UserPrompt(&self) {}
}

impl vr::IVRSystem019On020 for System {
    fn DriverDebugRequest(
        &self,
        _un_device_index: vr::TrackedDeviceIndex_t,
        _pch_request: *const std::os::raw::c_char,
        response_buffer: *mut std::os::raw::c_char,
        response_buffer_size: u32,
    ) -> u32 {
        crate::warn_unimplemented!("DriverDebugRequest");
        if !response_buffer.is_null() && response_buffer_size > 0 {
            unsafe { *response_buffer = 0 };
        }
        0
    }
}

impl vr::IVRSystem017On019 for System {
    fn IsInputFocusCapturedByAnotherProcess(&self) -> bool {
        false
    }
    fn ReleaseInputFocus(&self) {}
    fn CaptureInputFocus(&self) -> bool {
        true
    }
}

impl vr::IVRSystem016On017 for System {
    fn GetOutputDevice(&self, device: *mut u64, texture_type: vr::ETextureType) {
        vr::IVRSystem022_Interface::GetOutputDevice(
            self,
            device,
            texture_type,
            std::ptr::null_mut(),
        )
    }
}

impl vr::IVRSystem014On015 for System {
    fn GetProjectionMatrix(
        &self,
        eye: vr::EVREye,
        near_z: f32,
        far_z: f32,
        _proj_type: vr::EGraphicsAPIConvention,
    ) -> vr::HmdMatrix44_t {
        // According to this bug: https://github.com/ValveSoftware/openvr/issues/70 the projection type
        // is straight up ignored in SteamVR anyway, lol. Bug for bug compat!

        <Self as vr::IVRSystem022_Interface>::GetProjectionMatrix(self, eye, near_z, far_z)
    }
}

impl vr::IVRSystem012On014 for System {
    fn ComputeDistortion(&self, eye: vr::EVREye, u: f32, v: f32) -> vr::DistortionCoordinates_t {
        let mut ret = vr::DistortionCoordinates_t::default();
        <Self as vr::IVRSystem022_Interface>::ComputeDistortion(self, eye, u, v, &mut ret);
        ret
    }

    fn GetHiddenAreaMesh(&self, eye: vr::EVREye) -> vr::HiddenAreaMesh_t {
        <Self as vr::IVRSystem022_Interface>::GetHiddenAreaMesh(
            self,
            eye,
            vr::EHiddenAreaMeshType::Standard,
        )
    }

    fn GetControllerState(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
    ) -> bool {
        <Self as vr::IVRSystem022_Interface>::GetControllerState(
            self,
            device_index,
            state,
            std::mem::size_of::<vr::VRControllerState_t>() as u32,
        )
    }

    fn GetControllerStateWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        device_pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        <Self as vr::IVRSystem022_Interface>::GetControllerStateWithPose(
            self,
            origin,
            device_index,
            state,
            std::mem::size_of::<vr::VRControllerState_t>() as u32,
            device_pose,
        )
    }
}

impl vr::IVRSystem011On012 for System {
    fn PerformanceTestEnableCapture(&self, _: bool) {
        crate::warn_unimplemented!("PerformanceTestEnableCapture");
    }

    fn PerformanceTestReportFidelityLevelChange(&self, _: i32) {
        crate::warn_unimplemented!("PerformanceTestReportFidelityLevelChange");
    }
}

impl vr::IVRSystem009On011 for System {
    fn PollNextEvent(&self, event: *mut vr::vr_0_9_12::VREvent_t) -> bool {
        self.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Seated,
            event,
            std::ptr::null_mut(),
        )
    }

    fn PollNextEventWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::vr_0_9_12::VREvent_t,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        let mut e = vr::VREvent_t::default();
        let ret = <Self as vr::IVRSystem022_Interface>::PollNextEventWithPose(
            self,
            origin,
            &mut e,
            std::mem::size_of_val(&e) as u32,
            pose,
        );

        if ret && !event.is_null() {
            let Ok(event_type) = vr::EVREventType::try_from(e.eventType) else {
                error!("Unhandled event type for 0.9.12: {}", e.eventType);
                return false;
            };

            unsafe {
                (&raw mut (*event).eventType).write(event_type);
                (&raw mut (*event).trackedDeviceIndex).write(e.trackedDeviceIndex);
                match event_type {
                    vr::EVREventType::ButtonPress
                    | vr::EVREventType::ButtonUnpress
                    | vr::EVREventType::ButtonTouch
                    | vr::EVREventType::ButtonUntouch => {
                        (&raw mut (*event).data.controller).write(e.data.controller);
                    }
                    vr::EVREventType::Quit => {
                        (&raw mut (*event).data.process.pid).write(e.data.process.pid);
                        (&raw mut (*event).data.process.oldPid).write(e.data.process.oldPid);
                        (&raw mut (*event).data.process.bForced).write(e.data.process.bForced);
                    }
                    other => {
                        error!("Unhandled event type data for 0.9.12: {other:?}");
                        return false;
                    }
                }
            }
        }

        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clientcore::Injector, openxr_data::OpenXrData};
    use std::ffi::CStr;
    use vr::IVRSystem022_Interface;

    #[test]
    fn distortion_is_identity() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());

        let mut coords = vr::DistortionCoordinates_t::default();
        assert!(system.ComputeDistortion(vr::EVREye::Left, 0.25, 0.75, &mut coords));
        assert_eq!(coords.rfRed, [0.25, 0.75]);
        assert_eq!(coords.rfGreen, [0.25, 0.75]);
        assert_eq!(coords.rfBlue, [0.25, 0.75]);
        assert!(!system.ComputeDistortion(vr::EVREye::Left, 0.0, 0.0, std::ptr::null_mut()));

        let input = [
            vr::DistortionCoordinate_t { u: 0.1, v: 0.2 },
            vr::DistortionCoordinate_t { u: 0.9, v: 0.8 },
        ];
        let mut output = [vr::DistortionCoordinate_t::default(); 2];
        assert!(vr::IVRSystem026_Interface::ComputeDistortionSet(
            &system,
            vr::EVREye::Right,
            vr::EVRDistortionChannel::Green,
            false,
            2,
            input.as_ptr(),
            output.as_mut_ptr(),
        ));
        assert_eq!(output.map(|c| (c.u, c.v)), [(0.1, 0.2), (0.9, 0.8)]);
    }

    #[test]
    fn device_to_absolute_tracking_pose_takes_any_seconds() {
        // fakexr doesn't have XR_KHR_convert_timespec_time, so this also covers its absence.
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());

        for seconds in [0.0, -0.011, f32::NAN, f32::NEG_INFINITY] {
            let mut poses = [vr::TrackedDevicePose_t::default(); 2];
            system.GetDeviceToAbsoluteTrackingPose(
                vr::ETrackingUniverseOrigin::Seated,
                seconds,
                poses.as_mut_ptr(),
                poses.len() as u32,
            );
        }
        system.GetDeviceToAbsoluteTrackingPose(
            vr::ETrackingUniverseOrigin::Seated,
            0.0,
            std::ptr::null_mut(),
            0,
        );
    }

    #[test]
    fn views_survive_locate_failure() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());
        let projection = || {
            let [mut left, mut right, mut top, mut bottom] = [f32::NAN; 4];
            system.GetProjectionRaw(
                vr::EVREye::Left,
                &mut left,
                &mut right,
                &mut top,
                &mut bottom,
            );
            [left, right, top, bottom]
        };

        // Never located: made up views.
        fakexr::set_locate_views_error(Some(xr::sys::Result::ERROR_RUNTIME_FAILURE));
        assert!(system.located_views(xr::ReferenceSpaceType::VIEW).is_none());
        for (got, expected) in projection().into_iter().zip([-1.0, 1.0, -1.0, 1.0]) {
            assert!((got - expected).abs() < 1e-6, "{got} != {expected}");
        }
        let eye_x = system.GetEyeToHeadTransform(vr::EVREye::Left).m[0][3];
        assert!(eye_x < 0.0, "{eye_x}");

        // fakexr's views have a zero FOV.
        fakexr::set_locate_views_error(None);
        assert_eq!(projection(), [0.0; 4]);

        // Failing again on the next frame: the last located views.
        fakexr::set_locate_views_error(Some(xr::sys::Result::ERROR_RUNTIME_FAILURE));
        system.reset_views();
        assert_eq!(projection(), [0.0; 4]);
        assert!(system.located_views(xr::ReferenceSpaceType::VIEW).is_some());
    }

    #[test]
    fn hidden_area_mesh_runtime_error() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        assert!(xr.enabled_extensions.khr_visibility_mask);
        let system = System::new(xr, &Injector::default());

        // fakexr fails every visibility mask request.
        let mesh = vr::IVRSystem026_Interface::GetHiddenAreaMesh(
            &system,
            vr::EVREye::Left,
            vr::EHiddenAreaMeshType::Standard,
        );
        assert!(mesh.pVertexData.is_null());
        assert_eq!(mesh.unTriangleCount, 0);
    }

    #[test]
    fn unity_required_properties() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let injector = Injector::default();
        let input = Arc::new(Input::new(xr.clone()));
        let system = System::new(xr, &injector);

        system.input.set(Arc::downgrade(&input));

        let test_prop = |property| {
            let mut err = vr::ETrackedPropertyError::Success;
            let len = system.GetStringTrackedDeviceProperty(
                vr::k_unTrackedDeviceIndex_Hmd,
                property,
                std::ptr::null_mut(),
                0,
                &mut err,
            );
            assert_eq!(err, vr::ETrackedPropertyError::BufferTooSmall);
            assert!(len > 0);
            let mut buf = vec![0; len as usize];

            let len = system.GetStringTrackedDeviceProperty(
                vr::k_unTrackedDeviceIndex_Hmd,
                property,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut err,
            );
            assert_eq!(err, vr::ETrackedPropertyError::Success);
            assert_eq!(len, buf.len() as u32);

            let slice = unsafe { std::slice::from_raw_parts(buf.as_ptr() as _, buf.len()) };
            CStr::from_bytes_with_nul(slice)
                .expect("Failed to convert returned buffer for {property:?} to CStr");
        };

        test_prop(vr::ETrackedDeviceProperty::SerialNumber_String);
        test_prop(vr::ETrackedDeviceProperty::ManufacturerName_String);
        test_prop(vr::ETrackedDeviceProperty::ControllerType_String);
        test_prop(vr::ETrackedDeviceProperty::TrackingSystemName_String);
        test_prop(vr::ETrackedDeviceProperty::ModelNumber_String);
    }

    #[test]
    fn display_frequency_fallback_is_hz() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        xr.display_period_nanos
            .store(11_111_111, std::sync::atomic::Ordering::Relaxed);

        let mut err = vr::ETrackedPropertyError::Success;
        let freq = system.GetFloatTrackedDeviceProperty(
            vr::k_unTrackedDeviceIndex_Hmd,
            vr::ETrackedDeviceProperty::DisplayFrequency_Float,
            &mut err,
        );
        assert_eq!(err, vr::ETrackedPropertyError::Success);
        assert!((freq - 90.0).abs() < 0.01, "{freq}");
    }

    #[test]
    fn test_session_stop() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let injector = Injector::default();
        let system = Arc::new(System::new(xr.clone(), &injector));

        xr.system.set(Arc::downgrade(&system));

        xr.session_data.get().session.request_exit().unwrap();
        xr.poll_events();
        let mut event = vr::VREvent_t::default();
        assert!(system.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Standing,
            &mut event,
            std::mem::size_of_val(&event) as u32,
            std::ptr::null_mut()
        ));
        assert_eq!(event.eventType, vr::EVREventType::Quit as u32);
    }

    #[test]
    fn hmd_identity_available_before_first_frame() {
        fakexr::set_default_interaction_profile(
            fakexr::UserPath::RightHand,
            Some("/interaction_profiles/oculus/touch_controller"),
        );
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        fakexr::set_default_interaction_profile(fakexr::UserPath::RightHand, None);

        let injector = Injector::default();
        let input = Arc::new(Input::new(xr.clone()));
        let system = System::new(xr.clone(), &injector);
        system.input.set(Arc::downgrade(&input));
        xr.input.set(Arc::downgrade(&input));

        let get_prop = |property| {
            let mut err = vr::ETrackedPropertyError::Success;
            let mut buf = vec![0; 64];
            let len = system.GetStringTrackedDeviceProperty(
                vr::k_unTrackedDeviceIndex_Hmd,
                property,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut err,
            );
            assert_eq!(err, vr::ETrackedPropertyError::Success);
            let slice = unsafe { std::slice::from_raw_parts(buf.as_ptr() as _, len as usize) };
            CStr::from_bytes_with_nul(slice).unwrap().to_owned()
        };

        assert_eq!(
            get_prop(vr::ETrackedDeviceProperty::TrackingSystemName_String).as_c_str(),
            c"oculus"
        );
        assert_eq!(
            get_prop(vr::ETrackedDeviceProperty::ManufacturerName_String).as_c_str(),
            c"Oculus"
        );
        assert_eq!(
            get_prop(vr::ETrackedDeviceProperty::ModelNumber_String).as_c_str(),
            c"Oculus Quest2"
        );
        assert_eq!(
            get_prop(vr::ETrackedDeviceProperty::ControllerType_String).as_c_str(),
            c"rift"
        );
    }
}
