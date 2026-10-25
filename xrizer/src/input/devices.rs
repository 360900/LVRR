use std::any::TypeId;
use std::collections::{HashMap, VecDeque};
use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use glam::{Mat4, Quat, Vec3};
use openvr as vr;
use openxr as xr;
use openxr::sys::Handle;

use crate::input::BoundPoseType;
use crate::input::profiles::knuckles::Knuckles;
#[cfg(feature = "monado")]
use crate::input::profiles::vive_tracker::ViveTracker;
#[cfg(feature = "monado")]
use openxr_mndx_xdev_space::{SessionXDevExtensionMNDX, XDev, XR_MNDX_XDEV_SPACE_EXTENSION_NAME};

use crate::input::profiles::{InputDevice, PoseTransformations, ProfileProperties};
use crate::openxr_data::{self, Hand, OpenXrData, SessionData};
use crate::tracy_span;
use log::trace;

use super::{
    Input, InteractionProfile,
    profiles::{
        self, MainAxisType, RunWithProfile, simple_controller::SimpleController,
        vrlink_hand::VRLinkHand,
    },
};

#[derive(Clone)]
pub struct HandSkeleton {
    pub joints: xr::HandJointLocations,
    pub data_source: xr::HandTrackingDataSourceEXT,
}

pub enum TrackedDeviceType {
    Hmd,
    Controller {
        hand: Hand,
        hand_tracker: Option<xr::HandTracker>,
        skeleton_cache: Mutex<HashMap<u64, Option<HandSkeleton>>>,
        pose_is_synthesised: AtomicBool,
    },
    #[cfg(feature = "monado")]
    GenericTracker {
        space: xr::Space,
        serial: CString,
    },
}

impl std::fmt::Debug for TrackedDeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackedDeviceType::Hmd => write!(f, "HMD"),
            TrackedDeviceType::Controller { hand, .. } => write!(f, "Controller ({hand:?})"),
            #[cfg(feature = "monado")]
            TrackedDeviceType::GenericTracker { serial, .. } => {
                write!(f, "Generic Tracker ({})", serial.to_string_lossy())
            }
        }
    }
}

pub struct ProfileData {
    pub properties: &'static ProfileProperties,
    get_hand_offset: fn(Hand) -> Mat4,
    get_pose_transformation: fn(BoundPoseType) -> Option<PoseTransformations>,
    /// For Knuckles, the skeleton thumb tries to accurately match where the physical
    /// thumb is, e.g. the curl depends on which part of the touchpad is being touched,
    /// or how the thumbstick is being pushed, but in GetSkeletalSummaryData with
    /// EVRSummaryType::FromDevice the curl is set to 1 if any of touchpad, A/B,
    /// or thumbstick is being touched, so just use the skeletal input actions to
    /// determine the curl value for the thumb.
    pub force_estimated_thumb: bool,
    pub skeletal_tracking_level: Option<vr::EVRSkeletalTrackingLevel>,
    /// See [`InteractionProfile::SENSES_FINGERS`].
    pub senses_fingers: bool,
    /// See [`InteractionProfile::LEGACY_TRIGGER_CLICK`].
    pub legacy_trigger_click: Option<(f32, f32)>,
    /// See [`InteractionProfile::LEGACY_GRIP_CLICK`].
    pub legacy_grip_click: Option<(f32, f32)>,
}

impl ProfileData {
    pub fn new<P: InteractionProfile>() -> Self {
        Self {
            properties: P::properties(),
            get_hand_offset: P::offset_grip_pose,
            get_pose_transformation: P::pose_transformation,
            force_estimated_thumb: TypeId::of::<P>() == TypeId::of::<Knuckles>(),
            skeletal_tracking_level: P::SKELETAL_TRACKING_LEVEL,
            senses_fingers: P::SENSES_FINGERS,
            legacy_trigger_click: P::LEGACY_TRIGGER_CLICK,
            legacy_grip_click: P::LEGACY_GRIP_CLICK,
        }
    }

    /// Looks up the profile data for an interaction profile path, e.g.
    /// "/interaction_profiles/oculus/touch_controller".
    pub fn for_profile_name(profile_name: &str) -> Option<Self> {
        struct Data<'a> {
            profile_name: &'a str,
            data: Option<ProfileData>,
        }
        impl RunWithProfile for Data<'_> {
            fn run<P: InteractionProfile>(&mut self) {
                if P::profile_path() == self.profile_name {
                    self.data = Some(ProfileData::new::<P>())
                }
            }

            #[inline]
            fn keep_running(&self) -> bool {
                self.data.is_none()
            }
        }

        let mut data = Data {
            profile_name,
            data: None,
        };
        profiles::run_for_all_profiles(&mut data);
        data.data
    }

    /// Looks up the profile data and hand for the first profile whose render model name for a
    /// hand satisfies `matches`.
    pub fn for_render_model(matches: impl FnMut(&CStr) -> bool) -> Option<(Self, Hand)> {
        struct Data<F> {
            matches: F,
            data: Option<(ProfileData, Hand)>,
        }
        impl<F: FnMut(&CStr) -> bool> RunWithProfile for Data<F> {
            fn run<P: InteractionProfile>(&mut self) {
                let names = &P::properties().render_model_name;
                self.data = [Hand::Left, Hand::Right]
                    .into_iter()
                    .find(|&hand| (self.matches)(names.get(hand)))
                    .map(|hand| (ProfileData::new::<P>(), hand));
            }

            #[inline]
            fn keep_running(&self) -> bool {
                self.data.is_none()
            }
        }

        let mut data = Data {
            matches,
            data: None,
        };
        profiles::run_for_all_profiles(&mut data);
        data.data
    }

    #[inline]
    pub fn hand_offset(&self, hand: Hand) -> Mat4 {
        (self.get_hand_offset)(hand)
    }

    #[inline]
    pub fn pose_transformation(&self, hand: Hand, pose_type: BoundPoseType) -> Option<Mat4> {
        (self.get_pose_transformation)(pose_type).map(|transforms| match hand {
            Hand::Left => transforms.left_hand,
            Hand::Right => transforms.right_hand,
        })
    }
}

/// How long a controller stays connected after it was last seen, bound to an interaction profile
/// or tracked as a hand. WiVRn unbinds the profiles whenever the headset reports none for a
/// moment, and SteamVR only reports a device gone when its driver does, so a blip mustn't make the
/// app drop and set up its controllers again.
pub(super) const CONNECTION_GRACE: Duration = Duration::from_millis(500);

pub struct TrackedDevice {
    device_type: TrackedDeviceType,
    pub profile_data: Option<ProfileData>,
    /// The bound interaction profile, NULL while there is none.
    pub profile_path: xr::Path,
    /// The last interaction profile that was bound, which the device's identity comes from.
    pub last_profile_path: xr::Path,
    /// For a controller, bound to an interaction profile. It can be there without one, see
    /// [TrackedDevice::is_connected].
    pub connected: bool,
    /// When the controller was last seen. It stays connected for [CONNECTION_GRACE] after.
    last_seen: Mutex<Option<Instant>>,
    /// Kept connected while the runtime holds the input focus, see
    /// [TrackedDevice::hold_connection].
    held: AtomicBool,
    /// Another controller took this one's place, so the app should see this one go.
    replaced: AtomicBool,
    /// The connection state last reported to the app. Atomic so polling events only needs a read
    /// lock on the device list.
    previous_connected: AtomicBool,
    /// See [TrackedDevice::is_tracked_hand].
    tracked_hand: AtomicBool,
    pose_cache: Mutex<PoseCache>,
}

/// Raw poses this device was located at, so all queries for the same time and origin in a frame
/// get the same pose.
#[derive(Default)]
struct PoseCache {
    /// Bounded, since apps that never call WaitGetPoses never clear it, and "now" based queries
    /// like GetDeviceToAbsoluteTrackingPose use a new time on every call. Least recently used
    /// first, so those are evicted before the display time poses the frame keeps reading.
    recent: VecDeque<(i64, vr::ETrackingUniverseOrigin, vr::TrackedDevicePose_t)>,
    /// The last valid pose per origin, reported while the device is untracked.
    last_valid: HashMap<vr::ETrackingUniverseOrigin, vr::HmdMatrix34_t>,
}

impl PoseCache {
    const MAX_RECENT: usize = 16;

    fn get(
        &mut self,
        nanos: i64,
        origin: vr::ETrackingUniverseOrigin,
    ) -> Option<vr::TrackedDevicePose_t> {
        let i = self
            .recent
            .iter()
            .position(|(t, o, _)| *t == nanos && *o == origin)?;
        let entry = self.recent.remove(i)?;
        self.recent.push_back(entry);
        Some(entry.2)
    }

    fn insert(
        &mut self,
        nanos: i64,
        origin: vr::ETrackingUniverseOrigin,
        pose: vr::TrackedDevicePose_t,
    ) {
        if pose.bPoseIsValid {
            self.last_valid
                .insert(origin, pose.mDeviceToAbsoluteTracking);
        }
        if self.recent.len() == Self::MAX_RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((nanos, origin, pose));
    }
}

fn get_hmd_pose(
    xr_time: xr::Time,
    session_data: &SessionData,
    origin: vr::ETrackingUniverseOrigin,
) -> Option<vr::TrackedDevicePose_t> {
    let (location, velocity) = {
        session_data
            .view_space
            .relate(session_data.get_space_for_origin(origin), xr_time)
            .ok()?
    };

    Some(vr::space_relation_to_openvr_pose(location, velocity))
}

fn get_controller_pose(
    xr_time: xr::Time,
    session_data: &SessionData,
    controller: &TrackedDevice,
    origin: vr::ETrackingUniverseOrigin,
) -> Option<vr::TrackedDevicePose_t> {
    let TrackedDeviceType::Controller {
        hand,
        hand_tracker,
        pose_is_synthesised,
        ..
    } = controller.get_type()
    else {
        return None;
    };
    let hand = *hand;
    // Try to synthesise a grip pose from palm joint
    let pose_from_hand_tracking = || {
        let pose = hand_tracker.as_ref().and_then(|tracker| {
            session_data
                .get_space_for_origin(origin)
                .relate_hand_joints(tracker, xr_time)
                .ok()
                .flatten()
        });
        if let Some((joints, velocities)) = pose {
            let bone = joints[xr::HandJointEXT::PALM];
            let velocity = velocities[xr::HandJointEXT::PALM];
            if !bone
                .location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
            {
                trace!("Palm bone is untracked, returning empty pose");
                pose_is_synthesised.store(false, Ordering::Relaxed);
                return (xr::SpaceLocation::default(), xr::SpaceVelocity::default());
            }

            let pose = {
                let position = bone.pose.position;
                let orientation = bone.pose.orientation;
                let mut pose = Mat4::from_rotation_translation(
                    Quat::from_xyzw(orientation.x, orientation.y, orientation.z, orientation.w),
                    Vec3::from_array([position.x, position.y, position.z]),
                );

                let mut offset = Mat4::from_translation(match hand {
                    Hand::Left => Vec3::from_array([0.09, -0.03, -0.09]),
                    Hand::Right => Vec3::from_array([-0.09, -0.03, -0.09]),
                });
                offset *= Mat4::from_euler(
                    glam::EulerRot::XYZ,
                    0.0,
                    if hand == Hand::Left {
                        -FRAC_PI_4
                    } else {
                        FRAC_PI_4
                    },
                    if hand == Hand::Left {
                        -FRAC_PI_2
                    } else {
                        FRAC_PI_2
                    },
                );

                pose *= offset;

                let (_, rot, pos) = pose.to_scale_rotation_translation();
                xr::Posef {
                    position: xr::Vector3f {
                        x: pos.x,
                        y: pos.y,
                        z: pos.z,
                    },
                    orientation: xr::Quaternionf {
                        x: rot.x,
                        y: rot.y,
                        z: rot.z,
                        w: rot.w,
                    },
                }
            };

            pose_is_synthesised.store(true, Ordering::Relaxed);
            (
                xr::SpaceLocation {
                    location_flags: bone.location_flags,
                    pose,
                },
                xr::SpaceVelocity {
                    velocity_flags: velocity.velocity_flags,
                    linear_velocity: velocity.linear_velocity,
                    angular_velocity: velocity.angular_velocity,
                },
            )
        } else {
            trace!("No hand tracking available, returning empty pose");
            (xr::SpaceLocation::default(), xr::SpaceVelocity::default())
        }
    };
    let pose_data = &session_data.input_data.pose_data;

    let spaces = match hand {
        Hand::Left => &pose_data.left_space,
        Hand::Right => &pose_data.right_space,
    };

    let (location, mut velocity) = if let Some(raw) =
        spaces.try_get_or_init_raw(&controller.profile_data, session_data, pose_data)
    {
        let pose = raw
            .relate(session_data.get_space_for_origin(origin), xr_time)
            .ok();

        pose.filter(|(loc, _vel)| {
            loc.location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
        })
        .inspect(|_| {
            pose_is_synthesised.store(false, Ordering::Relaxed);
        })
        .unwrap_or_else(pose_from_hand_tracking)
    } else {
        trace!("Failed to get raw space, trying hand tracking");

        pose_from_hand_tracking()
    };

    if crate::quirks::synthesized_velocity()
        && location
            .location_flags
            .contains(xr::SpaceLocationFlags::POSITION_VALID)
    {
        let pos = location.pose.position;
        if let Some(v) = spaces.update_history_and_velocity(
            xr_time.as_nanos(),
            Vec3::new(pos.x, pos.y, pos.z),
            origin,
        ) {
            let v = v.clamp_length_max(crate::quirks::MAX_SYNTHESIZED_SPEED);
            velocity.linear_velocity = xr::Vector3f {
                x: v.x,
                y: v.y,
                z: v.z,
            };
            velocity.velocity_flags |= xr::SpaceVelocityFlags::LINEAR_VALID;
        }
    }

    Some(vr::space_relation_to_openvr_pose(location, velocity))
}

// We must use the unsafe API if we want the data source, as openxrs
// does not provide a safe API similar to `xr::space::Space::locate_hand_joints` that returns the data source state.
fn get_joints_with_data_source(
    base: &xr::Space,
    tracker: &xr::HandTracker,
    time: xr::Time,
) -> xr::Result<Option<HandSkeleton>> {
    let Some(ext) = base.instance().exts().ext_hand_tracking.as_ref() else {
        return Err(xr::sys::Result::ERROR_EXTENSION_NOT_PRESENT);
    };

    let locate_info = xr::sys::HandJointsLocateInfoEXT {
        ty: xr::sys::HandJointsLocateInfoEXT::TYPE,
        next: std::ptr::null(),
        base_space: base.as_raw(),
        time,
    };
    let mut source_state = xr::sys::HandTrackingDataSourceStateEXT {
        ty: xr::sys::HandTrackingDataSourceStateEXT::TYPE,
        next: std::ptr::null_mut(),
        ..unsafe { std::mem::zeroed() }
    };
    let mut locations = MaybeUninit::<[xr::HandJointLocation; xr::HAND_JOINT_COUNT]>::uninit();
    let mut location_info = xr::sys::HandJointLocationsEXT {
        ty: xr::sys::HandJointLocationsEXT::TYPE,
        next: &mut source_state as *mut _ as _,
        joint_count: xr::HAND_JOINT_COUNT as u32,
        joint_locations: locations.as_mut_ptr() as _,
        ..unsafe { std::mem::zeroed() }
    };
    let r = unsafe { (ext.locate_hand_joints)(tracker.as_raw(), &locate_info, &mut location_info) };
    if r.into_raw() < 0 {
        return Err(r);
    }
    Ok(if location_info.is_active.into() {
        Some(HandSkeleton {
            joints: unsafe { locations.assume_init() },
            data_source: source_state.data_source,
        })
    } else {
        None
    })
}

#[cfg(feature = "monado")]
fn get_generic_tracker_pose(
    xr_time: xr::Time,
    session_data: &SessionData,
    tracker: &TrackedDevice,
    origin: vr::ETrackingUniverseOrigin,
) -> Option<vr::TrackedDevicePose_t> {
    let TrackedDeviceType::GenericTracker { space, .. } = tracker.get_type() else {
        return None;
    };

    let (location, velocity) = space
        .relate(session_data.get_space_for_origin(origin), xr_time)
        .ok()?;

    Some(vr::space_relation_to_openvr_pose(location, velocity))
}

/// simple_controller is what runtimes bind when nothing better matches, e.g. optically tracked
/// hands, so it says little about the hardware.
pub(super) fn is_simple_controller(properties: &ProfileProperties) -> bool {
    std::ptr::eq(properties, SimpleController::properties())
}

impl TrackedDevice {
    pub(super) fn new(
        device_type: TrackedDeviceType,
        profile_path: Option<xr::Path>,
        profile_data: Option<ProfileData>,
    ) -> Self {
        let profile_path = profile_path.unwrap_or(xr::Path::NULL);
        Self {
            profile_data,
            profile_path,
            last_profile_path: profile_path,
            connected: matches!(device_type, TrackedDeviceType::Hmd),
            device_type,
            last_seen: Mutex::default(),
            held: false.into(),
            replaced: false.into(),
            previous_connected: false.into(),
            tracked_hand: false.into(),
            pose_cache: Mutex::default(),
        }
    }

    pub fn get_pose(
        &self,
        xr_time: xr::Time,
        session_data: &SessionData,
        origin: vr::ETrackingUniverseOrigin,
        pose_type: Option<BoundPoseType>,
    ) -> vr::TrackedDevicePose_t {
        // Any other value from the app is located like RawAndUncalibrated, so it shares those
        // cache entries rather than adding its own.
        let origin = match origin {
            vr::ETrackingUniverseOrigin::Seated
            | vr::ETrackingUniverseOrigin::Standing
            | vr::ETrackingUniverseOrigin::RawAndUncalibrated => origin,
            _ => {
                crate::warn_once!("Unknown tracking universe {:?}, using raw", origin);
                vr::ETrackingUniverseOrigin::RawAndUncalibrated
            }
        };
        let nanos = xr_time.as_nanos();
        let mut pose_cache = self.pose_cache.lock().unwrap();
        let mut pose = if let Some(pose) = pose_cache.get(nanos, origin) {
            pose
        } else {
            let pose = match self.device_type {
                TrackedDeviceType::Hmd => get_hmd_pose(xr_time, session_data, origin),
                TrackedDeviceType::Controller { .. } => {
                    get_controller_pose(xr_time, session_data, self, origin)
                }
                #[cfg(feature = "monado")]
                TrackedDeviceType::GenericTracker { .. } => {
                    get_generic_tracker_pose(xr_time, session_data, self, origin)
                }
            }
            .unwrap_or_default();
            pose_cache.insert(nanos, origin, pose);
            pose
        };

        // Like SteamVR, a device that loses tracking stays connected, and is reported out of range
        // at its last known pose.
        let mut has_pose = pose.bPoseIsValid;
        pose.bDeviceIsConnected = match self.device_type {
            TrackedDeviceType::Controller { .. } if self.connected || pose.bPoseIsValid => {
                self.seen();
                true
            }
            TrackedDeviceType::Controller { .. } => self.recently_seen(),
            _ => self.connected || pose.bPoseIsValid,
        };
        if pose.bDeviceIsConnected && !pose.bPoseIsValid {
            pose.eTrackingResult = vr::ETrackingResult::Running_OutOfRange;
            if let Some(last) = pose_cache.last_valid.get(&origin) {
                pose.mDeviceToAbsoluteTracking = *last;
                has_pose = true;
            }
        }
        drop(pose_cache);

        // The cache holds the raw pose; the profile's pose transformation is
        // applied per call, since different bindings can ask for different
        // pose types (raw, tip, handgrip, ...) in the same frame.
        if let Some(p_type) = pose_type
            && has_pose
        {
            pose.mDeviceToAbsoluteTracking = vr::HmdMatrix34_t::from(
                self.transform_pose(xr::Posef::from(pose.mDeviceToAbsoluteTracking), p_type),
            );
        }
        pose
    }

    pub fn get_hand_skeleton(
        &self,
        xr_data: &OpenXrData<impl crate::openxr_data::Compositor>,
        base: &xr::Space,
    ) -> Option<HandSkeleton> {
        let TrackedDeviceType::Controller {
            hand_tracker,
            skeleton_cache,
            ..
        } = self.get_type()
        else {
            return None;
        };
        let mut skeleton_cache = skeleton_cache.lock().unwrap();
        if let Some(skeleton) = skeleton_cache.get(&base.as_raw().into_raw()) {
            return skeleton.clone();
        }

        let display_time = xr_data.get_display_time();
        let r = if xr_data.enabled_extensions.ext_hand_tracking_data_source {
            get_joints_with_data_source(base, hand_tracker.as_ref()?, display_time)
                .unwrap_or_default()
        } else {
            base.locate_hand_joints(hand_tracker.as_ref()?, display_time)
                .unwrap_or_default()
                .map(|joints| HandSkeleton {
                    joints,
                    data_source: xr::HandTrackingDataSourceEXT::UNOBSTRUCTED,
                })
        };

        skeleton_cache.insert(base.as_raw().into_raw(), r.clone());
        r
    }

    /// The data source of the hand's skeleton, if it's tracked. Unlike the joints, it doesn't
    /// depend on the base space, so this reuses any skeleton located this frame.
    pub fn get_hand_data_source(
        &self,
        xr_data: &OpenXrData<impl crate::openxr_data::Compositor>,
        base: &xr::Space,
    ) -> Option<xr::HandTrackingDataSourceEXT> {
        if let TrackedDeviceType::Controller { skeleton_cache, .. } = self.get_type() {
            let skeleton_cache = skeleton_cache.lock().unwrap();
            if !skeleton_cache.is_empty() {
                return skeleton_cache
                    .values()
                    .flatten()
                    .next()
                    .map(|skeleton| skeleton.data_source);
            }
        }
        self.get_hand_skeleton(xr_data, base)
            .map(|skeleton| skeleton.data_source)
    }

    /// Whether a skeleton from `source` only reflects the controller's inputs, like the one the
    /// Quest derives for its controllers. The estimated skeleton follows the same inputs with
    /// SteamVR's hand poses.
    pub fn skeleton_follows_inputs(&self, source: xr::HandTrackingDataSourceEXT) -> bool {
        source == xr::HandTrackingDataSourceEXT::CONTROLLER
            && self
                .profile_data
                .as_ref()
                .is_some_and(|data| !data.senses_fingers)
    }

    /// Forgets every pose, including the last valid ones that clear_pose_cache keeps.
    pub fn forget_poses(&self) {
        *self.pose_cache.lock().unwrap() = PoseCache::default();
    }

    pub fn clear_pose_cache(&self) {
        self.pose_cache.lock().unwrap().recent.clear();
        if let TrackedDeviceType::Controller { skeleton_cache, .. } = self.get_type() {
            skeleton_cache.lock().unwrap().clear();
        }
    }

    /// Whether the device is there. A controller is while it's bound to an interaction profile or
    /// tracked as a hand, and for [CONNECTION_GRACE] after, see [TrackedDevice::hold_connection].
    pub fn is_connected(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
    ) -> bool {
        match self.get_type() {
            TrackedDeviceType::Controller { .. } => {
                let seen = self.connected
                    || self
                        .get_hand_skeleton(xr_data, session_data.tracking_space())
                        .is_some();
                if seen {
                    self.seen();
                }
                seen || self.recently_seen()
            }
            _ => self.connected,
        }
    }

    pub(super) fn seen(&self) {
        *self.last_seen.lock().unwrap() = Some(Instant::now());
    }

    pub(super) fn recently_seen(&self) -> bool {
        self.held.load(Ordering::Relaxed)
            || self
                .last_seen
                .lock()
                .unwrap()
                .is_some_and(|seen| seen.elapsed() < CONNECTION_GRACE)
    }

    /// Keeps a controller that is still there connected while the runtime holds the input focus:
    /// SteamVR keeps devices connected while its dashboard has it, but runtimes may unbind their
    /// profiles meanwhile (WiVRn does). Once the app has the focus back, the grace starts over.
    pub(super) fn hold_connection(&self, focus_captured: bool) {
        if focus_captured {
            if !self.held.load(Ordering::Relaxed) && self.recently_seen() {
                self.held.store(true, Ordering::Relaxed);
            }
        } else if self.held.swap(false, Ordering::Relaxed) {
            self.seen();
        }
    }

    /// Marks the device as one the app should see go, and come back as a new one.
    pub(super) fn replace(&self) {
        self.replaced.store(true, Ordering::Relaxed);
    }

    /// Whether the device was replaced since the last call.
    pub(super) fn take_replaced(&self) -> bool {
        self.replaced.swap(false, Ordering::Relaxed)
    }

    /// Whether `connected` differs from the state last reported to the app, which it becomes.
    pub fn has_connected_changed(&self, connected: bool) -> bool {
        self.previous_connected.swap(connected, Ordering::Relaxed) != connected
    }

    #[cfg(test)]
    pub(super) fn expire_connection_grace(&self) {
        let mut last_seen = self.last_seen.lock().unwrap();
        if let Some(seen) = last_seen.as_mut() {
            *seen = Instant::now()
                .checked_sub(CONNECTION_GRACE)
                .unwrap_or(*seen);
        }
    }

    pub fn get_type(&self) -> &TrackedDeviceType {
        &self.device_type
    }

    pub fn get_controller_hand(&self) -> Option<Hand> {
        match &self.device_type {
            TrackedDeviceType::Controller { hand, .. } => Some(*hand),
            _ => None,
        }
    }

    pub fn device_class(&self) -> vr::ETrackedDeviceClass {
        match self.device_type {
            TrackedDeviceType::Hmd => vr::ETrackedDeviceClass::HMD,
            TrackedDeviceType::Controller { .. } => vr::ETrackedDeviceClass::Controller,
            #[cfg(feature = "monado")]
            TrackedDeviceType::GenericTracker { .. } => vr::ETrackedDeviceClass::GenericTracker,
        }
    }

    /// Returns the properties of the interaction profile this device's identity
    /// should be derived from.
    fn get_profile_properties(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
        devices: &TrackedDeviceList,
        last_profile: Option<&'static ProfileProperties>,
    ) -> Option<&'static ProfileProperties> {
        match self.device_type {
            TrackedDeviceType::Hmd => {
                let controllers = || {
                    [Hand::Left, Hand::Right]
                        .into_iter()
                        .filter_map(|hand| devices.get_controller(hand))
                };
                let profiles = || {
                    controllers()
                        .filter_map(|controller| controller.profile_data.as_ref())
                        .map(|data| data.properties)
                };
                // simple_controller says nothing about the headset, so prefer any real
                // controller seen before, then tracked hands.
                let real = |properties: &&ProfileProperties| !is_simple_controller(properties);
                profiles()
                    .find(real)
                    .or(last_profile.filter(real))
                    .or_else(|| {
                        controllers()
                            .any(|controller| controller.is_tracked_hand(xr_data, session_data))
                            .then(VRLinkHand::properties)
                    })
                    .or_else(|| profiles().next())
                    .or(last_profile)
            }
            _ if self.is_tracked_hand(xr_data, session_data) => Some(VRLinkHand::properties()),
            _ => self
                .profile_data
                .as_ref()
                .map(|data| data.properties)
                .or_else(|| self.synthesised_profile()),
        }
    }

    /// Whether this controller is a hand the runtime tracks optically and binds to
    /// simple_controller (as Monado and WiVRn do), so it should be reported as a hand rather than
    /// a generic controller. This is latched until the interaction profile changes, so the
    /// identity doesn't flip whenever the hand briefly loses tracking. Without
    /// XR_EXT_hand_tracking_data_source, joints derived from a controller look the same.
    pub(super) fn is_tracked_hand(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
    ) -> bool {
        let has_tracker = matches!(
            self.device_type,
            TrackedDeviceType::Controller {
                hand_tracker: Some(_),
                ..
            }
        );
        if !has_tracker
            || !xr_data.enabled_extensions.ext_hand_tracking_data_source
            || !self
                .profile_data
                .as_ref()
                .is_some_and(|data| is_simple_controller(data.properties))
        {
            return false;
        }

        if !self.tracked_hand.load(Ordering::Relaxed)
            && self.get_hand_data_source(xr_data, session_data.tracking_space())
                == Some(xr::HandTrackingDataSourceEXT::UNOBSTRUCTED)
        {
            self.tracked_hand.store(true, Ordering::Relaxed);
        }
        self.tracked_hand.load(Ordering::Relaxed)
    }

    pub(super) fn reset_tracked_hand(&self) {
        self.tracked_hand.store(false, Ordering::Relaxed);
    }

    /// The interaction profile reported when the controller has no bound
    /// interaction profile but is tracked through hand tracking (e.g. WiVRn).
    fn synthesised_profile(&self) -> Option<&'static ProfileProperties> {
        match self.device_type {
            TrackedDeviceType::Controller {
                hand_tracker: Some(_),
                ..
            } => Some(VRLinkHand::properties()),
            _ => None,
        }
    }

    /// Modify a pose's position & orientation if the device defines a
    /// transformation for the given pose type.
    pub fn transform_pose(&self, raw_pose: xr::Posef, pose_type: BoundPoseType) -> xr::Posef {
        self.profile_data
            .as_ref()
            .and_then(|profile_data| {
                self.get_controller_hand().and_then(|hand| {
                    profile_data
                        .pose_transformation(hand, pose_type)
                        .map(|location_transform| {
                            let pose_mat = Mat4::from_rotation_translation(
                                Quat::from_xyzw(
                                    raw_pose.orientation.x,
                                    raw_pose.orientation.y,
                                    raw_pose.orientation.z,
                                    raw_pose.orientation.w,
                                ),
                                Vec3 {
                                    x: raw_pose.position.x,
                                    y: raw_pose.position.y,
                                    z: raw_pose.position.z,
                                },
                            );
                            let (_, new_quat, new_vec) =
                                (pose_mat * location_transform).to_scale_rotation_translation();
                            let [quat_x, quat_y, quat_z, quat_w] = new_quat.to_array();
                            xr::Posef {
                                orientation: xr::Quaternionf {
                                    x: quat_x,
                                    y: quat_y,
                                    z: quat_z,
                                    w: quat_w,
                                },
                                position: xr::Vector3f {
                                    x: new_vec.x,
                                    y: new_vec.y,
                                    z: new_vec.z,
                                },
                            }
                        })
                })
            })
            .unwrap_or(raw_pose)
    }

    fn get_string_property(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
        devices: &TrackedDeviceList,
        last_profile: Option<&'static ProfileProperties>,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<&CStr> {
        let hand = match self.device_type {
            TrackedDeviceType::Controller { hand, .. } => hand,
            _ => Hand::Left,
        };

        let data = self.get_profile_properties(xr_data, session_data, devices, last_profile)?;

        if let TrackedDeviceType::Hmd = self.device_type {
            return match property {
                vr::ETrackedDeviceProperty::ControllerType_String => Some(data.hmd.controller_type),
                vr::ETrackedDeviceProperty::ModelNumber_String => Some(data.hmd.model),
                vr::ETrackedDeviceProperty::SerialNumber_String => Some(data.hmd.serial_number),
                vr::ETrackedDeviceProperty::TrackingSystemName_String => {
                    Some(data.tracking_system_name)
                }
                vr::ETrackedDeviceProperty::ManufacturerName_String => Some(data.manufacturer_name),
                _ => None,
            };
        }

        match property {
            // Audica likes to apply controller specific tweaks via this property
            vr::ETrackedDeviceProperty::ControllerType_String => Some(data.openvr_controller_type),
            // I Expect You To Die 3 identifies controllers with this property -
            // why it couldn't just use ControllerType instead is beyond me...
            // Because some controllers have different model names for each hand......
            vr::ETrackedDeviceProperty::ModelNumber_String => Some(*data.model.get(hand)),
            // Resonite won't recognize controllers without this
            vr::ETrackedDeviceProperty::RenderModelName_String => {
                Some(*data.render_model_name.get(hand))
            }
            vr::ETrackedDeviceProperty::RegisteredDeviceType_String => {
                Some(*data.registered_device_type.get(hand))
            }
            vr::ETrackedDeviceProperty::TrackingSystemName_String => {
                Some(data.tracking_system_name)
            }
            // Required for controllers to be acknowledged in I Expect You To Die 3
            vr::ETrackedDeviceProperty::SerialNumber_String => match self.get_type() {
                TrackedDeviceType::Controller { .. } => Some(*data.serial_number.get(hand)),
                #[cfg(feature = "monado")]
                TrackedDeviceType::GenericTracker { serial, .. } => Some(serial.as_c_str()),
                TrackedDeviceType::Hmd => unreachable!(),
            },
            vr::ETrackedDeviceProperty::ManufacturerName_String => Some(data.manufacturer_name),
            // Per the OpenVR docs this defaults to the tracking system name
            vr::ETrackedDeviceProperty::InputProfilePath_String => {
                if data.input_profile_path.is_empty() {
                    Some(data.tracking_system_name)
                } else {
                    Some(data.input_profile_path)
                }
            }
            _ => None,
        }
    }

    fn get_int_property(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
        devices: &TrackedDeviceList,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<i32> {
        match property {
            vr::ETrackedDeviceProperty::DeviceClass_Int32 => {
                return Some(self.device_class().0 as i32);
            }
            // Engines like godot_openvr assign hands from this instead of
            // GetControllerRoleForTrackedDeviceIndex.
            vr::ETrackedDeviceProperty::ControllerRoleHint_Int32 => {
                let role = match self.device_type {
                    TrackedDeviceType::Hmd => vr::ETrackedControllerRole::Invalid,
                    TrackedDeviceType::Controller { hand, .. } => hand.into(),
                    // Like SteamVR's trackers, so they never take a hand's role.
                    #[cfg(feature = "monado")]
                    TrackedDeviceType::GenericTracker { .. } => vr::ETrackedControllerRole::OptOut,
                };
                return Some(role.0 as i32);
            }
            _ => {}
        }

        match self.device_type {
            TrackedDeviceType::Controller { .. } => {
                let data = self.get_profile_properties(xr_data, session_data, devices, None)?;

                match property {
                    vr::ETrackedDeviceProperty::Axis0Type_Int32 => match data.main_axis {
                        MainAxisType::Thumbstick => {
                            Some(vr::EVRControllerAxisType::Joystick.0 as _)
                        }
                        MainAxisType::Trackpad => Some(vr::EVRControllerAxisType::TrackPad.0 as _),
                    },
                    vr::ETrackedDeviceProperty::Axis1Type_Int32 => {
                        Some(vr::EVRControllerAxisType::Trigger.0 as _)
                    }
                    vr::ETrackedDeviceProperty::Axis2Type_Int32 => {
                        // This is actually the grip, and gets recognized as such
                        Some(vr::EVRControllerAxisType::Trigger.0 as _)
                    }
                    // TODO: report knuckles trackpad?
                    vr::ETrackedDeviceProperty::Axis3Type_Int32
                    | vr::ETrackedDeviceProperty::Axis4Type_Int32 => {
                        Some(vr::EVRControllerAxisType::None.0 as _)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn get_uint_property(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
        devices: &TrackedDeviceList,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<u64> {
        match self.device_type {
            TrackedDeviceType::Controller { .. } => {
                let data = self.get_profile_properties(xr_data, session_data, devices, None)?;

                match property {
                    vr::ETrackedDeviceProperty::SupportedButtons_Uint64 => {
                        Some(data.legacy_buttons_mask)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct SubactionPaths {
    pub left: xr::Path,
    pub right: xr::Path,
    pub gamepad: xr::Path,
}

impl SubactionPaths {
    pub fn new(instance: &xr::Instance) -> Self {
        let [left, right, gamepad] = InputDevice::ALL.map(|device| {
            instance
                .string_to_path(device.path())
                .expect("Failed to convert string to path")
        });
        Self {
            left,
            right,
            gamepad,
        }
    }

    pub fn get(&self, device: InputDevice) -> xr::Path {
        match device {
            InputDevice::Hand(Hand::Left) => self.left,
            InputDevice::Hand(Hand::Right) => self.right,
            InputDevice::Gamepad => self.gamepad,
        }
    }

    /// In the order of [InputDevice::ALL].
    pub fn all(&self) -> [xr::Path; 3] {
        InputDevice::ALL.map(|device| self.get(device))
    }
}

pub struct TrackedDeviceList {
    devices: Vec<TrackedDevice>,
    /// XrSystemHandTrackingPropertiesEXT::supportsHandTracking. Creating hand trackers fails
    /// without it.
    hand_tracking: bool,
}

impl TrackedDeviceList {
    pub fn new(session: &xr::Session<xr::AnyGraphics>, hand_tracking: bool) -> Self {
        let mut list = Self {
            devices: vec![TrackedDevice::new(TrackedDeviceType::Hmd, None, None)],
            hand_tracking,
        };
        for hand in [Hand::Left, Hand::Right] {
            let hand_tracker = list.create_hand_tracker(session, hand);
            list.devices.push(TrackedDevice::new(
                TrackedDeviceType::Controller {
                    hand,
                    hand_tracker,
                    skeleton_cache: Mutex::new(Default::default()),
                    pose_is_synthesised: false.into(),
                },
                None,
                None,
            ));
        }
        list
    }

    pub(super) fn create_hand_tracker<G>(
        &self,
        session: &xr::Session<G>,
        hand: Hand,
    ) -> Option<xr::HandTracker> {
        if !self.hand_tracking {
            return None;
        }
        create_hand_tracker(session, hand.into())
            .inspect_err(|e| log::warn!("Failed to create hand tracker for hand {hand:?}: {e}"))
            .ok()
    }
}

/// Creates a hand tracker that requests its data sources if XR_EXT_hand_tracking_data_source is
/// enabled: [get_joints_with_data_source] queries the data source state, which runtimes reject
/// for trackers created without them.
fn create_hand_tracker<G>(
    session: &xr::Session<G>,
    hand: xr::HandEXT,
) -> xr::Result<xr::HandTracker> {
    let exts = session.instance().exts();
    let Some(ext) = exts.ext_hand_tracking.as_ref() else {
        return Err(xr::sys::Result::ERROR_EXTENSION_NOT_PRESENT);
    };
    if exts.ext_hand_tracking_data_source.is_none() {
        return session.create_hand_tracker(hand);
    }

    let data_sources = [
        xr::HandTrackingDataSourceEXT::CONTROLLER,
        xr::HandTrackingDataSourceEXT::UNOBSTRUCTED,
    ];
    let data_source_info = xr::sys::HandTrackingDataSourceInfoEXT {
        ty: xr::sys::HandTrackingDataSourceInfoEXT::TYPE,
        next: std::ptr::null(),
        requested_data_source_count: data_sources.len() as u32,
        requested_data_sources: data_sources.as_ptr() as *mut xr::HandTrackingDataSourceEXT,
    };
    let info = xr::sys::HandTrackerCreateInfoEXT {
        ty: xr::sys::HandTrackerCreateInfoEXT::TYPE,
        next: &data_source_info as *const _ as _,
        hand,
        // If this ever changes, update the joint_counts set in `Space::locate_hand_joints`
        hand_joint_set: xr::sys::HandJointSetEXT::DEFAULT,
    };

    let handle = {
        let mut handle = xr::sys::HandTrackerEXT::NULL;
        let r = unsafe { (ext.create_hand_tracker)(session.as_raw(), &info, &mut handle) };
        if r.into_raw() < 0 {
            return Err(r);
        }
        handle
    };

    Ok(unsafe { xr::HandTracker::from_raw(session, handle) })
}

impl TrackedDeviceList {
    pub(super) fn get_device(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
    ) -> Option<&TrackedDevice> {
        self.devices.get(device_index as usize)
    }
    pub(super) fn get_device_mut(
        &mut self,
        device_index: vr::TrackedDeviceIndex_t,
    ) -> Option<&mut TrackedDevice> {
        self.devices.get_mut(device_index as usize)
    }

    pub(super) fn push_device(
        &mut self,
        device: TrackedDevice,
    ) -> Result<vr::TrackedDeviceIndex_t, vr::EVRInputError> {
        let index = self.devices.len() as vr::TrackedDeviceIndex_t;

        if index >= vr::k_unMaxTrackedDeviceCount {
            return Err(vr::EVRInputError::MaxCapacityReached);
        }

        self.devices.push(device);

        Ok(index)
    }

    pub(super) fn get_controller(&self, hand: Hand) -> Option<&TrackedDevice> {
        self.get_device(self.get_controller_index(hand)?)
    }
    pub(super) fn get_controller_mut(&mut self, hand: Hand) -> Option<&mut TrackedDevice> {
        self.get_device_mut(self.get_controller_index(hand)?)
    }

    pub(super) fn get_controller_index(&self, hand: Hand) -> Option<vr::TrackedDeviceIndex_t> {
        self.iter()
            .enumerate()
            .find(|(_, device)| device.get_controller_hand() == Some(hand))
            .map(|(i, _)| i as vr::TrackedDeviceIndex_t)
    }

    #[cfg(feature = "monado")]
    pub(super) fn create_monado_generic_trackers(
        &mut self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
    ) -> xr::Result<()> {
        if !xr_data
            .enabled_extensions
            .other
            .contains(&[XR_MNDX_XDEV_SPACE_EXTENSION_NAME.as_bytes(), b"\0"].concat())
        {
            return Ok(());
        }

        fn tracker_serial(device: &TrackedDevice) -> Option<&[u8]> {
            match &device.device_type {
                TrackedDeviceType::GenericTracker { serial, .. } => Some(serial.to_bytes()),
                _ => None,
            }
        }

        // Trackers identify as Vive Trackers, which some games sniff to pick a
        // controller scheme (e.g. SUPERHOT VR switches to its Vive scheme when
        // it sees one), so allow capping or disabling them per game.
        let default_max = if crate::quirks::get().no_generic_trackers {
            0
        } else {
            usize::MAX
        };
        let max_generic_trackers = std::env::var("XRIZER_MAX_TRACKERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_max)
            .min(
                vr::k_unMaxTrackedDeviceCount as usize
                    - self
                        .devices
                        .iter()
                        .filter(|device| tracker_serial(device).is_none())
                        .count(),
            );
        let extra_tracker_serials = std::env::var("XRIZER_TRACKER_SERIALS")
            .map_or(vec![], |trackers| {
                trackers.split(";").map(|t| t.to_string()).collect()
            });

        let mut xdevs: Vec<XDev> = session_data
            .session
            .get_xdev_list()?
            .enumerate_xdevs()?
            .into_iter()
            .filter(|xdev| {
                xdev.can_create_space()
                    && (xdev.name().to_lowercase().contains("tracker")
                        || extra_tracker_serials.contains(&xdev.serial().to_string()))
            })
            .collect();

        xdevs.truncate(max_generic_trackers);

        // Listed again on every interaction profile change. Trackers seen before keep their device
        // and index, like in SteamVR, and stay there disconnected once they aren't listed.
        for device in &mut self.devices {
            let listed = tracker_serial(device)
                .map(|serial| xdevs.iter().any(|xdev| xdev.serial().as_bytes() == serial));
            if let Some(listed) = listed {
                device.connected = listed;
            }
        }
        xdevs.retain(|xdev| {
            !self
                .devices
                .iter()
                .any(|device| tracker_serial(device) == Some(xdev.serial().as_bytes()))
        });
        xdevs.truncate(vr::k_unMaxTrackedDeviceCount as usize - self.devices.len());

        // Checked on every interaction profile change, which mostly finds none.
        let level = if xdevs.is_empty() {
            log::Level::Debug
        } else {
            log::Level::Info
        };
        log::log!(
            level,
            "Creating {} generic trackers via Monado XDev extension",
            xdevs.len()
        );

        let trackers = xdevs.into_iter().filter_map(|xdev| {
            let serial = CString::new(xdev.serial()).unwrap();
            let space = xdev
                .create_space(xr::Posef::IDENTITY)
                .inspect_err(|e| log::warn!("Failed to create space for {serial:?}: {e}"))
                .ok()?;
            let mut tracker = TrackedDevice::new(
                TrackedDeviceType::GenericTracker { serial, space },
                None,
                Some(ProfileData::new::<ViveTracker>()),
            );
            tracker.connected = true;
            Some(tracker)
        });
        self.devices.extend(trackers);

        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &TrackedDevice> {
        self.devices.iter()
    }
}

/// The pose that legacy bindings chosen for the app give `device`, if another than its own.
fn legacy_pose_type(session_data: &SessionData, device: &TrackedDevice) -> Option<BoundPoseType> {
    let loaded = session_data.input_data.get_loaded_actions()?;
    loaded.legacy.as_ref()?.pose_type(loaded, device)
}

impl<C: openxr_data::Compositor> Input<C> {
    pub fn get_poses(
        &self,
        poses: &mut [vr::TrackedDevicePose_t],
        origin: Option<vr::ETrackingUniverseOrigin>,
        xr_time: xr::Time,
    ) {
        tracy_span!();
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();

        // Slots without a device get an invalid, disconnected pose.
        for (i, pose) in poses.iter_mut().enumerate() {
            *pose = devices
                .get_device(i as u32)
                .map(|device| {
                    device.get_pose(
                        xr_time,
                        &session_data,
                        origin.unwrap_or(session_data.current_origin),
                        legacy_pose_type(&session_data, device),
                    )
                })
                .unwrap_or_default();
        }
    }

    pub fn get_controller_pose(
        &self,
        hand: Hand,
        origin: Option<vr::ETrackingUniverseOrigin>,
        xr_time: xr::Time,
        pose_type: Option<BoundPoseType>,
    ) -> Option<vr::TrackedDevicePose_t> {
        let session_data = self.openxr.session_data.get();
        let controller_index = session_data
            .input_data
            .devices
            .read()
            .unwrap()
            .get_controller_index(hand)?;

        self.get_device_pose(controller_index, origin, xr_time, pose_type)
    }

    pub fn get_device_pose(
        &self,
        index: vr::TrackedDeviceIndex_t,
        origin: Option<vr::ETrackingUniverseOrigin>,
        xr_time: xr::Time,
        pose_type: Option<BoundPoseType>,
    ) -> Option<vr::TrackedDevicePose_t> {
        tracy_span!();

        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();
        let device = devices.get_device(index)?;

        Some(device.get_pose(
            xr_time,
            &session_data,
            origin.unwrap_or(session_data.current_origin),
            pose_type.or_else(|| legacy_pose_type(&session_data, device)),
        ))
    }

    pub fn is_device_connected(&self, index: vr::TrackedDeviceIndex_t) -> bool {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();

        devices
            .get_device(index)
            .is_some_and(|d| d.is_connected(&self.openxr, &session_data))
    }

    pub fn device_index_to_tracked_device_class(
        &self,
        index: vr::TrackedDeviceIndex_t,
    ) -> Option<vr::ETrackedDeviceClass> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();

        devices.get_device(index).map(TrackedDevice::device_class)
    }

    pub fn device_index_to_hand(&self, index: vr::TrackedDeviceIndex_t) -> Option<Hand> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();
        let device = devices.get_device(index)?;

        device.get_controller_hand()
    }

    pub fn get_controller_device_index(&self, hand: Hand) -> Option<vr::TrackedDeviceIndex_t> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();

        devices.get_controller_index(hand)
    }

    pub fn get_device_string_tracked_property(
        &self,
        index: vr::TrackedDeviceIndex_t,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<CString> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();
        let device = devices.get_device(index)?;

        let last_profile = *self.last_profile_properties.lock().unwrap();
        device
            .get_string_property(
                &self.openxr,
                &session_data,
                &devices,
                last_profile,
                property,
            )
            .map(|s| s.to_owned())
    }

    pub fn get_device_int_tracked_property(
        &self,
        index: vr::TrackedDeviceIndex_t,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<i32> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();
        let device = devices.get_device(index)?;

        device.get_int_property(&self.openxr, &session_data, &devices, property)
    }

    pub fn get_device_uint_tracked_property(
        &self,
        index: vr::TrackedDeviceIndex_t,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<u64> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();
        let device = devices.get_device(index)?;

        device.get_uint_property(&self.openxr, &session_data, &devices, property)
    }
}

#[cfg(test)]
mod tests {
    use super::{PoseCache, TrackedDeviceType};
    use crate::input::BoundPoseType;
    use crate::input::profiles::{knuckles::Knuckles, oculus_touch::OculusTouch};
    use crate::input::tests::Fixture;
    use crate::openxr_data::Hand;
    use openvr as vr;
    use openxr as xr;

    fn hmd_height(f: &Fixture, origin: vr::ETrackingUniverseOrigin, nanos: i64) -> f32 {
        let pose = f
            .input
            .get_device_pose(
                vr::k_unTrackedDeviceIndex_Hmd,
                Some(origin),
                xr::Time::from_nanos(nanos),
                None,
            )
            .unwrap();
        assert!(pose.bPoseIsValid);
        pose.mDeviceToAbsoluteTracking.m[1][3]
    }

    #[test]
    fn pose_cache_is_per_origin() {
        let f = Fixture::new();
        fakexr::set_reference_space_pose(
            f.raw_session(),
            xr::ReferenceSpaceType::STAGE,
            xr::Posef {
                position: xr::Vector3f {
                    x: 0.0,
                    y: -1.5,
                    z: 0.0,
                },
                ..xr::Posef::IDENTITY
            },
        );

        let seated = vr::ETrackingUniverseOrigin::Seated;
        let standing = vr::ETrackingUniverseOrigin::Standing;
        assert_eq!(hmd_height(&f, seated, 1), 0.0);
        assert_eq!(hmd_height(&f, standing, 1), 1.5);

        assert_eq!(hmd_height(&f, standing, 2), 1.5);
        assert_eq!(hmd_height(&f, seated, 2), 0.0);
    }

    #[test]
    fn pose_cache_keeps_poses_read_again() {
        let f = Fixture::new();
        let standing = vr::ETrackingUniverseOrigin::Standing;
        assert_eq!(hmd_height(&f, standing, 1), 0.0);

        fakexr::set_reference_space_pose(
            f.raw_session(),
            xr::ReferenceSpaceType::STAGE,
            xr::Posef {
                position: xr::Vector3f {
                    x: 0.0,
                    y: -1.5,
                    z: 0.0,
                },
                ..xr::Posef::IDENTITY
            },
        );
        // the display time pose stays the same, however many "now" queries come in between
        for nanos in 2..=100 {
            assert_eq!(hmd_height(&f, standing, nanos), 1.5);
            assert_eq!(hmd_height(&f, standing, 1), 0.0);
        }
    }

    #[test]
    fn pose_cache_is_bounded() {
        let f = Fixture::new();
        // e.g. an app polling GetDeviceToAbsoluteTrackingPose without ever calling WaitGetPoses
        for nanos in 1..=100 {
            hmd_height(&f, vr::ETrackingUniverseOrigin::Seated, nanos);
        }

        let data = f.input.openxr.session_data.get();
        let devices = data.input_data.devices.read().unwrap();
        let hmd = devices.get_device(vr::k_unTrackedDeviceIndex_Hmd).unwrap();
        assert_eq!(
            hmd.pose_cache.lock().unwrap().recent.len(),
            PoseCache::MAX_RECENT
        );
    }

    #[test]
    fn unknown_origins_share_the_raw_pose() {
        let f = Fixture::new();
        for origin in [7, 8, u32::MAX] {
            hmd_height(&f, vr::ETrackingUniverseOrigin(origin), 1);
        }
        hmd_height(&f, vr::ETrackingUniverseOrigin::RawAndUncalibrated, 1);

        let data = f.input.openxr.session_data.get();
        let devices = data.input_data.devices.read().unwrap();
        let hmd = devices.get_device(vr::k_unTrackedDeviceIndex_Hmd).unwrap();
        let cache = hmd.pose_cache.lock().unwrap();
        assert_eq!(cache.recent.len(), 1);
        assert_eq!(cache.last_valid.len(), 1);
    }

    /// The Tip pose, which has a pose transformation on Knuckles.
    fn tip_pose(f: &Fixture, hand: Hand, nanos: i64) -> vr::TrackedDevicePose_t {
        f.input
            .get_controller_pose(
                hand,
                Some(vr::ETrackingUniverseOrigin::Seated),
                xr::Time::from_nanos(nanos),
                Some(BoundPoseType::Tip),
            )
            .unwrap()
    }

    fn sync_set1(f: &mut Fixture) {
        let set1 = f.get_action_set_handle(c"/actions/set1");
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
    }

    /// Pose actions lose tracking when their set isn't synced.
    fn stop_tracking(f: &Fixture) {
        let data = f.input.openxr.session_data.get();
        data.session.sync_actions(&[]).unwrap();
    }

    /// Binds Knuckles to the right hand, locates it at time 1, then stops tracking it. Returns the
    /// tracked pose.
    fn lose_right_controller(f: &mut Fixture) -> vr::TrackedDevicePose_t {
        f.load_actions(c"actions.json");
        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::RightHand);
        // the profile is bound after two frames
        for _ in 0..2 {
            f.input.openxr.poll_events();
            f.input.frame_start_update();
        }
        fakexr::set_grip(
            f.raw_session(),
            fakexr::UserPath::RightHand,
            xr::Posef {
                position: xr::Vector3f {
                    x: 0.1,
                    y: 0.2,
                    z: 0.3,
                },
                ..xr::Posef::IDENTITY
            },
        );
        sync_set1(f);

        let tracked = tip_pose(f, Hand::Right, 1);
        assert!(tracked.bPoseIsValid);
        assert!(tracked.bDeviceIsConnected);
        assert_eq!(tracked.eTrackingResult, vr::ETrackingResult::Running_OK);

        stop_tracking(f);
        tracked
    }

    #[test]
    fn untracked_controller_stays_connected() {
        let mut f = Fixture::new();
        let tracked = lose_right_controller(&mut f);

        let untracked = tip_pose(&f, Hand::Right, 2);
        assert!(!untracked.bPoseIsValid);
        assert!(untracked.bDeviceIsConnected);
        assert_eq!(
            untracked.eTrackingResult,
            vr::ETrackingResult::Running_OutOfRange
        );
        // the last pose gets the Tip transformation too
        assert_eq!(
            untracked.mDeviceToAbsoluteTracking.m,
            tracked.mDeviceToAbsoluteTracking.m
        );

        // a controller without an interaction profile isn't connected
        let left = tip_pose(&f, Hand::Left, 2);
        assert!(!left.bDeviceIsConnected);
        assert!(!left.bPoseIsValid);
    }

    #[test]
    fn profile_change_forgets_last_pose() {
        let mut f = Fixture::new();
        lose_right_controller(&mut f);

        f.set_interaction_profile::<OculusTouch>(fakexr::UserPath::RightHand);
        sync_set1(&mut f);
        stop_tracking(&f);

        // the last pose was of another controller, or with another offset
        let untracked = tip_pose(&f, Hand::Right, 2);
        assert!(!untracked.bPoseIsValid);
        assert!(untracked.bDeviceIsConnected);
        assert_eq!(untracked.mDeviceToAbsoluteTracking.m, [[0.0; 4]; 3]);
    }

    #[test]
    fn zero_pose_reset_forgets_last_pose() {
        let mut f = Fixture::new();
        lose_right_controller(&mut f);

        // the last pose is in the old seated space
        f.input
            .openxr
            .reset_tracking_space(vr::ETrackingUniverseOrigin::Seated);
        let untracked = tip_pose(&f, Hand::Right, 2);
        assert!(!untracked.bPoseIsValid);
        assert!(untracked.bDeviceIsConnected);
        assert_eq!(untracked.mDeviceToAbsoluteTracking.m, [[0.0; 4]; 3]);
    }

    #[test]
    #[cfg_attr(not(feature = "monado"), ignore)]
    fn get_tracker_pose() {
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::LeftHand);
        fakexr::add_trackers(f.input.openxr.session_data.get().session.as_raw());

        let frame = || {
            f.input.openxr.poll_events();
            f.input.frame_start_update();
        };

        // we need to wait two frames for the tracker to be connected.
        frame();
        assert!(f.input.device_index_to_tracked_device_class(3).is_none());
        frame();
        assert!(
            f.input.device_index_to_tracked_device_class(3)
                == Some(vr::ETrackedDeviceClass::GenericTracker)
        );

        let pose2 = f.input.get_device_pose(
            3,
            Some(vr::ETrackingUniverseOrigin::Seated),
            f.input.openxr.get_display_time(),
            None,
        );
        assert!(pose2.is_some());
    }

    #[test]
    #[cfg_attr(not(feature = "monado"), ignore)]
    fn get_tracker_serial() {
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::LeftHand);
        fakexr::add_trackers(f.input.openxr.session_data.get().session.as_raw());

        let frame = || {
            f.input.openxr.poll_events();
            f.input.frame_start_update();
        };

        // we need to wait two frames for the tracker to be connected.
        frame();
        assert!(f.input.device_index_to_tracked_device_class(3).is_none());
        frame();
        assert!(
            f.input.device_index_to_tracked_device_class(3)
                == Some(vr::ETrackedDeviceClass::GenericTracker)
        );

        let serial = f
            .input
            .get_device_string_tracked_property(3, vr::ETrackedDeviceProperty::SerialNumber_String)
            .unwrap();
        assert_eq!(serial.to_str().unwrap(), "FAKEXR-SERIAL");
    }

    #[test]
    #[cfg_attr(not(feature = "monado"), ignore)]
    fn tracker_role_hint_and_device_class() {
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::LeftHand);
        fakexr::add_trackers(f.input.openxr.session_data.get().session.as_raw());
        // we need to wait two frames for the tracker to be connected.
        for _ in 0..2 {
            f.input.openxr.poll_events();
            f.input.frame_start_update();
        }

        let get = |prop| f.input.get_device_int_tracked_property(3, prop);
        assert_eq!(
            get(vr::ETrackedDeviceProperty::ControllerRoleHint_Int32),
            Some(vr::ETrackedControllerRole::OptOut.0 as i32)
        );
        assert_eq!(
            get(vr::ETrackedDeviceProperty::DeviceClass_Int32),
            Some(vr::ETrackedDeviceClass::GenericTracker.0 as i32)
        );
    }

    fn hand_skeleton(f: &Fixture, hand: Hand) -> Option<super::HandSkeleton> {
        let session = f.input.openxr.session_data.get();
        let devices = session.input_data.devices.read().unwrap();
        devices
            .get_controller(hand)
            .unwrap()
            .get_hand_skeleton(&f.input.openxr, session.tracking_space())
    }

    #[test]
    fn no_hand_trackers_without_system_support() {
        let f = Fixture::new();
        assert_eq!(fakexr::hand_trackers_created(), 0);
        let session = f.input.openxr.session_data.get();
        let devices = session.input_data.devices.read().unwrap();
        for hand in [Hand::Left, Hand::Right] {
            let TrackedDeviceType::Controller { hand_tracker, .. } =
                devices.get_controller(hand).unwrap().get_type()
            else {
                unreachable!()
            };
            assert!(hand_tracker.is_none());
        }
    }

    #[test]
    fn hand_trackers_request_data_sources() {
        fakexr::set_hand_tracking_supported(true);
        let f = Fixture::new();
        fakexr::set_hand_tracked(
            f.raw_session(),
            fakexr::UserPath::LeftHand,
            Some(xr::HandTrackingDataSourceEXT::CONTROLLER),
        );

        // The runtime rejects data source queries for trackers that didn't request them.
        let skeleton = hand_skeleton(&f, Hand::Left).expect("left hand should be tracked");
        assert_eq!(
            skeleton.data_source,
            xr::HandTrackingDataSourceEXT::CONTROLLER
        );
        assert!(hand_skeleton(&f, Hand::Right).is_none());
    }

    #[test]
    fn hand_tracked_pose_is_per_origin() {
        use std::sync::atomic::Ordering;

        fakexr::set_hand_tracking_supported(true);
        let f = Fixture::new();
        fakexr::set_reference_space_pose(
            f.raw_session(),
            xr::ReferenceSpaceType::STAGE,
            xr::Posef {
                position: xr::Vector3f {
                    x: 0.0,
                    y: -1.5,
                    z: 0.0,
                },
                ..xr::Posef::IDENTITY
            },
        );
        // A hand without a controller, so its pose is synthesised from the joints.
        fakexr::set_hand_tracked(
            f.raw_session(),
            fakexr::UserPath::LeftHand,
            Some(xr::HandTrackingDataSourceEXT::UNOBSTRUCTED),
        );

        let pose = |origin| {
            let pose = f
                .input
                .get_controller_pose(Hand::Left, Some(origin), xr::Time::from_nanos(1), None)
                .unwrap();
            assert!(pose.bPoseIsValid, "{origin:?}");
            assert!(pose.bDeviceIsConnected, "{origin:?}");
            pose.mDeviceToAbsoluteTracking.m
        };
        let seated = pose(vr::ETrackingUniverseOrigin::Seated);
        let standing = pose(vr::ETrackingUniverseOrigin::Standing);

        let session = f.input.openxr.session_data.get();
        let devices = session.input_data.devices.read().unwrap();
        let TrackedDeviceType::Controller {
            pose_is_synthesised,
            ..
        } = devices.get_controller(Hand::Left).unwrap().get_type()
        else {
            unreachable!()
        };
        assert!(pose_is_synthesised.load(Ordering::Relaxed));

        // Each origin is located in its own space: only the stage offset tells them apart.
        for (row, (seated, standing)) in seated.iter().zip(standing).enumerate() {
            for (col, (seated, standing)) in seated.iter().zip(standing).enumerate() {
                let offset = if (row, col) == (1, 3) { 1.5 } else { 0.0 };
                assert!(
                    (standing - seated - offset).abs() < 1e-5,
                    "[{row}][{col}]: seated {seated}, standing {standing}"
                );
            }
        }
    }

    #[test]
    fn tracked_hands_report_hand_identity() {
        use crate::input::profiles::{
            oculus_touch::OculusTouch, simple_controller::SimpleController,
        };
        use fakexr::UserPath::*;
        use vr::ETrackedDeviceProperty as Prop;
        use xr::HandTrackingDataSourceEXT as Source;
        const HAND: Option<&str> = Some("svl_hand_interaction_augmented");
        const GENERIC: Option<&str> = Some("<unknown>");
        const TOUCH: Option<&str> = Some("oculus_touch");

        fakexr::set_hand_tracking_supported(true);
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        let set = f.get_action_set_handle(c"/actions/set1");
        let sync = |f: &mut Fixture| {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set,
                ..Default::default()
            });
            // drop cached hand skeletons
            f.input.frame_start_update();
        };
        let get = |f: &Fixture, index, prop| {
            f.input
                .get_device_string_tracked_property(index, prop)
                .map(|s| s.into_string().unwrap())
        };
        let ty = |f: &Fixture, index| get(f, index, Prop::ControllerType_String);
        let hmd = |f: &Fixture| {
            get(
                f,
                vr::k_unTrackedDeviceIndex_Hmd,
                Prop::TrackingSystemName_String,
            )
        };

        // Hands bound to simple_controller, before any real controller showed up.
        f.set_interaction_profile::<SimpleController>(LeftHand);
        fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::UNOBSTRUCTED));
        sync(&mut f);
        assert_eq!(ty(&f, 1).as_deref(), HAND);
        assert_eq!(hmd(&f).as_deref(), Some("vrlink"));

        // A real controller identifies the headset from then on.
        f.set_interaction_profile::<OculusTouch>(RightHand);
        sync(&mut f);
        assert_eq!(ty(&f, 2).as_deref(), TOUCH);
        assert_eq!(hmd(&f).as_deref(), Some("oculus"));

        // Joints derived from a controller aren't a hand.
        f.set_interaction_profile::<SimpleController>(RightHand);
        fakexr::set_hand_tracked(f.raw_session(), RightHand, Some(Source::CONTROLLER));
        sync(&mut f);
        assert_eq!(ty(&f, 2).as_deref(), GENERIC);
        assert_eq!(hmd(&f).as_deref(), Some("oculus"));

        // A hand stays one while it isn't tracked, until the profile changes.
        fakexr::set_hand_tracked(f.raw_session(), RightHand, Some(Source::UNOBSTRUCTED));
        sync(&mut f);
        assert_eq!(ty(&f, 2).as_deref(), HAND);
        fakexr::set_hand_tracked(f.raw_session(), RightHand, None);
        sync(&mut f);
        assert_eq!(ty(&f, 2).as_deref(), HAND);
        assert_eq!(hmd(&f).as_deref(), Some("oculus"));

        f.set_interaction_profile::<OculusTouch>(RightHand);
        sync(&mut f);
        assert_eq!(ty(&f, 2).as_deref(), TOUCH);
    }

    #[test]
    fn role_hint_and_device_class() {
        use vr::ETrackedDeviceProperty as Prop;

        let f = Fixture::new();
        let get = |index, prop| f.input.get_device_int_tracked_property(index, prop);

        use vr::ETrackedControllerRole as Role;
        use vr::ETrackedDeviceClass as Class;
        for (index, role, class) in [
            (vr::k_unTrackedDeviceIndex_Hmd, Role::Invalid, Class::HMD),
            (1, Role::LeftHand, Class::Controller),
            (2, Role::RightHand, Class::Controller),
        ] {
            assert_eq!(
                get(index, Prop::ControllerRoleHint_Int32),
                Some(role.0 as i32)
            );
            assert_eq!(get(index, Prop::DeviceClass_Int32), Some(class.0 as i32));
        }
    }

    #[test]
    fn hmd_properties_follow_controller_profile() {
        use crate::input::profiles::oculus_touch::OculusTouch;
        use vr::ETrackedDeviceProperty as Prop;

        let mut f = Fixture::new();
        f.load_actions(c"actions.json");

        let get = |f: &Fixture, prop| {
            f.input
                .get_device_string_tracked_property(vr::k_unTrackedDeviceIndex_Hmd, prop)
                .map(|s| s.to_str().unwrap().to_owned())
        };

        // No controllers connected yet - nothing to derive HMD identity from.
        assert_eq!(get(&f, Prop::TrackingSystemName_String), None);

        let set = f.get_action_set_handle(c"/actions/set1");
        // profile changes are only applied by the runtime after an action sync
        let sync = |f: &mut Fixture| {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set,
                ..Default::default()
            })
        };

        f.set_interaction_profile::<OculusTouch>(fakexr::UserPath::RightHand);
        sync(&mut f);

        assert_eq!(
            get(&f, Prop::TrackingSystemName_String).as_deref(),
            Some("oculus")
        );
        assert_eq!(
            get(&f, Prop::ManufacturerName_String).as_deref(),
            Some("Oculus")
        );
        assert_eq!(
            get(&f, Prop::ModelNumber_String).as_deref(),
            Some("Oculus Quest2")
        );
        assert_eq!(
            get(&f, Prop::ControllerType_String).as_deref(),
            Some("rift")
        );
        assert_eq!(
            get(&f, Prop::SerialNumber_String).as_deref(),
            Some("WMHD315M3010GV")
        );

        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::RightHand);
        sync(&mut f);

        assert_eq!(
            get(&f, Prop::TrackingSystemName_String).as_deref(),
            Some("lighthouse")
        );
        assert_eq!(get(&f, Prop::ModelNumber_String).as_deref(), Some("Index"));
        assert_eq!(
            get(&f, Prop::ControllerType_String).as_deref(),
            Some("indexhmd")
        );
    }
}
