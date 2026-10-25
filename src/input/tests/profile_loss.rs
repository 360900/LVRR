//! Controllers whose interaction profile changes or goes away for a moment, like WiVRn reports
//! when the headset has none (WiVRn/WiVRn#1100).

use super::Fixture;
use crate::input::{
    InteractionProfile,
    profiles::{knuckles::Knuckles, oculus_touch::OculusTouch},
};
use fakexr::UserPath::*;
use openvr as vr;
use openxr as xr;

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
