#[path = "skeletal_generated.rs"]
mod generated;

use super::{ActionKey, Input, ManifestLoadedActions, SpaceReadGuard, inactive_state};
use crate::input::TrackedDeviceType;
use crate::input::devices::{HandSkeleton, TrackedDevice};
use crate::openxr_data::{self, Hand, SessionData};
use HandSkeletonBone::*;
use glam::{Affine3A, Quat, Vec3};
use log::debug;
use openvr as vr;
use openxr::{self as xr};
use paste::paste;
use slotmap::KeyData;
use std::cell::RefCell;
use std::f32::consts::{FRAC_PI_2, PI};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(super) type BoneCache = Mutex<Option<[vr::VRBoneTransform_t; Count as usize]>>;

impl<C: openxr_data::Compositor> Input<C> {
    /// The bones computed this frame, in parent space.
    pub(super) fn bone_cache(&self, hand: Hand, range: vr::EVRSkeletalMotionRange) -> &BoneCache {
        let with_controller = range == vr::EVRSkeletalMotionRange::WithController;
        &self.skeletal_bone_cache[hand as usize - 1][usize::from(with_controller)]
    }

    /// Tracked bones are the same for both motion ranges: like OpenComposite, they aren't
    /// limited to what the controller allows.
    pub(super) fn get_bones_from_hand_tracking(
        &self,
        session_data: &SessionData,
        space: vr::EVRSkeletalTransformSpace,
        hand: Hand,
        range: vr::EVRSkeletalMotionRange,
    ) -> Option<[vr::VRBoneTransform_t; Count as usize]> {
        use HandSkeletonBone::*;

        // Same order as frame_start_update and get_estimated_bones: devices, then the bone cache.
        let devices = session_data.input_data.devices.read().unwrap();
        let mut bone_cache = self.bone_cache(hand, range).lock().unwrap();
        if let Some(mut bones) = *bone_cache {
            finalize_transforms(&mut bones, space);
            return Some(bones);
        }

        let mut transforms: [vr::VRBoneTransform_t; Count as usize] = Default::default();

        let controller = devices.get_controller(hand)?;

        let grip: xr::Posef = controller
            .get_pose(
                self.openxr.display_time.get(),
                session_data,
                session_data.current_origin,
                None,
            )
            .unwrap_or_default()
            .mDeviceToAbsoluteTracking
            .into();
        let (raw, is_synthesised) = joint_space(session_data, controller, hand);
        let base: &xr::Space = match &raw {
            Some(raw) => raw,
            None if is_synthesised => session_data.tracking_space(),
            None => {
                drop(bone_cache);
                return Some(self.get_estimated_bones(session_data, space, hand, range));
            }
        };

        let Some(HandSkeleton { joints, .. }) = controller.get_hand_skeleton(&self.openxr, base)
        else {
            drop(bone_cache);
            return Some(self.get_estimated_bones(session_data, space, hand, range));
        };

        let mut joints: Box<[_]> = joints
            .into_iter()
            .map(|joint_location| {
                let xr::Posef {
                    position,
                    orientation,
                } = joint_location.pose;
                let mut position = Vec3::from_array([position.x, position.y, position.z]);
                if is_synthesised {
                    position -=
                        Vec3::from_array([grip.position.x, grip.position.y, grip.position.z]);
                }
                let orientation =
                    Quat::from_xyzw(orientation.x, orientation.y, orientation.z, orientation.w);

                Affine3A::from_rotation_translation(orientation, position)
            })
            .collect();

        let xr_joint_to_vr_bone = |joint: &Affine3A, bone: &mut vr::VRBoneTransform_t| {
            let (_, mut rot, mut pos) = joint.to_scale_rotation_translation();

            // The following transform converts our joints to the OpenVR coordinate system.
            // I have no idea what this transform is or how it works, but both Monado and ALVR
            // appear to have it, and it seems to work, so here it is.
            // https://github.com/alvr-org/ALVR/blob/cf52f875c2720b2c17ef490cfbec4c07ee5f41aa/alvr/server_openvr/src/tracking.rs#L82
            // https://gitlab.freedesktop.org/monado/monado/-/blob/d7089f182b0514e13554e99512d63e69c30523c5/src/xrt/state_trackers/steamvr_drv/ovrd_driver.cpp#L239
            std::mem::swap(&mut pos.x, &mut pos.z);
            pos.z = -pos.z;

            let r = &mut *rot;
            std::mem::swap(&mut r.x, &mut r.z);
            rot.z = -rot.z;

            if hand == Hand::Left {
                pos.x = -pos.x;
                pos.y = -pos.y;
                rot.x = -rot.x;
                rot.y = -rot.y;
            }

            *bone = vr::VRBoneTransform_t {
                position: pos.into(),
                orientation: rot.into(),
            };
        };

        // The wrists appear to have to some sort of strange orientation compared
        // to the other joints - this rotation fixes it up
        joints[xr::HandJoint::WRIST] *= Affine3A::from_quat(Quat::from_euler(
            glam::EulerRot::YZXEx,
            -FRAC_PI_2,
            FRAC_PI_2,
            0.0,
        ));

        // OpenXR reports all our bones in "model" space (basically), so we need to
        // convert everything into parent space.
        // For each finger, the metacarpal is a child of the wrist, and then each consecutive
        // joint in that finger is a parent->child relationship.
        // https://github.com/ValveSoftware/openvr/wiki/Hand-Skeleton#bone-structure
        let parent_id = RefCell::new(xr::HandJoint::WRIST);
        let mut parented_joints = joints.clone();
        let mut localize = |joint: xr::HandJoint| {
            let mut parent_id = parent_id.borrow_mut();
            parented_joints[joint] = joints[*parent_id].inverse() * parented_joints[joint];
            *parent_id = joint;
        };

        for joint_list in JOINTS_TO_BONES.iter().copied().skip(1) {
            for (joint, _) in joint_list.iter().copied() {
                localize(joint);
            }
            *parent_id.borrow_mut() = xr::HandJoint::WRIST;
        }

        joints = parented_joints;

        // The root bone is supposed to not transform
        // Changing the root bone appears to change the offset of the hand, but causes issues in
        // games such as The Lab, it also won't work in model space because the transform won't get
        // applied to the wrist in the conversion method.
        transforms[Root as usize] = Affine3A::IDENTITY.into();

        if is_synthesised {
            transforms[Wrist as usize] = match hand {
                Hand::Left => generated::left_hand::OPENHAND[Wrist as usize],
                Hand::Right => generated::right_hand::OPENHAND[Wrist as usize],
            };
        } else {
            // Currently as is, the hands will point down
            // This rotation corrects them so they are pointing the correct direction
            // Note that it is hand specific.
            joints[xr::HandJoint::WRIST] *= match hand {
                Hand::Left => {
                    Affine3A::from_quat(Quat::from_euler(glam::EulerRot::YZXEx, FRAC_PI_2, PI, 0.0))
                }
                Hand::Right => Affine3A::from_rotation_y(-FRAC_PI_2),
            };
            transforms[Wrist as usize] = joints[xr::HandJoint::WRIST].into();
        }

        for (joint, bone) in JOINTS_TO_BONES[1..]
            .iter()
            .flat_map(|list| list.iter())
            .copied()
        {
            xr_joint_to_vr_bone(&joints[joint], &mut transforms[bone as usize])
        }
        set_aux_bones(&mut transforms, hand);

        *bone_cache = Some(transforms);

        finalize_transforms(&mut transforms, space);
        Some(transforms)
    }

    /// A skeleton is active while its hand is present and its set was activated for that hand.
    pub(super) fn is_skeleton_active(
        &self,
        session_data: &SessionData,
        loaded: &ManifestLoadedActions,
        action: vr::VRActionHandle_t,
        hand: Hand,
    ) -> bool {
        let connected = session_data
            .input_data
            .devices
            .read()
            .unwrap()
            .get_controller(hand)
            .is_some_and(|controller| controller.is_connected(&self.openxr, session_data));
        if !connected {
            return false;
        }

        let Some(set) = loaded
            .skeleton_sets
            .get(ActionKey::from(KeyData::from_ffi(action)))
        else {
            return false;
        };

        let hand_path = self.get_subaction_path(hand);
        self.active_sets
            .read()
            .unwrap()
            .iter()
            .any(|(key, restriction)| {
                key == set && (*restriction == xr::Path::NULL || *restriction == hand_path)
            })
    }

    pub(super) fn skeletal_tracking_level(
        &self,
        session_data: &SessionData,
        hand: Hand,
    ) -> vr::EVRSkeletalTrackingLevel {
        let devices = session_data.input_data.devices.read().unwrap();
        let Some(controller) = devices.get_controller(hand) else {
            return vr::EVRSkeletalTrackingLevel::Estimated;
        };
        // SteamVR reports a fixed level for some controllers, e.g. Partial for the Index even
        // when the runtime doesn't track its fingers.
        if let Some(level) = controller
            .profile_data
            .as_ref()
            .filter(|_| controller.connected)
            .and_then(|p| p.skeletal_tracking_level)
        {
            return level;
        }

        let (raw, _) = joint_space(session_data, controller, hand);
        let base = raw.as_deref().unwrap_or(session_data.tracking_space());
        match controller.get_hand_skeleton(&self.openxr, base) {
            None => vr::EVRSkeletalTrackingLevel::Estimated,
            Some(HandSkeleton {
                data_source: xr::HandTrackingDataSourceEXT::UNOBSTRUCTED,
                ..
            }) => vr::EVRSkeletalTrackingLevel::Full,
            // Joints derived from a controller, or an unknown source
            Some(_) => vr::EVRSkeletalTrackingLevel::Partial,
        }
    }

    pub(super) fn get_bone_summary_from_hand_tracking(
        &self,
        session_data: &SessionData,
        summary_type: vr::EVRSummaryType,
        summary_data: &mut vr::VRSkeletalSummaryData_t,
        hand: Hand,
    ) {
        let devices = session_data.input_data.devices.read().unwrap();

        let Some(controller) = devices.get_controller(hand) else {
            self.get_estimated_bone_summary(session_data, summary_type, summary_data, hand);
            return;
        };

        // The curls only depend on the joints relative to each other, so the tracking space
        // works for a hand without a grip space.
        let (raw, _) = joint_space(session_data, controller, hand);
        let base = raw.as_deref().unwrap_or(session_data.tracking_space());
        let Some(HandSkeleton { joints, .. }) = controller.get_hand_skeleton(&self.openxr, base)
        else {
            self.get_estimated_bone_summary(session_data, summary_type, summary_data, hand);
            return;
        };

        let joints: Box<[_]> = joints
            .into_iter()
            .map(|joint_location| {
                let position = joint_location.pose.position;
                let orientation = joint_location.pose.orientation;
                (
                    Vec3::from_array([position.x, position.y, position.z]),
                    Quat::from_xyzw(orientation.x, orientation.y, orientation.z, orientation.w),
                )
            })
            .collect();

        // FIXME: calculate splay
        summary_data.flFingerSplay.fill(0.2);

        for (i, out_curl) in summary_data.flFingerCurl.iter_mut().enumerate() {
            if i == 0
                && controller
                    .profile_data
                    .as_ref()
                    .is_some_and(|p| p.force_estimated_thumb)
            {
                *out_curl = self.get_finger_state(session_data, hand).thumb;
                continue;
            }

            let (metacarpal, proximal, tip) = match i {
                0 => (
                    joints[xr::HandJoint::THUMB_METACARPAL],
                    joints[xr::HandJoint::THUMB_PROXIMAL],
                    joints[xr::HandJoint::THUMB_TIP],
                ),
                1 => (
                    joints[xr::HandJoint::INDEX_METACARPAL],
                    joints[xr::HandJoint::INDEX_PROXIMAL],
                    joints[xr::HandJoint::INDEX_TIP],
                ),
                2 => (
                    joints[xr::HandJoint::MIDDLE_METACARPAL],
                    joints[xr::HandJoint::MIDDLE_PROXIMAL],
                    joints[xr::HandJoint::MIDDLE_TIP],
                ),
                3 => (
                    joints[xr::HandJoint::RING_METACARPAL],
                    joints[xr::HandJoint::RING_PROXIMAL],
                    joints[xr::HandJoint::RING_TIP],
                ),
                4 => (
                    joints[xr::HandJoint::LITTLE_METACARPAL],
                    joints[xr::HandJoint::LITTLE_PROXIMAL],
                    joints[xr::HandJoint::LITTLE_TIP],
                ),
                _ => unreachable!(),
            };

            // Vector pointing from the knuckle to the metacarpal
            let proximal_metacarpal_delta = metacarpal.0 - proximal.0;
            // Vector pointing from the knuckle to the tip of the finger
            let tip_proximal_delta = tip.0 - proximal.0;

            // The dotproduct will give us the angle between the two vectors
            let dot = proximal_metacarpal_delta.dot(tip_proximal_delta);
            let a = proximal_metacarpal_delta.length();
            let b = tip_proximal_delta.length();

            let curl = if a == 0.0 || b == 0.0 {
                // If the joints converge, say the finger is fully curled
                1.0
            } else {
                // Isolate cos(angle)
                let ang_cos = (dot / (a * b)).clamp(-1.0, 1.0);
                // Convert the angle to radians
                let ang = ang_cos.acos();

                // When the finger is straight, the angle is 180deg (PI radians)
                // When the finger is fully curled, the angle is (theoretically) 0
                1.0 - (ang / PI)
            };

            // But in the real world, when a finger is curled, an angle of 0 is physically
            // not possible. The curl maxes out at around 0.8, give or take some depending on
            // the user's hands. Remap the value so >=0.8 (arbitrary value) means fully curled
            const MAX_CURL: f32 = 0.8;
            *out_curl = (curl / MAX_CURL).clamp(0.0, 1.0);
        }

        log::trace!(
            "skeletal summary for {hand:?} (hand tracking): curls {:?}",
            summary_data.flFingerCurl
        );
    }

    pub(super) fn get_estimated_bones(
        &self,
        session_data: &SessionData,
        space: vr::EVRSkeletalTransformSpace,
        hand: Hand,
        range: vr::EVRSkeletalMotionRange,
    ) -> [vr::VRBoneTransform_t; Count as usize] {
        let mut bone_cache = self.bone_cache(hand, range).lock().unwrap();
        if let Some(mut bones) = *bone_cache {
            finalize_transforms(&mut bones, space);
            return bones;
        }

        let mut transforms: [vr::VRBoneTransform_t; Count as usize] = Default::default();

        let finger_state = self.get_finger_state(session_data, hand);
        // With the controller, a closed hand wraps around it rather than making a fist.
        let with_controller = range == vr::EVRSkeletalMotionRange::WithController;
        let (open, closed) = match (hand, with_controller) {
            (Hand::Left, false) => (&generated::left_hand::OPENHAND, &generated::left_hand::FIST),
            (Hand::Left, true) => (
                &generated::left_hand::OPENHAND,
                &generated::left_hand::GRIPLIMIT,
            ),
            (Hand::Right, false) => (
                &generated::right_hand::OPENHAND,
                &generated::right_hand::FIST,
            ),
            (Hand::Right, true) => (
                &generated::right_hand::OPENHAND,
                &generated::right_hand::GRIPLIMIT,
            ),
        };

        // lerp to target pose
        for idx in HandSkeletonBone::Root as usize..HandSkeletonBone::Count as usize {
            let curl_state = finger_state
                .get_bone_state(unsafe { std::mem::transmute::<usize, HandSkeletonBone>(idx) });

            let (start_pos, start_rot) = bone_transform_to_glam(open[idx]);
            let (closed_pos, closed_rot) = bone_transform_to_glam(closed[idx]);

            transforms[idx] = vr::VRBoneTransform_t {
                position: start_pos.lerp(closed_pos, curl_state).into(),
                orientation: start_rot.slerp(closed_rot, curl_state).into(),
            };
        }

        *bone_cache = Some(transforms);

        finalize_transforms(&mut transforms, space);
        transforms
    }

    pub(super) fn get_estimated_bone_summary(
        &self,
        session_data: &SessionData,
        _: vr::EVRSummaryType,
        summary_data: &mut vr::VRSkeletalSummaryData_t,
        hand: Hand,
    ) {
        let state = self.get_finger_state(session_data, hand);

        *summary_data = vr::VRSkeletalSummaryData_t {
            flFingerSplay: [0.2; 4],
            flFingerCurl: [
                state.thumb,
                state.index,
                state.middle,
                state.ring,
                state.pinky,
            ],
        };

        log::trace!(
            "skeletal summary for {hand:?} (estimated): curls {:?}",
            summary_data.flFingerCurl
        );
    }

    fn get_finger_state(&self, session_data: &SessionData, hand: Hand) -> FingerState {
        // Determines the speed at which fingers follow the input states
        // This value seems to feel right for both analog inputs and binary ones (like vive wands)
        const FINGER_SMOOTHING_SPEED: f32 = 24.0;

        let actions = &session_data
            .input_data
            .estimated_skeleton_actions
            .get()
            .unwrap()
            .actions;

        let subaction = self.get_subaction_path(hand);

        let thumb_touch = actions
            .thumb_touch
            .state(&session_data.session, subaction)
            .unwrap_or_else(inactive_state)
            .current_state;
        let index_touch = actions
            .index_touch
            .state(&session_data.session, subaction)
            .unwrap_or_else(inactive_state)
            .current_state;
        let index_curl = actions
            .index_curl
            .state(&session_data.session, subaction)
            .unwrap_or_else(inactive_state)
            .current_state;
        let rest_curl = actions
            .rest_curl
            .state(&session_data.session, subaction)
            .unwrap_or_else(inactive_state)
            .current_state;

        let index = index_curl.max(
            // Curl the index finger slightly on touch input
            if index_touch || index_curl > 0.0 {
                0.3
            } else {
                0.0
            },
        );

        let mut state = self.estimated_finger_state[hand as usize - 1]
            .lock()
            .unwrap();

        let current_time = std::time::Instant::now();

        let target = FingerState {
            index,
            // Make other fingers curl with the index slightly to mimic how real human hands work
            middle: rest_curl.max(index / 2.0),
            ring: rest_curl.max(index / 4.0),
            pinky: rest_curl.max(index / 6.0),
            thumb: if thumb_touch { 1.0 } else { 0.0 },
            time: current_time,
        };

        let elapsed_time = current_time.duration_since(state.time).as_secs_f32();
        let t = (elapsed_time * FINGER_SMOOTHING_SPEED).min(1.0);

        *state = state.lerp(&target, t);

        *state
    }

    pub(super) fn get_reference_transforms(
        &self,
        hand: Hand,
        space: vr::EVRSkeletalTransformSpace,
        pose: vr::EVRSkeletalReferencePose,
        out_transforms: &mut [vr::VRBoneTransform_t; Count as usize],
    ) -> Result<(), vr::EVRInputError> {
        let mut skeleton = match hand {
            Hand::Left => match pose {
                vr::EVRSkeletalReferencePose::BindPose => generated::left_hand::BINDPOSE,
                vr::EVRSkeletalReferencePose::OpenHand => generated::left_hand::OPENHAND,
                vr::EVRSkeletalReferencePose::Fist => generated::left_hand::FIST,
                vr::EVRSkeletalReferencePose::GripLimit => generated::left_hand::GRIPLIMIT,
                _ => return Err(vr::EVRInputError::InvalidParam),
            },
            Hand::Right => match pose {
                vr::EVRSkeletalReferencePose::BindPose => generated::right_hand::BINDPOSE,
                vr::EVRSkeletalReferencePose::OpenHand => generated::right_hand::OPENHAND,
                vr::EVRSkeletalReferencePose::Fist => generated::right_hand::FIST,
                vr::EVRSkeletalReferencePose::GripLimit => generated::right_hand::GRIPLIMIT,
                _ => return Err(vr::EVRInputError::InvalidParam),
            },
        };

        finalize_transforms(&mut skeleton, space);
        *out_transforms = skeleton;
        Ok(())
    }
}

/// The space a controller's hand joints are located in: its raw grip space, or the tracking space
/// (`None`) if it has none or its pose is synthesised from the joints (the returned flag). Bone
/// data, the summary and the tracking level all use it, so a hand is located once per frame.
fn joint_space<'a>(
    session_data: &'a SessionData,
    controller: &TrackedDevice,
    hand: Hand,
) -> (Option<SpaceReadGuard<'a>>, bool) {
    let TrackedDeviceType::Controller {
        pose_is_synthesised,
        ..
    } = controller.get_type()
    else {
        return (None, false);
    };
    if pose_is_synthesised.load(Ordering::Relaxed) {
        return (None, true);
    }
    let pose_data = &session_data.input_data.pose_data;
    let raw = match hand {
        Hand::Left => &pose_data.left_space,
        Hand::Right => &pose_data.right_space,
    }
    .try_get_or_init_raw(&controller.profile_data, session_data, pose_data);
    (raw, false)
}

/// trait alias
trait PoseIterator: Iterator<Item = (Vec3, Quat)> {}
impl<T: Iterator<Item = (Vec3, Quat)>> PoseIterator for T {}

fn parent_to_model_space_bone_data(it: impl PoseIterator) -> impl PoseIterator {
    struct State {
        /// Index for current finger in JOINTS_TO_BONES
        finger_slice_idx: usize,
        wrist_transform: Affine3A,
        /// None for metacarpal joints, in which case we use the wrist transform
        parent_transform: Option<Affine3A>,
    }

    it.enumerate().scan(
        State {
            finger_slice_idx: 1,
            wrist_transform: Affine3A::ZERO,
            parent_transform: None,
        },
        |state, (idx, (pos, rot))| {
            if idx == Wrist as usize {
                state.wrist_transform = Affine3A::from_rotation_translation(rot, pos);
            }
            if idx <= Wrist as usize || idx >= AuxThumb as usize {
                return Some((pos, rot));
            }

            let mut mat = Affine3A::from_rotation_translation(rot, pos);
            if let Some(parent) = state.parent_transform.take() {
                mat = parent * mat;
            } else {
                mat = state.wrist_transform * mat;
            }

            if idx == JOINTS_TO_BONES[state.finger_slice_idx].last().unwrap().1 as usize {
                state.finger_slice_idx += 1;
                state.parent_transform = None;
            } else {
                state.parent_transform = Some(mat);
            }

            let (_, rot, pos) = mat.to_scale_rotation_translation();
            Some((pos, rot))
        },
    )
}

fn bone_transform_to_glam(transform: vr::VRBoneTransform_t) -> (Vec3, Quat) {
    let rot = transform.orientation;
    (
        Vec3::from_slice(&transform.position.v[..3]),
        Quat::from_xyzw(rot.x, rot.y, rot.z, rot.w),
    )
}

fn finalize_transforms(
    transforms: &mut [vr::VRBoneTransform_t],
    space: vr::EVRSkeletalTransformSpace,
) {
    // Convert back to model space if needed
    // it is unnecessary to convert back and forth, but it works and it's easy
    if space == vr::EVRSkeletalTransformSpace::Model {
        let bone_data: Vec<(Vec3, Quat)> =
            parent_to_model_space_bone_data(transforms.iter().map(|t| bone_transform_to_glam(*t)))
                .collect();

        for (transform, (pos, rot)) in transforms.iter_mut().zip(bone_data) {
            transform.position = pos.into();
            transform.orientation = rot.into();
        }
    }
}

macro_rules! joints_for_finger {
    ($xr_finger:ident, $vr_finger:ident) => {
        paste! {[
            (xr::HandJoint::[<$xr_finger _METACARPAL>], [<$vr_finger Finger0>]),
            (xr::HandJoint::[<$xr_finger _PROXIMAL>], [<$vr_finger Finger1>]),
            (xr::HandJoint::[<$xr_finger _INTERMEDIATE>], [<$vr_finger Finger2>]),
            (xr::HandJoint::[<$xr_finger _DISTAL>], [<$vr_finger Finger3>]),
            (xr::HandJoint::[<$xr_finger _TIP>], [<$vr_finger Finger4>])
        ].as_slice()}
    };
}

static JOINTS_TO_BONES: &[&[(xr::HandJoint, HandSkeletonBone)]] = &[
    [(xr::HandJoint::WRIST, Wrist)].as_slice(),
    &[
        (xr::HandJoint::THUMB_METACARPAL, Thumb0),
        (xr::HandJoint::THUMB_PROXIMAL, Thumb1),
        (xr::HandJoint::THUMB_DISTAL, Thumb2),
        (xr::HandJoint::THUMB_TIP, Thumb3),
    ],
    joints_for_finger!(INDEX, Index),
    joints_for_finger!(MIDDLE, Middle),
    joints_for_finger!(RING, Ring),
    joints_for_finger!(LITTLE, Pinky),
];

/// Each aux bone and the last knuckle of its finger.
static AUX_BONES: &[(HandSkeletonBone, HandSkeletonBone)] = &[
    (AuxThumb, Thumb2),
    (AuxIndexFinger, IndexFinger3),
    (AuxMiddleFinger, MiddleFinger3),
    (AuxRingFinger, RingFinger3),
    (AuxPinkyFinger, PinkyFinger3),
];

/// Aux bones are children of the root that share the model space pose of their finger's last
/// knuckle, so they have to be derived from the finished finger chain.
fn set_aux_bones(transforms: &mut [vr::VRBoneTransform_t; Count as usize], hand: Hand) {
    let model_space: Vec<_> =
        parent_to_model_space_bone_data(transforms.iter().map(|t| bone_transform_to_glam(*t)))
            .collect();
    // SteamVR's left hand aux bones are turned around the knuckle's X axis.
    let flip = match hand {
        Hand::Left => Quat::from_rotation_x(PI),
        Hand::Right => Quat::IDENTITY,
    };
    for (aux, knuckle) in AUX_BONES.iter().copied() {
        let (pos, rot) = model_space[knuckle as usize];
        transforms[aux as usize] = vr::VRBoneTransform_t {
            position: pos.into(),
            orientation: (rot * flip).into(),
        };
    }
}

/// SteamVR's bone names, with `#` standing for the hand ("l" or "r").
static BONE_NAMES: [&str; Count as usize] = [
    "Root",
    "wrist_#",
    "finger_thumb_0_#",
    "finger_thumb_1_#",
    "finger_thumb_2_#",
    "finger_thumb_#_end",
    "finger_index_meta_#",
    "finger_index_0_#",
    "finger_index_1_#",
    "finger_index_2_#",
    "finger_index_#_end",
    "finger_middle_meta_#",
    "finger_middle_0_#",
    "finger_middle_1_#",
    "finger_middle_2_#",
    "finger_middle_#_end",
    "finger_ring_meta_#",
    "finger_ring_0_#",
    "finger_ring_1_#",
    "finger_ring_2_#",
    "finger_ring_#_end",
    "finger_pinky_meta_#",
    "finger_pinky_0_#",
    "finger_pinky_1_#",
    "finger_pinky_2_#",
    "finger_pinky_#_end",
    "finger_thumb_#_aux",
    "finger_index_#_aux",
    "finger_middle_#_aux",
    "finger_ring_#_aux",
    "finger_pinky_#_aux",
];

pub(super) fn bone_name(bone: vr::BoneIndex_t, hand: Hand) -> Option<String> {
    let side = match hand {
        Hand::Left => "l",
        Hand::Right => "r",
    };
    let name = BONE_NAMES.get(usize::try_from(bone).ok()?)?;
    Some(name.replace('#', side))
}

/// The parent of each bone, -1 for the root. Each finger hangs off the wrist, and the aux bones
/// off the root.
pub(super) static BONE_PARENTS: [vr::BoneIndex_t; Count as usize] = [
    -1, 0, 1, 2, 3, 4, 1, 6, 7, 8, 9, 1, 11, 12, 13, 14, 1, 16, 17, 18, 19, 1, 21, 22, 23, 24, 0,
    0, 0, 0, 0,
];

#[derive(Copy, Clone)]
pub(super) struct FingerState {
    index: f32,
    middle: f32,
    ring: f32,
    pinky: f32,
    thumb: f32,
    time: Instant,
}

impl FingerState {
    pub fn new() -> FingerState {
        FingerState {
            index: 0.0,
            middle: 0.0,
            ring: 0.0,
            pinky: 0.0,
            thumb: 0.0,
            time: Instant::now(),
        }
    }

    fn lerp(&self, target: &Self, amount: f32) -> Self {
        Self {
            index: self.index + (target.index - self.index) * amount,
            middle: self.middle + (target.middle - self.middle) * amount,
            ring: self.ring + (target.ring - self.ring) * amount,
            pinky: self.pinky + (target.pinky - self.pinky) * amount,
            thumb: self.thumb + (target.thumb - self.thumb) * amount,
            time: target.time,
        }
    }

    fn get_bone_state(&self, bone: HandSkeletonBone) -> f32 {
        match bone {
            HandSkeletonBone::IndexFinger0
            | HandSkeletonBone::IndexFinger1
            | HandSkeletonBone::IndexFinger2
            | HandSkeletonBone::IndexFinger3
            | HandSkeletonBone::IndexFinger4
            | HandSkeletonBone::AuxIndexFinger => self.index,
            HandSkeletonBone::MiddleFinger0
            | HandSkeletonBone::MiddleFinger1
            | HandSkeletonBone::MiddleFinger2
            | HandSkeletonBone::MiddleFinger3
            | HandSkeletonBone::MiddleFinger4
            | HandSkeletonBone::AuxMiddleFinger => self.middle,
            HandSkeletonBone::RingFinger0
            | HandSkeletonBone::RingFinger1
            | HandSkeletonBone::RingFinger2
            | HandSkeletonBone::RingFinger3
            | HandSkeletonBone::RingFinger4
            | HandSkeletonBone::AuxRingFinger => self.ring,
            HandSkeletonBone::PinkyFinger0
            | HandSkeletonBone::PinkyFinger1
            | HandSkeletonBone::PinkyFinger2
            | HandSkeletonBone::PinkyFinger3
            | HandSkeletonBone::PinkyFinger4
            | HandSkeletonBone::AuxPinkyFinger => self.pinky,
            HandSkeletonBone::Thumb0
            | HandSkeletonBone::Thumb1
            | HandSkeletonBone::Thumb2
            | HandSkeletonBone::Thumb3
            | HandSkeletonBone::AuxThumb => self.thumb,
            _ => 0.0,
        }
    }
}

#[repr(usize)]
#[derive(Copy, Clone)]
pub(super) enum HandSkeletonBone {
    Root = 0,
    Wrist,
    Thumb0,
    Thumb1,
    Thumb2,
    Thumb3,
    IndexFinger0,
    IndexFinger1,
    IndexFinger2,
    IndexFinger3,
    IndexFinger4,
    MiddleFinger0,
    MiddleFinger1,
    MiddleFinger2,
    MiddleFinger3,
    MiddleFinger4,
    RingFinger0,
    RingFinger1,
    RingFinger2,
    RingFinger3,
    RingFinger4,
    PinkyFinger0,
    PinkyFinger1,
    PinkyFinger2,
    PinkyFinger3,
    PinkyFinger4,
    AuxThumb,
    AuxIndexFinger,
    AuxMiddleFinger,
    AuxRingFinger,
    AuxPinkyFinger,
    Count,
}

macro_rules! skeletal_input_actions {
    ($($field:ident: $ty:ty),+$(,)?) => {
        pub struct SkeletalInputActions {
            $(pub $field: xr::Action<$ty>),+
        }
        pub struct SkeletalInputBindings {
            $(pub $field: Vec<xr::Path>),+
        }
        impl SkeletalInputBindings {
            pub fn binding_iter(self, actions: &SkeletalInputActions) -> impl Iterator<Item = xr::Binding<'_>> {
                std::iter::empty()
                $(
                    .chain(
                        self.$field.into_iter().map(|binding| xr::Binding::new(&actions.$field, binding))
                    )
                )+
            }
        }
    }
}

skeletal_input_actions! {
    thumb_touch: bool,
    index_touch: bool,
    index_curl: f32,
    rest_curl: f32,
}

pub struct SkeletalInputActionData {
    pub set: xr::ActionSet,
    pub actions: SkeletalInputActions,
}

impl SkeletalInputActionData {
    pub fn new(instance: &xr::Instance, left_hand: xr::Path, right_hand: xr::Path) -> Self {
        debug!("creating skeletal input actions");
        let leftright = [left_hand, right_hand];
        let set = instance
            .create_action_set("xrizer-skeletal-input", "XRizer Skeletal Input", 0)
            .unwrap();
        let thumb_touch = set
            .create_action("thumb-touch", "Thumb Touch", &leftright)
            .unwrap();
        let index_touch = set
            .create_action("index-touch", "Index Touch", &leftright)
            .unwrap();
        let index_curl = set
            .create_action("index-curl", "Index Curl", &leftright)
            .unwrap();
        let rest_curl = set
            .create_action("rest-curl", "Rest Curl", &leftright)
            .unwrap();

        Self {
            set,
            actions: SkeletalInputActions {
                thumb_touch,
                index_touch,
                index_curl,
                rest_curl,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Count, bone_transform_to_glam, finalize_transforms, generated};
    use crate::input::tests::Fixture;
    use crate::vr::{self, IVRInput010_Interface};
    use glam::Affine3A;

    const SENTINEL: vr::VRBoneTransform_t = vr::VRBoneTransform_t {
        position: vr::HmdVector4_t {
            v: [7.0, 7.0, 7.0, 7.0],
        },
        orientation: vr::HmdQuaternionf_t {
            w: 7.0,
            x: 7.0,
            y: 7.0,
            z: 7.0,
        },
    };

    fn is_sentinel(bone: &vr::VRBoneTransform_t) -> bool {
        bone.position.v == SENTINEL.position.v && bone.orientation.w == SENTINEL.orientation.w
    }

    #[test]
    fn bone_arrays_larger_than_the_skeleton() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
        f.input.openxr.poll_events();
        f.input.frame_start_update();

        let mut bones = [SENTINEL; Count as usize + 6];
        assert_eq!(
            f.input.GetSkeletalBoneData(
                skeleton,
                vr::EVRSkeletalTransformSpace::Parent,
                vr::EVRSkeletalMotionRange::WithoutController,
                bones.as_mut_ptr(),
                bones.len() as u32,
            ),
            vr::EVRInputError::None
        );
        assert!(!bones[..Count as usize].iter().any(is_sentinel));
        assert!(bones[Count as usize..].iter().all(is_sentinel));

        let mut bones = [SENTINEL; Count as usize + 6];
        assert_eq!(
            f.input.GetSkeletalReferenceTransforms(
                skeleton,
                vr::EVRSkeletalTransformSpace::Parent,
                vr::EVRSkeletalReferencePose::Fist,
                bones.as_mut_ptr(),
                bones.len() as u32,
            ),
            vr::EVRInputError::None
        );
        for (bone, expected) in bones.iter().zip(&generated::left_hand::FIST) {
            assert_eq!(bone.position.v, expected.position.v);
        }
        assert!(bones[Count as usize..].iter().all(is_sentinel));
    }

    #[test]
    fn bone_arrays_too_small_or_null() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");

        let mut bones = [SENTINEL; Count as usize - 1];
        for (array, count, error) in [
            (
                bones.as_mut_ptr(),
                bones.len() as u32,
                vr::EVRInputError::BufferTooSmall,
            ),
            (
                std::ptr::null_mut(),
                Count as u32,
                vr::EVRInputError::InvalidParam,
            ),
        ] {
            assert_eq!(
                f.input.GetSkeletalBoneData(
                    skeleton,
                    vr::EVRSkeletalTransformSpace::Parent,
                    vr::EVRSkeletalMotionRange::WithoutController,
                    array,
                    count,
                ),
                error
            );
            assert_eq!(
                f.input.GetSkeletalReferenceTransforms(
                    skeleton,
                    vr::EVRSkeletalTransformSpace::Parent,
                    vr::EVRSkeletalReferencePose::Fist,
                    array,
                    count,
                ),
                error
            );
        }
        assert!(bones.iter().all(is_sentinel));
    }

    #[test]
    fn bone_hierarchy() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        let skeleton = f.get_action_handle(c"/actions/set1/in/skellyr");

        let mut parents = [i32::MAX; Count as usize + 1];
        assert_eq!(
            f.input
                .GetBoneHierarchy(skeleton, parents.as_mut_ptr(), Count as u32 - 1),
            vr::EVRInputError::BufferTooSmall
        );
        assert_eq!(
            f.input
                .GetBoneHierarchy(skeleton, parents.as_mut_ptr(), parents.len() as u32),
            vr::EVRInputError::None
        );
        assert_eq!(parents[Count as usize], i32::MAX);

        // Walking the reported parents must give the same model space pose as our own conversion.
        let parent_space = generated::right_hand::FIST;
        let mut model_space = parent_space;
        finalize_transforms(&mut model_space, vr::EVRSkeletalTransformSpace::Model);
        let to_affine = |bone| {
            let (pos, rot) = bone_transform_to_glam(bone);
            Affine3A::from_rotation_translation(rot, pos)
        };
        let mut walked = Vec::<Affine3A>::new();
        for (bone, parent) in parent_space.iter().zip(&parents[..Count as usize]) {
            let parent = usize::try_from(*parent).map_or(Affine3A::IDENTITY, |p| walked[p]);
            walked.push(parent * to_affine(*bone));
        }
        for (i, (walked, expected)) in walked.iter().zip(model_space).enumerate() {
            assert!(walked.abs_diff_eq(to_affine(expected), 1e-4), "bone {i}");
        }
    }

    #[test]
    fn bone_names() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        let left = f.get_action_handle(c"/actions/set1/in/skellyl");
        let right = f.get_action_handle(c"/actions/set1/in/skellyr");

        let name = |action, bone, size: usize| {
            let mut buf = vec![0x7f as std::ffi::c_char; size];
            let err = f
                .input
                .GetBoneName(action, bone, buf.as_mut_ptr(), size as u32);
            (err == vr::EVRInputError::None)
                .then(|| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_owned())
                .ok_or(err)
        };

        assert_eq!(name(right, 0, 32), Ok(c"Root".into()));
        assert_eq!(name(right, 1, 32), Ok(c"wrist_r".into()));
        assert_eq!(name(left, 5, 32), Ok(c"finger_thumb_l_end".into()));
        assert_eq!(name(left, 6, 32), Ok(c"finger_index_meta_l".into()));
        assert_eq!(name(right, 30, 32), Ok(c"finger_pinky_r_aux".into()));
        assert_eq!(name(right, 1, 8), Ok(c"wrist_r".into()));
        assert_eq!(name(right, 1, 7), Err(vr::EVRInputError::BufferTooSmall));
        assert_eq!(
            name(right, 31, 32),
            Err(vr::EVRInputError::InvalidBoneIndex)
        );
        assert_eq!(
            name(right, -1, 32),
            Err(vr::EVRInputError::InvalidBoneIndex)
        );
    }

    #[test]
    fn index_tracking_level_is_partial() {
        use crate::input::profiles::{knuckles::Knuckles, oculus_touch::OculusTouch};
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
        let set = f.get_action_set_handle(c"/actions/set1");
        let sync = |f: &mut Fixture| {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set,
                ..Default::default()
            })
        };
        // Queried before any bone data, which is when games pick their hand mode.
        let level = |f: &Fixture| {
            let mut level = vr::EVRSkeletalTrackingLevel::Full;
            assert_eq!(
                f.input.GetSkeletalTrackingLevel(skeleton, &mut level),
                vr::EVRInputError::None
            );
            level
        };

        assert_eq!(level(&f), vr::EVRSkeletalTrackingLevel::Estimated);
        f.set_interaction_profile::<Knuckles>(fakexr::UserPath::LeftHand);
        sync(&mut f);
        assert_eq!(level(&f), vr::EVRSkeletalTrackingLevel::Partial);
        // Turned off
        fakexr::set_interaction_profile(
            f.raw_session(),
            fakexr::UserPath::LeftHand,
            openxr::Path::NULL,
        );
        sync(&mut f);
        f.input.openxr.poll_events();
        assert_eq!(level(&f), vr::EVRSkeletalTrackingLevel::Estimated);
        f.set_interaction_profile::<OculusTouch>(fakexr::UserPath::LeftHand);
        sync(&mut f);
        assert_eq!(level(&f), vr::EVRSkeletalTrackingLevel::Estimated);
    }

    #[test]
    fn aux_bones_follow_the_last_knuckles() {
        use super::{AUX_BONES, set_aux_bones};
        use crate::openxr_data::Hand;
        // SteamVR's own open hand. (Its fist moves the thumb but not the thumb's aux bone.)
        for (hand, reference) in [
            (Hand::Left, &generated::left_hand::OPENHAND),
            (Hand::Right, &generated::right_hand::OPENHAND),
        ] {
            let mut bones = *reference;
            set_aux_bones(&mut bones, hand);
            for (aux, _) in AUX_BONES {
                let (pos, rot) = bone_transform_to_glam(bones[*aux as usize]);
                let (expected_pos, expected_rot) = bone_transform_to_glam(reference[*aux as usize]);
                assert!(
                    pos.distance(expected_pos) < 0.0015,
                    "{hand:?} bone {}: {pos} != {expected_pos}",
                    *aux as usize
                );
                assert!(
                    rot.dot(expected_rot).abs() > 0.999,
                    "{hand:?} bone {}: {rot} != {expected_rot}",
                    *aux as usize
                );
            }
        }
    }

    #[test]
    fn skeleton_active_only_for_present_hands_and_active_sets() {
        use crate::input::profiles::knuckles::Knuckles;
        use fakexr::UserPath::*;
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
        let set = f.get_action_set_handle(c"/actions/set1");
        let left = f.get_input_source_handle(c"/user/hand/left");
        let right = f.get_input_source_handle(c"/user/hand/right");
        let data = |f: &Fixture| {
            let mut data = vr::InputSkeletalActionData_t::default();
            assert_eq!(
                f.input.GetSkeletalActionData(
                    skeleton,
                    &mut data,
                    std::mem::size_of_val(&data) as u32
                ),
                vr::EVRInputError::None
            );
            (data.bActive, data.activeOrigin)
        };
        let sync = |f: &mut Fixture, restrict| {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set,
                ulRestrictedToDevice: restrict,
                ..Default::default()
            })
        };

        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);
        // Before the first UpdateActionState
        assert_eq!(data(&f), (false, vr::k_ulInvalidInputValueHandle));
        sync(&mut f, vr::k_ulInvalidInputValueHandle);
        assert_eq!(data(&f), (true, left));
        sync(&mut f, right);
        assert_eq!(data(&f), (false, vr::k_ulInvalidInputValueHandle));
        sync(&mut f, left);
        assert_eq!(data(&f), (true, left));
        // The secondary set of a set restricted to the other hand
        let set2 = f.get_action_set_handle(c"/actions/set2");
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set2,
            ulRestrictedToDevice: right,
            ulSecondaryActionSet: set,
            ..Default::default()
        });
        assert_eq!(data(&f), (true, left));
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set2,
            ..Default::default()
        });
        assert_eq!(data(&f), (false, vr::k_ulInvalidInputValueHandle));
        sync(&mut f, left);

        // The controller turns off.
        fakexr::set_interaction_profile(f.raw_session(), LeftHand, openxr::Path::NULL);
        sync(&mut f, left);
        f.input.openxr.poll_events();
        assert_eq!(data(&f), (false, vr::k_ulInvalidInputValueHandle));
    }

    #[test]
    fn estimated_bones_respect_motion_range() {
        use super::HandSkeletonBone::IndexFinger1;
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
        let set = f.get_action_set_handle(c"/actions/set1");

        let data = f.input.openxr.session_data.get();
        let actions = &data
            .input_data
            .estimated_skeleton_actions
            .get()
            .unwrap()
            .actions;
        for action in [&actions.index_curl, &actions.rest_curl] {
            fakexr::set_action_state(
                action.as_raw(),
                fakexr::ActionState::Float(1.0),
                fakexr::UserPath::LeftHand,
            );
        }
        drop(data);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set,
            ..Default::default()
        });
        // Let the finger smoothing reach the target.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let bones = |range| {
            let mut bones = [vr::VRBoneTransform_t::default(); Count as usize];
            assert_eq!(
                f.input.GetSkeletalBoneData(
                    skeleton,
                    vr::EVRSkeletalTransformSpace::Parent,
                    range,
                    bones.as_mut_ptr(),
                    bones.len() as u32,
                ),
                vr::EVRInputError::None
            );
            bone_transform_to_glam(bones[IndexFinger1 as usize]).1
        };
        let reference = |pose: &[vr::VRBoneTransform_t; Count as usize]| {
            bone_transform_to_glam(pose[IndexFinger1 as usize]).1
        };

        // Both ranges in the same frame
        let without = bones(vr::EVRSkeletalMotionRange::WithoutController);
        let with = bones(vr::EVRSkeletalMotionRange::WithController);
        assert!(without.abs_diff_eq(reference(&generated::left_hand::FIST), 1e-4));
        assert!(with.abs_diff_eq(reference(&generated::left_hand::GRIPLIMIT), 1e-4));
    }
}
