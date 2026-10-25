//! Vibrations: OpenVR's parameters turned into OpenXR's, and vibrations that start later.

use openxr as xr;
use std::sync::Mutex;
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
    /// The longest pulse a Vive wand plays, which legacy games scale their strength to (e.g.
    /// `Mathf.Lerp(0, 3999, strength)`).
    const FULL_LEGACY_PULSE_US: f32 = 3999.0;

    /// A legacy `TriggerHapticPulse`. Games call it every frame with a pulse of up to 3999 us,
    /// whose length is the strength on a Vive wand. Amplitudes are what scale on other
    /// controllers, so the length becomes the amplitude.
    pub fn legacy_pulse(duration_us: u16) -> Option<Self> {
        (duration_us > 0).then(|| Self {
            duration: f32::from(duration_us) / 1_000_000.0,
            frequency: xr::FREQUENCY_UNSPECIFIED,
            amplitude: (f32::from(duration_us) / Self::FULL_LEGACY_PULSE_US).min(1.0),
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
    fn legacy_pulse_strength() {
        assert_eq!(Vibration::legacy_pulse(0), None);
        let values = |us| xr_values(Vibration::legacy_pulse(us).unwrap()).unwrap();
        assert_eq!(values(3999), (MIN_NANOS, xr::FREQUENCY_UNSPECIFIED, 1.0));
        let (_, _, quarter) = values(1000);
        assert!((quarter - 0.25).abs() < 0.001, "{quarter}");
        // Past what a wand plays, the length is kept at full strength.
        assert_eq!(values(20000), (20_000_000, xr::FREQUENCY_UNSPECIFIED, 1.0));
    }

    use crate::input::profiles::oculus_touch::OculusTouch;
    use crate::input::tests::Fixture;
    use fakexr::{HapticFeedback, UserPath, UserPath::*};
    use openvr::{self as vr, IVRInput010_Interface};

    fn touch_fixture(actions: &std::ffi::CStr) -> Fixture {
        let mut f = Fixture::new();
        f.load_actions(actions);
        f.set_interaction_profile::<OculusTouch>(LeftHand);
        f.set_interaction_profile::<OculusTouch>(RightHand);
        f
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
