use super::{
    ActionData, Input, InteractionProfile,
    profiles::{
        frame_controller::FrameController, knuckles::Knuckles, oculus_touch::OculusTouch,
        simple_controller::SimpleController, vive_controller::ViveWands,
        xbox_controller::XboxController,
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

mod profile_loss;

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
            ActionData::Vector3 { .. } => f.write_str("InputAction::Vector3"),
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
fn null_names() {
    let f = Fixture::new();
    let mut handle = 0;
    assert_eq!(
        f.input.GetActionHandle(std::ptr::null(), &mut handle),
        vr::EVRInputError::InvalidParam
    );
    assert_eq!(
        f.input.GetActionSetHandle(std::ptr::null(), &mut handle),
        vr::EVRInputError::InvalidParam
    );
    assert_eq!(
        f.input.GetInputSourceHandle(std::ptr::null(), &mut handle),
        vr::EVRInputError::InvalidParam
    );
}

#[test]
fn concurrent_lookups_share_handles() {
    const THREADS: usize = 4;
    let f = Fixture::new();
    for i in 0..50 {
        let set = std::ffi::CString::new(format!("/actions/set{i}")).unwrap();
        let action = std::ffi::CString::new(format!("/actions/set{i}/in/action")).unwrap();
        let source = std::ffi::CString::new(format!("/user/hand/left/input/thing{i}")).unwrap();
        let barrier = Barrier::new(THREADS);
        let handles: Vec<_> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        (
                            f.get_action_set_handle(&set),
                            f.get_action_handle(&action),
                            f.get_input_source_handle(&source),
                        )
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        assert!(handles.iter().all(|h| *h == handles[0]), "{handles:?}");
    }
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
    // The Index's pinch gesture (Blade & Sorcery grabs with it) follows the trigger.
    f.verify_custom_bindings(
        path,
        c"/actions/hands/in/grabpinch",
        ["/user/hand/left/input/trigger/value".into()],
    );
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
fn menu_bindings_converted_in_every_set() {
    let f = Fixture::new();
    // Two sets and a chord use the same menu buttons.
    f.load_actions(c"actions_vive_only_menus.json");

    let both = |left: &str, right: &str| {
        [
            format!("/user/hand/left/input/{left}"),
            format!("/user/hand/right/input/{right}"),
        ]
    };
    for action in [c"/actions/main/in/pause", c"/actions/menu/in/back"] {
        f.verify_bindings::<bool>(Knuckles::profile_path(), action, both("b/click", "b/click"));
        f.verify_bindings::<bool>(
            OculusTouch::profile_path(),
            action,
            both("menu/click", "b/click"),
        );
        f.verify_bindings::<bool>(
            FrameController::profile_path(),
            action,
            both("view/click", "menu/click"),
        );
    }
    f.verify_custom_bindings(
        Knuckles::profile_path(),
        c"/actions/menu/in/reset",
        both("b/click", "b/click"),
    );
}

/// Sets an environment variable for the action manifests this thread loads.
pub(super) fn set_test_env(name: &'static str, value: &str) {
    super::action_manifest::TEST_ENV.with_borrow_mut(|env| env.insert(name, value.into()));
}

#[test]
fn custom_bindings_for_unlisted_controllers() {
    set_test_env(
        "XRIZER_CUSTOM_BINDINGS_DIR",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/input_data/custom_bindings"
        ),
    );
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    // The custom bindings rather than converted ones.
    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/a/click".into()],
    );
    // Controllers without them are still converted.
    f.verify_bindings::<bool>(
        OculusTouch::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/value".into(),
        ],
    );
}

#[test]
fn additional_bindings() {
    set_test_env("XRIZER_ADDITIONAL_BINDINGS", "oculus_touch;vive_controller");
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    // From bindings_oculus_touch.json next to the manifest, rather than converted.
    f.verify_bindings::<bool>(
        OculusTouch::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/x/click".into()],
    );
    // The game's own bindings win over bindings_vive_controller.json.
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/menu/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
}

#[test]
fn bindings_converted_for_pico() {
    use super::profiles::pico::Pico4;
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    let pico = Pico4::profile_path();
    // The right menu button goes to the upper face button.
    f.verify_bindings::<bool>(
        pico,
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
    f.verify_bindings::<xr::Vector2f>(
        pico,
        c"/actions/main/in/vec2act",
        ["/user/hand/left/input/thumbstick".into()],
    );
    f.verify_threshold_bindings(
        pico,
        c"/actions/main/in/gripact",
        ["/user/hand/left/input/squeeze/value".into()],
    );
}

#[test]
fn bindings_converted_for_cosmos() {
    use super::profiles::vive_cosmos::ViveCosmos;
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    let cosmos = ViveCosmos::profile_path();
    f.verify_bindings::<bool>(
        cosmos,
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
    f.verify_bindings::<xr::Vector2f>(
        cosmos,
        c"/actions/main/in/vec2act",
        ["/user/hand/left/input/thumbstick".into()],
    );
    f.verify_bindings::<bool>(
        cosmos,
        c"/actions/main/in/gripact",
        ["/user/hand/left/input/squeeze/click".into()],
    );
}

#[test]
fn bindings_converted_from_cosmos() {
    use super::profiles::vive_cosmos::ViveCosmos;
    let f = Fixture::new();
    f.load_actions(c"actions_cosmos_only.json");

    // Its own, with the bumper.
    f.verify_bindings::<bool>(
        ViveCosmos::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/x/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/left/input/shoulder/click".into(),
            "/user/hand/left/input/menu/click".into(),
        ],
    );
    // Others without a bumper drop it.
    f.verify_bindings::<bool>(
        OculusTouch::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/x/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/left/input/menu/click".into(),
        ],
    );
    f.verify_bindings::<xr::Vector2f>(
        OculusTouch::profile_path(),
        c"/actions/main/in/vec2act",
        ["/user/hand/left/input/thumbstick".into()],
    );
    f.verify_threshold_bindings(
        OculusTouch::profile_path(),
        c"/actions/main/in/gripact",
        ["/user/hand/right/input/squeeze/value".into()],
    );
    // Index has no X or menu button: they go to its face buttons.
    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/a/click".into(),
            "/user/hand/left/input/b/click".into(),
            "/user/hand/right/input/b/click".into(),
        ],
    );
    // The Frame has bumpers.
    f.verify_bindings::<bool>(
        FrameController::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/dpad_down/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/left/input/bumper/click".into(),
            "/user/hand/left/input/view/click".into(),
        ],
    );
}

#[test]
fn converted_bindings_simulate_their_controller() {
    use super::profiles::{
        pico::Pico4, touch_pro::TouchPro, vive_cosmos::ViveCosmos, vive_focus3::ViveFocus3,
    };
    use vr::ETrackedDeviceProperty as Prop;

    let get = |f: &Fixture, index, prop| {
        f.input
            .get_device_string_tracked_property(index, prop)
            .map(|s| s.into_string().unwrap())
    };
    let identity = |f: &Fixture| {
        [
            get(f, 1, Prop::ControllerType_String),
            get(f, 2, Prop::RenderModelName_String),
            get(
                f,
                vr::k_unTrackedDeviceIndex_Hmd,
                Prop::ControllerType_String,
            ),
        ]
        .map(Option::unwrap)
    };
    fn connect<P: InteractionProfile>(f: &mut Fixture, set: &CStr) {
        f.set_interaction_profile::<P>(LeftHand);
        f.set_interaction_profile::<P>(RightHand);
        let set = f.get_action_set_handle(set);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set,
            ..Default::default()
        });
    }

    // Converted from the Vive's bindings, the Pico and Cosmos controllers look like the Vive's,
    // headset included.
    let mut f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");
    let vive = ["vive_controller", "vr_controller_vive_1_5", "vive"];
    connect::<Pico4>(&mut f, c"/actions/main");
    assert_eq!(identity(&f), vive);
    connect::<ViveCosmos>(&mut f, c"/actions/main");
    assert_eq!(identity(&f), vive);
    // The other profiles show themselves.
    connect::<ViveFocus3>(&mut f, c"/actions/main");
    assert_eq!(
        identity(&f),
        [
            "vive_focus3_controller",
            "vive_focus3_controller_right",
            "vive_focus3"
        ]
    );
    connect::<TouchPro>(&mut f, c"/actions/main");
    assert_eq!(
        identity(&f),
        ["oculus_touch", "oculus_quest_pro_controller_right", "rift"]
    );

    // With bindings of their own, they show themselves.
    let mut f = Fixture::new();
    f.load_actions(c"actions.json");
    connect::<Pico4>(&mut f, c"/actions/set1");
    assert_eq!(
        identity(&f),
        ["pico_controller", "pico_4_controller_right", "rift"]
    );
    assert_eq!(
        get(&f, 1, Prop::ModelNumber_String).as_deref(),
        Some("PICO 4 (Left Controller)")
    );
    connect::<ViveCosmos>(&mut f, c"/actions/set1");
    assert_eq!(
        identity(&f),
        [
            "vive_cosmos_controller",
            "vive_cosmos_controller_right",
            "vive_cosmos"
        ]
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

/// A home directory in a temp dir with a Steam install where SteamVR left its settings, for the
/// manifests this thread loads. Removed on drop.
pub(super) struct FakeSteam {
    pub home: std::path::PathBuf,
}

impl FakeSteam {
    pub const APP_KEY: &str = "steam.app.4242";

    pub fn new(test: &str) -> Self {
        let home =
            std::env::temp_dir().join(format!("xrizer-steamvr-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".local/share/Steam/config")).unwrap();
        set_test_env("HOME", home.to_str().unwrap());
        set_test_env("SteamAppId", "4242");
        Self { home }
    }

    fn root(&self) -> std::path::PathBuf {
        self.home.join(".local/share/Steam")
    }

    /// Saves bindings choices for the app, as `(key, url)`.
    pub fn choose(&self, choices: &[(&str, &str)]) {
        self.choose_for(Self::APP_KEY, choices);
    }

    /// Saves bindings choices for the app key `app_key`, as `(key, url)`.
    fn choose_for(&self, app_key: &str, choices: &[(&str, &str)]) {
        let section: serde_json::Map<_, _> = choices
            .iter()
            .map(|(key, url)| (key.to_string(), (*url).into()))
            .collect();
        let settings = serde_json::json!({
            "steamvr": { "enableHomeApp": false },
            app_key: section,
        });
        std::fs::write(
            self.root().join("config/steamvr.vrsettings"),
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();
    }

    /// Puts a Workshop item of SteamVR's in a second library, like Steam downloads them.
    fn add_workshop_item(&self, item: &str, file: &str, contents: &[u8]) {
        let library = self.home.join("games/SteamLibrary");
        let dir = library.join("steamapps/workshop/content/250820").join(item);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), contents).unwrap();
        let root = self.root();
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\"\n\t\t{{\n\t\t\t\"228980\"\t\t\"181169252\"\n\t\t}}\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\"\n\t\t{{\n\t\t\t\"250820\"\t\t\"5636124391\"\n\t\t}}\n\t}}\n}}\n",
                root.display(),
                library.display()
            ),
        )
        .unwrap();
    }
}

impl Drop for FakeSteam {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

pub(super) fn input_data(file: &str) -> String {
    format!("{}/tests/input_data/{file}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn steamvr_chosen_bindings() {
    let steam = FakeSteam::new("chosen");
    // A file the user picked, at a path SteamVR escapes in the URL.
    let picked = steam.home.join("my bindings/vive.json");
    std::fs::create_dir_all(picked.parent().unwrap()).unwrap();
    std::fs::copy(input_data("vive_only_single.json"), &picked).unwrap();
    let url = format!("file://{}", picked.display()).replace(' ', "%20");
    steam.add_workshop_item(
        "3822741892",
        "4573893327432113271_legacy.bin",
        &std::fs::read(input_data("steamvr_workshop_knuckles.json")).unwrap(),
    );
    steam.choose(&[
        ("vive_controller_250820_CurrentURL_steamvrinput", &url),
        // Older SteamVR's key loses to the current one.
        (
            "vive_controller_CurrentURL_steamvrinput",
            "file:///nonexistent.json",
        ),
        // The game has no bindings for Index controllers.
        (
            "knuckles_250820_CurrentURL_steamvrinput",
            "vr-input-workshop://3822741892",
        ),
        // A legacy app's choice.
        (
            "oculus_touch_250820_CurrentURL_legacy",
            "vr-input-workshop://3822741892",
        ),
    ]);
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/trigger/click".into()],
    );
    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/right/input/b/click".into()],
    );
    // Converted from the chosen bindings, which replace the game's like custom ones do.
    f.verify_bindings::<bool>(
        OculusTouch::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/trigger/value".into()],
    );
}

#[test]
fn custom_bindings_over_steamvr_choice() {
    let steam = FakeSteam::new("custom");
    steam.add_workshop_item(
        "3822741892",
        "1_legacy.bin",
        &std::fs::read(input_data("steamvr_workshop_knuckles.json")).unwrap(),
    );
    steam.choose(&[(
        "knuckles_250820_CurrentURL_steamvrinput",
        "vr-input-workshop://3822741892",
    )]);
    set_test_env("XRIZER_CUSTOM_BINDINGS_DIR", &input_data("custom_bindings"));
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/a/click".into()],
    );
}

#[test]
fn unusable_steamvr_choice_uses_the_default() {
    let steam = FakeSteam::new("unusable");
    // The item is for another controller.
    steam.add_workshop_item(
        "3822741892",
        "1_legacy.bin",
        &std::fs::read(input_data("steamvr_workshop_knuckles.json")).unwrap(),
    );
    steam.choose(&[
        (
            "vive_controller_250820_CurrentURL_steamvrinput",
            "file:///nonexistent/bindings.json",
        ),
        (
            "oculus_touch_250820_CurrentURL_steamvrinput",
            "vr-input-workshop://3822741892",
        ),
        (
            "knuckles_250820_CurrentURL_steamvrinput",
            "vr-input-workshop://111",
        ),
    ]);
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/menu/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
    f.verify_bindings::<bool>(
        OculusTouch::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/value".into(),
        ],
    );
}

/// Puts custom bindings for the app key in the per-app directory under `config`.
pub(super) fn add_app_bindings(config: &std::path::Path, key: &str, file: &str, from: &str) {
    let dir = config.join("xrizer/bindings").join(key);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(input_data(from), dir.join(file)).unwrap();
}

#[test]
fn per_app_custom_bindings() {
    let steam = FakeSteam::new("per-app");
    let url = format!("file://{}", input_data("knuckles.json"));
    steam.choose(&[("knuckles_250820_CurrentURL_steamvrinput", &url)]);
    add_app_bindings(
        &steam.home.join(".config"),
        FakeSteam::APP_KEY,
        "knuckles.json",
        "steamvr_workshop_knuckles.json",
    );
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    // Over the choice in SteamVR.
    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/right/input/b/click".into()],
    );
}

#[test]
fn per_app_custom_bindings_in_xdg_config_home() {
    let steam = FakeSteam::new("per-app-xdg");
    let xdg = steam.home.join("xdg");
    set_test_env("XDG_CONFIG_HOME", xdg.to_str().unwrap());
    add_app_bindings(
        &xdg,
        FakeSteam::APP_KEY,
        "knuckles.json",
        "steamvr_workshop_knuckles.json",
    );
    // Not where XDG_CONFIG_HOME points.
    add_app_bindings(
        &steam.home.join(".config"),
        FakeSteam::APP_KEY,
        "oculus_touch.json",
        "steamvr_workshop_knuckles.json",
    );
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/right/input/b/click".into()],
    );
    f.verify_bindings::<bool>(
        OculusTouch::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/b/click".into(),
            "/user/hand/right/input/trigger/value".into(),
        ],
    );
}

#[test]
fn custom_bindings_dir_over_per_app_bindings() {
    let steam = FakeSteam::new("dir-over-per-app");
    add_app_bindings(
        &steam.home.join(".config"),
        FakeSteam::APP_KEY,
        "knuckles.json",
        "steamvr_workshop_knuckles.json",
    );
    set_test_env("XRIZER_CUSTOM_BINDINGS_DIR", &input_data("custom_bindings"));
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/a/click".into()],
    );
}

/// The Vive bindings of actions_vive_only.json.
fn default_vive_boolact() -> [String; 3] {
    [
        "/user/hand/left/input/menu/click".into(),
        "/user/hand/right/input/menu/click".into(),
        "/user/hand/right/input/trigger/click".into(),
    ]
}

/// Vivecraft loads its action manifest, then adds its application manifest and identifies as
/// its app. The bindings chosen for that app key replace the ones loaded before.
#[test]
fn bindings_chosen_for_key_identified_after_manifest() {
    const KEY: &str = "org.example.vivecraft";
    const OTHER_KEY: &str = "org.example.other";
    let steam = FakeSteam::new("identified");
    let url = format!("file://{}", input_data("vive_only_single.json"));
    steam.choose_for(
        KEY,
        &[("vive_controller_250820_CurrentURL_steamvrinput", &url)],
    );
    add_app_bindings(
        &steam.home.join(".config"),
        OTHER_KEY,
        "knuckles.json",
        "steamvr_workshop_knuckles.json",
    );
    let f = Fixture::new();
    f.input.openxr.restart_session().unwrap(); // get to real session
    f.load_actions(c"actions_vive_only.json");
    let boolact = f.get_action_handle(c"/actions/main/in/boolact");
    let action = || f.get_action::<bool>(boolact);
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        default_vive_boolact(),
    );

    f.input.app_identified(KEY);
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/trigger/click".into()],
    );
    let reloaded = action();
    f.input.app_identified(KEY);
    assert_eq!(action(), reloaded, "same key, nothing to reload");

    // An app changing its key gets the custom bindings in the other key's directory, and loses
    // the first key's choice.
    f.input.app_identified(OTHER_KEY);
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        default_vive_boolact(),
    );
    f.verify_bindings::<bool>(
        Knuckles::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/right/input/b/click".into()],
    );
}

/// Before the first frame, the session restart for the real session loads the bindings chosen for
/// the key.
#[test]
fn bindings_chosen_for_key_identified_before_real_session() {
    const KEY: &str = "org.example.vivecraft";
    let steam = FakeSteam::new("identified-early");
    let url = format!("file://{}", input_data("vive_only_single.json"));
    steam.choose_for(
        KEY,
        &[("vive_controller_250820_CurrentURL_steamvrinput", &url)],
    );
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");
    let boolact = f.get_action_handle(c"/actions/main/in/boolact");
    let loaded = f.get_action::<bool>(boolact);

    f.input.app_identified(KEY);
    assert_eq!(
        f.get_action::<bool>(boolact),
        loaded,
        "no restart before the real session"
    );
    f.input.openxr.restart_session().unwrap(); // get to real session
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/trigger/click".into()],
    );
}

/// A key nothing is chosen for keeps the bindings loaded.
#[test]
fn identified_key_without_bindings_keeps_them() {
    let steam = FakeSteam::new("identified-nothing");
    let url = format!("file://{}", input_data("vive_only_single.json"));
    steam.choose(&[("vive_controller_250820_CurrentURL_steamvrinput", &url)]);
    let f = Fixture::new();
    f.input.openxr.restart_session().unwrap(); // get to real session
    f.load_actions(c"actions_vive_only.json");
    let boolact = f.get_action_handle(c"/actions/main/in/boolact");
    let loaded = f.get_action::<bool>(boolact);

    // The Steam app id's choice still applies.
    f.input.app_identified("org.example.unknown");
    assert_eq!(f.get_action::<bool>(boolact), loaded);
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/trigger/click".into()],
    );
}

#[test]
fn steamvr_choice_disabled() {
    let steam = FakeSteam::new("disabled");
    let url = format!("file://{}", input_data("vive_only_single.json"));
    steam.choose(&[("vive_controller_250820_CurrentURL_steamvrinput", &url)]);
    set_test_env("XRIZER_STEAMVR_BINDINGS", "0");
    let f = Fixture::new();
    f.load_actions(c"actions_vive_only.json");

    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/menu/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
}

/// The binding variant `device` reports, and the result with a buffer that's too small for it.
fn binding_variant(f: &Fixture, device: vr::VRInputValueHandle_t) -> (String, vr::EVRInputError) {
    use vr::IVRInput011_Interface as Input;
    let mut buffer = [1 as std::ffi::c_char; 64];
    assert_eq!(
        Input::GetBindingVariant(&*f.input, device, buffer.as_mut_ptr(), buffer.len() as u32),
        vr::EVRInputError::None
    );
    let variant = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    let mut small = [1 as std::ffi::c_char; 1];
    let small_result = Input::GetBindingVariant(&*f.input, device, small.as_mut_ptr(), 1);
    assert_eq!(small[0], 0);
    (variant, small_result)
}

fn sync_main_set(f: &mut Fixture) {
    let set = f.get_action_set_handle(c"/actions/main");
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set,
        ..Default::default()
    });
}

#[test]
fn binding_variant_of_the_first_bindings() {
    let mut f = Fixture::new();
    f.load_actions(c"actions_variants.json");
    f.set_interaction_profile::<ViveWands>(LeftHand);
    sync_main_set(&mut f);

    // Like SteamVR, the first file listed is the default, and it has no variant.
    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        [
            "/user/hand/left/input/menu/click".into(),
            "/user/hand/right/input/menu/click".into(),
            "/user/hand/right/input/trigger/click".into(),
        ],
    );
    let left = f.get_input_source_handle(c"/user/hand/left");
    assert_eq!(
        binding_variant(&f, left),
        (String::new(), vr::EVRInputError::None)
    );
}

#[test]
fn binding_variant_chosen_in_steamvr() {
    let steam = FakeSteam::new("variant");
    let url = format!("file://{}", input_data("vive_only_single.json"));
    steam.choose(&[("vive_controller_250820_CurrentURL_steamvrinput", &url)]);
    let mut f = Fixture::new();
    f.load_actions(c"actions_variants.json");
    f.set_interaction_profile::<ViveWands>(LeftHand);
    f.set_interaction_profile::<Knuckles>(RightHand);
    sync_main_set(&mut f);

    f.verify_bindings::<bool>(
        ViveWands::profile_path(),
        c"/actions/main/in/boolact",
        ["/user/hand/left/input/trigger/click".into()],
    );
    let left = f.get_input_source_handle(c"/user/hand/left");
    let single = (
        "singlecontroller".to_string(),
        vr::EVRInputError::BufferTooSmall,
    );
    assert_eq!(binding_variant(&f, left), single);
    // Bindings converted from them are the same variant.
    let right = f.get_input_source_handle(c"/user/hand/right/input/trigger");
    assert_eq!(binding_variant(&f, right), single);
    assert_eq!(binding_variant(&f, vr::k_ulInvalidInputValueHandle), single);
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

/// Like SteamVR, pose actions are only active for a hand that is there.
#[test]
fn pose_action_inactive_without_its_device() {
    fakexr::set_hand_tracking_supported(true);
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let pose = f.get_action_handle(c"/actions/set1/in/pose");
    let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
    let left = f.get_input_source_handle(c"/user/hand/left");
    let right = f.get_input_source_handle(c"/user/hand/right");
    f.load_actions(c"actions.json");
    // fakexr can't locate in the stage space.
    f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
        // drop cached poses and hand skeletons
        f.input.frame_start_update();
    };
    #[track_caller]
    fn active(f: &Fixture, action: vr::VRActionHandle_t, restrict: u64) -> (bool, u64) {
        let data = f.get_pose(action, restrict).unwrap();
        (data.bActive, data.activeOrigin)
    }
    let inactive = (false, vr::k_ulInvalidInputValueHandle);

    // No controllers or hands yet.
    sync(&mut f);
    for restrict in [0, left, right] {
        assert_eq!(active(&f, pose, restrict), inactive, "restrict {restrict}");
    }
    assert_eq!(active(&f, skeleton, 0), inactive);

    // A tracked hand without an interaction profile.
    fakexr::set_hand_tracked(
        f.raw_session(),
        LeftHand,
        Some(xr::HandTrackingDataSourceEXT::UNOBSTRUCTED),
    );
    sync(&mut f);
    assert_eq!(active(&f, pose, left), (true, left));
    assert_eq!(active(&f, pose, right), inactive);
    assert_eq!(active(&f, skeleton, 0), (true, left));
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, None);

    // Only the left controller, whose bindings are used for the right hand too. The origin is
    // the bound pose source.
    let left_raw = f.get_input_source_handle(c"/user/hand/left/pose/raw");
    let right_raw = f.get_input_source_handle(c"/user/hand/right/pose/raw");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    sync(&mut f);
    assert_eq!(active(&f, pose, left), (true, left_raw));
    assert_eq!(active(&f, pose, right), inactive);
    assert_eq!(active(&f, pose, 0), (true, left_raw));

    // Only the right one: unrestricted reads use it.
    fakexr::set_interaction_profile(f.raw_session(), LeftHand, xr::Path::NULL);
    f.set_interaction_profile::<OculusTouch>(RightHand);
    sync(&mut f);
    f.input.expire_connection_grace();
    assert_eq!(active(&f, pose, left), inactive);
    assert_eq!(active(&f, pose, right), (true, right_raw));
    assert_eq!(active(&f, pose, 0), (true, right_raw));
    assert_eq!(active(&f, skeleton, 0), inactive);
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
// A thread blocking on a parking_lot lock trips Miri: parking_lot_core 0.9 passes a reference
// where the futex syscall takes a pointer.
#[cfg_attr(miri, ignore)]
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
    let mut origins = [vr::k_ulInvalidInputValueHandle; vr::k_unMaxActionOriginCount as usize];
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
    // The device, not the input, which Unity looks up to find the hand.
    assert_ne!(origins[0], f.get_input_source_handle(c"/user/hand/left"));
    assert_eq!(
        info.devicePath,
        f.get_input_source_handle(c"/user/hand/left")
    );
    let proximity = f.get_input_source_handle(c"/user/head/proximity");
    assert_eq!(
        get_origin_info(&f, proximity).devicePath,
        f.get_input_source_handle(c"/user/head")
    );

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
    assert_eq!(buf_str(&name), "Left Hand Oculus Touch Trigger");

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

    assert_eq!(
        binding_info_rows(&info),
        [[
            "/user/hand/left",
            "/input/trackpad",
            "dpad",
            "north",
            "trackpad"
        ]]
    );

    let origins = get_action_origins(&f, set1, action);
    assert!(!origins.is_empty());

    let origin_info = get_origin_info(&f, origins[0]);
    assert_eq!(
        buf_str(&origin_info.rchRenderModelComponentName),
        "trackpad"
    );
}

/// Device, input, mode, slot and input type of each binding.
fn binding_info_rows(info: &[vr::InputBindingInfo_t]) -> Vec<[String; 5]> {
    info.iter()
        .map(|info| {
            [
                buf_str(&info.rchDevicePathName),
                buf_str(&info.rchInputPathName),
                buf_str(&info.rchModeName),
                buf_str(&info.rchSlotName),
                buf_str(&info.rchInputSourceType),
            ]
        })
        .collect()
}

/// Loads `actions`, makes both controllers `P` and syncs `set`.
fn fixture_with<P: InteractionProfile>(actions: &CStr, set: &CStr) -> Fixture {
    let mut f = Fixture::new();
    let set = f.get_action_set_handle(set);
    f.load_actions(actions);
    f.set_interaction_profile::<P>(LeftHand);
    f.set_interaction_profile::<P>(RightHand);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set,
        ..Default::default()
    });
    f
}

#[track_caller]
fn binding_info_of<P: InteractionProfile>(
    actions: &CStr,
    set: &CStr,
    action: &CStr,
) -> Vec<[String; 5]> {
    let f = fixture_with::<P>(actions, set);
    let action = f.get_action_handle(action);
    binding_info_rows(&get_binding_info(&f, action))
}

#[test]
fn binding_info_modes() {
    let set1 = c"/actions/set1";
    let main = c"/actions/main";
    let touch = |action| binding_info_of::<OculusTouch>(c"actions.json", set1, action);

    assert_eq!(
        touch(c"/actions/set1/in/boolact"),
        [
            ["/user/hand/left", "/input/x", "button", "click", "button"],
            ["/user/hand/left", "/input/y", "button", "click", "button"],
            ["/user/hand/right", "/input/a", "button", "click", "button"],
            ["/user/hand/right", "/input/b", "button", "click", "button"],
            // The Touch's left menu button is its system button in SteamVR.
            [
                "/user/hand/left",
                "/input/system",
                "button",
                "click",
                "button"
            ],
            [
                "/user/hand/left",
                "/input/trigger",
                "trigger",
                "click",
                "trigger"
            ],
            [
                "/user/hand/left",
                "/input/thumbrest",
                "button",
                "touch",
                "button"
            ],
            [
                "/user/hand/left",
                "/input/grip",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/right",
                "/input/grip",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/left",
                "/input/trigger",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/right",
                "/input/trigger",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/right",
                "/input/joystick",
                "joystick",
                "click",
                "joystick"
            ],
            [
                "/user/hand/right",
                "/input/joystick",
                "joystick",
                "touch",
                "joystick"
            ],
        ]
    );
    assert_eq!(
        touch(c"/actions/set1/in/vec1act"),
        [
            [
                "/user/hand/left",
                "/input/trigger",
                "trigger",
                "pull",
                "trigger"
            ],
            [
                "/user/hand/right",
                "/input/trigger",
                "trigger",
                "pull",
                "trigger"
            ],
        ]
    );
    assert_eq!(
        touch(c"/actions/set1/in/vec2act"),
        [
            [
                "/user/hand/right",
                "/input/joystick",
                "joystick",
                "position",
                "joystick"
            ],
            [
                "/user/hand/left",
                "/input/joystick",
                "joystick",
                "position",
                "joystick"
            ],
        ]
    );
    assert_eq!(
        touch(c"/actions/set1/in/vib"),
        [
            ["/user/hand/left", "/output/haptic", "", "", "vibration"],
            ["/user/hand/right", "/output/haptic", "", "", "vibration"],
        ]
    );
    // The Vive's grip is a button.
    assert_eq!(
        binding_info_of::<ViveWands>(c"actions.json", set1, c"/actions/set1/in/boolact"),
        [
            [
                "/user/hand/left",
                "/input/grip",
                "button",
                "click",
                "button"
            ],
            [
                "/user/hand/right",
                "/input/grip",
                "button",
                "click",
                "button"
            ],
            [
                "/user/hand/left",
                "/input/application_menu",
                "button",
                "click",
                "button"
            ],
            [
                "/user/hand/right",
                "/input/application_menu",
                "button",
                "click",
                "button"
            ],
            [
                "/user/hand/left",
                "/input/trigger",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/right",
                "/input/trigger",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/left",
                "/input/trackpad",
                "trackpad",
                "click",
                "trackpad"
            ],
            [
                "/user/hand/left",
                "/input/trackpad",
                "trackpad",
                "touch",
                "trackpad"
            ],
        ]
    );

    assert_eq!(
        binding_info_of::<Knuckles>(c"actions_scroll.json", main, c"/actions/main/in/scroll_pad"),
        [[
            "/user/hand/left",
            "/input/trackpad",
            "scroll",
            "scroll",
            "trackpad"
        ]]
    );
    assert_eq!(
        binding_info_of::<Knuckles>(
            c"actions_scroll.json",
            main,
            c"/actions/main/in/scroll_stick"
        ),
        [[
            "/user/hand/right",
            "/input/thumbstick",
            "scroll",
            "scroll",
            "joystick"
        ]]
    );
    assert_eq!(
        binding_info_of::<Knuckles>(c"actions_scroll.json", main, c"/actions/main/in/move"),
        [[
            "/user/hand/left",
            "/input/thumbstick",
            "joystick",
            "position",
            "joystick"
        ]]
    );
    assert_eq!(
        binding_info_of::<OculusTouch>(c"actions_grab_mode.json", set1, c"/actions/set1/in/grab"),
        [["/user/hand/left", "/input/grip", "grab", "grab", "trigger"]]
    );
    assert_eq!(
        binding_info_of::<Knuckles>(c"actions_toggle.json", set1, c"/actions/set1/in/boolact"),
        [
            [
                "/user/hand/left",
                "/input/a",
                "toggle_button",
                "click",
                "button"
            ],
            [
                "/user/hand/right",
                "/input/a",
                "toggle_button",
                "click",
                "button"
            ],
        ]
    );
    let scalar =
        |action| binding_info_of::<OculusTouch>(c"actions_scalar_constant.json", main, action);
    assert_eq!(
        scalar(c"/actions/main/in/alternateuse"),
        [[
            "/user/hand/right",
            "/input/a",
            "scalar_constant",
            "value",
            "button"
        ]]
    );
    // Values other than 1 are bound too.
    assert_eq!(
        scalar(c"/actions/main/in/reverse"),
        [[
            "/user/hand/left",
            "/input/x",
            "scalar_constant",
            "value",
            "button"
        ]]
    );
}

#[test]
fn binding_info_chords() {
    let chord =
        |action| binding_info_of::<Knuckles>(c"actions_chords.json", c"/actions/main", action);
    assert_eq!(
        chord(c"/actions/main/in/dodge"),
        [
            [
                "/user/hand/left",
                "/input/grip",
                "button",
                "click",
                "trigger"
            ],
            ["/user/hand/right", "/input/a", "button", "click", "button"],
        ]
    );
    assert_eq!(
        chord(c"/actions/main/in/facepiece"),
        [
            [
                "/user/hand/right",
                "/input/grip",
                "button",
                "click",
                "trigger"
            ],
            [
                "/user/hand/right",
                "/input/thumbstick",
                "dpad",
                "south",
                "joystick"
            ],
        ]
    );
    // Ignored chords have no bindings.
    assert!(chord(c"/actions/main/in/sit").is_empty());
    assert!(chord(c"/actions/main/in/bad").is_empty());
}

#[test]
fn binding_info_of_converted_bindings() {
    // Vive wand bindings on the Touch report the Touch's inputs.
    let vive_only = |action| {
        binding_info_of::<OculusTouch>(c"actions_vive_only.json", c"/actions/main", action)
    };
    assert_eq!(
        vive_only(c"/actions/main/in/vec2act"),
        [[
            "/user/hand/left",
            "/input/joystick",
            "trackpad",
            "position",
            "joystick"
        ]]
    );
    assert_eq!(
        vive_only(c"/actions/main/in/gripact"),
        [[
            "/user/hand/left",
            "/input/grip",
            "button",
            "click",
            "trigger"
        ]]
    );
}

/// GetOriginLocalizedName's result and name.
fn origin_name(
    f: &Fixture,
    origin: vr::VRInputValueHandle_t,
    sections: i32,
) -> (vr::EVRInputError, String) {
    let mut name = [0x55 as std::ffi::c_char; 256];
    let err = vr::IVRInput011_Interface::GetOriginLocalizedName(
        f.input.as_ref(),
        origin,
        name.as_mut_ptr(),
        name.len() as u32,
        sections,
    );
    (err, buf_str(&name))
}

#[track_caller]
fn names_on<P: InteractionProfile>(paths: &[(&CStr, &str)]) {
    let f = fixture_with::<P>(c"actions.json", c"/actions/set1");
    for (path, expected) in paths {
        let origin = f.get_input_source_handle(path);
        assert_eq!(
            origin_name(&f, origin, -1),
            (vr::EVRInputError::None, expected.to_string()),
            "{path:?} on {}",
            P::profile_path()
        );
    }
}

#[test]
fn origin_names_per_controller() {
    use crate::input::profiles::{
        meta_touch_plus::MetaTouchPlus,
        pico::Pico4,
        vive_cosmos::ViveCosmos,
        vive_focus3::ViveFocus3,
        wmr::{
            hp_motion_controller::ReverbG2Controller, ms_motion_controller::HolographicController,
        },
    };

    names_on::<Knuckles>(&[
        (
            c"/user/hand/left/input/thumbstick",
            "Left Hand Index Controller Thumb Stick",
        ),
        (
            c"/user/hand/right/input/a",
            "Right Hand Index Controller A Button",
        ),
        (
            c"/user/hand/right/input/b/click",
            "Right Hand Index Controller B Button",
        ),
        (
            c"/user/hand/left/input/trigger",
            "Left Hand Index Controller Trigger",
        ),
        (
            c"/user/hand/left/input/trackpad",
            "Left Hand Index Controller Trackpad",
        ),
        (
            c"/user/hand/left/input/grip",
            "Left Hand Index Controller Grip",
        ),
        (
            c"/user/hand/left/input/system",
            "Left Hand Index Controller System",
        ),
        (
            c"/user/hand/left/input/finger/index",
            "Left Hand Index Controller Index Finger",
        ),
        (
            c"/user/hand/left/input/pinch",
            "Left Hand Index Controller Pinch Gesture",
        ),
    ]);
    names_on::<OculusTouch>(&[
        (
            c"/user/hand/left/input/joystick",
            "Left Hand Oculus Touch Joystick",
        ),
        (c"/user/hand/left/input/grip", "Left Hand Oculus Touch Grip"),
        (
            c"/user/hand/left/input/x",
            "Left Hand Oculus Touch X Button",
        ),
        (
            c"/user/hand/right/input/b",
            "Right Hand Oculus Touch B Button",
        ),
        (
            c"/user/hand/left/input/system",
            "Left Hand Oculus Touch System",
        ),
        (
            c"/user/hand/left/input/thumbrest",
            "Left Hand Oculus Touch Thumbrest",
        ),
        // OpenXR's spelling.
        (
            c"/user/hand/left/input/squeeze/value",
            "Left Hand Oculus Touch Grip",
        ),
        (
            c"/user/hand/right/output/haptic",
            "Right Hand Oculus Touch Haptics",
        ),
    ]);
    names_on::<Pico4>(&[(
        c"/user/hand/left/input/joystick",
        "Left Hand Pico Controller Joystick",
    )]);
    names_on::<ViveCosmos>(&[(
        c"/user/hand/right/input/bumper",
        "Right Hand Vive Cosmos Controller Bumper Button",
    )]);
    names_on::<MetaTouchPlus>(&[(
        c"/user/hand/left/input/trigger",
        "Left Hand Oculus Touch Trigger",
    )]);
    names_on::<ViveWands>(&[
        (
            c"/user/hand/left/input/trackpad",
            "Left Hand Vive Controller Trackpad",
        ),
        (
            c"/user/hand/left/input/trigger",
            "Left Hand Vive Controller Trigger",
        ),
        (
            c"/user/hand/left/input/grip",
            "Left Hand Vive Controller Grip",
        ),
        (
            c"/user/hand/left/input/application_menu",
            "Left Hand Vive Controller Menu",
        ),
        (
            c"/user/hand/left/input/system",
            "Left Hand Vive Controller System",
        ),
    ]);
    names_on::<FrameController>(&[
        (
            c"/user/hand/left/input/system",
            "Left Hand Frame Controller Steam Button",
        ),
        (
            c"/user/hand/left/input/thumbstick",
            "Left Hand Frame Controller Thumb Stick",
        ),
        (
            c"/user/hand/left/input/dpad_up",
            "Left Hand Frame Controller Dpad Up",
        ),
        (
            c"/user/hand/right/input/bumper",
            "Right Hand Frame Controller Bumper",
        ),
    ]);
    names_on::<HolographicController>(&[
        (
            c"/user/hand/left/input/joystick",
            "Left Hand Windows Mixed Reality Controller Joystick",
        ),
        (
            c"/user/hand/left/input/application_menu",
            "Left Hand Windows Mixed Reality Controller Menu",
        ),
    ]);
    names_on::<ReverbG2Controller>(&[(
        c"/user/hand/right/input/a",
        "Right Hand HP Motion Controller A Button",
    )]);
    names_on::<ViveFocus3>(&[(
        c"/user/hand/left/input/joystick",
        "Left Hand Vive Focus 3 Controller Joystick",
    )]);
    // Inputs and controllers SteamVR has no name for.
    names_on::<OculusTouch>(&[(
        c"/user/hand/left/input/pinch_ext/value",
        "Left Hand Oculus Touch Pinch Ext",
    )]);
    names_on::<SimpleController>(&[(
        c"/user/hand/left/input/select/click",
        "Left Hand Controller Select",
    )]);
}

#[test]
fn origin_name_sections() {
    use vr::EVRInputError::*;
    let f = fixture_with::<OculusTouch>(c"actions.json", c"/actions/set1");
    let trigger = f.get_input_source_handle(c"/user/hand/left/input/trigger");
    for (sections, expected) in [
        (0x1, "Left Hand"),
        (0x2, "Oculus Touch"),
        (0x3, "Left Hand Oculus Touch"),
        (0x4, "Trigger"),
        (0x5, "Left Hand Trigger"),
        (0x6, "Oculus Touch Trigger"),
        (0x7, "Left Hand Oculus Touch Trigger"),
        (-1, "Left Hand Oculus Touch Trigger"),
        // Unknown bits are ignored.
        (0x9, "Left Hand"),
    ] {
        assert_eq!(
            origin_name(&f, trigger, sections),
            (None, expected.to_owned()),
            "0x{sections:x}"
        );
    }
    // No sections.
    assert_eq!(origin_name(&f, trigger, 0), (InvalidParam, String::new()));
    assert_eq!(origin_name(&f, trigger, 0x8), (InvalidParam, String::new()));

    // A whole hand has no input.
    let left = f.get_input_source_handle(c"/user/hand/left");
    assert_eq!(
        origin_name(&f, left, -1),
        (None, "Left Hand Oculus Touch".into())
    );
    assert_eq!(origin_name(&f, left, 0x4), (None, "Left Hand".into()));

    // The headset has no hand.
    let head = f.get_input_source_handle(c"/user/head");
    assert_eq!(origin_name(&f, head, -1), (None, "Oculus Headset".into()));
    let proximity = f.get_input_source_handle(c"/user/head/proximity");
    assert_eq!(
        origin_name(&f, proximity, -1),
        (None, "Oculus Headset Proximity Sensor".into())
    );
    assert_eq!(
        origin_name(&f, proximity, 0x1),
        (None, "Oculus Headset".into())
    );

    assert_eq!(origin_name(&f, 0, -1), (InvalidHandle, String::new()));
    assert_eq!(
        origin_name(&f, 0xdead_beef, -1),
        (InvalidHandle, String::new())
    );
    let treadmill = f.get_input_source_handle(c"/user/treadmill/input/a");
    assert_eq!(
        origin_name(&f, treadmill, -1),
        (InvalidDevice, String::new())
    );
    // The gamepad has no tracked device, but SteamVR's driver names it and its inputs.
    let gamepad = f.get_input_source_handle(c"/user/gamepad/input/a");
    assert_eq!(
        origin_name(&f, gamepad, -1),
        (None, "Gamepad A Button".into())
    );
    let stick = f.get_input_source_handle(c"/user/gamepad/input/joystick_left");
    assert_eq!(
        origin_name(&f, stick, -1),
        (None, "Gamepad Left Thumbstick".into())
    );
}

#[test]
fn origin_name_buffer() {
    use vr::EVRInputError::*;
    let f = fixture_with::<OculusTouch>(c"actions.json", c"/actions/set1");
    let trigger = f.get_input_source_handle(c"/user/hand/left/input/trigger");
    let get = |size: usize| {
        let mut name = vec![0x55 as std::ffi::c_char; size + 1];
        let err = vr::IVRInput011_Interface::GetOriginLocalizedName(
            f.input.as_ref(),
            trigger,
            name.as_mut_ptr(),
            size as u32,
            -1,
        );
        // Nothing is written past the buffer.
        assert_eq!(name[size], 0x55);
        let name = (size > 0 && name[size - 1] != 0x55).then(|| buf_str(&name));
        (err, name)
    };

    let full = "Left Hand Oculus Touch Trigger";
    assert_eq!(get(full.len() + 1), (None, Some(full.into())));
    assert_eq!(
        get(full.len()),
        (BufferTooSmall, Some(full[..full.len() - 1].into()))
    );
    assert_eq!(get(10), (BufferTooSmall, Some("Left Hand".into())));
    assert_eq!(get(1), (BufferTooSmall, Some(String::new())));
    assert_eq!(get(0), (InvalidParam, Option::None));
    assert_eq!(
        vr::IVRInput011_Interface::GetOriginLocalizedName(
            f.input.as_ref(),
            trigger,
            std::ptr::null_mut(),
            64,
            -1
        ),
        InvalidParam
    );
}

#[track_caller]
fn origin_path(f: &Fixture, origin: vr::VRInputValueHandle_t) -> String {
    f.input.input_source_map.read().unwrap()[super::InputSourceKey::from(KeyData::from_ffi(origin))]
        .to_string_lossy()
        .into_owned()
}

#[test]
fn action_origins_are_openvr_inputs() {
    let f = fixture_with::<OculusTouch>(c"actions.json", c"/actions/set1");
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let origins_of = |action| {
        let action = f.get_action_handle(action);
        get_action_origins(&f, set1, action)
            .into_iter()
            .map(|origin| {
                let info = get_origin_info(&f, origin);
                (
                    origin_path(&f, origin),
                    origin_name(&f, origin, -1).1,
                    buf_str(&info.rchRenderModelComponentName),
                )
            })
            .collect::<Vec<_>>()
    };
    let owned = |list: &[(&str, &str, &str)]| {
        list.iter()
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
            .collect::<Vec<_>>()
    };

    // Bound to the grip's value in OpenXR, on one hand only.
    assert_eq!(
        origins_of(c"/actions/set1/in/boolact2"),
        owned(&[(
            "/user/hand/left/input/grip",
            "Left Hand Oculus Touch Grip",
            "button_grip"
        )])
    );
    // Bound to /input/thumbstick, which SteamVR's Touch calls a joystick.
    assert_eq!(
        origins_of(c"/actions/set1/in/vec2act"),
        owned(&[
            (
                "/user/hand/left/input/joystick",
                "Left Hand Oculus Touch Joystick",
                "thumbstick"
            ),
            (
                "/user/hand/right/input/joystick",
                "Right Hand Oculus Touch Joystick",
                "thumbstick"
            ),
        ])
    );
    // Every bound input, left hand first.
    assert_eq!(
        origins_of(c"/actions/set1/in/boolact"),
        owned(&[
            (
                "/user/hand/left/input/x",
                "Left Hand Oculus Touch X Button",
                "button_x"
            ),
            (
                "/user/hand/left/input/y",
                "Left Hand Oculus Touch Y Button",
                "button_y"
            ),
            (
                "/user/hand/left/input/system",
                "Left Hand Oculus Touch System",
                "button_enter"
            ),
            (
                "/user/hand/left/input/trigger",
                "Left Hand Oculus Touch Trigger",
                "trigger"
            ),
            (
                "/user/hand/left/input/thumbrest",
                "Left Hand Oculus Touch Thumbrest",
                "thumbrest"
            ),
            (
                "/user/hand/left/input/grip",
                "Left Hand Oculus Touch Grip",
                "button_grip"
            ),
            (
                "/user/hand/right/input/a",
                "Right Hand Oculus Touch A Button",
                "button_a"
            ),
            (
                "/user/hand/right/input/b",
                "Right Hand Oculus Touch B Button",
                "button_b"
            ),
            (
                "/user/hand/right/input/grip",
                "Right Hand Oculus Touch Grip",
                "button_grip"
            ),
            (
                "/user/hand/right/input/trigger",
                "Right Hand Oculus Touch Trigger",
                "trigger"
            ),
            (
                "/user/hand/right/input/joystick",
                "Right Hand Oculus Touch Joystick",
                "thumbstick"
            ),
        ])
    );

    // Like SteamVR, a buffer too small for them all fails.
    let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
    let mut origins = [vr::k_ulInvalidInputValueHandle; 4];
    assert_eq!(
        vr::IVRInput011_Interface::GetActionOrigins(
            f.input.as_ref(),
            set1,
            boolact,
            origins.as_mut_ptr(),
            origins.len() as u32,
        ),
        vr::EVRInputError::BufferTooSmall
    );
    assert_eq!(
        origins.to_vec(),
        get_action_origins(&f, set1, boolact)[..4].to_vec()
    );
}

#[test]
fn origin_render_model_components() {
    let components = |f: &Fixture, paths: &[&CStr]| {
        paths
            .iter()
            .map(|path| {
                let origin = f.get_input_source_handle(path);
                buf_str(&get_origin_info(f, origin).rchRenderModelComponentName)
            })
            .collect::<Vec<_>>()
    };

    let f = fixture_with::<Knuckles>(c"actions.json", c"/actions/set1");
    assert_eq!(
        components(
            &f,
            &[
                c"/user/hand/left/input/grip",
                c"/user/hand/left/input/thumbstick",
                c"/user/hand/right/input/a",
                c"/user/hand/right/input/system",
                c"/user/hand/right/input/trackpad",
            ]
        ),
        [
            "squeeze",
            "thumbstick",
            "button_a",
            "sys_button",
            "trackpad"
        ]
    );
    let f = fixture_with::<ViveWands>(c"actions.json", c"/actions/set1");
    assert_eq!(
        components(
            &f,
            &[
                c"/user/hand/left/input/grip",
                c"/user/hand/left/input/application_menu",
                c"/user/hand/left/input/system",
            ]
        ),
        ["lgrip", "button", "sys_button"]
    );
    // Models SteamVR has no components for keep the input's name.
    let f = fixture_with::<SimpleController>(c"actions.json", c"/actions/set1");
    assert_eq!(
        components(&f, &[c"/user/hand/left/input/select"]),
        ["select"]
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

    // samples from another tracking origin are in a different space
    let other_origin = space.update_history_and_velocity(
        60_000_000,
        Vec3::ZERO,
        vr::ETrackingUniverseOrigin::Seated,
    );
    assert!(other_origin.is_none());

    // but querying another origin doesn't discard this origin's samples
    let v3 = space
        .update_history_and_velocity(60_000_000, Vec3::new(0.06, 0.0, 0.0), origin)
        .unwrap();
    assert!((v3 - Vec3::X).length() < 1e-3, "{v3:?}");

    // a sample from seconds ago says nothing about the current velocity
    let after_pause = space.update_history_and_velocity(
        3_060_000_000,
        Vec3::ONE,
        vr::ETrackingUniverseOrigin::Seated,
    );
    assert!(after_pause.is_none());
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
    let action = f.get_action::<bool>(boolact);
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        })
    };
    // OpenXR combines the inputs bound directly, so it's the first one on the hand.
    let origin = |f: &Fixture| origin_path(f, f.get_bool_state(boolact).unwrap().activeOrigin);
    let set = |hand, pressed: bool, secs: i64| {
        let time = xr::Time::from_nanos(secs * 1_000_000_000);
        fakexr::set_action_state_with_time(action, pressed.into(), hand, time);
    };

    sync(&mut f);
    let state = f.get_bool_state(boolact).unwrap();
    assert!(!state.bActive);
    assert_eq!(state.activeOrigin, 0);

    set(RightHand, true, 1);
    sync(&mut f);
    let state = f.get_bool_state(boolact).unwrap();
    assert!(state.bState);
    assert_eq!(origin(&f), "/user/hand/right/input/a");
    let info = get_origin_info(&f, state.activeOrigin);
    assert_eq!(info.trackedDeviceIndex, Hand::Right as u32);
    assert_eq!(
        info.devicePath,
        f.get_input_source_handle(c"/user/hand/right")
    );

    // Active but not pressed still has an origin: the input released last.
    set(RightHand, false, 2);
    sync(&mut f);
    sync(&mut f);
    let state = f.get_bool_state(boolact).unwrap();
    assert!(state.bActive && !state.bState);
    assert_eq!(origin(&f), "/user/hand/right/input/a");

    set(LeftHand, true, 3);
    sync(&mut f);
    assert_eq!(origin(&f), "/user/hand/left/input/a");

    // Left wins ties.
    set(LeftHand, false, 4);
    sync(&mut f);
    set(LeftHand, true, 5);
    set(RightHand, true, 5);
    sync(&mut f);
    assert_eq!(origin(&f), "/user/hand/left/input/a");

    // A read restricted to a hand reports that hand's input.
    let right = f.get_input_source_handle(c"/user/hand/right");
    let state = f.get_bool_state_hand(boolact, right).unwrap();
    assert_eq!(
        origin_path(&f, state.activeOrigin),
        "/user/hand/right/input/a"
    );
}

#[test]
fn action_data_origin_is_the_driving_input() {
    let mut f = fixture_with::<OculusTouch>(c"actions_origins.json", c"/actions/main");
    let main = f.get_action_set_handle(c"/actions/main");
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: main,
            ..Default::default()
        })
    };
    let fire = f.get_action_handle(c"/actions/main/in/fire");
    let throttle = f.get_action_handle(c"/actions/main/in/throttle");
    let move_ = f.get_action_handle(c"/actions/main/in/move");
    let pose = f.get_action_handle(c"/actions/main/in/pose");
    let [left, right] =
        [c"/user/hand/left", c"/user/hand/right"].map(|hand| f.get_input_source_handle(hand));
    // Button mode on the analog trigger is a threshold xrizer evaluates.
    let trigger_click = f.custom_binding_action(fire, OculusTouch::profile_path(), |ty| {
        matches!(ty, BoolBindingType::ThresholdFloat(_))
    });
    let fire_xr = f.get_action::<bool>(fire);
    let throttle_xr = f.get_action::<f32>(throttle);
    let move_xr = f.get_action::<xr::Vector2f>(move_);
    let fire_origin = |f: &Fixture| {
        let state = f.get_bool_state(fire).unwrap();
        (state.bState, origin_path(f, state.activeOrigin))
    };
    let named = |f: &Fixture, origin| origin_name(f, origin, -1).1;
    let at = |secs: i64| xr::Time::from_nanos(secs * 1_000_000_000);
    let trigger = |value, secs| {
        let value = fakexr::ActionState::Float(value);
        fakexr::set_action_state_with_time(trigger_click, value, RightHand, at(secs));
    };

    // Nothing changed yet: the first hand's first input.
    for hand in [LeftHand, RightHand] {
        fakexr::set_action_state(fire_xr, false.into(), hand);
    }
    trigger(0.0, 0);
    sync(&mut f);
    sync(&mut f);
    assert_eq!(fire_origin(&f), (false, "/user/hand/left/input/x".into()));

    // The trigger's threshold drives it, and Unity names the origin after it.
    trigger(1.0, 1);
    sync(&mut f);
    assert_eq!(
        fire_origin(&f),
        (true, "/user/hand/right/input/trigger".into())
    );
    let state = f.get_bool_state(fire).unwrap();
    assert_eq!(
        named(&f, state.activeOrigin),
        "Right Hand Oculus Touch Trigger"
    );
    let info = get_origin_info(&f, state.activeOrigin);
    assert_eq!(buf_str(&info.rchRenderModelComponentName), "trigger");
    assert_eq!(info.devicePath, right);
    // An origin action data reports is one GetActionOrigins lists.
    assert!(get_action_origins(&f, main, fire).contains(&state.activeOrigin));

    // Released, it stays the origin until another input changes.
    trigger(0.0, 2);
    sync(&mut f);
    sync(&mut f);
    assert_eq!(
        fire_origin(&f),
        (false, "/user/hand/right/input/trigger".into())
    );

    // A pressed button wins over a released threshold.
    fakexr::set_action_state_with_time(fire_xr, true.into(), RightHand, at(3));
    sync(&mut f);
    assert_eq!(fire_origin(&f), (true, "/user/hand/right/input/a".into()));
    let state = f.get_bool_state_hand(fire, left).unwrap();
    assert_eq!(
        origin_path(&f, state.activeOrigin),
        "/user/hand/left/input/x"
    );

    // Analog actions.
    fakexr::set_action_state(throttle_xr, fakexr::ActionState::Float(0.5), RightHand);
    fakexr::set_action_state(move_xr, fakexr::ActionState::Vector2(0.0, 1.0), RightHand);
    sync(&mut f);
    let state = get_analog_state(&f, throttle, 0);
    assert_eq!(
        origin_path(&f, state.activeOrigin),
        "/user/hand/right/input/trigger"
    );
    let state = get_analog_state(&f, move_, right);
    assert_eq!(
        origin_path(&f, state.activeOrigin),
        "/user/hand/right/input/joystick"
    );
    assert_eq!(
        named(&f, state.activeOrigin),
        "Right Hand Oculus Touch Joystick"
    );

    // Poses report their pose source.
    f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    f.input.frame_start_update();
    for (restrict, expected, name, hand) in [
        (
            left,
            "/user/hand/left/pose/raw",
            "Left Hand Oculus Touch Raw",
            left,
        ),
        (
            right,
            "/user/hand/right/pose/tip",
            "Right Hand Oculus Touch Tip",
            right,
        ),
        (
            0,
            "/user/hand/left/pose/raw",
            "Left Hand Oculus Touch Raw",
            left,
        ),
    ] {
        let data = f.get_pose(pose, restrict).unwrap();
        assert!(data.bActive);
        assert_eq!(origin_path(&f, data.activeOrigin), expected);
        assert_eq!(named(&f, data.activeOrigin), name);
        assert_eq!(get_origin_info(&f, data.activeOrigin).devicePath, hand);
    }
    let tip = f.get_pose(pose, right).unwrap().activeOrigin;
    assert_eq!(
        buf_str(&get_origin_info(&f, tip).rchRenderModelComponentName),
        "tip"
    );
    let origins = get_action_origins(&f, main, pose);
    assert_eq!(
        origins
            .iter()
            .map(|origin| origin_path(&f, *origin))
            .collect::<Vec<_>>(),
        ["/user/hand/left/pose/raw", "/user/hand/right/pose/tip"]
    );
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

    // Without XR_KHR_convert_timespec_time (fakexr has none), now is the display time.
    f.input
        .openxr
        .set_display_time(xr::Time::from_nanos(10_000_000_000));
    let time_from_now = |seconds| f.input.openxr.time_from_now(seconds).as_nanos();
    assert_eq!(time_from_now(-0.25), 9_750_000_000);
    assert_eq!(time_from_now(f32::NAN), 10_000_000_000);
    assert_eq!(time_from_now(f32::NEG_INFINITY), 10_000_000_000);
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
    f.input.openxr.set_display_time(secs(10.0));

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
fn analog_action_update_time() {
    let mut f = Fixture::new();
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let vec1 = f.get_action_handle(c"/actions/set1/in/vec1act");
    let vec2 = f.get_action_handle(c"/actions/set1/in/vec2act");
    f.load_actions(c"actions.json");
    let left = f.get_input_source_handle(c"/user/hand/left");
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        })
    };
    let secs = |secs: f64| xr::Time::from_nanos((secs * 1e9) as i64);
    f.input.openxr.set_display_time(secs(10.0));

    fakexr::set_action_state_with_time(
        f.get_action::<f32>(vec1),
        fakexr::ActionState::Float(0.5),
        LeftHand,
        secs(9.75),
    );
    fakexr::set_action_state_with_time(
        f.get_action::<xr::Vector2f>(vec2),
        fakexr::ActionState::Vector2(0.5, 0.5),
        LeftHand,
        secs(9.5),
    );
    sync(&mut f);
    for restrict in [left, 0] {
        assert_eq!(get_analog_state(&f, vec1, restrict).fUpdateTime, -0.25);
        assert_eq!(get_analog_state(&f, vec2, restrict).fUpdateTime, -0.5);
    }

    sync(&mut f);
    assert_eq!(get_analog_state(&f, vec1, 0).fUpdateTime, 0.0);
    assert_eq!(get_analog_state(&f, vec2, 0).fUpdateTime, 0.0);
}

#[test]
fn scalar_constant_values() {
    let mut f = Fixture::new();
    let main = f.get_action_set_handle(c"/actions/main");
    let [reverse, offset, throttle, steer] = [
        c"/actions/main/in/reverse",
        c"/actions/main/in/offset",
        c"/actions/main/in/throttle",
        c"/actions/main/in/steer",
    ]
    .map(|name| f.get_action_handle(name));
    let [left, right] =
        [c"/user/hand/left", c"/user/hand/right"].map(|hand| f.get_input_source_handle(hand));
    f.load_actions(c"actions_scalar_constant.json");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);

    let input = |path: &CStr| f.get_action::<f32>(f.get_action_handle(path));
    let x = input(c"/user/hand/left/input/x/click-/actions/main");
    let y = input(c"/user/hand/left/input/y/click-/actions/main");
    let b = input(c"/user/hand/right/input/b/click-/actions/main");
    let grip = input(c"/user/hand/left/input/squeeze/value-/actions/main");
    let trigger = f.get_action::<f32>(throttle);
    let stick = f.get_action::<xr::Vector2f>(steer);
    let set = |action, hand, value| {
        fakexr::set_action_state(action, fakexr::ActionState::Float(value), hand)
    };
    for (action, hand) in [
        (x, LeftHand),
        (y, LeftHand),
        (grip, LeftHand),
        (b, RightHand),
        (trigger, RightHand),
    ] {
        set(action, hand, 0.0);
    }
    fakexr::set_action_state(stick, fakexr::ActionState::Vector2(0.3, 0.0), LeftHand);
    let sync = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: main,
            ..Default::default()
        })
    };
    let xyz = |f: &Fixture, action, restrict| {
        let s = get_analog_state(f, action, restrict);
        assert!(s.bActive);
        (s.x, s.y, s.z)
    };

    // Released inputs report the off values.
    sync(&mut f);
    assert_eq!(xyz(&f, reverse, left), (0.0, 0.0, 0.0));
    assert_eq!(xyz(&f, offset, left), (0.25, 0.0, 0.0));

    // Pressed ones the on values: -1 reverses.
    set(x, LeftHand, 1.0);
    set(y, LeftHand, 1.0);
    sync(&mut f);
    assert_eq!(xyz(&f, reverse, left), (-1.0, 0.0, 0.0));
    assert_eq!(xyz(&f, reverse, 0), (-1.0, 0.0, 0.0));
    assert_eq!(xyz(&f, offset, left), (0.5, 0.0, 0.0));

    // The largest value wins among an action's bindings, like OpenXR's own.
    set(b, RightHand, 1.0);
    set(trigger, RightHand, 0.3);
    sync(&mut f);
    assert_eq!(xyz(&f, throttle, right), (-0.5, 0.0, 0.0));
    set(trigger, RightHand, 0.8);
    sync(&mut f);
    assert_eq!(xyz(&f, throttle, right), (0.8, 0.0, 0.0));
    set(b, RightHand, 0.0);
    sync(&mut f);
    assert_eq!(xyz(&f, throttle, right), (0.8, 0.0, 0.0));

    // Vector2 actions get every axis, 1 by default.
    assert_eq!(xyz(&f, steer, left), (0.3, 0.0, 0.0));
    set(grip, LeftHand, 1.0);
    sync(&mut f);
    assert_eq!(xyz(&f, steer, left), (-1.0, 1.0, 0.0));
    fakexr::set_action_state(stick, fakexr::ActionState::Vector2(0.0, -1.0), LeftHand);
    set(grip, LeftHand, 0.0);
    sync(&mut f);
    let steer_state = get_analog_state(&f, steer, left);
    assert_eq!(
        (
            steer_state.x,
            steer_state.y,
            steer_state.deltaX,
            steer_state.deltaY
        ),
        (0.0, -1.0, 1.0, -2.0)
    );

    // Deltas and update times follow the presses.
    let secs = |secs: f64| xr::Time::from_nanos((secs * 1e9) as i64);
    f.input.openxr.set_display_time(secs(10.0));
    fakexr::set_action_state_with_time(x, fakexr::ActionState::Float(0.0), LeftHand, secs(9.75));
    sync(&mut f);
    let s = get_analog_state(&f, reverse, left);
    assert_eq!((s.x, s.deltaX, s.fUpdateTime), (0.0, 1.0, -0.25));
    sync(&mut f);
    let s = get_analog_state(&f, reverse, left);
    assert_eq!((s.x, s.deltaX, s.fUpdateTime), (0.0, 0.0, 0.0));
    fakexr::set_action_state_with_time(x, fakexr::ActionState::Float(1.0), LeftHand, secs(9.5));
    sync(&mut f);
    let s = get_analog_state(&f, reverse, left);
    assert_eq!((s.x, s.deltaX, s.fUpdateTime), (-1.0, -1.0, -0.5));
}

#[test]
fn vector3_actions() {
    let mut f = Fixture::new();
    let main = f.get_action_set_handle(c"/actions/main");
    let menu = f.get_action_set_handle(c"/actions/menu");
    let look = f.get_action_handle(c"/actions/main/in/look");
    let unbound = f.get_action_handle(c"/actions/main/in/unbound");
    let [left, right] =
        [c"/user/hand/left", c"/user/hand/right"].map(|hand| f.get_input_source_handle(hand));
    f.load_actions(c"actions_scalar_constant.json");
    f.set_interaction_profile::<OculusTouch>(RightHand);
    let grip = f.get_action::<f32>(
        f.get_action_handle(c"/user/hand/right/input/squeeze/value-/actions/main"),
    );
    fakexr::set_action_state(grip, fakexr::ActionState::Float(0.0), RightHand);
    let sync = |f: &mut Fixture, menu_priority| {
        f.sync_sets(&mut [
            vr::VRActiveActionSet_t {
                ulActionSet: main,
                ..Default::default()
            },
            vr::VRActiveActionSet_t {
                ulActionSet: menu,
                nPriority: menu_priority,
                ..Default::default()
            },
        ])
    };

    // Before any sync, and without bindings, the action is valid but inactive.
    for action in [look, unbound] {
        assert!(!get_analog_state(&f, action, 0).bActive);
    }
    // Scalar constants drive every axis.
    let xyz = |s: vr::InputAnalogActionData_t| (s.x, s.y, s.z);
    sync(&mut f, 0);
    let s = get_analog_state(&f, look, right);
    assert!(s.bActive);
    assert_eq!(xyz(s), (0.0, 0.0, -1.0));
    fakexr::set_action_state(grip, fakexr::ActionState::Float(1.0), RightHand);
    sync(&mut f, 0);
    let s = get_analog_state(&f, look, right);
    assert_eq!(xyz(s), (0.5, -0.5, 1.0));
    assert_eq!((s.deltaX, s.deltaY, s.deltaZ), (0.5, -0.5, 2.0));
    // The grip's, which drives the scalar constants.
    assert_eq!(
        s.activeOrigin,
        f.get_input_source_handle(c"/user/hand/right/input/grip")
    );
    assert_eq!(xyz(get_analog_state(&f, look, 0)), (0.5, -0.5, 1.0));
    assert!(!get_analog_state(&f, look, left).bActive);

    let s = get_analog_state(&f, unbound, 0);
    assert!(!s.bActive);
    assert_eq!((s.x, s.y, s.z), (0.0, 0.0, 0.0));
    assert!(get_action_origins(&f, main, unbound).is_empty());
    assert!(get_binding_info(&f, unbound).is_empty());

    let origins = get_action_origins(&f, main, look);
    assert!(
        origins
            .iter()
            .any(|origin| get_origin_info(&f, *origin).trackedDeviceIndex == Hand::Right as u32)
    );
    assert_eq!(get_binding_info(&f, look).len(), 1);

    let mut digital = vr::InputDigitalActionData_t::default();
    assert_eq!(
        f.input.GetDigitalActionData(
            look,
            &mut digital,
            std::mem::size_of_val(&digital) as u32,
            0
        ),
        vr::EVRInputError::WrongType
    );

    // The menu binds the grip too, so it masks the action while it takes priority.
    sync(&mut f, 1);
    assert!(!get_analog_state(&f, look, right).bActive);
    sync(&mut f, 0);
    assert!(get_analog_state(&f, look, right).bActive);
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
    // fakexr can't locate in the stage space.
    f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    let get_level = |f: &Fixture| {
        let mut level = Estimated;
        assert_eq!(
            f.input.GetSkeletalTrackingLevel(skeleton, &mut level),
            vr::EVRInputError::None
        );
        level
    };
    let level = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
        // drop cached hand skeletons
        f.input.frame_start_update();
        get_level(f)
    };

    let bones = |f: &Fixture| {
        let mut bones = [vr::VRBoneTransform_t::default(); super::HandSkeletonBone::Count as usize];
        assert_eq!(
            f.input.GetSkeletalBoneData(
                skeleton,
                vr::EVRSkeletalTransformSpace::Parent,
                vr::EVRSkeletalMotionRange::WithController,
                bones.as_mut_ptr(),
                bones.len() as u32,
            ),
            vr::EVRInputError::None
        );
        bones
    };
    let index_finger = |bones: &[vr::VRBoneTransform_t]| {
        let bone = bones[super::HandSkeletonBone::IndexFinger2 as usize];
        (bone.position.v, bone.orientation.w)
    };

    // Reported before any bone data was requested.
    f.set_interaction_profile::<SimpleController>(LeftHand);
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::UNOBSTRUCTED));
    assert_eq!(level(&mut f), Full);

    // Joints already located this frame aren't located again.
    f.input.frame_start_update();
    let tracked = bones(&f);
    let located = fakexr::hand_joints_located();
    assert_eq!(get_level(&f), Full);
    assert_eq!(fakexr::hand_joints_located(), located);
    // fakexr's joints are all at the origin, so the bones are on their parents.
    let (position, w) = index_finger(&tracked);
    assert_eq!(position[..3], [0.0; 3]);
    assert_eq!(w, 1.0);

    fakexr::set_hand_tracked(f.raw_session(), LeftHand, None);
    assert_eq!(level(&mut f), Full);
    // Nor do estimated bones lower it.
    let estimated = bones(&f);
    assert_eq!(get_level(&f), Full);
    assert_ne!(index_finger(&estimated).0[..3], [0.0; 3]);

    // A different device doesn't inherit it.
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    assert_eq!(level(&mut f), Estimated);
    // A skeleton derived from the Touch's inputs is replaced by the estimated one.
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::CONTROLLER));
    assert_eq!(level(&mut f), Estimated);
    assert_ne!(index_finger(&bones(&f)).0[..3], [0.0; 3]);

    // One from a controller that senses the fingers is kept.
    f.set_interaction_profile::<FrameController>(LeftHand);
    assert_eq!(level(&mut f), Partial);
    assert_eq!(index_finger(&bones(&f)).0[..3], [0.0; 3]);
}

#[test]
fn skeletal_summary_from_hand_tracking() {
    use xr::HandTrackingDataSourceEXT as Source;

    fakexr::set_hand_tracking_supported(true);
    let mut f = Fixture::new();
    f.load_actions(c"actions.json");
    let set1 = f.get_action_set_handle(c"/actions/set1");
    let skeleton = f.get_action_handle(c"/actions/set1/in/skellyl");
    // fakexr can't locate in the stage space.
    f.input
        .openxr
        .set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    let curls = |f: &mut Fixture| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
        // drop cached hand skeletons
        f.input.frame_start_update();
        let mut summary = vr::VRSkeletalSummaryData_t::default();
        assert_eq!(
            f.input
                .GetSkeletalSummaryData(skeleton, vr::EVRSummaryType::FromDevice, &mut summary),
            vr::EVRInputError::None
        );
        summary.flFingerCurl
    };

    // fakexr's joints are all at the origin, which makes for fully curled fingers.
    // A hand without a controller, whose joints aren't located in a grip space.
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::UNOBSTRUCTED));
    assert_eq!(curls(&mut f), [1.0; 5]);

    f.set_interaction_profile::<SimpleController>(LeftHand);
    assert_eq!(curls(&mut f), [1.0; 5]);
    // The bones of the same frame reuse the joints.
    let located = fakexr::hand_joints_located();
    let mut bones = [vr::VRBoneTransform_t::default(); super::HandSkeletonBone::Count as usize];
    assert_eq!(
        f.input.GetSkeletalBoneData(
            skeleton,
            vr::EVRSkeletalTransformSpace::Parent,
            vr::EVRSkeletalMotionRange::WithController,
            bones.as_mut_ptr(),
            bones.len() as u32,
        ),
        vr::EVRInputError::None
    );
    assert_eq!(fakexr::hand_joints_located(), located);

    // Estimated, from no inputs.
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, None);
    assert_eq!(curls(&mut f), [0.0; 5]);

    // Joints derived from the Touch's inputs are estimated too, unlike the Frame's.
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    fakexr::set_hand_tracked(f.raw_session(), LeftHand, Some(Source::CONTROLLER));
    assert_eq!(curls(&mut f), [0.0; 5]);
    f.set_interaction_profile::<FrameController>(LeftHand);
    assert_eq!(curls(&mut f), [1.0; 5]);
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
    assert!(!f.get_bool_state(boolact).unwrap().bActive);

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
    // So the menu's trigger comes from a custom binding.
    f.custom_binding_action(select, Knuckles::profile_path(), |_| true);
    let left = f.get_input_source_handle(c"/user/hand/left");
    let right = f.get_input_source_handle(c"/user/hand/right");
    // Fire and throttle are bound to the trigger.
    let trigger = f.get_input_source_handle(c"/user/hand/left/input/trigger");
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
    assert_eq!(digital(&f, 0), (true, true, true, trigger));

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
    assert_eq!(digital(&f, 0), (true, true, false, trigger));
    assert!(get_analog_state(&f, throttle, 0).bActive);

    // A set restricted to one hand only masks that hand.
    sync(&mut f, 1, right);
    assert_eq!(digital(&f, left), (true, true, false, trigger));
    assert_eq!(digital(&f, right), (false, false, true, 0));
    assert_eq!(digital(&f, 0), (true, true, false, trigger));
    let throttle_state = get_analog_state(&f, throttle, 0);
    assert!(throttle_state.bActive);
    assert_eq!(throttle_state.activeOrigin, trigger);
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
    let toggle = f.custom_binding_action(crouch, Knuckles::profile_path(), |ty| {
        matches!(ty, BoolBindingType::Toggle(_))
    });
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

/// Actions bound to /user/head/proximity in the generic_hmd bindings follow user presence.
#[test]
fn head_proximity() {
    let mut f = Fixture::new();
    let main = f.get_action_set_handle(c"/actions/main");
    let other = f.get_action_set_handle(c"/actions/other");
    let on_head = f.get_action_handle(c"/actions/main/in/HeadsetOnHead");
    let on_head_or_a = f.get_action_handle(c"/actions/main/in/OnHeadOrA");
    f.load_actions(c"actions_head_proximity.json");
    f.set_interaction_profile::<Knuckles>(RightHand);
    let head = f.get_input_source_handle(c"/user/head");
    let left = f.get_input_source_handle(c"/user/hand/left");

    let sync = |f: &mut Fixture, set| {
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set,
            ..Default::default()
        })
    };
    let set_present = |f: &Fixture, present| {
        fakexr::set_user_presence(f.raw_session(), present);
        f.input.openxr.poll_events();
    };
    let state = |f: &Fixture, action, restrict| f.get_bool_state_hand(action, restrict).unwrap();
    let flags = |s: vr::InputDigitalActionData_t| (s.bActive, s.bState, s.bChanged);

    // The vector1 action isn't bound.
    let data = f.input.openxr.session_data.get();
    let proximity = &data.input_data.get_loaded_actions().unwrap().head_proximity;
    assert_eq!(proximity.len(), 2);
    drop(data);

    assert_eq!(flags(state(&f, on_head, 0)), (false, false, false));
    sync(&mut f, main);
    let s = state(&f, on_head, 0);
    assert_eq!(flags(s), (true, true, true));
    let proximity_origin = f.get_input_source_handle(c"/user/head/proximity");
    assert_eq!(s.activeOrigin, proximity_origin);
    sync(&mut f, main);
    assert_eq!(flags(state(&f, on_head, 0)), (true, true, false));

    set_present(&f, false);
    sync(&mut f, main);
    assert_eq!(flags(state(&f, on_head, 0)), (true, false, true));
    assert_eq!(flags(state(&f, on_head, head)), (true, false, true));
    // Not on a hand.
    assert_eq!(flags(state(&f, on_head, left)), (false, false, false));

    let mut info = Default::default();
    assert_eq!(
        f.input
            .GetOriginTrackedDeviceInfo(head, &mut info, std::mem::size_of_val(&info) as u32),
        vr::EVRInputError::None
    );
    assert_eq!(info.trackedDeviceIndex, vr::k_unTrackedDeviceIndex_Hmd);

    // Hand bindings of the same action count too.
    let a = f.get_action::<bool>(on_head_or_a);
    fakexr::set_action_state(a, fakexr::ActionState::Bool(true), RightHand);
    sync(&mut f, main);
    let s = state(&f, on_head_or_a, 0);
    assert_eq!(flags(s), (true, true, true));
    assert_eq!(
        s.activeOrigin,
        f.get_input_source_handle(c"/user/hand/right/input/a")
    );
    assert_eq!(flags(state(&f, on_head_or_a, head)), (true, false, false));
    set_present(&f, true);
    sync(&mut f, main);
    assert_eq!(flags(state(&f, on_head_or_a, 0)), (true, true, false));
    fakexr::set_action_state(a, fakexr::ActionState::Bool(false), RightHand);
    sync(&mut f, main);
    let s = state(&f, on_head_or_a, 0);
    assert_eq!(flags(s), (true, true, false));
    assert_eq!(s.activeOrigin, proximity_origin);

    // Inactive with its set, or with the set restricted to a hand.
    sync(&mut f, other);
    assert_eq!(flags(state(&f, on_head, 0)), (false, false, false));
    assert_eq!(flags(state(&f, on_head, head)), (false, false, false));
    sync(&mut f, main);
    assert_eq!(flags(state(&f, on_head, 0)), (true, true, true));
    sync(&mut f, main);
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: main,
        ulRestrictedToDevice: left,
        ..Default::default()
    });
    assert_eq!(flags(state(&f, on_head, 0)), (false, false, false));
}

#[test]
fn generic_hmd_controller_type() {
    use super::action_manifest::ControllerType;
    let ty: ControllerType = serde_json::from_str(r#""generic_hmd""#).unwrap();
    assert_eq!(ty, ControllerType::GenericHmd);
    assert_eq!("generic_hmd".parse::<ControllerType>(), Ok(ty.clone()));
    assert_eq!(ty.to_string(), "generic_hmd");
}

/// A fixture whose runtime reports actions active per hand like Monado, with both hands on `P`.
fn stick_fixture<P: InteractionProfile>(actions: &CStr) -> Fixture {
    fakexr::set_binding_activity(true);
    let mut f = Fixture::new();
    f.load_actions(actions);
    f.set_interaction_profile::<P>(LeftHand);
    f.set_interaction_profile::<P>(RightHand);
    f
}

/// Moves a thumbstick: every action bound to it or its touch sees the change.
fn move_stick(
    f: &mut Fixture,
    sets: &mut [vr::VRActiveActionSet_t],
    hand: &str,
    touched: bool,
    [x, y]: [f32; 2],
) {
    let session = f.raw_session();
    let input = format!("/user/hand/{hand}/input/thumbstick");
    fakexr::set_input(
        session,
        &format!("{input}/touch"),
        fakexr::InputValue::Bool(touched),
    );
    fakexr::set_input(session, &input, fakexr::InputValue::Vector2(x, y));
    f.sync_sets(sets);
}

/// The unrestricted value of an analog action, rounded off.
fn stick_value(f: &Fixture, name: &CStr) -> [f32; 2] {
    let state = get_analog_state(f, f.get_action_handle(name), 0);
    assert!(state.bActive, "{name:?} inactive");
    [state.x, state.y].map(|v| (v * 100.0).round() / 100.0)
}

/// The boolean actions of `names` that are pressed, unrestricted.
fn pressed_of(f: &Fixture, names: &[&CStr]) -> Vec<String> {
    names
        .iter()
        .filter(|name| f.get_bool_state(f.get_action_handle(name)).unwrap().bState)
        .map(|name| {
            name.to_string_lossy()
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Half-Life: Alyx turns, dashes and scrolls with the right stick: two touch dpads, a joystick
/// with a deadzone and a plain joystick in another set, all on the same input (#321).
fn turn_stick_bindings<P: InteractionProfile>() {
    let mut f = stick_fixture::<P>(c"actions_turn_stick.json");
    let mut sets = ["/actions/move", "/actions/scroll", "/actions/weapon"].map(|set| {
        vr::VRActiveActionSet_t {
            ulActionSet: f.get_action_set_handle(&std::ffi::CString::new(set).unwrap()),
            ..Default::default()
        }
    });
    let buttons = [
        c"/actions/move/in/TurnLeft",
        c"/actions/move/in/TurnRight",
        c"/actions/move/in/WalkMantle",
        c"/actions/move/in/ShiftDashJump",
        c"/actions/move/in/MoveBack",
        c"/actions/move/in/Teleport",
        c"/actions/move/in/Walk",
    ];
    let turn = c"/actions/move/in/ContinuousTurn";
    let teleport_turn = c"/actions/move/in/TeleportTurn";
    let scroll = c"/actions/scroll/in/Scroll";
    let right = f.get_input_source_handle(c"/user/hand/right");
    let left = f.get_input_source_handle(c"/user/hand/left");

    for (direction, [x, y], expected) in [
        ("east", [1.0, 0.0], vec!["TurnRight"]),
        ("west", [-1.0, 0.0], vec!["TurnLeft"]),
        ("north", [0.0, 1.0], vec!["WalkMantle"]),
        ("south", [0.0, -1.0], vec!["ShiftDashJump", "MoveBack"]),
    ] {
        move_stick(&mut f, &mut sets, "right", true, [x, y]);
        assert_eq!(
            pressed_of(&f, &buttons),
            expected,
            "right stick {direction}"
        );
        assert_eq!(stick_value(&f, turn), [x, y], "right stick {direction}");
        assert_eq!(stick_value(&f, scroll), [x, y], "right stick {direction}");
        assert_eq!(stick_value(&f, teleport_turn), [0.0, 0.0]);
        let turn_right = f.get_action_handle(c"/actions/move/in/TurnRight");
        assert_eq!(
            [right, left].map(|hand| f.get_bool_state_hand(turn_right, hand).unwrap().bState),
            [direction == "east", false]
        );
        // Sticky dpads let go in the middle.
        move_stick(&mut f, &mut sets, "right", true, [0.0, 0.0]);
        assert_eq!(pressed_of(&f, &buttons), Vec::<String>::new());
    }

    // Inside the 75% deadzone.
    move_stick(&mut f, &mut sets, "right", true, [0.5, 0.0]);
    assert_eq!(pressed_of(&f, &buttons), Vec::<String>::new());
    assert_eq!(stick_value(&f, turn), [0.0, 0.0]);
    assert_eq!(stick_value(&f, scroll), [0.5, 0.0]);
    // The dpads need the stick touched, the joysticks don't.
    move_stick(&mut f, &mut sets, "right", false, [1.0, 0.0]);
    assert_eq!(pressed_of(&f, &buttons), Vec::<String>::new());
    assert_eq!(stick_value(&f, turn), [1.0, 0.0]);
    move_stick(&mut f, &mut sets, "right", false, [0.0, 0.0]);

    // The left stick teleports and walks, and doesn't turn.
    move_stick(&mut f, &mut sets, "left", true, [0.0, 1.0]);
    assert_eq!(pressed_of(&f, &buttons), ["Teleport", "Walk"]);
    assert_eq!(stick_value(&f, teleport_turn), [0.0, 1.0]);
    assert_eq!(stick_value(&f, scroll), [0.0, 1.0]);
    assert_eq!(stick_value(&f, turn), [0.0, 0.0]);
}

#[test]
fn turn_stick_bindings_touch() {
    turn_stick_bindings::<OculusTouch>();
}

#[test]
fn turn_stick_bindings_touch_plus() {
    turn_stick_bindings::<super::profiles::meta_touch_plus::MetaTouchPlus>();
}

/// The Unreal SteamVR Input plugin names a stick's action like
/// "MoveForward_X,MoveForward_Y X Y_axis2d" and reads it unrestricted. The Walking Dead: Saints &
/// Sinners turns with the right stick and inverts the left one's Y axis (#321).
fn axis2d_bindings<P: InteractionProfile>() {
    let mut f = stick_fixture::<P>(c"actions_axis2d.json");
    let mut sets = [vr::VRActiveActionSet_t {
        ulActionSet: f.get_action_set_handle(c"/actions/main"),
        ..Default::default()
    }];
    let buttons = [
        c"/actions/main/in/TurnAxisLeft",
        c"/actions/main/in/TouchMovementActive",
    ];
    let turn = c"/actions/main/in/ConfigurableTurnAxis_X,ConfigurableTurnAxis_Y X Y_axis2d";
    let movement = c"/actions/main/in/MoveForward_X,MoveForward_Y X Y_axis2d";

    for [x, y] in [[1.0, 0.0], [-1.0, 0.0], [0.0, 1.0], [0.0, -0.5]] {
        move_stick(&mut f, &mut sets, "right", true, [x, y]);
        assert_eq!(stick_value(&f, turn), [x, y]);
        assert_eq!(stick_value(&f, movement), [0.0, 0.0]);
        assert_eq!(pressed_of(&f, &buttons), ["TurnAxisLeft"]);
    }
    move_stick(&mut f, &mut sets, "right", false, [0.0, 0.0]);

    move_stick(&mut f, &mut sets, "left", true, [0.25, 1.0]);
    assert_eq!(stick_value(&f, movement), [0.25, -1.0]);
    assert_eq!(stick_value(&f, turn), [0.0, 0.0]);
    assert_eq!(pressed_of(&f, &buttons), ["TouchMovementActive"]);
}

#[test]
fn axis2d_bindings_touch() {
    axis2d_bindings::<OculusTouch>();
}

#[test]
fn axis2d_bindings_touch_plus() {
    axis2d_bindings::<super::profiles::meta_touch_plus::MetaTouchPlus>();
}

/// Loads actions_gamepad.json with Touch controllers and a gamepad, and syncs the main set.
fn gamepad_fixture() -> (Fixture, vr::VRActionSetHandle_t) {
    let mut f = Fixture::new();
    let main = f.get_action_set_handle(c"/actions/main");
    f.load_actions(c"actions_gamepad.json");
    f.set_interaction_profile::<OculusTouch>(LeftHand);
    f.set_interaction_profile::<OculusTouch>(RightHand);
    f.set_interaction_profile::<XboxController>(Gamepad);
    sync_set(&mut f, main);
    (f, main)
}

fn sync_set(f: &mut Fixture, set: vr::VRActionSetHandle_t) {
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: set,
        ..Default::default()
    });
}

#[test]
fn gamepad_actions() {
    use fakexr::ActionState::*;
    let (mut f, main) = gamepad_fixture();
    let jump = f.get_action_handle(c"/actions/main/in/jump");
    let throttle = f.get_action_handle(c"/actions/main/in/throttle");
    let move_ = f.get_action_handle(c"/actions/main/in/move");
    let turn = f.get_action_handle(c"/actions/main/in/turn");
    let left = f.get_input_source_handle(c"/user/hand/left");
    let gamepad = f.get_input_source_handle(c"/user/gamepad");

    fakexr::set_action_state(f.get_action::<bool>(jump), true.into(), Gamepad);
    fakexr::set_action_state(f.get_action::<f32>(throttle), Float(0.25), Gamepad);
    fakexr::set_action_state(f.get_action::<f32>(throttle), Float(0.5), LeftHand);
    let stick = f.get_action::<xr::Vector2f>(move_);
    fakexr::set_action_state(stick, Vector2(0.0, 1.0), Gamepad);
    sync_set(&mut f, main);

    // Pressed on the gamepad's A, which is the origin.
    let a = f.get_input_source_handle(c"/user/gamepad/input/a");
    let s = f.get_bool_state(jump).unwrap();
    assert_eq!((s.bActive, s.bState, s.activeOrigin), (true, true, a));
    let s = f.get_bool_state_hand(jump, gamepad).unwrap();
    assert_eq!((s.bActive, s.bState, s.activeOrigin), (true, true, a));
    assert!(!f.get_bool_state_hand(jump, left).unwrap().bState);
    assert!(f.get_bool_state_hand(jump, a).unwrap().bState);

    // Each device's own value, and the largest one unrestricted.
    assert_eq!(get_analog_state(&f, throttle, gamepad).x, 0.25);
    assert_eq!(get_analog_state(&f, throttle, left).x, 0.5);
    let s = get_analog_state(&f, throttle, 0);
    let left_trigger = f.get_input_source_handle(c"/user/hand/left/input/trigger");
    assert_eq!((s.x, s.activeOrigin), (0.5, left_trigger));
    let s = get_analog_state(&f, move_, 0);
    let left_stick = f.get_input_source_handle(c"/user/gamepad/input/joystick_left");
    assert_eq!((s.y, s.activeOrigin), (1.0, left_stick));
    assert_eq!(get_analog_state(&f, move_, gamepad).y, 1.0);
    assert_eq!(get_analog_state(&f, move_, left).y, 0.0);

    // A dpad on the right stick, which xrizer computes.
    let right_stick = f.custom_binding_action(turn, XboxController::profile_path(), |ty| {
        matches!(ty, BoolBindingType::Dpad(_))
    });
    fakexr::set_action_state(right_stick, Vector2(1.0, 0.0), Gamepad);
    sync_set(&mut f, main);
    let s = f.get_bool_state(turn).unwrap();
    let right_stick_origin = f.get_input_source_handle(c"/user/gamepad/input/joystick_right");
    assert_eq!(
        (s.bState, s.bChanged, s.activeOrigin),
        (true, true, right_stick_origin)
    );
    assert!(f.get_bool_state_hand(turn, gamepad).unwrap().bState);
    assert!(!f.get_bool_state_hand(turn, left).unwrap().bState);
    fakexr::set_action_state(right_stick, Vector2(0.0, 0.0), Gamepad);
    sync_set(&mut f, main);
    let s = f.get_bool_state(turn).unwrap();
    assert_eq!((s.bState, s.bChanged), (false, true));

    // The gamepad has no pose.
    let pose = f.get_action_handle(c"/actions/main/in/pose");
    assert!(!f.get_pose(pose, gamepad).unwrap().bActive);
}

#[test]
fn gamepad_sets_restricted_and_prioritized() {
    let (mut f, main) = gamepad_fixture();
    let menu = f.get_action_set_handle(c"/actions/menu");
    let jump = f.get_action_handle(c"/actions/main/in/jump");
    let fire = f.get_action_handle(c"/actions/main/in/fire");
    let gamepad = f.get_input_source_handle(c"/user/gamepad");
    fakexr::set_action_state(f.get_action::<bool>(jump), true.into(), Gamepad);

    // A set can be restricted to the gamepad, as its actions have its subaction path.
    f.sync(vr::VRActiveActionSet_t {
        ulActionSet: main,
        ulRestrictedToDevice: gamepad,
        ..Default::default()
    });
    assert!(f.get_bool_state_hand(jump, gamepad).unwrap().bState);

    // A higher priority set's binding on the gamepad's trigger masks the game's there.
    let trigger = f.custom_binding_action(fire, XboxController::profile_path(), |ty| {
        matches!(ty, BoolBindingType::ThresholdFloat(_))
    });
    fakexr::set_action_state(trigger, fakexr::ActionState::Float(1.0), Gamepad);
    let sync = |f: &mut Fixture, menu_priority| {
        f.sync_sets(&mut [
            vr::VRActiveActionSet_t {
                ulActionSet: main,
                ..Default::default()
            },
            vr::VRActiveActionSet_t {
                ulActionSet: menu,
                nPriority: menu_priority,
                ..Default::default()
            },
        ])
    };
    sync(&mut f, 0);
    let s = f.get_bool_state(fire).unwrap();
    let trigger_origin = f.get_input_source_handle(c"/user/gamepad/input/trigger_right");
    assert_eq!(
        (s.bActive, s.bState, s.activeOrigin),
        (true, true, trigger_origin)
    );
    sync(&mut f, 1);
    let s = f.get_bool_state_hand(fire, gamepad).unwrap();
    assert_eq!((s.bActive, s.bState, s.bChanged), (false, false, true));
    // Jump is on A, which the menu doesn't use.
    assert!(f.get_bool_state_hand(jump, gamepad).unwrap().bState);
}

#[test]
fn gamepad_haptics() {
    let (mut f, main) = gamepad_fixture();
    let vib = f.get_action_handle(c"/actions/main/in/vib");
    let gamepad = f.get_input_source_handle(c"/user/gamepad");
    assert_eq!(
        f.input
            .TriggerHapticVibrationAction(vib, 0.0, 0.1, 160.0, 0.5, gamepad),
        vr::EVRInputError::None
    );
    let feedback = fakexr::take_haptic_feedback(f.get_action::<xr::Haptic>(vib));
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].hand, Some(Gamepad));

    // SteamVR's binding haptic for a click from the gamepad's trigger.
    let fire = f.get_action_handle(c"/actions/main/in/fire");
    let trigger = f.custom_binding_action(fire, XboxController::profile_path(), |ty| {
        matches!(ty, BoolBindingType::ThresholdFloat(_))
    });
    let haptic = f
        .input
        .openxr
        .session_data
        .get()
        .input_data
        .get_loaded_actions()
        .unwrap()
        .haptic_action
        .as_raw();
    sync_set(&mut f, main);
    fakexr::set_action_state(trigger, fakexr::ActionState::Float(1.0), Gamepad);
    sync_set(&mut f, main);
    let feedback = fakexr::take_haptic_feedback(haptic);
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].hand, Some(Gamepad));
}

#[test]
fn gamepad_origins() {
    let (mut f, main) = gamepad_fixture();
    let jump = f.get_action_handle(c"/actions/main/in/jump");
    let a = f.get_input_source_handle(c"/user/gamepad/input/a");
    let gamepad_row = [
        "/user/gamepad".to_owned(),
        "/input/a".to_owned(),
        "button".to_owned(),
        "click".to_owned(),
        "button".to_owned(),
    ];

    // WiVRn binds the gamepad's profile while none is connected, which its inputs show.
    assert!(!get_action_origins(&f, main, jump).contains(&a));
    assert!(!binding_info_rows(&get_binding_info(&f, jump)).contains(&gamepad_row));
    // Only bound on the gamepad.
    let turn = f.get_action_handle(c"/actions/main/in/turn");
    assert!(get_action_origins(&f, main, turn).is_empty());

    let presence = f
        .input
        .openxr
        .session_data
        .get()
        .input_data
        .get_loaded_actions()
        .unwrap()
        .gamepad_presence
        .as_raw();
    fakexr::set_action_state(presence, false.into(), Gamepad);
    sync_set(&mut f, main);
    let origins = get_action_origins(&f, main, jump);
    assert!(origins.contains(&a), "{origins:?}");
    let rows = binding_info_rows(&get_binding_info(&f, jump));
    assert!(rows.contains(&gamepad_row), "{rows:?}");
    // The hands' come first.
    assert_eq!(rows[0][0], "/user/hand/right");
    let right_stick = f.get_input_source_handle(c"/user/gamepad/input/joystick_right");
    assert_eq!(get_action_origins(&f, main, turn), [right_stick]);

    let info = get_origin_info(&f, a);
    assert_eq!(info.trackedDeviceIndex, vr::k_unTrackedDeviceIndexInvalid);
    assert_eq!(info.devicePath, f.get_input_source_handle(c"/user/gamepad"));
}

#[test]
fn gamepad_controller_type() {
    use super::action_manifest::ControllerType;
    let ty: ControllerType = serde_json::from_str(r#""gamepad""#).unwrap();
    assert_eq!(ty, ControllerType::Gamepad);
    assert_eq!("gamepad".parse::<ControllerType>(), Ok(ty.clone()));
    assert_eq!(ty.to_string(), "gamepad");
}
