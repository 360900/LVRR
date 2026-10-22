use super::{
    ActionData, Input, InteractionProfile,
    profiles::{
        knuckles::Knuckles, oculus_touch::OculusTouch, simple_controller::SimpleController,
        vive_controller::ViveWands,
    },
};
use crate::{
    input::{
        ActionKey,
        custom_bindings::{BoolBindingData, BoolBindingType},
    },
    openxr_data::{FakeCompositor, Hand, OpenXrData},
    vr::{self, IVRInput010_Interface},
};
use fakexr::UserPath::*;
use glam::{Mat4, Quat};
use openxr as xr;
use slotmap::KeyData;
use std::collections::HashSet;
use std::f32::consts::FRAC_PI_4;
use std::ffi::CStr;
use std::sync::{Arc, Barrier};

static ACTIONS_JSONS_DIR: &CStr = unsafe {
    CStr::from_bytes_with_nul_unchecked(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/input_data/\0").as_bytes(),
    )
};

impl std::fmt::Debug for ActionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionData::Bool(_) => f.write_str("InputAction::Bool"),
            ActionData::Vector1 { .. } => f.write_str("InputAction::Float"),
            ActionData::Vector2 { .. } => f.write_str("InputAction::Vector2"),
            ActionData::Pose => f.write_str("InputAction::Pose"),
            ActionData::Skeleton { .. } => f.write_str("InputAction::Skeleton"),
            ActionData::Haptic(_) => f.write_str("InputAction::Haptic"),
        }
    }
}

pub(super) struct Fixture {
    pub input: Arc<Input<FakeCompositor>>,
    pending_profile_change: bool,
    _comp: Arc<FakeCompositor>,
}

pub(super) trait ActionType: xr::ActionTy {
    fn get_xr_action(data: &ActionData) -> Result<xr::sys::Action, String>;
}

macro_rules! impl_action_type {
    ($ty:ty, $err_ty:literal, $pattern:pat => $extract_action:expr) => {
        impl ActionType for $ty {
            fn get_xr_action(data: &ActionData) -> Result<xr::sys::Action, String> {
                match data {
                    $pattern => Ok($extract_action),
                    other => Err(format!("Expected {} action, got {other:?}", $err_ty)),
                }
            }
        }
    };
}

impl_action_type!(bool, "boolean", ActionData::Bool(a) => a.as_raw());
impl_action_type!(f32, "vector1", ActionData::Vector1 { action, .. } => action.as_raw());
impl_action_type!(xr::Vector2f, "vector2", ActionData::Vector2{ action, .. } => action.as_raw());
impl_action_type!(xr::Haptic, "haptic", ActionData::Haptic(a) => a.as_raw());
//impl_action_type!(xr::Posef, "pose", ActionData::Pose { action, .. } => action.as_raw());

impl Fixture {
    pub fn new() -> Self {
        crate::init_logging();
        let xr = Arc::new(
            OpenXrData::new(
                &crate::clientcore::Injector::default(),
                vr::EVRApplicationType::Other,
            )
            .unwrap(),
        );
        let comp = Arc::new(FakeCompositor::new(&xr));
        xr.compositor.set(Arc::downgrade(&comp));
        let ret = Self {
            input: Input::new(xr.clone()).into(),
            pending_profile_change: false,
            _comp: comp,
        };
        xr.input.set(Arc::downgrade(&ret.input));

        ret
    }

    pub fn load_actions(&self, file: &CStr) {
        let path = &[ACTIONS_JSONS_DIR.to_bytes(), file.to_bytes_with_nul()].concat();
        assert_eq!(
            self.input.SetActionManifestPath(path.as_ptr() as _),
            vr::EVRInputError::None,
            "check manifest path: {}",
            std::str::from_utf8(path).expect("Non utf8 path!")
        );
    }

    fn verify_bindings_core(
        &self,
        action: xr::sys::Action,
        interaction_profile: &str,
        action_name: &CStr,
        action_type: &str,
        mut expected_bindings: HashSet<String>,
    ) {
        let profile = self
            .input
            .openxr
            .instance
            .string_to_path(interaction_profile)
            .unwrap();

        let bindings = fakexr::get_suggested_bindings(action, profile);

        let mut found_bindings = Vec::new();

        for binding in bindings {
            assert!(
                expected_bindings.remove(binding.as_str()) || found_bindings.contains(&binding),
                concat!(
                    "Unexpected binding {} for {} action {:?}\n",
                    "found bindings: {:#?}\n",
                    "remaining bindings: {:#?}"
                ),
                binding,
                action_type,
                action_name,
                found_bindings,
                expected_bindings,
            );

            found_bindings.push(binding);
        }

        assert!(
            expected_bindings.is_empty(),
            "Missing expected bindings for {action_type} action {action_name:?}: {expected_bindings:#?}",
        );
    }

    #[track_caller]
    pub fn verify_bindings<T: ActionType>(
        &self,
        interaction_profile: &str,
        action_name: &CStr,
        expected_bindings: impl Into<HashSet<String>>,
    ) {
        let handle = self.get_action_handle(action_name);
        let action = self.get_action::<T>(handle);

        self.verify_bindings_core(
            action,
            interaction_profile,
            action_name,
            std::any::type_name::<T>(),
            expected_bindings.into(),
        )
    }

    /// Checks the inputs read by an action's custom bindings in a profile.
    #[track_caller]
    pub fn verify_custom_bindings(
        &self,
        interaction_profile: &str,
        action_name: &CStr,
        expected_bindings: impl Into<HashSet<String>>,
    ) {
        self.verify_custom_bindings_of(
            interaction_profile,
            action_name,
            |_| true,
            expected_bindings,
        );
    }

    /// Checks the inputs read by an action's threshold bindings in a profile.
    #[track_caller]
    pub fn verify_threshold_bindings(
        &self,
        interaction_profile: &str,
        action_name: &CStr,
        expected_bindings: impl Into<HashSet<String>>,
    ) {
        self.verify_custom_bindings_of(
            interaction_profile,
            action_name,
            |ty| {
                matches!(
                    ty,
                    BoolBindingType::ThresholdFloat(_) | BoolBindingType::ThresholdVec2(_)
                )
            },
            expected_bindings,
        );
    }

    #[track_caller]
    fn verify_custom_bindings_of(
        &self,
        interaction_profile: &str,
        action_name: &CStr,
        filter: impl Fn(&BoolBindingType) -> bool,
        expected_bindings: impl Into<HashSet<String>>,
    ) {
        let handle = self.get_action_handle(action_name);
        let profile = self
            .input
            .openxr
            .instance
            .string_to_path(interaction_profile)
            .unwrap();
        let data = self.input.openxr.session_data.get();
        let loaded = data.input_data.get_loaded_actions().unwrap();
        let found: HashSet<String> = loaded
            .try_get_bindings(handle, profile)
            .into_iter()
            .flatten()
            .filter(|binding| filter(&binding.ty))
            .flat_map(|binding| binding.ty.actions())
            .flat_map(|action| fakexr::get_suggested_bindings(action, profile))
            .collect();
        assert_eq!(
            found,
            expected_bindings.into(),
            "custom bindings for {action_name:?}"
        );
    }

    /// The first OpenXR action read by a custom binding of `handle` in a profile that matches.
    #[track_caller]
    pub fn custom_binding_action(
        &self,
        handle: vr::VRActionHandle_t,
        interaction_profile: &str,
        matches: impl Fn(&BoolBindingType) -> bool,
    ) -> xr::sys::Action {
        let profile = self
            .input
            .openxr
            .instance
            .string_to_path(interaction_profile)
            .unwrap();
        let data = self.input.openxr.session_data.get();
        let loaded = data.input_data.get_loaded_actions().unwrap();
        loaded
            .try_get_bindings(handle, profile)
            .unwrap()
            .iter()
            .find(|binding| matches(&binding.ty))
            .expect("no matching custom binding")
            .ty
            .actions()[0]
    }
}

impl Fixture {
    pub fn get_action_handle(&self, name: &CStr) -> vr::VRActionHandle_t {
        let mut handle = 0;
        assert_eq!(
            self.input.GetActionHandle(name.as_ptr(), &mut handle),
            vr::EVRInputError::None
        );
        assert_ne!(handle, 0);
        handle
    }

    pub fn get_action_set_handle(&self, name: &CStr) -> vr::VRActionSetHandle_t {
        let mut handle = 0;
        assert_eq!(
            self.input.GetActionSetHandle(name.as_ptr(), &mut handle),
            vr::EVRInputError::None
        );
        assert_ne!(handle, 0);
        handle
    }

    pub fn get_input_source_handle(&self, name: &CStr) -> vr::VRInputValueHandle_t {
        let mut src = 0;
        assert_eq!(
            self.input.GetInputSourceHandle(name.as_ptr(), &mut src),
            vr::EVRInputError::None
        );
        src
    }

    pub fn sync(&mut self, active: vr::VRActiveActionSet_t) {
        self.sync_sets(&mut [active]);
    }

    pub fn sync_sets(&mut self, active: &mut [vr::VRActiveActionSet_t]) {
        assert_eq!(
            self.input.UpdateActionState(
                active.as_mut_ptr(),
                std::mem::size_of::<vr::VRActiveActionSet_t>() as u32,
                active.len() as u32
            ),
            vr::EVRInputError::None
        );
        if self.pending_profile_change {
            self.input.openxr.poll_events();
            self.pending_profile_change = false;
        }
    }

    #[track_caller]
    pub fn get_action<T: ActionType>(&self, handle: vr::VRActionHandle_t) -> xr::sys::Action {
        let data = self.input.openxr.session_data.get();
        let actions = data
            .input_data
            .get_loaded_actions()
            .expect("Actions aren't loaded");
        let action = actions.try_get_action(handle).unwrap_or_else(|_| {
            let key = ActionKey::from(KeyData::from_ffi(handle));
            panic!(
                "Couldn't find action ({}) for handle ({handle})",
                self.input.action_map.read().unwrap()[key].path
            );
        });

        T::get_xr_action(action).expect("Couldn't get OpenXR handle for action")
    }

    pub fn get_pose(
        &self,
        handle: vr::VRActionHandle_t,
        restrict: vr::VRInputValueHandle_t,
    ) -> Result<vr::InputPoseActionData_t, vr::EVRInputError> {
        self.input.openxr.poll_events();
        let mut state = Default::default();
        let err = self.input.GetPoseActionDataForNextFrame(
            handle,
            vr::ETrackingUniverseOrigin::Seated,
            &mut state,
            std::mem::size_of_val(&state) as u32,
            restrict,
        );

        if err != vr::EVRInputError::None {
            Err(err)
        } else {
            Ok(state)
        }
    }

    pub fn get_bool_state(
        &self,
        handle: vr::VRActionHandle_t,
    ) -> Result<vr::InputDigitalActionData_t, vr::EVRInputError> {
        self.get_bool_state_hand(handle, 0)
    }

    pub fn get_bool_state_hand(
        &self,
        handle: vr::VRActionHandle_t,
        restrict: vr::VRInputValueHandle_t,
    ) -> Result<vr::InputDigitalActionData_t, vr::EVRInputError> {
        let mut state = Default::default();
        let err = self.input.GetDigitalActionData(
            handle,
            &mut state,
            std::mem::size_of::<vr::InputDigitalActionData_t>() as u32,
            restrict,
        );

        if err != vr::EVRInputError::None {
            Err(err)
        } else {
            Ok(state)
        }
    }

    pub fn set_interaction_profile<P: InteractionProfile>(&mut self, hand: fakexr::UserPath) {
        fakexr::set_interaction_profile(
            self.raw_session(),
            hand,
            self.input
                .openxr
                .instance
                .string_to_path(P::profile_path())
                .unwrap(),
        );
        self.pending_profile_change = true;
    }

    pub fn raw_session(&self) -> xr::sys::Session {
        self.input.openxr.session_data.get().session.as_raw()
    }
}

#[test]
fn unknown_handles() {
    let f = Fixture::new();
    f.load_actions(c"actions.json");

    let handle = f.get_action_handle(c"/actions/set1/in/fakeaction");
    let mut state = Default::default();
    assert_ne!(
        f.input.GetDigitalActionData(
            handle,
            &mut state,
            std::mem::size_of::<vr::InputDigitalActionData_t>() as u32,
            0
        ),
        vr::EVRInputError::None
    );
}

#[test]
fn dominant_hand() {
    use vr::ETrackedControllerRole as Role;
    let f = Fixture::new();
    let get = || {
        let mut hand = Role::Invalid;
        assert_eq!(f.input.GetDominantHand(&mut hand), vr::EVRInputError::None);
        hand
    };

    assert_eq!(get(), Role::RightHand);
    assert_eq!(
        f.input.SetDominantHand(Role::LeftHand),
        vr::EVRInputError::None
    );
    assert_eq!(get(), Role::LeftHand);
    assert_eq!(
        f.input.SetDominantHand(Role::Treadmill),
        vr::EVRInputError::InvalidParam
    );
    assert_eq!(
        f.input.SetDominantHand(Role(1234)),
        vr::EVRInputError::InvalidParam
    );
    assert_eq!(get(), Role::LeftHand);
    assert_eq!(
        f.input.GetDominantHand(std::ptr::null_mut()),
        vr::EVRInputError::InvalidParam
    );
}

#[test]
fn handles_dont_change_after_load() {
    let f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

    f.load_actions(c"actions.json");

    let set_load = f.get_action_set_handle(c"/actions/set1");
    assert_eq!(set_load, set1);
    let act_load = f.get_action_handle(c"/actions/set1/in/boolact");
    assert_eq!(act_load, boolact);
}

#[test]
fn input_state_flow() {
    let mut f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

    f.load_actions(c"actions.json");

    assert!(
        f.input
            .openxr
            .session_data
            .get()
            .input_data
            .get_legacy_actions()
            .is_none()
    );

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let state = f.get_bool_state(boolact).unwrap();
    assert!(!state.bState);
    assert!(!state.bActive);
    assert!(!state.bChanged);

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let state = f.get_bool_state(boolact).unwrap();
    assert!(!state.bState);
    assert!(!state.bActive);
    assert!(!state.bChanged);

    fakexr::set_action_state(
        f.get_action::<bool>(boolact),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let state = f.get_bool_state(boolact).unwrap();
    assert!(state.bState);
    assert!(state.bActive);
    assert!(state.bChanged);
}

#[test]
fn reload_manifest_on_session_restart() {
    let mut f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

    f.load_actions(c"actions.json");
    f.input.openxr.restart_session().unwrap();

    fakexr::set_action_state(
        f.get_action::<bool>(boolact),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let state = f.get_bool_state(boolact).unwrap();
    assert!(state.bState);
    assert!(state.bActive);
}

#[test]
fn second_manifest_path() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    let manifest = |name: &str| {
        let dir = ACTIONS_JSONS_DIR.to_str().unwrap();
        std::ffi::CString::new(format!("{dir}{name}")).unwrap()
    };

    // A failed load doesn't pin its path.
    assert_eq!(
        f.input
            .SetActionManifestPath(manifest("missing.json").as_ptr()),
        vr::EVRInputError::InvalidParam
    );
    f.load_actions(c"actions.json");
    // The same file, spelled differently.
    assert_eq!(
        f.input
            .SetActionManifestPath(manifest("../input_data/actions.json").as_ptr()),
        vr::EVRInputError::None
    );
    assert_eq!(
        f.input
            .SetActionManifestPath(manifest("actions_toggle.json").as_ptr()),
        vr::EVRInputError::MismatchedActionManifest
    );

    // The first manifest stays loaded.
    fakexr::set_action_state(
        f.get_action::<bool>(boolact),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });
    assert!(f.get_bool_state(boolact).unwrap().bState);
}

#[test]
fn awkward_action_names() {
    let f = Fixture::new();
    // Names OpenVR accepts but OpenXR doesn't: missing sets, colliding cleaned names, long
    // and duplicated (localized) names. None of them may abort the load.
    f.load_actions(c"actions_names.json");

    let localized = |name: &CStr| {
        let handle = f.get_action_handle(name);
        fakexr::get_action_localized_name(f.get_action::<bool>(handle))
    };
    for name in [
        c"/actions/main/in/grab",
        c"/actions/main/in/jump!",
        c"/actions/main/in/jump?",
        c"/actions/main/in/",
        c"/actions/averyveryverylongactionsetnamethatgoespastthesixtyfourbytelimitofopenxr/in/action",
        c"/actions/other/in/action",
    ] {
        let name = localized(name);
        assert!(!name.is_empty() && name.len() < 128, "{name:?}");
    }
    assert_eq!(localized(c"/actions/main/in/mixedcase"), "Mixed Case");
    f.get_action::<xr::Haptic>(f.get_action_handle(c"/actions/main/out/grab"));

    // Custom bindings of a long action get actions of their own.
    f.verify_custom_bindings(
        Knuckles::profile_path(),
        c"/actions/main/in/a_long_action_name_for_a_button_bound_through_extra_actions_x",
        [
            "/user/hand/left/input/trigger/value".into(),
            "/user/hand/left/input/trigger/click".into(),
            "/user/hand/left/input/squeeze/force".into(),
            "/user/hand/left/input/squeeze/value".into(),
            "/user/hand/left/input/a/click".into(),
        ],
    );

    let data = f.input.openxr.session_data.get();
    let loaded = data.input_data.get_loaded_actions().unwrap();
    for name in [c"/actions/grab", c"/noprefix/in/action"] {
        assert!(loaded.try_get_action(f.get_action_handle(name)).is_err());
    }
}

#[test]
fn lenient_bindings() {
    let f = Fixture::new();
    // Invalid entries are skipped without dropping the rest of the file, and a controller type
    // listed again (also non-consecutively) only loads the first file.
    f.load_actions(c"actions_lenient.json");

    let path = Knuckles::profile_path();
    f.verify_bindings::<bool>(
        path,
        c"/actions/main/in/a",
        ["/user/hand/left/input/a/click".into()],
    );
    let profile = f.input.openxr.instance.string_to_path(path).unwrap();
    let data = f.input.openxr.session_data.get();
    let loaded = data.input_data.get_loaded_actions().unwrap();

    // Invalid parameters fall back to the defaults.
    let b = f.get_action_handle(c"/actions/main/in/b");
    assert_eq!(loaded.try_get_bindings(b, profile).unwrap().len(), 1);
    // The older dpad_click mode is a dpad.
    let dpad = f.get_action_handle(c"/actions/main/in/dpad");
    assert!(matches!(
        loaded.try_get_bindings(dpad, profile).unwrap()[..],
        [BoolBindingData {
            ty: BoolBindingType::Dpad(_),
            ..
        }]
    ));
    // A set with only poses, one of them for a tracker.
    let pose = f.get_action_handle(c"/actions/poses/in/pose");
    let bound = loaded.try_get_pose(pose, profile).unwrap();
    assert!(bound.left.is_none());
    assert!(bound.right.is_some());
}

#[test]
fn index_finger_curls() {
    let f = Fixture::new();
    f.load_actions(c"actions_fingers.json");
    let path = Knuckles::profile_path();
    f.verify_bindings::<f32>(
        path,
        c"/actions/hands/in/indexcurl",
        [
            "/user/hand/left/input/trigger/value".into(),
            "/user/hand/right/input/trigger/value".into(),
        ],
    );
    for action in [
        c"/actions/hands/in/middlecurl",
        c"/actions/hands/in/ringcurl",
        c"/actions/hands/in/pinkycurl",
    ] {
        f.verify_bindings::<f32>(
            path,
            action,
            [
                "/user/hand/left/input/squeeze/value".into(),
                "/user/hand/right/input/squeeze/value".into(),
            ],
        );
    }
}

#[test]
fn bindings_converted_for_unlisted_controllers() {
    let f = Fixture::new();
    // Only Vive wand bindings: other controllers get them by input role.
    f.load_actions(c"actions_vive_only.json");

    let touch = OculusTouch::profile_path();
    f.verify_bindings::<bool>(
        touch,
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/value".into(),
        ],
    );
    f.verify_bindings::<xr::Vector2f>(
        touch,
        c"/actions/main/in/vec2act",
        ["/user/hand/left/input/thumbstick".into()],
    );
    f.verify_custom_bindings(
        touch,
        c"/actions/main/in/dpadact",
        [
            "/user/hand/right/input/thumbstick".into(),
            "/user/hand/right/input/thumbstick/click".into(),
        ],
    );
    f.verify_threshold_bindings(
        touch,
        c"/actions/main/in/gripact",
        ["/user/hand/left/input/squeeze/value".into()],
    );

    // Index has no menu button: it goes to the upper face button.
    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/b/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
    f.verify_bindings::<xr::Vector2f>(
        Knuckles::profile_path(),
        c"/actions/main/in/vec2act",
        ["/user/hand/left/input/thumbstick".into()],
    );
}

#[test]
fn controllers_tracked_without_bindings() {
    let f = Fixture::new();
    f.load_actions(c"actions_no_bindings.json");

    let data = f.input.openxr.session_data.get();
    let grip = data.input_data.pose_data.grip.as_raw();
    for profile in [OculusTouch::profile_path(), Knuckles::profile_path()] {
        let profile = f.input.openxr.instance.string_to_path(profile).unwrap();
        let suggested = fakexr::get_suggested_bindings(grip, profile);
        assert!(suggested.contains(&"/user/hand/left/input/grip/pose".to_string()));
    }
}

#[track_caller]
pub fn compare_pose(expected: xr::Posef, actual: xr::Posef) {
    fn float_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < f32::EPSILON
    }
    let epos = expected.position;
    let apos = actual.position;
    assert!(
        float_eq(apos.x, epos.x) && float_eq(apos.y, epos.y) && float_eq(apos.z, epos.z),
        "expected position: {epos:?}\nactual position: {apos:?}"
    );

    let erot = expected.orientation;
    let arot = actual.orientation;
    assert!(
        float_eq(arot.x, erot.x)
            && float_eq(arot.y, erot.y)
            && float_eq(arot.z, erot.z)
            && float_eq(arot.w, erot.w),
        "expected orientation: {erot:?}\nactual orientation: {arot:?}",
    );
}

#[test]
fn raw_pose_waitgetposes_and_skeletal_pose_identical_for_valid_pose() {
    raw_pose_waitgetposes_and_skeletal_pose_identical_harness(true);
}

#[test]
fn raw_pose_waitgetposes_and_skeletal_pose_identical_for_invalid_pose() {
    raw_pose_waitgetposes_and_skeletal_pose_identical_harness(false);
}

// Helper to test pose generation against valid & invalid poses.
fn raw_pose_waitgetposes_and_skeletal_pose_identical_harness(valid_poses: bool) {
    let mut f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let left_hand = f.get_input_source_handle(c"/user/hand/left");
    let pose_handle = f.get_action_handle(c"/actions/set1/in/pose");
    let skel_handle = f.get_action_handle(c"/actions/set1/in/skellyl");

    f.load_actions(c"actions.json");
    f.set_interaction_profile::<Knuckles>(LeftHand);

    let frame = || {
        f.input.openxr.poll_events();
        f.input.frame_start_update();
    };

    // we need to wait two frames for the controller to be connected.
    {
        let idx = f
            .input
            .get_controller_device_index(super::Hand::Left)
            .unwrap();
        frame();
        assert!(!f.input.is_device_connected(idx));
        frame();
        assert!(f.input.is_device_connected(idx));
    }

    let rot = Quat::from_rotation_x(-FRAC_PI_4);
    let pose = xr::Posef {
        position: xr::Vector3f {
            x: 0.5,
            y: 0.5,
            z: 0.5,
        },
        orientation: xr::Quaternionf {
            x: rot.x,
            y: rot.y,
            z: rot.z,
            w: rot.w,
        },
    };
    fakexr::set_grip(f.raw_session(), LeftHand, pose);
    fakexr::set_aim(f.raw_session(), LeftHand, pose);
    let xr_time = xr::Time::from_nanos(1234);
    if valid_poses {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
    }

    let assert_validity = |val: bool, pose_descr: &str| {
        if valid_poses {
            assert!(val, "{pose_descr} pose should be valid");
        } else {
            assert!(!val, "{pose_descr} pose should be invalid");
        }
    };

    let seated_origin = vr::ETrackingUniverseOrigin::Seated;
    let waitgetposes_pose =
        f.input
            .get_controller_pose(super::Hand::Left, Some(seated_origin), xr_time, None);
    assert_validity(waitgetposes_pose.unwrap().bPoseIsValid, "WaitGetPoses");

    let mut raw_pose = vr::InputPoseActionData_t {
        pose: vr::TrackedDevicePose_t {
            eTrackingResult: vr::ETrackingResult::Running_OutOfRange,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut skel_pose = raw_pose;

    let ret = f.input.GetPoseActionDataForNextFrame(
        pose_handle,
        seated_origin,
        &mut raw_pose,
        std::mem::size_of_val(&raw_pose) as u32,
        left_hand,
    );
    assert_eq!(ret, vr::EVRInputError::None);
    assert_validity(raw_pose.pose.bPoseIsValid, "GetPoseActionDataForNextFrame");
    compare_pose(
        waitgetposes_pose.unwrap().mDeviceToAbsoluteTracking.into(),
        raw_pose.pose.mDeviceToAbsoluteTracking.into(),
    );

    let ret = f.input.GetPoseActionDataForNextFrame(
        skel_handle,
        seated_origin,
        &mut skel_pose,
        std::mem::size_of_val(&skel_pose) as u32,
        0,
    );
    assert_eq!(ret, vr::EVRInputError::None);
    assert_validity(
        skel_pose.pose.bPoseIsValid,
        "GetPoseActionDataForNextFrame skeletal",
    );

    compare_pose(
        waitgetposes_pose.unwrap().mDeviceToAbsoluteTracking.into(),
        skel_pose.pose.mDeviceToAbsoluteTracking.into(),
    );
}

#[test]
fn actions_with_bad_paths() {
    let mut f = Fixture::new();
    let spaces = f.get_action_handle(c"/actions/set1/in/action with spaces");
    let commas = f.get_action_handle(c"/actions/set1/in/action,with,commas");
    let mixed = f.get_action_handle(c"/actions/set1/in/mixed, action");
    let paren = f.get_action_handle(c"/actions/set1/in/(action)(with)(parenthesis)");
    let brackets = f.get_action_handle(c"/actions/set1/in/[action][with][brackets]");
    let long_bad1 = f.get_action_handle(c"/actions/set1/in/ThisActionHasAReallyLongNameThatIsMostCertainlyLongerThanTheOpenXRLimit,However,ItWillBeGivenASimpleLocalizedName");
    let long_bad2 = f.get_action_handle(c"/actions/set1/in/ThisActionWillAlsoHaveAReallyLongNameAndAShortLocalizedName,MuchLikeThePreviousAction");
    let long_exact = f.get_action_handle(
        c"/actions/set1/in/this right here is an action that is exactly 64 characters long!",
    );

    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions_malformed_paths.json");

    fakexr::set_action_state(
        f.get_action::<bool>(spaces),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<f32>(commas),
        fakexr::ActionState::Float(0.5),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<bool>(mixed),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<bool>(long_bad1),
        fakexr::ActionState::Bool(false),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<bool>(long_bad2),
        fakexr::ActionState::Bool(false),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<bool>(long_exact),
        fakexr::ActionState::Bool(false),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<bool>(paren),
        fakexr::ActionState::Bool(false),
        LeftHand,
    );
    fakexr::set_action_state(
        f.get_action::<bool>(brackets),
        fakexr::ActionState::Bool(false),
        LeftHand,
    );
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let s = f.get_bool_state(spaces).unwrap();
    assert!(s.bActive);
    assert!(s.bState);
    assert!(s.bChanged);

    let s = f.get_bool_state(mixed).unwrap();
    assert!(s.bActive);
    assert!(s.bState);
    assert!(s.bChanged);

    let s = f.get_bool_state(paren).unwrap();
    assert!(s.bActive);
    assert!(!s.bState);
    assert!(!s.bChanged);

    let s = f.get_bool_state(brackets).unwrap();
    assert!(s.bActive);
    assert!(!s.bState);
    assert!(!s.bChanged);

    let s = f.get_bool_state(long_bad1).unwrap();
    assert!(s.bActive);
    assert!(!s.bState);
    assert!(!s.bChanged);

    let s = f.get_bool_state(long_bad2).unwrap();
    assert!(s.bActive);
    assert!(!s.bState);
    assert!(!s.bChanged);

    let s = f.get_bool_state(long_exact).unwrap();
    assert!(s.bActive);
    assert!(!s.bState);
    assert!(!s.bChanged);

    let mut s = vr::InputAnalogActionData_t::default();
    let ret = f
        .input
        .GetAnalogActionData(commas, &mut s, std::mem::size_of_val(&s) as u32, 0);
    assert_eq!(ret, vr::EVRInputError::None);

    assert!(s.bActive);
    assert_eq!(s.x, 0.5);
}

#[test]
fn bindings_with_mismatched_action_type_are_skipped() {
    let f = Fixture::new();
    // The binding file binds actions to inputs of a different type than the action:
    // a boolean action to a trigger's pull (float), a vector1 action to a joystick's position
    // (vector2) and a vector2 action to a joystick's click (boolean).
    // These must be skipped without preventing the valid bindings from being suggested.
    f.load_actions(c"actions_type_mismatch.json");

    let path = Knuckles::profile_path();
    f.verify_bindings::<bool>(
        path,
        c"/actions/default/in/use",
        [
            "/user/hand/left/input/trigger/touch".into(),
            "/user/hand/left/input/thumbstick/touch".into(),
        ],
    );
    f.verify_bindings::<f32>(
        path,
        c"/actions/default/in/grabaxis",
        ["/user/hand/right/input/trigger/value".into()],
    );
    f.verify_bindings::<xr::Vector2f>(
        path,
        c"/actions/default/in/move",
        ["/user/hand/right/input/thumbstick".into()],
    );
    // Haptics, poses and skeletons naming an action of another type are skipped too.
    f.verify_bindings::<xr::Haptic>(
        path,
        c"/actions/default/out/haptic",
        ["/user/hand/right/output/haptic".into()],
    );
    let pose = f.get_action_handle(c"/actions/default/in/pose");
    let profile = f.input.openxr.instance.string_to_path(path).unwrap();
    let data = f.input.openxr.session_data.get();
    let loaded = data.input_data.get_loaded_actions().unwrap();
    let bound = loaded.try_get_pose(pose, profile).unwrap();
    assert!(bound.left.is_none());
    assert!(bound.right.is_some());
}

#[test]
fn pose_action_no_restrict() {
    let mut f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let posel = f.get_action_handle(c"/actions/set1/in/posel");
    let poser = f.get_action_handle(c"/actions/set1/in/poser");

    f.load_actions(c"actions.json");
    f.set_interaction_profile::<SimpleController>(LeftHand);
    f.set_interaction_profile::<SimpleController>(RightHand);
    let session = f.input.openxr.session_data.get().session.as_raw();
    let pose_left = xr::Posef {
        position: xr::Vector3f {
            x: 0.5,
            y: 0.5,
            z: 0.5,
        },
        orientation: xr::Quaternionf::IDENTITY,
    };
    fakexr::set_grip(session, LeftHand, pose_left);

    let pose_right = xr::Posef {
        position: xr::Vector3f {
            x: 0.6,
            y: 0.6,
            z: 0.6,
        },
        orientation: xr::Quaternionf::IDENTITY,
    };
    fakexr::set_grip(session, RightHand, pose_right);

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    for (handle, expected) in [(posel, pose_left), (poser, pose_right)] {
        let actual = f.get_pose(handle, 0).unwrap();
        assert!(actual.bActive);
        let p = actual.pose;
        assert!(p.bPoseIsValid);
        compare_pose(expected, p.mDeviceToAbsoluteTracking.into());
    }
}

#[test]
fn raw_pose_switch_profile() {
    let mut f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let posel = f.get_action_handle(c"/actions/set1/in/posel");
    let poser = f.get_action_handle(c"/actions/set1/in/poser");

    f.load_actions(c"actions.json");
    f.set_interaction_profile::<SimpleController>(LeftHand);
    f.set_interaction_profile::<SimpleController>(RightHand);
    let session = f.input.openxr.session_data.get().session.as_raw();
    let pose_left = xr::Posef {
        position: xr::Vector3f {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        orientation: xr::Quaternionf::IDENTITY,
    };
    fakexr::set_grip(session, LeftHand, pose_left);

    let pose_right = xr::Posef {
        position: xr::Vector3f {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        orientation: xr::Quaternionf::IDENTITY,
    };
    fakexr::set_grip(session, RightHand, pose_right);

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    fn offset_to_pose(offset: &Mat4) -> xr::Posef {
        let translation = offset.w_axis.truncate();
        let rotation = Quat::from_mat4(offset);

        xr::Posef {
            orientation: xr::Quaternionf {
                x: rotation.x,
                y: rotation.y,
                z: rotation.z,
                w: rotation.w,
            },
            position: xr::Vector3f {
                x: translation.x,
                y: translation.y,
                z: translation.z,
            },
        }
    }

    for (handle, hand) in [(posel, Hand::Left), (poser, Hand::Right)] {
        let expected = &SimpleController::offset_grip_pose(hand);
        let actual = f.get_pose(handle, 0).unwrap();
        assert!(actual.bActive, "{hand:?} not active");
        let p = actual.pose;
        assert!(p.bPoseIsValid, "{hand:?} invalid pose");
        compare_pose(offset_to_pose(expected), p.mDeviceToAbsoluteTracking.into());
    }

    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);

    // Cached poses don't reset until next frame
    f.input.openxr.poll_events();
    f.input.frame_start_update();

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    for (handle, expected) in [
        (posel, &OculusTouch::offset_grip_pose(Hand::Left)),
        (poser, &OculusTouch::offset_grip_pose(Hand::Right)),
    ] {
        let actual = f.get_pose(handle, 0).unwrap();
        assert!(actual.bActive);
        let p = actual.pose;
        assert!(p.bPoseIsValid);
        compare_pose(offset_to_pose(expected), p.mDeviceToAbsoluteTracking.into());
    }
}

#[test]
fn cased_actions() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions_cased.json");

    let path = ViveWands::profile_path();
    f.verify_bindings::<bool>(
        path,
        c"/actions/set1/in/BoolAct",
        ["/user/hand/left/input/squeeze/click".into()],
    );
    f.verify_bindings::<f32>(
        path,
        c"/actions/set1/in/Vec1Act",
        ["/user/hand/left/input/trigger/value".into()],
    );
    f.verify_bindings::<xr::Vector2f>(
        path,
        c"/actions/set1/in/Vec2Act",
        ["/user/hand/left/input/trackpad".into()],
    );
    f.verify_bindings::<xr::Haptic>(
        path,
        c"/actions/set1/in/VibAct",
        ["/user/hand/left/output/haptic".into()],
    );

    f.set_interaction_profile::<ViveWands>(LeftHand);
    let session = f.input.openxr.session_data.get().session.as_raw();
    fakexr::set_grip(session, LeftHand, xr::Posef::IDENTITY);
    fakexr::set_aim(session, LeftHand, xr::Posef::IDENTITY);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let poseact = f.get_action_handle(c"/actions/set1/in/PoseAct");
    let pose = f.get_pose(poseact, 0).unwrap();
    assert!(pose.bActive);
    assert!(pose.pose.bPoseIsValid);

    let skelact = f.get_action_handle(c"/actions/set1/in/SkelAct");
    let pose = f.get_pose(skelact, 0).unwrap();
    assert!(pose.bActive);
    assert!(pose.pose.bPoseIsValid);
}

#[test]
fn digital_action_initalize_on_failure() {
    let f = Fixture::new();
    f.load_actions(c"actions.json");
    let bad_state = vr::InputDigitalActionData_t {
        bActive: true,
        bState: true,
        activeOrigin: 12345,
        bChanged: true,
        fUpdateTime: -999.0,
    };

    let mut state = bad_state;
    let bad_handle = 3434343343;
    assert_ne!(
        f.input.GetDigitalActionData(
            bad_handle,
            &mut state,
            std::mem::size_of::<vr::InputDigitalActionData_t>() as u32,
            0
        ),
        vr::EVRInputError::None
    );
    assert!(!state.bActive);
    assert!(!state.bState);
    assert_eq!(state.activeOrigin, 0);
    assert!(!state.bChanged);
    assert_eq!(state.fUpdateTime, 0.0);

    let mut state = bad_state;
    let vecact = f.get_action_handle(c"/actions/set1/in/vec1act");
    assert_ne!(
        f.input.GetDigitalActionData(
            vecact,
            &mut state,
            std::mem::size_of::<vr::InputDigitalActionData_t>() as u32,
            0
        ),
        vr::EVRInputError::None
    );

    assert!(!state.bActive);
    assert!(!state.bState);
    assert_eq!(state.activeOrigin, 0);
    assert!(!state.bChanged);
    assert_eq!(state.fUpdateTime, 0.0);
}

#[test]
fn analog_action_initialize_on_failure() {
    let f = Fixture::new();
    f.load_actions(c"actions.json");

    let bad_state = vr::InputAnalogActionData_t {
        bActive: true,
        activeOrigin: 12345,
        x: 300.0,
        y: 600.0,
        z: 900.0,
        deltaX: 1000.0,
        deltaY: 3000.0,
        deltaZ: -999.999,
        fUpdateTime: 395.0,
    };

    let check_state = |state: vr::InputAnalogActionData_t, desc: &str| {
        assert!(!state.bActive, "{desc}");
        assert_eq!(state.activeOrigin, 0, "{desc}");
        assert_eq!(state.x, 0.0, "{desc}");
        assert_eq!(state.deltaX, 0.0, "{desc}");
        assert_eq!(state.y, 0.0, "{desc}");
        assert_eq!(state.deltaY, 0.0, "{desc}");
        assert_eq!(state.z, 0.0, "{desc}");
        assert_eq!(state.deltaZ, 0.0, "{desc}");
        assert_eq!(state.fUpdateTime, 0.0, "{desc}");
    };

    let mut state = bad_state;
    let bad_handle = 3434343343;
    assert_ne!(
        f.input.GetAnalogActionData(
            bad_handle,
            &mut state,
            std::mem::size_of::<vr::InputAnalogActionData_t>() as u32,
            0
        ),
        vr::EVRInputError::None
    );
    check_state(state, "bad handle");

    let mut state = bad_state;
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    assert_ne!(
        f.input.GetAnalogActionData(
            boolact,
            &mut state,
            std::mem::size_of::<vr::InputAnalogActionData_t>() as u32,
            0
        ),
        vr::EVRInputError::None
    );
    check_state(state, "wrong type");
}

/// Input calls must not panic once the runtime lost the session. A sync that hits the loss marks
/// the session lost, after which the calls fail without touching it.
#[test]
fn session_loss() {
    let f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    let vec1act = f.get_action_handle(c"/actions/set1/in/vec1act");
    let poseact = f.get_action_handle(c"/actions/set1/in/pose");
    let vib = f.get_action_handle(c"/actions/set1/in/vib");
    f.load_actions(c"actions.json");

    let mut active = vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    };
    let mut sync = || {
        f.input.UpdateActionState(
            &mut active,
            std::mem::size_of::<vr::VRActiveActionSet_t>() as u32,
            1,
        )
    };
    fakexr::set_action_state(
        f.get_action::<bool>(boolact),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );
    assert_eq!(sync(), vr::EVRInputError::None);
    assert!(f.get_bool_state(boolact).unwrap().bState);

    fakexr::lose_session(f.raw_session());
    // Before the loss is noticed, failed reads count as inactive.
    let state = f.get_bool_state(boolact).unwrap();
    assert!(!state.bActive && !state.bState);

    assert_eq!(sync(), vr::EVRInputError::IPCError);
    assert!(f.input.openxr.is_lost());
    assert_eq!(sync(), vr::EVRInputError::IPCError);
    assert_eq!(
        f.get_bool_state(boolact).unwrap_err(),
        vr::EVRInputError::IPCError
    );
    let mut analog = vr::InputAnalogActionData_t {
        bActive: true,
        ..Default::default()
    };
    assert_eq!(
        f.input.GetAnalogActionData(
            vec1act,
            &mut analog,
            std::mem::size_of_val(&analog) as u32,
            0
        ),
        vr::EVRInputError::IPCError
    );
    assert!(!analog.bActive);
    assert_eq!(
        f.get_pose(poseact, 0).unwrap_err(),
        vr::EVRInputError::IPCError
    );
    assert_eq!(
        f.input
            .TriggerHapticVibrationAction(vib, 0.0, 1.0, 1.0, 1.0, 0),
        vr::EVRInputError::IPCError
    );
    f.input.frame_start_update();
}

#[test]
fn implicit_action_sets() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    f.load_actions(c"actions_missing_sets.json");

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let res = f.get_bool_state(boolact);
    assert!(res.is_ok(), "{res:?}");
}

#[test]
fn detect_controller_after_manifest_load() {
    let mut f = Fixture::new();
    f.load_actions(c"actions.json");

    let input = f.input.clone();
    let frame = || {
        input.openxr.poll_events();
        input.frame_start_update();
    };
    let index = f.input.get_controller_device_index(Hand::Left).unwrap();

    frame();
    assert!(!f.input.is_device_connected(index));

    f.set_interaction_profile::<Knuckles>(fakexr::UserPath::LeftHand);
    frame();
    // Profile won't be set for this frame - we call sync after events have already been polled
    assert!(!f.input.is_device_connected(index));

    frame();
    assert!(f.input.is_device_connected(index));
}

#[test]
fn empty_manifest() {
    let f = Fixture::new();
    f.input
        .SetActionManifestPath(c"empty_manifest.json".as_ptr() as _);

    f.input.openxr.restart_session().unwrap();
    assert!(f.input.action_map.read().unwrap().is_empty());
}

#[test]
fn load_actions_race() {
    let mut f = Fixture::new();
    f.input.openxr.restart_session().unwrap(); // get to real session

    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);

    let mut f = Arc::new(f);
    f.input.frame_start_update(); // load legacy
    f.input.openxr.poll_events();
    let got_input = f.input.get_legacy_controller_state(
        1,
        &mut vr::VRControllerState_t::default(),
        std::mem::size_of::<vr::VRControllerState_t>() as _,
    );
    assert!(got_input);

    std::thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(2));
        {
            let input = f.input.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                // arbitrary delay, so we get the frame start update right after restart
                std::thread::sleep(std::time::Duration::from_micros(500));
                input.frame_start_update();
            });
        }
        {
            let f = f.clone();
            scope.spawn(move || {
                barrier.wait();
                f.load_actions(c"actions.json");
            });
        }
    });

    let got_input = f.input.get_legacy_controller_state(
        0,
        &mut vr::VRControllerState_t::default(),
        std::mem::size_of::<vr::VRControllerState_t>() as _,
    );
    assert!(!got_input);

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    Arc::get_mut(&mut f).unwrap().sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let res = f.get_bool_state(boolact);
    assert!(res.is_ok(), "{res:?}");
}

/// The bone cache must not be held while waiting for the device list: frame_start_update takes
/// them in the other order.
#[test]
fn bone_cache_not_held_while_waiting_for_devices() {
    let f = Fixture::new();
    f.load_actions(c"actions.json");
    let data = f.input.openxr.session_data.get();
    let devices = data.input_data.devices.write().unwrap();

    std::thread::scope(|s| {
        let bones = s.spawn(|| {
            let data = f.input.openxr.session_data.get();
            f.input.get_bones_from_hand_tracking(
                &data,
                vr::EVRSkeletalTransformSpace::Model,
                Hand::Left,
                vr::EVRSkeletalMotionRange::WithoutController,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let cache_free = f
            .input
            .bone_cache(Hand::Left, vr::EVRSkeletalMotionRange::WithoutController)
            .try_lock()
            .is_ok();
        drop(devices);
        bones.join().unwrap();
        assert!(
            cache_free,
            "bone cache held while waiting for the device list"
        );
    });
}

/// Polling events must not wait for threads reading the device list.
#[test]
fn poll_event_while_devices_are_read() {
    let f = Fixture::new();
    let data = f.input.openxr.session_data.get();
    let devices = data.input_data.devices.read().unwrap();

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut event = vr::VREvent_t::default();
            let got = f
                .input
                .get_next_event(std::mem::size_of_val(&event) as u32, &mut event);
            done_tx.send((got, event.eventType)).unwrap();
        });
        let polled = done_rx.recv_timeout(std::time::Duration::from_secs(10));
        drop(devices);
        let (got, ty) = polled.expect("PollNextEvent waited for a device list reader");
        assert!(got);
        assert_eq!(ty, vr::EVREventType::TrackedDeviceActivated.0);
    });

    // The HMD's activation is only reported once.
    let mut event = vr::VREvent_t::default();
    assert!(
        !f.input
            .get_next_event(std::mem::size_of_val(&event) as u32, &mut event)
    );
}

/// Legacy actions set up on the frame thread must not race an action manifest loaded on another
/// thread - both would attach action sets to the same session.
#[test]
fn legacy_setup_racing_manifest_load() {
    let mut f = Fixture::new();
    f.input.openxr.restart_session().unwrap(); // get to real session

    let (attaching_tx, attaching_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let session = f.input.openxr.session_data.get().session.as_raw();
    fakexr::on_next_attach(session, move || {
        attaching_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });

    std::thread::scope(|s| {
        let frame = s.spawn(|| f.input.frame_start_update());
        attaching_rx.recv().unwrap();
        let manifest = s.spawn(|| f.load_actions(c"actions.json"));
        // Give the manifest load time to run into the legacy setup.
        std::thread::sleep(std::time::Duration::from_millis(100));
        release_tx.send(()).unwrap();
        frame.join().unwrap();
        manifest.join().unwrap();
    });

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });
    let res = f.get_bool_state(boolact);
    assert!(res.is_ok(), "{res:?}");
}

/// An action bound to a native input *and* a custom (threshold) binding must report the release
/// edge of whichever one was actually being used.
///
/// Half-Life: Alyx binds its grab action this way - grip as a `trigger` mode click, which becomes a
/// native binding, and trigger as a `button` mode click, which becomes a threshold on
/// trigger/value. Releasing the threshold binding used to fall back to reporting the native
/// binding's state: the action correctly read as no longer held, but `bChanged` was false and
/// `fUpdateTime`'s companion `last_change_time` came from whenever the *grip* last moved. Alyx
/// never saw the release edge, so held objects dropped straight down instead of being thrown.
#[test]
fn release_edge_with_native_and_threshold_bindings() {
    let mut f = Fixture::new();

    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

    f.load_actions(c"actions_native_and_threshold.json");
    f.set_interaction_profile::<Knuckles>(LeftHand);

    let native = f.get_action::<bool>(boolact);
    let threshold = f.custom_binding_action(boolact, Knuckles::profile_path(), |ty| {
        matches!(ty, BoolBindingType::ThresholdFloat(_))
    });

    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        })
    };

    sync(&mut f);
    let s = f.get_bool_state(boolact).unwrap();
    assert!(!s.bState, "should start unpressed");

    // The native binding on its own: press and release both report an edge. This half always
    // worked, and is here to keep the two halves honest against each other.
    fakexr::set_action_state(native, fakexr::ActionState::Bool(true), LeftHand);
    sync(&mut f);
    let s = f.get_bool_state(boolact).unwrap();
    assert!(s.bState && s.bChanged, "native press: {s:?}");

    fakexr::set_action_state(native, fakexr::ActionState::Bool(false), LeftHand);
    sync(&mut f);
    let s = f.get_bool_state(boolact).unwrap();
    assert!(!s.bState && s.bChanged, "native release: {s:?}");

    // The custom threshold binding, with the native input left alone the whole time. The release
    // is the regression: it used to come back with bChanged == false.
    fakexr::set_action_state(threshold, fakexr::ActionState::Float(1.0), LeftHand);
    sync(&mut f);
    let s = f.get_bool_state(boolact).unwrap();
    assert!(s.bState && s.bChanged, "threshold press: {s:?}");

    fakexr::set_action_state(threshold, fakexr::ActionState::Float(0.0), LeftHand);
    sync(&mut f);
    let s = f.get_bool_state(boolact).unwrap();
    assert!(!s.bState, "threshold release should not be held: {s:?}");
    assert!(
        s.bChanged,
        "threshold release must report the edge, not the idle native binding's state: {s:?}"
    );
}

#[test]
fn wrong_struct_sizes() {
    use vr::EVRInputError::{InvalidParam, None};
    use vr::IVRInput010_Interface;

    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions.json");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    let mut active = vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    };
    let size = std::mem::size_of_val(&active) as u32;
    assert_eq!(
        f.input.UpdateActionState(&mut active, size + 8, 1),
        InvalidParam
    );
    assert_eq!(
        f.input.UpdateActionState(std::ptr::null_mut(), size, 1),
        InvalidParam
    );
    f.sync(active);

    // The size is rejected before anything is written, so the marker stays.
    fn check<T: Default, M: Copy + PartialEq + std::fmt::Debug>(
        marker: fn(&mut T) -> &mut M,
        value: M,
        get: impl Fn(*mut T, u32) -> vr::EVRInputError,
    ) {
        let size = std::mem::size_of::<T>() as u32;
        assert_eq!(get(std::ptr::null_mut(), size), InvalidParam);
        let mut data = T::default();
        *marker(&mut data) = value;
        for wrong_size in [0, size - 4, size + 4] {
            assert_eq!(
                get(&raw mut data, wrong_size),
                InvalidParam,
                "size {wrong_size}"
            );
            assert_eq!(*marker(&mut data), value);
        }
        assert_eq!(get(&raw mut data, size), None);
        assert_ne!(*marker(&mut data), value);
    }

    let digital = f.get_action_handle(c"/actions/set1/in/boolact");
    let update_time: fn(&mut vr::InputDigitalActionData_t) -> &mut f32 = |d| &mut d.fUpdateTime;
    check(update_time, 42.0, |data, size| {
        f.input.GetDigitalActionData(digital, data, size, 0)
    });

    let analog = f.get_action_handle(c"/actions/set1/in/vec1act");
    let update_time: fn(&mut vr::InputAnalogActionData_t) -> &mut f32 = |d| &mut d.fUpdateTime;
    check(update_time, 42.0, |data, size| {
        f.input.GetAnalogActionData(analog, data, size, 0)
    });

    let pose = f.get_action_handle(c"/actions/set1/in/posel");
    let origin = vr::ETrackingUniverseOrigin::Seated;
    let active_origin: fn(&mut vr::InputPoseActionData_t) -> &mut u64 = |d| &mut d.activeOrigin;
    check(active_origin, 42, |data, size| {
        f.input
            .GetPoseActionDataForNextFrame(pose, origin, data, size, 0)
    });

    let origins = get_action_origins(&f, set1, digital);
    let device_index: fn(&mut vr::InputOriginInfo_t) -> &mut u32 = |i| &mut i.trackedDeviceIndex;
    check(device_index, 42, |info, size| {
        f.input.GetOriginTrackedDeviceInfo(origins[0], info, size)
    });
}

#[test]
fn skeletal_action_data_004_bone_count() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions.json");
    f.set_interaction_profile::<Knuckles>(LeftHand);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    // fakexr can't locate relative to the stage.
    f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);

    let action = f.get_action_handle(c"/actions/set1/in/SkellyL");
    let mut data = vr::vr_1_0_17::InputSkeletalActionData_t::default();
    assert_eq!(
        vr::IVRInput004_Interface::GetSkeletalActionData(
            f.input.as_ref(),
            action,
            &mut data,
            std::mem::size_of_val(&data) as u32,
            vr::k_ulInvalidInputValueHandle,
        ),
        vr::EVRInputError::None
    );
    assert!(data.bActive);
    assert_eq!(
        data.activeOrigin,
        f.get_input_source_handle(c"/user/hand/left")
    );
    assert_eq!(data.boneCount, 31);
}

#[test]
fn skeletal_reference_transforms_unknown_pose() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions.json");
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let action = f.get_action_handle(c"/actions/set1/in/SkellyL");
    let mut transforms = [vr::VRBoneTransform_t::default(); 31];
    let mut get = |pose| {
        f.input.GetSkeletalReferenceTransforms(
            action,
            vr::EVRSkeletalTransformSpace::Parent,
            pose,
            transforms.as_mut_ptr(),
            transforms.len() as u32,
        )
    };
    assert_eq!(
        get(vr::EVRSkeletalReferencePose(99)),
        vr::EVRInputError::InvalidParam
    );
    assert_eq!(
        get(vr::EVRSkeletalReferencePose::OpenHand),
        vr::EVRInputError::None
    );
}

#[track_caller]
fn buf_str(buf: &[std::ffi::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[track_caller]
fn get_binding_info(f: &Fixture, action: vr::VRActionHandle_t) -> Vec<vr::InputBindingInfo_t> {
    let mut count = 0;
    assert_eq!(
        vr::IVRInput011_Interface::GetActionBindingInfo(
            f.input.as_ref(),
            action,
            std::ptr::null_mut(),
            0,
            0,
            &mut count,
        ),
        vr::EVRInputError::None
    );

    let mut info = vec![vr::InputBindingInfo_t::default(); count as usize];
    if count > 0 {
        assert_eq!(
            vr::IVRInput011_Interface::GetActionBindingInfo(
                f.input.as_ref(),
                action,
                info.as_mut_ptr(),
                std::mem::size_of::<vr::InputBindingInfo_t>() as u32,
                count,
                &mut count,
            ),
            vr::EVRInputError::None
        );
    }
    info
}

#[track_caller]
fn get_action_origins(
    f: &Fixture,
    set: vr::VRActionSetHandle_t,
    action: vr::VRActionHandle_t,
) -> Vec<vr::VRInputValueHandle_t> {
    let mut origins = [vr::k_ulInvalidInputValueHandle; 8];
    assert_eq!(
        vr::IVRInput011_Interface::GetActionOrigins(
            f.input.as_ref(),
            set,
            action,
            origins.as_mut_ptr(),
            origins.len() as u32,
        ),
        vr::EVRInputError::None
    );
    origins
        .into_iter()
        .filter(|o| *o != vr::k_ulInvalidInputValueHandle)
        .collect()
}

#[track_caller]
fn get_origin_info(f: &Fixture, origin: vr::VRInputValueHandle_t) -> vr::InputOriginInfo_t {
    let mut info = vr::InputOriginInfo_t::default();
    assert_eq!(
        vr::IVRInput011_Interface::GetOriginTrackedDeviceInfo(
            f.input.as_ref(),
            origin,
            &mut info,
            std::mem::size_of::<vr::InputOriginInfo_t>() as u32,
        ),
        vr::EVRInputError::None
    );
    info
}

#[test]
fn action_binding_info() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions.json");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    // Bound only to the left trigger touch in oculus.json.
    let action = f.get_action_handle(c"/actions/set1/in/boolact3");
    let info = get_binding_info(&f, action);

    assert_eq!(info.len(), 1);
    assert_eq!(buf_str(&info[0].rchDevicePathName), "/user/hand/left");
    assert_eq!(buf_str(&info[0].rchInputPathName), "/input/trigger");
    assert_eq!(buf_str(&info[0].rchModeName), "trigger");
    assert_eq!(buf_str(&info[0].rchSlotName), "touch");
    assert_eq!(buf_str(&info[0].rchInputSourceType), "trigger");
}

#[test]
fn legacy_action_binding_info() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions.json");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let action = f.get_action_handle(c"/actions/set1/in/boolact");
    let expected = get_binding_info(&f, action);
    assert!(expected.len() > 1);

    // IVRInput_007 apps pass an array of the struct without rchInputSourceType.
    const LEGACY_SIZE: usize = 512;
    let len = expected.len() * LEGACY_SIZE;
    let mut buf = vec![0x55 as std::ffi::c_char; len + LEGACY_SIZE];
    let mut count = 0;
    assert_eq!(
        vr::IVRInput007_Interface::GetActionBindingInfo(
            f.input.as_ref(),
            action,
            buf.as_mut_ptr().cast(),
            LEGACY_SIZE as u32,
            expected.len() as u32,
            &mut count,
        ),
        vr::EVRInputError::None
    );
    assert_eq!(count as usize, expected.len());
    for (entry, expected) in buf.chunks(LEGACY_SIZE).zip(&expected) {
        let field = |i: usize| buf_str(&entry[i * 128..][..128]);
        assert_eq!(field(0), buf_str(&expected.rchDevicePathName));
        assert_eq!(field(1), buf_str(&expected.rchInputPathName));
        assert_eq!(field(2), buf_str(&expected.rchModeName));
        assert_eq!(field(3), buf_str(&expected.rchSlotName));
    }
    assert!(buf[len..].iter().all(|&c| c == 0x55));

    assert_eq!(
        vr::IVRInput007_Interface::GetActionBindingInfo(
            f.input.as_ref(),
            action,
            buf.as_mut_ptr().cast(),
            LEGACY_SIZE as u32 + 4,
            1,
            &mut count,
        ),
        vr::EVRInputError::InvalidParam
    );
}

#[test]
fn action_origins() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions.json");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    let action = f.get_action_handle(c"/actions/set1/in/boolact3");
    let origins = get_action_origins(&f, set1, action);
    assert!(!origins.is_empty());

    let info = get_origin_info(&f, origins[0]);
    assert_eq!(info.trackedDeviceIndex, Hand::Left as u32);
    assert_eq!(buf_str(&info.rchRenderModelComponentName), "trigger");

    let mut name = [0 as std::ffi::c_char; 256];
    assert_eq!(
        vr::IVRInput011_Interface::GetOriginLocalizedName(
            f.input.as_ref(),
            origins[0],
            name.as_mut_ptr(),
            name.len() as u32,
            0x01 | 0x02 | 0x04,
        ),
        vr::EVRInputError::None
    );
    assert_eq!(
        buf_str(&name),
        "Left Hand / touch controller / /user/hand/left/input/trigger/touch"
    );

    // IVRInput_004 has no section mask and gets the whole name.
    let mut name_004 = [0 as std::ffi::c_char; 256];
    assert_eq!(
        vr::IVRInput004On005::GetOriginLocalizedName(
            f.input.as_ref(),
            origins[0],
            name_004.as_mut_ptr(),
            name_004.len() as u32,
        ),
        vr::EVRInputError::None
    );
    assert_eq!(buf_str(&name_004), buf_str(&name));
}

#[test]
fn dpad_action_binding_info() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    f.load_actions(c"actions_dpad.json");
    f.set_interaction_profile::<ViveWands>(LeftHand);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    });

    // Bound to the north direction of a trackpad dpad, which is driven by the
    // synthetic parent action (see wands_dpad.json).
    let action = f.get_action_handle(c"/actions/set1/in/boolact");
    let info = get_binding_info(&f, action);

    assert_eq!(info.len(), 1);
    assert_eq!(buf_str(&info[0].rchDevicePathName), "/user/hand/left");
    assert_eq!(buf_str(&info[0].rchInputPathName), "/input/trackpad");
    assert_eq!(buf_str(&info[0].rchModeName), "trackpad");
    assert_eq!(buf_str(&info[0].rchSlotName), "position");

    let origins = get_action_origins(&f, set1, action);
    assert!(!origins.is_empty());

    let origin_info = get_origin_info(&f, origins[0]);
    assert_eq!(
        buf_str(&origin_info.rchRenderModelComponentName),
        "trackpad"
    );
}

#[test]
fn synthesized_velocity_from_history() {
    use super::HandSpace;
    use glam::Vec3;

    let space = HandSpace {
        hand: Hand::Left,
        hand_path: xr::Path::NULL,
        raw: Default::default(),
        pose_history: Default::default(),
    };
    let origin = vr::ETrackingUniverseOrigin::Standing;

    // 1 m/s along +X, sampled every 10ms
    let mut out = None;
    for i in 0..6i64 {
        out = space.update_history_and_velocity(
            i * 10_000_000,
            Vec3::new(0.01 * i as f32, 0.0, 0.0),
            origin,
        );
    }
    let v = out.expect("velocity after enough history");
    assert!((v - Vec3::X).length() < 1e-3, "{v:?}");

    // repeated queries at the same predicted time don't corrupt the window
    let v2 = space
        .update_history_and_velocity(50_000_000, Vec3::new(0.05, 0.0, 0.0), origin)
        .unwrap();
    assert!((v2 - v).length() < 1e-3, "{v2:?}");

    // switching tracking origins invalidates the old samples
    let after_switch = space.update_history_and_velocity(
        60_000_000,
        Vec3::ZERO,
        vr::ETrackingUniverseOrigin::Seated,
    );
    assert!(after_switch.is_none());
}

#[test]
fn restricted_set_without_openxr_actions() {
    let mut f = Fixture::new();
    f.load_actions(c"actions_pose_only_set.json");
    let main = f.get_action_set_handle(c"/actions/main");
    let poses = f.get_action_set_handle(c"/actions/poses");
    let left = f.get_input_source_handle(c"/user/hand/left");

    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: main,
        ulRestrictedToDevice: left,
        ..Default::default()
    });
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: poses,
        ulRestrictedToDevice: left,
        ..Default::default()
    });
}

#[test]
fn unrestricted_digital_action_origin() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    f.load_actions(c"actions.json");
    f.set_interaction_profile::<Knuckles>(LeftHand);
    f.set_interaction_profile::<Knuckles>(RightHand);
    let left = f.get_input_source_handle(c"/user/hand/left");
    let right = f.get_input_source_handle(c"/user/hand/right");
    let action = f.get_action::<bool>(boolact);
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        })
    };

    sync(&mut f);
    let state = f.get_bool_state(boolact).unwrap();
    assert!(!state.bActive);
    assert_eq!(state.activeOrigin, 0);

    fakexr::set_action_state(action, true.into(), RightHand);
    sync(&mut f);
    let state = f.get_bool_state(boolact).unwrap();
    assert!(state.bState);
    assert_eq!(state.activeOrigin, right);
    assert_eq!(
        get_origin_info(&f, state.activeOrigin).trackedDeviceIndex,
        Hand::Right as u32
    );

    // Active but not pressed still has an origin.
    fakexr::set_action_state(action, false.into(), RightHand);
    sync(&mut f);
    sync(&mut f);
    let state = f.get_bool_state(boolact).unwrap();
    assert!(state.bActive && !state.bState);
    assert_ne!(state.activeOrigin, 0);

    fakexr::set_action_state(action, true.into(), LeftHand);
    sync(&mut f);
    assert_eq!(f.get_bool_state(boolact).unwrap().activeOrigin, left);
}

#[test]
fn pose_relative_to_now_in_the_past() {
    let f = Fixture::new();
    f.load_actions(c"actions.json");
    let pose = f.get_action_handle(c"/actions/set1/in/pose");
    for seconds in [-0.25, f32::NAN, f32::NEG_INFINITY] {
        let mut data = vr::InputPoseActionData_t::default();
        assert_eq!(
            f.input.GetPoseActionDataRelativeToNow(
                pose,
                vr::ETrackingUniverseOrigin::Seated,
                seconds,
                &mut data,
                std::mem::size_of_val(&data) as u32,
                0,
            ),
            vr::EVRInputError::None
        );
    }
}

#[test]
fn digital_action_update_time() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    f.load_actions(c"actions.json");
    let action = f.get_action::<bool>(boolact);
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        })
    };
    let secs = |secs: f64| xr::Time::from_nanos((secs * 1e9) as i64);
    // Without XR_KHR_convert_timespec_time (fakexr has none), this is the closest to now.
    f.input.openxr.display_time.set(secs(10.0));

    fakexr::set_action_state_with_time(action, true.into(), LeftHand, secs(9.75));
    sync(&mut f);
    assert_eq!(f.get_bool_state(boolact).unwrap().fUpdateTime, -0.25);

    // Unchanged states were updated by the sync.
    sync(&mut f);
    assert_eq!(f.get_bool_state(boolact).unwrap().fUpdateTime, 0.0);

    fakexr::set_action_state_with_time(action, false.into(), LeftHand, secs(1.0));
    sync(&mut f);
    assert_eq!(f.get_bool_state(boolact).unwrap().fUpdateTime, -1.0);
}

#[track_caller]
fn get_analog_state(
    f: &Fixture,
    action: vr::VRActionHandle_t,
    restrict: vr::VRInputValueHandle_t,
) -> vr::InputAnalogActionData_t {
    let mut state = Default::default();
    assert_eq!(
        f.input.GetAnalogActionData(
            action,
            &mut state,
            std::mem::size_of::<vr::InputAnalogActionData_t>() as u32,
            restrict,
        ),
        vr::EVRInputError::None
    );
    state
}

#[test]
fn vector1_deltas_per_sync_and_hand() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let vec1 = f.get_action_handle(c"/actions/set1/in/vec1act");
    f.load_actions(c"actions.json");
    let left = f.get_input_source_handle(c"/user/hand/left");
    let right = f.get_input_source_handle(c"/user/hand/right");
    let action = f.get_action::<f32>(vec1);
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        })
    };
    let read = |f: &Fixture, restrict| {
        let state = get_analog_state(f, vec1, restrict);
        (state.x, state.deltaX, state.activeOrigin)
    };

    fakexr::set_action_state(action, fakexr::ActionState::Float(0.5), LeftHand);
    fakexr::set_action_state(action, fakexr::ActionState::Float(1.0), RightHand);
    sync(&mut f);
    // Reads neither consume the deltas nor share them between hands.
    for _ in 0..2 {
        assert_eq!(read(&f, left), (0.5, 0.5, left));
        assert_eq!(read(&f, right), (1.0, 1.0, right));
        assert_eq!(read(&f, 0), (1.0, 1.0, right));
    }

    sync(&mut f);
    assert_eq!(read(&f, left), (0.5, 0.0, left));
    assert_eq!(read(&f, right), (1.0, 0.0, right));
    assert_eq!(read(&f, 0), (1.0, 0.0, right));

    fakexr::set_action_state(action, fakexr::ActionState::Float(0.25), RightHand);
    sync(&mut f);
    assert_eq!(read(&f, right), (0.25, -0.75, right));
    assert_eq!(read(&f, 0), (0.5, -0.5, left));
}

#[test]
fn skeletal_tracking_level_follows_hand_tracking() {
    use vr::EVRSkeletalTrackingLevel::*;
    use xr::HandTrackingDataSourceEXT as Source;

    fakexr::set_hand_tracking_supported(true);
    let mut f = Fixture::new();
    f.load_actions(c"actions.json");
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
    let level = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
        // drop cached hand skeletons
        f.input.frame_start_update();
        let mut level = Estimated;
        assert_eq!(
            f.input.GetSkeletalTrackingLevel(skeleton, &mut level),
            vr::EVRInputError::None
        );
        level
    };

    // Reported before any bone data was requested.
    f.set_interaction_profile::<SimpleController>(LeftHand);
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::UNOBSTRUCTED));
    assert_eq!(level(&mut f), Full);
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, None);
    assert_eq!(level(&mut f), Full);

    // A different device doesn't inherit it.
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    assert_eq!(level(&mut f), Estimated);
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::CONTROLLER));
    assert_eq!(level(&mut f), Partial);
}

#[test]
fn secondary_set_needs_restricted_primary() {
    let mut f = Fixture::new();
    f.load_actions(c"actions_pose_only_set.json");
    let main = f.get_action_set_handle(c"/actions/main");
    let poses = f.get_action_set_handle(c"/actions/poses");
    let boolact = f.get_action_handle(c"/actions/main/in/boolact");
    let left = f.get_input_source_handle(c"/user/hand/left");
    fakexr::set_action_state(f.get_action::<bool>(boolact), true.into(), RightHand);

    // Unrestricted: the secondary set is ignored, even if it isn't a valid handle.
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: poses,
        ulSecondaryActionSet: main,
        ..Default::default()
    });
    assert!(!f.get_bool_state(boolact).unwrap().bActive);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: poses,
        ulSecondaryActionSet: 12345,
        ..Default::default()
    });

    // Restricted to the left hand: the secondary set is active for the right hand.
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: poses,
        ulRestrictedToDevice: left,
        ulSecondaryActionSet: main,
        ..Default::default()
    });
    assert!(f.get_bool_state(boolact).unwrap().bActive);
}

#[test]
fn action_set_priorities() {
    let mut f = Fixture::new();
    f.load_actions(c"actions_priority.json");
    let game = f.get_action_set_handle(c"/actions/game");
    let menu = f.get_action_set_handle(c"/actions/menu");
    let fire = f.get_action_handle(c"/actions/game/in/fire");
    let jump = f.get_action_handle(c"/actions/game/in/jump");
    let throttle = f.get_action_handle(c"/actions/game/in/throttle");
    let move_ = f.get_action_handle(c"/actions/game/in/move");
    let select = f.get_action_handle(c"/actions/menu/in/select");
    // So the menu's trigger comes from a custom binding's action.
    assert!(
        f.get_extra_action(select, ExtraActionType::Analog)
            .is_some()
    );
    let left = f.get_input_source_handle(c"/user/hand/left");
    let right = f.get_input_source_handle(c"/user/hand/right");
    f.set_interaction_profile::<Knuckles>(LeftHand);
    f.set_interaction_profile::<Knuckles>(RightHand);

    for hand in [LeftHand, RightHand] {
        fakexr::set_action_state(f.get_action::<bool>(fire), true.into(), hand);
        fakexr::set_action_state(f.get_action::<bool>(jump), true.into(), hand);
        fakexr::set_action_state(
            f.get_action::<f32>(throttle),
            fakexr::ActionState::Float(0.5),
            hand,
        );
        fakexr::set_action_state(
            f.get_action::<xr::Vector2f>(move_),
            fakexr::ActionState::Vector2(0.0, 1.0),
            hand,
        );
    }
    let sync = |f: &mut Fixture, menu_priority, menu_restrict| {
        f.sync_sets(&mut [
            vr::VRActiveActionSet_t {
                ulActionSet: game,
                ..Default::default()
            },
            vr::VRActiveActionSet_t {
                ulActionSet: menu,
                ulRestrictedToDevice: menu_restrict,
                nPriority: menu_priority,
                ..Default::default()
            },
        ])
    };
    let digital = |f: &Fixture, restrict| {
        let s = f.get_bool_state_hand(fire, restrict).unwrap();
        (s.bActive, s.bState, s.bChanged, s.activeOrigin)
    };

    sync(&mut f, 0, 0);
    assert_eq!(digital(&f, 0), (true, true, true, left));

    // The menu binds the trigger and thumbstick: actions bound to them are masked, and a held
    // button is released.
    sync(&mut f, 1, 0);
    assert_eq!(digital(&f, 0), (false, false, true, 0));
    assert_eq!(digital(&f, left), (false, false, true, 0));
    assert!(!get_analog_state(&f, throttle, 0).bActive);
    assert!(!get_analog_state(&f, move_, right).bActive);
    // Jump is also bound to A, which the menu doesn't use.
    assert!(f.get_bool_state(jump).unwrap().bState);

    sync(&mut f, 1, 0);
    assert_eq!(digital(&f, 0), (false, false, false, 0));

    // Unmasking a held button doesn't press it.
    sync(&mut f, 0, 0);
    assert_eq!(digital(&f, 0), (true, true, false, left));
    assert!(get_analog_state(&f, throttle, 0).bActive);

    // A set restricted to one hand only masks that hand.
    sync(&mut f, 1, right);
    assert_eq!(digital(&f, left), (true, true, false, left));
    assert_eq!(digital(&f, right), (false, false, true, 0));
    assert_eq!(digital(&f, 0), (true, true, false, left));
    let throttle_state = get_analog_state(&f, throttle, 0);
    assert!(throttle_state.bActive);
    assert_eq!(throttle_state.activeOrigin, left);
    assert!(!get_analog_state(&f, move_, right).bActive);
}

#[test]
fn masked_toggle_ignores_presses() {
    let mut f = Fixture::new();
    f.load_actions(c"actions_priority.json");
    let game = f.get_action_set_handle(c"/actions/game");
    let menu = f.get_action_set_handle(c"/actions/menu");
    let crouch = f.get_action_handle(c"/actions/game/in/crouch");
    f.set_interaction_profile::<Knuckles>(LeftHand);
    let toggle = f
        .get_extra_action(crouch, ExtraActionType::ToggleAction)
        .unwrap();
    fakexr::set_action_state(toggle, false.into(), LeftHand);
    // The menu binds B too.
    let sync = |f: &mut Fixture, menu_priority| {
        f.sync_sets(&mut [
            vr::VRActiveActionSet_t {
                ulActionSet: game,
                ..Default::default()
            },
            vr::VRActiveActionSet_t {
                ulActionSet: menu,
                nPriority: menu_priority,
                ..Default::default()
            },
        ]);
        let s = f.get_bool_state(crouch).unwrap();
        (s.bActive, s.bState, s.bChanged)
    };

    assert_eq!(sync(&mut f, 0), (true, false, false));
    assert_eq!(sync(&mut f, 1), (false, false, false));
    fakexr::set_action_state(toggle, true.into(), LeftHand);
    assert_eq!(sync(&mut f, 1), (false, false, false));
    fakexr::set_action_state(toggle, false.into(), LeftHand);
    assert_eq!(sync(&mut f, 1), (false, false, false));

    // The press went to the menu, so the toggle is still off.
    assert_eq!(sync(&mut f, 0), (true, false, false));
    fakexr::set_action_state(toggle, true.into(), LeftHand);
    assert_eq!(sync(&mut f, 0), (true, true, true));
}

#[test]
fn extreme_set_priorities() {
    let mut f = Fixture::new();
    f.load_actions(c"actions.json");
    let set1 = f.get_action_set_handle(c"/actions/set1");
    for priority in [i32::MIN, i32::MAX] {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            nPriority: priority,
            ..Default::default()
        });
    }
}

#[test]
fn rejected_profile_doesnt_stop_manifest_load() {
    fakexr::reject_profile(Knuckles::profile_path());
    let f = Fixture::new();
    f.load_actions(c"actions.json");

    let action = f.get_action::<bool>(f.get_action_handle(c"/actions/set1/in/boolact"));
    let path = |p| f.input.openxr.instance.string_to_path(p).unwrap();
    assert!(fakexr::check_no_suggested_bindings(
        action,
        path(Knuckles::profile_path())
    ));
    assert!(!fakexr::check_no_suggested_bindings(
        action,
        path(OculusTouch::profile_path())
    ));
}

#[test]
fn pose_relative_to_now_takes_any_seconds() {
    // fakexr doesn't have XR_KHR_convert_timespec_time, so this also covers its absence.
    let f = Fixture::new();
    f.load_actions(c"actions.json");
    let pose = f.get_action_handle(c"/actions/set1/in/pose");

    for seconds in [0.0, -0.011, f32::NAN, f32::INFINITY] {
        let mut data = vr::InputPoseActionData_t::default();
        assert_eq!(
            f.input.GetPoseActionDataRelativeToNow(
                pose,
                vr::ETrackingUniverseOrigin::Seated,
                seconds,
                &mut data,
                std::mem::size_of_val(&data) as u32,
                0,
            ),
            vr::EVRInputError::None,
            "{seconds}"
        );
    }
}

#[test]
fn input_003_update_action_state() {
    use vr::IVRInput003On004;

    let f = Fixture::new();
    f.load_actions(c"actions.json");
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    fakexr::set_action_state(
        f.get_action::<bool>(boolact),
        fakexr::ActionState::Bool(true),
        LeftHand,
    );

    // The beta's sets have no priority, so they're smaller.
    let mut set = vr::vr_1_0_15::VRActiveActionSet_t {
        ulActionSet: set1,
        ..Default::default()
    };
    assert_eq!(std::mem::size_of_val(&set), 24);
    let mut update = |size| IVRInput003On004::UpdateActionState(&*f.input, &mut set, size, 1);
    assert_eq!(update(32), vr::EVRInputError::InvalidParam);
    assert_eq!(update(24), vr::EVRInputError::None);

    let mut state = vr::InputDigitalActionData_t::default();
    assert_eq!(
        IVRInput003On004::GetDigitalActionData(
            &*f.input,
            boolact,
            &mut state,
            std::mem::size_of_val(&state) as u32
        ),
        vr::EVRInputError::None
    );
    assert!(state.bActive && state.bState);
}

#[test]
fn input_003_compressed_skeletal_data_fails() {
    use vr::IVRInput003On004;

    let f = Fixture::new();
    let mut required = u32::MAX;
    assert_ne!(
        IVRInput003On004::GetSkeletalActionDataCompressed(
            &*f.input,
            0,
            vr::EVRSkeletalTransformSpace::Model,
            0.0,
            std::ptr::null_mut(),
            0,
            &mut required,
        ),
        vr::EVRInputError::None
    );
    assert_eq!(required, 0);
}
