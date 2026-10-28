use crate::{
    clientcore::{Injected, Injector},
    input::Input,
    openxr_data::{Hand, RealOpenXrData, SessionData},
    overlay::OverlayMan,
    tracy_span,
};

#[cfg(feature = "monado")]
use libmonado::BatteryStatus;

use glam::{Mat3, Quat, Vec2, Vec3};
use log::{debug, info, trace, warn};
use openvr as vr;
use openxr as xr;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

#[derive(Copy, Clone)]
pub struct ViewData {
    pub flags: xr::ViewStateFlags,
    pub views: [xr::View; 2],
}

/// Index of the eye's view. Like the OpenVR sample driver, anything that isn't the left eye is the
/// right eye.
fn view_index(eye: vr::EVREye) -> usize {
    usize::from(eye != vr::EVREye::Left)
}

#[derive(Copy, Clone)]
struct ViewDataViewSpace {
    data: ViewData,
    /// The VIEW space orientation of each view, which was removed from the views in every space.
    canting: [Quat; 2],
}

impl ViewData {
    /// Stand-in for views the runtime never located: a typical IPD and a 90 degree FOV.
    fn fallback() -> Self {
        let view = |x| xr::View {
            pose: xr::Posef {
                orientation: xr::Quaternionf::IDENTITY,
                position: xr::Vector3f { x, y: 0.0, z: 0.0 },
            },
            fov: FALLBACK_FOV,
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

/// Locates the PRIMARY_STEREO views in `space`. Monado returns success without writing the views
/// or their flags when it can't locate the headset, so they start out zeroed: no valid pose and
/// an invalid FOV rather than whatever was in memory.
fn locate_views(
    session: &SessionData,
    display_time: xr::Time,
    space: &xr::Space,
) -> xr::Result<(xr::ViewStateFlags, [xr::View; 2])> {
    let session = &session.session;
    let info = xr::sys::ViewLocateInfo {
        ty: xr::sys::ViewLocateInfo::TYPE,
        next: std::ptr::null(),
        view_configuration_type: xr::ViewConfigurationType::PRIMARY_STEREO,
        display_time,
        space: space.as_raw(),
    };
    let mut state = xr::sys::ViewState {
        ty: xr::sys::ViewState::TYPE,
        next: std::ptr::null_mut(),
        view_state_flags: xr::ViewStateFlags::EMPTY,
    };
    let mut views = [xr::sys::View {
        ty: xr::sys::View::TYPE,
        next: std::ptr::null_mut(),
        pose: Default::default(),
        fov: Default::default(),
    }; 2];
    let mut count = 0;
    let result = unsafe {
        (session.instance().fp().locate_views)(
            session.as_raw(),
            &info,
            &mut state,
            views.len() as u32,
            &mut count,
            views.as_mut_ptr(),
        )
    };
    if result.into_raw() < 0 {
        return Err(result);
    }
    // PRIMARY_STEREO always has two views, but a runtime getting that wrong shouldn't panic.
    if count != 2 {
        crate::warn_once!("Expected 2 views, got {}", count);
        return Err(xr::sys::Result::ERROR_RUNTIME_FAILURE);
    }

    // Like openxr's locate_views, leave out what isn't valid.
    let flags = state.view_state_flags;
    let views = views.map(|view| xr::View {
        pose: xr::Posef {
            orientation: if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
                view.pose.orientation
            } else {
                Default::default()
            },
            position: if flags.contains(xr::ViewStateFlags::POSITION_VALID) {
                view.pose.position
            } else {
                Default::default()
            },
        },
        fov: view.fov,
    });
    Ok((flags, views))
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
        last_valid_fovs: &mut [Option<xr::Fovf>; 2],
    ) -> xr::Result<ViewData> {
        let view = match self.view {
            Some(view) => view,
            None => *self.view.insert(Self::get_views_view_space(
                session,
                display_time,
                last_valid_fovs,
            )?),
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
        let data = Self::get_views_other_space(session, display_time, ty, &view)?;
        Ok(*cached.insert(data))
    }

    fn get_views_view_space(
        session: &SessionData,
        display_time: xr::Time,
        last_valid_fovs: &mut [Option<xr::Fovf>; 2],
    ) -> xr::Result<ViewDataViewSpace> {
        let (flags, mut views) = locate_views(
            session,
            display_time,
            session.get_space_from_type(xr::ReferenceSpaceType::VIEW),
        )?;

        for (view, last_valid_fov) in views.iter_mut().zip(last_valid_fovs) {
            if is_valid_fov(&view.fov) {
                *last_valid_fov = Some(view.fov);
            } else {
                crate::warn_once!(
                    "Runtime reported an invalid FOV ({:?}), using the last valid one",
                    view.fov
                );
                view.fov = last_valid_fov.unwrap_or(FALLBACK_FOV);
            }
        }

        // Canted-view handling.
        //
        // Runtimes for HMDs with canted displays (Pimax, Bigscreen Beyond etc.)
        // report per-eye rotations in the VIEW reference space to describe the
        // tilt of each panel. By default xrizer normalises those away by forcing
        // VIEW-space views to identity ("parallel views") and multiplying the
        // inverse back into LOCAL/STAGE views, since many games assume parallel
        // views. The FOV is widened to still cover the whole panel.
        //
        // Opt-in via XRIZER_PRESERVE_VIEW_CANTING=1 to preserve the canted
        // orientations end-to-end (avoids rendering the wider FOV; may confuse
        // games that assume parallel views).
        static PRESERVE_CANTING: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            std::env::var("XRIZER_PRESERVE_VIEW_CANTING")
                .ok()
                .filter(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
                .is_some()
        });

        let canting: [Quat; 2] = if *PRESERVE_CANTING {
            [Quat::IDENTITY, Quat::IDENTITY]
        } else {
            views
                .iter_mut()
                .map(|view| {
                    let o = &mut view.pose.orientation;
                    let mut canting = if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
                        Quat::from_xyzw(o.x, o.y, o.z, o.w)
                    } else {
                        Quat::IDENTITY
                    };
                    *o = xr::Quaternionf::IDENTITY;
                    let fov = parallel_fov(view.fov, canting);
                    if is_valid_fov(&fov) {
                        view.fov = fov;
                    } else {
                        // Only a broken orientation gets here. Leave it out, with the FOV as is.
                        crate::warn_once!(
                            "Couldn't make a view with orientation {:?} parallel, ignoring it",
                            canting
                        );
                        canting = Quat::IDENTITY;
                    }
                    canting
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap()
        };

        Ok(ViewDataViewSpace {
            data: ViewData { flags, views },
            canting,
        })
    }

    fn get_views_other_space(
        session: &SessionData,
        display_time: xr::Time,
        ty: xr::ReferenceSpaceType,
        view_space: &ViewDataViewSpace,
    ) -> xr::Result<ViewData> {
        let (flags, mut views) =
            locate_views(session, display_time, session.get_space_from_type(ty))?;

        for ((view, canting), view_space_view) in views
            .iter_mut()
            .zip(view_space.canting)
            .zip(&view_space.data.views)
        {
            // The FOV doesn't depend on the space, and was already checked and widened.
            view.fov = view_space_view.fov;
            let rot = &mut view.pose.orientation;
            let quat = Quat::from_xyzw(rot.x, rot.y, rot.z, rot.w);
            // rotate the inverse of the view space view rotation by this space's
            // view orientation to remove the canting from the displays in this space
            let adjusted_rot = quat * canting.inverse();
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
    /// The last usable FOV the runtime reported for each view.
    last_valid_fovs: [Option<xr::Fovf>; 2],
    /// The IPD the app last got, see [Self::report_ipd].
    reported_ipd: Option<f32>,
}

/// An IPD change under this isn't reported: it's below what a headset's slider reports, so noise
/// in the located view positions doesn't flood the app with events.
const IPD_CHANGE_THRESHOLD: f32 = 0.0005;

impl Views {
    /// Sends the app IpdChanged when the headset's IPD changes, like SteamVR when the slider
    /// moves. The first IPD located is the one the app starts with.
    fn report_ipd(&mut self, openxr: &RealOpenXrData, data: &ViewData) {
        if !data.flags.contains(xr::ViewStateFlags::POSITION_VALID) {
            return;
        }
        let ipd = data.views[1].pose.position.x - data.views[0].pose.position.x;
        if !ipd.is_finite() || ipd <= 0.0 {
            return;
        }
        match self.reported_ipd {
            None => self.reported_ipd = Some(ipd),
            Some(last) if (ipd - last).abs() >= IPD_CHANGE_THRESHOLD => {
                info!(
                    "IPD changed from {:.1} mm to {:.1} mm",
                    last * 1000.0,
                    ipd * 1000.0
                );
                self.reported_ipd = Some(ipd);
                openxr.push_event(
                    vr::EVREventType::IpdChanged,
                    vr::VREvent_Data_t {
                        ipd: vr::VREvent_Ipd_t { ipdMeters: ipd },
                    },
                );
            }
            Some(_) => {}
        }
    }

    /// The rotation removed from each view to make the views parallel, none if the runtime never
    /// located them.
    fn canting(&mut self, session: &SessionData, openxr: &RealOpenXrData) -> [Quat; 2] {
        self.get(session, openxr, xr::ReferenceSpaceType::VIEW);
        self.frame
            .view
            .or(self.last.view)
            .map_or([Quat::IDENTITY; 2], |view| view.canting)
    }

    fn get(
        &mut self,
        session: &SessionData,
        openxr: &RealOpenXrData,
        ty: xr::ReferenceSpaceType,
    ) -> Option<ViewData> {
        // Every call fails once the session is lost.
        if openxr.is_lost() {
            return self.last.cached(ty);
        }
        let fovs = &mut self.last_valid_fovs;
        let result = self
            .frame
            .get_views(session, openxr.get_display_time(), ty, fovs)
            .or_else(|e| match openxr.now() {
                // Before the first frame the display time can be long gone.
                Some(now) => self.frame.get_views(session, now, ty, fovs),
                None => Err(e),
            });
        match result {
            Ok(data) => {
                if std::mem::take(&mut self.failing) {
                    info!("Locating views works again");
                }
                if let Some(view) = self.frame.view {
                    self.report_ipd(openxr, &view.data);
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
                // A loss has to reach the app.
                openxr.handle_error("xrLocateViews", e);
                if !std::mem::replace(&mut self.failing, true) {
                    warn!("Couldn't locate views, using the last ones: {e}");
                }
                self.last.cached(ty)
            }
        }
    }

    /// The views for a frame displayed at `time` rather than the current one, which aren't
    /// cached. Falls back to the current frame's views.
    fn get_at(
        &mut self,
        session: &SessionData,
        openxr: &RealOpenXrData,
        ty: xr::ReferenceSpaceType,
        time: xr::Time,
    ) -> Option<ViewData> {
        if !openxr.is_lost() {
            let mut cache = ViewCache::default();
            match cache.get_views(session, time, ty, &mut self.last_valid_fovs) {
                Ok(data) => return Some(data),
                Err(e) => openxr.handle_error("xrLocateViews", e),
            }
        }
        self.get(session, openxr, ty)
    }
}

/// Used until the runtime reports a valid FOV, or locates the views at all.
const FALLBACK_FOV: xr::Fovf = xr::Fovf {
    angle_left: -std::f32::consts::FRAC_PI_4,
    angle_right: std::f32::consts::FRAC_PI_4,
    angle_up: std::f32::consts::FRAC_PI_4,
    angle_down: -std::f32::consts::FRAC_PI_4,
};

/// Zero, inverted or non-finite FOVs give games NaN projections.
fn is_valid_fov(fov: &xr::Fovf) -> bool {
    let angles = [
        fov.angle_left,
        fov.angle_right,
        fov.angle_up,
        fov.angle_down,
    ];
    angles
        .iter()
        .all(|angle| angle.abs() < std::f32::consts::FRAC_PI_2)
        && fov.angle_left < fov.angle_right
        && fov.angle_down < fov.angle_up
}

/// Where the direction through `tangents` on the z = -1 plane of a view rotated by `canting`
/// crosses the z = -1 plane of the unrotated view.
fn parallel_tangents(canting: Quat, tangents: Vec2) -> Vec2 {
    let dir = canting * tangents.extend(-1.0);
    // Directions at or behind the eye's plane only come from extreme FOVs and canting.
    let depth = (-dir.z).max(0.01);
    Vec2::new(dir.x, dir.y) / depth
}

/// The smallest FOV of an unrotated view that covers the FOV of a view rotated by `canting`.
fn parallel_fov(fov: xr::Fovf, canting: Quat) -> xr::Fovf {
    if canting == Quat::IDENTITY {
        return fov;
    }
    // A rotated frustum still projects to a quadrilateral spanned by its corners.
    let corners = [
        (fov.angle_left, fov.angle_down),
        (fov.angle_left, fov.angle_up),
        (fov.angle_right, fov.angle_down),
        (fov.angle_right, fov.angle_up),
    ]
    .map(|(x, y)| parallel_tangents(canting, Vec2::new(x.tan(), y.tan())));
    let min = corners.into_iter().reduce(Vec2::min).unwrap();
    let max = corners.into_iter().reduce(Vec2::max).unwrap();
    xr::Fovf {
        angle_left: min.x.atan(),
        angle_right: max.x.atan(),
        angle_up: max.y.atan(),
        angle_down: min.y.atan(),
    }
}

#[derive(macros::InterfaceImpl)]
#[interface = "IVRSystem"]
#[versions(026, 023, 022, 021, 020, 019, 017, 016, 015, 014, 012, 011, 010, 009)]
pub struct System {
    openxr: Arc<RealOpenXrData>, // We don't need to test session restarting.
    events: Mutex<VecDeque<QueuedEvent>>,
    input: Injected<Input<crate::compositor::Compositor>>,
    overlay: Injected<OverlayMan>,
    vtables: Vtables,
    views: Mutex<Views>,
    hidden_area_meshes: Mutex<HashMap<(u32, u32), &'static [vr::HmdVector2_t]>>,
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
            hidden_area_meshes: Mutex::default(),
        }
    }

    /// See [crate::openxr_data::OpenXrData::pump]. Legacy actions, which make controllers show up
    /// for apps without an action manifest, are set up by Input, so it has to exist.
    fn pump(&self) {
        self.input.force(|_| Input::new(self.openxr.clone()));
        self.openxr.pump();
    }

    /// Drops the cached views. Those in LOCAL and STAGE are relative to the tracking origin, so
    /// the last located ones in them are stale too.
    pub fn clear_views(&self) {
        let mut views = self.views.lock().unwrap();
        views.frame = ViewCache::default();
        views.last.local = None;
        views.last.stage = None;
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

    /// Like [Self::located_views], for the frame displayed at `time`. That is an earlier frame
    /// than the current one when the app waited for the next frame before handing this one off.
    pub fn located_views_at(&self, ty: xr::ReferenceSpaceType, time: xr::Time) -> Option<ViewData> {
        if time == self.openxr.waited_display_time() {
            return self.located_views(ty);
        }
        tracy_span!();
        let session = self.openxr.session_data.get();
        let mut views = self.views.lock().unwrap();
        views.get_at(&session, &self.openxr, ty, time)
    }

    /// Like [Self::located_views], with made up views if there are none - apps can't handle
    /// failure here.
    pub fn get_views(&self, ty: xr::ReferenceSpaceType) -> ViewData {
        self.located_views(ty).unwrap_or_else(ViewData::fallback)
    }

    /// The rotation removed from `eye`'s view to make the views parallel.
    fn view_canting(&self, eye: vr::EVREye) -> Quat {
        let session = self.openxr.session_data.get();
        let mut views = self.views.lock().unwrap();
        views.canting(&session, &self.openxr)[view_index(eye)]
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

    /// `origin`'s zero pose in the standing universe, identity if the runtime can't relate them.
    fn zero_pose_to_standing(&self, origin: vr::ETrackingUniverseOrigin) -> vr::HmdMatrix34_t {
        self.openxr
            .locate_origin(origin, vr::ETrackingUniverseOrigin::Standing)
            .unwrap_or(xr::Posef::IDENTITY)
            .into()
    }

    pub fn push_event(&self, event: vr::VREvent_t) {
        let mut events = self.events.lock().unwrap();
        events.push_back(QueuedEvent::new(event));
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

/// An event waiting for the app to poll it, with when it happened for `eventAgeSeconds`.
pub struct QueuedEvent {
    at: std::time::Instant,
    event: vr::VREvent_t,
}

impl QueuedEvent {
    pub fn new(event: vr::VREvent_t) -> Self {
        Self {
            at: std::time::Instant::now(),
            event,
        }
    }

    /// The event with its age filled in.
    pub fn take(self) -> vr::VREvent_t {
        vr::VREvent_t {
            eventAgeSeconds: self.at.elapsed().as_secs_f32(),
            ..self.event
        }
    }
}

/// The type of a tracked device property, from the suffix of its name in the headers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PropertyType {
    Bool,
    Float,
    Int32,
    Uint64,
    String,
    Matrix34,
    Vector3,
    Array,
    Binary,
}

/// None for values the headers don't name: vendor properties, other SDK versions, garbage.
fn property_type(prop: vr::ETrackedDeviceProperty) -> Option<PropertyType> {
    let name = format!("{prop:?}");
    let name = name.strip_suffix("_deprecated").unwrap_or(&name);
    let (_, suffix) = name.rsplit_once('_')?;
    Some(match suffix {
        "Bool" => PropertyType::Bool,
        "Float" => PropertyType::Float,
        "Int32" => PropertyType::Int32,
        "Uint64" => PropertyType::Uint64,
        "String" => PropertyType::String,
        "Matrix34" => PropertyType::Matrix34,
        "Vector3" => PropertyType::Vector3,
        "Array" => PropertyType::Array,
        "Binary" => PropertyType::Binary,
        _ => return None,
    })
}

/// The error for a property a getter of `expected` values has no value for: like SteamVR,
/// WrongDataType if the property is of another type, UnknownProperty if the device doesn't
/// provide it.
fn missing_property_error(
    prop: vr::ETrackedDeviceProperty,
    expected: PropertyType,
) -> vr::ETrackedPropertyError {
    match property_type(prop) {
        Some(ty) if ty != expected => vr::ETrackedPropertyError::WrongDataType,
        _ => vr::ETrackedPropertyError::UnknownProperty,
    }
}

impl vr::IVRSystem026_Interface for System {
    fn GetRecommendedRenderTargetSize(&self, width: *mut u32, height: *mut u32) {
        let (eye_width, eye_height) = self.openxr.recommended_eye_size();

        if !width.is_null() {
            unsafe { *width = eye_width };
        }

        if !height.is_null() {
            unsafe { *height = eye_height };
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
        let view = self.get_views(ty).views[view_index(eye)];

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
        let view = views[view_index(eye)];
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
    fn AcknowledgeQuit_Exiting(&self) {
        self.openxr.quit_watchdog.acknowledged();
    }
    fn PerformFirmwareUpdate(&self, _: vr::TrackedDeviceIndex_t) -> vr::EVRFirmwareError {
        crate::warn_unimplemented!("PerformFirmwareUpdate");
        vr::EVRFirmwareError::Fail
    }
    // The runtime holding input focus is our equivalent of the SteamVR dashboard being open.
    fn ShouldApplicationReduceRenderingWork(&self) -> bool {
        self.openxr.input_focus_captured()
    }
    fn ShouldApplicationPause(&self) -> bool {
        self.openxr.input_focus_captured()
    }
    fn IsSteamVRDrawingControllers(&self) -> bool {
        self.openxr.input_focus_captured()
    }
    fn IsInputAvailable(&self) -> bool {
        !self.openxr.input_focus_captured()
    }
    fn GetControllerAxisTypeNameFromEnum(
        &self,
        axis_type: vr::EVRControllerAxisType,
    ) -> *const std::os::raw::c_char {
        crate::enum_name("k_eControllerAxis_", axis_type)
    }
    fn GetButtonIdNameFromEnum(&self, button: vr::EVRButtonId) -> *const std::os::raw::c_char {
        crate::enum_name("k_EButton_", button)
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

        if !self.GetControllerState(device_index, state, state_size) {
            return false;
        }
        // The pose is optional.
        if let Some(pose) = unsafe { pose.as_mut() } {
            *pose = input
                .get_controller_pose(hand, Some(origin), self.openxr.get_display_time(), None)
                .unwrap_or_default();
        }
        true
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
            _ => {
                warn!(
                    "Unexpectedly got hidden area mesh type {ty:?} - returning default area mesh"
                );
                return Default::default();
            }
        };

        let mask = self
            .openxr
            .session_data
            .get()
            .session
            .get_visibility_mask_khr(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                view_index(eye) as u32,
                mask_ty,
            )
            .inspect_err(|e| debug!("Failed to get {ty:?} hidden area mesh for {eye:?}: {e}"));
        let Some(mask) = self.openxr.check("xrGetVisibilityMaskKHR", mask) else {
            return Default::default();
        };

        trace!("openxr mask: {:#?} {:#?}", mask.indices, mask.vertices);

        // Apps keep the mesh, so don't scale it with made up views.
        let space = self
            .openxr
            .session_data
            .get()
            .current_origin_as_reference_space();
        if self.located_views(space).is_none() {
            warn!("No views located yet, returning an empty {ty:?} hidden area mesh");
            return Default::default();
        }

        let [mut left, mut right, mut top, mut bottom] = [0.0; 4];
        self.GetProjectionRaw(eye, &mut left, &mut right, &mut top, &mut bottom);
        let canting = self.view_canting(eye);

        // convert from indices + vertices to just vertices
        let vertices: Vec<_> = mask
            .indices
            .into_iter()
            .map(|i| {
                // The mask is in the canted view, the projection in the parallel one.
                let v = mask.vertices[i as usize];
                let v = parallel_tangents(canting, Vec2::new(v.x, v.y));

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
        // For a line loop, the "triangle" count is the vertex count.
        let count = match ty {
            vr::EHiddenAreaMeshType::LineLoop => vertices.len(),
            _ => vertices.len() / 3,
        };
        if count == 0 {
            return Default::default();
        }

        // Apps never free the mesh, and may still use an old one after asking again. Leak each
        // distinct mesh once.
        let mut meshes = self.hidden_area_meshes.lock().unwrap();
        let bits = |mesh: &[vr::HmdVector2_t]| {
            mesh.iter()
                .map(|v| v.v.map(f32::to_bits))
                .collect::<Vec<_>>()
        };
        let key = (view_index(eye) as u32, ty.0);
        let mesh = match meshes.get(&key) {
            Some(&mesh) if bits(mesh) == bits(&vertices) => mesh,
            _ => {
                let mesh: &'static [_] = Vec::leak(vertices);
                meshes.insert(key, mesh);
                mesh
            }
        };

        vr::HiddenAreaMesh_t {
            pVertexData: mesh.as_ptr(),
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

    fn GetEventTypeNameFromEnum(&self, ty: vr::EVREventType) -> *const std::os::raw::c_char {
        crate::enum_name("VREvent_", ty)
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
        self.pump();
        let queued = self.events.lock().unwrap().pop_front();
        let got_event = match queued {
            Some(queued) => {
                // Truncated to the app's size, so older event structs only get what they have
                // room for.
                Self::copy_event(&queued.take(), event, size);
                true
            }
            None => match self.input.get() {
                Some(input) => input.get_next_event(size, event),
                None => false,
            },
        };
        if got_event && !pose.is_null() {
            // The event's device where it's tracked, like SteamVR; the identity pose otherwise.
            let index = unsafe { (&raw const (*event).trackedDeviceIndex).read() };
            let device_pose = self.input.get().and_then(|input| {
                input.get_device_pose(index, Some(origin), self.openxr.get_display_time(), None)
            });
            unsafe { pose.write(device_pose.unwrap_or_default()) };
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
        error: vr::ETrackedPropertyError,
    ) -> *const std::os::raw::c_char {
        crate::enum_name("TrackedProp_", error)
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
                // SteamVR's drivers all report this model, and default the rest to the tracking
                // system name.
                (
                    vr::k_unTrackedDeviceIndex_Hmd,
                    vr::ETrackedDeviceProperty::RenderModelName_String,
                ) => Some(c"generic_hmd".to_owned()),
                (
                    vr::k_unTrackedDeviceIndex_Hmd,
                    vr::ETrackedDeviceProperty::InputProfilePath_String
                    | vr::ETrackedDeviceProperty::ResourceRoot_String,
                ) => Some(self.hmd_identity().tracking_system.clone()),
                _ => None,
            });

        let Some(data) = data else {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = missing_property_error(prop, PropertyType::String);
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
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        tag: vr::PropertyTypeTag_t,
        buffer: *mut std::os::raw::c_void,
        size: u32,
        error: *mut vr::ETrackedPropertyError,
    ) -> u32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting array property: {prop:?} ({device_index})");
        let set_error = |e| {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = e;
            }
        };
        if !self.IsTrackedDeviceConnected(device_index) {
            set_error(vr::ETrackedPropertyError::InvalidDevice);
            return 0;
        }
        if property_type(prop) != Some(PropertyType::Array) {
            set_error(missing_property_error(prop, PropertyType::Array));
            return 0;
        }

        // The elements as bytes, and the tag of their type.
        let value: Option<(Vec<u8>, vr::PropertyTypeTag_t)> = match (device_index, prop) {
            (
                vr::k_unTrackedDeviceIndex_Hmd,
                vr::ETrackedDeviceProperty::DisplayAvailableFrameRates_Float_Array,
            ) => self.openxr.display_refresh_rates().map(|rates| {
                let bytes = rates.iter().flat_map(|rate| rate.to_ne_bytes()).collect();
                (bytes, vr::k_unFloatPropertyTag)
            }),
            _ => None,
        };
        let Some((bytes, expected_tag)) = value else {
            set_error(vr::ETrackedPropertyError::UnknownProperty);
            return 0;
        };
        if tag != expected_tag {
            set_error(vr::ETrackedPropertyError::WrongDataType);
            return 0;
        }
        let needed = bytes.len() as u32;
        if needed > 0 && (buffer.is_null() || size < needed) {
            set_error(vr::ETrackedPropertyError::BufferTooSmall);
            return needed;
        }
        if needed > 0 {
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast(), bytes.len()) };
        }
        set_error(vr::ETrackedPropertyError::Success);
        needed
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

        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            match prop {
                // The runtime's head pose is the headset's; it has no camera or status display of
                // its own to offset.
                vr::ETrackedDeviceProperty::CameraToHeadTransform_Matrix34
                | vr::ETrackedDeviceProperty::StatusDisplayTransform_Matrix34
                | vr::ETrackedDeviceProperty::ImuToHeadTransform_Matrix34 => {
                    if let Some(err) = unsafe { err.as_mut() } {
                        *err = vr::ETrackedPropertyError::Success;
                    }
                    return xr::Posef::IDENTITY.into();
                }
                _ => {}
            }
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = missing_property_error(prop, PropertyType::Matrix34);
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

        // The runtime's tracking space is the one universe there is; 0 would mean none.
        if device_index == vr::k_unTrackedDeviceIndex_Hmd
            && prop == vr::ETrackedDeviceProperty::CurrentUniverseId_Uint64
        {
            return 1;
        }

        self.input
            .get()
            .and_then(|input| input.get_device_uint_tracked_property(device_index, prop))
            .unwrap_or_else(|| {
                if let Some(err) = unsafe { err.as_mut() } {
                    *err = missing_property_error(prop, PropertyType::Uint64);
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
                    *err = missing_property_error(prop, PropertyType::Int32);
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
                // GetTimeSinceLastVsync counts from the last predicted display time, i.e. when
                // the photons of a frame reach the eyes, so apps predicting a frame duration
                // past the last vsync plus this land on the next display time.
                vr::ETrackedDeviceProperty::SecondsFromVsyncToPhotons_Float => return 0.0,
                // SteamVR's default dashboard scale.
                vr::ETrackedDeviceProperty::DashboardScale_Float => return 1.0,
                _ => {}
            }
        }

        if let Some(error) = unsafe { error.as_mut() } {
            *error = missing_property_error(prop, PropertyType::Float);
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
            vr::ETrackedDeviceProperty::ContainsProximitySensor_Bool
                if device_index == vr::k_unTrackedDeviceIndex_Hmd =>
            {
                return self.openxr.supports_user_presence;
            }
            vr::ETrackedDeviceProperty::DisplaySupportsRuntimeFramerateChange_Bool
                if device_index == vr::k_unTrackedDeviceIndex_Hmd =>
            {
                return self.openxr.enabled_extensions.fb_display_refresh_rate;
            }
            vr::ETrackedDeviceProperty::DisplaySupportsMultipleFramerates_Bool
                if device_index == vr::k_unTrackedDeviceIndex_Hmd =>
            {
                return self
                    .openxr
                    .display_refresh_rates()
                    .is_some_and(|rates| rates.len() > 1);
            }
            // The driver components SteamVR would ask for: the headset is a display, controllers
            // and trackers have inputs, and nothing has a camera, direct mode display, virtual
            // display or spatial anchors.
            vr::ETrackedDeviceProperty::HasDisplayComponent_Bool => {
                return device_index == vr::k_unTrackedDeviceIndex_Hmd;
            }
            vr::ETrackedDeviceProperty::HasControllerComponent_Bool => {
                return device_index != vr::k_unTrackedDeviceIndex_Hmd;
            }
            vr::ETrackedDeviceProperty::HasCameraComponent_Bool
            | vr::ETrackedDeviceProperty::HasDriverDirectModeComponent_Bool
            | vr::ETrackedDeviceProperty::HasVirtualDisplayComponent_Bool
            | vr::ETrackedDeviceProperty::HasSpatialAnchorsSupport_Bool => return false,
            _ => {}
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = missing_property_error(prop, PropertyType::Bool);
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
        // Trackers opt out of the hand roles, like their ControllerRoleHint says.
        if input.device_index_to_tracked_device_class(index)
            == Some(vr::ETrackedDeviceClass::GenericTracker)
        {
            return vr::ETrackedControllerRole::OptOut;
        }
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

        // Like SteamVR, a role is only held by a connected controller.
        Hand::try_from(role)
            .ok()
            .and_then(|hand| input.get_controller_device_index(hand))
            .filter(|&index| input.is_device_connected(index))
            .unwrap_or(vr::k_unTrackedDeviceIndexInvalid)
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

        // A fixed transform only rotates velocities.
        let velocity = rot * Vec3::from(input_pose.vVelocity.v);
        let angular_velocity = rot * Vec3::from(input_pose.vAngularVelocity.v);
        unsafe {
            (*output) = input_pose;
            (*output).mDeviceToAbsoluteTracking = result.into();
            (*output).vVelocity = velocity.into();
            (*output).vAngularVelocity = angular_velocity.into();
        }
    }
    fn GetTrackedDeviceActivityLevel(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
    ) -> vr::EDeviceActivityLevel {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd if !self.openxr.user_present() => {
                vr::EDeviceActivityLevel::Idle
            }
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
        class: vr::ETrackedDeviceClass,
        indices: *mut vr::TrackedDeviceIndex_t,
        capacity: u32,
        relative_to: vr::TrackedDeviceIndex_t,
    ) -> u32 {
        let mut found: Vec<_> = (0..vr::k_unMaxTrackedDeviceCount)
            .filter(|&i| self.GetTrackedDeviceClass(i) == class && self.IsTrackedDeviceConnected(i))
            .collect();

        // Sorted right to left as seen from the relative device, or in tracking space if there is
        // none. Devices without a pose go last.
        if let Some(input) = self.input.get().filter(|_| found.len() > 1) {
            let time = self.openxr.get_display_time();
            let pose = |index| {
                input
                    .get_device_pose(index, None, time, None)
                    .filter(|pose| pose.bPoseIsValid)
                    .map(|pose| xr::Posef::from(pose.mDeviceToAbsoluteTracking))
            };
            let reference = if relative_to == vr::k_unTrackedDeviceIndexInvalid {
                Some(xr::Posef::IDENTITY)
            } else {
                pose(relative_to)
            };
            if let Some(xr::Posef {
                position: p,
                orientation: o,
            }) = reference
            {
                let to_reference = Quat::from_xyzw(o.x, o.y, o.z, o.w).inverse();
                let x = |index| {
                    pose(index).map(|xr::Posef { position: d, .. }| {
                        (to_reference * Vec3::new(d.x - p.x, d.y - p.y, d.z - p.z)).x
                    })
                };
                let mut keyed: Vec<_> = found.iter().map(|&index| (index, x(index))).collect();
                keyed.sort_by(|(_, a), (_, b)| match (a, b) {
                    (Some(a), Some(b)) => b.total_cmp(a),
                    _ => b.is_some().cmp(&a.is_some()),
                });
                found = keyed.into_iter().map(|(index, _)| index).collect();
            }
        }

        if !indices.is_null() {
            let count = found.len().min(capacity as usize);
            unsafe { std::slice::from_raw_parts_mut(indices, count) }
                .copy_from_slice(&found[..count]);
        }
        found.len() as u32
    }
    fn GetRawZeroPoseToStandingAbsoluteTrackingPose(&self) -> vr::HmdMatrix34_t {
        self.zero_pose_to_standing(vr::ETrackingUniverseOrigin::RawAndUncalibrated)
    }
    fn GetSeatedZeroPoseToStandingAbsoluteTrackingPose(&self) -> vr::HmdMatrix34_t {
        self.zero_pose_to_standing(vr::ETrackingUniverseOrigin::Seated)
    }
    fn GetDeviceToAbsoluteTrackingPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        seconds_to_photon_from_now: f32,
        pose_array: *mut vr::TrackedDevicePose_t,
        pose_count: u32,
    ) {
        self.pump();
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
        let Some(device) = (unsafe { device.as_mut() }) else {
            return;
        };
        // "If an error occurs the device will be set to 0."
        *device = 0;
        if texture_type != vr::ETextureType::Vulkan {
            // Proton doesn't seem to properly translate this function, but it doesn't appear to
            // actually matter.
            warn!("GetOutputDevice: unsupported texture type {texture_type:?}, returning 0");
            return;
        }
        // IVRSystem_016 has no instance to pass, and runtimes don't check it.
        if instance.is_null() {
            warn!("GetOutputDevice: no VkInstance given, returning 0");
            return;
        }
        // Without the extension, the openxr crate panics instead of returning an error.
        if !self.openxr.enabled_extensions.khr_vulkan_enable {
            return;
        }
        let physical_device = unsafe {
            self.openxr
                .instance
                .vulkan_graphics_device(self.openxr.system_id, instance as _)
        };
        if let Some(physical_device) = self
            .openxr
            .check("xrGetVulkanGraphicsDeviceKHR", physical_device)
        {
            *device = physical_device as _;
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
    // Halts the timeout, the app quits once the user answered.
    fn AcknowledgeQuit_UserPrompt(&self) {
        self.openxr.quit_watchdog.stop();
    }
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
        self.openxr.input_focus_captured()
    }
    fn ReleaseInputFocus(&self) {}
    fn CaptureInputFocus(&self) -> bool {
        true
    }
}

impl vr::IVRSystem016On017 for System {
    // Without an instance there's no physical device to return, so this always writes 0.
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

// 0.9.14 events are a distinct type with the same layout as 0.9.12's: 16 bytes of data, then
// eventAgeSeconds, which the 0.9.12 conversion writes by offset.
const _: () = assert!(
    std::mem::offset_of!(vr::vr_0_9_12::VREvent_t, data)
        == std::mem::offset_of!(vr::vr_0_9_14::VREvent_t, data)
);

impl vr::IVRSystem010On011 for System {
    fn PollNextEvent(&self, event: *mut vr::vr_0_9_14::VREvent_t) -> bool {
        vr::IVRSystem009On010::PollNextEvent(self, event.cast())
    }

    fn PollNextEventWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::vr_0_9_14::VREvent_t,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        vr::IVRSystem009On010::PollNextEventWithPose(self, origin, event.cast(), pose)
    }
}

impl vr::IVRSystem009On010 for System {
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
            unsafe { Self::write_event_0_9_12(&e, event) };
        }

        ret
    }
}

impl System {
    /// Writes `e` in the 0.9.12 layout, which 0.9.14 shares.
    ///
    /// # Safety
    /// `event` must point to a writable 0.9.12 VREvent_t.
    pub unsafe fn write_event_0_9_12(e: &vr::VREvent_t, event: *mut vr::vr_0_9_12::VREvent_t) {
        // The 0.9.12 VREvent_t (pack(4)) has 16 bytes of data followed by eventAgeSeconds.
        // Our binding of its data union is built from the current, larger member structs, so
        // write by offset rather than through its fields. The members' leading fields match.
        const DATA_SIZE: usize = std::mem::size_of::<vr::vr_0_9_12::VREvent_Reserved_t>();
        unsafe {
            (&raw mut (*event).eventType).write(vr::EVREventType(e.eventType));
            (&raw mut (*event).trackedDeviceIndex).write(e.trackedDeviceIndex);
            let data = event
                .cast::<u8>()
                .add(std::mem::offset_of!(vr::vr_0_9_12::VREvent_t, data));
            std::ptr::copy_nonoverlapping((&raw const e.data).cast::<u8>(), data, DATA_SIZE);
            data.add(DATA_SIZE)
                .cast::<f32>()
                .write_unaligned(e.eventAgeSeconds);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clientcore::Injector, openxr_data::OpenXrData};
    use std::ffi::CStr;
    use vr::IVRSystem022_Interface;

    /// AcknowledgeQuit_Exiting gives an app longer to quit before it's killed,
    /// AcknowledgeQuit_UserPrompt lets it take as long as the user does.
    #[test]
    fn acknowledging_quit() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr.clone(), &Injector::default());

        let start = std::time::Instant::now();
        xr.quit_watchdog.start();
        assert!(xr.quit_watchdog.deadline().unwrap() < start + std::time::Duration::from_secs(10));
        system.AcknowledgeQuit_Exiting();
        assert!(xr.quit_watchdog.deadline().unwrap() >= start + std::time::Duration::from_secs(30));
        vr::IVRSystem020On021::AcknowledgeQuit_UserPrompt(&system);
        assert_eq!(xr.quit_watchdog.deadline(), None);
    }

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

    fn projection_raw(system: &System, eye: vr::EVREye) -> [f32; 4] {
        let [mut left, mut right, mut top, mut bottom] = [0.0; 4];
        system.GetProjectionRaw(eye, &mut left, &mut right, &mut top, &mut bottom);
        [left, right, top, bottom]
    }

    #[test]
    fn invalid_fov_falls_back() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        let session = xr.session_data.get().session.as_raw();

        // fakexr reports a zero FOV until views are set.
        assert_eq!(
            projection_raw(&system, vr::EVREye::Left),
            [-1.0, 1.0, -1.0, 1.0]
        );
        let matrix = system.GetProjectionMatrix(vr::EVREye::Left, 0.1, 100.0);
        assert!(matrix.m.as_flattened().iter().all(|v| v.is_finite()));

        let pose = xr::Posef::IDENTITY;
        let fov = xr::Fovf {
            angle_left: -0.5,
            angle_right: 0.4,
            angle_up: 0.3,
            angle_down: -0.6,
        };
        let expected = [-0.5f32.tan(), 0.4f32.tan(), -0.6f32.tan(), 0.3f32.tan()];
        fakexr::set_views(session, [(pose, fov); 2]);
        system.reset_views();
        assert_eq!(projection_raw(&system, vr::EVREye::Right), expected);

        // The last valid FOV is kept.
        for fov in [
            xr::Fovf::default(),
            xr::Fovf {
                angle_left: f32::NAN,
                ..fov
            },
        ] {
            fakexr::set_views(session, [(pose, fov); 2]);
            system.reset_views();
            assert_eq!(projection_raw(&system, vr::EVREye::Right), expected);
        }
    }

    /// Monado returns success without writing the views when it can't locate the headset, and a
    /// broken orientation can't be made parallel. Neither replaces the last valid FOV.
    #[test]
    fn unlocated_views_keep_valid_fov() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        let session = xr.session_data.get().session.as_raw();
        let pose = xr::Posef::IDENTITY;
        let fov = xr::Fovf {
            angle_left: -0.5,
            angle_right: 0.4,
            angle_up: 0.3,
            angle_down: -0.6,
        };
        let expected = [-0.5f32.tan(), 0.4f32.tan(), -0.6f32.tan(), 0.3f32.tan()];
        fakexr::set_views(session, [(pose, fov); 2]);
        system.reset_views();
        assert_eq!(projection_raw(&system, vr::EVREye::Left), expected);

        fakexr::set_locate_views_error(Some(xr::sys::Result::SUCCESS));
        system.reset_views();
        let views = system.located_views(xr::ReferenceSpaceType::VIEW).unwrap();
        assert_eq!(views.flags, xr::ViewStateFlags::EMPTY);
        assert_eq!(projection_raw(&system, vr::EVREye::Left), expected);
        fakexr::set_locate_views_error(None);

        let broken = xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 0.0,
            },
            ..pose
        };
        fakexr::set_views(session, [(broken, fov); 2]);
        system.reset_views();
        assert_eq!(projection_raw(&system, vr::EVREye::Left), expected);
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
    fn enum_names() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());
        let name = |ptr| unsafe { CStr::from_ptr(ptr) };

        assert_eq!(
            name(system.GetButtonIdNameFromEnum(vr::EVRButtonId::DPad_Left)),
            c"k_EButton_DPad_Left"
        );
        assert_eq!(
            name(system.GetButtonIdNameFromEnum(vr::EVRButtonId::SteamVR_Trigger)),
            c"k_EButton_Axis1"
        );
        assert_eq!(
            name(system.GetControllerAxisTypeNameFromEnum(vr::EVRControllerAxisType::TrackPad)),
            c"k_eControllerAxis_TrackPad"
        );
        assert_eq!(
            name(system.GetEventTypeNameFromEnum(vr::EVREventType::ButtonPress)),
            c"VREvent_ButtonPress"
        );
        assert_eq!(
            name(system.GetPropErrorNameFromEnum(vr::ETrackedPropertyError::BufferTooSmall)),
            c"TrackedProp_BufferTooSmall"
        );

        // Values the headers don't name, e.g. from a 0..k_EButton_Max loop.
        assert_eq!(
            name(system.GetButtonIdNameFromEnum(vr::EVRButtonId(20))),
            c"Unknown"
        );
        assert_eq!(
            name(system.GetControllerAxisTypeNameFromEnum(vr::EVRControllerAxisType(99))),
            c"Unknown"
        );
        assert_eq!(
            name(system.GetEventTypeNameFromEnum(vr::EVREventType(u32::MAX))),
            c"Unknown"
        );
        assert_eq!(
            name(system.GetPropErrorNameFromEnum(vr::ETrackedPropertyError(1000))),
            c"Unknown"
        );
    }

    #[test]
    fn views_survive_locate_failure() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
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

        // Located views, with a FOV unlike the made up one.
        let fov = xr::Fovf {
            angle_left: -0.5,
            angle_right: 0.4,
            angle_up: 0.3,
            angle_down: -0.6,
        };
        let located = [-0.5f32.tan(), 0.4f32.tan(), -0.6f32.tan(), 0.3f32.tan()];
        let session = xr.session_data.get().session.as_raw();
        fakexr::set_views(session, [(xr::Posef::IDENTITY, fov); 2]);
        fakexr::set_locate_views_error(None);
        assert_eq!(projection(), located);

        // Failing again on the next frame: the last located views.
        fakexr::set_locate_views_error(Some(xr::sys::Result::ERROR_RUNTIME_FAILURE));
        system.reset_views();
        assert_eq!(projection(), located);
        assert!(system.located_views(xr::ReferenceSpaceType::VIEW).is_some());
    }

    #[test]
    fn parallel_fov_covers_canted_view() {
        let fov = xr::Fovf {
            angle_left: -std::f32::consts::FRAC_PI_4,
            angle_right: std::f32::consts::FRAC_PI_4,
            angle_up: std::f32::consts::FRAC_PI_4,
            angle_down: -std::f32::consts::FRAC_PI_4,
        };
        assert_eq!(parallel_fov(fov, Quat::IDENTITY), fov);

        // Turned 10 degrees to the left.
        let canting = Quat::from_rotation_y(10f32.to_radians());
        let wide = parallel_fov(fov, canting);
        let degrees = [
            wide.angle_left,
            wide.angle_right,
            wide.angle_up,
            wide.angle_down,
        ]
        .map(f32::to_degrees);
        for (angle, expected) in degrees.into_iter().zip([-55.0, 35.0, 50.9524, -50.9524]) {
            assert!((angle - expected).abs() < 1e-3, "{degrees:?}");
        }

        // The canted view's edges end up on the parallel view's edges.
        let left_edge = parallel_tangents(canting, Vec2::new(-1.0, 0.0));
        assert!((left_edge.x - wide.angle_left.tan()).abs() < 1e-5);
        let top_left = parallel_tangents(canting, Vec2::new(-1.0, 1.0));
        assert!((top_left.y - wide.angle_up.tan()).abs() < 1e-5);
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
    #[cfg_attr(miri, ignore)] // The meshes are leaked on purpose.
    fn hidden_area_mesh_types() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());
        let mesh =
            |ty| vr::IVRSystem026_Interface::GetHiddenAreaMesh(&system, vr::EVREye::Left, ty);
        let square = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]
            .map(|(x, y)| xr::Vector2f { x, y })
            .to_vec();

        fakexr::set_visibility_mask(Some((square.clone(), vec![0, 1, 2, 3])));
        let line_loop = mesh(vr::EHiddenAreaMeshType::LineLoop);
        assert!(!line_loop.pVertexData.is_null());
        // One per vertex for a line loop.
        assert_eq!(line_loop.unTriangleCount, 4);

        fakexr::set_visibility_mask(Some((square, vec![0, 1, 2, 0, 2, 3])));
        let triangles = mesh(vr::EHiddenAreaMeshType::Standard);
        assert_eq!(triangles.unTriangleCount, 2);
        // The same mesh again isn't another allocation.
        assert_eq!(
            mesh(vr::EHiddenAreaMeshType::Standard).pVertexData,
            triangles.pVertexData
        );

        // Anything that isn't the left eye shares the right eye's mesh.
        let eye_mesh = |eye| {
            vr::IVRSystem026_Interface::GetHiddenAreaMesh(
                &system,
                eye,
                vr::EHiddenAreaMeshType::Standard,
            )
        };
        let right = eye_mesh(vr::EVREye::Right);
        assert_eq!(right.unTriangleCount, 2);
        assert_eq!(eye_mesh(vr::EVREye(2)).pVertexData, right.pVertexData);

        // Runtimes may not have a mask yet.
        fakexr::set_visibility_mask(Some((Vec::new(), Vec::new())));
        let empty = mesh(vr::EHiddenAreaMeshType::Standard);
        assert!(empty.pVertexData.is_null());
        assert_eq!(empty.unTriangleCount, 0);
    }

    /// Mask vertices are moved from the canted views to the parallel ones, and scaled to their
    /// projection.
    #[test]
    #[cfg_attr(miri, ignore)] // The meshes are leaked on purpose.
    fn hidden_area_mesh_vertices() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        let session = xr.session_data.get().session.as_raw();
        let fov = xr::Fovf {
            angle_left: -std::f32::consts::FRAC_PI_4,
            angle_right: std::f32::consts::FRAC_PI_4,
            angle_up: std::f32::consts::FRAC_PI_4,
            angle_down: -std::f32::consts::FRAC_PI_4,
        };
        let view = |yaw: f32, x| {
            let q = Quat::from_rotation_y(yaw.to_radians());
            let pose = xr::Posef {
                orientation: xr::Quaternionf {
                    x: q.x,
                    y: q.y,
                    z: q.z,
                    w: q.w,
                },
                position: xr::Vector3f { x, y: 0.0, z: 0.0 },
            };
            (pose, fov)
        };
        // The left and right edges of the views' FOV, halfway up.
        let edges = [(-1.0, 0.0), (1.0, 0.0), (0.0, 1.0)]
            .map(|(x, y)| xr::Vector2f { x, y })
            .to_vec();
        fakexr::set_visibility_mask(Some((edges, vec![0, 1, 2])));

        // Parallel views, then views turned outwards.
        for yaw in [0.0, 10.0] {
            fakexr::set_views(session, [view(yaw, -0.03), view(-yaw, 0.03)]);
            system.reset_views();
            for eye in [vr::EVREye::Left, vr::EVREye::Right] {
                let mesh = vr::IVRSystem026_Interface::GetHiddenAreaMesh(
                    &system,
                    eye,
                    vr::EHiddenAreaMeshType::Standard,
                );
                assert_eq!(mesh.unTriangleCount, 1);
                let vertices = unsafe { std::slice::from_raw_parts(mesh.pVertexData, 3) };
                let x = [vertices[0].v[0], vertices[1].v[0]];
                assert!(
                    x[0].abs() < 1e-5 && (x[1] - 1.0).abs() < 1e-5,
                    "{yaw} degrees, {eye:?}: {x:?}"
                );
            }
        }
    }

    #[test]
    fn hidden_area_mesh_without_views() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());
        fakexr::set_locate_views_error(Some(xr::sys::Result::ERROR_RUNTIME_FAILURE));
        fakexr::set_visibility_mask(Some((vec![xr::Vector2f::default(); 3], vec![0, 1, 2])));

        let mesh = vr::IVRSystem026_Interface::GetHiddenAreaMesh(
            &system,
            vr::EVREye::Left,
            vr::EHiddenAreaMeshType::Standard,
        );
        assert!(mesh.pVertexData.is_null());
        assert_eq!(mesh.unTriangleCount, 0);
    }

    /// A loss first seen when locating views or getting the visibility mask has to reach the app.
    #[test]
    fn session_lost_on_view_queries() {
        let queries: [fn(&System); 2] = [
            |system| {
                // Nothing is cached yet, so this locates views.
                system.GetEyeToHeadTransform(vr::EVREye::Left);
            },
            |system| {
                vr::IVRSystem026_Interface::GetHiddenAreaMesh(
                    system,
                    vr::EVREye::Left,
                    vr::EHiddenAreaMeshType::Standard,
                );
            },
        ];
        for query in queries {
            let xr = Arc::new(
                OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap(),
            );
            let system = Arc::new(System::new(xr.clone(), &Injector::default()));
            xr.system.set(Arc::downgrade(&system));
            fakexr::lose_session(xr.session_data.get().session.as_raw());

            query(&system);
            assert!(xr.is_lost());
            let mut event = vr::VREvent_t::default();
            assert!(system.PollNextEventWithPose(
                vr::ETrackingUniverseOrigin::Standing,
                &mut event,
                std::mem::size_of_val(&event) as u32,
                std::ptr::null_mut(),
            ));
            assert_eq!(event.eventType, vr::EVREventType::Quit.0);
        }
    }

    #[test]
    fn output_device_without_instance() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());

        let mut device = u64::MAX;
        system.GetOutputDevice(&mut device, vr::ETextureType::DirectX, std::ptr::null_mut());
        assert_eq!(device, 0);

        // 016 has no instance parameter.
        let mut device = u64::MAX;
        vr::IVRSystem016On017::GetOutputDevice(&system, &mut device, vr::ETextureType::Vulkan);
        assert_eq!(device, 0);
        system.GetOutputDevice(
            std::ptr::null_mut(),
            vr::ETextureType::Vulkan,
            std::ptr::null_mut(),
        );
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
    fn unknown_enum_values() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let injector = Injector::default();
        let input = Arc::new(Input::new(xr.clone()));
        let system = System::new(xr, &injector);
        system.input.set(Arc::downgrade(&input));

        const HMD: u32 = vr::k_unTrackedDeviceIndex_Hmd;
        // A hole in the enum (PreviousUniverseId_Uint64 in older SDKs), a vendor property, garbage
        for prop in [2005, 10500, u32::MAX].map(vr::ETrackedDeviceProperty) {
            assert_eq!(format!("{prop:?}"), prop.0.to_string());

            let mut err = vr::ETrackedPropertyError::Success;
            let check = |err: &mut vr::ETrackedPropertyError| {
                assert_eq!(*err, vr::ETrackedPropertyError::UnknownProperty, "{prop:?}");
                *err = vr::ETrackedPropertyError::Success;
            };
            assert!(!system.GetBoolTrackedDeviceProperty(HMD, prop, &mut err));
            check(&mut err);
            assert_eq!(
                system.GetFloatTrackedDeviceProperty(HMD, prop, &mut err),
                0.0
            );
            check(&mut err);
            assert_eq!(system.GetInt32TrackedDeviceProperty(HMD, prop, &mut err), 0);
            check(&mut err);
            assert_eq!(
                system.GetUint64TrackedDeviceProperty(HMD, prop, &mut err),
                0
            );
            check(&mut err);
            system.GetMatrix34TrackedDeviceProperty(HMD, prop, &mut err);
            check(&mut err);
            let len =
                system.GetStringTrackedDeviceProperty(HMD, prop, std::ptr::null_mut(), 0, &mut err);
            assert_eq!(len, 0);
            check(&mut err);
        }
        assert_eq!(
            format!("{:?}", vr::ETrackedDeviceProperty::ModelNumber_String),
            "ModelNumber_String"
        );

        // Anything that isn't the left eye is the right eye.
        let right = system.GetEyeToHeadTransform(vr::EVREye::Right);
        let eye = vr::EVREye(2);
        assert_eq!(system.GetEyeToHeadTransform(eye).m, right.m);
        let projection = |eye| {
            let mut p = [0.0; 4];
            let [left, right, top, bottom] = p.each_mut();
            system.GetProjectionRaw(eye, left, right, top, bottom);
            p
        };
        assert_eq!(projection(eye), projection(vr::EVREye::Right));

        let mesh = vr::IVRSystem026_Interface::GetHiddenAreaMesh(
            &system,
            vr::EVREye::Left,
            vr::EHiddenAreaMeshType(77),
        );
        assert!(mesh.pVertexData.is_null());
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
        assert_eq!(event.eventType, vr::EVREventType::Quit.0);
    }

    #[test]
    fn session_stop_0_9_14() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = Arc::new(System::new(xr.clone(), &Injector::default()));
        xr.system.set(Arc::downgrade(&system));

        xr.session_data.get().session.request_exit().unwrap();
        xr.poll_events();
        let mut event: vr::vr_0_9_14::VREvent_t = unsafe { std::mem::zeroed() };
        assert!(vr::IVRSystem010On011::PollNextEvent(&*system, &mut event));
        assert_eq!(event.eventType, vr::EVREventType::Quit);
    }

    #[test]
    fn device_to_absolute_tracking_pose_null_array() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());
        system.GetDeviceToAbsoluteTrackingPose(
            vr::ETrackingUniverseOrigin::Standing,
            0.0,
            std::ptr::null_mut(),
            vr::k_unMaxTrackedDeviceCount,
        );
    }

    fn system_with_events() -> (Arc<RealOpenXrData>, Arc<System>) {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = Arc::new(System::new(xr.clone(), &Injector::default()));
        xr.system.set(Arc::downgrade(&system));
        // The first poll sets up input, which reports the headset. The tests look at what follows.
        while next_event(&system).is_some() {}
        (xr, system)
    }

    fn next_event(system: &System) -> Option<vr::VREvent_t> {
        let mut event = vr::VREvent_t::default();
        system
            .PollNextEvent(&mut event, std::mem::size_of_val(&event) as u32)
            .then_some(event)
    }

    /// The app is told when the headset's IPD changes, like SteamVR when the slider moves.
    #[test]
    fn ipd_change_event() {
        let (xr, system) = system_with_events();
        let session = session(&xr);
        let views = |ipd: f32| {
            [-0.5f32, 0.5].map(|side| {
                let pose = xr::Posef {
                    orientation: xr::Quaternionf::IDENTITY,
                    position: xr::Vector3f {
                        x: side * ipd,
                        y: 0.0,
                        z: 0.0,
                    },
                };
                (pose, FALLBACK_FOV)
            })
        };
        let ipd = |system: &System| {
            let mut err = vr::ETrackedPropertyError::Success;
            system.GetFloatTrackedDeviceProperty(
                vr::k_unTrackedDeviceIndex_Hmd,
                vr::ETrackedDeviceProperty::UserIpdMeters_Float,
                &mut err,
            )
        };

        // The first IPD is what the app starts with.
        fakexr::set_views(session, views(0.063));
        system.reset_views();
        assert!((ipd(&system) - 0.063).abs() < 1e-6);
        assert!(next_event(&system).is_none());

        // Noise in the located positions isn't a change.
        fakexr::set_views(session, views(0.0631));
        system.reset_views();
        assert!(next_event(&system).is_none());

        fakexr::set_views(session, views(0.065));
        system.reset_views();
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::IpdChanged.0);
        assert!((unsafe { event.data.ipd.ipdMeters } - 0.065).abs() < 1e-6);
        assert!(next_event(&system).is_none());
    }

    /// A refresh rate change is a change of the headset's display frequency property.
    #[test]
    fn refresh_rate_change_event() {
        let (xr, system) = system_with_events();
        fakexr::set_display_refresh_rate(session(&xr), 120.0);
        xr.poll_events();
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::PropertyChanged.0);
        assert_eq!(event.trackedDeviceIndex, vr::k_unTrackedDeviceIndex_Hmd);
        let prop = unsafe { event.data.property.prop };
        assert_eq!(prop, vr::ETrackedDeviceProperty::DisplayFrequency_Float);
        assert_eq!(xr.get_refresh_rate(), 120.0);

        // Runtimes report the rate again when the headset reconnects, which changes nothing.
        fakexr::set_display_refresh_rate(session(&xr), 120.0);
        xr.poll_events();
        assert!(next_event(&system).is_none());
    }

    fn session(xr: &RealOpenXrData) -> xr::sys::Session {
        xr.session_data.get().session.as_raw()
    }

    type TestInput = Input<crate::compositor::Compositor>;

    /// A System with its Input, like an app gets, before any controller showed up. The input's
    /// test manifest is loaded, so controllers connect once their profile is bound and synced.
    fn system_with_input() -> (Arc<RealOpenXrData>, Arc<System>, Arc<TestInput>) {
        use vr::IVRInput010_Interface;
        fakexr::advertise_extension(xr::sys::FB_DISPLAY_REFRESH_RATE_EXTENSION_NAME);
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = Arc::new(System::new(xr.clone(), &Injector::default()));
        let input = Arc::new(Input::new(xr.clone()));
        system.input.set(Arc::downgrade(&input));
        xr.input.set(Arc::downgrade(&input));
        xr.system.set(Arc::downgrade(&system));
        let manifest = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/input_data/actions.json\0"
        );
        assert_eq!(
            input.SetActionManifestPath(manifest.as_ptr() as _),
            vr::EVRInputError::None
        );
        (xr, system, input)
    }

    /// Syncs the test manifest's first action set and applies pending profile changes.
    fn sync_profiles(xr: &RealOpenXrData, input: &TestInput) {
        use vr::IVRInput010_Interface;
        let mut set = 0;
        assert_eq!(
            input.GetActionSetHandle(c"/actions/set1".as_ptr(), &mut set),
            vr::EVRInputError::None
        );
        let mut active = vr::VRActiveActionSet_t {
            ulActionSet: set,
            ..Default::default()
        };
        assert_eq!(
            input.UpdateActionState(
                &mut active,
                std::mem::size_of::<vr::VRActiveActionSet_t>() as u32,
                1
            ),
            vr::EVRInputError::None
        );
        xr.poll_events();
    }

    fn bind_profile(xr: &RealOpenXrData, input: &TestInput, hand: fakexr::UserPath, profile: &str) {
        let path = xr.instance.string_to_path(profile).unwrap();
        fakexr::set_interaction_profile(session(xr), hand, path);
        sync_profiles(xr, input);
    }

    #[test]
    fn property_errors_tell_wrong_type_from_unknown() {
        use vr::ETrackedDeviceProperty as Prop;
        use vr::ETrackedPropertyError as Err;
        let (_xr, system, _input) = system_with_input();
        const HMD: u32 = vr::k_unTrackedDeviceIndex_Hmd;
        let mut err = Err::Success;

        // A property of another type.
        assert_eq!(
            system.GetFloatTrackedDeviceProperty(HMD, Prop::ModelNumber_String, &mut err),
            0.0
        );
        assert_eq!(err, Err::WrongDataType);
        assert!(!system.GetBoolTrackedDeviceProperty(HMD, Prop::DisplayFrequency_Float, &mut err));
        assert_eq!(err, Err::WrongDataType);
        assert_eq!(
            system.GetInt32TrackedDeviceProperty(HMD, Prop::ContainsProximitySensor_Bool, &mut err),
            0
        );
        assert_eq!(err, Err::WrongDataType);
        assert_eq!(
            system.GetUint64TrackedDeviceProperty(HMD, Prop::DashboardScale_Float, &mut err),
            0
        );
        assert_eq!(err, Err::WrongDataType);
        system.GetMatrix34TrackedDeviceProperty(HMD, Prop::ModelNumber_String, &mut err);
        assert_eq!(err, Err::WrongDataType);
        assert_eq!(
            system.GetStringTrackedDeviceProperty(
                HMD,
                Prop::DisplayFrequency_Float,
                std::ptr::null_mut(),
                0,
                &mut err
            ),
            0
        );
        assert_eq!(err, Err::WrongDataType);
        assert_eq!(
            system.GetArrayTrackedDeviceProperty(
                HMD,
                Prop::ModelNumber_String,
                vr::k_unStringPropertyTag,
                std::ptr::null_mut(),
                0,
                &mut err
            ),
            0
        );
        assert_eq!(err, Err::WrongDataType);

        // A property of the right type the device doesn't provide.
        assert_eq!(
            system.GetFloatTrackedDeviceProperty(
                HMD,
                Prop::UserHeadToEyeDepthMeters_Float,
                &mut err
            ),
            0.0
        );
        assert_eq!(err, Err::UnknownProperty);
        assert_eq!(
            system.GetStringTrackedDeviceProperty(
                HMD,
                Prop::DriverVersion_String,
                std::ptr::null_mut(),
                0,
                &mut err
            ),
            0
        );
        assert_eq!(err, Err::UnknownProperty);
        assert_eq!(
            system.GetArrayTrackedDeviceProperty(
                HMD,
                Prop::CameraToHeadTransforms_Matrix34_Array,
                vr::k_unHmdMatrix34PropertyTag,
                std::ptr::null_mut(),
                0,
                &mut err
            ),
            0
        );
        assert_eq!(err, Err::UnknownProperty);
    }

    #[test]
    fn hmd_display_and_component_properties() {
        use vr::ETrackedDeviceProperty as Prop;
        use vr::ETrackedPropertyError as Err;
        let (_xr, system, _input) = system_with_input();
        const HMD: u32 = vr::k_unTrackedDeviceIndex_Hmd;

        let get_bool = |prop| {
            let mut err = Err::UnknownProperty;
            let value = system.GetBoolTrackedDeviceProperty(HMD, prop, &mut err);
            assert_eq!(err, Err::Success, "{prop:?}");
            value
        };
        assert!(get_bool(Prop::HasDisplayComponent_Bool));
        assert!(!get_bool(Prop::HasControllerComponent_Bool));
        assert!(!get_bool(Prop::HasCameraComponent_Bool));
        assert!(!get_bool(Prop::HasDriverDirectModeComponent_Bool));
        assert!(!get_bool(Prop::HasSpatialAnchorsSupport_Bool));
        // fakexr has XR_FB_display_refresh_rate with several rates.
        assert!(get_bool(Prop::DisplaySupportsRuntimeFramerateChange_Bool));
        assert!(get_bool(Prop::DisplaySupportsMultipleFramerates_Bool));

        let get_float = |prop| {
            let mut err = Err::UnknownProperty;
            let value = system.GetFloatTrackedDeviceProperty(HMD, prop, &mut err);
            assert_eq!(err, Err::Success, "{prop:?}");
            value
        };
        assert_eq!(get_float(Prop::SecondsFromVsyncToPhotons_Float), 0.0);
        assert_eq!(get_float(Prop::DashboardScale_Float), 1.0);

        let mut err = Err::UnknownProperty;
        assert_eq!(
            system.GetUint64TrackedDeviceProperty(HMD, Prop::CurrentUniverseId_Uint64, &mut err),
            1
        );
        assert_eq!(err, Err::Success);

        let identity = vr::HmdMatrix34_t::from(xr::Posef::IDENTITY).m;
        for prop in [
            Prop::CameraToHeadTransform_Matrix34,
            Prop::StatusDisplayTransform_Matrix34,
            Prop::ImuToHeadTransform_Matrix34,
        ] {
            let mut err = Err::UnknownProperty;
            let matrix = system.GetMatrix34TrackedDeviceProperty(HMD, prop, &mut err);
            assert_eq!(err, Err::Success, "{prop:?}");
            assert_eq!(matrix.m, identity, "{prop:?}");
        }

        let get_string = |prop| {
            let mut err = Err::UnknownProperty;
            let mut buf = vec![0; 64];
            let len = system.GetStringTrackedDeviceProperty(
                HMD,
                prop,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut err,
            );
            assert_eq!(err, Err::Success, "{prop:?}");
            let slice = unsafe { std::slice::from_raw_parts(buf.as_ptr() as _, len as usize) };
            CStr::from_bytes_with_nul(slice).unwrap().to_owned()
        };
        assert_eq!(
            get_string(Prop::RenderModelName_String).as_c_str(),
            c"generic_hmd"
        );
        // Without a controller profile to go by, SteamVR's defaults: the tracking system name.
        let tracking_system = get_string(Prop::TrackingSystemName_String);
        assert_eq!(get_string(Prop::InputProfilePath_String), tracking_system);
        assert_eq!(get_string(Prop::ResourceRoot_String), tracking_system);
    }

    #[test]
    fn display_available_frame_rates() {
        use vr::ETrackedDeviceProperty as Prop;
        use vr::ETrackedPropertyError as Err;
        let (_xr, system, _input) = system_with_input();
        const HMD: u32 = vr::k_unTrackedDeviceIndex_Hmd;
        let prop = Prop::DisplayAvailableFrameRates_Float_Array;
        let mut err = Err::Success;

        let needed = system.GetArrayTrackedDeviceProperty(
            HMD,
            prop,
            vr::k_unFloatPropertyTag,
            std::ptr::null_mut(),
            0,
            &mut err,
        );
        assert_eq!(err, Err::BufferTooSmall);
        let expected = fakexr::DISPLAY_REFRESH_RATES;
        assert_eq!(needed as usize, std::mem::size_of_val(&expected));

        let mut rates = [0.0f32; 4];
        assert_eq!(
            system.GetArrayTrackedDeviceProperty(
                HMD,
                prop,
                vr::k_unFloatPropertyTag,
                rates.as_mut_ptr().cast(),
                needed,
                &mut err
            ),
            needed
        );
        assert_eq!(err, Err::Success);
        assert_eq!(rates, expected);

        assert_eq!(
            system.GetArrayTrackedDeviceProperty(
                HMD,
                prop,
                vr::k_unInt32PropertyTag,
                rates.as_mut_ptr().cast(),
                needed,
                &mut err
            ),
            0
        );
        assert_eq!(err, Err::WrongDataType);

        assert_eq!(
            system.GetArrayTrackedDeviceProperty(
                1,
                prop,
                vr::k_unFloatPropertyTag,
                std::ptr::null_mut(),
                0,
                &mut err
            ),
            0
        );
        assert_eq!(err, Err::InvalidDevice);
    }

    #[test]
    fn seated_zero_pose_in_standing() {
        let (xr, system) = system_with_events();
        // LOCAL at a fixed 1.6 m above the floor, like WiVRn.
        fakexr::set_reference_space_pose(
            session(&xr),
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
        let seated = system.GetSeatedZeroPoseToStandingAbsoluteTrackingPose();
        assert_eq!(translation(seated), [0.0, 1.6, 0.0]);
        let raw = system.GetRawZeroPoseToStandingAbsoluteTrackingPose();
        assert_eq!(translation(raw), [0.0, 0.0, 0.0]);

        fakexr::set_reference_space_pose(
            session(&xr),
            xr::ReferenceSpaceType::VIEW,
            xr::Posef {
                position: xr::Vector3f {
                    x: 0.5,
                    y: -0.4,
                    z: 0.0,
                },
                ..xr::Posef::IDENTITY
            },
        );
        xr.reset_tracking_space(vr::ETrackingUniverseOrigin::Standing);
        // The standing zero moved under the head, the raw one stays where it was.
        let raw = system.GetRawZeroPoseToStandingAbsoluteTrackingPose();
        assert_eq!(translation(raw), [-0.5, 0.0, 0.0]);
        let seated = system.GetSeatedZeroPoseToStandingAbsoluteTrackingPose();
        assert_eq!(translation(seated), [-0.5, 1.6, 0.0]);
    }

    /// The projection layer goes out in the current tracking space, so views cached before the
    /// app moved its zero pose are stale.
    #[test]
    fn zero_pose_reset_drops_cached_views() {
        let (xr, system) = system_with_events();
        system.get_views(xr::ReferenceSpaceType::STAGE);
        assert!(system.views.lock().unwrap().frame.stage.is_some());
        xr.reset_tracking_space(vr::ETrackingUniverseOrigin::Standing);
        let views = system.views.lock().unwrap();
        assert!(views.frame.stage.is_none() && views.last.stage.is_none());
    }

    /// A runtime recenter is the user resetting their seated position: the head becomes the
    /// seated zero, replacing what the app set.
    #[test]
    fn runtime_recenter_resets_seated_zero_pose() {
        let (xr, system) = system_with_events();
        let head = |x, y| xr::Posef {
            position: xr::Vector3f { x, y, z: 0.0 },
            ..xr::Posef::IDENTITY
        };
        let head_in_seated = || {
            let data = xr.session_data.get();
            data.view_space
                .locate(
                    data.get_space_for_origin(vr::ETrackingUniverseOrigin::Seated),
                    xr.get_display_time(),
                )
                .unwrap()
                .pose
                .position
        };

        fakexr::set_reference_space_pose(
            session(&xr),
            xr::ReferenceSpaceType::VIEW,
            head(1.0, 0.0),
        );
        xr.reset_tracking_space(vr::ETrackingUniverseOrigin::Seated);
        assert_eq!(head_in_seated().x, 0.0);

        // The runtime moved LOCAL; the head is now 0.4 m below it.
        fakexr::set_reference_space_pose(
            session(&xr),
            xr::ReferenceSpaceType::VIEW,
            head(0.0, -0.4),
        );
        fakexr::reference_space_change_pending(
            session(&xr),
            xr::ReferenceSpaceType::LOCAL,
            xr::Time::from_nanos(0),
        );
        xr.poll_events();
        let position = head_in_seated();
        assert_eq!([position.x, position.y, position.z], [0.0, 0.0, 0.0]);

        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::SeatedZeroPoseReset.0);
        assert!(unsafe { event.data.seatedZeroPoseReset.bResetBySystemMenu });
        assert!(next_event(&system).is_none());

        // A moved stage drops the app's standing zero pose.
        fakexr::set_reference_space_pose(
            session(&xr),
            xr::ReferenceSpaceType::VIEW,
            head(1.0, 0.0),
        );
        xr.reset_tracking_space(vr::ETrackingUniverseOrigin::Standing);
        assert_ne!(
            translation(system.GetRawZeroPoseToStandingAbsoluteTrackingPose()),
            [0.0, 0.0, 0.0]
        );
        fakexr::reference_space_change_pending(
            session(&xr),
            xr::ReferenceSpaceType::STAGE,
            xr::Time::from_nanos(0),
        );
        xr.poll_events();
        assert_eq!(
            translation(system.GetRawZeroPoseToStandingAbsoluteTrackingPose()),
            [0.0, 0.0, 0.0]
        );
        let event = next_event(&system).expect("no event");
        assert_eq!(
            event.eventType,
            vr::EVREventType::ChaperoneUniverseHasChanged.0
        );
    }

    /// The runtime says when the recentered LOCAL takes effect, the head is measured after that.
    #[test]
    fn runtime_recenter_waits_for_change_time() {
        let (xr, system) = system_with_events();
        fakexr::set_reference_space_pose(
            session(&xr),
            xr::ReferenceSpaceType::VIEW,
            xr::Posef {
                position: xr::Vector3f {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                },
                ..xr::Posef::IDENTITY
            },
        );
        let change_time = xr::Time::from_nanos(xr.waited_display_time().as_nanos() + 1_000_000);
        fakexr::reference_space_change_pending(
            session(&xr),
            xr::ReferenceSpaceType::LOCAL,
            change_time,
        );
        let seated = || translation(system.GetSeatedZeroPoseToStandingAbsoluteTrackingPose());

        xr.poll_events();
        assert_eq!(seated(), [0.0, 0.0, 0.0]);
        assert!(next_event(&system).is_none());

        xr.set_display_time(change_time);
        xr.poll_events();
        assert_eq!(seated(), [1.0, 0.0, 0.0]);
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::SeatedZeroPoseReset.0);
    }

    #[test]
    fn runtime_focus_loss_pauses_app() {
        use vr::IVROverlay028_Interface;
        let (xr, system) = system_with_events();
        let overlays = OverlayMan::new(xr.clone(), &Injector::default());
        let set_state = |state| {
            fakexr::set_session_state(session(&xr), state);
            xr.poll_events();
        };

        // Gaining focus at startup doesn't release anything.
        set_state(xr::SessionState::SYNCHRONIZED);
        set_state(xr::SessionState::VISIBLE);
        set_state(xr::SessionState::FOCUSED);
        assert!(next_event(&system).is_none());
        assert!(system.IsInputAvailable());
        assert!(!system.ShouldApplicationPause());
        assert!(!overlays.IsDashboardVisible());

        set_state(xr::SessionState::VISIBLE);
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::InputFocusCaptured.0);
        assert_eq!(unsafe { event.data.process.oldPid }, 0);
        // The runtime draws the controllers in its menu, like SteamVR in its dashboard.
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::HideRenderModels.0);
        assert!(!system.IsInputAvailable());
        assert!(system.ShouldApplicationPause());
        assert!(overlays.IsDashboardVisible());
        assert!(system.ShouldApplicationReduceRenderingWork());
        assert!(vr::IVRSystem017On019::IsInputFocusCapturedByAnotherProcess(
            &*system
        ));

        set_state(xr::SessionState::FOCUSED);
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::InputFocusReleased.0);
        assert_eq!(unsafe { event.data.process.pid }, 0);
        let event = next_event(&system).expect("no event");
        assert_eq!(event.eventType, vr::EVREventType::ShowRenderModels.0);
        assert!(system.IsInputAvailable());
        assert!(!system.ShouldApplicationPause());
        assert!(!overlays.IsDashboardVisible());
        assert!(next_event(&system).is_none());
    }

    #[test]
    fn user_presence() {
        let (xr, system) = system_with_events();
        let mut err = vr::ETrackedPropertyError::UnknownProperty;
        assert!(system.GetBoolTrackedDeviceProperty(
            vr::k_unTrackedDeviceIndex_Hmd,
            vr::ETrackedDeviceProperty::ContainsProximitySensor_Bool,
            &mut err
        ));
        assert_eq!(err, vr::ETrackedPropertyError::Success);
        let activity = || system.GetTrackedDeviceActivityLevel(vr::k_unTrackedDeviceIndex_Hmd);
        assert_eq!(activity(), vr::EDeviceActivityLevel::UserInteraction);

        // The runtime reports the initial state when the session begins.
        fakexr::set_user_presence(session(&xr), true);
        xr.poll_events();
        assert!(next_event(&system).is_none());

        fakexr::set_user_presence(session(&xr), false);
        xr.poll_events();
        let event = next_event(&system).expect("no event");
        assert_eq!(
            event.eventType,
            vr::EVREventType::TrackedDeviceUserInteractionEnded.0
        );
        assert_eq!(event.trackedDeviceIndex, vr::k_unTrackedDeviceIndex_Hmd);
        assert_eq!(activity(), vr::EDeviceActivityLevel::Idle);

        fakexr::set_user_presence(session(&xr), true);
        xr.poll_events();
        let event = next_event(&system).expect("no event");
        assert_eq!(
            event.eventType,
            vr::EVREventType::TrackedDeviceUserInteractionStarted.0
        );
        assert_eq!(activity(), vr::EDeviceActivityLevel::UserInteraction);
    }

    #[test]
    fn queued_event_fills_device_index_and_pose() {
        let (xr, system) = system_with_events();
        xr.session_data.get().session.request_exit().unwrap();
        xr.poll_events();

        let mut event = vr::VREvent_t {
            trackedDeviceIndex: 1234,
            ..Default::default()
        };
        let mut pose = vr::TrackedDevicePose_t {
            bPoseIsValid: true,
            ..Default::default()
        };
        assert!(system.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Standing,
            &mut event,
            std::mem::size_of_val(&event) as u32,
            &mut pose
        ));
        assert_eq!(event.eventType, vr::EVREventType::Quit.0);
        assert_eq!(event.trackedDeviceIndex, vr::k_unTrackedDeviceIndexInvalid);
        assert!(!pose.bPoseIsValid);
    }

    /// IVRSystem_009's event struct is 28 bytes, with eventAgeSeconds right after 16 bytes of
    /// data.
    #[test]
    fn poll_event_0_9_12() {
        #[repr(C)]
        struct Event0912 {
            event_type: u32,
            device: u32,
            data: [u32; 4],
            age: f32,
            canary: u32,
        }

        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let input = Arc::new(Input::new(xr.clone()));
        let system = System::new(xr, &Injector::default());
        system.input.set(Arc::downgrade(&input));

        let mut event = Event0912 {
            event_type: 0,
            device: 1234,
            data: [u32::MAX; 4],
            age: -1.0,
            canary: 0xdeadbeef,
        };
        assert!(vr::IVRSystem009On010::PollNextEvent(
            &system,
            (&raw mut event).cast()
        ));
        assert_eq!(event.event_type, vr::EVREventType::TrackedDeviceActivated.0);
        assert_eq!(event.device, vr::k_unTrackedDeviceIndex_Hmd);
        assert_eq!(event.data, [0; 4]);
        assert!((0.0..1.0).contains(&event.age));
        assert_eq!(event.canary, 0xdeadbeef);
    }

    /// eventAgeSeconds is how long ago the event happened, like in SteamVR.
    #[test]
    fn queued_event_reports_its_age() {
        let (_xr, system) = system_with_events();
        system.push_event(vr::VREvent_t {
            eventType: vr::EVREventType::IpdChanged.0,
            ..Default::default()
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut event = vr::VREvent_t::default();
        assert!(system.PollNextEvent(&mut event, std::mem::size_of_val(&event) as u32));
        assert_eq!(event.eventType, vr::EVREventType::IpdChanged.0);
        assert!(event.eventAgeSeconds >= 0.02, "{}", event.eventAgeSeconds);
        assert!(event.eventAgeSeconds < 1.0, "{}", event.eventAgeSeconds);
    }

    #[test]
    fn apply_transform_rotates_velocities() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let system = System::new(xr, &Injector::default());

        let rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let transform: vr::HmdMatrix34_t = xr::Posef {
            orientation: xr::Quaternionf {
                x: rotation.x,
                y: rotation.y,
                z: rotation.z,
                w: rotation.w,
            },
            position: xr::Vector3f {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
        }
        .into();
        let mut pose = vr::TrackedDevicePose_t {
            mDeviceToAbsoluteTracking: xr::Posef::IDENTITY.into(),
            vVelocity: vr::HmdVector3_t { v: [1.0, 0.0, 0.0] },
            vAngularVelocity: vr::HmdVector3_t { v: [0.0, 0.0, 1.0] },
            ..Default::default()
        };
        system.ApplyTransform(&mut pose, &pose, &transform);

        let close =
            |a: vr::HmdVector3_t, b: [f32; 3]| a.v.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-6);
        assert!(
            close(pose.vVelocity, [0.0, 0.0, -1.0]),
            "{:?}",
            pose.vVelocity
        );
        assert!(
            close(pose.vAngularVelocity, [1.0, 0.0, 0.0]),
            "{:?}",
            pose.vAngularVelocity
        );
    }

    /// Apps that never call WaitGetPoses (VRCX, VNyan) still need their events polled and actions
    /// synced, or controllers never show up and Quit never arrives.
    #[test]
    fn controllers_and_quit_without_frame_loop() {
        use fakexr::UserPath::*;
        use vr::IVRInput010_Interface;
        crate::init_logging();
        fakexr::set_headless_supported(true);
        for hand in [LeftHand, RightHand] {
            fakexr::set_default_interaction_profile(
                hand,
                Some("/interaction_profiles/oculus/touch_controller"),
            );
        }
        let xr = Arc::new(
            OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Background).unwrap(),
        );
        assert!(xr.headless);
        let system = Arc::new(System::new(xr.clone(), &Injector::default()));
        let input = Arc::new(Input::new(xr.clone()));
        system.input.set(Arc::downgrade(&input));
        xr.input.set(Arc::downgrade(&input));
        xr.system.set(Arc::downgrade(&system));

        let mut event = vr::VREvent_t::default();
        let size = std::mem::size_of_val(&event) as u32;
        let mut poll_until = |done: &mut dyn FnMut(&vr::VREvent_t) -> bool| {
            for _ in 0..100 {
                while system.PollNextEvent(&mut event, size) {
                    if done(&event) {
                        return true;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(12));
            }
            false
        };

        let mut activated = [false; 3];
        assert!(
            poll_until(&mut |event| {
                if event.eventType == vr::EVREventType::TrackedDeviceActivated.0 {
                    activated[event.trackedDeviceIndex as usize] = true;
                }
                activated[1] && activated[2]
            }),
            "controllers never activated"
        );
        assert!(system.IsTrackedDeviceConnected(1));
        assert!(system.IsTrackedDeviceConnected(2));

        // Replacing the legacy actions restarts the session, which has no compositor here.
        let manifest = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/input_data/actions.json\0"
        );
        assert_eq!(
            input.SetActionManifestPath(manifest.as_ptr().cast()),
            vr::EVRInputError::None
        );

        xr.session_data.get().session.request_exit().unwrap();
        assert!(
            poll_until(&mut |event| event.eventType == vr::EVREventType::Quit.0),
            "Quit never arrived"
        );
    }

    /// The pose is optional.
    #[test]
    fn controller_state_without_pose() {
        use fakexr::UserPath::*;
        crate::init_logging();
        fakexr::set_headless_supported(true);
        for hand in [LeftHand, RightHand] {
            fakexr::set_default_interaction_profile(
                hand,
                Some("/interaction_profiles/oculus/touch_controller"),
            );
        }
        let xr = Arc::new(
            OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Background).unwrap(),
        );
        let system = System::new(xr.clone(), &Injector::default());
        let input = Arc::new(Input::new(xr.clone()));
        system.input.set(Arc::downgrade(&input));
        xr.input.set(Arc::downgrade(&input));
        input.frame_start_update();
        xr.poll_events();
        input.frame_start_update();

        let mut state = vr::VRControllerState_t::default();
        let size = std::mem::size_of_val(&state) as u32;
        let mut pose = vr::TrackedDevicePose_t::default();
        for pose in [std::ptr::null_mut(), &mut pose] {
            assert!(system.GetControllerStateWithPose(
                vr::ETrackingUniverseOrigin::Seated,
                1,
                &mut state,
                size,
                pose,
            ));
        }
    }

    #[test]
    fn sorted_tracked_device_indices_of_class() {
        use fakexr::UserPath::*;
        use vr::ETrackedDeviceClass as Class;
        crate::init_logging();
        fakexr::set_headless_supported(true);
        for hand in [LeftHand, RightHand] {
            fakexr::set_default_interaction_profile(
                hand,
                Some("/interaction_profiles/oculus/touch_controller"),
            );
        }
        let xr = Arc::new(
            OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Background).unwrap(),
        );
        let system = System::new(xr.clone(), &Injector::default());
        let input = Arc::new(Input::new(xr.clone()));
        system.input.set(Arc::downgrade(&input));
        xr.input.set(Arc::downgrade(&input));
        // fakexr only locates in LOCAL space.
        xr.set_tracking_space(vr::ETrackingUniverseOrigin::Seated);

        let hmd = vr::k_unTrackedDeviceIndex_Hmd;
        let absolute = vr::k_unTrackedDeviceIndexInvalid;
        let sorted = |class, relative_to| {
            let count = system.GetSortedTrackedDeviceIndicesOfClass(
                class,
                std::ptr::null_mut(),
                0,
                relative_to,
            );
            let mut indices = vec![u32::MAX; count as usize];
            assert_eq!(
                system.GetSortedTrackedDeviceIndicesOfClass(
                    class,
                    indices.as_mut_ptr(),
                    count,
                    relative_to
                ),
                count
            );
            indices
        };
        assert_eq!(sorted(Class::HMD, hmd), [hmd]);
        assert!(sorted(Class::Controller, hmd).is_empty());

        // Bind the controllers and put them to the left and right of the HMD.
        input.frame_start_update();
        xr.poll_events();
        let session = xr.session_data.get().session.as_raw();
        let grip = |x, orientation| xr::Posef {
            position: xr::Vector3f { x, y: 1.0, z: -0.5 },
            orientation,
        };
        fakexr::set_grip(session, LeftHand, grip(-0.5, xr::Quaternionf::IDENTITY));
        fakexr::set_grip(session, RightHand, grip(0.5, xr::Quaternionf::IDENTITY));
        input.frame_start_update();

        assert_eq!(sorted(Class::Controller, hmd), [2, 1]);
        assert_eq!(sorted(Class::Controller, absolute), [2, 1]);
        assert_eq!(sorted(Class::HMD, absolute), [hmd]);
        assert!(sorted(Class::GenericTracker, hmd).is_empty());

        // Turned around, the left controller has the right one on its left.
        let turned = xr::Quaternionf {
            x: 0.0,
            y: 1.0,
            z: 0.0,
            w: 0.0,
        };
        fakexr::set_grip(session, LeftHand, grip(-0.5, turned));
        input.frame_start_update();
        assert_eq!(sorted(Class::Controller, 1), [1, 2]);

        // Too small an array gets the first devices and the full count.
        let mut first = u32::MAX;
        assert_eq!(
            system.GetSortedTrackedDeviceIndicesOfClass(Class::Controller, &mut first, 1, hmd),
            2
        );
        assert_eq!(first, 2);
    }

    /// While the app runs a frame loop, events are left to WaitGetPoses.
    #[test]
    // Depends on wall clock time between frames, which is far longer under miri.
    #[cfg_attr(miri, ignore)]
    fn no_pumping_during_frame_loop() {
        use crate::compositor::Compositor;
        use vr::IVRCompositor028_Interface;
        crate::init_logging();
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let comp = Arc::new(Compositor::new(xr.clone(), &Injector::default()));
        xr.compositor.set(Arc::downgrade(&comp));
        let system = Arc::new(System::new(xr.clone(), &Injector::default()));
        xr.system.set(Arc::downgrade(&system));
        // Created up front to keep the first PollNextEvent short, as it has to land within the
        // frame loop timeout.
        let input = Arc::new(Input::new(xr.clone()));
        system.input.set(Arc::downgrade(&input));
        xr.input.set(Arc::downgrade(&input));

        let mut event = vr::VREvent_t::default();
        let size = std::mem::size_of_val(&event) as u32;
        let mut got_quit = || {
            let mut quit = false;
            while system.PollNextEvent(&mut event, size) {
                quit |= event.eventType == vr::EVREventType::Quit.0;
            }
            quit
        };

        let wait_get_poses = || {
            comp.WaitGetPoses(std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
        };
        wait_get_poses();
        xr.session_data.get().session.request_exit().unwrap();
        assert!(!got_quit());
        wait_get_poses();
        assert!(got_quit());
    }

    #[test]
    fn controller_roles_need_a_connected_controller() {
        use fakexr::UserPath::LeftHand;
        let (xr, system, input) = system_with_input();
        let role = vr::ETrackedControllerRole::LeftHand;
        assert_eq!(
            system.GetTrackedDeviceIndexForControllerRole(role),
            vr::k_unTrackedDeviceIndexInvalid
        );
        assert_eq!(
            system.GetTrackedDeviceClass(1),
            vr::ETrackedDeviceClass::Invalid
        );

        bind_profile(
            &xr,
            &input,
            LeftHand,
            "/interaction_profiles/oculus/touch_controller",
        );
        assert!(system.IsTrackedDeviceConnected(1));
        assert_eq!(system.GetTrackedDeviceIndexForControllerRole(role), 1);
        assert_eq!(system.GetControllerRoleForTrackedDeviceIndex(1), role);
        assert_eq!(
            system.GetTrackedDeviceClass(1),
            vr::ETrackedDeviceClass::Controller
        );

        // Turned off: SteamVR keeps the device, but no longer answers for its role.
        fakexr::set_interaction_profile(session(&xr), LeftHand, xr::Path::NULL);
        sync_profiles(&xr, &input);
        input.expire_connection_grace();
        assert!(!system.IsTrackedDeviceConnected(1));
        assert_eq!(
            system.GetTrackedDeviceIndexForControllerRole(role),
            vr::k_unTrackedDeviceIndexInvalid
        );
        assert_eq!(
            system.GetTrackedDeviceClass(1),
            vr::ETrackedDeviceClass::Controller
        );
    }

    #[test]
    #[cfg_attr(not(feature = "monado"), ignore)]
    fn trackers_opt_out_of_hand_roles() {
        let (xr, system, input) = system_with_input();
        // Trackers are listed when a profile changes.
        fakexr::add_trackers(session(&xr));
        bind_profile(
            &xr,
            &input,
            fakexr::UserPath::LeftHand,
            "/interaction_profiles/valve/index_controller",
        );
        // we need to wait two frames for the tracker to be connected.
        for _ in 0..2 {
            xr.poll_events();
            input.frame_start_update();
        }
        assert_eq!(
            system.GetTrackedDeviceClass(3),
            vr::ETrackedDeviceClass::GenericTracker
        );
        let role = vr::ETrackedControllerRole::OptOut;
        assert_eq!(system.GetControllerRoleForTrackedDeviceIndex(3), role);
        let mut err = vr::ETrackedPropertyError::UnknownProperty;
        assert_eq!(
            system.GetInt32TrackedDeviceProperty(
                3,
                vr::ETrackedDeviceProperty::ControllerRoleHint_Int32,
                &mut err
            ),
            role.0 as i32
        );
        assert_eq!(err, vr::ETrackedPropertyError::Success);
    }

    fn translation(m: vr::HmdMatrix34_t) -> [f32; 3] {
        [m.m[0][3], m.m[1][3], m.m[2][3]]
    }

    #[test]
    fn hmd_identity_available_before_first_frame() {
        fakexr::set_default_interaction_profile(
            fakexr::UserPath::RightHand,
            Some("/interaction_profiles/oculus/touch_controller"),
        );
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Scene).unwrap());
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
