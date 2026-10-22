use std::any::TypeId;
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

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

use crate::input::profiles::{PoseTransformations, ProfileProperties};
use crate::openxr_data::{self, Hand, OpenXrData, SessionData};
use crate::tracy_span;
use log::trace;

use super::{
    Input, InteractionProfile,
    profiles::{self, MainAxisType, RunWithProfile, vrlink_hand::VRLinkHand},
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
}

impl ProfileData {
    pub fn new<P: InteractionProfile>() -> Self {
        Self {
            properties: P::properties(),
            get_hand_offset: P::offset_grip_pose,
            get_pose_transformation: P::pose_transformation,
            force_estimated_thumb: TypeId::of::<P>() == TypeId::of::<Knuckles>(),
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

pub struct TrackedDevice {
    device_type: TrackedDeviceType,
    pub profile_data: Option<ProfileData>,
    pub profile_path: xr::Path,
    pub connected: bool,
    pub previous_connected: bool,
    pose_cache: Mutex<HashMap<i64, vr::TrackedDevicePose_t>>,
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
                .tracking_space()
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

impl TrackedDevice {
    pub(super) fn new(
        device_type: TrackedDeviceType,
        profile_path: Option<xr::Path>,
        profile_data: Option<ProfileData>,
    ) -> Self {
        Self {
            profile_data,
            profile_path: profile_path.unwrap_or(xr::Path::NULL),
            connected: matches!(device_type, TrackedDeviceType::Hmd),
            device_type,
            previous_connected: false,
            pose_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_pose(
        &self,
        xr_time: xr::Time,
        session_data: &SessionData,
        origin: vr::ETrackingUniverseOrigin,
        pose_type: Option<BoundPoseType>,
    ) -> Option<vr::TrackedDevicePose_t> {
        let nanos = xr_time.as_nanos();
        let mut pose_cache = self.pose_cache.lock().unwrap();
        let pose = if let Some(pose) = pose_cache.get(&nanos) {
            *pose
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
            }?;
            *pose_cache.entry(nanos).or_insert(pose)
        };

        // The cache holds the raw pose; the profile's pose transformation is
        // applied per call, since different bindings can ask for different
        // pose types (raw, tip, handgrip, ...) in the same frame.
        Some(match pose_type {
            Some(p_type) if pose.bPoseIsValid => {
                let mut pose = pose;
                pose.mDeviceToAbsoluteTracking = vr::HmdMatrix34_t::from(
                    self.transform_pose(xr::Posef::from(pose.mDeviceToAbsoluteTracking), p_type),
                );
                pose
            }
            _ => pose,
        })
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

        let display_time = xr_data.display_time.get();
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

    pub fn clear_pose_cache(&self) {
        self.pose_cache.lock().unwrap().clear();
        if let TrackedDeviceType::Controller { skeleton_cache, .. } = self.get_type() {
            skeleton_cache.lock().unwrap().clear();
        }
    }

    pub fn is_connected(
        &self,
        xr_data: &OpenXrData<impl openxr_data::Compositor>,
        session_data: &SessionData,
    ) -> bool {
        match self.get_type() {
            TrackedDeviceType::Controller { .. } => {
                self.connected
                    || self
                        .get_hand_skeleton(xr_data, session_data.tracking_space())
                        .is_some()
            }
            _ => self.connected,
        }
    }

    pub fn has_connected_changed(&mut self) -> bool {
        if self.previous_connected != self.connected {
            self.previous_connected = self.connected;
            true
        } else {
            false
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

    /// Returns the properties of the interaction profile this device's identity
    /// should be derived from.
    fn get_profile_properties(
        &self,
        devices: &TrackedDeviceList,
        last_profile: Option<&'static ProfileProperties>,
    ) -> Option<&'static ProfileProperties> {
        match self.device_type {
            TrackedDeviceType::Hmd => [Hand::Left, Hand::Right]
                .into_iter()
                .filter_map(|hand| devices.get_controller(hand))
                .find_map(|controller| controller.profile_data.as_ref())
                .map(|data| data.properties)
                .or(last_profile),
            _ => self
                .profile_data
                .as_ref()
                .map(|data| data.properties)
                .or_else(|| self.synthesised_profile()),
        }
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
        devices: &TrackedDeviceList,
        last_profile: Option<&'static ProfileProperties>,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<&CStr> {
        let hand = match self.device_type {
            TrackedDeviceType::Controller { hand, .. } => hand,
            _ => Hand::Left,
        };

        let data = self.get_profile_properties(devices, last_profile)?;

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
        _xr_data: &OpenXrData<impl openxr_data::Compositor>,
        _session_data: &SessionData,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<i32> {
        match self.device_type {
            TrackedDeviceType::Controller { .. } => {
                let data = self
                    .profile_data
                    .as_ref()
                    .map(|data| data.properties)
                    .or_else(|| self.synthesised_profile())?;

                match property {
                    vr::ETrackedDeviceProperty::Axis0Type_Int32 => match data.main_axis {
                        MainAxisType::Thumbstick => Some(vr::EVRControllerAxisType::Joystick as _),
                        MainAxisType::Trackpad => Some(vr::EVRControllerAxisType::TrackPad as _),
                    },
                    vr::ETrackedDeviceProperty::Axis1Type_Int32 => {
                        Some(vr::EVRControllerAxisType::Trigger as _)
                    }
                    vr::ETrackedDeviceProperty::Axis2Type_Int32 => {
                        // This is actually the grip, and gets recognized as such
                        Some(vr::EVRControllerAxisType::Trigger as _)
                    }
                    // TODO: report knuckles trackpad?
                    vr::ETrackedDeviceProperty::Axis3Type_Int32
                    | vr::ETrackedDeviceProperty::Axis4Type_Int32 => {
                        Some(vr::EVRControllerAxisType::None as _)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn get_uint_property(
        &self,
        _xr_data: &OpenXrData<impl openxr_data::Compositor>,
        _session_data: &SessionData,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<u64> {
        match self.device_type {
            TrackedDeviceType::Controller { .. } => {
                let data = self
                    .profile_data
                    .as_ref()
                    .map(|data| data.properties)
                    .or_else(|| self.synthesised_profile())?;

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

pub struct SubactionPaths {
    pub left: xr::Path,
    pub right: xr::Path,
}

impl SubactionPaths {
    pub fn new(instance: &xr::Instance) -> Self {
        let left = instance
            .string_to_path("/user/hand/left")
            .expect("Failed to convert string to path");
        let right = instance
            .string_to_path("/user/hand/right")
            .expect("Failed to convert string to path");

        Self { left, right }
    }
}

pub struct TrackedDeviceList {
    devices: Vec<TrackedDevice>,
}

impl TrackedDeviceList {
    pub fn new(session: &xr::Session<xr::AnyGraphics>) -> Self {
        let create_hand_tracker = |hand: Hand| {
            session
                .create_hand_tracker(hand.into())
                .inspect_err(|e| {
                    if !matches!(
                        *e,
                        xr::sys::Result::ERROR_EXTENSION_NOT_PRESENT
                            | xr::sys::Result::ERROR_FEATURE_UNSUPPORTED
                    ) {
                        log::debug!("Failed to create hand tracker for hand {hand:?}: {e}");
                    }
                })
                .ok()
        };

        Self {
            devices: vec![
                TrackedDevice::new(TrackedDeviceType::Hmd, None, None),
                TrackedDevice::new(
                    TrackedDeviceType::Controller {
                        hand: Hand::Left,
                        hand_tracker: create_hand_tracker(Hand::Left),
                        skeleton_cache: Mutex::new(Default::default()),
                        pose_is_synthesised: false.into(),
                    },
                    None,
                    None,
                ),
                TrackedDevice::new(
                    TrackedDeviceType::Controller {
                        hand: Hand::Right,
                        hand_tracker: create_hand_tracker(Hand::Right),
                        skeleton_cache: Mutex::new(Default::default()),
                        pose_is_synthesised: false.into(),
                    },
                    None,
                    None,
                ),
            ],
        }
    }
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

    fn get_controller_index(&self, hand: Hand) -> Option<vr::TrackedDeviceIndex_t> {
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

        self.devices.retain(|device| {
            !matches!(device.device_type, TrackedDeviceType::GenericTracker { .. })
        });

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
            .min(vr::k_unMaxTrackedDeviceCount as usize - self.devices.len());
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

        log::info!(
            "Creating {} generic trackers via Monado XDev extension",
            xdevs.len()
        );

        let trackers = xdevs.into_iter().map(|xdev| {
            let serial = CString::new(xdev.serial()).unwrap();
            let space = xdev.create_space(xr::Posef::IDENTITY).unwrap();
            let mut tracker = TrackedDevice::new(
                TrackedDeviceType::GenericTracker { serial, space },
                None,
                Some(ProfileData::new::<ViveTracker>()),
            );
            tracker.connected = true;
            tracker
        });
        self.devices.extend(trackers);

        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &TrackedDevice> {
        self.devices.iter()
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut TrackedDevice> {
        self.devices.iter_mut()
    }
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

        for (i, pose) in poses.iter_mut().enumerate() {
            let device = devices.get_device(i as u32);

            if let Some(device) = device {
                *pose = device
                    .get_pose(
                        xr_time,
                        &session_data,
                        origin.unwrap_or(session_data.current_origin),
                        None,
                    )
                    .unwrap_or_default();
            }
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

        devices.get_device(index)?.get_pose(
            xr_time,
            &session_data,
            origin.unwrap_or(session_data.current_origin),
            pose_type,
        )
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
        let device = devices.get_device(index)?;

        match device.get_type() {
            TrackedDeviceType::Hmd => Some(vr::ETrackedDeviceClass::HMD),
            TrackedDeviceType::Controller { .. } => Some(vr::ETrackedDeviceClass::Controller),
            #[cfg(feature = "monado")]
            TrackedDeviceType::GenericTracker { .. } => {
                Some(vr::ETrackedDeviceClass::GenericTracker)
            }
        }
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
            .get_string_property(&devices, last_profile, property)
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

        device.get_int_property(&self.openxr, &session_data, property)
    }

    pub fn get_device_uint_tracked_property(
        &self,
        index: vr::TrackedDeviceIndex_t,
        property: vr::ETrackedDeviceProperty,
    ) -> Option<u64> {
        let session_data = self.openxr.session_data.get();
        let devices = session_data.input_data.devices.read().unwrap();
        let device = devices.get_device(index)?;

        device.get_uint_property(&self.openxr, &session_data, property)
    }
}

#[cfg(test)]
mod tests {
    use crate::input::{profiles::knuckles::Knuckles, tests::Fixture};
    use openvr as vr;

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
            f.input.openxr.display_time.get(),
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
    fn hmd_properties_follow_controller_profile() {
        use crate::input::profiles::oculus_touch::OculusTouch;
        use vr::ETrackedDeviceProperty::*;

        let mut f = Fixture::new();
        f.load_actions(c"actions.json");

        let get = |f: &Fixture, prop| {
            f.input
                .get_device_string_tracked_property(vr::k_unTrackedDeviceIndex_Hmd, prop)
                .map(|s| s.to_str().unwrap().to_owned())
        };

        // No controllers connected yet - nothing to derive HMD identity from.
        assert_eq!(get(&f, TrackingSystemName_String), None);

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
            get(&f, TrackingSystemName_String).as_deref(),
            Some("oculus")
        );
        assert_eq!(get(&f, ManufacturerName_String).as_deref(), Some("Oculus"));
        assert_eq!(
            get(&f, ModelNumber_String).as_deref(),
            Some("Oculus Quest2")
        );
        assert_eq!(get(&f, ControllerType_String).as_deref(), Some("rift"));
        assert_eq!(
            get(&f, SerialNumber_String).as_deref(),
            Some("WMHD315M3010GV")
        );

        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::RightHand);
        sync(&mut f);

        assert_eq!(
            get(&f, TrackingSystemName_String).as_deref(),
            Some("lighthouse")
        );
        assert_eq!(get(&f, ModelNumber_String).as_deref(), Some("Index"));
        assert_eq!(get(&f, ControllerType_String).as_deref(), Some("indexhmd"));
    }
}
