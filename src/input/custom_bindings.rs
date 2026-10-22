use super::action_manifest::{ClickThresholdParams, GrabParameters};
use crate::input::inactive_state;
use crate::openxr_data::SessionData;
use log::{error, warn};
use openvr as vr;
use openxr as xr;
use std::f32::consts::{FRAC_PI_4, PI};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
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
    pub transform: Vector2Transform,
}

/// How a 2D input becomes an OpenVR vector2 value.
pub(super) enum Vector2Transform {
    Position {
        invert: Vector2Inversion,
        /// Fractions of the full deflection
        deadzone: f32,
        maxzone: f32,
    },
    Scroll(ScrollData),
}

impl Vector2Transform {
    /// Whether a position binding is used as is.
    pub fn is_identity(&self) -> bool {
        matches!(
            self,
            Self::Position {
                invert: Vector2Inversion::None,
                deadzone: 0.0,
                maxzone: 1.0,
            }
        )
    }

    fn apply(
        &self,
        mut value: xr::Vector2f,
        session: &xr::Session<xr::AnyGraphics>,
        hand: xr::Path,
    ) -> xr::Vector2f {
        match self {
            Self::Position {
                invert,
                deadzone,
                maxzone,
            } => {
                invert.apply(&mut value);
                apply_deadzone(&mut value, *deadzone, *maxzone);
                value
            }
            Self::Scroll(scroll) => scroll.apply(value, session, hand),
        }
    }
}

/// Rescales the length of `value` so that `deadzone` maps to 0 and `maxzone` to 1.
fn apply_deadzone(value: &mut xr::Vector2f, deadzone: f32, maxzone: f32) {
    let len = value.x.hypot(value.y);
    if len == 0.0 || (deadzone == 0.0 && maxzone == 1.0) {
        return;
    }
    let scaled = if maxzone <= deadzone {
        if len >= deadzone { 1.0 } else { 0.0 }
    } else {
        ((len - deadzone) / (maxzone - deadzone)).clamp(0.0, 1.0)
    };
    value.x *= scaled / len;
    value.y *= scaled / len;
}

#[cfg(test)]
#[test]
fn deadzone_remap() {
    let remap = |y, deadzone, maxzone| {
        let mut value = xr::Vector2f { x: 0.0, y };
        apply_deadzone(&mut value, deadzone, maxzone);
        // Round off float noise.
        (value.y * 1000.0).round() / 1000.0
    };
    assert_eq!(remap(0.5, 0.0, 1.0), 0.5);
    assert_eq!(remap(0.1, 0.2, 1.0), 0.0);
    assert_eq!(remap(0.6, 0.2, 1.0), 0.5);
    assert_eq!(remap(-0.6, 0.2, 1.0), -0.5);
    assert_eq!(remap(0.95, 0.2, 0.9), 1.0);
    // No division by zero when the zones overlap.
    assert_eq!(remap(0.4, 0.5, 0.5), 0.0);
    assert_eq!(remap(0.6, 0.5, 0.5), 1.0);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScrollSource {
    Trackpad,
    Joystick,
}

#[derive(Default)]
struct ScrollState {
    /// Trackpad position at the last sync, while touched.
    last_position: Option<xr::Vector2f>,
    /// Trackpad movement not yet turned into a tick.
    accumulated: xr::Vector2f,
    last_sync: Option<Instant>,
    /// Joystick direction per axis, and when it next repeats.
    joystick: [(i8, Option<Instant>); 2],
}

/// Scroll mode: reports scroll amounts rather than the position. Discrete scrolling reports
/// ticks of ±1, smooth scrolling fractional amounts.
pub(super) struct ScrollData {
    input: ScrollSource,
    smooth: bool,
    multiplier: f32,
    /// The trackpad's touch, if it has one.
    touch: Option<xr::Action<f32>>,
    state: Mutex<ScrollState>,
}

impl ScrollData {
    /// Trackpad travel (of 2 across the pad) per discrete tick.
    const TRACKPAD_TICK: f32 = 0.2;
    const JOYSTICK_PRESS: f32 = 0.5;
    const JOYSTICK_RELEASE: f32 = 0.4;
    const JOYSTICK_REPEAT_DELAY: Duration = Duration::from_millis(500);
    const JOYSTICK_REPEAT: Duration = Duration::from_millis(125);
    /// Smooth joystick scrolling at full deflection, in ticks per second.
    const JOYSTICK_SMOOTH_RATE: f32 = 8.0;

    pub fn new(
        input: ScrollSource,
        smooth: bool,
        multiplier: f32,
        touch: Option<xr::Action<f32>>,
    ) -> Self {
        Self {
            input,
            smooth,
            multiplier,
            touch,
            state: Default::default(),
        }
    }

    fn apply(
        &self,
        position: xr::Vector2f,
        session: &xr::Session<xr::AnyGraphics>,
        hand: xr::Path,
    ) -> xr::Vector2f {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let dt = state
            .last_sync
            .replace(now)
            .map_or(0.0, |last| (now - last).as_secs_f32().min(0.1));

        match self.input {
            ScrollSource::Trackpad => {
                let touched = match self.touch.as_ref().map(|t| t.state(session, hand)) {
                    Some(Ok(touch)) if touch.is_active => touch.current_state > 0.5,
                    _ => position.x != 0.0 || position.y != 0.0,
                };
                if !touched {
                    state.last_position = None;
                    state.accumulated = Default::default();
                    return Default::default();
                }
                // Touching down is not a movement.
                let Some(last) = state.last_position.replace(position) else {
                    return Default::default();
                };
                let delta = xr::Vector2f {
                    x: position.x - last.x,
                    y: position.y - last.y,
                };
                if self.smooth {
                    return xr::Vector2f {
                        x: delta.x * self.multiplier,
                        y: delta.y * self.multiplier,
                    };
                }
                let tick = |accumulated: &mut f32, delta: f32| {
                    *accumulated += delta;
                    let ticks = (*accumulated / Self::TRACKPAD_TICK).trunc();
                    *accumulated -= ticks * Self::TRACKPAD_TICK;
                    ticks
                };
                xr::Vector2f {
                    x: tick(&mut state.accumulated.x, delta.x),
                    y: tick(&mut state.accumulated.y, delta.y),
                }
            }
            ScrollSource::Joystick if self.smooth => xr::Vector2f {
                x: position.x * self.multiplier * Self::JOYSTICK_SMOOTH_RATE * dt,
                y: position.y * self.multiplier * Self::JOYSTICK_SMOOTH_RATE * dt,
            },
            ScrollSource::Joystick => {
                let mut tick = |axis: usize, value: f32| {
                    let (direction, next_repeat) = &mut state.joystick[axis];
                    let threshold = if *direction == 0 {
                        Self::JOYSTICK_PRESS
                    } else {
                        Self::JOYSTICK_RELEASE
                    };
                    let new_direction = if value.abs() >= threshold {
                        value.signum() as i8
                    } else {
                        0
                    };
                    if new_direction != *direction {
                        *direction = new_direction;
                        *next_repeat = Some(now + Self::JOYSTICK_REPEAT_DELAY);
                        return new_direction as f32;
                    }
                    match next_repeat {
                        Some(at) if *direction != 0 && now >= *at => {
                            *at += Self::JOYSTICK_REPEAT;
                            *direction as f32
                        }
                        _ => 0.0,
                    }
                };
                xr::Vector2f {
                    x: tick(0, position.x),
                    y: tick(1, position.y),
                }
            }
        }
    }
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
                state.current_state = binding.transform.apply(state.current_state, session, hand);
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
    Timed(TimedPressData),
    Chord(ChordData),
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
            Self::Timed(timed) => timed.state(session, subaction_path),
            Self::Chord(chord) => chord.state(session),
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
            Self::Timed(timed) => timed.press.actions(),
            Self::Chord(chord) => chord.parts.iter().flat_map(|(_, p)| p.actions()).collect(),
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
            // Still bound, so report the release edge like any other button.
            ret_state.changed_since_last_sync = self
                .last_state
                .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
            return Ok(Some(ret_state));
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
        let hold_threshold = params
            .and_then(|p| p.value_hold_threshold.as_deref().copied())
            .unwrap_or(Self::DEFAULT_GRAB_THRESHOLD);
        Self {
            force_action,
            value_action,
            hold_threshold,
            release_threshold: params
                .and_then(|p| p.value_release_threshold.as_deref().copied())
                .unwrap_or(Self::DEFAULT_RELEASE_THRESHOLD)
                // Releasing above the grab point would flip the state at every sync between them.
                .min(hold_threshold),
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
        let click_threshold = params
            .and_then(|p| p.click_activate_threshold.as_deref().copied())
            .unwrap_or(Self::DEFAULT_CLICK_THRESHOLD);
        Self {
            action,
            click_threshold,
            release_threshold: params
                .and_then(|p| p.click_deactivate_threshold.as_deref().copied())
                .unwrap_or(Self::DEFAULT_RELEASE_THRESHOLD)
                // Releasing above the click point would flip the state at every sync between them.
                .min(click_threshold),
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

#[derive(Clone, Copy)]
pub(super) enum TimedPress {
    /// Active from the given hold time until release.
    Long(Duration),
    /// Active for one sync after a press that isn't the start of a double tap (if one is bound)
    /// or a long press (if one is bound, lasting the given time).
    Single {
        double: bool,
        long_delay: Option<Duration>,
    },
}

#[derive(Default)]
struct TimedPressState {
    pressed_at: Option<Instant>,
    /// When a press that may become a single press was released.
    released_at: Option<Instant>,
    /// The current press is the second one of a double tap.
    double: bool,
    last_state: bool,
}

/// Button presses that depend on how long the button is held, timed at each sync.
pub(super) struct TimedPressData {
    press: Box<BoolBindingType>,
    kind: TimedPress,
    state: Mutex<TimedPressState>,
}

impl TimedPressData {
    pub const DEFAULT_LONG_PRESS_DELAY: Duration = Duration::from_millis(500);

    pub fn new(press: BoolBindingType, kind: TimedPress) -> Self {
        Self {
            press: press.into(),
            kind,
            state: Default::default(),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        subaction_path: xr::Path,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let mut state = self.state.lock().unwrap();
        let Some(press) = self.press.state(session, subaction_path)? else {
            *state = Default::default();
            return Ok(None);
        };
        let now = Instant::now();
        let pressed = press.current_state;

        let current_state = match self.kind {
            TimedPress::Long(delay) => {
                if pressed {
                    now - *state.pressed_at.get_or_insert(now) >= delay
                } else {
                    state.pressed_at = None;
                    false
                }
            }
            TimedPress::Single { double, long_delay } => {
                if pressed && state.pressed_at.is_none() {
                    state.pressed_at = Some(now);
                    state.double = state.released_at.take().is_some();
                } else if !pressed && let Some(pressed_at) = state.pressed_at.take() {
                    let long = long_delay.is_some_and(|delay| now - pressed_at >= delay);
                    if !std::mem::take(&mut state.double) && !long {
                        state.released_at = Some(now);
                    }
                }
                let wait = if double {
                    Duration::from_millis(DoubleTapData::TIMEOUT_MS as u64)
                } else {
                    Duration::ZERO
                };
                let fired = state.released_at.is_some_and(|at| now - at >= wait);
                if fired {
                    state.released_at = None;
                }
                fired
            }
        };

        let changed_since_last_sync = state.last_state != current_state;
        state.last_state = current_state;
        Ok(Some(xr::ActionState {
            current_state,
            changed_since_last_sync,
            last_change_time: press.last_change_time,
            is_active: true,
        }))
    }
}

/// Active while all its parts are, each read on its own hand.
pub(super) struct ChordData {
    parts: Vec<(xr::Path, BoolBindingType)>,
    last_state: AtomicBool,
}

impl ChordData {
    pub fn new(parts: Vec<(xr::Path, BoolBindingType)>) -> Self {
        Self {
            parts,
            last_state: false.into(),
        }
    }

    fn state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
    ) -> xr::Result<Option<xr::ActionState<bool>>> {
        let mut current_state = true;
        let mut last_change_time = xr::Time::from_nanos(0);
        let mut active = true;
        // Every part is read, as they keep their own state between syncs.
        for (hand, part) in &self.parts {
            match part.state(session, *hand)? {
                Some(state) if state.is_active => {
                    current_state &= state.current_state;
                    if state.last_change_time.as_nanos() > last_change_time.as_nanos() {
                        last_change_time = state.last_change_time;
                    }
                }
                _ => active = false,
            }
        }
        if !active {
            self.last_state.store(false, Ordering::Relaxed);
            return Ok(None);
        }

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
            last_change_time,
            is_active: true,
        }))
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
    use openvr::IVRInput010_Interface;
    use slotmap::Key;
    use std::ffi::CStr;
    use std::time::Duration;

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

        // Lifting the finger still in the north wedge releases it.
        fakexr::set_action_state(
            dpad_data.click_or_touch.as_ref().unwrap().as_raw(),
            fakexr::ActionState::Float(0.0),
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

        f.sync(vr::VRActiveActionSet_t {
            ulActionSet: set1,
            ..Default::default()
        });
        let state = f.get_bool_state(boolact).unwrap();
        assert!(state.bActive);
        assert!(!state.bState);
        assert!(!state.bChanged);

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
        assert!(state.bState);
        assert!(state.bChanged);

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
    fn grab_without_force_sensor() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let grab = f.get_action_handle(c"/actions/set1/in/grab");
        f.load_actions(c"actions_grab_mode.json");
        f.verify_custom_bindings(
            OculusTouch::profile_path(),
            c"/actions/set1/in/grab",
            ["/user/hand/left/input/squeeze/value".into()],
        );

        f.set_interaction_profile::<OculusTouch>(LeftHand);
        let squeeze = f.get_action::<f32>(
            f.get_action_handle(c"/user/hand/left/input/squeeze/value-/actions/set1"),
        );
        let mut check = |value, state| {
            fakexr::set_action_state(squeeze, ActionState::Float(value), LeftHand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });
            let s = f.get_bool_state(grab).unwrap();
            assert!(s.bActive);
            assert_eq!(s.bState, state, "{value}");
        };
        check(0.68, false);
        check(0.75, true);
        check(0.68, true);
        check(0.6, false);
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
    fn grip_touch_from_pull_knuckles() {
        // Knuckles has no grip touch, only an analog squeeze value. Touch must turn on at
        // the slightest pull, not at the runtime's click point.
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        let left = f.get_input_source_handle(c"/user/hand/left");
        f.load_actions(c"actions.json");
        f.set_interaction_profile::<Knuckles>(LeftHand);

        let squeeze = f.get_action::<f32>(
            f.get_action_handle(c"/user/hand/left/input/squeeze/value-/actions/set1"),
        );
        let mut check = |value, state, changed| {
            fakexr::set_action_state(squeeze, ActionState::Float(value), LeftHand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });
            let s = f.get_bool_state_hand(boolact, left).unwrap();
            assert!(s.bActive);
            assert_eq!((s.bState, s.bChanged), (state, changed), "squeeze {value}");
        };
        check(0.0, false, false);
        check(0.02, true, true);
        check(0.0, false, true);
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
    fn trigger_click_thresholds() {
        let mut f = Fixture::new();
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let fire = f.get_action_handle(c"/actions/set1/in/fire");
        let left = f.get_input_source_handle(c"/user/hand/left");
        f.load_actions(c"actions_trigger_click.json");

        // Touch has no trigger click: the binding's thresholds apply to the value. Without
        // thresholds the runtime's click point stays.
        f.verify_threshold_bindings(
            OculusTouch::profile_path(),
            c"/actions/set1/in/fire",
            ["/user/hand/left/input/trigger/value".into()],
        );
        f.verify_bindings::<bool>(
            OculusTouch::profile_path(),
            c"/actions/set1/in/fire",
            ["/user/hand/right/input/trigger/value".into()],
        );
        // The physical click is kept.
        f.verify_bindings::<bool>(
            Knuckles::profile_path(),
            c"/actions/set1/in/fire",
            [
                "/user/hand/left/input/trigger/click".into(),
                "/user/hand/right/input/trigger/click".into(),
            ],
        );

        f.set_interaction_profile::<OculusTouch>(LeftHand);
        let trigger = f.get_action::<f32>(
            f.get_action_handle(c"/user/hand/left/input/trigger/value-/actions/set1"),
        );
        let mut check = |value, state| {
            fakexr::set_action_state(trigger, ActionState::Float(value), LeftHand);
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });
            assert_eq!(
                f.get_bool_state_hand(fire, left).unwrap().bState,
                state,
                "{value}"
            );
        };
        check(0.7, false);
        check(0.76, true);
        check(0.73, true);
        check(0.71, false);

        // Only an activation threshold below the default release threshold: the state holds
        // between them instead of flipping at every sync.
        let fire_light = f.get_action_handle(c"/actions/set1/in/fire_light");
        let right = f.get_input_source_handle(c"/user/hand/right");
        f.set_interaction_profile::<OculusTouch>(RightHand);
        let trigger = f.get_action::<f32>(
            f.get_action_handle(c"/user/hand/right/input/trigger/value-/actions/set1"),
        );
        fakexr::set_action_state(trigger, ActionState::Float(0.15), RightHand);
        for _ in 0..3 {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            });
            assert!(f.get_bool_state_hand(fire_light, right).unwrap().bState);
        }
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
    // Depends on wall clock time between syncs, which is far longer under miri.
    #[cfg_attr(miri, ignore)]
    fn timed_presses() {
        let mut f = Fixture::new();
        let set1 = vr::VRActiveActionSet_t {
            ulActionSet: f.get_action_set_handle(c"/actions/set1"),
            ..Default::default()
        };
        let [click, long, held, single, double, single_now] = [
            c"/actions/set1/in/click",
            c"/actions/set1/in/long",
            c"/actions/set1/in/held",
            c"/actions/set1/in/single",
            c"/actions/set1/in/double",
            c"/actions/set1/in/single_now",
        ]
        .map(|name| f.get_action_handle(name));
        f.load_actions(c"actions_timed.json");
        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);

        let a_click = f.get_action::<bool>(click);
        let a_bool = f.get_action::<bool>(
            f.get_action_handle(c"/user/hand/left/input/a/click-/actions/set1-bool"),
        );
        let a_float = f
            .get_action::<f32>(f.get_action_handle(c"/user/hand/left/input/a/click-/actions/set1"));
        let time = std::cell::Cell::new(0);
        let press_a = |pressed: bool| {
            // Presses a second apart, so only the timed bindings see them close together.
            time.set(time.get() + 1_000_000_000);
            fakexr::set_action_state(a_click, pressed.into(), LeftHand);
            fakexr::set_action_state_with_time(
                a_bool,
                pressed.into(),
                LeftHand,
                xr::Time::from_nanos(time.get()),
            );
            fakexr::set_action_state(a_float, ActionState::Float(pressed as u8 as f32), LeftHand);
        };
        let states = |f: &mut Fixture| {
            f.sync(set1);
            [click, long, held, single, double].map(|a| f.get_bool_state(a).unwrap().bState)
        };

        // Holding past the long press delay.
        press_a(false);
        assert_eq!(states(&mut f), [false; 5]);
        press_a(true);
        assert_eq!(states(&mut f), [true, false, false, false, false]);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(states(&mut f), [true, true, true, false, false]);
        press_a(false);
        // Not a single press either.
        assert_eq!(states(&mut f), [false; 5]);
        std::thread::sleep(Duration::from_millis(DoubleTapData::TIMEOUT_MS as u64 + 10));
        assert_eq!(states(&mut f), [false; 5]);

        // A short press is a single press once a double tap is ruled out.
        press_a(true);
        assert_eq!(states(&mut f), [true, false, false, false, false]);
        press_a(false);
        assert_eq!(states(&mut f), [false; 5]);
        std::thread::sleep(Duration::from_millis(DoubleTapData::TIMEOUT_MS as u64 + 10));
        assert_eq!(states(&mut f), [false, false, false, true, false]);
        assert_eq!(states(&mut f), [false; 5]);

        // Without a double or long binding, single is a click.
        let b_click = f.get_action::<bool>(single_now);
        fakexr::set_action_state(b_click, true.into(), RightHand);
        f.sync(set1);
        let right = f.get_input_source_handle(c"/user/hand/right");
        assert!(f.get_bool_state_hand(single_now, right).unwrap().bState);
    }

    #[test]
    fn chords() {
        let mut f = Fixture::new();
        let set = vr::VRActiveActionSet_t {
            ulActionSet: f.get_action_set_handle(c"/actions/main"),
            ..Default::default()
        };
        let [dodge, facepiece, sit, bad, grab] = [
            c"/actions/main/in/dodge",
            c"/actions/main/in/facepiece",
            c"/actions/main/in/sit",
            c"/actions/main/in/bad",
            c"/actions/main/in/grab",
        ]
        .map(|name| f.get_action_handle(name));
        let right = f.get_input_source_handle(c"/user/hand/right");
        f.load_actions(c"actions_chords.json");
        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);

        let input = |path: &CStr| f.get_action_handle(path);
        let left_grip =
            f.get_action::<f32>(input(c"/user/hand/left/input/squeeze/value-/actions/main"));
        let right_grip =
            f.get_action::<f32>(input(c"/user/hand/right/input/squeeze/value-/actions/main"));
        let right_a = f.get_action::<f32>(input(c"/user/hand/right/input/a/click-/actions/main"));
        let right_stick =
            f.get_action::<xr::Vector2f>(input(c"/user/hand/right/input/thumbstick-/actions/main"));
        for (action, state) in [
            (left_grip, ActionState::Float(0.0)),
            (right_grip, ActionState::Float(0.0)),
            (right_a, ActionState::Float(0.0)),
        ] {
            fakexr::set_action_state(action, state, LeftHand);
            fakexr::set_action_state(action, state, RightHand);
        }
        fakexr::set_action_state(right_stick, ActionState::Vector2(0.0, 0.0), RightHand);

        let check = |f: &mut Fixture, handle, state: BoolState| {
            f.sync(set);
            f.verify_bool_state(handle, state);
        };
        let idle = BoolState::default().set_active();
        check(&mut f, dodge, idle);

        // One input of the chord alone doesn't count, but still drives its own binding.
        fakexr::set_action_state(left_grip, ActionState::Float(1.0), LeftHand);
        check(&mut f, dodge, idle);
        f.verify_bool_state(grab, idle.set_state().set_changed());
        // Inputs on both hands.
        fakexr::set_action_state(right_a, ActionState::Float(1.0), RightHand);
        check(&mut f, dodge, idle.set_state().set_changed());
        let s = f.get_bool_state_hand(dodge, right).unwrap();
        assert!(s.bState);
        check(&mut f, dodge, idle.set_state());
        fakexr::set_action_state(left_grip, ActionState::Float(0.0), LeftHand);
        check(&mut f, dodge, idle.set_changed());

        // A dpad direction as part of a chord.
        fakexr::set_action_state(right_grip, ActionState::Float(1.0), RightHand);
        check(&mut f, facepiece, idle);
        fakexr::set_action_state(right_stick, ActionState::Vector2(0.0, -0.8), RightHand);
        check(&mut f, facepiece, idle.set_state().set_changed());

        // Chords without inputs or with unknown ones are skipped.
        for handle in [sit, bad] {
            let s = f.get_bool_state(handle).unwrap();
            assert!(!s.bActive);
        }
    }

    #[test]
    // Depends on wall clock time between syncs, which is far longer under miri.
    #[cfg_attr(miri, ignore)]
    fn scroll_and_deadzone() {
        let mut f = Fixture::new();
        let set = vr::VRActiveActionSet_t {
            ulActionSet: f.get_action_set_handle(c"/actions/main"),
            ..Default::default()
        };
        let [movement, scroll_pad, scroll_stick] = [
            c"/actions/main/in/move",
            c"/actions/main/in/scroll_pad",
            c"/actions/main/in/scroll_stick",
        ]
        .map(|name| f.get_action_handle(name));
        let [left, right] =
            [c"/user/hand/left", c"/user/hand/right"].map(|hand| f.get_input_source_handle(hand));
        f.load_actions(c"actions_scroll.json");
        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);

        let vector2 = |path: &CStr| f.get_action::<xr::Vector2f>(f.get_action_handle(path));
        let left_stick = vector2(c"/user/hand/left/input/thumbstick-/actions/main");
        let right_stick = vector2(c"/user/hand/right/input/thumbstick-/actions/main");
        let pad = vector2(c"/user/hand/left/input/trackpad-/actions/main");
        let pad_touch = f.get_action::<f32>(
            f.get_action_handle(c"/user/hand/left/input/trackpad/touch-/actions/main"),
        );
        let analog = |f: &Fixture, handle, hand| {
            let mut data = vr::InputAnalogActionData_t::default();
            assert_eq!(
                f.input.GetAnalogActionData(
                    handle,
                    &mut data,
                    std::mem::size_of_val(&data) as u32,
                    hand,
                ),
                vr::EVRInputError::None
            );
            assert!(data.bActive);
            (
                (data.x * 1000.0).round() / 1000.0,
                (data.y * 1000.0).round() / 1000.0,
            )
        };
        let set_state = |action, hand, x, y| {
            fakexr::set_action_state(action, ActionState::Vector2(x, y), hand);
        };

        // Deadzone and maxzone of the stick.
        set_state(right_stick, RightHand, 0.0, 0.0);
        fakexr::set_action_state(pad_touch, ActionState::Float(0.0), LeftHand);
        set_state(pad, LeftHand, 0.0, 0.0);
        for (y, expected) in [(0.1, 0.0), (0.55, 0.5), (1.0, 1.0)] {
            set_state(left_stick, LeftHand, 0.0, y);
            f.sync(set);
            assert_eq!(analog(&f, movement, left), (0.0, expected), "{y}");
        }

        // A pushed stick scrolls one tick, then repeats only after a while.
        set_state(right_stick, RightHand, 0.0, 0.8);
        f.sync(set);
        assert_eq!(analog(&f, scroll_stick, right), (0.0, 1.0));
        f.sync(set);
        assert_eq!(analog(&f, scroll_stick, right), (0.0, 0.0));
        set_state(right_stick, RightHand, 0.0, 0.0);
        f.sync(set);
        assert_eq!(analog(&f, scroll_stick, right), (0.0, 0.0));
        set_state(right_stick, RightHand, 0.0, -0.8);
        f.sync(set);
        assert_eq!(analog(&f, scroll_stick, right), (0.0, -1.0));

        // A resting thumb doesn't scroll, moving it does.
        fakexr::set_action_state(pad_touch, ActionState::Float(1.0), LeftHand);
        set_state(pad, LeftHand, 0.0, 0.5);
        f.sync(set);
        assert_eq!(analog(&f, scroll_pad, left), (0.0, 0.0));
        f.sync(set);
        assert_eq!(analog(&f, scroll_pad, left), (0.0, 0.0));
        set_state(pad, LeftHand, 0.0, 0.75);
        f.sync(set);
        assert_eq!(analog(&f, scroll_pad, left), (0.0, 1.0));
        set_state(pad, LeftHand, 0.0, 0.8);
        f.sync(set);
        assert_eq!(analog(&f, scroll_pad, left), (0.0, 0.0));
        // Lifting the thumb and touching elsewhere isn't a movement.
        fakexr::set_action_state(pad_touch, ActionState::Float(0.0), LeftHand);
        f.sync(set);
        fakexr::set_action_state(pad_touch, ActionState::Float(1.0), LeftHand);
        set_state(pad, LeftHand, 0.0, -0.9);
        f.sync(set);
        assert_eq!(analog(&f, scroll_pad, left), (0.0, 0.0));
    }

    #[test]
    fn scalar_constant() {
        // Buttons drive the action (Blade & Sorcery binds Touch A and X this way), and analog
        // inputs set it to 1 when pressed rather than passing on the pull.
        let mut f = Fixture::new();
        let set = vr::VRActiveActionSet_t {
            ulActionSet: f.get_action_set_handle(c"/actions/main"),
            ..Default::default()
        };
        let repel = f.get_action_handle(c"/actions/main/in/repel");
        let left = f.get_input_source_handle(c"/user/hand/left");
        f.load_actions(c"actions_scalar_constant.json");
        f.verify_bindings::<f32>(
            OculusTouch::profile_path(),
            c"/actions/main/in/alternateuse",
            ["/user/hand/right/input/a/click".into()],
        );
        // Other constants than 1 can't be reported, so they aren't bound.
        let reverse = c"/actions/main/in/reverse";
        let profile = f
            .input
            .openxr
            .instance
            .string_to_path(OculusTouch::profile_path())
            .unwrap();
        assert!(fakexr::check_no_suggested_bindings(
            f.get_action::<f32>(f.get_action_handle(reverse)),
            profile
        ));
        f.verify_custom_bindings(OculusTouch::profile_path(), reverse, []);

        f.set_interaction_profile::<OculusTouch>(LeftHand);
        let trigger = f.get_action::<f32>(
            f.get_action_handle(c"/user/hand/left/input/trigger/value-/actions/main"),
        );
        for (pull, expected) in [(0.1, 0.0), (0.6, 1.0)] {
            fakexr::set_action_state(trigger, ActionState::Float(pull), LeftHand);
            f.sync(set);
            let mut data = vr::InputAnalogActionData_t::default();
            f.input.GetAnalogActionData(
                repel,
                &mut data,
                std::mem::size_of_val(&data) as u32,
                left,
            );
            assert!(data.bActive);
            assert_eq!(data.x, expected, "{pull}");
        }
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
