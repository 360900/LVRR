//! Early interaction profile discovery.
//!
//! Some games (Fallout 4 VR, Skyrim VR, Serious Sam 3 VR) read the HMD's identity properties
//! once, right after init, to decide on a control scheme - and never again. Our HMD identity is
//! derived from the controller interaction profile, but runtimes only resolve that on
//! xrSyncActions in a focused session, which normally isn't until the game submits its first
//! frame. So before the real session is created, spin up a throwaway session, attach a dummy
//! action set bound to every supported profile, submit empty frames until the session is
//! focused, sync once and remember what got bound.

use super::profiles::{self, InteractionProfile, RunWithProfile};
use crate::openxr_data::{Hand, SessionData};
use crate::quirks::env_override;
use log::{info, warn};
use openvr as vr;
use openxr as xr;
use std::time::{Duration, Instant};

/// The interaction profiles bound to each hand at init.
#[derive(Default)]
pub struct ProbedProfiles {
    pub left: Option<String>,
    pub right: Option<String>,
    /// The suggested bindings live on the instance and reference this action. Keep it alive for
    /// as long as the instance rather than finding out what runtimes do with bindings to a
    /// destroyed action.
    _probe_set: Option<(xr::ActionSet, xr::Action<xr::Posef>)>,
}

impl ProbedProfiles {
    pub fn get(&self, hand: Hand) -> Option<&str> {
        match hand {
            Hand::Left => self.left.as_deref(),
            Hand::Right => self.right.as_deref(),
        }
    }
}

const FOCUS_TIMEOUT: Duration = Duration::from_secs(1);
const EXIT_TIMEOUT: Duration = Duration::from_secs(1);

pub fn probe_interaction_profiles(
    instance: &xr::Instance,
    system_id: xr::SystemId,
    enabled_extensions: &xr::ExtensionSet,
    app_type: vr::EVRApplicationType,
) -> ProbedProfiles {
    let mut probed = ProbedProfiles::default();
    if !should_probe(app_type, env_override("XRIZER_PROFILE_PROBE")) {
        info!("Skipping interaction profile probe ({app_type:?})");
        return probed;
    }
    if let Err(e) = probe(instance, system_id, enabled_extensions, &mut probed) {
        warn!("Failed to probe interaction profiles: {e}");
    }
    probed
}

/// Background and utility apps never render, so they don't need the probe - and a probe session
/// there could steal focus from the app that does.
fn should_probe(app_type: vr::EVRApplicationType, env: Option<bool>) -> bool {
    env != Some(false)
        && !matches!(
            app_type,
            vr::EVRApplicationType::Background | vr::EVRApplicationType::Utility
        )
}

fn probe(
    instance: &xr::Instance,
    system_id: xr::SystemId,
    enabled_extensions: &xr::ExtensionSet,
    probed: &mut ProbedProfiles,
) -> xr::Result<()> {
    let left = instance.string_to_path("/user/hand/left")?;
    let right = instance.string_to_path("/user/hand/right")?;

    // Every interaction profile supports the grip pose, so a single pose action is enough to get
    // any of them bound.
    let set = instance.create_action_set("xrizer_probe", "XRizer profile probe", 0)?;
    let action = set.create_action::<xr::Posef>("grip", "Grip", &[left, right])?;
    probed._probe_set = Some((set.clone(), action.clone()));

    struct Suggester<'a> {
        instance: &'a xr::Instance,
        enabled_extensions: &'a xr::ExtensionSet,
        action: &'a xr::Action<xr::Posef>,
        /// Ok once any profile was accepted, otherwise the last error.
        result: xr::Result<()>,
    }
    impl RunWithProfile for Suggester<'_> {
        fn run<P: InteractionProfile>(&mut self) {
            if !P::has_required_extensions(self.enabled_extensions) {
                return;
            }
            // A runtime rejecting one profile shouldn't keep the others from being probed.
            let result = (|| {
                let profile = self.instance.string_to_path(P::profile_path())?;
                let bindings = [
                    self.instance
                        .string_to_path("/user/hand/left/input/grip/pose")?,
                    self.instance
                        .string_to_path("/user/hand/right/input/grip/pose")?,
                ]
                .map(|path| xr::Binding::new(self.action, path));
                self.instance
                    .suggest_interaction_profile_bindings(profile, &bindings)
            })();
            match result {
                Ok(()) => self.result = Ok(()),
                Err(e) => {
                    warn!("Skipping {} in profile probe: {e}", P::profile_path());
                    if self.result.is_err() {
                        self.result = Err(e);
                    }
                }
            }
        }
    }
    let mut suggester = Suggester {
        instance,
        enabled_extensions,
        action: &action,
        result: Err(xr::sys::Result::ERROR_PATH_UNSUPPORTED),
    };
    profiles::run_for_all_profiles(&mut suggester);
    suggester.result?;

    let (session_data, mut waiter, stream) = SessionData::new(
        instance,
        system_id,
        vr::ETrackingUniverseOrigin::Standing,
        None,
        false,
    )
    .map_err(|e| {
        warn!("Failed to create probe session: {e:?}");
        xr::sys::Result::ERROR_RUNTIME_FAILURE
    })?;
    // Sessions without create info are always created with Vulkan.
    let mut stream: xr::FrameStream<xr::Vulkan> =
        stream.try_into().unwrap_or_else(|_| unreachable!());
    let session = &session_data.session;

    let mut state = xr::SessionState::READY;
    let mut events = EventPoller::new(instance);
    let result = (|| {
        session.attach_action_sets(&[&set])?;

        // Profiles are only resolved on xrSyncActions in a focused session, and the runtime won't
        // focus us until we submit frames.
        let start = Instant::now();
        while state != xr::SessionState::FOCUSED && start.elapsed() < FOCUS_TIMEOUT {
            let frame = waiter.wait()?;
            stream.begin()?;
            stream.end(
                frame.predicted_display_time,
                xr::EnvironmentBlendMode::OPAQUE,
                &[],
            )?;
            state = events.poll(state);
        }

        if state != xr::SessionState::FOCUSED {
            warn!("Probe session never became focused, interaction profiles unknown until later");
            return Ok(());
        }
        session.sync_actions(&[xr::ActiveActionSet::new(&set)])?;
        state = events.poll(state);
        let profile_for = |subaction| {
            session
                .current_interaction_profile(subaction)
                .ok()
                .filter(|p| *p != xr::Path::NULL)
                .and_then(|p| instance.path_to_string(p).ok())
        };
        probed.left = profile_for(left);
        probed.right = profile_for(right);
        info!(
            "Probed interaction profiles: left: {}, right: {}",
            probed.left.as_deref().unwrap_or("<null>"),
            probed.right.as_deref().unwrap_or("<null>")
        );
        Ok(())
    })();

    // Tear the session down cleanly before the real one gets created. Best effort: a failure here
    // shouldn't throw away profiles we already probed.
    if let Err(e) = session.request_exit() {
        warn!("Failed to request probe session exit: {e}");
    }
    state = events.wait_for(state, xr::SessionState::STOPPING, EXIT_TIMEOUT);
    if let Err(e) = session.end() {
        warn!("Failed to end probe session: {e}");
    }
    events.wait_for(state, xr::SessionState::EXITING, EXIT_TIMEOUT);
    // The frame stream and waiter hold a reference to the session, which has to be gone before
    // the temporary graphics setup inside SessionData is destroyed.
    drop(stream);
    drop(waiter);
    drop(session_data);

    result
}

struct EventPoller<'a> {
    instance: &'a xr::Instance,
    buf: xr::EventDataBuffer,
}

impl<'a> EventPoller<'a> {
    fn new(instance: &'a xr::Instance) -> Self {
        Self {
            instance,
            buf: xr::EventDataBuffer::new(),
        }
    }

    /// Drains pending events, returning the latest session state.
    fn poll(&mut self, mut state: xr::SessionState) -> xr::SessionState {
        while let Ok(Some(event)) = self.instance.poll_event(&mut self.buf) {
            if let xr::Event::SessionStateChanged(event) = event {
                state = event.state();
            }
        }
        state
    }

    fn wait_for(
        &mut self,
        mut state: xr::SessionState,
        target: xr::SessionState,
        timeout: Duration,
    ) -> xr::SessionState {
        let start = Instant::now();
        while state != target && start.elapsed() < timeout {
            state = self.poll(state);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clientcore::Injector;
    use crate::openxr_data::RealOpenXrData;
    use vr::EVRApplicationType::*;

    fn probe_with_profile(profile: &str) -> Option<String> {
        fakexr::set_default_interaction_profile(fakexr::UserPath::LeftHand, Some(profile));
        let xr = RealOpenXrData::new(&Injector::default(), Other).unwrap();
        fakexr::set_default_interaction_profile(fakexr::UserPath::LeftHand, None);
        xr.probed_profiles.left.clone()
    }

    #[test]
    fn rejected_profile_doesnt_stop_probe() {
        // The first profile we suggest.
        fakexr::reject_profile("/interaction_profiles/htc/vive_controller");
        let touch = "/interaction_profiles/oculus/touch_controller";
        assert_eq!(probe_with_profile(touch).as_deref(), Some(touch));
    }

    #[test]
    fn probe_hand_interaction() {
        fakexr::advertise_extension(xr::sys::EXT_HAND_INTERACTION_EXTENSION_NAME);
        let hand = "/interaction_profiles/ext/hand_interaction_ext";
        assert_eq!(probe_with_profile(hand).as_deref(), Some(hand));
    }

    #[test]
    fn should_probe_skips_non_rendering_apps() {
        assert!(should_probe(Scene, None));
        assert!(should_probe(Overlay, None));
        assert!(should_probe(Other, Some(true)));
        assert!(!should_probe(Background, None));
        assert!(!should_probe(Utility, None));
        assert!(!should_probe(Background, Some(true)));
        assert!(!should_probe(Scene, Some(false)));
    }
}
