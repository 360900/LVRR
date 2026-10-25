use super::actions::{LoadedActionDataMap, XrNames};
use super::bindings::{ActionPath, DpadParameters, DpadSubMode};
use crate::input::custom_bindings::{
    BoolBindingData, BoolBindingType, DpadActions, Vector2ActionData, Vector2BindingData,
    Vector2Transform,
};
use crate::input::origins::BindingInfo;
use crate::input::profiles::{DynInputPath, paths};
use crate::input::skeletal::SkeletalInputActionData;
use crate::input::{ActionData, BoundPose, Input, InteractionProfile};
use crate::openxr_data::{self, Hand};
use log::{debug, info, trace, warn};
use openxr as xr;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

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
    pub grip_action: &'a xr::Action<xr::Posef>,
    pub info_action: &'a xr::Action<bool>,
    pub haptic_action: &'a xr::Action<xr::Haptic>,
    pub skeletal_input: &'a SkeletalInputActionData,
}

impl<'a> BindingsLoadContext<'a> {
    pub fn new(
        action_sets: &'a HashMap<String, xr::ActionSet>,
        actions: LoadedActionDataMap,
        names: XrNames,
        grip_action: &'a xr::Action<xr::Posef>,
        info_action: &'a xr::Action<bool>,
        haptic_action: &'a xr::Action<xr::Haptic>,
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
            grip_action,
            info_action,
            haptic_action,
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

        let hands = [
            input.get_subaction_path(Hand::Left),
            input.get_subaction_path(Hand::Right),
        ];

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
            grip_action: self.grip_action,
            info_action: self.info_action,
            haptic_action: self.haptic_action,
            skeletal_input: self.skeletal_input,
            instance,
            hands,
            bindings: Vec::new(),
            use_force_for_dpad: P::USE_FORCE_DPAD,
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
    pub grip_action: &'a xr::Action<xr::Posef>,
    pub info_action: &'a xr::Action<bool>,
    pub haptic_action: &'a xr::Action<xr::Haptic>,
    pub skeletal_input: &'a SkeletalInputActionData,
    pub instance: &'a xr::Instance,
    pub hands: [xr::Path; 2],
    pub bindings: Vec<(String, xr::Path)>,
    use_force_for_dpad: bool,
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

fn hand_to_path(hand: Hand) -> &'static str {
    match hand {
        Hand::Left => "/user/hand/left",
        Hand::Right => "/user/hand/right",
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

    pub fn hand_path(&self, hand: Hand) -> xr::Path {
        match hand {
            Hand::Left => self.hands[0],
            Hand::Right => self.hands[1],
        }
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
        let action = match self.actions.entry(key.clone()) {
            Entry::Occupied(entry) => T::from_data(entry.get())?.clone(),
            Entry::Vacant(entry) => {
                let name = self.names.action_name(action_set_name, "xrizer-input");
                let action = action_set
                    .create_action::<T>(&name, &name, &self.hands)
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

    /// Adds a custom binding for `output`, which reads `input` on `hand`.
    pub fn add_custom_binding(
        &mut self,
        output: &ActionPath,
        hand: Hand,
        input: DynInputPath,
        ty: BoolBindingType,
    ) {
        if let Ok(input) = self.instance.string_to_path(&input.to_string()) {
            self.push_input_path(&output.path, input);
        }
        let hand_path = self.hand_path(hand);
        self.bindings_parsed
            .entry(output.path.clone())
            .or_default()
            .push(BoolBindingData::new(ty, hand_path));
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
        let hand = self.hand_path(path.hand);
        let ActionData::Vector2 { data, .. } = self.actions.get_mut(&output.path).unwrap() else {
            unreachable!();
        };
        data.bindings.push(Vector2BindingData {
            action,
            profile: self.interaction_profile,
            hand,
            transform,
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
        let haptic_path = format!("{}/output/haptic", hand_to_path(parent_path.hand));
        let binding = self.instance.string_to_path(&haptic_path).ok()?;
        let key = format!("{parent_path}-{action_set_name}-haptic");
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
                    .create_action::<xr::Haptic>(&name, &name, &self.hands)
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
