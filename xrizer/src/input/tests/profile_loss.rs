//! Controllers whose interaction profile goes away for a moment, like WiVRn reports when the
//! headset has none (WiVRn/WiVRn#1100), and hands tracked without any profile (WiVRn/WiVRn#1134).

use super::Fixture;
use crate::input::{
    InteractionProfile,
    profiles::{knuckles::Knuckles, oculus_touch::OculusTouch},
};
use crate::openxr_data::Hand;
use fakexr::UserPath::*;
use openvr::{self as vr, IVRInput010_Interface};
use openxr as xr;
use vr::ETrackedDeviceProperty as Prop;
use vr::EVREventType as Ev;

struct Blip {
    f: Fixture,
    set: vr::VRActionSetHandle_t,
}

impl Blip {
    fn new() -> Self {
        fakexr::set_actions_need_profile(true);
        let f = Fixture::new();
        f.load_actions(c"actions_profile_loss.json");
        let set = f.get_action_set_handle(c"/actions/main");
        Self { f, set }
    }

    /// An UpdateActionState and the frame update after it.
    fn sync(&mut self) {
        self.f.sync(vr::VRActiveActionSet_t {
            ulActionSet: self.set,
            ..Default::default()
        });
        self.f.input.frame_start_update();
    }

    fn bind<P: InteractionProfile>(&mut self) {
        for hand in [LeftHand, RightHand] {
            self.f.set_interaction_profile::<P>(hand);
        }
    }

    fn unbind(&mut self) {
        for hand in [LeftHand, RightHand] {
            fakexr::set_interaction_profile(self.f.raw_session(), hand, xr::Path::NULL);
        }
        self.f.pending_profile_change = true;
    }

    fn action(&self, name: &std::ffi::CStr) -> vr::VRActionHandle_t {
        self.f.get_action_handle(name)
    }

    /// (active, state, changed)
    #[track_caller]
    fn digital(&self, action: vr::VRActionHandle_t) -> (bool, bool, bool) {
        let state = self.f.get_bool_state(action).unwrap();
        (state.bActive, state.bState, state.bChanged)
    }

    /// The OpenXR actions read by `action`'s custom bindings with `P`.
    fn custom_inputs<P: InteractionProfile>(
        &self,
        action: vr::VRActionHandle_t,
    ) -> Vec<xr::sys::Action> {
        let profile = self
            .f
            .input
            .openxr
            .instance
            .string_to_path(P::profile_path())
            .unwrap();
        let data = self.f.input.openxr.session_data.get();
        let loaded = data.input_data.get_loaded_actions().unwrap();
        loaded
            .try_get_bindings(action, profile)
            .unwrap()
            .iter()
            .flat_map(|binding| binding.ty.actions())
            .collect()
    }

    fn events(&self) -> Vec<(Ev, u32)> {
        std::iter::from_fn(|| {
            let mut event = vr::VREvent_t::default();
            let got = self
                .f
                .input
                .get_next_event(std::mem::size_of_val(&event) as u32, &mut event);
            got.then_some((Ev(event.eventType), event.trackedDeviceIndex))
        })
        .collect()
    }

    fn controller_type(&self, index: vr::TrackedDeviceIndex_t) -> Option<String> {
        self.f
            .input
            .get_device_string_tracked_property(index, Prop::ControllerType_String)
            .map(|s| s.into_string().unwrap())
    }

    /// Checks that the controllers kept their indices, class and role, and identify as
    /// `controller_type`.
    #[track_caller]
    fn assert_controllers(&self, controller_type: &str) {
        let input = &self.f.input;
        for (hand, index) in [(Hand::Left, 1), (Hand::Right, 2)] {
            assert_eq!(input.get_controller_device_index(hand), Some(index));
            assert_eq!(input.device_index_to_hand(index), Some(hand));
            assert_eq!(
                input.device_index_to_tracked_device_class(index),
                Some(vr::ETrackedDeviceClass::Controller)
            );
            assert_eq!(
                input.get_device_int_tracked_property(index, Prop::ControllerRoleHint_Int32),
                Some(vr::ETrackedControllerRole::from(hand).0 as i32)
            );
            assert_eq!(
                self.controller_type(index).as_deref(),
                Some(controller_type),
                "{hand:?}"
            );
        }
    }

    fn connected(&self, index: vr::TrackedDeviceIndex_t) -> bool {
        self.f.input.is_device_connected(index)
    }

    fn left_pose(&self, nanos: i64) -> vr::TrackedDevicePose_t {
        self.f
            .input
            .get_controller_pose(
                Hand::Left,
                Some(vr::ETrackingUniverseOrigin::Seated),
                xr::Time::from_nanos(nanos),
                None,
            )
            .unwrap()
    }
}

#[test]
fn profile_blip_keeps_controllers() {
    let mut b = Blip::new();
    let button = b.action(c"/actions/main/in/button");
    let button_input = b.f.get_action::<bool>(button);
    let pose = b.action(c"/actions/main/in/pose");
    let left = b.f.get_input_source_handle(c"/user/hand/left");
    fakexr::set_grip(
        b.f.raw_session(),
        LeftHand,
        xr::Posef {
            position: xr::Vector3f {
                x: 0.1,
                y: 1.2,
                z: -0.3,
            },
            ..xr::Posef::IDENTITY
        },
    );

    b.bind::<Knuckles>();
    fakexr::set_action_state(button_input, true.into(), LeftHand);
    b.sync();
    assert_eq!(b.digital(button), (true, true, true));
    let tracked = b.left_pose(1);
    assert!(tracked.bPoseIsValid);
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceActivated, 0),
            (Ev::TrackedDeviceActivated, 1),
            (Ev::TrackedDeviceActivated, 2),
        ]
    );
    b.assert_controllers("knuckles");

    // The actions are inactive meanwhile, but the controllers are still there, out of range
    // where they were last seen.
    b.unbind();
    b.sync();
    assert_eq!(b.digital(button), (false, false, false));
    assert_eq!(b.events(), []);
    assert!(b.connected(1) && b.connected(2));
    b.assert_controllers("knuckles");
    let untracked = b.left_pose(2);
    assert!(untracked.bDeviceIsConnected && !untracked.bPoseIsValid);
    assert_eq!(
        untracked.eTrackingResult,
        vr::ETrackingResult::Running_OutOfRange
    );
    assert_eq!(
        untracked.mDeviceToAbsoluteTracking.m,
        tracked.mDeviceToAbsoluteTracking.m
    );
    let action_pose = b.f.get_pose(pose, left).unwrap();
    assert!(action_pose.bActive && !action_pose.pose.bPoseIsValid);

    b.bind::<Knuckles>();
    b.sync();
    assert!(b.digital(button).0);
    assert!(b.digital(button).1);
    assert_eq!(b.events(), []);
    b.assert_controllers("knuckles");
    assert!(b.left_pose(3).bPoseIsValid);
    fakexr::set_action_state(button_input, false.into(), LeftHand);
    b.sync();
    assert_eq!(b.digital(button), (true, false, true));
}

#[test]
fn profile_loss_disconnects_after_grace() {
    let mut b = Blip::new();
    let button = b.action(c"/actions/main/in/button");
    let button_input = b.f.get_action::<bool>(button);
    b.bind::<Knuckles>();
    b.sync();
    b.events();

    b.unbind();
    b.sync();
    assert_eq!(b.events(), []);
    b.f.input.expire_connection_grace();
    assert!(!b.connected(1) && !b.connected(2));
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceDeactivated, 1),
            (Ev::TrackedDeviceDeactivated, 2),
        ]
    );
    // A disconnected device keeps its index and identity, like in SteamVR.
    b.assert_controllers("knuckles");
    let pose =
        b.f.input
            .get_controller_pose(Hand::Left, None, xr::Time::from_nanos(1), None)
            .unwrap();
    assert!(!pose.bDeviceIsConnected && !pose.bPoseIsValid);

    // It stays disconnected until the profile is back.
    b.sync();
    assert_eq!(b.events(), []);

    b.bind::<Knuckles>();
    fakexr::set_action_state(button_input, true.into(), LeftHand);
    b.sync();
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceActivated, 1),
            (Ev::TrackedDeviceActivated, 2),
        ]
    );
    assert!(b.connected(1) && b.connected(2));
    assert_eq!(b.digital(button), (true, true, true));
}

#[test]
fn profile_blip_with_another_controller_replaces_the_device() {
    let mut b = Blip::new();
    let button = b.action(c"/actions/main/in/button");
    b.bind::<Knuckles>();
    b.sync();
    b.events();

    b.unbind();
    b.sync();
    b.bind::<OculusTouch>();
    b.sync();
    // The app sees the Index controllers go and the Touch controllers come, at the same indices.
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceDeactivated, 1),
            (Ev::TrackedDeviceActivated, 1),
            (Ev::TrackedDeviceDeactivated, 2),
            (Ev::TrackedDeviceActivated, 2),
        ]
    );
    b.assert_controllers("oculus_touch");

    let touch_button = b.f.get_action::<bool>(button);
    fakexr::set_action_state(touch_button, true.into(), LeftHand);
    b.sync();
    assert_eq!(b.digital(button), (true, true, true));
}

#[test]
fn direct_profile_change_replaces_the_device() {
    let mut b = Blip::new();
    b.bind::<Knuckles>();
    b.sync();
    b.events();

    b.f.set_interaction_profile::<OculusTouch>(LeftHand);
    b.sync();
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceDeactivated, 1),
            (Ev::TrackedDeviceActivated, 1),
        ]
    );
    assert_eq!(b.controller_type(1).as_deref(), Some("oculus_touch"));
    assert_eq!(b.controller_type(2).as_deref(), Some("knuckles"));
}

#[test]
fn input_focus_holds_off_disconnects() {
    let mut b = Blip::new();
    b.bind::<Knuckles>();
    b.sync();
    b.events();
    let session = b.f.raw_session();
    let set_state = |b: &Blip, state| {
        fakexr::set_session_state(session, state);
        b.f.input.openxr.poll_events();
    };
    for state in [
        xr::SessionState::SYNCHRONIZED,
        xr::SessionState::VISIBLE,
        xr::SessionState::FOCUSED,
    ] {
        set_state(&b, state);
    }

    // WiVRn's headset reports no profile when its system menu opens, just before the session
    // loses the focus.
    b.unbind();
    b.sync();
    set_state(&b, xr::SessionState::VISIBLE);
    assert!(b.f.input.openxr.input_focus_captured());
    b.f.input.frame_start_update();
    b.f.input.expire_connection_grace();
    b.f.input.frame_start_update();
    assert_eq!(b.events(), []);
    assert!(b.connected(1) && b.connected(2));

    // Once it's back, the profile has some time to come back too.
    set_state(&b, xr::SessionState::FOCUSED);
    b.f.input.frame_start_update();
    assert!(b.connected(1) && b.connected(2));
    b.bind::<Knuckles>();
    b.sync();
    assert_eq!(b.events(), []);

    // A controller that is gone when the focus is lost isn't brought back.
    b.unbind();
    b.sync();
    b.f.input.expire_connection_grace();
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceDeactivated, 1),
            (Ev::TrackedDeviceDeactivated, 2),
        ]
    );
    set_state(&b, xr::SessionState::VISIBLE);
    b.f.input.frame_start_update();
    set_state(&b, xr::SessionState::FOCUSED);
    b.f.input.frame_start_update();
    assert!(!b.connected(1) && !b.connected(2));
    assert_eq!(b.events(), []);
}

#[test]
fn custom_bindings_recover_after_profile_blip() {
    let mut b = Blip::new();
    let dpad = b.action(c"/actions/main/in/dpad");
    let grab = b.action(c"/actions/main/in/grab");
    let toggle = b.action(c"/actions/main/in/toggle");
    b.bind::<Knuckles>();
    b.sync();

    let [dpad_xy, dpad_click] = b.custom_inputs::<Knuckles>(dpad)[..] else {
        panic!("expected the dpad's position and click");
    };
    let [grab_force, grab_value] = b.custom_inputs::<Knuckles>(grab)[..] else {
        panic!("expected the grab's force and value");
    };
    let [toggle_click] = b.custom_inputs::<Knuckles>(toggle)[..] else {
        panic!("expected the toggle's click");
    };
    let press_dpad = |pressed: bool| {
        let (y, click) = if pressed { (0.9, 1.0) } else { (0.0, 0.0) };
        fakexr::set_action_state(dpad_xy, fakexr::ActionState::Vector2(0.0, y), LeftHand);
        fakexr::set_action_state(dpad_click, fakexr::ActionState::Float(click), LeftHand);
    };
    let squeeze = |value: f32| {
        fakexr::set_action_state(grab_force, fakexr::ActionState::Float(0.0), LeftHand);
        fakexr::set_action_state(grab_value, fakexr::ActionState::Float(value), LeftHand);
    };

    press_dpad(true);
    squeeze(1.0);
    fakexr::set_action_state(toggle_click, true.into(), LeftHand);
    b.sync();
    assert_eq!(b.digital(dpad), (true, true, true));
    assert_eq!(b.digital(grab), (true, true, true));
    assert_eq!(b.digital(toggle), (true, true, true));

    // Everything is let go of while the profile is gone.
    b.unbind();
    b.sync();
    for action in [dpad, grab, toggle] {
        assert!(!b.digital(action).0);
    }
    press_dpad(false);
    squeeze(0.0);
    fakexr::set_action_state(toggle_click, false.into(), LeftHand);

    // The bindings read the inputs again, rather than what they were before.
    b.bind::<Knuckles>();
    b.sync();
    assert_eq!(b.digital(dpad), (true, false, true));
    assert_eq!(b.digital(grab), (true, false, true));
    assert_eq!(b.digital(toggle), (true, true, false));

    press_dpad(true);
    squeeze(1.0);
    fakexr::set_action_state(toggle_click, true.into(), LeftHand);
    b.sync();
    assert_eq!(b.digital(dpad), (true, true, true));
    assert_eq!(b.digital(grab), (true, true, true));
    assert_eq!(b.digital(toggle), (true, false, true));
}

#[test]
fn custom_bindings_follow_the_new_profile_before_its_event() {
    let mut b = Blip::new();
    let grab = b.action(c"/actions/main/in/grab");
    b.bind::<Knuckles>();
    b.sync();
    let [grab_value] = b.custom_inputs::<OculusTouch>(grab)[..] else {
        panic!("expected the grab's value");
    };
    let sync_unpolled = |b: &mut Blip| {
        b.f.pending_profile_change = false;
        b.f.sync(vr::VRActiveActionSet_t {
            ulActionSet: b.set,
            ..Default::default()
        });
    };

    // The devices only learn about a new profile from its event, which the frame loop polls, but
    // apps may update their actions several times per frame.
    fakexr::set_action_state(grab_value, fakexr::ActionState::Float(1.0), LeftHand);
    b.f.set_interaction_profile::<OculusTouch>(LeftHand);
    sync_unpolled(&mut b);
    assert_eq!(b.digital(grab), (true, true, true));
    fakexr::set_action_state(grab_value, fakexr::ActionState::Float(0.0), LeftHand);
    sync_unpolled(&mut b);
    assert_eq!(b.digital(grab), (true, false, true));
    sync_unpolled(&mut b);
    assert_eq!(b.digital(grab), (true, false, false));
}

#[test]
fn hand_tracking_without_any_profile() {
    use xr::HandTrackingDataSourceEXT as Source;

    fakexr::set_hand_tracking_supported(true);
    let mut b = Blip::new();
    let pose = b.action(c"/actions/main/in/pose");
    let skeleton = b.action(c"/actions/main/in/skeleton_left");
    let left = b.f.get_input_source_handle(c"/user/hand/left");
    // fakexr can't locate in the stage space.
    b.f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    b.sync();
    assert_eq!(b.events(), [(Ev::TrackedDeviceActivated, 0)]);
    assert!(!b.connected(1));

    fakexr::set_hand_tracked(b.f.raw_session(), LeftHand, Some(Source::UNOBSTRUCTED));
    b.sync();
    assert_eq!(b.events(), [(Ev::TrackedDeviceActivated, 1)]);
    assert!(b.connected(1) && !b.connected(2));
    assert_eq!(
        b.controller_type(1).as_deref(),
        Some("svl_hand_interaction_augmented")
    );
    assert_eq!(
        b.f.input
            .get_device_string_tracked_property(1, Prop::RenderModelName_String)
            .unwrap()
            .to_str()
            .unwrap(),
        "{vrlink}/rendermodels/shuttlecock"
    );

    // Located from the joints.
    let device_pose =
        b.f.input
            .get_controller_pose(Hand::Left, None, xr::Time::from_nanos(1), None)
            .unwrap();
    assert!(device_pose.bPoseIsValid && device_pose.bDeviceIsConnected);
    let action_pose = b.f.get_pose(pose, left).unwrap();
    assert!(action_pose.bActive && action_pose.pose.bPoseIsValid);

    let mut data = vr::InputSkeletalActionData_t::default();
    assert_eq!(
        b.f.input
            .GetSkeletalActionData(skeleton, &mut data, std::mem::size_of_val(&data) as u32),
        vr::EVRInputError::None
    );
    assert!(data.bActive);
    let mut level = vr::EVRSkeletalTrackingLevel::Estimated;
    assert_eq!(
        b.f.input.GetSkeletalTrackingLevel(skeleton, &mut level),
        vr::EVRInputError::None
    );
    assert_eq!(level, vr::EVRSkeletalTrackingLevel::Full);
    let mut bones =
        [vr::VRBoneTransform_t::default(); crate::input::HandSkeletonBone::Count as usize];
    assert_eq!(
        b.f.input.GetSkeletalBoneData(
            skeleton,
            vr::EVRSkeletalTransformSpace::Parent,
            vr::EVRSkeletalMotionRange::WithoutController,
            bones.as_mut_ptr(),
            bones.len() as u32,
        ),
        vr::EVRInputError::None
    );
    // fakexr's joints are all at the origin, unlike the estimated ones.
    let index_finger = bones[crate::input::HandSkeletonBone::IndexFinger2 as usize];
    assert_eq!(index_finger.position.v[..3], [0.0; 3]);

    // Losing sight of the hand for a moment isn't a disconnect.
    fakexr::set_hand_tracked(b.f.raw_session(), LeftHand, None);
    b.sync();
    assert!(b.connected(1));
    let device_pose =
        b.f.input
            .get_controller_pose(Hand::Left, None, xr::Time::from_nanos(2), None)
            .unwrap();
    assert!(!device_pose.bPoseIsValid && device_pose.bDeviceIsConnected);
    assert_eq!(
        device_pose.eTrackingResult,
        vr::ETrackingResult::Running_OutOfRange
    );
    assert_eq!(b.events(), []);

    b.f.input.expire_connection_grace();
    assert!(!b.connected(1));
    assert_eq!(b.events(), [(Ev::TrackedDeviceDeactivated, 1)]);
}

#[test]
fn controller_replaces_hand_tracked_without_profile() {
    use xr::HandTrackingDataSourceEXT as Source;

    fakexr::set_hand_tracking_supported(true);
    let mut b = Blip::new();
    b.f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    b.sync();
    b.events();
    fakexr::set_hand_tracked(b.f.raw_session(), LeftHand, Some(Source::UNOBSTRUCTED));
    b.sync();
    assert_eq!(b.events(), [(Ev::TrackedDeviceActivated, 1)]);

    // Picking up the controller: the app sees the hand go and the controller come.
    b.f.set_interaction_profile::<OculusTouch>(LeftHand);
    b.sync();
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceDeactivated, 1),
            (Ev::TrackedDeviceActivated, 1),
        ]
    );
    assert_eq!(b.controller_type(1).as_deref(), Some("oculus_touch"));

    // A controller turned on while no hand was there just connects.
    b.f.set_interaction_profile::<OculusTouch>(RightHand);
    b.sync();
    assert_eq!(b.events(), [(Ev::TrackedDeviceActivated, 2)]);
}

#[test]
fn hand_tracking_with_unbound_hand_interaction_profile() {
    use xr::HandTrackingDataSourceEXT as Source;

    // A runtime offering XR_EXT_hand_interaction, which xrizer leaves disabled: the hand
    // interaction profile can't be bound, so tracked hands still have no profile.
    fakexr::advertise_extension(xr::sys::EXT_HAND_INTERACTION_EXTENSION_NAME);
    fakexr::set_hand_tracking_supported(true);
    let mut b = Blip::new();
    b.f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    assert!(!b.f.input.openxr.enabled_extensions.ext_hand_interaction);
    fakexr::set_hand_tracked(b.f.raw_session(), RightHand, Some(Source::UNOBSTRUCTED));
    b.sync();
    assert!(b.connected(2));
    let pose =
        b.f.input
            .get_controller_pose(Hand::Right, None, xr::Time::from_nanos(1), None)
            .unwrap();
    assert!(pose.bPoseIsValid);
    assert_eq!(
        b.controller_type(2).as_deref(),
        Some("svl_hand_interaction_augmented")
    );
}

#[test]
#[cfg_attr(not(feature = "monado"), ignore)]
fn profile_changes_keep_generic_trackers() {
    let mut b = Blip::new();
    fakexr::add_trackers(b.f.raw_session());
    b.bind::<Knuckles>();
    b.sync();
    assert_eq!(
        b.events(),
        [
            (Ev::TrackedDeviceActivated, 0),
            (Ev::TrackedDeviceActivated, 1),
            (Ev::TrackedDeviceActivated, 2),
            (Ev::TrackedDeviceActivated, 3),
        ]
    );

    // Trackers are listed again on every profile change, but they're the same devices.
    b.unbind();
    b.sync();
    b.bind::<OculusTouch>();
    b.sync();
    let events = b.events();
    assert!(
        !events.iter().any(|(_, index)| *index == 3),
        "tracker events: {events:?}"
    );
    assert!(b.connected(3));
    assert_eq!(
        b.f.input.device_index_to_tracked_device_class(3),
        Some(vr::ETrackedDeviceClass::GenericTracker)
    );
    assert_eq!(b.f.input.device_index_to_tracked_device_class(4), None);
}
