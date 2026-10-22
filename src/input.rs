mod action_manifest;
mod custom_bindings;
mod devices;
mod legacy;
mod probe;
mod profiles;
mod skeletal;

#[cfg(test)]
mod tests;

pub use devices::TrackedDeviceType;
pub use probe::{ProbedProfiles, probe_interaction_profiles};
pub use profiles::InteractionProfile;

use devices::{SubactionPaths, TrackedDevice, TrackedDeviceList, is_simple_controller};
use skeletal::FingerState;
use skeletal::HandSkeletonBone;
use skeletal::SkeletalInputActionData;

pub use crate::input::devices::ProfileData;
use crate::input::profiles::ProfileProperties;
use crate::{
    openxr_data::{self, Hand, OpenXrData, RestartError, SessionData},
    tracy_span,
};
use custom_bindings::{BoolBindingData, Vector2ActionData};
use glam::Quat;
use legacy::LegacyActionData;
use log::{debug, info, trace, warn};
use openvr as vr;
use openxr as xr;
use slotmap::{Key, KeyData, SecondaryMap, SlotMap, new_key_type};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard};

new_key_type! {
    struct InputSourceKey;
    struct ActionKey;
    struct ActionSetKey;
}

/// Normalize OpenXR component names to their OpenVR equivalents.
fn normalize_component(name: &str) -> &str {
    match name {
        "thumbstick" => "joystick",
        "squeeze" => "grip",
        _ => name,
    }
}

/// Parse an OpenXR path into (device_path, input_kind, component, slot).
/// e.g. "/user/hand/right/input/trigger/value" → ("/user/hand/right", "input", "trigger", "value")
fn parse_input_path(path: &str) -> (&str, &str, &str, &str) {
    for kind in ["input", "output"] {
        let needle = format!("/{kind}/");
        if let Some(pos) = path.find(&needle) {
            let device = &path[..pos];
            let after = &path[pos + needle.len()..];
            let mut parts = after.splitn(2, '/');
            let comp = parts.next().unwrap_or("");
            let slot = parts.next().unwrap_or("");
            return (device, kind, comp, slot);
        }
    }
    ("", "", "", "")
}

/// The first [`HandSkeletonBone::Count`] entries of an app's bone array, which may be longer.
///
/// # Safety
/// `array` must be null or valid for `count` entries.
unsafe fn bone_array<'a>(
    array: *mut vr::VRBoneTransform_t,
    count: u32,
) -> Result<&'a mut [vr::VRBoneTransform_t; HandSkeletonBone::Count as usize], vr::EVRInputError> {
    if count < HandSkeletonBone::Count as u32 {
        return Err(vr::EVRInputError::BufferTooSmall);
    }
    unsafe {
        array
            .cast::<[_; HandSkeletonBone::Count as usize]>()
            .as_mut()
    }
    .ok_or(vr::EVRInputError::InvalidParam)
}

/// Copy a string into a fixed-size `c_char` buffer with null terminator.
fn copy_cstr(dst: &mut [c_char], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (d, &b) in dst.iter_mut().zip(bytes[..n].iter()) {
        *d = b as c_char;
    }
    dst[n] = 0;
}

#[derive(macros::InterfaceImpl)]
#[interface = "IVRInput"]
#[versions(011, 010, 007, 006, 005, 004, 003)]
pub struct Input<C: openxr_data::Compositor> {
    openxr: Arc<OpenXrData<C>>,
    vtables: Vtables<C>,
    input_source_map: RwLock<SlotMap<InputSourceKey, CString>>,
    left_hand_key: InputSourceKey,
    right_hand_key: InputSourceKey,
    action_map: RwLock<SlotMap<ActionKey, Action>>,
    set_map: RwLock<SlotMap<ActionSetKey, String>>,
    loaded_actions_path: OnceLock<PathBuf>,
    legacy_state: legacy::LegacyState,
    /// See [Input::current_skeletal_tracking_level], the only writer besides profile changes.
    skeletal_tracking_level: RwLock<[vr::EVRSkeletalTrackingLevel; 2]>,
    /// The sets activated by the last UpdateActionState, with the hand they're restricted to
    /// (or NULL).
    active_sets: RwLock<Vec<(ActionSetKey, xr::Path)>>,
    /// Per hand and motion range (see [`Input::bone_cache`]).
    skeletal_bone_cache: [[skeletal::BoneCache; 2]; 2],
    estimated_finger_state: [Mutex<FingerState>; 2],
    subaction_paths: SubactionPaths,
    events: Mutex<VecDeque<InputEvent>>,
    /// Held while actions are set up, so legacy actions (set up from the frame thread) and an
    /// action manifest are never both attached to a session.
    action_setup: Mutex<()>,
    left_handed: AtomicBool,
    /// The properties of the most recently seen controller interaction profile. The HMD's
    /// identity is derived from this, so it needs to survive session restarts (which reset the
    /// device list) and controllers going to sleep.
    last_profile_properties: Mutex<Option<&'static ProfileProperties>>,
}

struct InputEvent {
    ty: vr::EVREventType,
    index: vr::TrackedDeviceIndex_t,
    data: vr::VREvent_Controller_t,
}

#[derive(Debug)]
struct Action {
    path: String,
}

struct WriteOnDrop<T> {
    value: ManuallyDrop<T>,
    ptr: *mut T,
}

impl<T: Default> WriteOnDrop<T> {
    fn new(ptr: *mut T) -> Self {
        Self {
            value: Default::default(),
            ptr,
        }
    }
}

impl<T> Drop for WriteOnDrop<T> {
    fn drop(&mut self) {
        unsafe {
            let val = ManuallyDrop::take(&mut self.value);
            self.ptr.write(val);
        }
    }
}

/// For `action.state(..).unwrap_or_else(inactive_state)`: an action whose state can't be read,
/// e.g. because the runtime lost the session, reads as inactive.
fn inactive_state<T: xr::ActionInput + Default>(error: xr::sys::Result) -> xr::ActionState<T> {
    crate::warn_once!("Failed to get action state: {}", error);
    xr::ActionState {
        current_state: T::default(),
        changed_since_last_sync: false,
        last_change_time: xr::Time::from_nanos(0),
        is_active: false,
    }
}

impl<C: openxr_data::Compositor> Input<C> {
    pub fn new(openxr: Arc<OpenXrData<C>>) -> Self {
        let mut map = SlotMap::with_key();
        let left_hand_key = map.insert(c"/user/hand/left".into());
        let right_hand_key = map.insert(c"/user/hand/right".into());
        let subaction_paths = SubactionPaths::new(&openxr.instance);

        // Until controllers actually show up, the HMD identity comes from whatever was bound at
        // init.
        let probed_profile = [Hand::Left, Hand::Right]
            .into_iter()
            .filter_map(|hand| openxr.probed_profiles.get(hand))
            .filter_map(|name| ProfileData::for_profile_name(name).map(|d| d.properties))
            .min_by_key(|properties| is_simple_controller(properties));

        Self {
            openxr,
            vtables: Default::default(),
            input_source_map: RwLock::new(map),
            action_map: Default::default(),
            set_map: Default::default(),
            loaded_actions_path: OnceLock::new(),
            left_hand_key,
            right_hand_key,
            legacy_state: Default::default(),
            skeletal_tracking_level: RwLock::new([vr::EVRSkeletalTrackingLevel::Estimated; 2]),
            active_sets: Default::default(),
            skeletal_bone_cache: Default::default(),
            estimated_finger_state: [
                Mutex::new(FingerState::new()),
                Mutex::new(FingerState::new()),
            ],
            subaction_paths,
            events: Mutex::default(),
            action_setup: Mutex::default(),
            left_handed: false.into(),
            last_profile_properties: Mutex::new(probed_profile),
        }
    }

    fn get_subaction_path(&self, hand: Hand) -> xr::Path {
        match hand {
            Hand::Left => self.subaction_paths.left,
            Hand::Right => self.subaction_paths.right,
        }
    }

    fn subaction_path_from_handle(&self, handle: vr::VRInputValueHandle_t) -> Option<xr::Path> {
        if handle == vr::k_ulInvalidInputValueHandle {
            Some(xr::Path::NULL)
        } else {
            let key = InputSourceKey::from(KeyData::from_ffi(handle));
            self.hand_from_key(key)
                .map(|hand| self.get_subaction_path(hand))
        }
    }

    /// Determine which hand an input source key belongs to, including
    /// specific input paths like `/user/hand/right/input/trigger`.
    fn hand_from_key(&self, key: InputSourceKey) -> Option<Hand> {
        if key == self.left_hand_key {
            return Some(Hand::Left);
        }
        if key == self.right_hand_key {
            return Some(Hand::Right);
        }
        let map = self.input_source_map.read().unwrap();
        map.get(key).and_then(|path| {
            let s = path.to_string_lossy();
            if s.starts_with("/user/hand/left") {
                Some(Hand::Left)
            } else if s.starts_with("/user/hand/right") {
                Some(Hand::Right)
            } else {
                None
            }
        })
    }

    /// Get or create a handle for a specific input source path
    /// (e.g., `/user/hand/right/input/trigger/click`).
    fn get_or_create_input_source_handle(&self, path_str: &str) -> u64 {
        let c_path = CString::new(path_str).unwrap();
        {
            let guard = self.input_source_map.read().unwrap();
            if let Some((key, _)) = guard
                .iter()
                .find(|(_, src)| src.as_c_str() == c_path.as_c_str())
            {
                return key.data().as_ffi();
            }
        }
        let mut guard = self.input_source_map.write().unwrap();
        if let Some((key, _)) = guard
            .iter()
            .find(|(_, src)| src.as_c_str() == c_path.as_c_str())
        {
            key.data().as_ffi()
        } else {
            let key = guard.insert(c_path);
            key.data().as_ffi()
        }
    }

    /// Collect all action keys that share the same input suffix across
    /// `_left` / `_right` / base action set variants.
    fn related_action_keys(&self, action_key: ActionKey) -> Vec<ActionKey> {
        let guard = self.action_map.read().unwrap();
        let mut keys = vec![action_key];
        let action_path = guard
            .get(action_key)
            .map(|a| a.path.clone())
            .unwrap_or_default();

        if !action_path.is_empty()
            && let Some((set_part, input_part)) = action_path.split_once("/in/")
        {
            let suffix = format!("/in/{input_part}");
            let set_candidates: Vec<String> = if let Some(base) = set_part.strip_suffix("_left") {
                vec![base.to_string(), format!("{base}_right")]
            } else if let Some(base) = set_part.strip_suffix("_right") {
                vec![base.to_string(), format!("{base}_left")]
            } else {
                vec![format!("{set_part}_left"), format!("{set_part}_right")]
            };

            for set_candidate in set_candidates {
                let candidate = format!("{set_candidate}{suffix}");
                if let Some((key, _)) = guard.iter().find(|(_, a)| a.path == candidate)
                    && !keys.contains(&key)
                {
                    keys.push(key);
                }
            }
        }

        keys
    }

    /// Resolve the currently active interaction profile, preferring the
    /// runtime-reported profile over the profile with the most bindings.
    /// Returns `None` only when no profile is known at all.
    fn resolve_active_profile(
        &self,
        session_data: &openxr_data::SessionData,
        loaded: &ManifestLoadedActions,
    ) -> Option<xr::Path> {
        let runtime_profile = {
            let devices = session_data.input_data.devices.read().unwrap();
            [Hand::Left, Hand::Right].into_iter().find_map(|h| {
                devices
                    .get_controller(h)
                    .map(|dev| dev.profile_path)
                    .filter(|p| *p != xr::Path::NULL)
            })
        }
        .or_else(|| {
            [Hand::Left, Hand::Right].into_iter().find_map(|h| {
                session_data
                    .session
                    .current_interaction_profile(self.get_subaction_path(h))
                    .ok()
                    .filter(|p| *p != xr::Path::NULL)
            })
        });

        let simple_profile = self
            .openxr
            .instance
            .string_to_path("/interaction_profiles/khr/simple_controller")
            .ok();

        let is_simple = |p: xr::Path| simple_profile.map(|sp| p == sp).unwrap_or(false);

        let preferred_profile = loaded
            .per_profile_input_paths
            .iter()
            .max_by_key(|(profile, map)| {
                let total_paths: usize = map.values().map(Vec::len).sum();
                let is_non_simple = simple_profile.map(|sp| **profile != sp).unwrap_or(true);
                (is_non_simple as usize, total_paths)
            })
            .map(|(p, _)| *p);

        match runtime_profile {
            Some(profile) if !is_simple(profile) => Some(profile),
            Some(_) => preferred_profile.or(runtime_profile),
            None => preferred_profile,
        }
    }

    fn state_from_bindings_left_right(
        &self,
        action: vr::VRActionHandle_t,
    ) -> Option<(xr::ActionState<bool>, vr::VRInputValueHandle_t)> {
        debug_assert!(self.left_hand_key.0.as_ffi() != 0);
        debug_assert!(self.right_hand_key.0.as_ffi() != 0);
        let left_state = self.state_from_bindings(action, self.left_hand_key.0.as_ffi());
        let right_state = self.state_from_bindings(action, self.right_hand_key.0.as_ffi());
        match (left_state, right_state) {
            (Some((left, _)), Some((right, _))) => {
                if binding_rank(&right) > binding_rank(&left) {
                    right_state
                } else {
                    left_state
                }
            }
            (state @ Some(_), None) | (None, state) => state,
        }
    }

    fn state_from_bindings(
        &self,
        action: vr::VRActionHandle_t,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> Option<(xr::ActionState<bool>, vr::VRInputValueHandle_t)> {
        let subaction = self.subaction_path_from_handle(restrict_to_device)?;
        if subaction == xr::Path::NULL {
            return self.state_from_bindings_left_right(action);
        }

        let session = self.openxr.session_data.get();
        let LoadedActions::Manifest(loaded_actions) = session.input_data.actions.get()? else {
            return None;
        };

        let interaction_profile = session
            .session
            .current_interaction_profile(subaction)
            .ok()?;
        let bindings = loaded_actions
            .try_get_bindings(action, interaction_profile)
            .ok()?;

        let mut best_state: Option<xr::ActionState<bool>> = None;

        for x in bindings.iter() {
            let Ok(Some(state)) = x.state(&session, subaction) else {
                continue;
            };
            if !state.is_active {
                continue;
            }

            if best_state.is_none_or(|best| binding_rank(&state) > binding_rank(&best)) {
                best_state = Some(state);
            }

            if state.current_state {
                break;
            }
        }

        best_state.map(|x| (x, restrict_to_device))
    }
}

impl<C: openxr_data::Compositor> Input<C> {
    /// fUpdateTime for an action state: when it changed, relative to now. OpenXR samples inputs
    /// on every sync, so an unchanged state was updated just now. The age is capped, so an app
    /// that stalled doesn't go looking for poses from the distant past.
    fn update_time(&self, changed: bool, last_change_time: xr::Time) -> f32 {
        const MAX_AGE_SECS: f64 = 1.0;
        if !changed || last_change_time.as_nanos() <= 0 {
            return 0.0;
        }
        let now = self.openxr.time_from_now(0.0);
        let age = now.as_nanos().saturating_sub(last_change_time.as_nanos()) as f64 / 1e9;
        -(age.clamp(0.0, MAX_AGE_SECS) as f32)
    }

    /// Updates a Vector1 action's state for each subaction (left, right, unrestricted). Like for
    /// Vector2 actions, this happens once per UpdateActionState, since OpenVR deltas are relative
    /// to the previous one, and must neither be consumed by reads nor shared between subactions.
    fn sync_vector1(
        &self,
        session_data: &SessionData,
        loaded: &ManifestLoadedActions,
        key: ActionKey,
        action: &xr::Action<f32>,
        state: &Mutex<[vr::InputAnalogActionData_t; 3]>,
        masked: [bool; 2],
    ) {
        let has_custom_bindings = loaded.actions_with_custom_bindings.contains(&key);
        let mut next = [vr::InputAnalogActionData_t::default(); 3];
        let hands = [
            (self.subaction_paths.left, self.left_hand_key),
            (self.subaction_paths.right, self.right_hand_key),
        ];
        for ((slot, (path, hand_key)), masked) in next.iter_mut().zip(hands).zip(masked) {
            if masked {
                continue;
            }
            let mut current = action
                .state(&session_data.session, path)
                .unwrap_or_else(inactive_state);
            let mut origin = hand_key.data().as_ffi();

            // It's generally not clear how SteamVR handles float actions with multiple bindings,
            // so emulate OpenXR, which takes the maximum among active bindings.
            if has_custom_bindings
                && let Some((binding, source)) =
                    self.state_from_bindings(key.data().as_ffi(), origin)
                && binding.is_active
                && (binding.current_state && current.current_state != 1.0 || !current.is_active)
            {
                current = xr::ActionState {
                    current_state: if binding.current_state { 1.0 } else { 0.0 },
                    is_active: true,
                    changed_since_last_sync: binding.changed_since_last_sync,
                    last_change_time: binding.last_change_time,
                };
                origin = source;
            }

            if current.is_active {
                *slot = vr::InputAnalogActionData_t {
                    bActive: true,
                    activeOrigin: origin,
                    x: current.current_state,
                    fUpdateTime: self
                        .update_time(current.changed_since_last_sync, current.last_change_time),
                    ..Default::default()
                };
            }
        }
        // Like OpenXR, report the largest value when unrestricted. Left wins ties.
        next[2] = if next[1].bActive && (!next[0].bActive || next[1].x.abs() > next[0].x.abs()) {
            next[1]
        } else {
            next[0]
        };

        let mut previous = state.lock().unwrap();
        for (next, previous) in next.iter_mut().zip(previous.iter()) {
            next.deltaX = next.x - previous.x;
        }
        *previous = next;
    }

    /// Emulates action set priorities. On each hand, an action is masked if every input source
    /// it's bound to is also bound in an active set of a higher priority, like SteamVR disables
    /// the bindings of those sources. SteamVR masks bindings rather than actions, but an OpenXR
    /// action's state can't be split by binding, so an action with a source that isn't masked
    /// stays entirely active.
    fn mask_by_priority(
        &self,
        session_data: &SessionData,
        loaded: &ManifestLoadedActions,
        set_map: &SlotMap<ActionSetKey, String>,
        set_priorities: &[(ActionSetKey, [bool; 2], i32)],
        profiles: [xr::Path; 2],
    ) -> HashSet<(ActionKey, usize)> {
        let mut masked = HashSet::new();
        if set_priorities
            .iter()
            .all(|(_, _, priority)| *priority == set_priorities[0].2)
        {
            return masked;
        }

        let sources = session_data
            .input_data
            .action_sources
            .get_or_init(|| self.action_sources(loaded, set_map));
        for (hand, profile) in profiles.into_iter().enumerate() {
            let Some(actions) = sources.sources.get(&profile) else {
                continue;
            };
            let priority = |action| {
                let set = *sources.sets.get(action)?;
                set_priorities
                    .iter()
                    .filter(|(key, hands, _)| *key == set && hands[hand])
                    .map(|(_, _, priority)| *priority)
                    .max()
            };

            // The highest priority each source is bound with.
            let mut claims = HashMap::<xr::Path, i32>::new();
            for (action, action_sources) in actions {
                let Some(priority) = priority(action) else {
                    continue;
                };
                for source in &action_sources[hand] {
                    let claim = claims.entry(*source).or_insert(priority);
                    *claim = (*claim).max(priority);
                }
            }

            for (action, action_sources) in actions {
                let Some(priority) = priority(action) else {
                    continue;
                };
                let action_sources = &action_sources[hand];
                if !action_sources.is_empty()
                    && action_sources
                        .iter()
                        .all(|source| claims[source] > priority)
                {
                    masked.insert((action, hand));
                }
            }
        }
        masked
    }

    fn action_sources(
        &self,
        loaded: &ManifestLoadedActions,
        set_map: &SlotMap<ActionSetKey, String>,
    ) -> ActionSources {
        let action_map = self.action_map.read().unwrap();
        let mut sources = ActionSources::default();

        // Input path -> hand index and source.
        let mut source_paths = HashMap::<xr::Path, Option<(usize, xr::Path)>>::new();
        let mut source_of = |path: xr::Path| {
            *source_paths.entry(path).or_insert_with(|| {
                let path = self.openxr.instance.path_to_string(path).ok()?;
                let (device, kind, component, _) = parse_input_path(&path);
                if kind != "input" {
                    return None;
                }
                let hand = ["/user/hand/left", "/user/hand/right"]
                    .iter()
                    .position(|hand| *hand == device)?;
                let source = format!("{device}/input/{component}");
                Some((hand, self.openxr.instance.string_to_path(&source).ok()?))
            })
        };

        for (key, data) in &loaded.actions {
            if !matches!(
                data,
                ActionData::Bool(_) | ActionData::Vector1 { .. } | ActionData::Vector2 { .. }
            ) {
                continue;
            }
            // Only application actions, not the input actions of custom bindings.
            let Some(set) = action_map
                .get(key)
                .and_then(|action| action.path.split_once("/in/"))
                .and_then(|(set, _)| {
                    set_map
                        .iter()
                        .find_map(|(set_key, name)| (name == set).then_some(set_key))
                })
            else {
                continue;
            };
            sources.sets.insert(key, set);

            // The inputs of custom and inverted bindings are recorded under the action too.
            for (profile, input_paths) in &loaded.per_profile_input_paths {
                let mut hands: [Vec<xr::Path>; 2] = Default::default();
                for path in input_paths.get(key).into_iter().flatten() {
                    if let Some((hand, source)) = source_of(*path)
                        && !hands[hand].contains(&source)
                    {
                        hands[hand].push(source);
                    }
                }
                if hands.iter().any(|sources| !sources.is_empty()) {
                    sources
                        .sources
                        .entry(*profile)
                        .or_default()
                        .insert(key, hands);
                }
            }
        }
        sources
    }

    /// The state of a boolean action that a higher priority action set masks on a hand, now or
    /// as of the previous sync. Masked hands don't count, and masking a pressed hand releases it.
    fn masked_bool_state(
        &self,
        handle: vr::VRActionHandle_t,
        action: &xr::Action<bool>,
        session_data: &SessionData,
        subaction_path: xr::Path,
        mask: HandMask,
    ) -> (xr::ActionState<bool>, vr::VRInputValueHandle_t) {
        let mut state = xr::ActionState {
            current_state: false,
            changed_since_last_sync: false,
            last_change_time: xr::Time::from_nanos(0),
            is_active: false,
        };
        let mut origin = vr::k_ulInvalidInputValueHandle;
        let mut best_rank = None;
        let mut previous = false;
        let hands = [
            (self.subaction_paths.left, self.left_hand_key),
            (self.subaction_paths.right, self.right_hand_key),
        ];
        for (hand, (path, hand_key)) in hands.into_iter().enumerate() {
            if subaction_path != xr::Path::NULL && subaction_path != path {
                continue;
            }
            // Don't evaluate custom bindings of a hand that stays masked: like SteamVR's disabled
            // bindings, they mustn't see inputs meant for the higher priority set (e.g. toggles).
            if mask.previous[hand] && mask.current[hand] {
                continue;
            }

            let hand_origin = hand_key.data().as_ffi();
            let native = action
                .state(&session_data.session, path)
                .unwrap_or_else(inactive_state);
            let (hand_state, hand_origin) = match self.state_from_bindings(handle, hand_origin) {
                Some((binding, source))
                    if binding.is_active
                        && (!native.is_active
                            || binding_rank(&binding) > binding_rank(&native)) =>
                {
                    (binding, source)
                }
                _ => (native, hand_origin),
            };

            // A hand that was masked counts as unchanged, so unmasking a held input isn't a press.
            previous |= if mask.previous[hand] {
                hand_state.current_state
            } else {
                hand_state.current_state != hand_state.changed_since_last_sync
            };
            if mask.current[hand] || !hand_state.is_active {
                continue;
            }

            state.is_active = true;
            state.current_state |= hand_state.current_state;
            if hand_state.last_change_time.as_nanos() > state.last_change_time.as_nanos() {
                state.last_change_time = hand_state.last_change_time;
            }
            let rank = binding_rank(&hand_state);
            if best_rank.is_none_or(|best| rank > best) {
                best_rank = Some(rank);
                origin = hand_origin;
            }
        }
        state.changed_since_last_sync = state.current_state != previous;
        (state, origin)
    }

    /// The hand whose natively bound inputs drive an unrestricted read of a boolean action, if
    /// any is active.
    fn native_bool_origin(
        &self,
        action: &xr::Action<bool>,
        session: &xr::Session<xr::AnyGraphics>,
    ) -> vr::VRInputValueHandle_t {
        [
            (self.subaction_paths.left, self.left_hand_key),
            (self.subaction_paths.right, self.right_hand_key),
        ]
        .into_iter()
        .filter_map(|(path, key)| {
            let state = action.state(session, path).unwrap_or_else(inactive_state);
            state.is_active.then(|| (binding_rank(&state), key))
        })
        // Left wins ties.
        .reduce(|best, hand| if hand.0 > best.0 { hand } else { best })
        .map_or(vr::k_ulInvalidInputValueHandle, |(_, key)| {
            key.data().as_ffi()
        })
    }
}

/// Ranking used to decide which of an action's bindings should represent it. Highest wins.
fn binding_rank(state: &xr::ActionState<bool>) -> (bool, bool, i64) {
    (
        state.current_state,
        state.changed_since_last_sync,
        state.last_change_time.as_nanos(),
    )
}

pub struct InputSessionData {
    actions: OnceLock<LoadedActions>,
    estimated_skeleton_actions: OnceLock<SkeletalInputActionData>,
    pose_data: PoseData,
    devices: RwLock<TrackedDeviceList>,
    action_sources: OnceLock<ActionSources>,
    priority_masks: Mutex<PriorityMasks>,
}

/// The input sources each application action is bound to, per interaction profile and hand.
/// Sources are components (`/user/hand/<hand>/input/<component>`), which is what action set
/// priorities apply to.
#[derive(Default)]
struct ActionSources {
    sets: SecondaryMap<ActionKey, ActionSetKey>,
    sources: HashMap<xr::Path, SecondaryMap<ActionKey, [Vec<xr::Path>; 2]>>,
}

/// The actions masked on each hand (by index) by a higher priority action set, as of the latest
/// and the previous UpdateActionState.
#[derive(Default)]
struct PriorityMasks {
    current: HashSet<(ActionKey, usize)>,
    previous: HashSet<(ActionKey, usize)>,
}

#[derive(Clone, Copy)]
struct HandMask {
    current: [bool; 2],
    previous: [bool; 2],
}

impl InputSessionData {
    pub(crate) fn new(
        instance: &xr::Instance,
        system_id: xr::SystemId,
        session: &xr::Session<xr::AnyGraphics>,
    ) -> Self {
        let paths = SubactionPaths::new(instance);
        let hand_tracking = instance
            .supports_hand_tracking(system_id)
            .is_ok_and(|supported| supported);
        Self {
            actions: OnceLock::new(),
            estimated_skeleton_actions: OnceLock::new(),
            pose_data: PoseData::new(instance, paths.left, paths.right),
            devices: RwLock::new(TrackedDeviceList::new(session, hand_tracking)),
            action_sources: OnceLock::new(),
            priority_masks: Mutex::default(),
        }
    }

    fn hand_mask(&self, key: ActionKey) -> Option<HandMask> {
        let masks = self.priority_masks.lock().unwrap();
        let get = |masked: &HashSet<_>| [0, 1].map(|hand| masked.contains(&(key, hand)));
        let mask = HandMask {
            current: get(&masks.current),
            previous: get(&masks.previous),
        };
        mask.current
            .iter()
            .chain(&mask.previous)
            .any(|masked| *masked)
            .then_some(mask)
    }

    #[inline]
    fn get_loaded_actions(&self) -> Option<&ManifestLoadedActions> {
        match self.actions.get()? {
            LoadedActions::Manifest(m) => Some(m),
            _ => None,
        }
    }
    #[inline]
    fn get_legacy_actions(&self) -> Option<&LegacyActionData> {
        match self.actions.get()? {
            LoadedActions::Legacy(l) => Some(l),
            _ => None,
        }
    }

    pub(crate) fn interaction_profile_changed(&self) {
        // If the interaction profile changes the offsets must be updated too
        // Delete the current raw spaces so they can be recreated later
        self.pose_data.reset_spaces();
    }

    /// Forgets every pose located in the tracking spaces, for when they move.
    pub(crate) fn forget_poses(&self) {
        for device in self.devices.read().unwrap().iter() {
            device.forget_poses();
        }
        for space in [&self.pose_data.left_space, &self.pose_data.right_space] {
            space.pose_history.lock().unwrap().clear();
        }
    }
}
enum ActionData {
    Bool(xr::Action<bool>),
    Vector1 {
        action: xr::Action<f32>,
        /// Left, right, unrestricted. See [Input::sync_vector1].
        state: Mutex<[vr::InputAnalogActionData_t; 3]>,
        /// For actions that only feed custom bindings. Those read the action themselves, so the
        /// state isn't synced.
        input_only: bool,
    },
    Vector2 {
        action: xr::Action<xr::Vector2f>,
        data: Vector2ActionData,
    },
    Pose,
    Skeleton(Hand),
    Haptic(xr::Action<xr::Haptic>),
}

#[derive(Debug, Default)]
struct BoundPose {
    left: Option<BoundPoseType>,
    right: Option<BoundPoseType>,
}

#[derive(Clone, Copy, Debug)]
pub enum BoundPoseType {
    /// Equivalent to what is returned by WaitGetPoses, this appears to be the same or close to
    /// OpenXR's grip pose in the same position as the aim pose.
    /// "All tracked devices also get two pose components registered regardless of what render model they use: /pose/raw and /pose/tip"
    /// "By default, both are set to the unaltered pose of the device."
    /// ~https://github.com/ValveSoftware/openvr/wiki/Input-Profiles#pose-components
    Raw,
    /// "If you provide /pose/tip in your rendermodel you should set it to the position and rotation that are appropriate for pointing (i.e. with a laser pointer) with your controller."
    /// ~https://github.com/ValveSoftware/openvr/wiki/Input-Profiles#pose-components
    Tip,
    Base,
    /// Not sure why games still use this, but having it be equivalent to raw seems to work fine.
    Gdc2015,
    Handgrip,
    Grip,
    OpenxrHandmodel,
    OpenxrPinch,
    OpenxrPoke,
    OpenxrAim,
    OpenxrGrip,
}

macro_rules! get_action_from_handle {
    ($self:expr, $handle:expr, $session_data:ident, $action:ident) => {
        get_action_from_handle!($self, $handle, $session_data, $action, loaded)
    };

    ($self:expr, $handle:expr, $session_data:ident, $action:ident, $loaded:ident) => {
        if $self.openxr.is_lost() {
            return vr::EVRInputError::IPCError;
        }
        let $session_data = $self.openxr.session_data.get();
        let Some($loaded) = $session_data.input_data.get_loaded_actions() else {
            return vr::EVRInputError::InvalidHandle;
        };

        let $action = match $loaded.try_get_action($handle) {
            Ok(action) => action,
            Err(e) => return e,
        };
    };
}

macro_rules! get_subaction_path {
    ($self:expr, $restrict:expr, $data:expr) => {
        match $self.subaction_path_from_handle($restrict) {
            Some(p) => p,
            None => {
                unsafe {
                    $data.write(Default::default());
                }
                return vr::EVRInputError::None;
            }
        }
    };
}

impl<C: openxr_data::Compositor> vr::IVRInput011_Interface for Input<C> {
    fn GetBindingVariant(
        &self,
        _: vr::VRInputValueHandle_t,
        _: *mut c_char,
        _: u32,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("GetBindingVariant");
        vr::EVRInputError::None
    }
    fn OpenBindingUI(
        &self,
        _: *const c_char,
        _: vr::VRActionSetHandle_t,
        _: vr::VRInputValueHandle_t,
        _: bool,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("OpenBindingUI");
        vr::EVRInputError::None
    }
    fn IsUsingLegacyInput(&self) -> bool {
        return self
            .openxr
            .session_data
            .get()
            .input_data
            .get_legacy_actions()
            .is_some();
    }
    fn GetComponentStateForBinding(
        &self,
        _: *const c_char,
        _: *const c_char,
        _: *const vr::InputBindingInfo_t,
        _: u32,
        _: u32,
        _: *mut vr::RenderModel_ComponentState_t,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("GetComponentStateForBinding");
        vr::EVRInputError::None
    }
    fn ShowBindingsForActionSet(
        &self,
        _: *mut vr::VRActiveActionSet_t,
        _: u32,
        _: u32,
        _: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("ShowBindingsForActionSet");
        vr::EVRInputError::None
    }
    fn ShowActionOrigins(
        &self,
        _: vr::VRActionSetHandle_t,
        _: vr::VRActionHandle_t,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("ShowActionOrigins");
        vr::EVRInputError::None
    }
    fn GetActionBindingInfo(
        &self,
        action_handle: vr::VRActionHandle_t,
        binding_info: *mut vr::InputBindingInfo_t,
        binding_info_size: u32,
        binding_info_count: u32,
        returned_binding_info_count: *mut u32,
    ) -> vr::EVRInputError {
        let session_data = self.openxr.session_data.get();
        let Some(loaded) = session_data.input_data.get_loaded_actions() else {
            if !returned_binding_info_count.is_null() {
                unsafe { *returned_binding_info_count = 0 };
            }
            return vr::EVRInputError::InvalidHandle;
        };

        let action_key = ActionKey::from(KeyData::from_ffi(action_handle));
        if !loaded.actions.contains_key(action_key) {
            // Action handle is valid (from GetActionHandle) but has no loaded bindings.
            // Return 0 bindings with None error instead of InvalidHandle, matching SteamVR behavior.
            if !returned_binding_info_count.is_null() {
                unsafe { *returned_binding_info_count = 0 };
            }
            let guard = self.action_map.read().unwrap();
            if guard.contains_key(action_key) {
                return vr::EVRInputError::None;
            }
            return vr::EVRInputError::InvalidHandle;
        }

        let related_action_keys = self.related_action_keys(action_key);

        let active_profile = self.resolve_active_profile(&session_data, loaded);

        let paths_for_profile = |profile: xr::Path| -> Vec<xr::Path> {
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            if let Some(map) = loaded.per_profile_input_paths.get(&profile) {
                for key in &related_action_keys {
                    if let Some(paths) = map.get(*key) {
                        for &p in paths {
                            if seen.insert(p) {
                                out.push(p);
                            }
                        }
                    }
                }
            }
            out
        };

        let paths: Vec<xr::Path> = if let Some(p) = active_profile {
            paths_for_profile(p)
        } else {
            let mut seen = HashSet::new();
            loaded
                .per_profile_input_paths
                .values()
                .flat_map(|m| {
                    related_action_keys
                        .iter()
                        .flat_map(|key| m.get(*key).into_iter().flatten().copied::<xr::Path>())
                })
                .filter(|p| seen.insert(*p))
                .collect()
        };

        if binding_info.is_null() || binding_info_count == 0 {
            if !returned_binding_info_count.is_null() {
                unsafe { *returned_binding_info_count = paths.len() as u32 };
            }
            return vr::EVRInputError::None;
        }

        // IVRInput_007's struct ends before rchInputSourceType.
        const LEGACY_SIZE: usize = std::mem::offset_of!(vr::InputBindingInfo_t, rchInputSourceType);
        let binding_info_size = binding_info_size as usize;
        if binding_info_size != std::mem::size_of::<vr::InputBindingInfo_t>()
            && binding_info_size != LEGACY_SIZE
        {
            if !returned_binding_info_count.is_null() {
                unsafe { *returned_binding_info_count = 0 };
            }
            return vr::EVRInputError::InvalidParam;
        }

        let count = paths.len().min(binding_info_count as usize);
        if !returned_binding_info_count.is_null() {
            unsafe { *returned_binding_info_count = count as u32 };
        }

        let action_is_bool = matches!(loaded.actions.get(action_key), Some(ActionData::Bool(_)));
        let action_name = self
            .action_map
            .read()
            .unwrap()
            .get(action_key)
            .map(|a| a.path.clone())
            .unwrap_or_default();

        for (i, &xr_path) in paths.iter().take(count).enumerate() {
            let mut info = vr::InputBindingInfo_t::default();

            let input_path_str = self
                .openxr
                .instance
                .path_to_string(xr_path)
                .unwrap_or_default();

            let (device_path, input_kind, component, slot_raw) = parse_input_path(&input_path_str);

            let slot = if slot_raw.is_empty() {
                match component {
                    "thumbstick" | "joystick" | "trackpad" => "position",
                    _ => "",
                }
            } else {
                slot_raw
            };

            let slot = if action_is_bool
                && slot == "value"
                && matches!(component, "trigger" | "squeeze" | "grip")
            {
                "click"
            } else {
                slot
            };

            let display_component = normalize_component(component);

            let input_component_path = if input_kind.is_empty() || display_component.is_empty() {
                String::new()
            } else {
                format!("/{input_kind}/{display_component}")
            };
            copy_cstr(&mut info.rchInputPathName, &input_component_path);

            // SteamVR's touch_profile.json lists grip as type "trigger"
            let mode_name = match display_component {
                "trackpad" => "trackpad",
                "joystick" => "joystick",
                "trigger" | "grip" => "trigger",
                _ => "button",
            };
            copy_cstr(&mut info.rchModeName, mode_name);

            copy_cstr(&mut info.rchSlotName, slot);

            copy_cstr(&mut info.rchDevicePathName, device_path);

            copy_cstr(&mut info.rchInputSourceType, mode_name);

            // Both layouts are only char arrays, the legacy one is a prefix of ours.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&raw const info).cast::<u8>(),
                    binding_info.cast::<u8>().add(i * binding_info_size),
                    binding_info_size,
                );
            }

            debug!(
                "GetActionBindingInfo: action={action_name:?} raw={input_path_str} device={device_path} input={input_component_path} mode={mode_name} slot={slot}"
            );
        }

        debug!(
            "GetActionBindingInfo: action {action_name:?} → {count} binding(s) (related_actions={})",
            related_action_keys.len(),
        );
        vr::EVRInputError::None
    }
    fn GetOriginTrackedDeviceInfo(
        &self,
        handle: vr::VRInputValueHandle_t,
        info: *mut vr::InputOriginInfo_t,
        info_size: u32,
    ) -> vr::EVRInputError {
        if info.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if info_size as usize != std::mem::size_of::<vr::InputOriginInfo_t>() {
            crate::warn_once!(
                "GetOriginTrackedDeviceInfo: unexpected size {}, expected {}",
                info_size,
                std::mem::size_of::<vr::InputOriginInfo_t>()
            );
            return vr::EVRInputError::InvalidParam;
        }

        let key = InputSourceKey::from(KeyData::from_ffi(handle));
        let source_path = {
            let map = self.input_source_map.read().unwrap();
            if !map.contains_key(key) {
                debug!("GetOriginTrackedDeviceInfo: invalid handle 0x{:x}", handle);
                unsafe {
                    info.write(Default::default());
                }
                return vr::EVRInputError::InvalidHandle;
            }
            map.get(key)
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        };

        let index = match self.hand_from_key(key) {
            Some(hand) => hand as u32,
            None => {
                debug!(
                    "GetOriginTrackedDeviceInfo: unknown device for handle 0x{:x} path={}",
                    handle, source_path
                );
                unsafe {
                    info.write(Default::default());
                }
                return vr::EVRInputError::InvalidDevice;
            }
        };

        let (_, _, raw_component, _) = parse_input_path(&source_path);
        let component_name = normalize_component(raw_component);

        let mut render_model_component: [c_char; 128] = [0; 128];
        copy_cstr(&mut render_model_component, component_name);

        debug!(
            "GetOriginTrackedDeviceInfo: handle=0x{:x} path={} index={} component={}",
            handle, source_path, index, component_name
        );

        unsafe {
            *info = vr::InputOriginInfo_t {
                devicePath: handle,
                trackedDeviceIndex: index,
                rchRenderModelComponentName: render_model_component,
            };
        }
        vr::EVRInputError::None
    }
    fn GetOriginLocalizedName(
        &self,
        origin: vr::VRInputValueHandle_t,
        name_array: *mut c_char,
        name_array_size: u32,
        strings_to_get: i32,
    ) -> vr::EVRInputError {
        if name_array.is_null() || name_array_size == 0 {
            return vr::EVRInputError::InvalidParam;
        }

        let key = InputSourceKey::from(KeyData::from_ffi(origin));
        let hand = self.hand_from_key(key);

        let source_path = {
            let map = self.input_source_map.read().unwrap();
            map.get(key)
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        };

        let mut parts: Vec<String> = Vec::new();

        // k_VRInputString_Hand = 0x01
        if strings_to_get & 0x01 != 0
            && let Some(h) = hand
        {
            parts.push(
                match h {
                    Hand::Left => "Left Hand",
                    Hand::Right => "Right Hand",
                }
                .into(),
            );
        }
        // k_VRInputString_ControllerType = 0x02
        if strings_to_get & 0x02 != 0 {
            let session_data = self.openxr.session_data.get();
            let friendly = hand
                .and_then(|h| {
                    session_data
                        .session
                        .current_interaction_profile(self.get_subaction_path(h))
                        .ok()
                        .filter(|p| *p != xr::Path::NULL)
                        .and_then(|p| self.openxr.instance.path_to_string(p).ok())
                })
                .and_then(|s| s.rsplit('/').next().map(|n| n.replace('_', " ")))
                .unwrap_or_default();
            if !friendly.is_empty() {
                parts.push(friendly);
            }
        }
        // k_VRInputString_InputSource = 0x04
        if strings_to_get & 0x04 != 0 {
            if !source_path.is_empty() {
                parts.push(source_path);
            } else if let Some(h) = hand {
                parts.push(
                    match h {
                        Hand::Left => "/user/hand/left",
                        Hand::Right => "/user/hand/right",
                    }
                    .into(),
                );
            }
        }

        let display = parts.join(" / ");

        let out = unsafe {
            std::slice::from_raw_parts_mut(name_array as *mut u8, name_array_size as usize)
        };
        let bytes = display.as_bytes();
        let n = bytes.len().min(out.len() - 1);
        out[..n].copy_from_slice(&bytes[..n]);
        out[n] = 0;

        debug!(
            "GetOriginLocalizedName: origin=0x{:x} strings_to_get=0x{:x} result={:?}",
            origin, strings_to_get, display
        );

        vr::EVRInputError::None
    }
    fn GetActionOrigins(
        &self,
        _action_set_handle: vr::VRActionSetHandle_t,
        action_handle: vr::VRActionHandle_t,
        origins_out: *mut vr::VRInputValueHandle_t,
        origin_out_count: u32,
    ) -> vr::EVRInputError {
        if origins_out.is_null() || origin_out_count == 0 {
            return vr::EVRInputError::InvalidParam;
        }

        let out = unsafe { std::slice::from_raw_parts_mut(origins_out, origin_out_count as usize) };
        for slot in out.iter_mut() {
            *slot = vr::k_ulInvalidInputValueHandle;
        }

        let session_data = self.openxr.session_data.get();
        let Some(loaded) = session_data.input_data.get_loaded_actions() else {
            return vr::EVRInputError::InvalidHandle;
        };

        let action_key = ActionKey::from(KeyData::from_ffi(action_handle));
        if !loaded.actions.contains_key(action_key) {
            // Action handle is valid (from GetActionHandle) but has no loaded bindings.
            // Return 0 origins with None error instead of InvalidHandle, matching SteamVR behavior.
            let guard = self.action_map.read().unwrap();
            if guard.contains_key(action_key) {
                return vr::EVRInputError::None;
            }
            return vr::EVRInputError::InvalidHandle;
        }

        let related_action_keys = self.related_action_keys(action_key);

        let mut idx = 0;

        debug!(
            "GetActionOrigins: action 0x{:x} type={:?}, related_actions={}",
            action_handle,
            loaded.actions.get(action_key).map(std::mem::discriminant),
            related_action_keys.len(),
        );

        match loaded.actions.get(action_key) {
            // Skeleton actions are tied to exactly one hand - return it directly
            // without needing any active interaction profile.
            Some(ActionData::Skeleton(hand)) => {
                if idx < origin_out_count as usize {
                    out[idx] = match hand {
                        Hand::Left => self.left_hand_key.data().as_ffi(),
                        Hand::Right => self.right_hand_key.data().as_ffi(),
                    };
                    idx += 1;
                }
            }

            // Pose actions: per_profile_pose_bindings stores explicit per-hand
            // information for every loaded profile. Scan all profiles so this
            // works even before the runtime sets an active interaction profile.
            Some(ActionData::Pose) => {
                let (mut has_left, mut has_right) = (false, false);
                for profile_map in loaded.per_profile_pose_bindings.values() {
                    if let Some(bound_pose) = profile_map.get(action_key) {
                        has_left |= bound_pose.left.is_some();
                        has_right |= bound_pose.right.is_some();
                    }
                }
                // Fallback: if no pose bindings at all, check regular bindings
                // (some profiles may bind pose actions as regular component paths).
                if !has_left && !has_right {
                    has_left = true;
                    has_right = loaded
                        .per_profile_bindings
                        .values()
                        .any(|m| m.get(action_key).map(|v| !v.is_empty()).unwrap_or(false));
                }
                for (has, hand_key) in [
                    (has_left, &self.left_hand_key),
                    (has_right, &self.right_hand_key),
                ] {
                    if has && idx < origin_out_count as usize {
                        out[idx] = hand_key.data().as_ffi();
                        idx += 1;
                    }
                }
            }

            // Bool / Analog / Haptic / other actions:
            // Return specific input source handles so that
            // GetOriginTrackedDeviceInfo can report the correct component
            // (e.g. "trigger", "thumbstick") instead of a bare hand path.
            _ => {
                let active_profile = self.resolve_active_profile(&session_data, loaded);

                let (mut left_path_str, mut right_path_str): (Option<String>, Option<String>) =
                    (None, None);

                let profile_maps: Vec<_> = if let Some(ap) = active_profile {
                    loaded
                        .per_profile_input_paths
                        .get(&ap)
                        .into_iter()
                        .collect()
                } else {
                    loaded.per_profile_input_paths.values().collect()
                };

                for m in &profile_maps {
                    for key in &related_action_keys {
                        if let Some(paths) = m.get(*key) {
                            for &path in paths {
                                if let Ok(path_str) = self.openxr.instance.path_to_string(path) {
                                    if path_str.starts_with("/user/hand/left")
                                        && left_path_str.is_none()
                                    {
                                        left_path_str = Some(path_str.clone());
                                    }
                                    if path_str.starts_with("/user/hand/right")
                                        && right_path_str.is_none()
                                    {
                                        right_path_str = Some(path_str);
                                    }
                                }
                            }
                        }
                    }
                }

                // Fallback: some custom bindings with NULL input_path end up only
                // in per_profile_bindings (legacy path). Return both hands for those.
                if left_path_str.is_none() && right_path_str.is_none() {
                    let has_any = loaded.per_profile_bindings.values().any(|m| {
                        related_action_keys
                            .iter()
                            .any(|k| m.get(*k).map(|v| !v.is_empty()).unwrap_or(false))
                    });
                    if has_any {
                        for hand_key in [&self.left_hand_key, &self.right_hand_key] {
                            if idx < origin_out_count as usize {
                                out[idx] = hand_key.data().as_ffi();
                                idx += 1;
                            }
                        }
                    }
                } else {
                    for (path_opt, hand_key) in [
                        (&left_path_str, &self.left_hand_key),
                        (&right_path_str, &self.right_hand_key),
                    ] {
                        if idx < origin_out_count as usize {
                            let handle = match path_opt {
                                Some(path_str) => self.get_or_create_input_source_handle(path_str),
                                None => hand_key.data().as_ffi(),
                            };
                            out[idx] = handle;
                            idx += 1;
                        }
                    }
                }
            }
        }

        debug!(
            "GetActionOrigins: found {} origin(s) for action 0x{:x}",
            idx, action_handle
        );
        vr::EVRInputError::None
    }
    fn TriggerHapticVibrationAction(
        &self,
        action: vr::VRActionHandle_t,
        start_seconds_from_now: f32,
        duration_seconds: f32,
        frequency: f32,
        amplitude: f32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        get_action_from_handle!(self, action, session_data, action);
        let Some(subaction_path) = self.subaction_path_from_handle(restrict_to_device) else {
            return vr::EVRInputError::None;
        };

        let ActionData::Haptic(action) = action else {
            return vr::EVRInputError::WrongType;
        };

        if start_seconds_from_now > 0.0 {
            warn!("start_seconds_from_now: {start_seconds_from_now}")
        }

        let result = action.apply_feedback(
            &session_data.session,
            subaction_path,
            &xr::HapticVibration::new()
                .amplitude(amplitude.clamp(0.0, 1.0))
                .frequency(frequency)
                .duration(xr::Duration::from_nanos((duration_seconds * 1e9) as _)),
        );
        if self.openxr.check("xrApplyHapticFeedback", result).is_none() {
            return vr::EVRInputError::IPCError;
        }

        vr::EVRInputError::None
    }
    fn DecompressSkeletalBoneData(
        &self,
        _: *const c_void,
        _: u32,
        _: vr::EVRSkeletalTransformSpace,
        _: *mut vr::VRBoneTransform_t,
        _: u32,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("DecompressSkeletalBoneData");
        vr::EVRInputError::None
    }
    fn GetSkeletalBoneDataCompressed(
        &self,
        _: vr::VRActionHandle_t,
        _: vr::EVRSkeletalMotionRange,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("GetSkeletalBoneDataCompressed");
        vr::EVRInputError::None
    }
    fn GetSkeletalSummaryData(
        &self,
        action: vr::VRActionHandle_t,
        summary_type: vr::EVRSummaryType,
        data: *mut vr::VRSkeletalSummaryData_t,
    ) -> vr::EVRInputError {
        get_action_from_handle!(self, action, session_data, action);

        let ActionData::Skeleton(hand) = action else {
            return vr::EVRInputError::WrongType;
        };

        let Some(data) = (unsafe { data.as_mut() }) else {
            return vr::EVRInputError::InvalidParam;
        };

        self.get_bone_summary_from_hand_tracking(&session_data, summary_type, data, *hand);

        vr::EVRInputError::None
    }
    fn GetSkeletalBoneData(
        &self,
        handle: vr::VRActionHandle_t,
        transform_space: vr::EVRSkeletalTransformSpace,
        motion_range: vr::EVRSkeletalMotionRange,
        transform_array: *mut vr::VRBoneTransform_t,
        transform_array_count: u32,
    ) -> vr::EVRInputError {
        let out_transforms = match unsafe { bone_array(transform_array, transform_array_count) } {
            Ok(array) => array,
            Err(e) => return e,
        };

        get_action_from_handle!(self, handle, session_data, action);
        let ActionData::Skeleton(hand) = action else {
            return vr::EVRInputError::WrongType;
        };

        let Some(transforms) =
            self.get_bones_from_hand_tracking(&session_data, transform_space, *hand, motion_range)
        else {
            return vr::EVRInputError::NoData;
        };
        *out_transforms = transforms;
        vr::EVRInputError::None
    }
    fn GetSkeletalTrackingLevel(
        &self,
        action: vr::VRActionHandle_t,
        level: *mut vr::EVRSkeletalTrackingLevel,
    ) -> vr::EVRInputError {
        get_action_from_handle!(self, action, data, action);
        let ActionData::Skeleton(hand) = action else {
            return vr::EVRInputError::WrongType;
        };
        let Some(level) = (unsafe { level.as_mut() }) else {
            return vr::EVRInputError::InvalidParam;
        };

        *level = self.current_skeletal_tracking_level(&data, *hand);
        vr::EVRInputError::None
    }
    fn GetSkeletalReferenceTransforms(
        &self,
        handle: vr::VRActionHandle_t,
        space: vr::EVRSkeletalTransformSpace,
        pose: vr::EVRSkeletalReferencePose,
        transform_array: *mut vr::VRBoneTransform_t,
        transform_array_count: u32,
    ) -> vr::EVRInputError {
        // As far as I'm aware this is only/mainly used by HL:A
        // For some reason it is required to position the wrist bone at all times, at least when it comes to Quest controllers

        let transforms = match unsafe { bone_array(transform_array, transform_array_count) } {
            Ok(array) => array,
            Err(e) => return e,
        };

        get_action_from_handle!(self, handle, session_data, action);
        let ActionData::Skeleton(hand) = action else {
            return vr::EVRInputError::WrongType;
        };

        self.get_reference_transforms(*hand, space, pose, transforms)
            .err()
            .unwrap_or(vr::EVRInputError::None)
    }
    fn GetBoneName(
        &self,
        action: vr::VRActionHandle_t,
        bone: vr::BoneIndex_t,
        name: *mut c_char,
        name_size: u32,
    ) -> vr::EVRInputError {
        get_action_from_handle!(self, action, session_data, action);
        let ActionData::Skeleton(hand) = action else {
            return vr::EVRInputError::WrongType;
        };
        let Some(bone_name) = skeletal::bone_name(bone, *hand) else {
            return vr::EVRInputError::InvalidBoneIndex;
        };
        if name.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if (name_size as usize) <= bone_name.len() {
            return vr::EVRInputError::BufferTooSmall;
        }

        copy_cstr(
            unsafe { std::slice::from_raw_parts_mut(name, name_size as usize) },
            &bone_name,
        );
        vr::EVRInputError::None
    }
    fn GetBoneHierarchy(
        &self,
        action: vr::VRActionHandle_t,
        parents: *mut vr::BoneIndex_t,
        parent_count: u32,
    ) -> vr::EVRInputError {
        get_action_from_handle!(self, action, session_data, action);
        if !matches!(action, ActionData::Skeleton(_)) {
            return vr::EVRInputError::WrongType;
        }
        if parents.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if parent_count < HandSkeletonBone::Count as u32 {
            return vr::EVRInputError::BufferTooSmall;
        }

        unsafe { std::slice::from_raw_parts_mut(parents, skeletal::BONE_PARENTS.len()) }
            .copy_from_slice(&skeletal::BONE_PARENTS);
        vr::EVRInputError::None
    }
    fn GetBoneCount(&self, handle: vr::VRActionHandle_t, count: *mut u32) -> vr::EVRInputError {
        get_action_from_handle!(self, handle, session_data, action);
        if !matches!(action, ActionData::Skeleton { .. }) {
            return vr::EVRInputError::WrongType;
        }

        let Some(count) = (unsafe { count.as_mut() }) else {
            return vr::EVRInputError::InvalidParam;
        };
        *count = skeletal::HandSkeletonBone::Count as u32;

        vr::EVRInputError::None
    }
    fn SetDominantHand(&self, hand: vr::ETrackedControllerRole) -> vr::EVRInputError {
        let left = match hand {
            vr::ETrackedControllerRole::LeftHand => true,
            vr::ETrackedControllerRole::RightHand => false,
            _ => return vr::EVRInputError::InvalidParam,
        };
        self.left_handed.store(left, Ordering::Relaxed);
        vr::EVRInputError::None
    }
    fn GetDominantHand(&self, hand: *mut vr::ETrackedControllerRole) -> vr::EVRInputError {
        if hand.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        let role = if self.left_handed.load(Ordering::Relaxed) {
            vr::ETrackedControllerRole::LeftHand
        } else {
            vr::ETrackedControllerRole::RightHand
        };
        unsafe { hand.write(role) };
        vr::EVRInputError::None
    }
    fn GetSkeletalActionData(
        &self,
        action: vr::VRActionHandle_t,
        action_data: *mut vr::InputSkeletalActionData_t,
        _action_data_size: u32,
    ) -> vr::EVRInputError {
        //assert_eq!(
        //    action_data_size as usize,
        //    std::mem::size_of::<vr::InputSkeletalActionData_t>()
        //);

        let handle = action;
        get_action_from_handle!(self, handle, session_data, action, loaded);
        let ActionData::Skeleton(hand) = action else {
            return vr::EVRInputError::WrongType;
        };
        if action_data.is_null() {
            return vr::EVRInputError::InvalidParam;
        }

        let active = self.is_skeleton_active(&session_data, loaded, handle, *hand);
        let origin = match (active, hand) {
            (false, _) => vr::k_ulInvalidInputValueHandle,
            (true, Hand::Left) => self.left_hand_key.data().as_ffi(),
            (true, Hand::Right) => self.right_hand_key.data().as_ffi(),
        };
        unsafe {
            std::ptr::addr_of_mut!((*action_data).bActive).write(active);
            std::ptr::addr_of_mut!((*action_data).activeOrigin).write(origin);
        }
        vr::EVRInputError::None
    }

    fn GetPoseActionDataForNextFrame(
        &self,
        action: vr::VRActionHandle_t,
        origin: vr::ETrackingUniverseOrigin,
        action_data: *mut vr::InputPoseActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        self.get_pose_action_data(
            action,
            origin,
            action_data,
            action_data_size,
            restrict_to_device,
            self.openxr.get_display_time(),
        )
    }

    fn GetPoseActionDataRelativeToNow(
        &self,
        action: vr::VRActionHandle_t,
        origin: vr::ETrackingUniverseOrigin,
        seconds_from_now: f32,
        action_data: *mut vr::InputPoseActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        self.get_pose_action_data(
            action,
            origin,
            action_data,
            action_data_size,
            restrict_to_device,
            self.openxr.time_from_now(seconds_from_now),
        )
    }

    fn GetAnalogActionData(
        &self,
        handle: vr::VRActionHandle_t,
        action_data: *mut vr::InputAnalogActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        if action_data.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if action_data_size as usize != std::mem::size_of::<vr::InputAnalogActionData_t>() {
            crate::warn_once!(
                "GetAnalogActionData: unexpected size {}, expected {}",
                action_data_size,
                std::mem::size_of::<vr::InputAnalogActionData_t>()
            );
            return vr::EVRInputError::InvalidParam;
        }

        let mut out = WriteOnDrop::new(action_data);
        get_action_from_handle!(self, handle, session_data, action);
        let subaction_path = get_subaction_path!(self, restrict_to_device, action_data);

        let state = match action {
            ActionData::Vector1 { state, .. } => state,
            ActionData::Vector2 { data, .. } => &data.state,
            _ => return vr::EVRInputError::WrongType,
        };
        let index = if subaction_path == self.subaction_paths.left {
            0
        } else if subaction_path == self.subaction_paths.right {
            1
        } else {
            2
        };
        *out.value = state.lock().unwrap()[index];

        if log::log_enabled!(log::Level::Trace) {
            let action_map = self.action_map.read().unwrap();
            let action_key = ActionKey::from(KeyData::from_ffi(handle));
            let state = &*out.value;
            trace!(
                "analog action {:?}: x {} y {} (dx {} dy {}, active: {})",
                action_map.get(action_key).map(|a| &a.path),
                state.x,
                state.y,
                state.deltaX,
                state.deltaY,
                state.bActive
            );
        }

        vr::EVRInputError::None
    }

    fn GetDigitalActionData(
        &self,
        handle: vr::VRActionHandle_t,
        action_data: *mut vr::InputDigitalActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        if action_data.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if action_data_size as usize != std::mem::size_of::<vr::InputDigitalActionData_t>() {
            crate::warn_once!(
                "GetDigitalActionData: unexpected size {}, expected {}",
                action_data_size,
                std::mem::size_of::<vr::InputDigitalActionData_t>()
            );
            return vr::EVRInputError::InvalidParam;
        }

        let mut out = WriteOnDrop::new(action_data);

        get_action_from_handle!(self, handle, session_data, action);
        let subaction_path = get_subaction_path!(self, restrict_to_device, action_data);
        let ActionData::Bool(action) = &action else {
            return vr::EVRInputError::WrongType;
        };

        let key = ActionKey::from(KeyData::from_ffi(handle));
        let (state, active_hand) = if let Some(mask) = session_data.input_data.hand_mask(key) {
            self.masked_bool_state(handle, action, &session_data, subaction_path, mask)
        } else {
            let mut state = action
                .state(&session_data.session, subaction_path)
                .unwrap_or_else(inactive_state);
            let mut active_hand = if subaction_path == xr::Path::NULL {
                self.native_bool_origin(action, &session_data.session)
            } else {
                restrict_to_device
            };

            // An action can be driven both by a natively bound input and by a custom binding.
            if let Some((binding_state, binding_source)) =
                self.state_from_bindings(handle, restrict_to_device)
                && binding_state.is_active
                && (!state.is_active || binding_rank(&binding_state) > binding_rank(&state))
            {
                state = binding_state;
                active_hand = binding_source;
            }
            (state, active_hand)
        };

        if log::log_enabled!(log::Level::Trace) {
            let action_map = self.action_map.read().unwrap();
            trace!(
                "digital action {:?}: {} (changed: {}, active: {})",
                action_map.get(key).map(|a| &a.path),
                state.current_state,
                state.changed_since_last_sync,
                state.is_active
            );
        }

        *out.value = vr::InputDigitalActionData_t {
            bActive: state.is_active,
            bState: state.current_state,
            activeOrigin: active_hand,
            bChanged: state.changed_since_last_sync,
            fUpdateTime: self.update_time(state.changed_since_last_sync, state.last_change_time),
        };

        vr::EVRInputError::None
    }

    fn GetEyeTrackingDataRelativeToNow(
        &self,
        _: openvr::VRActionHandle_t,
        _: openvr::ETrackingUniverseOrigin,
        _: f32,
        _: *mut openvr::VREyeTrackingData_t,
        _: u32,
    ) -> openvr::EVRInputError {
        crate::warn_unimplemented!("GetEyeTrackingDataRelativeToNow");
        vr::EVRInputError::NoData
    }
    fn GetEyeTrackingDataForNextFrame(
        &self,
        _: openvr::VRActionHandle_t,
        _: openvr::ETrackingUniverseOrigin,
        _: *mut openvr::VREyeTrackingData_t,
        _: u32,
    ) -> openvr::EVRInputError {
        crate::warn_unimplemented!("GetEyeTrackingDataForNextFrame");
        vr::EVRInputError::NoData
    }

    fn UpdateActionState(
        &self,
        active_sets: *mut vr::VRActiveActionSet_t,
        active_set_size: u32,
        active_set_count: u32,
    ) -> vr::EVRInputError {
        self.openxr.pump();
        // alyx, portal 2 ce
        if active_set_count == 0 || active_set_size == 0 {
            return vr::EVRInputError::NoActiveActionSet;
        }
        if self.openxr.is_lost() {
            return vr::EVRInputError::IPCError;
        }
        if active_sets.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if active_set_size as usize != std::mem::size_of::<vr::VRActiveActionSet_t>() {
            crate::warn_once!(
                "UpdateActionState: unexpected size {}, expected {}",
                active_set_size,
                std::mem::size_of::<vr::VRActiveActionSet_t>()
            );
            return vr::EVRInputError::InvalidParam;
        }

        let active_sets =
            unsafe { std::slice::from_raw_parts(active_sets, active_set_count as usize) };

        let data = self.openxr.session_data.get();
        let Some(actions) = data.input_data.get_loaded_actions() else {
            return vr::EVRInputError::InvalidParam;
        };

        let set_map = self.set_map.read().unwrap();
        let mut sync_sets = Vec::with_capacity(active_sets.len() + 3);
        // The sets synced below and their hand, for the skeletal actions
        let mut activated_sets = Vec::with_capacity(active_sets.len());
        // The priority of each set, and the hands it's active for.
        let mut set_priorities = Vec::with_capacity(active_sets.len());

        {
            tracy_span!("UpdateActionState generate active sets");

            let hands_for = |path| {
                [
                    path != self.subaction_paths.right,
                    path != self.subaction_paths.left,
                ]
            };

            for set in active_sets {
                let key = ActionSetKey::from(KeyData::from_ffi(set.ulActionSet));
                let name = set_map.get(key);

                // A restricted set without OpenXR actions doesn't have the hand subaction paths,
                // which would fail xrSyncActions. It has nothing to restrict, so sync it
                // unrestricted.
                let restrict = |path, key| {
                    if actions.hand_restrictable_sets.contains(&key) {
                        path
                    } else {
                        xr::Path::NULL
                    }
                };

                let mut path = xr::Path::NULL;
                if let Some(new_path) = self.subaction_path_from_handle(set.ulRestrictedToDevice) {
                    path = new_path;
                    // The secondary set is ignored unless the primary set is restricted.
                    if path != xr::Path::NULL
                        && set.ulSecondaryActionSet != vr::k_ulInvalidActionSetHandle
                    {
                        let left_hand_subaction_path = self.get_subaction_path(Hand::Left);
                        let right_hand_subaction_path = self.get_subaction_path(Hand::Right);
                        let path = if path == left_hand_subaction_path {
                            right_hand_subaction_path
                        } else {
                            left_hand_subaction_path
                        };
                        let key = ActionSetKey::from(KeyData::from_ffi(set.ulSecondaryActionSet));
                        let name = set_map.get(key);
                        let Some(set_data) = actions.sets.get(key) else {
                            debug!(
                                "Application passed invalid secondary action set key: {key:?} ({name:?})"
                            );
                            return vr::EVRInputError::InvalidHandle;
                        };
                        debug!("Activating secondary set {}", name.unwrap());
                        sync_sets.push(xr::ActiveActionSet::with_subaction(
                            set_data,
                            restrict(path, key),
                        ));
                        activated_sets.push((key, path));
                        // TODO: check if SteamVR actually puts these on the same priority as the primary
                        set_priorities.push((key, hands_for(path), set.nPriority));
                    }
                };

                let Some(set_data) = actions.sets.get(key) else {
                    debug!("Application passed invalid action set key: {key:?} ({name:?})");
                    return vr::EVRInputError::InvalidHandle;
                };
                debug!(
                    "Activating set {} with priority {}",
                    name.unwrap(),
                    set.nPriority
                );
                sync_sets.push(xr::ActiveActionSet::with_subaction(
                    set_data,
                    restrict(path, key),
                ));
                activated_sets.push((key, path));
                set_priorities.push((key, hands_for(path), set.nPriority));
            }

            let skeletal_input = data.input_data.estimated_skeleton_actions.get().unwrap();
            sync_sets.push(xr::ActiveActionSet::new(&data.input_data.pose_data.set));
            sync_sets.push(xr::ActiveActionSet::new(&skeletal_input.set));
            sync_sets.push(xr::ActiveActionSet::new(&actions.haptic_set));
            self.legacy_state.on_action_sync();
        }

        {
            tracy_span!("xrSyncActions");
            // XR_EXT_active_action_set_priority isn't used: it would let the application's sets
            // block xrizer's own sets on the same inputs, or the other way around. See
            // Input::mask_by_priority.
            let info = xr::sys::ActionsSyncInfo {
                ty: xr::sys::ActionsSyncInfo::TYPE,
                next: ptr::null(),
                count_active_action_sets: sync_sets.len() as u32,
                active_action_sets: sync_sets.as_ptr() as _,
            };

            let result =
                unsafe { (self.openxr.instance.fp().sync_actions)(data.session.as_raw(), &info) };
            if result.into_raw() < 0 {
                self.openxr.handle_error("xrSyncActions", result);
                return vr::EVRInputError::IPCError;
            }
        }
        *self.active_sets.write().unwrap() = activated_sets;

        let hands = [
            (
                self.subaction_paths.left,
                self.left_hand_key.data().as_ffi(),
            ),
            (
                self.subaction_paths.right,
                self.right_hand_key.data().as_ffi(),
            ),
        ]
        .map(|(hand, origin)| {
            let profile = data.session.current_interaction_profile(hand);
            (
                hand,
                self.openxr
                    .check("xrGetCurrentInteractionProfile", profile)
                    .unwrap_or(xr::Path::NULL),
                origin,
            )
        });
        let masked = self.mask_by_priority(
            &data,
            actions,
            &set_map,
            &set_priorities,
            hands.map(|(_, profile, _)| profile),
        );
        let masked_hands = |key| [0, 1].map(|hand| masked.contains(&(key, hand)));

        for (key, action) in &actions.actions {
            if let ActionData::Vector2 {
                action,
                data: vector2,
            } = action
            {
                vector2.sync(
                    action,
                    &data.session,
                    hands,
                    masked_hands(key),
                    |changed, time| self.update_time(changed, time),
                );
            }
        }

        let devices = data.input_data.devices.read().unwrap();
        let left_profile = devices
            .get_controller(Hand::Left)
            .map(|dev| dev.profile_path);
        let right_profile = devices
            .get_controller(Hand::Right)
            .map(|dev| dev.profile_path);
        for key in &actions.actions_with_custom_bindings {
            let unsync_custom_bindings = |key, profile| {
                if profile == xr::Path::NULL {
                    return;
                }

                let Some(bindings) = actions
                    .per_profile_bindings
                    .get(&profile)
                    .and_then(|map| map.get(key))
                else {
                    return;
                };

                for binding in bindings {
                    binding.unsync();
                }
            };

            match (left_profile, right_profile) {
                (Some(profile), None) | (None, Some(profile)) => {
                    unsync_custom_bindings(*key, profile)
                }
                (Some(left), Some(right)) => {
                    unsync_custom_bindings(*key, left);
                    if left != right {
                        unsync_custom_bindings(*key, right);
                    }
                }
                (None, None) => {}
            }
        }

        // After the custom bindings were unsynced, so they're evaluated for this sync.
        for (key, action) in &actions.actions {
            if let ActionData::Vector1 {
                action,
                state,
                input_only: false,
            } = action
            {
                self.sync_vector1(&data, actions, key, action, state, masked_hands(key));
            }
        }

        let mut masks = data.input_data.priority_masks.lock().unwrap();
        masks.previous = std::mem::replace(&mut masks.current, masked);

        vr::EVRInputError::None
    }

    fn GetInputSourceHandle(
        &self,
        input_source_path: *const c_char,
        handle: *mut vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        let path = unsafe { CStr::from_ptr(input_source_path) };

        let ret = {
            let guard = self.input_source_map.read().unwrap();
            match guard.iter().find(|(_, src)| src.as_c_str() == path) {
                Some((key, _)) => key.data().as_ffi(),
                None => {
                    drop(guard);
                    let mut guard = self.input_source_map.write().unwrap();
                    let key = guard.insert(path.into());
                    key.data().as_ffi()
                }
            }
        };
        if let Some(handle) = unsafe { handle.as_mut() } {
            debug!("requested handle for path {path:?}: {ret}");
            *handle = ret;
            vr::EVRInputError::None
        } else {
            vr::EVRInputError::InvalidParam
        }
    }

    fn GetActionHandle(
        &self,
        action_name: *const c_char,
        handle: *mut vr::VRActionHandle_t,
    ) -> vr::EVRInputError {
        let name = unsafe { CStr::from_ptr(action_name) }
            .to_string_lossy()
            .to_lowercase();
        let guard = self.action_map.read().unwrap();
        let val = match guard.iter().find(|(_, action)| action.path == name) {
            Some((key, _)) => key.data().as_ffi(),
            None => {
                drop(guard);
                let mut guard = self.action_map.write().unwrap();
                let key = guard.insert(Action { path: name });
                key.data().as_ffi()
            }
        };

        if let Some(handle) = unsafe { handle.as_mut() } {
            *handle = val;
            vr::EVRInputError::None
        } else {
            vr::EVRInputError::InvalidParam
        }
    }

    fn GetActionSetHandle(
        &self,
        action_set_name: *const c_char,
        handle: *mut vr::VRActionSetHandle_t,
    ) -> vr::EVRInputError {
        let name = unsafe { CStr::from_ptr(action_set_name) }
            .to_string_lossy()
            .to_lowercase();
        let guard = self.set_map.read().unwrap();
        let val = match guard.iter().find(|(_, set)| **set == name) {
            Some((key, _)) => key.data().as_ffi(),
            None => {
                drop(guard);
                let mut guard = self.set_map.write().unwrap();
                let key = guard.insert(name);
                key.data().as_ffi()
            }
        };

        if let Some(handle) = unsafe { handle.as_mut() } {
            *handle = val;
            vr::EVRInputError::None
        } else {
            vr::EVRInputError::InvalidParam
        }
    }

    fn SetActionManifestPath(&self, path: *const c_char) -> vr::EVRInputError {
        if path.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        let path = unsafe { CStr::from_ptr(path) }.to_string_lossy();
        let path = std::path::Path::new(&*path);
        info!("loading action manifest from {path:?}");

        let _setup = self.action_setup.lock().unwrap();
        // We need to restart the session if the legacy actions have already been attached.
        let mut data = self.openxr.session_data.get();
        if data.input_data.get_legacy_actions().is_some() {
            drop(data);
            // After a failed creation we're on a fresh temporary session, which can still load it.
            if self.openxr.restart_session() == Err(RestartError::Exiting) {
                return vr::EVRInputError::IPCError;
            }
            data = self.openxr.session_data.get();
        }

        match self.load_action_manifest(&data, path) {
            Ok(_) => vr::EVRInputError::None,
            Err(e) => e,
        }
    }
}

impl<C: openxr_data::Compositor> Input<C> {
    fn get_pose_action_data(
        &self,
        action: vr::VRActionHandle_t,
        origin: vr::ETrackingUniverseOrigin,
        action_data: *mut vr::InputPoseActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
        xr_time: xr::Time,
    ) -> vr::EVRInputError {
        if action_data.is_null() {
            return vr::EVRInputError::InvalidParam;
        }
        if action_data_size as usize != std::mem::size_of::<vr::InputPoseActionData_t>() {
            crate::warn_once!(
                "GetPoseActionData: unexpected size {}, expected {}",
                action_data_size,
                std::mem::size_of::<vr::InputPoseActionData_t>()
            );
            return vr::EVRInputError::InvalidParam;
        }

        if self.openxr.is_lost() {
            unsafe { action_data.write(Default::default()) };
            return vr::EVRInputError::IPCError;
        }

        if log::log_enabled!(log::Level::Trace) {
            let action_map = self.action_map.read().unwrap();
            let action_key = ActionKey::from(KeyData::from_ffi(action));
            let input_map = self.input_source_map.read().unwrap();
            let input_key = InputSourceKey::from(KeyData::from_ffi(restrict_to_device));
            trace!(
                "getting pose for {:?} (restrict: {:?})",
                action_map.get(action_key).map(|a| &a.path),
                input_map.get(input_key)
            );
        }

        let data = self.openxr.session_data.get();
        let Some(loaded) = data.input_data.get_loaded_actions() else {
            return vr::EVRInputError::InvalidHandle;
        };

        macro_rules! no_data {
            () => {{
                unsafe {
                    action_data.write(Default::default());
                }
                return vr::EVRInputError::None;
            }};
        }
        let subaction_path = get_subaction_path!(self, restrict_to_device, action_data);
        let devices = data.input_data.devices.read().unwrap();
        let get_hand = |hand| {
            devices
                .get_controller(hand)
                .map(|h| (hand, h.profile_path))
                .unzip()
        };
        let (active_origin, hand, ty) = match loaded.try_get_action(action) {
            Ok(ActionData::Pose) => {
                let (mut hand, interaction_profile) = match subaction_path {
                    x if x == self.get_subaction_path(Hand::Left) => get_hand(Hand::Left),
                    x if x == self.get_subaction_path(Hand::Right) => get_hand(Hand::Right),
                    x if x == xr::Path::NULL => (None, None),
                    _ => unreachable!(),
                };

                let get_hand_pose =
                    |hand: &TrackedDevice| loaded.try_get_pose(action, hand.profile_path).ok();

                let get_first_bound_hand_profile = || {
                    devices
                        .get_controller(Hand::Left)
                        .and_then(get_hand_pose)
                        .or_else(|| devices.get_controller(Hand::Right).and_then(get_hand_pose))
                };

                let Some(bound) = interaction_profile
                    .and_then(|p| loaded.try_get_pose(action, p).ok())
                    .or_else(get_first_bound_hand_profile)
                else {
                    match hand {
                        Some(hand) => {
                            trace!(
                                "action has no bindings for the {hand:?} hand's interaction profile"
                            );
                        }
                        None => {
                            trace!("action has no bindings for either hand's interaction profile");
                        }
                    }

                    // We may have no interaction profile (e.g. hand tracking
                    // only), so try a pose synthesised from the hand skeleton.
                    let pose = hand.and_then(|hand| {
                        devices
                            .get_controller(hand)
                            .map(|d| d.get_pose(xr_time, &data, origin, None))
                    });
                    if let Some(pose) = pose {
                        unsafe {
                            action_data.write(vr::InputPoseActionData_t {
                                bActive: true,
                                activeOrigin: 0,
                                pose,
                            });
                        }
                        return vr::EVRInputError::None;
                    }

                    no_data!()
                };

                let origin = hand.is_some().then_some(restrict_to_device);
                let pose_type = match hand {
                    Some(Hand::Left) => bound.left,
                    Some(Hand::Right) => bound.right,
                    None => {
                        hand = Some(Hand::Left);
                        bound.left.or_else(|| {
                            hand = Some(Hand::Right);
                            bound.right
                        })
                    }
                };

                let Some(ty) = pose_type else {
                    trace!("action has no bindings for the hand {hand:?}");
                    no_data!()
                };

                let hand = hand.unwrap();
                let origin = origin.unwrap_or_else(|| match hand {
                    Hand::Left => self.left_hand_key.data().as_ffi(),
                    Hand::Right => self.right_hand_key.data().as_ffi(),
                });

                (origin, hand, Some(ty))
            }
            Ok(ActionData::Skeleton(hand)) => {
                if subaction_path != xr::Path::NULL {
                    return vr::EVRInputError::InvalidDevice;
                }
                (0, *hand, None)
            }
            Ok(_) => return vr::EVRInputError::WrongType,
            Err(e) => return e,
        };

        drop(devices);
        drop(data);

        unsafe {
            let pose = self
                .get_controller_pose(hand, Some(origin), xr_time, ty)
                .unwrap_or_default();
            action_data.write(vr::InputPoseActionData_t {
                bActive: true,
                activeOrigin: active_origin,
                pose,
            });
        }

        vr::EVRInputError::None
    }
}

impl<C: openxr_data::Compositor> vr::IVRInput005On006 for Input<C> {
    #[inline]
    fn GetSkeletalSummaryData(
        &self,
        action: vr::VRActionHandle_t,
        summary_data: *mut vr::VRSkeletalSummaryData_t,
    ) -> vr::EVRInputError {
        <Self as vr::IVRInput010_Interface>::GetSkeletalSummaryData(
            self,
            action,
            vr::EVRSummaryType::FromAnimation,
            summary_data,
        )
    }

    #[inline]
    fn GetPoseActionData(
        &self,
        action: vr::VRActionHandle_t,
        origin: vr::ETrackingUniverseOrigin,
        seconds_from_now: f32,
        action_data: *mut vr::InputPoseActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        <Self as vr::IVRInput010_Interface>::GetPoseActionDataRelativeToNow(
            self,
            action,
            origin,
            seconds_from_now,
            action_data,
            action_data_size,
            restrict_to_device,
        )
    }
}

// IVRInput_003 is the SteamVR Input beta: no device restriction yet, and skeletal data in one call.
impl<C: openxr_data::Compositor> vr::IVRInput003On004 for Input<C> {
    fn UpdateActionState(
        &self,
        active_sets: *mut vr::vr_1_0_15::VRActiveActionSet_t,
        active_set_size: u32,
        active_set_count: u32,
    ) -> vr::EVRInputError {
        if active_set_count == 0 || active_set_size == 0 {
            return vr::EVRInputError::NoActiveActionSet;
        }
        if active_sets.is_null()
            || active_set_size as usize != std::mem::size_of::<vr::vr_1_0_15::VRActiveActionSet_t>()
        {
            return vr::EVRInputError::InvalidParam;
        }
        // The beta has no set priorities, which is the same as all of them being 0.
        let mut sets: Vec<_> =
            unsafe { std::slice::from_raw_parts(active_sets, active_set_count as usize) }
                .iter()
                .map(|set| vr::VRActiveActionSet_t {
                    ulActionSet: set.ulActionSet,
                    ulRestrictedToDevice: set.ulRestrictedToDevice,
                    ulSecondaryActionSet: set.ulSecondaryActionSet,
                    ..Default::default()
                })
                .collect();
        <Self as vr::IVRInput011_Interface>::UpdateActionState(
            self,
            sets.as_mut_ptr(),
            std::mem::size_of::<vr::VRActiveActionSet_t>() as u32,
            active_set_count,
        )
    }

    fn GetDigitalActionData(
        &self,
        action: vr::VRActionHandle_t,
        action_data: *mut vr::InputDigitalActionData_t,
        action_data_size: u32,
    ) -> vr::EVRInputError {
        <Self as vr::IVRInput010_Interface>::GetDigitalActionData(
            self,
            action,
            action_data,
            action_data_size,
            vr::k_ulInvalidInputValueHandle,
        )
    }

    fn GetAnalogActionData(
        &self,
        action: vr::VRActionHandle_t,
        action_data: *mut vr::InputAnalogActionData_t,
        action_data_size: u32,
    ) -> vr::EVRInputError {
        <Self as vr::IVRInput010_Interface>::GetAnalogActionData(
            self,
            action,
            action_data,
            action_data_size,
            vr::k_ulInvalidInputValueHandle,
        )
    }

    fn GetPoseActionData(
        &self,
        action: vr::VRActionHandle_t,
        origin: vr::ETrackingUniverseOrigin,
        seconds_from_now: f32,
        action_data: *mut vr::InputPoseActionData_t,
        action_data_size: u32,
    ) -> vr::EVRInputError {
        vr::IVRInput005On006::GetPoseActionData(
            self,
            action,
            origin,
            seconds_from_now,
            action_data,
            action_data_size,
            vr::k_ulInvalidInputValueHandle,
        )
    }

    fn GetSkeletalActionData(
        &self,
        action: vr::VRActionHandle_t,
        bone_parent: vr::EVRSkeletalTransformSpace,
        _seconds_from_now: f32,
        action_data: *mut vr::InputSkeletonActionData_t,
        action_data_size: u32,
        transform_array: *mut vr::VRBoneTransform_t,
        transform_array_count: u32,
    ) -> vr::EVRInputError {
        if action_data.is_null()
            || action_data_size as usize != std::mem::size_of::<vr::InputSkeletonActionData_t>()
        {
            return vr::EVRInputError::InvalidParam;
        }
        let mut data = vr::InputSkeletalActionData_t::default();
        let err = <Self as vr::IVRInput010_Interface>::GetSkeletalActionData(
            self,
            action,
            &mut data,
            std::mem::size_of_val(&data) as u32,
        );
        if err != vr::EVRInputError::None {
            return err;
        }
        unsafe {
            action_data.write(vr::InputSkeletonActionData_t {
                bActive: data.bActive,
                activeOrigin: data.activeOrigin,
            })
        };
        if !data.bActive {
            return vr::EVRInputError::None;
        }
        // The beta's spaces are Action (now Model), Parent and Additive, which we treat as Parent.
        let space = if bone_parent == vr::EVRSkeletalTransformSpace::Model {
            vr::EVRSkeletalTransformSpace::Model
        } else {
            vr::EVRSkeletalTransformSpace::Parent
        };
        <Self as vr::IVRInput010_Interface>::GetSkeletalBoneData(
            self,
            action,
            space,
            vr::EVRSkeletalMotionRange::WithController,
            transform_array,
            transform_array_count,
        )
    }

    fn GetSkeletalActionDataCompressed(
        &self,
        _: vr::VRActionHandle_t,
        _: vr::EVRSkeletalTransformSpace,
        _: f32,
        _: *mut c_void,
        _: u32,
        required_size: *mut u32,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("GetSkeletalActionDataCompressed");
        // An error, so the app doesn't size or uncompress a buffer we never filled.
        if let Some(required_size) = unsafe { required_size.as_mut() } {
            *required_size = 0;
        }
        vr::EVRInputError::InvalidParam
    }

    fn UncompressSkeletalActionData(
        &self,
        _: *mut c_void,
        _: u32,
        _: *mut vr::EVRSkeletalTransformSpace,
        _: *mut vr::VRBoneTransform_t,
        _: u32,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("UncompressSkeletalActionData");
        // We never hand out compressed data.
        vr::EVRInputError::InvalidCompressedData
    }

    fn TriggerHapticVibrationAction(
        &self,
        action: vr::VRActionHandle_t,
        start_seconds_from_now: f32,
        duration_seconds: f32,
        frequency: f32,
        amplitude: f32,
    ) -> vr::EVRInputError {
        <Self as vr::IVRInput010_Interface>::TriggerHapticVibrationAction(
            self,
            action,
            start_seconds_from_now,
            duration_seconds,
            frequency,
            amplitude,
            vr::k_ulInvalidInputValueHandle,
        )
    }

    fn ShowBindingsForActionSet(
        &self,
        _: *mut vr::vr_1_0_15::VRActiveActionSet_t,
        _: u32,
        _: u32,
        _: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("ShowBindingsForActionSet");
        vr::EVRInputError::None
    }
}

impl<C: openxr_data::Compositor> vr::IVRInput004On005 for Input<C> {
    #[inline]
    fn DecompressSkeletalBoneData(
        &self,
        _compressed_buffer: *mut c_void,
        _compressed_buffer_size: u32,
        _transform_space: *mut vr::EVRSkeletalTransformSpace,
        _transform_array: *mut vr::VRBoneTransform_t,
        _transform_array_count: u32,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("DecompressSkeletalBoneData");
        vr::EVRInputError::None
    }

    #[inline]
    fn GetOriginLocalizedName(
        &self,
        origin: vr::VRInputValueHandle_t,
        name_array: *mut c_char,
        name_array_size: u32,
    ) -> vr::EVRInputError {
        // IVRInput_004 has no section mask and returns the whole name (VRInputString_All).
        <Self as vr::IVRInput010_Interface>::GetOriginLocalizedName(
            self,
            origin,
            name_array,
            name_array_size,
            -1,
        )
    }

    #[inline]
    fn GetSkeletalActionData(
        &self,
        action: vr::VRActionHandle_t,
        action_data: *mut vr::vr_1_0_17::InputSkeletalActionData_t,
        action_data_size: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        // restrict_to_device not supported for skeletal actions
        if restrict_to_device != vr::k_ulInvalidInputValueHandle {
            return vr::EVRInputError::NoData;
        }

        // The newer struct is this one without boneCount.
        let err = <Self as vr::IVRInput010_Interface>::GetSkeletalActionData(
            self,
            action,
            action_data.cast(),
            action_data_size,
        );
        if err == vr::EVRInputError::None
            && action_data_size as usize
                >= std::mem::size_of::<vr::vr_1_0_17::InputSkeletalActionData_t>()
        {
            unsafe { (&raw mut (*action_data).boneCount).write(HandSkeletonBone::Count as u32) };
        }
        err
    }

    #[inline]
    fn GetSkeletalBoneData(
        &self,
        action: vr::VRActionHandle_t,
        transform_space: vr::EVRSkeletalTransformSpace,
        motion_range: vr::EVRSkeletalMotionRange,
        transform_array: *mut vr::VRBoneTransform_t,
        transform_array_count: u32,
        restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        // restrict_to_device not supported for skeletal actions
        if restrict_to_device != vr::k_ulInvalidInputValueHandle {
            return vr::EVRInputError::NoData;
        }

        <Self as vr::IVRInput010_Interface>::GetSkeletalBoneData(
            self,
            action,
            transform_space,
            motion_range,
            transform_array,
            transform_array_count,
        )
    }

    #[inline]
    fn GetSkeletalBoneDataCompressed(
        &self,
        _action: vr::VRActionHandle_t,
        _transform_space: vr::EVRSkeletalTransformSpace,
        _motion_range: vr::EVRSkeletalMotionRange,
        _compressed_data: *mut c_void,
        _compressed_size: u32,
        _required_compressed_size: *mut u32,
        _restrict_to_device: vr::VRInputValueHandle_t,
    ) -> vr::EVRInputError {
        crate::warn_unimplemented!("GetSkeletalBoneDataCompressed");
        vr::EVRInputError::None
    }
}

impl<C: openxr_data::Compositor> Input<C> {
    pub fn interaction_profile_changed(&self, session_data: &SessionData) {
        let mut devices = session_data.input_data.devices.write().unwrap();

        let mut devices_to_create = vec![];

        for hand in [Hand::Left, Hand::Right] {
            let mut controller = devices.get_controller_mut(hand);
            let subaction_path = self.get_subaction_path(hand);

            let Some(profile_path) = self.openxr.check(
                "xrGetCurrentInteractionProfile",
                session_data
                    .session
                    .current_interaction_profile(subaction_path),
            ) else {
                continue;
            };

            if let Some(controller) = controller.as_mut() {
                if controller.profile_path != profile_path {
                    controller.reset_tracked_hand();
                    // The new device's skeleton may come from elsewhere, or not at all.
                    self.skeletal_tracking_level.write().unwrap()[hand as usize - 1] =
                        vr::EVRSkeletalTrackingLevel::Estimated;
                    // The last pose is of a controller that's gone, or with the old profile's
                    // offset.
                    controller.forget_poses();
                }
                controller.profile_path = profile_path;
            }

            let profile_name = match profile_path {
                xr::Path::NULL => {
                    if let Some(controller) = controller.as_mut() {
                        controller.connected = false;
                    }
                    "<null>".to_owned()
                }
                path => {
                    if let Some(controller) = controller.as_mut() {
                        controller.connected = true;
                    }
                    self.openxr.instance.path_to_string(path).unwrap()
                }
            };

            if let Some(data) = ProfileData::for_profile_name(&profile_name) {
                {
                    // simple_controller says nothing about the headset, so don't let it replace a
                    // real controller (e.g. when switching to hand tracking).
                    let mut last_profile = self.last_profile_properties.lock().unwrap();
                    if last_profile.is_none() || !is_simple_controller(data.properties) {
                        *last_profile = Some(data.properties);
                    }
                }
                if let Some(controller) = controller.as_mut() {
                    controller.profile_data = Some(data);
                } else {
                    let hand_tracker = devices.create_hand_tracker(&session_data.session, hand);
                    devices_to_create.push((
                        TrackedDeviceType::Controller {
                            hand,
                            hand_tracker,
                            skeleton_cache: Mutex::new(Default::default()),
                            pose_is_synthesised: false.into(),
                        },
                        Some(profile_path),
                        Some(data),
                    ));
                }
            };

            session_data.input_data.interaction_profile_changed();

            info!(
                "{} interaction profile changed: {}",
                self.openxr
                    .instance
                    .path_to_string(self.get_subaction_path(hand))
                    .unwrap(),
                profile_name
            )
        }

        for (device_type, profile_path, profile_data) in devices_to_create {
            let mut device = TrackedDevice::new(device_type, profile_path, profile_data);
            device.connected = true;

            devices.push_device(device).unwrap_or_else(|e| {
                panic!("Failed to create new controller: {:?}", e);
            });
        }

        #[cfg(feature = "monado")]
        if let Err(e) = devices.create_monado_generic_trackers(&self.openxr, session_data) {
            self.openxr.handle_error("Creating generic trackers", e);
        }
    }

    pub fn frame_start_update(&self) {
        tracy_span!();
        if self.openxr.is_lost() {
            return;
        }
        let data = self.openxr.session_data.get();
        let devices = data.input_data.devices.read().unwrap();

        for cache in self.skeletal_bone_cache.iter().flatten() {
            std::mem::take(&mut *cache.lock().unwrap());
        }

        for device in devices.iter() {
            device.clear_pose_cache();
        }

        let left_hand = devices.get_controller(Hand::Left);
        let right_hand = devices.get_controller(Hand::Right);

        let input_data = &data.input_data;
        if let Some(loaded) = input_data.get_loaded_actions() {
            // If the game has loaded actions, we shouldn't need to sync the state because the game
            // should be doing it itself with UpdateActionState. However, some games (Tea for God)
            // don't actually call UpdateActionState if no controllers are reported as connected,
            // and interaction profiles are only updated after xrSyncActions is called. So here, we
            // do an action sync to try and get the runtime to update the interaction profile.
            if (left_hand.is_none_or(|hand| !hand.is_connected(&self.openxr, &data)))
                && (right_hand.is_none_or(|hand| !hand.is_connected(&self.openxr, &data)))
            {
                debug!("no controllers connected - syncing info set");
                let result = data
                    .session
                    .sync_actions(&[xr::ActiveActionSet::new(&loaded.info_set)]);
                self.openxr.check("xrSyncActions", result);
            }
            return;
        }

        match input_data.get_legacy_actions() {
            Some(actions) => {
                let result = data.session.sync_actions(&[
                    xr::ActiveActionSet::new(&actions.set),
                    xr::ActiveActionSet::new(&input_data.pose_data.set),
                ]);
                if self.openxr.check("xrSyncActions", result).is_some() {
                    self.legacy_state.on_action_sync();
                    self.update_legacy_state(&data.session, &devices, actions);
                }
            }
            None => {
                // Don't wait: SetActionManifestPath holds this while restarting the session, which
                // waits for our session guard.
                let Ok(_setup) = self.action_setup.try_lock() else {
                    debug!("not setting up legacy actions while actions are being loaded");
                    return;
                };
                // A manifest may have been loaded since we checked.
                if input_data.actions.get().is_some() {
                    return;
                }

                // If we haven't created our legacy actions yet but we're getting our per frame
                // update, go ahead and create them
                // This will force us to have to restart the session when we get an action
                // manifest, but that's fine, because in the event an action manifest is never
                // loaded (legacy input), having the legacy actions loaded and synced enables us to
                // determine if controllers are actually connected, and some games avoid getting
                // controller state unless they are reported as actually connected.

                // Make sure we're using the real session already
                // This avoids a scenario where we could go:
                // 1. attach legacy inputs
                // 2. restart session to attach action manifest
                // 3. restart to use real session
                if !data.is_real_session() {
                    debug!(
                        "Couldn't set up legacy actions because we're not in the real session yet."
                    );
                    return;
                }
                self.setup_legacy_actions();
            }
        }
    }

    pub fn post_session_restart(&self, data: &SessionData) {
        // This function is called while a write lock is called on the session, and as such should
        // not use self.openxr.session_data.get().
        if let Some(path) = self.loaded_actions_path.get() {
            let _ = self.load_action_manifest(data, path);
        }
    }

    pub fn get_next_event(&self, size: u32, out: *mut vr::VREvent_t) -> bool {
        const FUNC: &str = "get_next_event";
        if out.is_null() {
            warn!("{FUNC}: Got null event pointer.");
            return false;
        }
        let data = self.openxr.session_data.get();
        let devices = data.input_data.devices.read().unwrap();
        let mut events = self.events.lock().unwrap();

        // The frame update may already have queued button events for a device that connected
        // since the last poll, so its activation goes ahead of them. A deactivation goes after
        // the releases queued for it.
        let mut activations = 0;
        for (i, device) in devices.iter().enumerate() {
            let current = device.connected;

            if device.has_connected_changed() {
                debug!(
                    "sending {:?} {}connected",
                    device.get_type(),
                    if current { "" } else { "not " }
                );

                let event = InputEvent {
                    ty: if current {
                        vr::EVREventType::TrackedDeviceActivated
                    } else {
                        vr::EVREventType::TrackedDeviceDeactivated
                    },
                    index: i as vr::TrackedDeviceIndex_t,
                    data: Default::default(),
                };
                if current {
                    events.insert(activations, event);
                    activations += 1;
                } else {
                    events.push_back(event);
                }
            }
        }

        if let Some(event) = events.pop_front() {
            const MIN_CONTROLLER_EVENT_SIZE: usize = std::mem::offset_of!(vr::VREvent_t, data)
                + std::mem::size_of::<vr::VREvent_Controller_t>();
            if size < MIN_CONTROLLER_EVENT_SIZE as u32 {
                warn!(
                    "{FUNC}: Provided event struct size ({size}) is smaller than required ({MIN_CONTROLLER_EVENT_SIZE})."
                );
                return false;
            }
            // VREvent_t can be different sizes depending on the OpenVR version,
            // so we use raw pointers to avoid creating a reference, because if the
            // size doesn't match our VREvent_t's size, we are in UB land
            unsafe {
                (&raw mut (*out).eventType).write(event.ty.0);
                (&raw mut (*out).trackedDeviceIndex).write(event.index);
                (&raw mut (*out).eventAgeSeconds).write(0.0);
                (&raw mut (*out).data.controller).write(event.data);
            }
            true
        } else {
            false
        }
    }
}

enum LoadedActions {
    Legacy(LegacyActionData),
    Manifest(Box<ManifestLoadedActions>),
}

struct ManifestLoadedActions {
    sets: SecondaryMap<ActionSetKey, xr::ActionSet>,
    /// Sets that contain OpenXR actions (and thus the hand subaction paths).
    hand_restrictable_sets: HashSet<ActionSetKey>,
    actions: SecondaryMap<ActionKey, ActionData>,
    actions_with_custom_bindings: HashSet<ActionKey>,
    per_profile_pose_bindings: HashMap<xr::Path, SecondaryMap<ActionKey, BoundPose>>,
    per_profile_bindings: HashMap<xr::Path, SecondaryMap<ActionKey, Vec<BoolBindingData>>>,
    per_profile_input_paths: HashMap<xr::Path, SecondaryMap<ActionKey, Vec<xr::Path>>>,
    info_set: xr::ActionSet,
    _info_action: xr::Action<bool>,
    haptic_set: xr::ActionSet,
    haptic_action: xr::Action<xr::Haptic>,
    /// The set each skeleton action belongs to.
    skeleton_sets: SecondaryMap<ActionKey, ActionSetKey>,
}

impl ManifestLoadedActions {
    fn try_get_bindings(
        &self,
        handle: vr::VRActionHandle_t,
        interaction_profile: xr::Path,
    ) -> Result<&Vec<BoolBindingData>, vr::EVRInputError> {
        let key = ActionKey::from(KeyData::from_ffi(handle));
        self.per_profile_bindings
            .get(&interaction_profile)
            .ok_or(vr::EVRInputError::InvalidHandle)?
            .get(key)
            .ok_or(vr::EVRInputError::InvalidHandle)
    }

    fn try_get_action(
        &self,
        handle: vr::VRActionHandle_t,
    ) -> Result<&ActionData, vr::EVRInputError> {
        let key = ActionKey::from(KeyData::from_ffi(handle));
        self.actions
            .get(key)
            .ok_or(vr::EVRInputError::InvalidHandle)
            .inspect_err(|_| trace!("didn't find action for {key:?}"))
    }

    fn try_get_pose(
        &self,
        handle: vr::VRActionHandle_t,
        interaction_profile: xr::Path,
    ) -> Result<&BoundPose, vr::EVRInputError> {
        let key = ActionKey::from(KeyData::from_ffi(handle));
        self.per_profile_pose_bindings
            .get(&interaction_profile)
            .ok_or(vr::EVRInputError::InvalidHandle)?
            .get(key)
            .ok_or(vr::EVRInputError::InvalidHandle)
    }
}

struct PoseData {
    set: xr::ActionSet,
    grip: xr::Action<xr::Posef>,
    left_space: HandSpace,
    right_space: HandSpace,
}

impl PoseData {
    fn new(instance: &xr::Instance, left_path: xr::Path, right_path: xr::Path) -> Self {
        let set = instance
            .create_action_set("xrizer-pose-data", "xrizer pose data", 0)
            .unwrap();
        let grip = set
            .create_action("grip-pose", "Grip Pose", &[left_path, right_path])
            .unwrap();
        Self {
            set,
            grip,
            left_space: HandSpace {
                hand: Hand::Left,
                hand_path: left_path,
                raw: RwLock::default(),
                pose_history: Mutex::default(),
            },
            right_space: HandSpace {
                hand: Hand::Right,
                hand_path: right_path,
                raw: RwLock::default(),
                pose_history: Mutex::default(),
            },
        }
    }
    fn reset_spaces(&self) {
        self.left_space.reset_raw();
        self.right_space.reset_raw();
    }
}

struct HandSpace {
    hand: Hand,
    hand_path: xr::Path,

    /// Based on the controller jsons in SteamVR, the "raw" pose
    /// This is stored as a space so we can locate hand joints relative to it for skeletal data.
    raw: RwLock<Option<xr::Space>>,

    /// Recent (time, position) samples of the raw pose per tracking origin, for synthesizing
    /// linear velocities from position deltas (see the synthesized_velocity quirk). Kept per
    /// origin since apps can query several origins in the same frame.
    pose_history: Mutex<HashMap<vr::ETrackingUniverseOrigin, VecDeque<(i64, glam::Vec3)>>>,
}

struct SpaceReadGuard<'a>(RwLockReadGuard<'a, Option<xr::Space>>);
impl Deref for SpaceReadGuard<'_> {
    type Target = xr::Space;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl HandSpace {
    /// Record a position sample and return a finite-difference velocity over
    /// the recent window, if enough history exists.
    pub fn update_history_and_velocity(
        &self,
        time_ns: i64,
        position: glam::Vec3,
        origin: vr::ETrackingUniverseOrigin,
    ) -> Option<glam::Vec3> {
        const WINDOW_NS: i64 = 60_000_000; // 60ms
        const MIN_SPAN_NS: i64 = 8_000_000; // don't divide over tiny spans
        // Older samples are from before a pause, e.g. an origin the app only queries now and then.
        const MAX_GAP_NS: i64 = 4 * WINDOW_NS;

        let mut histories = self.pose_history.lock().unwrap();
        let history = histories.entry(origin).or_default();
        if history
            .back()
            .is_some_and(|&(t, _)| time_ns - t > MAX_GAP_NS)
        {
            history.clear();
        }
        // several queries per frame share the same predicted time
        if history.back().is_none_or(|&(t, _)| t != time_ns) {
            history.push_back((time_ns, position));
        }
        while let Some(&(t, _)) = history.front() {
            if time_ns - t > WINDOW_NS && history.len() > 2 {
                history.pop_front();
            } else {
                break;
            }
        }

        let &(t0, p0) = history.front()?;
        let span = time_ns - t0;
        if span < MIN_SPAN_NS {
            return None;
        }
        Some((position - p0) / (span as f32 / 1e9))
    }

    pub fn try_get_or_init_raw(
        &self,
        hand_profile: &Option<ProfileData>,
        session_data: &SessionData,
        pose_data: &PoseData,
    ) -> Option<SpaceReadGuard<'_>> {
        {
            let raw = self.raw.read().unwrap();
            if raw.is_some() {
                return Some(SpaceReadGuard(raw));
            }
        }
        {
            let Some(profile) = hand_profile.as_ref() else {
                trace!("no hand profile, no raw space will be created");
                return None;
            };

            let offset = profile.hand_offset(self.hand);
            let translation = offset.w_axis.truncate();
            let rotation = Quat::from_mat4(&offset);

            let offset_pose = xr::Posef {
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
            };

            let space = pose_data
                .grip
                .create_space(&session_data.session, self.hand_path, offset_pose)
                .inspect_err(|e| crate::warn_once!("Failed to create raw hand space: {}", e))
                .ok()?;
            *self.raw.write().unwrap() = Some(space);
        }

        Some(SpaceReadGuard(self.raw.read().unwrap()))
    }

    pub fn reset_raw(&self) {
        *self.raw.write().unwrap() = None;
    }
}
