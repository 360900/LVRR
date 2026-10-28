use super::{
    ActionData, ActionKey, ActionSetKey, BoundPoseType, Input, PoseData, TrackedDevice,
    TrackedDeviceList, WriteOnDrop,
};
use crate::{
    input::{
        LoadedActions, ManifestLoadedActions,
        action_manifest::legacy_actions::{self, LegacyOutput},
        devices::ProfileData,
        haptics::{self, Vibration},
        profiles::{self, RunWithProfile},
    },
    openxr_data::{self, Hand, SessionData},
};
use log::{debug, trace, warn};
use openvr as vr;
use openxr as xr;
use slotmap::Key;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Default)]
pub(super) struct LegacyState {
    packet_num: AtomicU32,
    /// Controller state per hand as of the last legacy action sync, including its packet number
    /// so both are published together.
    hands: [Mutex<HandState>; 2],
    /// The headset only reports k_EButton_ProximitySensor, as in SteamVR's generic HMD legacy
    /// bindings. None before the first sync, which sends no events since the headset was already
    /// on or off.
    hmd: Mutex<Option<vr::VRControllerState_t>>,
}

#[derive(Default)]
struct HandState {
    state: vr::VRControllerState_t,
    /// Whether the trigger value passed the touch threshold.
    trigger_touched: bool,
    /// Whether `main_xy_deflect` passed the press threshold.
    stick_deflected: bool,
    /// Whether the trigger value passed the profile's legacy press threshold.
    trigger_pressed: bool,
    /// Whether the squeeze value passed the profile's legacy grip press threshold.
    grip_pressed: bool,
}

impl LegacyState {
    pub fn on_action_sync(&self) {
        self.packet_num.fetch_add(1, Ordering::Relaxed);
    }
}

/// Button events are generated whether or not the app polls them, so an app that never calls
/// PollNextEvent must not grow the queue forever.
const MAX_QUEUED_EVENTS: usize = 256;

// Adapted from openvr.h
pub const fn button_mask_from_id(id: vr::EVRButtonId) -> u64 {
    1_u64 << id.0
}
#[macro_export]
macro_rules! button_mask_from_ids {
    ($($x:expr), * $(,)?) => {
        0u64 $(| button_mask_from_id($x))*
    };
}

impl<C: openxr_data::Compositor> Input<C> {
    pub fn setup_legacy_actions(&self) {
        debug!("setting up legacy actions");

        let session_data = self.openxr.session_data.get();
        let session = &session_data.session;
        let legacy = LegacyActionData::new(
            &self.openxr.instance,
            self.subaction_paths.left,
            self.subaction_paths.right,
        );
        let input_data = &session_data.input_data;

        // Bindings chosen for the app go through SteamVR's legacy action manifest, like there.
        let (chosen, choices) = self.chosen_legacy_bindings();
        if !chosen.is_empty() {
            match self.load_remapped_legacy(&session_data, legacy, chosen, choices) {
                Ok(loaded) => input_data
                    .actions
                    .set(LoadedActions::Manifest(Box::new(loaded)))
                    .unwrap_or_else(|_| panic!("Actions unexpectedly set up")),
                Err(e) => warn!("Couldn't load the legacy bindings: {e:?}"),
            }
            return;
        }

        self.suggest_fixed_legacy_bindings(&legacy, input_data, &HashSet::new());

        let pose_set = &input_data.pose_data.set;

        let attached = session.attach_action_sets(&[&legacy.set, pose_set]);
        if self
            .openxr
            .check("xrAttachSessionActionSets", attached)
            .is_none()
        {
            return;
        }
        let synced = session.sync_actions(&[
            xr::ActiveActionSet::new(&legacy.set),
            xr::ActiveActionSet::new(pose_set),
        ]);
        self.openxr.check("xrSyncActions", synced);

        input_data
            .actions
            .set(LoadedActions::Legacy(legacy, choices))
            .unwrap_or_else(|_| panic!("Actions unexpectedly set up"));
    }

    /// Suggests the fixed legacy bindings for every profile but those in `skip`.
    pub(super) fn suggest_fixed_legacy_bindings(
        &self,
        legacy: &LegacyActionData,
        input_data: &super::InputSessionData,
        skip: &HashSet<xr::Path>,
    ) {
        profiles::run_for_all_profiles(&mut Runner {
            openxr: &self.openxr,
            input_data,
            legacy,
            skip,
        });

        struct Runner<'a, C: openxr_data::Compositor> {
            openxr: &'a super::OpenXrData<C>,
            input_data: &'a super::InputSessionData,
            legacy: &'a LegacyActionData,
            skip: &'a HashSet<xr::Path>,
        }

        impl<C: openxr_data::Compositor> RunWithProfile for Runner<'_, C> {
            fn run<P: super::InteractionProfile>(&mut self) {
                if !P::has_required_extensions(&self.openxr.enabled_extensions) {
                    return;
                }

                let instance = &self.openxr.instance;
                let Ok(profile) = instance.string_to_path(P::profile_path()) else {
                    return;
                };
                if self.skip.contains(&profile) {
                    return;
                }
                let conv = super::profiles::InputToXrPath::new(instance);
                let bindings = P::legacy_bindings(&conv)
                    .into_iter(&self.legacy.actions, &self.input_data.pose_data)
                    .collect::<Vec<_>>();
                // A runtime rejecting one profile shouldn't take down the others.
                if let Err(e) = instance.suggest_interaction_profile_bindings(profile, &bindings) {
                    warn!(
                        "Couldn't suggest legacy bindings for {}, skipping it: {e}",
                        P::profile_path()
                    );
                }
            }
        }
    }

    /// Every axis pulses the controller's one haptic: SteamVR's legacy input has a single haptic
    /// output per hand (resources/config/legacy_actions.json), and the vibrations it sends drivers
    /// name a haptic component rather than an axis.
    pub fn legacy_haptic(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        axis_id: u32,
        duration_us: std::ffi::c_ushort,
    ) {
        let Some(hand) = self.device_index_to_hand(device_index) else {
            debug!("tried triggering haptic on invalid device index: {device_index}");
            return;
        };
        let hand_path = self.get_subaction_path(hand);
        let Some(vibration) = Vibration::legacy_pulse(duration_us) else {
            return;
        };
        trace!("legacy haptic on axis {axis_id} for {duration_us} microseconds: {vibration:?}");

        let data = self.openxr.session_data.get();
        if let Some(manifest_actions) = data.input_data.get_loaded_actions() {
            match &manifest_actions.legacy {
                Some(remap) => {
                    self.remapped_legacy_haptic(&data, manifest_actions, remap, hand, vibration)
                }
                // Game provided action manifest but also calls the legacy action's pulse method.
                None => self.legacy_haptic_via_manifest(
                    &data.session,
                    manifest_actions,
                    hand_path,
                    vibration,
                ),
            }
            return;
        }

        let Some(legacy) = data.input_data.get_legacy_actions() else {
            debug!("tried triggering haptic, but legacy actions aren't ready");
            return;
        };

        if let Err(e) = haptics::apply(&data.session, &legacy.actions.haptic, hand_path, vibration)
        {
            warn!("Failed to trigger haptic: {e:?}");
        }
    }

    /// Plays a legacy pulse on the hand's haptic outputs of the bindings chosen for its
    /// controller, or on the fixed legacy haptic.
    fn remapped_legacy_haptic(
        &self,
        data: &SessionData,
        loaded: &ManifestLoadedActions,
        remap: &RemappedLegacy,
        hand: Hand,
        vibration: Vibration,
    ) {
        let hand_path = self.get_subaction_path(hand);
        let actions: Vec<&xr::Action<xr::Haptic>> = if remap.is_remapped(data, hand) {
            remap
                .keys(hand, |output| matches!(output, LegacyOutput::Haptic))
                .filter_map(|key| match loaded.actions.get(key) {
                    Some(ActionData::Haptic(action)) => Some(action),
                    _ => None,
                })
                .collect()
        } else {
            vec![&remap.fixed.actions.haptic]
        };
        for action in actions {
            if let Err(e) = haptics::apply(&data.session, action, hand_path, vibration) {
                warn!("Failed to trigger haptic: {e:?}");
            }
        }
    }

    /// Trigger a vibration on the given path via the global `haptic_action` in our
    /// loaded Manifest Actions.
    ///
    /// This is necessary for the legacy input system to handle because applications may call
    /// legacy-input haptic interface functions while providing manifest files.
    fn legacy_haptic_via_manifest(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        manifest_actions: &ManifestLoadedActions,
        hand_path: xr::Path,
        vibration: Vibration,
    ) {
        trace!("triggered legacy haptic while using action manifest");
        if let Err(e) = haptics::apply(
            session,
            &manifest_actions.haptic_action,
            hand_path,
            vibration,
        ) {
            warn!("Failed to trigger haptic: {e:?}");
        }
    }

    pub fn get_legacy_controller_state(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        state_size: u32,
    ) -> bool {
        if state_size as usize != std::mem::size_of::<vr::VRControllerState_t>() {
            warn!(
                "Got an unexpected size for VRControllerState_t (expected {}, got {state_size})",
                std::mem::size_of::<vr::VRControllerState_t>()
            );
            return false;
        }

        if state.is_null() {
            return false;
        }

        let mut state = WriteOnDrop::new(state);
        let state = &mut state.value;

        if self.openxr.is_lost() {
            return false;
        }
        let data = self.openxr.session_data.get();
        if data
            .input_data
            .get_loaded_actions()
            .is_some_and(|loaded| loaded.legacy.is_none())
        {
            debug!("not returning legacy controller state due to loaded actions");
            return false;
        }

        if !data.input_data.has_legacy_actions() {
            debug!("tried getting controller state, but legacy actions aren't ready");
            return false;
        }

        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            let Some(hmd) = *self.legacy_state.hmd.lock().unwrap() else {
                return false;
            };
            **state = hmd;
            return true;
        }

        let Some(hand) = self.device_index_to_hand(device_index) else {
            debug!(
                "tried getting controller state, but device index {device_index} is invalid or not a controller!"
            );
            return false;
        };

        **state = self.legacy_state.hands[hand as usize - 1]
            .lock()
            .unwrap()
            .state;

        true
    }

    /// Reads the fixed legacy actions after they were synced, see [Self::update_hand_states].
    pub(super) fn update_legacy_state(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        devices: &TrackedDeviceList,
        legacy: &LegacyActionData,
    ) {
        self.update_hand_states(devices, |hand, controller, previous| {
            read_hand_state(
                &legacy.actions,
                session,
                self.get_subaction_path(hand),
                controller.profile_data.as_ref(),
                previous,
            )
        });
    }

    /// Syncs the legacy actions of the bindings chosen for the app and updates the controller
    /// state from them, see [Self::update_hand_states]. Controllers without chosen bindings read
    /// the fixed legacy actions.
    pub(super) fn update_remapped_legacy_state(
        &self,
        data: &SessionData,
        loaded: &ManifestLoadedActions,
        remap: &RemappedLegacy,
    ) {
        let sets: Vec<_> = remap
            .sets
            .iter()
            .map(|set| vr::VRActiveActionSet_t {
                ulActionSet: set.data().as_ffi(),
                ulRestrictedToDevice: vr::k_ulInvalidInputValueHandle,
                ulSecondaryActionSet: vr::k_ulInvalidActionSetHandle,
                unPadding: 0,
                nPriority: 0,
            })
            .collect();
        if self.sync_action_sets(data, loaded, &sets) != vr::EVRInputError::None {
            return;
        }
        let devices = data.input_data.devices.read().unwrap();
        self.update_hand_states(&devices, |hand, controller, previous| {
            if remap.profiles.contains(&controller.profile_path) {
                remap.read_hand_state(self, hand)
            } else {
                read_hand_state(
                    &remap.fixed.actions,
                    &data.session,
                    self.get_subaction_path(hand),
                    controller.profile_data.as_ref(),
                    previous,
                )
            }
        });
    }

    /// Updates each controller's state with `read` and queues button events for every change, so
    /// apps get them whether or not they poll GetControllerState.
    fn update_hand_states(
        &self,
        devices: &TrackedDeviceList,
        read: impl Fn(Hand, &TrackedDevice, &HandState) -> HandState,
    ) {
        let packet_num = self.legacy_state.packet_num.load(Ordering::Relaxed);
        for hand in [Hand::Left, Hand::Right] {
            let (Some(device_index), Some(controller)) = (
                devices.get_controller_index(hand),
                devices.get_controller(hand),
            ) else {
                continue;
            };
            let mut hand_state = self.legacy_state.hands[hand as usize - 1].lock().unwrap();
            let mut new = read(hand, controller, &hand_state);
            new.state.unPacketNum = packet_num;
            let (old, new_state) = (&hand_state.state, &new.state);

            self.push_button_events(device_index, &BUTTONS, old, new_state);
            *hand_state = new;
        }

        let mut hmd = self.legacy_state.hmd.lock().unwrap();
        let mut new = vr::VRControllerState_t {
            unPacketNum: packet_num,
            ..Default::default()
        };
        if self.openxr.user_present() {
            new.ulButtonPressed = button_mask_from_id(vr::EVRButtonId::ProximitySensor);
        }
        if let Some(old) = &*hmd {
            self.push_button_events(
                vr::k_unTrackedDeviceIndex_Hmd,
                &[vr::EVRButtonId::ProximitySensor],
                old,
                &new,
            );
        }
        *hmd = Some(new);
    }

    fn push_button_events(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        buttons: &[vr::EVRButtonId],
        old: &vr::VRControllerState_t,
        new: &vr::VRControllerState_t,
    ) {
        let mut events = self.events.lock().unwrap();
        let mut push = |ty, id: vr::EVRButtonId| {
            events.push_back(super::InputEvent::new(
                ty,
                device_index,
                vr::VREvent_Controller_t { button: id.0 },
            ));
        };
        for &id in buttons {
            let mask = button_mask_from_id(id);
            let touched = new.ulButtonTouched & mask != 0;
            let pressed = new.ulButtonPressed & mask != 0;
            let touch_changed = touched != (old.ulButtonTouched & mask != 0);
            // Touch comes before the press, and the untouch after the release.
            if touch_changed && touched {
                push(vr::EVREventType::ButtonTouch, id);
            }
            if pressed != (old.ulButtonPressed & mask != 0) {
                push(
                    if pressed {
                        vr::EVREventType::ButtonPress
                    } else {
                        vr::EVREventType::ButtonUnpress
                    },
                    id,
                );
            }
            if touch_changed && !touched {
                push(vr::EVREventType::ButtonUntouch, id);
            }
        }
        let excess = events.len().saturating_sub(MAX_QUEUED_EVENTS);
        events.drain(..excess);
    }
}

/// The buttons legacy input can report, in the order their events are sent. The fixed legacy
/// actions only drive the first six.
const BUTTONS: [vr::EVRButtonId; 9] = [
    vr::EVRButtonId::Axis0,
    vr::EVRButtonId::SteamVR_Trigger,
    vr::EVRButtonId::ApplicationMenu,
    vr::EVRButtonId::A,
    vr::EVRButtonId::Grip,
    vr::EVRButtonId::Axis2,
    vr::EVRButtonId::Axis3,
    vr::EVRButtonId::Axis4,
    vr::EVRButtonId::System,
];

/// Applies a threshold with hysteresis, like SteamVR's activate/deactivate thresholds.
fn threshold(was_active: bool, value: f32, activate: f32, deactivate: f32) -> bool {
    value >= if was_active { deactivate } else { activate }
}

fn read_hand_state(
    actions: &LegacyActions,
    session: &xr::Session<xr::AnyGraphics>,
    hand_path: xr::Path,
    profile: Option<&ProfileData>,
    previous: &HandState,
) -> HandState {
    let bool_state = |action: &xr::Action<bool>| {
        action
            .state(session, hand_path)
            .is_ok_and(|s| s.current_state)
    };
    let float_state = |action: &xr::Action<f32>| {
        action
            .state(session, hand_path)
            .map_or(0.0, |s| s.current_state)
    };

    let trigger = float_state(&actions.trigger);
    // SteamVR's legacy bindings also touch the trigger once it is pulled, which is all
    // controllers without a capacitive trigger can report.
    let trigger_touched = threshold(previous.trigger_touched, trigger, 0.1, 0.05);
    let squeeze = float_state(&actions.squeeze);
    // Some of SteamVR's legacy bindings also press these from the analog values.
    let trigger_pressed = profile
        .and_then(|p| p.legacy_trigger_click)
        .is_some_and(|(on, off)| threshold(previous.trigger_pressed, trigger, on, off));
    let grip_pressed = profile
        .and_then(|p| p.legacy_grip_click)
        .is_some_and(|(on, off)| threshold(previous.grip_pressed, squeeze, on, off));
    let deflection = actions
        .main_xy_deflect
        .state(session, hand_path)
        .map_or(0.0, |s| {
            glam::Vec2::new(s.current_state.x, s.current_state.y).length()
        });
    let stick_deflected = threshold(previous.stick_deflected, deflection, 0.8, 0.7);

    let mut state = vr::VRControllerState_t::default();
    let mut set_button = |id, pressed: bool, touched: bool| {
        let mask = button_mask_from_id(id);
        if pressed {
            state.ulButtonPressed |= mask;
        }
        // A button can't be pressed without being touched.
        if touched || pressed {
            state.ulButtonTouched |= mask;
        }
    };

    set_button(
        vr::EVRButtonId::Axis0,
        bool_state(&actions.main_xy_click) || stick_deflected,
        bool_state(&actions.main_xy_touch),
    );
    set_button(
        vr::EVRButtonId::SteamVR_Trigger,
        bool_state(&actions.trigger_click) || trigger_pressed,
        bool_state(&actions.trigger_touch) || trigger_touched,
    );
    set_button(
        vr::EVRButtonId::ApplicationMenu,
        bool_state(&actions.app_menu),
        bool_state(&actions.app_menu_touch),
    );
    set_button(
        vr::EVRButtonId::A,
        bool_state(&actions.a),
        bool_state(&actions.a_touch),
    );
    let squeeze_click = bool_state(&actions.squeeze_click);
    set_button(
        vr::EVRButtonId::Grip,
        squeeze_click || bool_state(&actions.grip_click) || grip_pressed,
        bool_state(&actions.grip_touch),
    );
    set_button(
        vr::EVRButtonId::Axis2,
        squeeze_click,
        bool_state(&actions.axis2_touch),
    );

    let main_xy = actions
        .main_xy
        .state(session, hand_path)
        .map_or(Default::default(), |s| s.current_state);
    state.rAxis[0] = vr::VRControllerAxis_t {
        x: main_xy.x,
        y: main_xy.y,
    };
    state.rAxis[1].x = trigger;
    state.rAxis[2].x = squeeze;

    HandState {
        state,
        trigger_touched,
        stick_deflected,
        trigger_pressed,
        grip_pressed,
    }
}

mod marker {
    use openxr as xr;
    // Some type magic to parameterize our legacy actions to act as actions or bindings
    pub trait ActionsMarker {
        type T<U: xr::ActionTy>;
    }
    pub struct Actions;
    pub struct Bindings {
        // This pose is handled separately, in the PoseData struct,
        // so we don't use an action for it, but we still need the binding.
        pub grip_pose: Vec<xr::Path>,
    }
    impl ActionsMarker for Actions {
        type T<U: xr::ActionTy> = xr::Action<U>;
    }
    impl ActionsMarker for Bindings {
        type T<U: xr::ActionTy> = Vec<xr::Path>;
    }

    pub type Action<T, M> = <M as ActionsMarker>::T<T>;
}
pub(super) use marker::Bindings;
use marker::*;

////////////////////////
// Whenever a field is added to this struct, it also needs to be added to LegacyBindings::into_iter below
///////////////////////
#[allow(private_interfaces, private_bounds)]
pub(super) struct Legacy<M: ActionsMarker> {
    pub app_menu: Action<bool, M>,
    pub app_menu_touch: Action<bool, M>,
    pub a: Action<bool, M>,
    pub a_touch: Action<bool, M>,
    pub trigger_click: Action<bool, M>,
    pub trigger_touch: Action<bool, M>,
    /// Presses k_EButton_Grip and k_EButton_Axis2.
    pub squeeze_click: Action<bool, M>,
    /// Presses only k_EButton_Grip (the Index A button, as in SteamVR's legacy bindings).
    pub grip_click: Action<bool, M>,
    /// Touches k_EButton_Grip.
    pub grip_touch: Action<bool, M>,
    /// Touches k_EButton_Axis2 (the Focus 3 grip, as in SteamVR's legacy bindings).
    pub axis2_touch: Action<bool, M>,
    pub trigger: Action<f32, M>,
    pub squeeze: Action<f32, M>,
    // This can be a stick or a trackpad, so we'll just call it "xy"
    pub main_xy: Action<xr::Vector2f, M>,
    pub main_xy_touch: Action<bool, M>,
    pub main_xy_click: Action<bool, M>,
    /// A stick that also presses Axis0 when pushed far enough, like SteamVR's Index legacy
    /// binding does for apps written for the Vive trackpad.
    pub main_xy_deflect: Action<xr::Vector2f, M>,
    pub haptic: Action<xr::Haptic, M>,
    pub extra: M,
}

pub(super) type LegacyActions = Legacy<Actions>;
pub(super) type LegacyBindings = Legacy<Bindings>;

impl LegacyBindings {
    fn into_iter<'a>(
        self,
        actions: &'a LegacyActions,
        pose_data: &'a PoseData,
    ) -> impl Iterator<Item = xr::Binding<'a>> {
        macro_rules! bindings {
            ($begin:expr, $($field:ident),+$(,)?) => {
                $begin $(
                    .chain(
                        self.$field.into_iter().map(|path| xr::Binding::new(&actions.$field, path))
                    )
                )+
            }
        }

        // TODO: figure out how to automatically derive this...
        bindings![
            self.extra
                .grip_pose
                .into_iter()
                .map(|path| xr::Binding::new(&pose_data.grip, path)),
            app_menu,
            app_menu_touch,
            a,
            a_touch,
            trigger_click,
            trigger_touch,
            squeeze_click,
            grip_click,
            grip_touch,
            axis2_touch,
            trigger,
            squeeze,
            main_xy,
            main_xy_touch,
            main_xy_click,
            main_xy_deflect,
            haptic,
        ]
    }
}

pub(super) struct LegacyActionData {
    pub set: xr::ActionSet,
    actions: LegacyActions,
}

/// Legacy input through the legacy action manifest, for controllers with bindings chosen for the
/// app, see [Input::load_remapped_legacy].
pub(super) struct RemappedLegacy {
    /// The interaction profiles that got chosen bindings.
    profiles: HashSet<xr::Path>,
    /// What the other profiles are bound to.
    fixed: LegacyActionData,
    /// The legacy action sets.
    sets: Vec<ActionSetKey>,
    /// Per hand, what each action drives, with the keys of its own and its mirrored action.
    hands: [Vec<(LegacyOutput, Vec<ActionKey>)>; 2],
}

impl RemappedLegacy {
    pub fn new<C: openxr_data::Compositor>(
        input: &Input<C>,
        profiles: HashSet<xr::Path>,
        fixed: LegacyActionData,
    ) -> Self {
        let action_map = input.action_map.read().unwrap();
        let set_map = input.set_map.read().unwrap();
        let action_key = |path: &str| {
            action_map
                .iter()
                .find_map(|(key, action)| (action.path == path).then_some(key))
        };
        let hand = |hand| {
            legacy_actions::HAND_ACTIONS
                .iter()
                .map(|(name, _, output)| {
                    let keys = legacy_actions::action_paths(hand, name, *output)
                        .iter()
                        .filter_map(|path| action_key(path))
                        .collect();
                    (*output, keys)
                })
                .collect()
        };
        Self {
            profiles,
            fixed,
            sets: legacy_actions::SETS
                .iter()
                .filter_map(|path| {
                    set_map
                        .iter()
                        .find_map(|(key, set)| (set == path).then_some(key))
                })
                .collect(),
            hands: [hand(Hand::Left), hand(Hand::Right)],
        }
    }

    /// The set of the fixed legacy actions, synced with the legacy action sets.
    pub fn fixed_set(&self) -> &xr::ActionSet {
        &self.fixed.set
    }

    /// The keys of the actions of `hand` with outputs that `matches`.
    fn keys(
        &self,
        hand: Hand,
        matches: impl Fn(LegacyOutput) -> bool,
    ) -> impl Iterator<Item = ActionKey> {
        self.hands[hand as usize - 1]
            .iter()
            .filter(move |(output, _)| matches(*output))
            .flat_map(|(_, keys)| keys.iter().copied())
    }

    /// Whether the controller of `hand` has chosen bindings.
    fn is_remapped(&self, data: &SessionData, hand: Hand) -> bool {
        let devices = data.input_data.devices.read().unwrap();
        devices
            .get_controller(hand)
            .is_some_and(|controller| self.profiles.contains(&controller.profile_path))
    }

    /// The pose the chosen bindings give `controller`, if they give it another than its own.
    pub fn pose_type(
        &self,
        loaded: &ManifestLoadedActions,
        controller: &TrackedDevice,
    ) -> Option<BoundPoseType> {
        let super::TrackedDeviceType::Controller { hand, .. } = controller.get_type() else {
            return None;
        };
        if !self.profiles.contains(&controller.profile_path) {
            return None;
        }
        let poses = loaded
            .per_profile_pose_bindings
            .get(&controller.profile_path)?;
        self.keys(*hand, |output| matches!(output, LegacyOutput::Pose))
            .find_map(|key| {
                let pose = poses.get(key)?;
                match hand {
                    Hand::Left => pose.left,
                    Hand::Right => pose.right,
                }
            })
            .filter(|pose| !matches!(pose, BoundPoseType::Raw))
    }

    /// The controller state of `hand` from the legacy actions, after they were synced.
    fn read_hand_state<C: openxr_data::Compositor>(
        &self,
        input: &Input<C>,
        hand: Hand,
    ) -> HandState {
        use vr::IVRInput011_Interface as _;
        let restrict = match hand {
            Hand::Left => input.left_hand_key,
            Hand::Right => input.right_hand_key,
        }
        .data()
        .as_ffi();
        let mut state = vr::VRControllerState_t::default();
        let (mut pressed, mut touched) = (0, 0);
        for (output, keys) in &self.hands[hand as usize - 1] {
            for key in keys {
                let handle = key.data().as_ffi();
                match *output {
                    LegacyOutput::Press(id) | LegacyOutput::Touch(id) => {
                        let mut data = vr::InputDigitalActionData_t::default();
                        let read = input.GetDigitalActionData(
                            handle,
                            &mut data,
                            std::mem::size_of_val(&data) as u32,
                            restrict,
                        );
                        if read == vr::EVRInputError::None && data.bActive && data.bState {
                            match output {
                                LegacyOutput::Press(_) => pressed |= button_mask_from_id(id),
                                _ => touched |= button_mask_from_id(id),
                            }
                        }
                    }
                    LegacyOutput::Axis(axis)
                    | LegacyOutput::AxisX(axis)
                    | LegacyOutput::AxisY(axis) => {
                        let mut data = vr::InputAnalogActionData_t::default();
                        let read = input.GetAnalogActionData(
                            handle,
                            &mut data,
                            std::mem::size_of_val(&data) as u32,
                            restrict,
                        );
                        if read != vr::EVRInputError::None || !data.bActive {
                            continue;
                        }
                        match output {
                            LegacyOutput::Axis(_) => {
                                state.rAxis[axis] = vr::VRControllerAxis_t {
                                    x: data.x,
                                    y: data.y,
                                }
                            }
                            LegacyOutput::AxisX(_) => state.rAxis[axis].x = data.x,
                            _ => state.rAxis[axis].y = data.x,
                        }
                    }
                    LegacyOutput::Pose | LegacyOutput::Haptic => {}
                }
            }
        }
        state.ulButtonPressed = pressed;
        // A button can't be pressed without being touched.
        state.ulButtonTouched = touched | pressed;
        HandState {
            state,
            ..Default::default()
        }
    }
}

impl LegacyActionData {
    pub fn new(instance: &xr::Instance, left_hand: xr::Path, right_hand: xr::Path) -> Self {
        debug!("creating legacy actions");
        let leftright = [left_hand, right_hand];

        let set = instance
            .create_action_set("xrizer-legacy-set", "XRizer Legacy Set", 0)
            .unwrap();

        let actions = LegacyActions {
            trigger_click: set
                .create_action("trigger-click", "Trigger Click", &leftright)
                .unwrap(),
            trigger_touch: set
                .create_action("trigger-touch", "Trigger Touch", &leftright)
                .unwrap(),
            trigger: set.create_action("trigger", "Trigger", &leftright).unwrap(),
            squeeze: set.create_action("squeeze", "Squeeze", &leftright).unwrap(),
            app_menu: set
                .create_action("app-menu", "Application Menu", &leftright)
                .unwrap(),
            app_menu_touch: set
                .create_action("app-menu-touch", "Application Menu Touch", &leftright)
                .unwrap(),
            a: set.create_action("a", "A Button", &leftright).unwrap(),
            a_touch: set
                .create_action("a-touch", "A Button Touch", &leftright)
                .unwrap(),
            squeeze_click: set
                .create_action("grip-click", "Grip Click", &leftright)
                .unwrap(),
            grip_click: set
                .create_action("grip-only-click", "Grip Only Click", &leftright)
                .unwrap(),
            grip_touch: set
                .create_action("grip-touch", "Grip Touch", &leftright)
                .unwrap(),
            axis2_touch: set
                .create_action("axis2-touch", "Axis 2 Touch", &leftright)
                .unwrap(),
            main_xy: set
                .create_action("main-joystick", "Main Joystick/Trackpad", &leftright)
                .unwrap(),
            main_xy_click: set
                .create_action("main-joystick-click", "Main Joystick Click", &leftright)
                .unwrap(),
            main_xy_touch: set
                .create_action("main-joystick-touch", "Main Joystick Touch", &leftright)
                .unwrap(),
            main_xy_deflect: set
                .create_action(
                    "main-joystick-deflect",
                    "Main Joystick Deflection",
                    &leftright,
                )
                .unwrap(),
            haptic: set.create_action("haptic", "Haptic", &leftright).unwrap(),
            extra: Actions,
        };

        Self { set, actions }
    }
}

#[cfg(test)]
mod tests {
    use super::Vibration;
    use crate::input::InteractionProfile;
    use crate::input::profiles::{
        frame_controller::FrameController, knuckles::Knuckles, oculus_touch::OculusTouch,
        simple_controller::SimpleController, vive_focus3::ViveFocus3,
    };
    use crate::input::tests::{Fixture, compare_pose};
    use crate::openxr_data::Hand;
    use openvr as vr;
    use openxr as xr;
    use slotmap::KeyData;
    use std::ffi::CStr;

    #[repr(C)]
    #[derive(Default)]
    struct MyEvent {
        ty: u32,
        index: vr::TrackedDeviceIndex_t,
        age: f32,
        data: EventData,
    }

    // A small version of the VREvent_Data_t union - writing to this should not cause UB!
    #[repr(C)]
    union EventData {
        controller: vr::VREvent_Controller_t,
    }

    impl Default for EventData {
        fn default() -> Self {
            Self {
                controller: Default::default(),
            }
        }
    }

    const _: () = {
        use std::mem::offset_of;

        macro_rules! verify_offset {
            ($real:ident, $fake:ident) => {
                assert!(offset_of!(vr::VREvent_t, $real) == offset_of!(MyEvent, $fake));
            };
        }
        verify_offset!(eventType, ty);
        verify_offset!(trackedDeviceIndex, index);
        verify_offset!(eventAgeSeconds, age);
        verify_offset!(data, data);
    };

    #[test]
    fn no_legacy_input_before_session_setup() {
        let fixture = Fixture::new();

        let got_input = fixture.input.get_legacy_controller_state(
            1,
            &mut vr::VRControllerState_t::default(),
            std::mem::size_of::<vr::VRControllerState_t>() as _,
        );
        assert!(!got_input);

        fixture.input.frame_start_update();
        let got_input = fixture.input.get_legacy_controller_state(
            1,
            &mut vr::VRControllerState_t::default(),
            std::mem::size_of::<vr::VRControllerState_t>() as _,
        );
        assert!(!got_input);
    }

    fn next_event(f: &Fixture) -> Option<MyEvent> {
        let mut event = MyEvent::default();
        f.input
            .get_next_event(
                std::mem::size_of_val(&event) as u32,
                &mut event as *mut _ as *mut vr::VREvent_t,
            )
            .then_some(event)
    }

    fn legacy_action(
        f: &Fixture,
        get_action: impl FnOnce(&super::LegacyActions) -> openxr::sys::Action,
    ) -> openxr::sys::Action {
        get_action(
            &f.input
                .openxr
                .session_data
                .get()
                .input_data
                .get_legacy_actions()
                .unwrap()
                .actions,
        )
    }

    #[track_caller]
    fn assert_legacy_bindings<P: InteractionProfile>(
        f: &Fixture,
        get_action: impl FnOnce(&super::LegacyActions) -> openxr::sys::Action,
        expected: &[&str],
    ) {
        let profile = f
            .input
            .openxr
            .instance
            .string_to_path(P::profile_path())
            .unwrap();
        let action = legacy_action(f, get_action);
        if expected.is_empty() {
            assert!(fakexr::check_no_suggested_bindings(action, profile));
            return;
        }
        let mut bindings = fakexr::get_suggested_bindings(action, profile);
        bindings.sort();
        let mut expected: Vec<_> = expected.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(bindings, expected);
    }

    /// Sets up legacy input with Index controllers, returning the requested legacy action.
    fn legacy_fixture(
        get_action: impl FnOnce(&super::LegacyActions) -> openxr::sys::Action,
    ) -> (Fixture, openxr::sys::Action) {
        legacy_fixture_for::<Knuckles>(get_action)
    }

    fn legacy_fixture_for<P: InteractionProfile>(
        get_action: impl FnOnce(&super::LegacyActions) -> openxr::sys::Action,
    ) -> (Fixture, openxr::sys::Action) {
        use fakexr::UserPath::*;
        let mut f = Fixture::new();
        f.input.openxr.restart_session().unwrap();

        f.set_interaction_profile::<P>(LeftHand);
        f.set_interaction_profile::<P>(RightHand);
        f.input.frame_start_update();
        f.input.openxr.poll_events();
        let action = legacy_action(&f, get_action);
        while let Some(event) = next_event(&f) {
            assert_eq!(event.ty, vr::EVREventType::TrackedDeviceActivated.0);
        }
        (f, action)
    }

    fn get_state(f: &Fixture, device_index: vr::TrackedDeviceIndex_t) -> vr::VRControllerState_t {
        let mut state = vr::VRControllerState_t::default();
        assert!(f.input.get_legacy_controller_state(
            device_index,
            &mut state,
            std::mem::size_of_val(&state) as u32
        ));
        state
    }

    #[track_caller]
    fn expect_button_event(f: &Fixture, ty: vr::EVREventType, index: u32, id: vr::EVRButtonId) {
        let event = next_event(f).unwrap_or_else(|| panic!("Expected {ty:?} for {id:?}"));
        assert_eq!(event.ty, ty.0, "{id:?}");
        assert_eq!(event.index, index, "{id:?}");
        assert_eq!(unsafe { event.data.controller }.button, id.0);
    }

    #[track_caller]
    fn expect_no_event(f: &Fixture) {
        if let Some(event) = next_event(f) {
            panic!("Got unexpected event: {}", event.ty);
        }
    }

    /// Toggles a boolean legacy action and checks the reported buttons and events.
    /// A pressed button is touched as well.
    fn legacy_input(
        get_action: impl FnOnce(&super::LegacyActions) -> openxr::sys::Action,
        ids: &[vr::EVRButtonId],
        touch: bool,
    ) {
        use fakexr::UserPath::*;
        let (f, action) = legacy_fixture(get_action);

        let mask = ids
            .iter()
            .fold(0, |mask, id| mask | super::button_mask_from_id(*id));
        let expect_state = |index, active: bool| {
            let state = get_state(&f, index);
            // Copy the fields out, since references to packed fields are undefined.
            let (pressed, touched) = (state.ulButtonPressed, state.ulButtonTouched);
            let mask = if active { mask } else { 0 };
            assert_eq!(touched, mask, "touched {pressed:b} (device {index})");
            assert_eq!(
                pressed,
                mask * !touch as u64,
                "pressed {pressed:b} (device {index})"
            );
        };
        let expect_events = |index, active: bool| {
            use vr::EVREventType as Ev;
            let types: &[vr::EVREventType] = match (touch, active) {
                (true, true) => &[Ev::ButtonTouch],
                (true, false) => &[Ev::ButtonUntouch],
                (false, true) => &[Ev::ButtonTouch, Ev::ButtonPress],
                (false, false) => &[Ev::ButtonUnpress, Ev::ButtonUntouch],
            };
            for id in ids {
                for ty in types {
                    expect_button_event(&f, *ty, index, *id);
                }
            }
        };
        let update_action_state = |left_state, right_state| {
            fakexr::set_action_state(action, fakexr::ActionState::Bool(left_state), LeftHand);
            fakexr::set_action_state(action, fakexr::ActionState::Bool(right_state), RightHand);
            f.input.frame_start_update();
        };

        for index in [1, 2] {
            expect_state(index, false);
        }
        expect_no_event(&f);

        // State change to true
        update_action_state(true, true);
        for index in [1, 2] {
            expect_state(index, true);
            expect_events(index, true);
        }
        expect_no_event(&f);

        // No frame update - no change
        for index in [1, 2] {
            expect_state(index, true);
        }
        expect_no_event(&f);

        // Frame update but no change
        f.input.frame_start_update();
        for index in [1, 2] {
            expect_state(index, true);
        }
        expect_no_event(&f);

        // State change to false
        update_action_state(false, false);
        for index in [1, 2] {
            expect_state(index, false);
            expect_events(index, false);
        }
        expect_no_event(&f);

        // State change one hand
        update_action_state(true, false);
        expect_state(1, true);
        expect_state(2, false);
        expect_events(1, true);
        expect_no_event(&f);
    }

    macro_rules! test_button {
        ($click:ident, $id:path $(| $other_id:path)*) => {
            paste::paste! {
                #[test]
                fn [<button_ $click>]() {
                    legacy_input(|actions| actions.$click.as_raw(), &[$id $(, $other_id)*], false);
                }
            }
        };
        ($click:ident, $id:path $(| $other_id:path)*, $touch:ident) => {
            test_button!($click, $id $(| $other_id)*);
            paste::paste! {
                #[test]
                fn [<button_ $touch>]() {
                    legacy_input(|actions| actions.$touch.as_raw(), &[$id $(, $other_id)*], true);
                }
            }
        };
    }

    test_button!(main_xy_click, vr::EVRButtonId::Axis0, main_xy_touch);
    test_button!(
        trigger_click,
        vr::EVRButtonId::SteamVR_Trigger,
        trigger_touch
    );
    test_button!(app_menu, vr::EVRButtonId::ApplicationMenu, app_menu_touch);
    test_button!(
        squeeze_click,
        vr::EVRButtonId::Grip | vr::EVRButtonId::Axis2
    );
    test_button!(a, vr::EVRButtonId::A, a_touch);

    test_button!(grip_click, vr::EVRButtonId::Grip);

    #[test]
    fn button_grip_touch() {
        legacy_input(
            |actions| actions.grip_touch.as_raw(),
            &[vr::EVRButtonId::Grip],
            true,
        );
    }

    #[test]
    fn button_axis2_touch() {
        legacy_input(
            |actions| actions.axis2_touch.as_raw(),
            &[vr::EVRButtonId::Axis2],
            true,
        );
    }

    #[test]
    fn touch_bindings() {
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        assert_legacy_bindings::<OculusTouch>(
            &f,
            |actions| actions.trigger_touch.as_raw(),
            &[
                "/user/hand/left/input/trigger/touch",
                "/user/hand/right/input/trigger/touch",
            ],
        );
        assert_legacy_bindings::<OculusTouch>(
            &f,
            |actions| actions.a_touch.as_raw(),
            &[
                "/user/hand/left/input/x/touch",
                "/user/hand/right/input/a/touch",
            ],
        );
        assert_legacy_bindings::<OculusTouch>(
            &f,
            |actions| actions.app_menu_touch.as_raw(),
            &[
                "/user/hand/left/input/y/touch",
                "/user/hand/right/input/b/touch",
            ],
        );
        // Like SteamVR's, the Focus 3 grip touches Axis2 as well.
        for action in [
            |actions: &super::LegacyActions| actions.grip_touch.as_raw(),
            |actions: &super::LegacyActions| actions.axis2_touch.as_raw(),
        ] {
            assert_legacy_bindings::<ViveFocus3>(
                &f,
                action,
                &[
                    "/user/hand/left/input/squeeze/touch",
                    "/user/hand/right/input/squeeze/touch",
                ],
            );
        }
        // Others hide it.
        assert_legacy_bindings::<Knuckles>(&f, |actions| actions.axis2_touch.as_raw(), &[]);
        assert_legacy_bindings::<FrameController>(&f, |actions| actions.axis2_touch.as_raw(), &[]);
    }

    #[test]
    fn frame_bindings() {
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        assert_legacy_bindings::<FrameController>(
            &f,
            |actions| actions.app_menu.as_raw(),
            &[
                "/user/hand/left/input/dpad_left/click",
                "/user/hand/left/input/dpad_right/click",
                "/user/hand/left/input/dpad_up/click",
                "/user/hand/right/input/b/click",
                "/user/hand/right/input/x/click",
                "/user/hand/right/input/y/click",
            ],
        );
        assert_legacy_bindings::<FrameController>(
            &f,
            |actions| actions.a.as_raw(),
            &[
                "/user/hand/left/input/dpad_down/click",
                "/user/hand/right/input/a/click",
            ],
        );
        assert_legacy_bindings::<FrameController>(
            &f,
            |actions| actions.trigger_click.as_raw(),
            &[
                "/user/hand/left/input/trigger/click",
                "/user/hand/right/input/trigger/click",
                "/user/hand/left/input/bumper/click",
                "/user/hand/right/input/bumper/click",
            ],
        );
    }

    #[test]
    fn focus3_bindings() {
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        assert_legacy_bindings::<ViveFocus3>(
            &f,
            |actions| actions.app_menu.as_raw(),
            &[
                "/user/hand/left/input/y/click",
                "/user/hand/left/input/menu/click",
                "/user/hand/right/input/b/click",
            ],
        );
    }

    #[test]
    fn touch_like_bindings() {
        use crate::input::profiles::{pico::Pico4, touch_pro::TouchPro};
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        fn check<P: InteractionProfile>(f: &Fixture) {
            assert_legacy_bindings::<P>(
                f,
                |a| a.trigger_click.as_raw(),
                &[
                    "/user/hand/left/input/trigger/value",
                    "/user/hand/right/input/trigger/value",
                ],
            );
            assert_legacy_bindings::<P>(
                f,
                |a| a.squeeze_click.as_raw(),
                &[
                    "/user/hand/left/input/squeeze/value",
                    "/user/hand/right/input/squeeze/value",
                ],
            );
            assert_legacy_bindings::<P>(
                f,
                |a| a.a_touch.as_raw(),
                &[
                    "/user/hand/left/input/x/touch",
                    "/user/hand/right/input/a/touch",
                ],
            );
            assert_legacy_bindings::<P>(
                f,
                |a| a.app_menu.as_raw(),
                &[
                    "/user/hand/left/input/y/click",
                    "/user/hand/right/input/b/click",
                ],
            );
            assert_legacy_bindings::<P>(f, |a| a.grip_touch.as_raw(), &[]);
        }
        check::<OculusTouch>(&f);
        // SteamVR's Pico legacy bindings are the Touch's.
        check::<Pico4>(&f);
        check::<TouchPro>(&f);
    }

    #[test]
    fn cosmos_bindings() {
        use crate::input::profiles::vive_cosmos::ViveCosmos;
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        // Like the Focus 3, with clicks only.
        assert_legacy_bindings::<ViveCosmos>(
            &f,
            |a| a.app_menu.as_raw(),
            &[
                "/user/hand/left/input/y/click",
                "/user/hand/left/input/menu/click",
                "/user/hand/right/input/b/click",
            ],
        );
        assert_legacy_bindings::<ViveCosmos>(
            &f,
            |a| a.squeeze.as_raw(),
            &[
                "/user/hand/left/input/squeeze/click",
                "/user/hand/right/input/squeeze/click",
            ],
        );
        assert_legacy_bindings::<ViveCosmos>(&f, |a| a.a_touch.as_raw(), &[]);
    }

    #[test]
    fn legacy_input_on_pico() {
        use crate::input::profiles::pico::Pico4;
        use fakexr::UserPath::*;
        let (f, action) = legacy_fixture_for::<Pico4>(|actions| actions.a.as_raw());
        fakexr::set_action_state(action, fakexr::ActionState::Bool(true), RightHand);
        f.input.frame_start_update();
        let pressed = |index| get_state(&f, index).ulButtonPressed;
        assert_eq!(pressed(2), super::button_mask_from_id(vr::EVRButtonId::A));
        assert_eq!(pressed(1), 0);
        expect_button_event(&f, vr::EVREventType::ButtonTouch, 2, vr::EVRButtonId::A);
        expect_button_event(&f, vr::EVREventType::ButtonPress, 2, vr::EVRButtonId::A);
        expect_no_event(&f);
    }

    #[test]
    fn index_bindings() {
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        let both = |paths: &[&str]| -> Vec<String> {
            paths
                .iter()
                .flat_map(|p| ["left", "right"].map(|hand| format!("/user/hand/{hand}/input/{p}")))
                .collect()
        };
        let check = |get_action: fn(&super::LegacyActions) -> openxr::sys::Action,
                     paths: &[&str]| {
            let expected = both(paths);
            let expected: Vec<_> = expected.iter().map(String::as_str).collect();
            assert_legacy_bindings::<Knuckles>(&f, get_action, &expected);
        };

        check(|a| a.main_xy.as_raw(), &["thumbstick", "trackpad"]);
        check(
            |a| a.main_xy_click.as_raw(),
            &["thumbstick/click", "trackpad/force"],
        );
        check(
            |a| a.main_xy_touch.as_raw(),
            &["thumbstick/touch", "trackpad/touch"],
        );
        check(|a| a.main_xy_deflect.as_raw(), &["thumbstick"]);
        // A is an extra grip button, and never reported as k_EButton_A.
        check(|a| a.a.as_raw(), &[]);
        check(|a| a.a_touch.as_raw(), &[]);
        check(|a| a.grip_click.as_raw(), &["a/click"]);
        check(|a| a.grip_touch.as_raw(), &["a/touch", "squeeze/value"]);
        check(|a| a.squeeze_click.as_raw(), &["squeeze/force"]);
        check(|a| a.squeeze.as_raw(), &["squeeze/force"]);
        check(|a| a.app_menu.as_raw(), &["b/click"]);
        check(|a| a.app_menu_touch.as_raw(), &["b/touch"]);
        // SteamVR's Prop_SupportedButtons_Uint64
        assert_eq!(Knuckles::properties().legacy_buttons_mask, 0x7_0000_0007);
    }

    #[test]
    fn stick_deflection_presses_axis0() {
        use fakexr::UserPath::*;
        let (f, stick) = legacy_fixture(|actions| actions.main_xy_deflect.as_raw());
        let set_stick = |x, y| {
            fakexr::set_action_state(stick, fakexr::ActionState::Vector2(x, y), LeftHand);
            f.input.frame_start_update();
            get_state(&f, 1).ulButtonPressed
        };
        let mask = super::button_mask_from_id(vr::EVRButtonId::Axis0);

        assert_eq!(set_stick(0.75, 0.0), 0);
        // Diagonal, length 0.85
        assert_eq!(set_stick(0.6, 0.6), mask);
        expect_button_event(&f, vr::EVREventType::ButtonTouch, 1, vr::EVRButtonId::Axis0);
        expect_button_event(&f, vr::EVREventType::ButtonPress, 1, vr::EVRButtonId::Axis0);
        // Stays pressed until the stick drops below the release threshold.
        assert_eq!(set_stick(0.75, 0.0), mask);
        expect_no_event(&f);
        assert_eq!(set_stick(0.65, 0.0), 0);
        expect_button_event(
            &f,
            vr::EVREventType::ButtonUnpress,
            1,
            vr::EVRButtonId::Axis0,
        );
        expect_button_event(
            &f,
            vr::EVREventType::ButtonUntouch,
            1,
            vr::EVRButtonId::Axis0,
        );
    }

    #[test]
    fn axes() {
        use fakexr::{ActionState, UserPath::*};
        let (f, main_xy) = legacy_fixture(|actions| actions.main_xy.as_raw());
        let trigger = legacy_action(&f, |actions| actions.trigger.as_raw());
        let squeeze = legacy_action(&f, |actions| actions.squeeze.as_raw());
        fakexr::set_action_state(main_xy, ActionState::Vector2(0.3, -0.6), LeftHand);
        fakexr::set_action_state(trigger, ActionState::Float(0.4), LeftHand);
        fakexr::set_action_state(squeeze, ActionState::Float(0.8), LeftHand);
        f.input.frame_start_update();

        let axes = |index| {
            // Copy the field out, since references to packed fields are undefined.
            let axes = { get_state(&f, index).rAxis };
            axes.map(|axis| (axis.x, axis.y))
        };
        assert_eq!(
            axes(1),
            [(0.3, -0.6), (0.4, 0.0), (0.8, 0.0), (0.0, 0.0), (0.0, 0.0)]
        );
        assert_eq!(axes(2), [(0.0, 0.0); 5]);
    }

    #[test]
    fn trigger_value_touches_trigger() {
        use fakexr::UserPath::*;
        let (f, trigger) = legacy_fixture(|actions| actions.trigger.as_raw());
        let set_trigger = |value| {
            fakexr::set_action_state(trigger, fakexr::ActionState::Float(value), RightHand);
            f.input.frame_start_update();
            get_state(&f, 2)
        };
        let mask = super::button_mask_from_id(vr::EVRButtonId::SteamVR_Trigger);

        let state = set_trigger(0.2);
        assert_eq!({ state.ulButtonTouched }, mask);
        assert_eq!({ state.ulButtonPressed }, 0);
        expect_button_event(
            &f,
            vr::EVREventType::ButtonTouch,
            2,
            vr::EVRButtonId::SteamVR_Trigger,
        );

        // Hysteresis: stays touched until the value drops below the release threshold.
        assert_eq!({ set_trigger(0.07).ulButtonTouched }, mask);
        expect_no_event(&f);

        assert_eq!({ set_trigger(0.03).ulButtonTouched }, 0);
        expect_button_event(
            &f,
            vr::EVREventType::ButtonUntouch,
            2,
            vr::EVRButtonId::SteamVR_Trigger,
        );
    }

    /// Reading the controller state must not panic once the runtime lost the session.
    #[test]
    fn legacy_input_after_session_loss() {
        use fakexr::UserPath::*;
        let mut f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.input.frame_start_update();
        f.input.openxr.poll_events();

        let action = f
            .input
            .openxr
            .session_data
            .get()
            .input_data
            .get_legacy_actions()
            .unwrap()
            .actions
            .trigger_click
            .as_raw();
        fakexr::set_action_state(action, fakexr::ActionState::Bool(true), LeftHand);
        f.input.frame_start_update();

        let mut state = vr::VRControllerState_t::default();
        let mut get_state = || {
            let size = std::mem::size_of_val(&state) as u32;
            let got = f.input.get_legacy_controller_state(1, &mut state, size);
            (got, { state.ulButtonPressed })
        };
        assert!(matches!(get_state(), (true, pressed) if pressed != 0));

        fakexr::lose_session(f.raw_session());
        // Before the loss is noticed, the state of the last sync is kept.
        assert!(matches!(get_state(), (true, pressed) if pressed != 0));
        // Reads that fail after a sync count as released.
        {
            let data = f.input.openxr.session_data.get();
            let devices = data.input_data.devices.read().unwrap();
            let legacy = data.input_data.get_legacy_actions().unwrap();
            f.input.update_legacy_state(&data.session, &devices, legacy);
        }
        assert_eq!(get_state(), (true, 0));
        // The next sync notices it.
        f.input.frame_start_update();
        assert!(f.input.openxr.is_lost());
        assert_eq!(get_state(), (false, 0));
    }

    #[test]
    fn analog_click_thresholds() {
        use fakexr::UserPath::*;
        let (trigger_id, grip_id) = (vr::EVRButtonId::SteamVR_Trigger, vr::EVRButtonId::Grip);
        let pressed = |f: &Fixture, action, value, id| {
            fakexr::set_action_state(action, fakexr::ActionState::Float(value), LeftHand);
            f.input.frame_start_update();
            get_state(f, 1).ulButtonPressed & super::button_mask_from_id(id) != 0
        };

        // Like SteamVR's legacy bindings for them, the Index and the Frame press the trigger
        // from 0.55 until it drops below 0.5.
        for (f, trigger) in [
            legacy_fixture(|actions| actions.trigger.as_raw()),
            legacy_fixture_for::<FrameController>(|actions| actions.trigger.as_raw()),
        ] {
            assert!(!pressed(&f, trigger, 0.5, trigger_id));
            assert!(pressed(&f, trigger, 0.55, trigger_id));
            assert!(pressed(&f, trigger, 0.5, trigger_id));
            assert!(!pressed(&f, trigger, 0.45, trigger_id));
        }

        // The Frame grip presses from 0.1 until it drops below 0.05.
        let (f, squeeze) =
            legacy_fixture_for::<FrameController>(|actions| actions.squeeze.as_raw());
        assert!(!pressed(&f, squeeze, 0.08, grip_id));
        assert!(pressed(&f, squeeze, 0.1, grip_id));
        assert!(pressed(&f, squeeze, 0.06, grip_id));
        assert!(!pressed(&f, squeeze, 0.04, grip_id));

        // Others only press at the trigger's click.
        let (f, trigger) = legacy_fixture_for::<OculusTouch>(|actions| actions.trigger.as_raw());
        assert!(!pressed(&f, trigger, 0.9, trigger_id));
    }

    #[test]
    fn button_events_without_polling_state() {
        use fakexr::UserPath::*;
        let (f, trigger) = legacy_fixture(|actions| actions.trigger_click.as_raw());

        fakexr::set_action_state(trigger, true.into(), RightHand);
        f.input.frame_start_update();
        fakexr::set_action_state(trigger, false.into(), RightHand);
        f.input.frame_start_update();

        for ty in [
            vr::EVREventType::ButtonTouch,
            vr::EVREventType::ButtonPress,
            vr::EVREventType::ButtonUnpress,
            vr::EVREventType::ButtonUntouch,
        ] {
            expect_button_event(&f, ty, 2, vr::EVRButtonId::SteamVR_Trigger);
        }
        expect_no_event(&f);
    }

    #[test]
    fn packet_num_changes_with_each_sync() {
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        let first = get_state(&f, 1).unPacketNum;
        assert_eq!({ get_state(&f, 1).unPacketNum }, first);
        f.input.frame_start_update();
        assert_ne!({ get_state(&f, 1).unPacketNum }, first);
    }

    #[test]
    fn head_proximity() {
        const PROXIMITY: vr::EVRButtonId = vr::EVRButtonId::ProximitySensor;
        let (f, _) = legacy_fixture(|actions| actions.a.as_raw());
        f.input.frame_start_update();
        expect_no_event(&f);
        let worn =
            |f: &Fixture| get_state(f, 0).ulButtonPressed == super::button_mask_from_id(PROXIMITY);
        let set_present = |f: &Fixture, present| {
            fakexr::set_user_presence(f.raw_session(), present);
            f.input.openxr.poll_events();
            f.input.frame_start_update();
        };
        assert!(worn(&f));

        set_present(&f, false);
        assert!(!worn(&f));
        expect_button_event(&f, vr::EVREventType::ButtonUnpress, 0, PROXIMITY);
        expect_no_event(&f);

        set_present(&f, true);
        assert!(worn(&f));
        expect_button_event(&f, vr::EVREventType::ButtonPress, 0, PROXIMITY);
        expect_no_event(&f);
    }

    #[test]
    fn buttons_held_across_connection_changes() {
        use fakexr::UserPath::*;
        use vr::EVREventType as Ev;
        let mut f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.input.frame_start_update();
        let trigger = legacy_action(&f, |actions| actions.trigger_click.as_raw());
        let expect_device_event = |f: &Fixture, ty: vr::EVREventType, index| {
            let event = next_event(f).unwrap_or_else(|| panic!("Expected {ty:?}"));
            assert_eq!((event.ty, event.index), (ty.0, index));
        };

        // Turned on with the trigger held: the activation comes first.
        fakexr::set_action_state(trigger, true.into(), LeftHand);
        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.input.frame_start_update();
        f.input.openxr.poll_events();
        f.input.frame_start_update();
        expect_device_event(&f, Ev::TrackedDeviceActivated, 0);
        expect_device_event(&f, Ev::TrackedDeviceActivated, 1);
        expect_button_event(&f, Ev::ButtonTouch, 1, vr::EVRButtonId::SteamVR_Trigger);
        expect_button_event(&f, Ev::ButtonPress, 1, vr::EVRButtonId::SteamVR_Trigger);
        expect_no_event(&f);

        // Turned off with the trigger held: it is released, and the deactivation follows once it
        // could no longer be a blip.
        fakexr::set_interaction_profile(f.raw_session(), LeftHand, xr::Path::NULL);
        fakexr::deactivate_action(trigger);
        f.input.frame_start_update();
        f.input.openxr.poll_events();
        expect_button_event(&f, Ev::ButtonUnpress, 1, vr::EVRButtonId::SteamVR_Trigger);
        expect_button_event(&f, Ev::ButtonUntouch, 1, vr::EVRButtonId::SteamVR_Trigger);
        expect_no_event(&f);
        f.input.expire_connection_grace();
        expect_device_event(&f, Ev::TrackedDeviceDeactivated, 1);
        expect_no_event(&f);
    }

    #[test]
    fn state_recovers_after_profile_blip() {
        use fakexr::UserPath::*;
        fakexr::set_actions_need_profile(true);
        let (mut f, a) = legacy_fixture(|actions| actions.a.as_raw());
        let frame = |f: &Fixture| {
            f.input.frame_start_update();
            f.input.openxr.poll_events();
        };
        let a_pressed = |f: &Fixture| {
            get_state(f, 1).ulButtonPressed & super::button_mask_from_id(vr::EVRButtonId::A) != 0
        };
        fakexr::set_action_state(a, true.into(), LeftHand);
        frame(&f);
        assert!(a_pressed(&f));
        expect_button_event(&f, vr::EVREventType::ButtonTouch, 1, vr::EVRButtonId::A);
        expect_button_event(&f, vr::EVREventType::ButtonPress, 1, vr::EVRButtonId::A);

        // Released while the profile is gone, without losing the controller.
        for hand in [LeftHand, RightHand] {
            fakexr::set_interaction_profile(f.raw_session(), hand, xr::Path::NULL);
        }
        frame(&f);
        assert!(!a_pressed(&f));
        expect_button_event(&f, vr::EVREventType::ButtonUnpress, 1, vr::EVRButtonId::A);
        expect_button_event(&f, vr::EVREventType::ButtonUntouch, 1, vr::EVRButtonId::A);
        expect_no_event(&f);

        f.set_interaction_profile::<Knuckles>(LeftHand);
        f.set_interaction_profile::<Knuckles>(RightHand);
        frame(&f);
        frame(&f);
        assert!(a_pressed(&f));
        expect_button_event(&f, vr::EVREventType::ButtonTouch, 1, vr::EVRButtonId::A);
        expect_button_event(&f, vr::EVREventType::ButtonPress, 1, vr::EVRButtonId::A);
        expect_no_event(&f);
    }

    #[test]
    fn unpolled_button_events_are_bounded() {
        use fakexr::UserPath::*;
        let (f, trigger) = legacy_fixture(|actions| actions.trigger_click.as_raw());

        // Each frame queues a touch and a press, or a release and an untouch.
        for i in 0..=super::MAX_QUEUED_EVENTS / 2 {
            fakexr::set_action_state(trigger, (i % 2 == 0).into(), RightHand);
            f.input.frame_start_update();
        }

        let events: Vec<_> = std::iter::from_fn(|| next_event(&f)).collect();
        assert_eq!(events.len(), super::MAX_QUEUED_EVENTS);
        // The oldest events are dropped, the newest ones kept.
        assert_eq!(events[0].ty, vr::EVREventType::ButtonUnpress.0);
        assert_eq!(events.last().unwrap().ty, vr::EVREventType::ButtonPress.0);
    }

    #[test]
    fn no_legacy_input_with_manifest() {
        let mut f = Fixture::new();

        f.input.openxr.restart_session().unwrap();

        f.set_interaction_profile::<SimpleController>(fakexr::UserPath::LeftHand);
        f.set_interaction_profile::<SimpleController>(fakexr::UserPath::RightHand);
        f.input.frame_start_update();
        f.input.openxr.poll_events();

        let mut state = vr::VRControllerState_t::default();
        assert!(f.input.get_legacy_controller_state(
            1,
            &mut state,
            std::mem::size_of_val(&state) as u32
        ));

        f.load_actions(c"actions.json");
        f.input.openxr.poll_events();
        f.input.frame_start_update();
        assert!(!f.input.get_legacy_controller_state(
            1,
            &mut state,
            std::mem::size_of_val(&state) as u32
        ));
    }

    #[test]
    fn poses_updated() {
        use fakexr::UserPath::*;
        let mut f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.set_interaction_profile::<SimpleController>(LeftHand);
        f.set_interaction_profile::<SimpleController>(RightHand);
        f.input.frame_start_update();
        f.input.openxr.poll_events();

        fakexr::set_grip(f.raw_session(), LeftHand, xr::Posef::IDENTITY);
        fakexr::set_grip(f.raw_session(), RightHand, xr::Posef::IDENTITY);
        f.input.frame_start_update();
        let xr_time = xr::Time::from_nanos(1234);

        let seated_origin = vr::ETrackingUniverseOrigin::Seated;
        let left_pose = f
            .input
            .get_controller_pose(Hand::Left, Some(seated_origin), xr_time, None);
        compare_pose(
            xr::Posef::IDENTITY,
            left_pose.unwrap().mDeviceToAbsoluteTracking.into(),
        );
        compare_pose(
            xr::Posef::IDENTITY,
            f.input
                .get_controller_pose(Hand::Right, Some(seated_origin), xr_time, None)
                .unwrap()
                .mDeviceToAbsoluteTracking
                .into(),
        );

        let new_pose = xr::Posef {
            position: xr::Vector3f {
                x: 0.5,
                y: 0.5,
                z: 0.5,
            },
            orientation: xr::Quaternionf::IDENTITY,
        };

        fakexr::set_grip(f.raw_session(), LeftHand, new_pose);
        fakexr::set_grip(f.raw_session(), RightHand, new_pose);
        f.input.frame_start_update();
        compare_pose(
            new_pose,
            f.input
                .get_controller_pose(Hand::Left, Some(seated_origin), xr_time, None)
                .unwrap()
                .mDeviceToAbsoluteTracking
                .into(),
        );
        compare_pose(
            new_pose,
            f.input
                .get_controller_pose(Hand::Right, Some(seated_origin), xr_time, None)
                .unwrap()
                .mDeviceToAbsoluteTracking
                .into(),
        );
    }

    #[test]
    fn init_controller_state_on_failure() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        f.input.frame_start_update();

        let mut state = std::mem::MaybeUninit::<vr::VRControllerState_t>::uninit();
        assert!(!f.input.get_legacy_controller_state(
            0,
            state.as_mut_ptr(),
            std::mem::size_of_val(&state) as u32
        ));

        let state = unsafe { state.assume_init() };
        assert_eq!({ state.ulButtonPressed }, 0);
    }

    #[test]
    fn legacy_haptic() {
        let mut f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.set_interaction_profile::<SimpleController>(fakexr::UserPath::LeftHand);
        f.set_interaction_profile::<SimpleController>(fakexr::UserPath::RightHand);
        f.input.openxr.poll_events();
        f.input.frame_start_update();

        f.input.openxr.poll_events();
        f.input.frame_start_update();
        let haptic = f
            .input
            .openxr
            .session_data
            .get()
            .input_data
            .get_legacy_actions()
            .unwrap()
            .actions
            .haptic
            .as_raw();

        assert!(!fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::LeftHand
        ));
        assert!(!fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::RightHand
        ));

        f.input.legacy_haptic(1, 0, 3000);
        assert!(fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::LeftHand
        ));

        f.input.legacy_haptic(2, 0, 3000);
        assert!(fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::RightHand
        ));

        // A full strength pulse, whatever its length.
        let feedback = fakexr::take_haptic_feedback(haptic);
        assert_eq!(feedback.len(), 2, "{feedback:?}");
        let hands = [fakexr::UserPath::LeftHand, fakexr::UserPath::RightHand];
        for (feedback, hand) in feedback.iter().zip(hands) {
            assert_eq!(feedback.hand, Some(hand));
            assert_eq!(feedback.amplitude, 1.0);
            assert_eq!(
                feedback.duration.as_nanos(),
                Vibration::MIN_DURATION.as_nanos() as i64
            );
            assert_eq!(feedback.frequency, xr::FREQUENCY_UNSPECIFIED);
        }

        // A pulse of nothing doesn't vibrate, and every axis pulses the one haptic.
        f.input.legacy_haptic(1, 0, 0);
        assert!(fakexr::take_haptic_feedback(haptic).is_empty());
        f.input.legacy_haptic(1, 1, 8000);
        let feedback = fakexr::take_haptic_feedback(haptic);
        assert_eq!(feedback.len(), 1, "{feedback:?}");
        assert_eq!(feedback[0].amplitude, 1.0);
    }

    #[test]
    fn legacy_haptic_with_action_manifest() {
        let mut f = Fixture::new();
        f.load_actions(c"actions.json");
        f.input.openxr.restart_session().unwrap();
        f.set_interaction_profile::<SimpleController>(fakexr::UserPath::LeftHand);
        f.set_interaction_profile::<SimpleController>(fakexr::UserPath::RightHand);
        f.input.openxr.poll_events();
        f.input.frame_start_update();

        f.input.openxr.poll_events();
        f.input.frame_start_update();

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

        assert!(!fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::LeftHand
        ));
        assert!(!fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::RightHand
        ));

        f.input.legacy_haptic(1, 0, 3000);
        assert!(fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::LeftHand
        ));

        f.input.legacy_haptic(2, 0, 3000);
        assert!(fakexr::is_haptic_activated(
            haptic,
            fakexr::UserPath::RightHand
        ));

        let feedback = fakexr::take_haptic_feedback(haptic);
        assert_eq!(feedback.len(), 2, "{feedback:?}");
        assert!(feedback.iter().all(|f| f.amplitude == 1.0), "{feedback:?}");
    }

    fn legacy_bindings_for<P: crate::input::InteractionProfile>(
        f: &Fixture,
        get_action: impl FnOnce(&super::LegacyActions) -> openxr::sys::Action,
    ) -> Option<Vec<String>> {
        let data = f.input.openxr.session_data.get();
        let action = get_action(&data.input_data.get_legacy_actions().unwrap().actions);
        let profile = f
            .input
            .openxr
            .instance
            .string_to_path(P::profile_path())
            .unwrap();
        (!fakexr::check_no_suggested_bindings(action, profile))
            .then(|| fakexr::get_suggested_bindings(action, profile))
    }

    #[test]
    fn rejected_profile_is_skipped() {
        use crate::input::profiles::InteractionProfile;
        use crate::input::profiles::oculus_touch::OculusTouch;

        fakexr::reject_profile(Knuckles::profile_path());
        let f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.input.frame_start_update();

        let trigger = |actions: &super::LegacyActions| actions.trigger.as_raw();
        assert!(legacy_bindings_for::<Knuckles>(&f, trigger).is_none());
        // Profiles suggested after the rejected one are still bound.
        assert!(legacy_bindings_for::<OculusTouch>(&f, trigger).is_some());
    }

    #[test]
    fn hand_interaction_not_suggested() {
        use crate::input::profiles::vrlink_hand::VRLinkHand;

        fakexr::advertise_extension(xr::sys::EXT_HAND_INTERACTION_EXTENSION_NAME);
        let f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.input.frame_start_update();

        // The suggestion would outlive legacy input and keep manifest bindings off hands.
        assert!(legacy_bindings_for::<VRLinkHand>(&f, |a| a.trigger.as_raw()).is_none());
    }

    /// Sets up legacy input with `L` and `R` controllers, with bindings chosen for the app.
    fn remapped_fixture<L: InteractionProfile, R: InteractionProfile>(
        bindings_dir: &str,
    ) -> Fixture {
        use crate::input::tests::{input_data, set_test_env};
        use fakexr::UserPath::*;
        set_test_env("XRIZER_CUSTOM_BINDINGS_DIR", &input_data(bindings_dir));
        let mut f = Fixture::new();
        f.input.openxr.restart_session().unwrap();
        f.set_interaction_profile::<L>(LeftHand);
        f.set_interaction_profile::<R>(RightHand);
        f.input.frame_start_update();
        f.input.openxr.poll_events();
        while let Some(event) = next_event(&f) {
            assert_eq!(event.ty, vr::EVREventType::TrackedDeviceActivated.0);
        }
        f
    }

    /// The OpenXR action of a legacy manifest action.
    fn remapped_action(f: &Fixture, name: &CStr) -> xr::sys::Action {
        let handle = f.get_action_handle(name);
        let data = f.input.openxr.session_data.get();
        let loaded = data.input_data.get_loaded_actions().unwrap();
        assert!(loaded.legacy.is_some(), "legacy input isn't remapped");
        match loaded.actions.get(KeyData::from_ffi(handle).into()) {
            Some(super::ActionData::Bool(action)) => action.as_raw(),
            Some(super::ActionData::Vector1 { action, .. }) => action.as_raw(),
            Some(super::ActionData::Vector2 { action, .. }) => action.as_raw(),
            Some(super::ActionData::Haptic(action)) => action.as_raw(),
            _ => panic!("no action {name:?}"),
        }
    }

    /// The fixed legacy actions that controllers without chosen bindings use.
    fn fixed_action(
        f: &Fixture,
        get_action: impl FnOnce(&super::LegacyActions) -> xr::sys::Action,
    ) -> xr::sys::Action {
        let data = f.input.openxr.session_data.get();
        let loaded = data.input_data.get_loaded_actions().unwrap();
        get_action(&loaded.legacy.as_ref().unwrap().fixed.actions)
    }

    #[test]
    fn remapped_buttons_and_axes() {
        use fakexr::{ActionState::*, UserPath::*};
        use vr::EVRButtonId as Button;
        let f = remapped_fixture::<OculusTouch, OculusTouch>("legacy_custom");
        // Names match in any case.
        f.verify_bindings::<bool>(
            OculusTouch::profile_path(),
            c"/actions/legacy/in/Right_ApplicationMenu_Press",
            ["/user/hand/right/input/a/click".into()],
        );
        f.verify_bindings::<bool>(
            OculusTouch::profile_path(),
            c"/actions/legacy/in/right_system_press",
            ["/user/hand/right/input/b/click".into()],
        );
        let set =
            |name, state, hand| fakexr::set_action_state(remapped_action(&f, name), state, hand);
        set(
            c"/actions/legacy/in/right_applicationmenu_press",
            Bool(true),
            RightHand,
        );
        set(
            c"/actions/legacy/in/right_axis3_press",
            Bool(true),
            RightHand,
        );
        set(
            c"/actions/legacy/in/right_axis1_value",
            Float(0.7),
            RightHand,
        );
        set(
            c"/actions/legacy/in/right_axis0_value",
            Vector2(0.5, -0.25),
            RightHand,
        );
        set(
            c"/actions/legacy/in/left_axis2_value2",
            Vector2(0.1, 0.2),
            LeftHand,
        );
        set(
            c"/actions/legacy/in/left_axis4_value_e0",
            Float(0.6),
            LeftHand,
        );
        set(
            c"/actions/legacy/in/left_axis4_value_e1",
            Float(0.3),
            LeftHand,
        );
        f.input.frame_start_update();

        let right = get_state(&f, 2);
        let pressed = super::button_mask_from_id(Button::ApplicationMenu)
            | super::button_mask_from_id(Button::Axis3);
        assert_eq!({ right.ulButtonPressed }, pressed);
        assert_eq!({ right.ulButtonTouched }, pressed);
        let axis = |state: &vr::VRControllerState_t, i: usize| {
            let axes = state.rAxis;
            (axes[i].x, axes[i].y)
        };
        assert_eq!(axis(&right, 0), (0.5, -0.25));
        assert_eq!(axis(&right, 1), (0.7, 0.0));
        let left = get_state(&f, 1);
        assert_eq!({ left.ulButtonPressed }, 0);
        assert_eq!(axis(&left, 2), (0.1, 0.2));
        assert_eq!(axis(&left, 4), (0.6, 0.3));

        for id in [Button::ApplicationMenu, Button::Axis3] {
            expect_button_event(&f, vr::EVREventType::ButtonTouch, 2, id);
            expect_button_event(&f, vr::EVREventType::ButtonPress, 2, id);
        }
        expect_no_event(&f);
    }

    /// Controllers without chosen bindings keep the fixed mapping.
    #[test]
    fn remapped_only_for_chosen_controllers() {
        use fakexr::UserPath::*;
        let f = remapped_fixture::<Knuckles, OculusTouch>("legacy_custom");
        let trigger_click = fixed_action(&f, |actions| actions.trigger_click.as_raw());
        assert!(fakexr::check_no_suggested_bindings(
            trigger_click,
            f.input
                .openxr
                .instance
                .string_to_path(OculusTouch::profile_path())
                .unwrap()
        ));
        fakexr::set_action_state(trigger_click, true.into(), LeftHand);
        fakexr::set_action_state(trigger_click, true.into(), RightHand);
        f.input.frame_start_update();

        let trigger = super::button_mask_from_id(vr::EVRButtonId::SteamVR_Trigger);
        assert_eq!({ get_state(&f, 1).ulButtonPressed }, trigger);
        assert_eq!({ get_state(&f, 2).ulButtonPressed }, 0);
    }

    #[test]
    fn remapped_haptics() {
        let f = remapped_fixture::<OculusTouch, OculusTouch>("legacy_custom");
        let left = remapped_action(&f, c"/actions/legacy/out/left_haptic");
        let right = remapped_action(&f, c"/actions/legacy/out/right_haptic");
        let fixed = fixed_action(&f, |actions| actions.haptic.as_raw());

        f.input.legacy_haptic(1, 0, 3000);
        let feedback = fakexr::take_haptic_feedback(left);
        assert_eq!(feedback.len(), 1, "{feedback:?}");
        assert_eq!(feedback[0].hand, Some(fakexr::UserPath::LeftHand));
        // The bindings give the right hand no haptic, so its pulses go nowhere.
        assert!(fakexr::check_no_suggested_bindings(
            right,
            f.input
                .openxr
                .instance
                .string_to_path(OculusTouch::profile_path())
                .unwrap()
        ));
        f.input.legacy_haptic(2, 0, 3000);
        assert!(fakexr::take_haptic_feedback(left).is_empty());
        assert!(fakexr::take_haptic_feedback(fixed).is_empty());
    }

    #[test]
    fn remapped_pose() {
        use crate::input::BoundPoseType;
        let f = remapped_fixture::<OculusTouch, OculusTouch>("legacy_custom");
        let grip = xr::Posef {
            position: xr::Vector3f {
                x: 0.1,
                y: 0.2,
                z: 0.3,
            },
            ..xr::Posef::IDENTITY
        };
        for hand in [fakexr::UserPath::LeftHand, fakexr::UserPath::RightHand] {
            fakexr::set_grip(f.raw_session(), hand, grip);
        }
        f.input.frame_start_update();
        let time = xr::Time::from_nanos(1);
        let pose = |index, ty| {
            f.input
                .get_device_pose(index, None, time, ty)
                .unwrap()
                .mDeviceToAbsoluteTracking
                .m
        };
        // Left_Pose is bound to the tip, Right_Pose to the raw pose.
        assert_eq!(pose(1, None), pose(1, Some(BoundPoseType::Tip)));
        assert_ne!(pose(1, None), pose(1, Some(BoundPoseType::Raw)));
        assert_eq!(pose(2, None), pose(2, Some(BoundPoseType::Raw)));
        let mut poses = [vr::TrackedDevicePose_t::default(); 3];
        f.input.get_poses(&mut poses, None, time);
        assert_eq!(
            poses[1].mDeviceToAbsoluteTracking.m,
            pose(1, Some(BoundPoseType::Tip))
        );
    }

    /// With mirror_actions, both hands drive /actions/legacy_mirrored and /actions/legacy is
    /// ignored, like in SteamVR.
    #[test]
    fn remapped_mirrored() {
        use fakexr::UserPath::*;
        let f = remapped_fixture::<OculusTouch, OculusTouch>("legacy_mirrored");
        let profile = OculusTouch::profile_path();
        f.verify_bindings::<f32>(
            profile,
            c"/actions/legacy_mirrored/in/axis1_value",
            [
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/right/input/trigger/value".into(),
            ],
        );
        assert!(fakexr::check_no_suggested_bindings(
            remapped_action(&f, c"/actions/legacy/in/right_system_press"),
            f.input.openxr.instance.string_to_path(profile).unwrap()
        ));

        let a = remapped_action(&f, c"/actions/legacy_mirrored/in/a_press");
        fakexr::set_action_state(a, true.into(), LeftHand);
        fakexr::set_action_state(a, false.into(), RightHand);
        f.input.frame_start_update();
        let mask = super::button_mask_from_id(vr::EVRButtonId::A);
        assert_eq!({ get_state(&f, 1).ulButtonPressed }, mask);
        assert_eq!({ get_state(&f, 2).ulButtonPressed }, 0);

        let haptic = remapped_action(&f, c"/actions/legacy_mirrored/out/haptic");
        f.input.legacy_haptic(2, 0, 3000);
        let feedback = fakexr::take_haptic_feedback(haptic);
        assert_eq!(feedback.len(), 1, "{feedback:?}");
        assert_eq!(feedback[0].hand, Some(RightHand));
    }

    /// The controller shows as the one simulated_controller_type names.
    #[test]
    fn remapped_simulated_controller_type() {
        let f = remapped_fixture::<OculusTouch, OculusTouch>("legacy_mirrored");
        let data = f.input.openxr.session_data.get();
        let devices = data.input_data.devices.read().unwrap();
        let properties = devices
            .get_controller(Hand::Left)
            .and_then(|controller| controller.profile_data.as_ref())
            .unwrap()
            .properties;
        assert!(std::ptr::eq(properties, Knuckles::properties()));
    }

    #[test]
    fn legacy_bindings_chosen_in_steamvr() {
        use crate::input::tests::{FakeSteam, input_data};
        let steam = FakeSteam::new("legacy");
        let url = format!("file://{}", input_data("legacy_custom/oculus_touch.json"));
        steam.choose(&[
            ("oculus_touch_250820_CurrentURL_legacy", &url),
            // The choice for manifest apps doesn't count.
            (
                "knuckles_250820_CurrentURL_steamvrinput",
                &format!("file://{}", input_data("legacy_custom/oculus_touch.json")),
            ),
        ]);
        let f = remapped_fixture::<OculusTouch, Knuckles>("nonexistent");
        f.verify_bindings::<bool>(
            OculusTouch::profile_path(),
            c"/actions/legacy/in/right_system_press",
            ["/user/hand/right/input/b/click".into()],
        );
        let data = f.input.openxr.session_data.get();
        let remap = data
            .input_data
            .get_loaded_actions()
            .unwrap()
            .legacy
            .as_ref()
            .unwrap();
        let profile = |name| f.input.openxr.instance.string_to_path(name).unwrap();
        assert!(
            remap
                .profiles
                .contains(&profile(OculusTouch::profile_path()))
        );
        assert!(!remap.profiles.contains(&profile(Knuckles::profile_path())));
    }

    /// Custom bindings for an action manifest aren't legacy bindings.
    #[test]
    fn custom_manifest_bindings_keep_fixed_legacy_input() {
        let f = remapped_fixture::<Knuckles, Knuckles>("custom_bindings");
        assert!(
            f.input
                .openxr
                .session_data
                .get()
                .input_data
                .get_legacy_actions()
                .is_some()
        );
    }

    /// An action manifest loaded after legacy input replaces it.
    #[test]
    fn manifest_after_remapped_legacy() {
        use vr::IVRInput010_Interface;
        let f = remapped_fixture::<OculusTouch, OculusTouch>("legacy_custom");
        assert!(f.input.IsUsingLegacyInput());
        f.load_actions(c"actions_vive_only.json");
        assert!(!f.input.IsUsingLegacyInput());
        let data = f.input.openxr.session_data.get();
        assert!(
            data.input_data
                .get_loaded_actions()
                .unwrap()
                .legacy
                .is_none()
        );
        drop(data);
        assert!(!f.input.get_legacy_controller_state(
            1,
            &mut vr::VRControllerState_t::default(),
            std::mem::size_of::<vr::VRControllerState_t>() as u32
        ));
    }

    /// Legacy bindings chosen for the key an app identifies as after legacy input started.
    #[test]
    fn legacy_bindings_for_identified_key() {
        use crate::input::tests::{FakeSteam, add_app_bindings};
        const KEY: &str = "org.example.legacy";
        let steam = FakeSteam::new("legacy-identified");
        add_app_bindings(
            &steam.home.join(".config"),
            KEY,
            "oculus_touch.json",
            "legacy_custom/oculus_touch.json",
        );
        let f = remapped_fixture::<OculusTouch, OculusTouch>("nonexistent");
        assert!(
            f.input
                .openxr
                .session_data
                .get()
                .input_data
                .get_legacy_actions()
                .is_some()
        );

        f.input.app_identified(KEY);
        f.input.frame_start_update();
        remapped_action(&f, c"/actions/legacy/in/right_system_press");
    }
}
