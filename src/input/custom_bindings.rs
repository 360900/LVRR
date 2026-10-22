use super::action_manifest::{ClickThresholdParams, GrabParameters};
use crate::input::inactive_state;
use crate::openxr_data::SessionData;
use log::{error, warn};
use openvr as vr;
use openxr as xr;
use std::f32::consts::{FRAC_PI_4, PI};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use xr::{Haptic, HapticVibration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "String")]
pub(super) enum Vector2Inversion {
    #[default]
    None,
    X,
    Y,
    XY,
}

impl From<String> for Vector2Inversion {
    fn from(value: String) -> Self {
        match value.as_str() {
            "" => Self::None,
            "x" => Self::X,
            "y" => Self::Y,
            "xy" => Self::XY,
            _ => {
                warn!("Unknown Vector2 invert parameter {value:?}; ignoring inversion");
                Self::None
            }
        }
    }
}

impl Vector2Inversion {
    fn apply(self, value: &mut xr::Vector2f) {
        if matches!(self, Self::X | Self::XY) {
            value.x = -value.x;
        }
        if matches!(self, Self::Y | Self::XY) {
            value.y = -value.y;
        }
    }
}

#[cfg(test)]
#[test]
fn vector2_inversion_apply() {
    let mut value = xr::Vector2f { x: 0.25, y: 0.5 };
    Vector2Inversion::Y.apply(&mut value);
    assert_eq!(value.x, 0.25);
    assert_eq!(value.y, -0.5);
}

pub(super) struct Vector2BindingData {
    pub action: xr::Action<xr::Vector2f>,
    pub profile: xr::Path,
    pub hand: xr::Path,
    pub invert: Vector2Inversion,
}

#[derive(Default)]
pub(super) struct Vector2ActionData {
    pub bindings: Vec<Vector2BindingData>,
    // Left, right, unrestricted. OpenVR deltas are relative to UpdateActionState,
    // so reads must neither consume deltas nor share history between subactions.
    pub state: Mutex<[vr::InputAnalogActionData_t; 3]>,
}

impl Vector2ActionData {
    pub fn sync(
        &self,
        direct: &xr::Action<xr::Vector2f>,
        session: &xr::Session<xr::AnyGraphics>,
        hands: [(xr::Path, xr::Path, vr::VRInputValueHandle_t); 2],
    ) {
        fn prefer(
            candidate: &vr::InputAnalogActionData_t,
            best: &vr::InputAnalogActionData_t,
        ) -> bool {
            // Match OpenXR's longest-vector rule. Keep the first source on ties.
            candidate.bActive
                && (!best.bActive
                    || candidate.x * candidate.x + candidate.y * candidate.y
                        > best.x * best.x + best.y * best.y)
        }

        let mut next = [vr::InputAnalogActionData_t::default(); 3];
        for (index, (hand, profile, origin)) in hands.into_iter().enumerate() {
            let mut consider = |state: xr::ActionState<xr::Vector2f>| {
                let candidate = vr::InputAnalogActionData_t {
                    bActive: state.is_active,
                    activeOrigin: origin,
                    x: state.current_state.x,
                    y: state.current_state.y,
                    // Like other analog actions, fUpdateTime is currently zero.
                    ..Default::default()
                };
                if prefer(&candidate, &next[index]) {
                    next[index] = candidate;
                }
            };
            consider(direct.state(session, hand).unwrap_or_else(inactive_state));
            for binding in &self.bindings {
                if binding.hand != hand || binding.profile != profile {
                    continue;
                }
                let mut state = binding
                    .action
                    .state(session, hand)
                    .unwrap_or_else(inactive_state);
                binding.invert.apply(&mut state.current_state);
                consider(state);
            }
        }
        next[2] = if prefer(&next[1], &next[0]) {
            next[1]
        } else {
            next[0]
        };

        let mut previous = self.state.lock().unwrap();
        for (next, previous) in next.iter_mut().zip(previous.iter()) {
            next.deltaX = next.x - previous.x;
            next.deltaY = next.y - previous.y;
        }
        *previous = next;
    }
}

/// A custom binding: a boolean OpenVR state that xrizer computes from OpenXR actions, each of
/// which is bound to a single input.
pub enum BoolBindingType {
    Dpad(DpadData),
    DoubleTap(DoubleTapData),
    Toggle(ToggleData),
    Grab(GrabBindingData),
    ThresholdFloat(ThresholdBindingData<f32>),
    ThresholdVec2(ThresholdBindingData<xr::Vector2f>),
}

impl BoolBindingType {
    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        match self {
            Self::Dpad(dpad) => dpad.state(session, subaction_path),
            Self::DoubleTap(double) => double.state(session, subaction_path),
            Self::Toggle(toggle) => toggle.state(session, subaction_path),
            Self::Grab(grab) => grab.state(session, subaction_path),
            Self::ThresholdFloat(threshold) => threshold.state(session, subaction_path),
            Self::ThresholdVec2(threshold) => threshold.state(session, subaction_path),
        }
    }

    /// The OpenXR actions this binding reads.
    #[cfg(test)]
    pub fn actions(&self) -> Vec<xr::sys::Action> {
        match self {
            Self::Dpad(dpad) => std::iter::once(dpad.actions.xy.as_raw())
                .chain(dpad.actions.click_or_touch.as_ref().map(|a| a.as_raw()))
                .collect(),
            Self::DoubleTap(double) => vec![double.action.as_raw()],
            Self::Toggle(toggle) => vec![toggle.action.as_raw()],
            Self::Grab(grab) => grab
                .force_action
                .iter()
                .chain([&grab.value_action])
                .map(|a| a.as_raw())
                .collect(),
            Self::ThresholdFloat(threshold) => vec![threshold.action.as_raw()],
            Self::ThresholdVec2(threshold) => vec![threshold.action.as_raw()],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum DpadDirection {
    North,
    East,
    South,
    West,
    Center,
}

#[derive(Clone)]
pub(super) struct DpadActions {
    pub xy: xr::Action<xr::Vector2f>,
    pub click_or_touch: Option<xr::Action<f32>>,
    pub haptic: Option<xr::Action<Haptic>>,
}

pub(super) struct DpadData {
    pub actions: DpadActions,
    direction: DpadDirection,
    last_state: AtomicBool,
    active: AtomicBool,
    changed: AtomicBool,
}

impl DpadData {
    const CENTER_ZONE: f32 = 0.5;

    // Thresholds for force-activated dpads, experimentally chosen to match SteamVR
    const DPAD_CLICK_THRESHOLD: f32 = 0.33;
    const DPAD_RELEASE_THRESHOLD: f32 = 0.2;

    pub fn new(actions: DpadActions, direction: DpadDirection) -> Self {
        Self {
            actions,
            direction,
            last_state: false.into(),
            active: false.into(),
            changed: false.into(),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let action = &self.actions;
        let parent_state = action.xy.state(session, subaction_path)?;
        let mut ret_state = xr::ActionState {
            current_state: false,
            last_change_time: parent_state.last_change_time, // TODO: this is wrong
            changed_since_last_sync: false,
            is_active: parent_state.is_active,
        };

        let last_active = self.last_state.load(Ordering::Relaxed);
        let active_threshold = if last_active {
            Self::DPAD_RELEASE_THRESHOLD
        } else {
            Self::DPAD_CLICK_THRESHOLD
        };

        let active = action
            .click_or_touch
            .as_ref()
            .map(|a| {
                // If this action isn't bound in the current interaction profile,
                // is_active will be false - in this case, it's probably a joystick touch dpad, in
                // which case we still want to read the current state.
                a.state(session, subaction_path)
                    .map(|s| !s.is_active || s.current_state > active_threshold)
            })
            .unwrap_or(Ok(true))?;

        if !active {
            let changed = self
                .last_state
                .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
            self.changed.store(changed, Ordering::Relaxed);
            self.active.store(false, Ordering::Relaxed);
            return Ok(None);
        }

        // convert to polar coordinates
        let xr::Vector2f { x, y } = parent_state.current_state;
        let radius = x.hypot(y);
        let angle = y.atan2(x);

        // pi/2 wedges, no overlap
        let in_bounds = match self.direction {
            DpadDirection::North => {
                radius >= Self::CENTER_ZONE && (FRAC_PI_4..=3.0 * FRAC_PI_4).contains(&angle)
            }
            DpadDirection::East => {
                radius >= Self::CENTER_ZONE && (-FRAC_PI_4..=FRAC_PI_4).contains(&angle)
            }
            DpadDirection::South => {
                radius >= Self::CENTER_ZONE && (-3.0 * FRAC_PI_4..=-FRAC_PI_4).contains(&angle)
            }
            // west section is disjoint with atan2
            DpadDirection::West => {
                radius >= Self::CENTER_ZONE
                    && ((3.0 * FRAC_PI_4..=PI).contains(&angle)
                        || (-PI..=-3.0 * FRAC_PI_4).contains(&angle))
            }
            DpadDirection::Center => radius < Self::CENTER_ZONE,
        };

        ret_state.current_state = in_bounds;
        if self
            .last_state
            .compare_exchange(!in_bounds, in_bounds, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            ret_state.changed_since_last_sync = true;
            if in_bounds && let Some(haptic) = &action.haptic {
                let haptic_event = HapticVibration::new()
                    .amplitude(0.25)
                    .duration(xr::Duration::MIN_HAPTIC)
                    .frequency(xr::FREQUENCY_UNSPECIFIED);
                let _ = haptic
                    .apply_feedback(session, subaction_path, &haptic_event)
                    .inspect_err(|e| error!("Couldn't activate dpad haptic: {e}"));
            }
        }

        self.changed
            .store(ret_state.changed_since_last_sync, Ordering::Relaxed);
        self.active.store(true, Ordering::Relaxed);

        Ok(Some(ret_state))
    }
}

pub(super) struct GrabBindingData {
    /// Missing on controllers without a force sensor, which then grab by value alone.
    force_action: Option<xr::Action<f32>>,
    value_action: xr::Action<f32>,
    hold_threshold: f32,
    release_threshold: f32,
    last_state: AtomicBool,
}

impl GrabBindingData {
    // Default thresholds as set by SteamVR binding UI
    /// How much force to apply to begin a grab
    const DEFAULT_GRAB_THRESHOLD: f32 = 0.70;
    /// How much the value component needs to be to release the grab.
    const DEFAULT_RELEASE_THRESHOLD: f32 = 0.65;

    pub fn new(
        force_action: Option<xr::Action<f32>>,
        value_action: xr::Action<f32>,
        params: Option<&GrabParameters>,
    ) -> Self {
        Self {
            force_action,
            value_action,
            hold_threshold: params
                .and_then(|p| p.value_hold_threshold.as_deref().copied())
                .unwrap_or(Self::DEFAULT_GRAB_THRESHOLD),
            release_threshold: params
                .and_then(|p| p.value_release_threshold.as_deref().copied())
                .unwrap_or(Self::DEFAULT_RELEASE_THRESHOLD),
            last_state: false.into(),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let force_state = self
            .force_action
            .as_ref()
            .map(|force| force.state(session, subaction_path))
            .transpose()?;
        let value_state = self.value_action.state(session, subaction_path)?;
        if !value_state.is_active || force_state.is_some_and(|s| !s.is_active) {
            self.last_state.store(false, Ordering::Relaxed);
            return Ok(None);
        }

        let prev_grabbed = self.last_state.load(Ordering::Relaxed);
        let value = match force_state {
            Some(force) if force.current_state > 0.0 => force.current_state + 1.0,
            _ => value_state.current_state,
        };

        let grabbed = (prev_grabbed && value > self.release_threshold)
            || (!prev_grabbed && value >= self.hold_threshold);

        let changed_since_last_sync = grabbed != prev_grabbed;
        self.last_state.store(grabbed, Ordering::Relaxed);

        Ok(Some(xr::ActionState {
            current_state: grabbed,
            changed_since_last_sync,
            last_change_time: force_state.unwrap_or(value_state).last_change_time,
            is_active: true,
        }))
    }
}

pub(super) struct ToggleData {
    action: xr::Action<bool>,
    last_state: AtomicBool,
}

impl ToggleData {
    pub fn new(action: xr::Action<bool>) -> Self {
        Self {
            action,
            last_state: false.into(),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let state = self.action.state(session, subaction_path)?;
        if !state.is_active {
            return Ok(None);
        }

        let s = self.last_state.load(Ordering::Relaxed);
        let current_state = if state.changed_since_last_sync && state.current_state {
            !s
        } else {
            s
        };

        let changed_since_last_sync = self
            .last_state
            .compare_exchange(
                !current_state,
                current_state,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok();

        Ok(Some(xr::ActionState {
            current_state,
            changed_since_last_sync,
            last_change_time: state.last_change_time,
            is_active: true,
        }))
    }
}

/// Inputs whose magnitude can be compared against a click threshold.
pub(super) trait ThresholdInput: xr::ActionInput {
    fn magnitude(self) -> f32;
}

impl ThresholdInput for f32 {
    fn magnitude(self) -> f32 {
        self
    }
}

impl ThresholdInput for xr::Vector2f {
    fn magnitude(self) -> f32 {
        self.x.hypot(self.y)
    }
}

/// Clicks when an analog input passes a threshold. Each bound input has its own action: OpenXR
/// would report the largest of several inputs bound to one action, so every input's threshold
/// would be compared against the others' values too.
pub(super) struct ThresholdBindingData<T: xr::ActionTy> {
    action: xr::Action<T>,
    click_threshold: f32,
    release_threshold: f32,
    last_state: AtomicBool,
}

impl<T: ThresholdInput> ThresholdBindingData<T> {
    const DEFAULT_CLICK_THRESHOLD: f32 = 0.25;
    const DEFAULT_RELEASE_THRESHOLD: f32 = 0.20;

    pub fn new(action: xr::Action<T>, params: Option<&ClickThresholdParams>) -> Self {
        Self {
            action,
            click_threshold: params
                .and_then(|p| p.click_activate_threshold.as_deref().copied())
                .unwrap_or(Self::DEFAULT_CLICK_THRESHOLD),
            release_threshold: params
                .and_then(|p| p.click_deactivate_threshold.as_deref().copied())
                .unwrap_or(Self::DEFAULT_RELEASE_THRESHOLD),
            last_state: false.into(),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let state = self.action.state(session, subaction_path)?;
        if !state.is_active {
            return Ok(None);
        }

        let s = self.last_state.load(Ordering::Relaxed);
        let threshold = if s {
            self.release_threshold
        } else {
            self.click_threshold
        };
        let current_state = state.current_state.magnitude() >= threshold;

        let changed_since_last_sync = self
            .last_state
            .compare_exchange(
                !current_state,
                current_state,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok();

        Ok(Some(xr::ActionState {
            current_state,
            changed_since_last_sync,
            last_change_time: state.last_change_time,
            is_active: true,
        }))
    }
}

mod atomic_time {
    use openxr as xr;
    use std::sync::atomic::{AtomicI64, Ordering};

    pub struct AtomicTime(AtomicI64);

    impl AtomicTime {
        pub fn new(time: i64) -> Self {
            Self(time.into())
        }

        pub fn store(&self, time: xr::Time) {
            self.0.store(time.as_nanos(), Ordering::Relaxed);
        }

        pub fn load(&self) -> xr::Time {
            xr::Time::from_nanos(self.0.load(Ordering::Relaxed))
        }
    }
}
use atomic_time::AtomicTime;

pub(super) struct DoubleTapData {
    action: xr::Action<bool>,
    clicked_once: AtomicBool,
    first_release_time: AtomicTime,
    active: AtomicBool,
}

impl DoubleTapData {
    const TIMEOUT_MS: u128 = 300;

    pub fn new(action: xr::Action<bool>) -> Self {
        Self {
            action,
            clicked_once: false.into(),
            active: false.into(),
            first_release_time: AtomicTime::new(0),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let state = self.action.state(session, subaction_path)?;
        if !state.is_active {
            return Ok(None);
        }

        if !state.current_state {
            if self.clicked_once.load(Ordering::Relaxed) {
                self.first_release_time.store(state.last_change_time);
            }
            return Ok(Some(xr::ActionState {
                current_state: false,
                changed_since_last_sync: self.active.swap(false, Ordering::Relaxed),
                ..state
            }));
        }

        if self.active.load(Ordering::Relaxed) {
            Ok(Some(state))
        } else {
            let clicked_once = self.clicked_once.fetch_not(Ordering::Relaxed);
            let active = clicked_once && {
                let elapsed: xr::Duration = state.last_change_time - self.first_release_time.load();
                let elapsed = Duration::from_nanos(
                    elapsed
                        .as_nanos()
                        .try_into()
                        // A controller's clock can briefly run backwards (a sample
                        // timestamped in the past), making elapsed negative. Out of
                        // order events are equivalent to zero elapsed time here, so
                        // clamp instead of panicking.
                        .unwrap_or(0),
                );
                elapsed.as_millis() <= Self::TIMEOUT_MS
            };

            if active {
                self.active.store(true, Ordering::Relaxed);
            } else if clicked_once {
                // If the double tap timed out, we'll need to reset our clicked state
                // If clicked_once is true, then self.clicked_once is false (because of fetch_not)
                self.clicked_once.store(true, Ordering::Relaxed);
            }

            Ok(Some(xr::ActionState {
                current_state: active,
                changed_since_last_sync: active,
                ..state
            }))
        }
    }
}

enum BindingState {
    Unsynced,
    Synced(Option<xr::ActionState<bool>>),
}

pub struct BoolBindingData {
    pub ty: BoolBindingType,
    pub hand: xr::Path,
    last_state: Mutex<BindingState>,
}

impl BoolBindingData {
    pub fn new(ty: BoolBindingType, hand: xr::Path) -> Self {
        Self {
            ty,
            hand,
            last_state: Mutex::new(BindingState::Unsynced),
        }
    }

    pub fn unsync(&self) {
        *self.last_state.lock().unwrap() = BindingState::Unsynced;
    }

    pub fn state(
        &self,
        session: &SessionData,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        assert_ne!(subaction_path, xr::Path::NULL);
        if self.hand != subaction_path {
            return Ok(None);
        }

        let mut last_state = self.last_state.lock().unwrap();
        if let BindingState::Synced(state) = *last_state {
            return Ok(state);
        }

        let state = self.ty.state(&session.session, subaction_path)?;
        *last_state = BindingState::Synced(state);
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InteractionProfile;
    use crate::input::profiles::knuckles::Knuckles;
    use crate::input::profiles::oculus_touch::OculusTouch;
    use crate::input::profiles::vive_controller::ViveWands;
    use crate::input::tests::Fixture;
    use crate::openxr_data::Hand;
    use fakexr::ActionState;
    use fakexr::UserPath::*;
    use openvr as vr;
    use slotmap::Key;
    use std::ffi::CStr;

    macro_rules! get_dpad_action {
        ($fixture:expr, $handle:expr, $dpad_data:ident, $profile:ident) => {
            let input = $fixture.input.clone();
            let data = input.openxr.session_data.get();
            let actions = data.input_data.get_loaded_actions().unwrap();
            let path = $fixture
                .input
                .openxr
                .instance
                .string_to_path($profile::profile_path())
                .unwrap();
            let bindings = actions.try_get_bindings($handle, path).unwrap();

            let bindings: Vec<&DpadData> = bindings
                .iter()
                .filter_map(|x| match x {
                    BoolBindingData {
                        ty: BoolBindingType::Dpad(a),
                        ..
                    } => Some(a),
                    _ => None,
                })
                .collect();
            if bindings.len() != 1 {
                panic!("Got {} dpad bindings when one was expected", bindings.len());
            }

            let $dpad_data = &bindings[0].actions;
        };
    }

    /// The force and value actions of a grab binding.
    fn grab_actions(
        f: &Fixture,
        handle: vr::VRActionHandle_t,
    ) -> (xr::sys::Action, xr::sys::Action) {
        grab_actions_hand(f, handle, None)
    }

    fn grab_actions_hand(
        f: &Fixture,
        handle: vr::VRActionHandle_t,
        hand: Option<Hand>,
    ) -> (xr::sys::Action, xr::sys::Action) {
        let hand = hand.map(|hand| f.input.get_subaction_path(hand));
        let data = f.input.openxr.session_data.get();
        let actions = data.input_data.get_loaded_actions().unwrap();
        let profile = f
            .input
            .openxr
            .instance
            .string_to_path(Knuckles::profile_path())
            .unwrap();
        actions
            .try_get_bindings(handle, profile)
            .unwrap()
            .iter()
            .filter(|b| hand.is_none_or(|hand| b.hand == hand))
            .find_map(|b| match &b.ty {
                BoolBindingType::Grab(grab) => Some((
                    grab.force_action.as_ref().unwrap().as_raw(),
                    grab.value_action.as_raw(),
                )),
                _ => None,
            })
            .unwrap()
    }

    #[derive(Copy, Clone, Default)]
    struct BoolState {
        active: bool,
        state: bool,
        changed: bool,
    }

    impl BoolState {
        fn set_active(mut self) -> Self {
            self.active = true;
            self
        }

        fn set_state(mut self) -> Self {
            self.state = true;
            self
        }

        fn set_changed(mut self) -> Self {
            self.changed = true;
            self
        }
    }

    impl Fixture {
        #[track_caller]
        fn verify_bool_state(
            &self,
            handle: vr::VRActionHandle_t,
            BoolState {
                active,
                state,
                changed,
            }: BoolState,
        ) {
            let act_state = self
                .get_bool_state(handle)
                .expect("Couldn't get bool action state");
            assert_eq!(active, act_state.bActive, "active does not match");
            assert_eq!(state, act_state.bState, "state does not match");
            assert_eq!(changed, act_state.bChanged, "changed does not match");
        }
    }

    #[test]
    fn dpad_input() {
        let mut f = Fixture::new();

        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

        f.load_actions(c"actions_dpad.json");
        f.input.openxr.restart_session().unwrap();

        get_dpad_action!(f, boolact, dpad_data, ViveWands);

        f.set_interaction_profile::<ViveWands>(LeftHand);
        fakexr::set_action_state(
            dpad_data.xy.as_raw(),
            fakexr::ActionState::Vector2(0.0, 0.55),
            LeftHand,
        );
        fakexr::set_action_state(
            dpad_data.click_or_touch.as_ref().unwrap().as_raw(),
            fakexr::ActionState::Float(1.0),
            LeftHand,
        );

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(state.bChanged);

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(!state.bChanged);

        fakexr::set_action_state(
            dpad_data.xy.as_raw(),
            fakexr::ActionState::Vector2(0.55, 0.0),
            LeftHand,
        );
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(!state.bState);
        assert!(state.bChanged);
    }

    #[test]
    fn dpad_input_different_sets_have_different_actions() {
        let f = Fixture::new();

        let boolact_set1 = f.get_action_handle(c"/actions/set1/in/boolact");
        let boolact_set2 = f.get_action_handle(c"/actions/set2/in/boolact");

        f.load_actions(c"actions_dpad.json");

        get_dpad_action!(f, boolact_set1, set1_dpad, ViveWands);
        get_dpad_action!(f, boolact_set2, set2_dpad, ViveWands);

        assert_ne!(set1_dpad.xy.as_raw(), set2_dpad.xy.as_raw());
    }

    #[test]
    fn dpad_input_use_non_dpad_when_available() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

        f.load_actions(c"actions_dpad_mixed.json");
        f.input.openxr.restart_session().unwrap();

        get_dpad_action!(f, boolact, _dpad, ViveWands);

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
    fn dpad_cross_profile_actions() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

        f.load_actions(c"actions_dpad_multi.json");
        f.input.openxr.restart_session().unwrap();

        get_dpad_action!(f, boolact, dpad_data_vive, ViveWands);
        get_dpad_action!(f, boolact, dpad_data_knuckles, Knuckles);

        // These bindings are on different dpads (trackpad vs thumbstick)
        assert_ne!(dpad_data_vive.xy.as_raw(), dpad_data_knuckles.xy.as_raw());

        f.set_interaction_profile::<ViveWands>(LeftHand);
        fakexr::set_action_state(
            dpad_data_vive.xy.as_raw(),
            fakexr::ActionState::Vector2(0.0, 0.55),
            LeftHand,
        );
        fakexr::set_action_state(
            dpad_data_vive.click_or_touch.as_ref().unwrap().as_raw(),
            fakexr::ActionState::Float(1.0),
            LeftHand,
        );

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(state.bChanged);

        f.set_interaction_profile::<Knuckles>(LeftHand);
        fakexr::set_action_state(
            dpad_data_knuckles.xy.as_raw(),
            fakexr::ActionState::Vector2(0.0, 0.0),
            LeftHand,
        );
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        // Any input on touchpad shouldn't trigger thumbstick dpad
        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(!state.bState);
        assert!(!state.bChanged);

        fakexr::set_action_state(
            dpad_data_knuckles.xy.as_raw(),
            fakexr::ActionState::Vector2(0.0, 0.55),
            LeftHand,
        );
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        // Verify thumbstick deflection is sufficient
        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(state.bChanged);

        f.set_interaction_profile::<ViveWands>(LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        // Verify action state stickiness across interaction profiles that this test assumes
        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(!state.bChanged);

        fakexr::set_action_state(
            dpad_data_vive.xy.as_raw(),
            fakexr::ActionState::Vector2(0.0, 0.0),
            LeftHand,
        );

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        // Verify dpad deactivation on sliding input to center
        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(!state.bState);
        assert!(state.bChanged);
    }

    #[test]
    fn dpad_input_same_action_on_different_inputs() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        f.load_actions(c"actions_dpad_two_inputs.json");

        f.set_interaction_profile::<OculusTouch>(LeftHand);
        let input = f.input.clone();
        let data = input.openxr.session_data.get();
        let actions = data.input_data.get_loaded_actions().unwrap();
        let path = f
            .input
            .openxr
            .instance
            .string_to_path(OculusTouch::profile_path())
            .unwrap();
        let bindings = actions.try_get_bindings(boolact, path).unwrap();

        let bindings: Vec<(&DpadData, xr::Path)> = bindings
            .iter()
            .filter_map(|x| match x {
                BoolBindingData {
                    hand,
                    ty: BoolBindingType::Dpad(a),
                    ..
                } => Some((a, *hand)),
                _ => None,
            })
            .collect();

        assert_eq!(bindings.len(), 2);
        let left_binding = bindings
            .iter()
            .find_map(|(data, path)| {
                (*path == f.input.get_subaction_path(Hand::Left)).then_some(*data)
            })
            .unwrap();
        let right_binding = bindings
            .iter()
            .find_map(|(data, path)| {
                (*path == f.input.get_subaction_path(Hand::Right)).then_some(*data)
            })
            .unwrap();
        assert!(!std::ptr::eq(left_binding, right_binding));

        fakexr::set_action_state(
            left_binding.actions.xy.as_raw(),
            ActionState::Vector2(1.0, 0.0),
            LeftHand,
        );

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s = f.get_bool_state(boolact).unwrap();
        assert!(s.bActive);
        assert!(s.bState);
        assert!(s.bChanged);

        let s = f
            .get_bool_state_hand(boolact, f.input.left_hand_key.data().as_ffi())
            .unwrap();
        assert!(s.bActive);
        assert!(s.bState);
        assert!(s.bChanged);

        let s = f
            .get_bool_state_hand(boolact, f.input.right_hand_key.data().as_ffi())
            .unwrap();
        assert!(!s.bActive);
        assert!(!s.bState);
        assert!(!s.bChanged);

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s = f.get_bool_state(boolact).unwrap();
        assert!(s.bActive);
        assert!(s.bState);
        assert!(!s.bChanged);

        let s = f
            .get_bool_state_hand(boolact, f.input.left_hand_key.data().as_ffi())
            .unwrap();
        assert!(s.bActive);
        assert!(s.bState);
        assert!(!s.bChanged);
    }

    #[test]
    fn grab_binding() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact2");
        f.load_actions(c"actions.json");
        let (force_action, value_action) = grab_actions(&f, boolact);

        f.set_interaction_profile::<Knuckles>(LeftHand);
        let mut value_state_check = |force, value, state, changed, line| {
            fakexr::set_action_state(force_action, fakexr::ActionState::Float(force), LeftHand);
            fakexr::set_action_state(value_action, fakexr::ActionState::Float(value), LeftHand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });

            let s = f.get_bool_state(boolact).unwrap();
            assert_eq!(s.bState, state, "state failed (line {line})");
            assert!(s.bActive, "active failed (line {line})");
            assert_eq!(s.bChanged, changed, "changed failed (line {line})");
        };

        let grab = GrabBindingData::DEFAULT_GRAB_THRESHOLD;
        let release = GrabBindingData::DEFAULT_RELEASE_THRESHOLD;
        value_state_check(0.0, grab - 0.1, false, false, line!());
        value_state_check(0.0, grab + 0.1, true, true, line!());
        value_state_check(0.1, 0.0, true, false, line!());
        value_state_check(0.0, 1.0, true, false, line!());
        value_state_check(0.0, release, false, true, line!());
        value_state_check(0.0, grab - 0.1, false, false, line!());
    }

    #[test]
    fn grab_per_hand() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");

        let left = f.get_input_source_handle(c"/user/hand/left");
        let right = f.get_input_source_handle(c"/user/hand/right");

        f.load_actions(c"actions_dpad_mixed.json");

        let left_actions = grab_actions_hand(&f, boolact, Some(Hand::Left));
        let right_actions = grab_actions_hand(&f, boolact, Some(Hand::Right));

        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);

        let mut value_state_check = |force, value, hand, state, changed, line| {
            let (force_action, value_action) = match hand {
                LeftHand => left_actions,
                RightHand => right_actions,
            };
            fakexr::set_action_state(force_action, fakexr::ActionState::Float(force), hand);
            fakexr::set_action_state(value_action, fakexr::ActionState::Float(value), hand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });

            let restrict = match hand {
                LeftHand => left,
                RightHand => right,
            };
            let s = f.get_bool_state_hand(boolact, restrict).unwrap();
            assert_eq!(s.bState, state, "State wrong (line {line})");
            assert!(s.bActive, "Active wrong (line {line})");
            assert_eq!(s.bChanged, changed, "Changed wrong (line {line})");
        };

        let grab = GrabBindingData::DEFAULT_GRAB_THRESHOLD;
        let release = GrabBindingData::DEFAULT_RELEASE_THRESHOLD;
        value_state_check(0.0, grab - 0.1, LeftHand, false, false, line!());
        value_state_check(0.0, grab - 0.1, RightHand, false, false, line!());

        value_state_check(0.0, grab, LeftHand, true, true, line!());
        value_state_check(0.0, grab, RightHand, true, true, line!());

        value_state_check(0.0, release, LeftHand, false, true, line!());
        value_state_check(0.0, 1.0, RightHand, true, false, line!());
    }

    #[test]
    fn grab_binding_custom_threshold() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        f.load_actions(c"actions.json");
        let (force_action, value_action) = grab_actions(&f, boolact);

        f.set_interaction_profile::<Knuckles>(RightHand);
        let mut value_state_check = |force, value, state, changed, line| {
            fakexr::set_action_state(force_action, fakexr::ActionState::Float(force), RightHand);
            fakexr::set_action_state(value_action, fakexr::ActionState::Float(value), RightHand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });

            let s = f.get_bool_state(boolact).unwrap();
            assert_eq!(s.bState, state, "state failed (line {line})");
            assert!(s.bActive, "active failed (line {line})");
            assert_eq!(s.bChanged, changed, "changed failed (line {line})");
        };

        let grab = 0.16;
        let release = 0.15;
        value_state_check(0.0, 1.0, false, false, line!());
        value_state_check(grab + 0.01, 0.0, true, true, line!());
        value_state_check(grab - 0.001, 0.0, true, false, line!());
        value_state_check(release, 0.0, false, true, line!());
        value_state_check(0.0, 1.0, false, false, line!());
    }

    #[test]
    fn toggle_button() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        f.load_actions(c"actions_toggle.json");

        let toggle_data = f.custom_binding_action(boolact, Knuckles::profile_path(), |ty| {
            matches!(ty, BoolBindingType::Toggle(_))
        });

        f.set_interaction_profile::<Knuckles>(LeftHand);
        fakexr::set_action_state(toggle_data, fakexr::ActionState::Bool(true), LeftHand);

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(state.bChanged);

        fakexr::set_action_state(toggle_data, fakexr::ActionState::Bool(false), LeftHand);

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(state.bState);
        assert!(!state.bChanged);

        fakexr::set_action_state(toggle_data, fakexr::ActionState::Bool(true), LeftHand);

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(!state.bState);
        assert!(state.bChanged);

        // no change across sync point
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(!state.bState);
        assert!(!state.bChanged);
    }

    #[test]
    fn toggle_button_per_hand() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        let left = f.get_input_source_handle(c"/user/hand/left");
        let right = f.get_input_source_handle(c"/user/hand/right");

        f.load_actions(c"actions_toggle.json");
        let toggle_action = |hand| {
            let hand = f.input.get_subaction_path(hand);
            let data = f.input.openxr.session_data.get();
            let actions = data.input_data.get_loaded_actions().unwrap();
            let profile = f
                .input
                .openxr
                .instance
                .string_to_path(Knuckles::profile_path())
                .unwrap();
            actions
                .try_get_bindings(boolact, profile)
                .unwrap()
                .iter()
                .find(|b| b.hand == hand && matches!(b.ty, BoolBindingType::Toggle(_)))
                .unwrap()
                .ty
                .actions()[0]
        };
        let left_act = toggle_action(Hand::Left);
        let right_act = toggle_action(Hand::Right);

        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);
        fakexr::set_action_state(left_act, false.into(), LeftHand);
        fakexr::set_action_state(right_act, false.into(), RightHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(!s_left.bState);
        assert!(!s_left.bChanged);

        let s_right = f.get_bool_state_hand(boolact, right).unwrap();
        assert!(s_right.bActive);
        assert!(!s_right.bState);
        assert!(!s_right.bChanged);

        fakexr::set_action_state(left_act, true.into(), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(s_left.bState);
        assert!(s_left.bChanged);

        let s_right = f.get_bool_state_hand(boolact, right).unwrap();
        assert!(s_right.bActive);
        assert!(!s_right.bState);
        assert!(!s_right.bChanged);

        fakexr::set_action_state(left_act, false.into(), LeftHand);
        fakexr::set_action_state(right_act, true.into(), RightHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(s_left.bState);
        assert!(!s_left.bChanged);

        let s_right = f.get_bool_state_hand(boolact, right).unwrap();
        assert!(s_right.bActive);
        assert!(s_right.bState);
        assert!(s_right.bChanged);
    }

    #[test]
    fn grip_touch_from_pull_oculus() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact2");
        let left = f.get_input_source_handle(c"/user/hand/left");

        f.load_actions(c"actions.json");
        f.verify_custom_bindings(
            OculusTouch::profile_path(),
            c"/actions/set1/in/boolact2",
            ["/user/hand/left/input/squeeze/value".into()],
        );
        let act = f.custom_binding_action(boolact, OculusTouch::profile_path(), |ty| {
            matches!(ty, BoolBindingType::ThresholdFloat(_))
        });

        f.set_interaction_profile::<OculusTouch>(LeftHand);
        fakexr::set_action_state(act, ActionState::Float(0.0), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(!s_left.bState);
        assert!(!s_left.bChanged);

        fakexr::set_action_state(act, ActionState::Float(0.01), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(s_left.bState);
        assert!(s_left.bChanged);

        fakexr::set_action_state(act, ActionState::Float(0.0), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(!s_left.bState);
        assert!(s_left.bChanged);
    }

    #[test]
    fn thresholds_per_input() {
        // Right grip (default threshold) and right trigger (0.75) both click boolact. Each must
        // be compared against its own threshold, not against the larger of the two inputs.
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        let right = f.get_input_source_handle(c"/user/hand/right");
        f.load_actions(c"actions.json");
        f.set_interaction_profile::<OculusTouch>(RightHand);

        let input_action = |path: &CStr| f.get_action::<f32>(f.get_action_handle(path));
        let grip = input_action(c"/user/hand/right/input/squeeze/value-/actions/set1");
        let trigger = input_action(c"/user/hand/right/input/trigger/value-/actions/set1");
        assert_ne!(grip, trigger);

        let mut check = |grip_value, trigger_value, state| {
            fakexr::set_action_state(grip, ActionState::Float(grip_value), RightHand);
            fakexr::set_action_state(trigger, ActionState::Float(trigger_value), RightHand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });
            let s = f.get_bool_state_hand(boolact, right).unwrap();
            assert_eq!(
                s.bState, state,
                "grip {grip_value}, trigger {trigger_value}"
            );
        };
        check(0.0, 0.3, false);
        check(0.0, 0.8, true);
        check(0.0, 0.0, false);
        check(0.3, 0.0, true);
    }

    #[test]
    fn trigger_no_touch_from_pull_oculus() {
        let f = Fixture::new();

        f.load_actions(c"actions.json");
        let handle = f.get_action_handle(c"/actions/set1/in/boolact3");
        let profile = f
            .input
            .openxr
            .instance
            .string_to_path(OculusTouch::profile_path())
            .unwrap();
        let data = f.input.openxr.session_data.get();
        let actions = data.input_data.get_loaded_actions().unwrap();
        assert!(
            actions
                .try_get_bindings(handle, profile)
                .into_iter()
                .flatten()
                .all(|b| !matches!(b.ty, BoolBindingType::ThresholdFloat(_)))
        );
    }

    #[test]
    fn grab_slot_in_button_mode_oculus() {
        // Some games (e.g. Half-Life 2: VR) bind grip grab with mode "button" and a
        // nonstandard "grab" input slot instead of "click". It should behave like click.
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        let left = f.get_input_source_handle(c"/user/hand/left");

        f.load_actions(c"actions_grab_button.json");
        f.verify_custom_bindings(
            OculusTouch::profile_path(),
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/squeeze/value".into(),
                "/user/hand/right/input/squeeze/value".into(),
            ],
        );
        let act = f.custom_binding_action(boolact, OculusTouch::profile_path(), |ty| {
            matches!(ty, BoolBindingType::ThresholdFloat(_))
        });

        f.set_interaction_profile::<OculusTouch>(LeftHand);
        fakexr::set_action_state(act, ActionState::Float(0.0), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(!s_left.bState);
        assert!(!s_left.bChanged);

        // Above the 0.8 activate threshold from the binding parameters
        fakexr::set_action_state(act, ActionState::Float(0.9), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(s_left.bState);
        assert!(s_left.bChanged);

        fakexr::set_action_state(act, ActionState::Float(0.5), LeftHand);
        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });

        let s_left = f.get_bool_state_hand(boolact, left).unwrap();
        assert!(s_left.bActive);
        assert!(!s_left.bState);
        assert!(s_left.bChanged);
    }

    #[test]
    fn double_tap() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let active_set = vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        };
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact3");
        f.load_actions(c"actions.json");
        let double_action = f.custom_binding_action(boolact, Knuckles::profile_path(), |ty| {
            matches!(ty, BoolBindingType::DoubleTap(_))
        });
        let set_action = |state: bool| {
            fakexr::set_action_state(double_action, fakexr::ActionState::Bool(state), LeftHand);
        };

        f.set_interaction_profile::<Knuckles>(LeftHand);
        set_action(false);
        f.sync(active_set);
        let inactive_state = BoolState::default().set_active();
        f.verify_bool_state(boolact, inactive_state);

        set_action(true);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);

        set_action(false);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);

        set_action(true);
        let active_state = inactive_state.set_state();
        f.sync(active_set);
        f.verify_bool_state(boolact, active_state.set_changed());

        // Hold
        set_action(true);
        f.sync(active_set);
        f.verify_bool_state(boolact, active_state);

        set_action(false);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state.set_changed());

        set_action(false);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);
    }

    #[test]
    fn double_tap_timeout() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let active_set = vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        };
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact3");
        f.load_actions(c"actions.json");
        let double_action = f.custom_binding_action(boolact, Knuckles::profile_path(), |ty| {
            matches!(ty, BoolBindingType::DoubleTap(_))
        });
        let set_action = |state: bool| {
            fakexr::set_action_state(double_action, fakexr::ActionState::Bool(state), LeftHand);
        };

        f.set_interaction_profile::<Knuckles>(LeftHand);

        set_action(false);
        f.sync(active_set);
        let inactive_state = BoolState::default().set_active();
        f.verify_bool_state(boolact, inactive_state);

        set_action(true);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);

        set_action(false);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);

        let duration = std::time::Duration::from_millis(DoubleTapData::TIMEOUT_MS as u64 + 1);
        let late_press_time = xr::Time::from_nanos(duration.as_nanos() as _);
        let set_action_late = |state| {
            fakexr::set_action_state_with_time(
                double_action,
                fakexr::ActionState::Bool(state),
                LeftHand,
                late_press_time,
            );
        };

        // fail
        set_action_late(true);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);

        // following double tap should succeed
        set_action_late(false);
        f.sync(active_set);
        f.verify_bool_state(boolact, inactive_state);

        let active_state = inactive_state.set_state();
        set_action_late(true);
        f.sync(active_set);
        f.verify_bool_state(boolact, active_state.set_changed());
    }

    #[test]
    fn double_tap_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        f.verify_custom_bindings(
            Knuckles::profile_path(),
            c"/actions/set1/in/boolact3",
            ["/user/hand/left/input/a/click".to_string()],
        );
    }
}
