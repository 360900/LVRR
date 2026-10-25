use super::actions::{LoadedActionDataMap, XrNames};
use super::bindings::{ActionPath, DpadParameters, DpadSubMode};
use crate::input::custom_bindings::{
    BoolBindingData, BoolBindingType, DpadActions, HandSpeeds, ScalarConstantBinding,
    Vector2ActionData, Vector2BindingData, Vector2Transform,
};
use crate::input::devices::SubactionPaths;
use crate::input::haptics::{BindingHaptic, BindingHapticData, BindingPress};
use crate::input::origins::BindingInfo;
use crate::input::profiles::{DynInputPath, InputDevice, ProfileProperties, paths};
use crate::input::skeletal::SkeletalInputActionData;
use crate::input::{
    ActionData, BoundPose, BoundPoseType, Input, InputSourceKey, InteractionProfile, origins,
};
use crate::openxr_data::{self, Hand};
use log::{debug, info, trace, warn};
use openvr as vr;
use openxr as xr;
use slotmap::SlotMap;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::{Arc, RwLock};

pub(super) struct BindingsLoadContext<'a> {
    pub action_sets: &'a HashMap<String, xr::ActionSet>,
    pub actions: LoadedActionDataMap,
    pub names: XrNames,
    /// Sets that got actions for custom bindings.
    pub extra_action_sets: HashSet<String>,
    /// Interaction profiles that got bindings.
    pub suggested_profiles: HashSet<xr::Path>,
    pub per_profile_bindings: HashMap<xr::Path, HashMap<String, Vec<BoolBindingData>>>,
    pub per_profile_pose_bindings: HashMap<xr::Path, HashMap<String, BoundPose>>,
    /// Maps each interaction profile path → action name → bound input paths.
    /// Used to implement GetActionBindingInfo/GetActionOrigins.
    pub per_profile_input_paths: HashMap<xr::Path, HashMap<String, Vec<xr::Path>>>,
    /// Maps each interaction profile path → action name → what GetActionBindingInfo reports.
    pub per_profile_binding_info: HashMap<xr::Path, HashMap<String, Vec<BindingInfo>>>,
    /// Boolean actions bound to the headset's proximity sensor.
    pub head_proximity: HashSet<String>,
    /// The scalar_constant bindings of analog actions that aren't bound to their input directly,
    /// in every profile.
    pub scalar_constants: HashMap<String, Vec<ScalarConstantBinding>>,
    /// The identity of the controller that converted bindings were made for, per interaction
    /// profile that shows it (see [InteractionProfile::SIMULATE_CONVERTED]).
    pub simulated_properties: HashMap<xr::Path, &'static ProfileProperties>,
    /// SteamVR's binding haptics, with the set and action of their binding.
    pub binding_haptics: Vec<(String, String, BindingHapticData)>,
    /// The controller speeds grab bindings drop with, if any binding reads them.
    pub hand_speeds: Arc<HandSpeeds>,
    pub uses_hand_speeds: bool,
    pub grip_action: &'a xr::Action<xr::Posef>,
    pub info_action: &'a xr::Action<bool>,
    pub haptic_action: &'a xr::Action<xr::Haptic>,
    pub gamepad_presence: &'a xr::Action<bool>,
    pub skeletal_input: &'a SkeletalInputActionData,
}

impl<'a> BindingsLoadContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        action_sets: &'a HashMap<String, xr::ActionSet>,
        actions: LoadedActionDataMap,
        names: XrNames,
        grip_action: &'a xr::Action<xr::Posef>,
        info_action: &'a xr::Action<bool>,
        haptic_action: &'a xr::Action<xr::Haptic>,
        gamepad_presence: &'a xr::Action<bool>,
        skeletal_input: &'a SkeletalInputActionData,
    ) -> Self {
        BindingsLoadContext {
            action_sets,
            actions,
            names,
            extra_action_sets: Default::default(),
            suggested_profiles: Default::default(),
            per_profile_bindings: Default::default(),
            per_profile_pose_bindings: Default::default(),
            per_profile_input_paths: Default::default(),
            per_profile_binding_info: Default::default(),
            head_proximity: Default::default(),
            scalar_constants: Default::default(),
            simulated_properties: Default::default(),
            binding_haptics: Default::default(),
            hand_speeds: Default::default(),
            uses_hand_speeds: false,
            grip_action,
            info_action,
            haptic_action,
            gamepad_presence,
            skeletal_input,
        }
    }
}

impl BindingsLoadContext<'_> {
    /// Binds `output` to the headset's proximity sensor, if it's a boolean action.
    pub fn add_head_proximity_binding(&mut self, output: &ActionPath) {
        match self.actions.get(&output.path) {
            Some(ActionData::Bool(_)) => {
                trace!("binding {} to the proximity sensor", output.path);
                self.head_proximity.insert(output.path.clone());
            }
            Some(_) => warn!(
                "{} can't be bound to the proximity sensor, it isn't a boolean action",
                output.path
            ),
            None => warn!("Couldn't find action {}, skipping", output.path),
        }
    }

    pub fn for_profile<'a, 'b: 'a, C: openxr_data::Compositor, P: InteractionProfile>(
        &'b mut self,
        input: &'a Input<C>,
    ) -> Option<BindingsProfileLoadContext<'a>> {
        let instance = &input.openxr.instance;
        let Ok(interaction_profile) = instance.string_to_path(P::profile_path()) else {
            warn!(
                "Controller type {} has no OpenXR path supported?",
                std::any::type_name::<P>()
            );
            return None;
        };
        if !P::has_required_extensions(&input.openxr.enabled_extensions) {
            info!(
                "Not loading bindings for {:?} because required extensions are not supported",
                std::any::type_name::<P>(),
            );
            return None;
        }

        self.suggested_profiles.insert(interaction_profile);
        let bindings_parsed = self
            .per_profile_bindings
            .entry(interaction_profile)
            .or_default();
        let pose_bindings = self
            .per_profile_pose_bindings
            .entry(interaction_profile)
            .or_default();
        let input_paths_out = self
            .per_profile_input_paths
            .entry(interaction_profile)
            .or_default();
        let binding_info_out = self
            .per_profile_binding_info
            .entry(interaction_profile)
            .or_default();
        Some(BindingsProfileLoadContext {
            interaction_profile,
            profile_name: P::profile_path(),
            adapted: false,
            action_sets: self.action_sets,
            actions: &mut self.actions,
            names: &mut self.names,
            extra_action_sets: &mut self.extra_action_sets,
            bindings_parsed,
            pose_bindings,
            input_paths_out,
            binding_info_out,
            scalar_constants: &mut self.scalar_constants,
            binding_haptics: &mut self.binding_haptics,
            hand_speeds: &self.hand_speeds,
            uses_hand_speeds: &mut self.uses_hand_speeds,
            grip_action: self.grip_action,
            info_action: self.info_action,
            haptic_action: self.haptic_action,
            gamepad_presence: self.gamepad_presence,
            skeletal_input: self.skeletal_input,
            instance,
            subaction_paths: input.subaction_paths,
            gamepad: P::GAMEPAD,
            bindings: Vec::new(),
            use_force_for_dpad: P::USE_FORCE_DPAD,
            controller_type: P::properties()
                .openvr_controller_type
                .to_str()
                .unwrap_or_default(),
            input_sources: &input.input_source_map,
        })
    }
}

pub(super) struct BindingsProfileLoadContext<'a> {
    interaction_profile: xr::Path,
    profile_name: &'static str,
    /// The bindings were made for another controller, so they're expected to have inputs this
    /// one lacks.
    pub adapted: bool,
    pub action_sets: &'a HashMap<String, xr::ActionSet>,
    pub actions: &'a mut LoadedActionDataMap,
    names: &'a mut XrNames,
    extra_action_sets: &'a mut HashSet<String>,
    bindings_parsed: &'a mut HashMap<String, Vec<BoolBindingData>>,
    pub pose_bindings: &'a mut HashMap<String, BoundPose>,
    /// Input paths bound to each action in the current profile, keyed by action name.
    pub input_paths_out: &'a mut HashMap<String, Vec<xr::Path>>,
    /// What GetActionBindingInfo reports for each action in the current profile.
    pub binding_info_out: &'a mut HashMap<String, Vec<BindingInfo>>,
    scalar_constants: &'a mut HashMap<String, Vec<ScalarConstantBinding>>,
    binding_haptics: &'a mut Vec<(String, String, BindingHapticData)>,
    hand_speeds: &'a Arc<HandSpeeds>,
    uses_hand_speeds: &'a mut bool,
    pub grip_action: &'a xr::Action<xr::Posef>,
    pub info_action: &'a xr::Action<bool>,
    pub haptic_action: &'a xr::Action<xr::Haptic>,
    pub gamepad_presence: &'a xr::Action<bool>,
    pub skeletal_input: &'a SkeletalInputActionData,
    pub instance: &'a xr::Instance,
    subaction_paths: SubactionPaths,
    /// The profile is the gamepad's, see [InteractionProfile::GAMEPAD].
    gamepad: bool,
    pub bindings: Vec<(String, xr::Path)>,
    use_force_for_dpad: bool,
    /// SteamVR's name of the controllers, whose inputs origins are named after.
    controller_type: &'static str,
    input_sources: &'a RwLock<SlotMap<InputSourceKey, CString>>,
}

pub trait WithActionPattern {
    const PATTERN: &'static str;
    fn check_match(data: &ActionData) -> bool;
}

macro_rules! action_match {
    ($ty:ty, $pat:pat) => {
        impl WithActionPattern for $ty {
            const PATTERN: &'static str = stringify!($pat);
            fn check_match(data: &ActionData) -> bool {
                matches!(data, $pat)
            }
        }
    };
}

action_match!(bool, ActionData::Bool(_) | ActionData::Vector1 { .. });
action_match!(f32, ActionData::Vector1 { .. });
action_match!(xr::Vector2f, ActionData::Vector2 { .. });

/// Types of the actions xrizer binds to single inputs to compute custom bindings from.
pub trait InputActionType: xr::ActionTy {
    /// Distinguishes actions of different types bound to the same input.
    const KEY_SUFFIX: &'static str;
    fn into_data(action: xr::Action<Self>) -> ActionData;
    fn from_data(data: &ActionData) -> Option<&xr::Action<Self>>;
}

impl InputActionType for bool {
    const KEY_SUFFIX: &'static str = "-bool";
    fn into_data(action: xr::Action<Self>) -> ActionData {
        ActionData::Bool(action)
    }
    fn from_data(data: &ActionData) -> Option<&xr::Action<Self>> {
        match data {
            ActionData::Bool(action) => Some(action),
            _ => None,
        }
    }
}

impl InputActionType for f32 {
    const KEY_SUFFIX: &'static str = "";
    fn into_data(action: xr::Action<Self>) -> ActionData {
        ActionData::Vector1 {
            action,
            state: Default::default(),
            input_only: true,
        }
    }
    fn from_data(data: &ActionData) -> Option<&xr::Action<Self>> {
        match data {
            ActionData::Vector1 { action, .. } => Some(action),
            _ => None,
        }
    }
}

impl InputActionType for xr::Vector2f {
    const KEY_SUFFIX: &'static str = "";
    fn into_data(action: xr::Action<Self>) -> ActionData {
        ActionData::Vector2 {
            action,
            data: Vector2ActionData::input_only(),
        }
    }
    fn from_data(data: &ActionData) -> Option<&xr::Action<Self>> {
        match data {
            ActionData::Vector2 { action, .. } => Some(action),
            _ => None,
        }
    }
}

impl BindingsProfileLoadContext<'_> {
    /// Logs a binding this controller can't use, which SteamVR ignores silently: binding files
    /// often list inputs that only other models of the controller have. Expected in bindings
    /// adapted from another controller, so those only show at debug level.
    pub fn unusable_binding(&self, message: std::fmt::Arguments) {
        if self.adapted {
            debug!("{}: {message}", self.profile_name);
        } else {
            crate::log_unique!(log::Level::Info, "{}: {message}", self.profile_name);
        }
    }

    /// The controller speeds that grab bindings drop with, which are measured at each action
    /// sync once a binding reads them.
    pub fn hand_speeds(&mut self) -> Arc<HandSpeeds> {
        *self.uses_hand_speeds = true;
        self.hand_speeds.clone()
    }

    /// The subaction path of `device`.
    pub fn device_path(&self, device: InputDevice) -> xr::Path {
        self.subaction_paths.get(device)
    }

    /// Whether the profile's inputs are on `device`.
    pub fn has_device(&self, device: InputDevice) -> bool {
        self.gamepad == (device == InputDevice::Gamepad)
    }

    /// The origin action data reports for a binding of `input`: the handle of its source path in
    /// SteamVR's profile of the controller, e.g. `/user/hand/left/input/joystick`.
    pub fn origin(&self, input: DynInputPath) -> vr::VRInputValueHandle_t {
        let path = origins::source_path(self.controller_type, input);
        crate::input::input_source_handle(self.input_sources, &path)
    }

    /// The origin of a pose binding, e.g. `/user/hand/left/pose/raw`.
    pub fn pose_origin(&self, hand: Hand, pose: BoundPoseType) -> vr::VRInputValueHandle_t {
        let path = format!("{}/pose/{}", origins::hand_path(hand), pose.openvr_name());
        let path = CString::new(path).expect("pose paths have no NUL");
        crate::input::input_source_handle(self.input_sources, &path)
    }

    /// The origin of a haptic output on `device`, e.g. `/user/hand/left/output/haptic`.
    pub fn haptic_origin(&self, device: InputDevice) -> vr::VRInputValueHandle_t {
        let path = format!("{}/output/haptic", device.path());
        let path = CString::new(path).expect("haptic paths have no NUL");
        crate::input::input_source_handle(self.input_sources, &path)
    }

    pub fn get_action_set(&self, p0: &String) -> Option<&xr::ActionSet> {
        self.action_sets.get(p0)
    }

    #[track_caller]
    pub fn find_action(&self, name: &str) -> bool {
        let ret = self.actions.contains_key(name);
        if !ret {
            let caller = std::panic::Location::caller();
            warn!(
                "Couldn't find action {name}, skipping (line {})",
                caller.line()
            );
        }
        ret
    }

    pub fn try_suggest_binding<T: WithActionPattern>(
        &mut self,
        action_path: String,
        input_path: DynInputPath,
    ) {
        if self.find_action(&action_path) {
            if !T::check_match(&self.actions[&action_path]) {
                warn!(
                    "invalid path {input_path} for {action_path}: action data didn't match pattern {}",
                    T::PATTERN
                );
                return;
            }
            trace!("suggesting {input_path} for {action_path}");
            let binding_path = self
                .instance
                .string_to_path(&input_path.to_string())
                .unwrap();
            self.input_paths_out
                .entry(action_path.clone())
                .or_default()
                .push(binding_path);
            self.bindings.push((action_path, binding_path));
        }
    }

    /// Whether `output` can get a custom (boolean) binding. Checked before creating the actions
    /// the binding reads.
    #[track_caller]
    pub fn accepts_custom_binding(&self, output: &ActionPath) -> bool {
        if !self.find_action(&output.path) {
            return false;
        }
        let ok = bool::check_match(&self.actions[&output.path]);
        if !ok {
            warn!(
                "{} can't get a boolean binding: action data didn't match pattern {}",
                output.path,
                bool::PATTERN
            );
        }
        ok
    }

    /// Returns an action bound to just `input` in this profile. Custom bindings read inputs
    /// through these, so one input's value never mixes with another's. The action is shared by
    /// every custom binding reading this input in the set, and across profiles.
    pub fn input_action<T: InputActionType>(
        &mut self,
        action_set_name: &str,
        action_set: &xr::ActionSet,
        input: DynInputPath,
    ) -> Option<xr::Action<T>> {
        let key = format!("{input}-{action_set_name}{}", T::KEY_SUFFIX);
        let binding = self.instance.string_to_path(&input.to_string()).ok()?;
        let paths = self.subaction_paths.all();
        let action = match self.actions.entry(key.clone()) {
            Entry::Occupied(entry) => T::from_data(entry.get())?.clone(),
            Entry::Vacant(entry) => {
                let name = self.names.action_name(action_set_name, "xrizer-input");
                let action = action_set
                    .create_action::<T>(&name, &name, &paths)
                    .inspect_err(|e| warn!("Couldn't create action for {input}: {e}"))
                    .ok()?;
                trace!("created {name} for {input} ({action_set_name})");
                self.extra_action_sets.insert(action_set_name.to_owned());
                entry.insert(T::into_data(action.clone()));
                action
            }
        };
        self.bindings.push((key, binding));
        Some(action)
    }

    /// Adds a custom binding for `output`, which reads `input` on `device`.
    pub fn add_custom_binding(
        &mut self,
        output: &ActionPath,
        device: InputDevice,
        input: DynInputPath,
        ty: BoolBindingType,
    ) {
        if let Ok(input) = self.instance.string_to_path(&input.to_string()) {
            self.push_input_path(&output.path, input);
        }
        let device_path = self.device_path(device);
        let origin = self.origin(input);
        self.bindings_parsed
            .entry(output.path.clone())
            .or_default()
            .push(BoolBindingData::new(ty, device_path).with_origin(origin));
    }

    /// Plays `haptic` when `press` changes on `device`, for `output`'s binding in a set.
    pub fn add_binding_haptic(
        &mut self,
        action_set_name: &str,
        output: &ActionPath,
        device: InputDevice,
        press: BindingPress,
        haptic: BindingHaptic,
    ) {
        let hand = self.device_path(device);
        self.binding_haptics.push((
            action_set_name.to_owned(),
            output.path.clone(),
            BindingHapticData::new(self.interaction_profile, hand, press, haptic),
        ));
    }

    pub fn add_vector2_binding(
        &mut self,
        output: &ActionPath,
        path: DynInputPath,
        transform: Vector2Transform,
        action_set_name: &str,
        action_set: &xr::ActionSet,
    ) {
        if !self.find_action(&output.path) {
            return;
        }
        if !xr::Vector2f::check_match(&self.actions[&output.path]) {
            warn!(
                "invalid path {path} for {}: action data didn't match pattern {}",
                output.path,
                xr::Vector2f::PATTERN
            );
            return;
        }
        let Some(action) = self.input_action::<xr::Vector2f>(action_set_name, action_set, path)
        else {
            return;
        };
        if let Ok(input) = self.instance.string_to_path(&path.to_string()) {
            self.push_input_path(&output.path, input);
        }
        let hand = self.device_path(path.device);
        let origin = self.origin(path);
        let ActionData::Vector2 { data, .. } = self.actions.get_mut(&output.path).unwrap() else {
            unreachable!();
        };
        data.bindings.push(Vector2BindingData {
            action,
            profile: self.interaction_profile,
            hand,
            origin,
            transform,
        });
    }

    /// Whether `output` needs a scalar_constant binding with these values rather than a direct
    /// one: a vector1 action can be bound to its input when it's 1 while pressed and 0 otherwise,
    /// like a button.
    pub fn is_custom_scalar_constant(
        &self,
        output: &ActionPath,
        on: [f32; 3],
        off: [f32; 3],
    ) -> bool {
        match self.actions.get(&output.path) {
            Some(ActionData::Vector1 { .. }) => on[0] != 1.0 || off[0] != 0.0,
            Some(ActionData::Vector2 { .. } | ActionData::Vector3 { .. }) => true,
            // The direct binding reports what's wrong with it.
            _ => false,
        }
    }

    /// Adds a scalar_constant binding for `output`, an analog action, that reads `input` through
    /// `press`.
    pub fn add_scalar_constant_binding(
        &mut self,
        output: &ActionPath,
        input: DynInputPath,
        press: BoolBindingType,
        mut on: [f32; 3],
        mut off: [f32; 3],
    ) {
        let axes = match self.actions.get(&output.path) {
            Some(ActionData::Vector1 { .. }) => 1,
            Some(ActionData::Vector2 { .. }) => 2,
            Some(ActionData::Vector3 { .. }) => 3,
            _ => {
                warn!(
                    "{} can't get a scalar constant, it isn't an analog action",
                    output.path
                );
                return;
            }
        };
        for values in [&mut on, &mut off] {
            values[axes..].fill(0.0);
        }
        if let Ok(path) = self.instance.string_to_path(&input.to_string()) {
            self.push_input_path(&output.path, path);
        }
        let hand = self.device_path(input.device);
        let origin = self.origin(input);
        self.scalar_constants
            .entry(output.path.clone())
            .or_default()
            .push(ScalarConstantBinding {
                press,
                profile: self.interaction_profile,
                hand,
                origin,
                on,
                off,
            });
    }

    pub fn push_binding(&mut self, action: String, path: xr::Path) {
        self.input_paths_out
            .entry(action.clone())
            .or_default()
            .push(path);
        self.bindings.push((action, path));
    }

    /// Record the input path bound to an action that doesn't get a regular
    /// `push_binding` (dpad directions are driven through custom bindings).
    pub fn push_input_path(&mut self, action: &str, path: xr::Path) {
        self.input_paths_out
            .entry(action.to_string())
            .or_default()
            .push(path);
    }

    pub fn get_dpad_actions(
        &mut self,
        validate_path: &dyn Fn(DynInputPath) -> Option<DynInputPath>,
        parent_path: DynInputPath,
        action_set_name: &str,
        action_set: &xr::ActionSet,
        parameters: Option<&DpadParameters>,
    ) -> Option<DpadActions> {
        // Shared by every dpad (and other custom binding) on the same path and set
        let xy = self.input_action(action_set_name, action_set, parent_path)?;
        let use_force =
            self.use_force_for_dpad && matches!(parent_path.subpath, paths::DynSubpath::Trackpad);

        // The click or touch that activates the dpad, if such a path exists
        let click_or_touch = parameters
            .map(|p| match p.sub_mode {
                DpadSubMode::Click if use_force => paths::DynComponent::Force,
                DpadSubMode::Click => paths::DynComponent::Click,
                DpadSubMode::Touch => paths::DynComponent::Touch,
            })
            .and_then(|component| validate_path(parent_path.with_component(component)))
            .and_then(|path| self.input_action(action_set_name, action_set, path));

        let haptic = if use_force {
            // the need for haptic coincides with force-using dpads for now
            self.dpad_haptic(parent_path, action_set_name, action_set)
        } else {
            None
        };

        Some(DpadActions {
            xy,
            click_or_touch,
            haptic,
        })
    }

    fn dpad_haptic(
        &mut self,
        parent_path: DynInputPath,
        action_set_name: &str,
        action_set: &xr::ActionSet,
    ) -> Option<xr::Action<xr::Haptic>> {
        let haptic_path = format!("{}/output/haptic", parent_path.device.path());
        let binding = self.instance.string_to_path(&haptic_path).ok()?;
        let key = format!("{parent_path}-{action_set_name}-haptic");
        let paths = self.subaction_paths.all();
        let action = match self.actions.entry(key.clone()) {
            Entry::Occupied(entry) => {
                let ActionData::Haptic(action) = entry.get() else {
                    return None;
                };
                action.clone()
            }
            Entry::Vacant(entry) => {
                let name = self
                    .names
                    .action_name(action_set_name, "xrizer-dpad-haptic");
                let action = action_set
                    .create_action::<xr::Haptic>(&name, &name, &paths)
                    .inspect_err(|e| warn!("Couldn't create dpad haptic action: {e}"))
                    .ok()?;
                self.extra_action_sets.insert(action_set_name.to_owned());
                entry.insert(ActionData::Haptic(action.clone()));
                action
            }
        };
        self.bindings.push((key, binding));
        Some(action)
    }
}
