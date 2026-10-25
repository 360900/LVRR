//! Vibrations: OpenVR's parameters turned into OpenXR's, the haptics SteamVR plays for bindings,
//! and vibrations that start later.

use super::custom_bindings::BoolBindingData;
use super::{ActionKey, ActionSetKey, ManifestLoadedActions};
use crate::openxr_data::SessionData;
use openxr as xr;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A vibration in OpenVR's terms, as SteamVR passes it to drivers (`VREvent_HapticVibration_t`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Vibration {
    /// Seconds. 0 asks for a single pulse.
    pub duration: f32,
    /// Hz. 0 leaves it to the runtime.
    pub frequency: f32,
    pub amplitude: f32,
}

impl Vibration {
    /// The shortest vibration sent to the runtime, which single pulses last too. ALVR, which passes
    /// SteamVR's vibrations on to Quest and Pico controllers, makes every vibration at least this
    /// long by default.
    pub const MIN_DURATION: Duration = Duration::from_millis(10);
    /// The ranges SteamVR's drivers clamp to (openvr docs/Driver_API_Documentation.md, "Haptic
    /// Events").
    const MAX_DURATION: f32 = 10.0;
    const MIN_FREQUENCY: f32 = 1_000_000.0 / 65535.0;
    const MAX_FREQUENCY: f32 = 1_000_000.0 / 300.0;

    /// A legacy `TriggerHapticPulse`: a pulse of that length at full strength, like Valve's Unity
    /// Interaction System turns it into a vibration (Hand.TriggerHapticPulse). A Vive wand plays
    /// the length as strength (games scale it to 3999 us); controllers that vibrate by amplitude
    /// play every pulse at full strength, as with SteamVR. Its frequency there, one pulse per
    /// length (which the drivers' 1e6/65535 to 1e6/300 Hz range is made for), is a wand's pulse
    /// rate rather than OpenXR's waveform frequency, so that's left to the runtime.
    pub fn legacy_pulse(duration_us: u16) -> Option<Self> {
        (duration_us > 0).then(|| Self {
            duration: f32::from(duration_us) / 1_000_000.0,
            frequency: xr::FREQUENCY_UNSPECIFIED,
            amplitude: 1.0,
        })
    }

    /// The OpenXR vibration, or None if there's nothing to play. Values OpenXR has no meaning for
    /// (zero, negative or NaN durations and frequencies) become a single pulse at the runtime's
    /// frequency.
    pub fn to_xr(self) -> Option<xr::HapticVibration<'static>> {
        if self.amplitude.is_nan() {
            return None;
        }
        let duration = if self.duration > Self::MIN_DURATION.as_secs_f32() {
            // Microseconds, as f32 seconds carry noise below that.
            Duration::from_micros((self.duration.min(Self::MAX_DURATION) * 1e6).round() as u64)
        } else {
            Self::MIN_DURATION
        };
        let frequency = if self.frequency > 0.0 {
            self.frequency
                .clamp(Self::MIN_FREQUENCY, Self::MAX_FREQUENCY)
        } else {
            xr::FREQUENCY_UNSPECIFIED
        };
        Some(
            xr::HapticVibration::new()
                .amplitude(self.amplitude.clamp(0.0, 1.0))
                .frequency(frequency)
                .duration(xr::Duration::from_nanos(duration.as_nanos() as i64)),
        )
    }
}

/// Applies `vibration` to `action` on `hand` (NULL for every hand).
pub(super) fn apply(
    session: &xr::Session<xr::AnyGraphics>,
    action: &xr::Action<xr::Haptic>,
    hand: xr::Path,
    vibration: Vibration,
) -> xr::Result<()> {
    match vibration.to_xr() {
        Some(vibration) => action.apply_feedback(session, hand, &vibration),
        None => Ok(()),
    }
}

struct PendingVibration {
    at: Instant,
    action: xr::Action<xr::Haptic>,
    hand: xr::Path,
    vibration: Vibration,
}

/// Vibrations to start later (`start_seconds_from_now`, a binding's `haptic_delay`). The frame and
/// UpdateActionState start them once they're due, so nothing waits for them.
#[derive(Default)]
pub(super) struct PendingVibrations(Mutex<Vec<PendingVibration>>);

impl PendingVibrations {
    /// A second of one vibration per frame on each hand, with room to spare.
    const MAX: usize = 512;

    pub fn push(
        &self,
        at: Instant,
        action: &xr::Action<xr::Haptic>,
        hand: xr::Path,
        vibration: Vibration,
    ) {
        let mut pending = self.0.lock().unwrap();
        if pending.len() >= Self::MAX {
            crate::warn_once!("Too many vibrations waiting to start, dropping some");
            return;
        }
        pending.push(PendingVibration {
            at,
            action: action.clone(),
            hand,
            vibration,
        });
    }

    /// Starts the vibrations that are due, oldest first, passing each result to `check`.
    pub fn start_due(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        check: impl Fn(xr::Result<()>),
    ) {
        let due = {
            let mut pending = self.0.lock().unwrap();
            if pending.is_empty() {
                return;
            }
            let now = Instant::now();
            let (mut due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
                .into_iter()
                .partition(|p| p.at <= now);
            *pending = later;
            due.sort_by_key(|p| p.at);
            due
        };
        for p in due {
            check(apply(session, &p.action, p.hand, p.vibration));
        }
    }
}

/// The haptic SteamVR plays where a binding's click comes from an analog input crossing a
/// threshold ("a haptic bump once you hit an axis threshold"): a pulse on the press and another
/// on the release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct BindingHaptic {
    pub on: Option<Vibration>,
    pub off: Option<Vibration>,
    pub delay: Duration,
}

impl BindingHaptic {
    /// The binding UI's defaults (dashboard controllerbindingui.js): 0.2 for button and toggle
    /// button modes on analog inputs, 0 for grabs, 20 Hz, and a single pulse. Trigger mode has
    /// button mode's, see handle_binding_haptics.
    pub const DEFAULT_AMPLITUDE: f32 = 0.2;
    pub const DEFAULT_FREQUENCY: f32 = 20.0;

    /// None if neither the press nor the release vibrates.
    pub fn new(on: Vibration, off: Vibration, delay: Duration) -> Option<Self> {
        let audible = |v: Vibration| (v.amplitude > 0.0).then_some(v);
        let (on, off) = (audible(on), audible(off));
        (on.is_some() || off.is_some()).then_some(Self { on, off, delay })
    }
}

/// The click a binding haptic follows.
pub(super) enum BindingPress {
    /// One xrizer computes, from a threshold or a grab.
    Custom(BoolBindingData),
    /// One the runtime reads off an analog input.
    Runtime(xr::Action<bool>),
}

pub(super) struct BindingHapticData {
    profile: xr::Path,
    hand: xr::Path,
    press: BindingPress,
    haptic: BindingHaptic,
    /// Whether the last sync read the press. A change after a gap is the set (or the controller)
    /// becoming active with the input held, not a press.
    read: AtomicBool,
}

impl BindingHapticData {
    pub fn new(
        profile: xr::Path,
        hand: xr::Path,
        press: BindingPress,
        haptic: BindingHaptic,
    ) -> Self {
        Self {
            profile,
            hand,
            press,
            haptic,
            read: false.into(),
        }
    }

    /// Reads the press if the binding is `active`, returning the vibration its change calls for.
    fn sync(&self, session_data: &SessionData, active: bool) -> Option<Vibration> {
        if !active {
            self.read.store(false, Ordering::Relaxed);
            return None;
        }
        let state = match &self.press {
            BindingPress::Custom(binding) => {
                binding.unsync();
                binding.state(session_data, self.hand).ok().flatten()
            }
            BindingPress::Runtime(action) => action
                .state(&session_data.session, self.hand)
                .ok()
                .filter(|state| state.is_active),
        };
        let after_gap = !self.read.swap(true, Ordering::Relaxed);
        let state = state.filter(|state| state.changed_since_last_sync && !after_gap)?;
        if state.current_state {
            self.haptic.on
        } else {
            self.haptic.off
        }
    }
}

/// Plays the haptics of the bindings whose click changed, in the sets the last UpdateActionState
/// activated. Bindings a higher priority set masks don't play, like SteamVR doesn't run them.
/// `devices` are each device's subaction path and interaction profile: the hands, left first,
/// then the gamepad.
pub(super) fn sync_binding_haptics(
    session_data: &SessionData,
    loaded: &ManifestLoadedActions,
    active_sets: &[(ActionSetKey, xr::Path)],
    devices: [(xr::Path, xr::Path); 3],
    masked: &HashSet<(ActionKey, usize)>,
    check: impl Fn(xr::Result<()>),
) {
    for (set, action, haptic) in &loaded.binding_haptics {
        let Some(index) = devices.iter().position(|(hand, _)| *hand == haptic.hand) else {
            continue;
        };
        let (hand, profile) = devices[index];
        let active = profile == haptic.profile
            && !masked.contains(&(*action, index))
            && active_sets.iter().any(|(key, restricted)| {
                key == set && (*restricted == xr::Path::NULL || *restricted == hand)
            });
        let Some(vibration) = haptic.sync(session_data, active) else {
            continue;
        };
        let delay = haptic.haptic.delay;
        if delay.is_zero() {
            check(apply(
                &session_data.session,
                &loaded.haptic_action,
                hand,
                vibration,
            ));
        } else if let Some(at) = Instant::now().checked_add(delay) {
            loaded
                .pending_vibrations
                .push(at, &loaded.haptic_action, hand, vibration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xr_values(vibration: Vibration) -> Option<(i64, f32, f32)> {
        vibration.to_xr().map(|v| {
            let v = v.into_raw();
            (v.duration.as_nanos(), v.frequency, v.amplitude)
        })
    }

    const MIN_NANOS: i64 = Vibration::MIN_DURATION.as_nanos() as i64;

    #[test]
    fn vibration_to_xr() {
        let vibration = |duration, frequency, amplitude| Vibration {
            duration,
            frequency,
            amplitude,
        };
        assert_eq!(
            xr_values(vibration(0.5, 160.0, 0.5)),
            Some((500_000_000, 160.0, 0.5))
        );
        // A single pulse, and durations OpenXR doesn't take, last the minimum.
        for duration in [0.0, 0.001, -1.0, f32::NAN, f32::NEG_INFINITY] {
            assert_eq!(
                xr_values(vibration(duration, 160.0, 0.5)),
                Some((MIN_NANOS, 160.0, 0.5)),
                "{duration}"
            );
        }
        assert_eq!(
            xr_values(vibration(f32::INFINITY, 160.0, 0.5)),
            Some((10_000_000_000, 160.0, 0.5))
        );
        // Frequencies without a meaning are left to the runtime, others clamped like SteamVR's
        // drivers do.
        for frequency in [0.0, -5.0, f32::NAN] {
            assert_eq!(
                xr_values(vibration(0.5, frequency, 0.5)),
                Some((500_000_000, xr::FREQUENCY_UNSPECIFIED, 0.5)),
                "{frequency}"
            );
        }
        assert_eq!(
            xr_values(vibration(0.5, 0.5, 0.5)).unwrap().1,
            Vibration::MIN_FREQUENCY
        );
        assert_eq!(
            xr_values(vibration(0.5, 1e6, 0.5)).unwrap().1,
            Vibration::MAX_FREQUENCY
        );
        assert_eq!(xr_values(vibration(0.5, 160.0, 2.0)).unwrap().2, 1.0);
        assert_eq!(xr_values(vibration(0.5, 160.0, -1.0)).unwrap().2, 0.0);
        assert_eq!(xr_values(vibration(0.5, 160.0, f32::NAN)), None);
    }

    #[test]
    fn legacy_pulses() {
        assert_eq!(Vibration::legacy_pulse(0), None);
        let values = |us| xr_values(Vibration::legacy_pulse(us).unwrap()).unwrap();
        // At full strength, whatever the length.
        let unspecified = xr::FREQUENCY_UNSPECIFIED;
        assert_eq!(values(3999), (MIN_NANOS, unspecified, 1.0));
        assert_eq!(values(500), (MIN_NANOS, unspecified, 1.0));
        assert_eq!(values(20000), (20_000_000, unspecified, 1.0));
    }

    #[test]
    fn binding_haptic_off() {
        let pulse = |amplitude| Vibration {
            duration: 0.0,
            frequency: 20.0,
            amplitude,
        };
        assert_eq!(
            BindingHaptic::new(pulse(0.0), pulse(0.0), Duration::ZERO),
            None
        );
        assert_eq!(
            BindingHaptic::new(pulse(0.2), pulse(0.0), Duration::ZERO),
            Some(BindingHaptic {
                on: Some(pulse(0.2)),
                off: None,
                delay: Duration::ZERO
            })
        );
    }

    use crate::input::InteractionProfile;
    use crate::input::custom_bindings::BoolBindingType;
    use crate::input::profiles::oculus_touch::OculusTouch;
    use crate::input::tests::Fixture;
    use fakexr::{ActionState, HapticFeedback, UserPath, UserPath::*};
    use openvr::{self as vr, IVRInput010_Interface};

    fn touch_fixture(actions: &std::ffi::CStr) -> Fixture {
        let mut f = Fixture::new();
        f.load_actions(actions);
        f.set_interaction_profile::<OculusTouch>(LeftHand);
        f.set_interaction_profile::<OculusTouch>(RightHand);
        f
    }

    /// The action binding haptics play on.
    fn binding_haptic_action(f: &Fixture) -> xr::sys::Action {
        let data = f.input.openxr.session_data.get();
        data.input_data
            .get_loaded_actions()
            .unwrap()
            .haptic_action
            .as_raw()
    }

    /// The OpenXR action a threshold binding of `action` reads.
    fn threshold_input(f: &Fixture, action: &std::ffi::CStr) -> xr::sys::Action {
        let handle = f.get_action_handle(action);
        f.custom_binding_action(handle, OculusTouch::profile_path(), |ty| {
            matches!(ty, BoolBindingType::ThresholdFloat(_))
        })
    }

    fn pulse(hand: UserPath, duration_ms: i64, frequency: f32, amplitude: f32) -> HapticFeedback {
        HapticFeedback {
            hand: Some(hand),
            duration: xr::Duration::from_nanos(duration_ms * 1_000_000),
            frequency,
            amplitude,
        }
    }

    #[test]
    fn binding_haptics_on_analog_clicks() {
        let mut f = touch_fixture(c"actions_haptics.json");
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let sync = |f: &mut Fixture| {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            })
        };
        sync(&mut f);
        sync(&mut f);
        let haptic = binding_haptic_action(&f);
        assert_eq!(fakexr::take_haptic_feedback(haptic), []);

        let step = |f: &mut Fixture, action, state, hand, expected: &[HapticFeedback]| {
            fakexr::set_action_state(action, state, hand);
            sync(f);
            assert_eq!(fakexr::take_haptic_feedback(haptic), expected);
        };

        // Button mode on an analog input: SteamVR's default pulse on the press and the release.
        let trigger = threshold_input(&f, c"/actions/set1/in/press");
        let default = pulse(LeftHand, 10, 20.0, 0.2);
        step(
            &mut f,
            trigger,
            ActionState::Float(1.0),
            LeftHand,
            &[default],
        );
        step(&mut f, trigger, ActionState::Float(0.9), LeftHand, &[]);
        step(
            &mut f,
            trigger,
            ActionState::Float(0.0),
            LeftHand,
            &[default],
        );

        // The binding's own parameters, with the release turned off.
        let grip = threshold_input(&f, c"/actions/set1/in/offonly");
        step(&mut f, grip, ActionState::Float(0.6), LeftHand, &[]);
        let press = pulse(LeftHand, 50, 100.0, 0.5);
        step(&mut f, grip, ActionState::Float(0.8), LeftHand, &[press]);
        step(&mut f, grip, ActionState::Float(0.0), LeftHand, &[]);

        // Physical buttons and an amplitude of 0 have none.
        let x = f.get_action::<bool>(f.get_action_handle(c"/actions/set1/in/physical"));
        step(&mut f, x, ActionState::Bool(true), LeftHand, &[]);
        let silent = threshold_input(&f, c"/actions/set1/in/silent");
        step(&mut f, silent, ActionState::Float(1.0), RightHand, &[]);
        step(&mut f, silent, ActionState::Float(0.0), RightHand, &[]);

        // Trigger mode's click has button mode's, here read off the trigger by the runtime.
        let trig = f.get_action::<bool>(
            f.get_action_handle(c"/user/hand/right/input/trigger/value-/actions/set1-bool"),
        );
        let default = pulse(RightHand, 10, 20.0, 0.2);
        step(&mut f, trig, ActionState::Bool(true), RightHand, &[default]);
        step(
            &mut f,
            trig,
            ActionState::Bool(false),
            RightHand,
            &[default],
        );

        // Toggle buttons follow the analog input, not the toggled state.
        let toggle = f.get_action_handle(c"/actions/set1/in/toggle");
        let toggle = f.custom_binding_action(toggle, OculusTouch::profile_path(), |ty| {
            matches!(ty, BoolBindingType::Toggle(_))
        });
        let default = pulse(RightHand, 10, 20.0, 0.2);
        step(
            &mut f,
            toggle,
            ActionState::Bool(true),
            RightHand,
            &[default],
        );
        step(
            &mut f,
            toggle,
            ActionState::Bool(false),
            RightHand,
            &[default],
        );
    }

    #[test]
    fn binding_haptics_follow_active_sets() {
        let mut f = touch_fixture(c"actions_haptics_sets.json");
        let game = f.get_action_set_handle(c"/actions/game");
        let menu = f.get_action_set_handle(c"/actions/menu");
        let set = |set, priority| vr::VRActiveActionSet_t {
            ulActionSet: set,
            nPriority: priority,
            ..Default::default()
        };
        f.sync(set(game, 0));
        f.sync(set(game, 0));
        let haptic = binding_haptic_action(&f);

        // Grabs only vibrate when the binding asks.
        let grab = f.get_action_handle(c"/actions/game/in/grab");
        let grab = f.custom_binding_action(grab, OculusTouch::profile_path(), |ty| {
            matches!(ty, BoolBindingType::Grab(_))
        });
        fakexr::set_action_state(grab, ActionState::Float(1.0), LeftHand);
        f.sync(set(game, 0));
        let grabbed = pulse(LeftHand, 10, 20.0, 0.3);
        assert_eq!(fakexr::take_haptic_feedback(haptic), [grabbed]);
        fakexr::set_action_state(grab, ActionState::Float(0.0), LeftHand);
        f.sync(set(game, 0));
        assert_eq!(fakexr::take_haptic_feedback(haptic), [grabbed]);

        // A set activated with its input held doesn't count it as a press.
        let drag = threshold_input(&f, c"/actions/menu/in/drag");
        fakexr::set_action_state(drag, ActionState::Float(1.0), RightHand);
        f.sync(set(game, 0));
        f.sync_sets(&mut [set(game, 0), set(menu, 0)]);
        assert_eq!(fakexr::take_haptic_feedback(haptic), []);
        fakexr::set_action_state(drag, ActionState::Float(0.0), RightHand);
        f.sync_sets(&mut [set(game, 0), set(menu, 0)]);
        let released = pulse(RightHand, 10, 20.0, 0.2);
        assert_eq!(fakexr::take_haptic_feedback(haptic), [released]);

        // A higher priority set's binding on the same input replaces the game's, haptic included.
        let fire = threshold_input(&f, c"/actions/game/in/fire");
        let select = threshold_input(&f, c"/actions/menu/in/select");
        for (menu_priority, expected) in [(1, vec![]), (0, vec![pulse(LeftHand, 10, 20.0, 0.2)])] {
            f.sync_sets(&mut [set(game, 0), set(menu, menu_priority)]);
            for pressed in [1.0, 0.0] {
                fakexr::set_action_state(fire, ActionState::Float(pressed), LeftHand);
                fakexr::set_action_state(select, ActionState::Float(pressed), LeftHand);
                f.sync_sets(&mut [set(game, 0), set(menu, menu_priority)]);
                assert_eq!(
                    fakexr::take_haptic_feedback(haptic),
                    expected,
                    "menu priority {menu_priority}, pressed {pressed}"
                );
            }
        }

        // haptic_delay holds the pulse back.
        let delayed = threshold_input(&f, c"/actions/game/in/delayed");
        fakexr::set_action_state(delayed, ActionState::Float(1.0), RightHand);
        f.sync(set(game, 0));
        assert_eq!(fakexr::take_haptic_feedback(haptic), []);
        std::thread::sleep(Duration::from_millis(60));
        f.sync(set(game, 0));
        assert_eq!(
            fakexr::take_haptic_feedback(haptic),
            [pulse(RightHand, 10, 20.0, 0.2)]
        );
    }

    #[test]
    fn vibration_action() {
        let mut f = touch_fixture(c"actions_haptics.json");
        let set1 = f.get_action_set_handle(c"/actions/set1");
        let sync = |f: &mut Fixture| {
            f.sync(vr::VRActiveActionSet_t {
                ulActionSet: set1,
                ..Default::default()
            })
        };
        sync(&mut f);
        let handle = f.get_action_handle(c"/actions/set1/out/haptic");
        let action = f.get_action::<xr::Haptic>(handle);
        let left = f.get_input_source_handle(c"/user/hand/left");
        let vibrate = |f: &Fixture, start, duration, frequency, amplitude| {
            f.input
                .TriggerHapticVibrationAction(handle, start, duration, frequency, amplitude, left)
        };

        // A single pulse at the runtime's frequency.
        assert_eq!(vibrate(&f, 0.0, 0.0, 0.0, 0.5), vr::EVRInputError::None);
        assert_eq!(
            fakexr::take_haptic_feedback(action),
            [pulse(LeftHand, 10, xr::FREQUENCY_UNSPECIFIED, 0.5)]
        );
        assert_eq!(
            vibrate(&f, 0.0, 0.1, 160.0, f32::NAN),
            vr::EVRInputError::None
        );
        assert_eq!(fakexr::take_haptic_feedback(action), []);

        // Started later by UpdateActionState or the frame, without blocking the call.
        assert_eq!(vibrate(&f, 0.05, 0.1, 160.0, 0.5), vr::EVRInputError::None);
        sync(&mut f);
        assert_eq!(fakexr::take_haptic_feedback(action), []);
        std::thread::sleep(Duration::from_millis(60));
        sync(&mut f);
        let later = pulse(LeftHand, 100, 160.0, 0.5);
        assert_eq!(fakexr::take_haptic_feedback(action), [later]);

        assert_eq!(vibrate(&f, 0.02, 0.1, 160.0, 0.5), vr::EVRInputError::None);
        std::thread::sleep(Duration::from_millis(30));
        f.input.frame_start_update();
        assert_eq!(fakexr::take_haptic_feedback(action), [later]);

        assert_eq!(
            vibrate(&f, f32::INFINITY, 0.1, 160.0, 0.5),
            vr::EVRInputError::None
        );
        sync(&mut f);
        assert_eq!(fakexr::take_haptic_feedback(action), []);
    }
}
