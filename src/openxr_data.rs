use crate::{
    clientcore::{Injected, Injector},
    graphics_backends::{GraphicsBackend, VulkanData, supported_apis_enum},
    input::InputSessionData,
    system::System,
};
use ash::vk::{self, Handle};

#[cfg(feature = "monado")]
use crate::monado::SafeMonado;

use derive_more::Deref;
use glam::f32::{Quat, Vec3};
use log::{debug, error, info, warn};
use openvr as vr;
use openxr as xr;
use parking_lot::{RwLock, RwLockReadGuard};
use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for the runtime to move the session to a requested state.
const SESSION_STATE_TIMEOUT: Duration = Duration::from_secs(3);

/// Unknown to the openxr crate, so it lives in `ExtensionSet::other`, whose names keep their nul.
pub const XR_VALVE_FRAME_CONTROLLER_INTERACTION: &[u8] = b"XR_VALVE_frame_controller_interaction\0";

/// How long after its last WaitGetPoses an app no longer counts as running a frame loop.
const FRAME_LOOP_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(feature = "monado")]
use openxr_mndx_xdev_space::XR_MNDX_XDEV_SPACE_EXTENSION_NAME;

pub trait Compositor: vr::InterfaceImpl {
    fn post_session_restart(
        &self,
        session: &SessionData,
        waiter: xr::FrameWaiter,
        stream: FrameStream,
    );

    fn get_session_create_info(
        &self,
        data: crate::compositor::CompositorSessionData,
    ) -> SessionCreateInfo;

    #[cfg(test)]
    fn on_restart(&self) {}
}

pub type RealOpenXrData = OpenXrData<crate::compositor::Compositor>;
pub struct OpenXrData<C: Compositor> {
    _entry: xr::Entry,
    pub instance: xr::Instance,
    pub system_id: xr::SystemId,
    pub session_data: SessionReadGuard,
    /// Only meaningful while the compositor waits for frames - read it through
    /// [Self::get_display_time].
    display_time: AtomicXrTime,
    pub display_period_nanos: AtomicI64,
    pub enabled_extensions: xr::ExtensionSet,
    /// The interaction profiles that were bound when we probed at init - see
    /// [crate::input::probe_interaction_profiles].
    pub probed_profiles: crate::input::ProbedProfiles,
    pub application_type: vr::EVRApplicationType,
    pub headless: bool,
    /// Set while we are ending the session ourselves (restart or shutdown), so the resulting
    /// STOPPING state isn't reported to the application as a Quit.
    exit_requested: AtomicBool,
    /// Set once the application was told to quit because the runtime stopped or lost the session.
    /// No new session is started afterwards, so this and `lost` are never cleared.
    exiting: AtomicBool,
    /// Set once the runtime lost the session or instance. Every call on the session fails from
    /// then on, so callers skip them.
    lost: AtomicBool,
    /// Set when creating the real session failed, so Submit doesn't tear down the temporary
    /// session for another attempt every frame.
    session_creation_failed: AtomicBool,
    /// The most composition layers the runtime accepts in one frame.
    pub max_layer_count: usize,
    /// Runtime errors already logged as warnings, so one that repeats every frame is logged once.
    logged_errors: Mutex<HashSet<(&'static str, i32)>>,
    /// Whether the runtime tells us when the user puts on or takes off the headset.
    pub supports_user_presence: bool,
    user_present: AtomicBool,
    /// Set while the runtime holds the input focus it took from us (e.g. its system menu).
    input_focus_captured: AtomicBool,
    /// Change time of a runtime recenter of LOCAL that hasn't taken effect yet.
    pending_recenter: parking_lot::Mutex<Option<xr::Time>>,
    /// Set when the runtime moved STAGE and the app's standing zero pose is still in place.
    stage_moved: AtomicBool,
    /// Held while WaitGetPoses or [Self::pump] run the per-frame update.
    frame_updates: parking_lot::Mutex<FrameUpdates>,

    /// should only be externally accessed for testing
    pub(crate) input: Injected<crate::input::Input<C>>,
    pub(crate) compositor: Injected<C>,
    pub(crate) system: Injected<System>,
}

impl<C: Compositor> Drop for OpenXrData<C> {
    fn drop(&mut self) {
        let mut data = unsafe { ManuallyDrop::take(self.session_data.0.get_mut()) };
        self.end_session(&mut data);
    }
}

#[derive(Debug)]
#[allow(dead_code)] // Results aren't used, but they're printed
#[allow(clippy::enum_variant_names)]
pub enum InitError {
    LoaderUnavailable(String),
    EnumeratingExtensionsFailed(xr::sys::Result),
    InstanceCreationFailed(xr::sys::Result),
    SystemCreationFailed(xr::sys::Result),
    SessionCreationFailed(SessionCreationError),
    /// The application type needs a headless session, which the runtime doesn't support.
    HeadlessUnsupported(vr::EVRApplicationType),
}

impl From<SessionCreationError> for InitError {
    fn from(value: SessionCreationError) -> Self {
        Self::SessionCreationFailed(value)
    }
}

fn get_app_name() -> Option<String> {
    let exe = std::fs::read_link("/proc/self/exe")
        .inspect_err(|e| warn!("Couldn't get app name from /proc/self/exe: {e}"))
        .ok()?;

    let basename = exe.file_name().unwrap();
    if basename == "wine64-preloader" || basename == "wine-preloader" {
        fn extract_wine_exe_name() -> Option<String> {
            let exe_path = std::env::args().next()?;
            // The Windows path separator is \ (instead of /) so we can't use Path.
            // We just want the basename anyway, so we'll just grab the last piece.
            let exe_name = exe_path.rsplit_once('\\')?.1;
            Some(
                exe_name
                    .strip_suffix(".exe")
                    .unwrap_or(exe_name)
                    .to_string(),
            )
        }
        if let Some(name) = extract_wine_exe_name() {
            return Some(name);
        }
    }

    Some(basename.to_string_lossy().into_owned())
}

/// The OpenXR loader, for Init and the HMD probe alike. Games with a flat mode fall back to it
/// on an init error, so this doesn't abort.
pub fn load_entry() -> Result<xr::Entry, InitError> {
    let platform_info = ();

    #[cfg(all(not(test), feature = "static-openxr"))]
    let entry =
        xr::Entry::linked(&platform_info).map_err(|e| InitError::LoaderUnavailable(e.to_string()));

    #[cfg(all(not(test), not(feature = "static-openxr")))]
    let entry = unsafe { xr::Entry::load(&platform_info) }.map_err(|e| {
        let hint = if cfg!(target_pointer_width = "32") {
            "this is the 32-bit xrizer, which needs a 32-bit OpenXR loader \
            (e.g. lib32-openxr) - alternatively, build with --features static-openxr"
        } else {
            "is libopenxr-loader installed?"
        };
        InitError::LoaderUnavailable(format!("Failed to load OpenXR loader ({hint}): {e:?}"))
    });

    #[cfg(test)]
    let entry = unsafe {
        xr::Entry::from_get_instance_proc_addr(fakexr::get_instance_proc_addr, &platform_info)
    }
    .map_err(|e| InitError::LoaderUnavailable(format!("{e:?}")));

    entry
}

fn make_version() -> u32 {
    env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>().unwrap_or(0) * 1000000
        + env!("CARGO_PKG_VERSION_MINOR").parse::<u32>().unwrap_or(0) * 1000
        + env!("CARGO_PKG_VERSION_PATCH").parse::<u32>().unwrap_or(1)
}

impl<C: Compositor> OpenXrData<C> {
    pub fn new(
        injector: &Injector,
        application_type: vr::EVRApplicationType,
    ) -> Result<Self, InitError> {
        let platform_info = ();
        let entry = load_entry()?;

        let supported_exts = entry
            .enumerate_extensions()
            .map_err(InitError::EnumeratingExtensionsFailed)?;

        let headless = match application_type {
            vr::EVRApplicationType::Background
            | vr::EVRApplicationType::Overlay
            | vr::EVRApplicationType::Utility => {
                if !supported_exts.mnd_headless {
                    error!("XR_MND_headless extension required for headless not supported!");
                    false
                } else if !supported_exts.khr_convert_timespec_time {
                    error!(
                        "XR_KHR_convert_timespec_time extension required for headless not supported!"
                    );
                    false
                } else {
                    true
                }
            }
            _ => false,
        };
        // A graphics session for these could take the display from the app that renders. Background
        // apps still fall back to one, since Proton's helper inits as Background on any runtime.
        if !headless
            && matches!(
                application_type,
                vr::EVRApplicationType::Overlay | vr::EVRApplicationType::Utility
            )
        {
            return Err(InitError::HeadlessUnsupported(application_type));
        }

        let mut exts = xr::ExtensionSet::default();
        if headless {
            exts.mnd_headless = supported_exts.mnd_headless;
            exts.khr_convert_timespec_time = supported_exts.khr_convert_timespec_time;
        }
        exts.khr_vulkan_enable = supported_exts.khr_vulkan_enable;
        exts.khr_opengl_enable = supported_exts.khr_opengl_enable;
        exts.mndx_egl_enable = supported_exts.mndx_egl_enable;
        exts.khr_convert_timespec_time = supported_exts.khr_convert_timespec_time;
        exts.ext_hand_tracking = supported_exts.ext_hand_tracking;
        exts.ext_hand_tracking_data_source = supported_exts.ext_hand_tracking_data_source;
        // Not XR_EXT_hand_interaction: once its profile is suggested, runtimes bind hands to it
        // instead of rebinding them to simple_controller, which action manifests do bind.
        exts.khr_visibility_mask = supported_exts.khr_visibility_mask;
        exts.khr_composition_layer_cylinder = supported_exts.khr_composition_layer_cylinder;
        exts.khr_composition_layer_equirect2 = supported_exts.khr_composition_layer_equirect2;
        exts.khr_composition_layer_color_scale_bias =
            supported_exts.khr_composition_layer_color_scale_bias;
        exts.htc_vive_focus3_controller_interaction =
            supported_exts.htc_vive_focus3_controller_interaction;
        exts.meta_touch_controller_plus = supported_exts.meta_touch_controller_plus;
        exts.ext_samsung_odyssey_controller = supported_exts.ext_samsung_odyssey_controller;
        exts.ext_hp_mixed_reality_controller = supported_exts.ext_hp_mixed_reality_controller;
        exts.fb_display_refresh_rate = supported_exts.fb_display_refresh_rate;
        exts.ext_user_presence = supported_exts.ext_user_presence;

        if supported_exts
            .other
            .contains(&XR_VALVE_FRAME_CONTROLLER_INTERACTION.to_vec())
        {
            exts.other
                .push(XR_VALVE_FRAME_CONTROLLER_INTERACTION.to_vec());
        }

        // Extension that enables simple full body tracking support via generic tracked devices.
        // Available only in the Monado OpenXR runtime.
        #[cfg(feature = "monado")]
        if supported_exts
            .other
            .contains(&[XR_MNDX_XDEV_SPACE_EXTENSION_NAME.as_bytes(), b"\0"].concat())
        {
            exts.other
                .push([XR_MNDX_XDEV_SPACE_EXTENSION_NAME.as_bytes(), b"\0"].concat());
        }

        let instance = entry
            .create_instance(
                &xr::ApplicationInfo {
                    application_name: get_app_name().as_deref().unwrap_or("XRizer"),
                    application_version: 0,
                    engine_name: "XRizer",
                    engine_version: make_version(),
                    ..Default::default()
                },
                &exts,
                &[],
                &platform_info,
            )
            .map_err(InitError::InstanceCreationFailed)?;

        let system_id = instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(InitError::SystemCreationFailed)?;
        let supports_user_presence =
            exts.ext_user_presence && supports_user_presence(&instance, system_id);

        let probed_profiles =
            crate::input::probe_interaction_profiles(&instance, system_id, &exts, application_type);

        let session_data = SessionReadGuard(RwLock::new(ManuallyDrop::new(
            SessionData::new(
                &instance,
                system_id,
                vr::ETrackingUniverseOrigin::Standing,
                None,
                headless,
            )?
            .0,
        )));

        // The spec guarantees at least 16.
        let max_layer_count = instance
            .system_properties(system_id)
            .map_or(16, |p| p.graphics_properties.max_layer_count as usize);

        Ok(Self {
            _entry: entry,
            instance,
            system_id,
            session_data,
            display_time: AtomicXrTime(1.into()), // This will get replaced on the first WaitGetPoses
            display_period_nanos: 11111111.into(), // This will get replaced on the first WaitGetPoses
            enabled_extensions: exts,
            probed_profiles,
            application_type,
            headless,
            exit_requested: false.into(),
            exiting: false.into(),
            lost: false.into(),
            session_creation_failed: false.into(),
            max_layer_count,
            logged_errors: Mutex::default(),
            supports_user_presence,
            user_present: true.into(),
            input_focus_captured: false.into(),
            pending_recenter: parking_lot::Mutex::new(None),
            stage_moved: false.into(),
            frame_updates: Default::default(),
            input: injector.inject(),
            compositor: injector.inject(),
            system: injector.inject(),
        })
    }

    /// Like in SteamVR, Background, Overlay and Utility apps can't render. They get headless
    /// sessions where available, and a graphics session for them would take the display from the
    /// app that renders.
    pub fn can_render(&self) -> bool {
        !matches!(
            self.application_type,
            vr::EVRApplicationType::Background
                | vr::EVRApplicationType::Overlay
                | vr::EVRApplicationType::Utility
        )
    }

    /// Marks WaitGetPoses' per-frame update (event polling, action sync), which keeps
    /// [Self::pump] out of the way while the app runs a frame loop.
    pub fn frame_update(&self) -> FrameUpdateGuard<'_> {
        FrameUpdateGuard(self.frame_updates.lock())
    }

    /// Events are polled and actions synced in WaitGetPoses, which apps that don't render
    /// (Background/Overlay/Utility apps, tools) never call - so they would never see controllers
    /// or Quit. This runs that update from the calls such apps do make instead, at most once per
    /// display period and only while no frame loop is running. Must be called without holding any
    /// session or device guards.
    pub fn pump(&self) {
        let Some(mut updates) = self.frame_updates.try_lock() else {
            return;
        };
        let period = self.display_period();
        let within = |time: Option<Instant>, duration| time.is_some_and(|t| t.elapsed() < duration);
        if within(updates.last_frame, FRAME_LOOP_TIMEOUT) || within(updates.last_pump, period) {
            return;
        }
        updates.last_pump = Some(Instant::now());

        self.poll_events();
        if let Some(input) = self.input.get() {
            input.frame_start_update();
        }
    }

    pub fn poll_events(&self) {
        if self.is_lost() {
            return;
        }
        let data = self.session_data.get();
        let session = data.session.as_raw();
        let state = self.poll_events_impl(&data);
        drop(data);
        if let Some(state) = state {
            let mut data = self.session_data.0.write();
            // Apps poll from several threads, so another one may have restarted the session in
            // between. The state is the old session's then.
            if data.session.as_raw() == session {
                data.state = state;
            }
        }
        if self.is_lost() {
            return;
        }
        if self.stage_moved.swap(false, Ordering::Relaxed) {
            // The runtime moved the stage, which replaces any standing zero pose the app set.
            self.move_origin(
                &mut self.session_data.0.write(),
                vr::ETrackingUniverseOrigin::Standing,
                xr::Posef::IDENTITY,
            );
            self.push_event(
                vr::EVREventType::ChaperoneUniverseHasChanged,
                Default::default(),
            );
        }
        self.apply_pending_recenter(*RECENTER_STANDING);
    }

    fn poll_events_impl(&self, session_data: &SessionData) -> Option<xr::SessionState> {
        let mut buf = xr::EventDataBuffer::new();
        let mut state = None;
        // Events for an older session (i.e. the probe session or a session we already replaced)
        // are stale.
        let is_stale = |session: xr::sys::Session, what: &dyn std::fmt::Debug| {
            let stale = session != session_data.session.as_raw();
            if stale {
                debug!("ignoring {what:?} for stale session");
            }
            stale
        };
        while let Some(event) = self
            .check("xrPollEvent", self.instance.poll_event(&mut buf))
            .flatten()
        {
            match event {
                xr::Event::SessionStateChanged(event) => {
                    if is_stale(event.session(), &event.state()) {
                        continue;
                    }
                    let previous = state.unwrap_or(session_data.state);
                    state = Some(event.state());
                    info!("OpenXR session state changed: {:?}", event.state());
                    self.update_input_focus(previous, event.state());
                    let exit_requested = self.exit_requested.load(Ordering::Relaxed);
                    match event.state() {
                        xr::SessionState::STOPPING if exit_requested => {
                            debug!("session stopping at our request, not pushing Quit");
                        }
                        xr::SessionState::STOPPING => self.push_quit(false),
                        xr::SessionState::LOSS_PENDING => self.mark_lost("session loss pending"),
                        // We only end the session ourselves, so the runtime ended it.
                        xr::SessionState::EXITING if !exit_requested => {
                            self.mark_lost("session exiting")
                        }
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => self.push_quit(true),
                xr::Event::InteractionProfileChanged(_) => {
                    if let Some(input) = self.input.get() {
                        input.interaction_profile_changed(session_data);
                    }
                }
                xr::Event::ReferenceSpaceChangePending(event) => {
                    let ty = event.reference_space_type();
                    if is_stale(event.session(), &ty) {
                        continue;
                    }
                    info!("Runtime is moving reference space {ty:?}");
                    // Kept until poll_events acts on it: ending the session for a restart polls
                    // too, and the move carries over to the new session.
                    match ty {
                        xr::ReferenceSpaceType::LOCAL => {
                            *self.pending_recenter.lock() = Some(event.change_time())
                        }
                        xr::ReferenceSpaceType::STAGE => {
                            self.stage_moved.store(true, Ordering::Relaxed)
                        }
                        _ => {}
                    }
                }
                xr::Event::UserPresenceChangedEXT(event) => {
                    let present = event.is_user_present();
                    if is_stale(event.session(), &present) {
                        continue;
                    }
                    info!("User present: {present}");
                    if self.user_present.swap(present, Ordering::Relaxed) != present {
                        self.push_event(
                            if present {
                                vr::EVREventType::TrackedDeviceUserInteractionStarted
                            } else {
                                vr::EVREventType::TrackedDeviceUserInteractionEnded
                            },
                            Default::default(),
                        );
                    }
                }
                _ => {
                    info!("unknown event");
                }
            }
        }

        state
    }

    /// Mirrors SteamVR's dashboard taking input focus when the runtime does (e.g. its system menu
    /// opens), and handing it back.
    fn update_input_focus(&self, previous: xr::SessionState, state: xr::SessionState) {
        let captured = match state {
            xr::SessionState::FOCUSED => false,
            // We also lose focus while ending the session ourselves, that's not the runtime.
            xr::SessionState::VISIBLE
                if previous == xr::SessionState::FOCUSED
                    && !self.exit_requested.load(Ordering::Relaxed) =>
            {
                true
            }
            _ => return,
        };
        // Only release what we captured, so the new session after a restart doesn't unpause a
        // game that never paused.
        if self.input_focus_captured.swap(captured, Ordering::Relaxed) == captured {
            return;
        }
        // The SteamVR Unity plugin only reacts to a capture with oldPid 0 and a release with pid
        // 0. u32::MAX stands in for the runtime.
        let mut data = vr::VREvent_Data_t::default();
        let ty = if captured {
            data.process.pid = u32::MAX;
            vr::EVREventType::InputFocusCaptured
        } else {
            data.process.oldPid = u32::MAX;
            vr::EVREventType::InputFocusReleased
        };
        self.push_event(ty, data);
    }

    /// Whether the runtime holds the input focus, i.e. the app shouldn't expect input.
    pub fn input_focus_captured(&self) -> bool {
        self.input_focus_captured.load(Ordering::Relaxed)
    }

    /// Whether the user is wearing the headset. Always true without XR_EXT_user_presence.
    pub fn user_present(&self) -> bool {
        self.user_present.load(Ordering::Relaxed)
    }

    /// Pushes a system event that isn't about a particular device.
    fn push_event(&self, ty: vr::EVREventType, data: vr::VREvent_Data_t) {
        if let Some(system) = self.system.get() {
            system.push_event(vr::VREvent_t {
                eventType: ty.0,
                trackedDeviceIndex: vr::k_unTrackedDeviceIndex_Hmd,
                eventAgeSeconds: 0.0,
                data,
            });
        }
    }

    /// A runtime recenter of LOCAL is the equivalent of the user resetting their seated position
    /// in the SteamVR dashboard: the seated zero becomes the current head pose, replacing what
    /// the app set. The head pose is only measured once the new LOCAL is in effect.
    fn apply_pending_recenter(&self, recenter_standing: bool) {
        let time = {
            let mut pending = self.pending_recenter.lock();
            let Some(change_time) = *pending else {
                return;
            };
            let time = self.get_display_time();
            if change_time.as_nanos() > time.as_nanos() {
                return;
            }
            *pending = None;
            time
        };

        let mut origins = vec![vr::ETrackingUniverseOrigin::Seated];
        if recenter_standing {
            origins.push(vr::ETrackingUniverseOrigin::Standing);
        }
        let mut session = self.session_data.0.write();
        for origin in origins {
            let zero = self
                .check("xrLocateSpace", session.head_zero_pose(origin, time))
                .flatten()
                .unwrap_or_else(|| {
                    warn!(
                        "Couldn't locate the headset, resetting {origin:?} zero pose to the runtime's"
                    );
                    xr::Posef::IDENTITY
                });
            self.move_origin(&mut session, origin, zero);
        }
        drop(session);

        let mut data = vr::VREvent_Data_t::default();
        data.seatedZeroPoseReset.bResetBySystemMenu = true;
        self.push_event(vr::EVREventType::SeatedZeroPoseReset, data);
        if recenter_standing {
            self.push_event(vr::EVREventType::StandingZeroPoseReset, Default::default());
        }
    }

    /// Whether the runtime lost the session or instance.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    /// Passes through a successful result. A failure is logged, and if it means the runtime lost
    /// the session or instance, the session is marked lost and the application told to quit.
    pub fn check<T>(&self, call: &'static str, result: xr::Result<T>) -> Option<T> {
        result.map_err(|e| self.handle_error(call, e)).ok()
    }

    /// See [Self::check].
    pub fn handle_error(&self, call: &'static str, error: xr::sys::Result) {
        match error {
            xr::sys::Result::ERROR_SESSION_LOST | xr::sys::Result::ERROR_INSTANCE_LOST => {
                self.mark_lost(format_args!("{call} failed: {error}"))
            }
            _ if self
                .logged_errors
                .lock()
                .unwrap()
                .insert((call, error.into_raw())) =>
            {
                warn!("{call} failed: {error}")
            }
            _ => debug!("{call} failed: {error}"),
        }
    }

    fn mark_lost(&self, reason: impl std::fmt::Display) {
        if !self.lost.swap(true, Ordering::Relaxed) {
            error!("OpenXR session lost ({reason}), asking the application to quit");
        }
        self.push_quit(true);
    }

    /// Sends the application VREvent_Quit, at most once.
    fn push_quit(&self, connection_lost: bool) {
        if self.exiting.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(system) = self.system.get() else {
            return;
        };
        debug!("pushing Quit event");
        system.push_event(vr::VREvent_t {
            eventType: vr::EVREventType::Quit.0,
            trackedDeviceIndex: vr::k_unTrackedDeviceIndexInvalid,
            eventAgeSeconds: 0.0,
            data: vr::VREvent_Data_t {
                process: vr::VREvent_Process_t {
                    pid: u32::MAX,
                    oldPid: u32::MAX,
                    bForced: false,
                    bConnectionLost: connection_lost,
                },
            },
        });
    }

    /// Why [Self::restart_session] would refuse to start a new session, if it would.
    pub fn restart_blocked(&self) -> Option<RestartError> {
        if self.exiting.load(Ordering::Relaxed) {
            Some(RestartError::Exiting)
        } else if self.session_creation_failed.load(Ordering::Relaxed) {
            Some(RestartError::CreationFailed)
        } else {
            None
        }
    }

    pub fn restart_session(&self) -> Result<(), RestartError> {
        let mut session_guard = self.session_data.0.write();
        if let Some(e) = self.restart_blocked() {
            debug!("not restarting session: {e:?}");
            return Err(e);
        }
        self.end_session(&mut session_guard);
        // A session lost while ending it is kept, since every call on it fails anyway.
        if self.is_lost() {
            return Err(RestartError::Exiting);
        }

        let origin = session_guard.current_origin;
        let offsets = session_guard.origin_offsets();
        // Headless sessions have no graphics to carry over, and headless apps may never have
        // requested a compositor (e.g. when an action manifest replaces legacy actions).
        let comp = (!self.headless).then(|| {
            self.compositor
                .get()
                .expect("Session is being restarted, but compositor has not been set up!")
        });

        let info = comp
            .as_ref()
            .map(|comp| comp.get_session_create_info(std::mem::take(&mut session_guard.comp_data)));

        // We need to destroy the old session before creating the new one.
        let _ = unsafe { ManuallyDrop::take(&mut *session_guard) };

        let (session, result) = match SessionData::new(
            &self.instance,
            self.system_id,
            origin,
            info.as_ref(),
            self.headless,
        ) {
            Ok((mut session, waiter, stream)) => {
                // A runtime recenter seen while ending the session replaces these on the next
                // poll.
                for (origin, offset) in offsets {
                    self.check(
                        "xrCreateReferenceSpace",
                        session.set_origin_offset(origin, offset),
                    );
                }
                if let Some(comp) = comp {
                    comp.post_session_restart(&session, waiter, stream);
                }
                (session, Ok(()))
            }
            Err(e) => {
                error!("Failed to create new session ({e:?}), going back to a temporary session");
                self.handle_error("Creating session", e.result());
                self.session_creation_failed.store(true, Ordering::Relaxed);
                // Without a session in the guard there is nothing left to run on.
                let mut session =
                    SessionData::new(&self.instance, self.system_id, origin, None, self.headless)
                        .unwrap_or_else(|e| {
                            self.handle_error("Recreating temporary session", e.result());
                            if self.is_lost() {
                                error!("OpenXR runtime is gone, exiting");
                                // Plain exit would run the app's exit handlers, which may call
                                // back into us while we hold the session lock.
                                unsafe { libc::_exit(1) }
                            }
                            panic!("Failed to recreate temporary session: {e:?}")
                        })
                        .0;
                for (origin, offset) in offsets {
                    self.check(
                        "xrCreateReferenceSpace",
                        session.set_origin_offset(origin, offset),
                    );
                }
                (session, Err(RestartError::CreationFailed))
            }
        };

        if let Some(input) = self.input.get() {
            input.post_session_restart(&session);
        }

        *session_guard = ManuallyDrop::new(session);
        result
    }

    pub fn set_tracking_space(&self, space: vr::ETrackingUniverseOrigin) {
        if !matches!(
            space,
            vr::ETrackingUniverseOrigin::Seated
                | vr::ETrackingUniverseOrigin::Standing
                | vr::ETrackingUniverseOrigin::RawAndUncalibrated
        ) {
            crate::warn_once!("Ignoring unknown tracking universe {:?}", space);
            return;
        }
        // The SteamVR Unity plugin sets the same space every frame. Don't make it wait for the
        // write lock, which takes up to a frame while WaitGetPoses holds a read across xrWaitFrame.
        if self.session_data.get().current_origin != space {
            self.session_data.0.write().current_origin = space;
        }
    }

    pub fn get_tracking_space(&self) -> vr::ETrackingUniverseOrigin {
        self.session_data.get().current_origin
    }

    /// The time poses for the current frame are predicted for. Only the compositor's frame loop
    /// advances the display time, which non-rendering (e.g. headless) apps never run and others
    /// pause (e.g. while loading), so without a recent frame this is the current time.
    pub fn get_display_time(&self) -> xr::Time {
        let display_time = self.display_time.get();
        match self.now() {
            Some(now)
                if now.as_nanos() - display_time.as_nanos()
                    > FRAME_LOOP_TIMEOUT.as_nanos() as i64 =>
            {
                now
            }
            _ => display_time,
        }
    }

    /// The predicted display time of the last frame the compositor waited for.
    pub fn waited_display_time(&self) -> xr::Time {
        self.display_time.get()
    }

    /// The display period of the last frame the compositor waited for, or 90 Hz before that.
    pub fn display_period(&self) -> Duration {
        Duration::from_nanos(
            self.display_period_nanos
                .load(Ordering::Relaxed)
                .try_into()
                .unwrap_or(0),
        )
    }

    /// Pretends the compositor waited for a frame displayed at `time`.
    #[cfg(test)]
    pub fn set_display_time(&self, time: xr::Time) {
        self.display_time.set(time);
    }

    /// Records the timing of a frame the compositor waited for.
    pub fn set_frame_timing(&self, display_time: xr::Time, display_period_nanos: i64) {
        self.display_time.set(display_time);
        self.display_period_nanos
            .store(display_period_nanos, Ordering::Relaxed);
    }

    pub fn reset_tracking_space(&self, origin: vr::ETrackingUniverseOrigin) {
        if self.is_lost() {
            return;
        }
        match origin {
            vr::ETrackingUniverseOrigin::Seated | vr::ETrackingUniverseOrigin::Standing => {}
            vr::ETrackingUniverseOrigin::RawAndUncalibrated => {
                // RawAndUncalibrated has no calibration to reset
                return;
            }
            _ => {
                crate::warn_once!("Not resetting unknown tracking universe {:?}", origin);
                return;
            }
        }
        let mut session = self.session_data.0.write();
        let zero = session.head_zero_pose(origin, self.get_display_time());
        match self.check("xrLocateSpace", zero).flatten() {
            Some(zero) => self.move_origin(&mut session, origin, zero),
            None => warn!("Couldn't locate the headset, not resetting {origin:?} zero pose"),
        }
    }

    /// Moves `origin`'s zero pose to `offset`, a pose in its reference space.
    fn move_origin(
        &self,
        session: &mut SessionData,
        origin: vr::ETrackingUniverseOrigin,
        offset: xr::Posef,
    ) {
        self.check(
            "xrCreateReferenceSpace",
            session.set_origin_offset(origin, offset),
        );
        // The projection layer goes out in the moved space, so views located in the old one
        // would put the frame where the app didn't render it.
        if let Some(system) = self.system.get() {
            system.clear_views();
        }
    }

    /// The pose of `origin`'s zero in the `base` universe now, if the runtime knows how the two
    /// relate.
    pub fn locate_origin(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        base: vr::ETrackingUniverseOrigin,
    ) -> Option<xr::Posef> {
        if self.is_lost() {
            return None;
        }
        let located = self
            .session_data
            .get()
            .locate_origin(origin, base, self.get_display_time());
        self.check("xrLocateSpace", located).flatten()
    }

    pub fn get_refresh_rate(&self) -> f32 {
        let get_fallback_rate = || {
            Duration::from_nanos(
                self.display_period_nanos
                    .load(Ordering::Relaxed)
                    .try_into()
                    .unwrap(),
            )
            .as_secs_f32()
            .recip()
        };
        if !self.enabled_extensions.fb_display_refresh_rate {
            return get_fallback_rate();
        }

        self.session_data
            .get()
            .session
            .get_display_refresh_rate()
            .unwrap_or_else(|_| get_fallback_rate())
    }

    /// Time since the most recent vsync and a running vsync count, extrapolated from the last
    /// predicted display time and period. `None` if the current time can't be queried.
    pub fn vsync_timing(&self) -> Option<(Duration, u64)> {
        let now = self.now()?;
        vsync_phase(
            now.as_nanos(),
            self.display_time.get().as_nanos(),
            self.display_period_nanos.load(Ordering::Relaxed),
        )
    }

    fn end_session(&self, session_data: &mut SessionData) {
        if self.is_lost() {
            debug!("not ending lost session");
            return;
        }
        self.exit_requested.store(true, Ordering::Relaxed);
        let mut state = session_data.state;
        if state != xr::SessionState::STOPPING {
            match session_data.session.request_exit() {
                Ok(()) => {
                    state = self.wait_for_state(session_data, state, xr::SessionState::STOPPING)
                }
                Err(e) => warn!("xrRequestExitSession failed: {e}"),
            }
        }
        #[cfg(test)]
        if let Some(comp) = self.compositor.get() {
            comp.on_restart();
        }
        match session_data.session.end() {
            Ok(_) => {
                self.wait_for_state(session_data, state, xr::SessionState::EXITING);
            }
            Err(e) => warn!("xrEndSession failed: {e}"),
        }
        self.exit_requested.store(false, Ordering::Relaxed);
    }

    /// Polls events until the session reaches `target`, giving up after [SESSION_STATE_TIMEOUT].
    fn wait_for_state(
        &self,
        session_data: &SessionData,
        mut state: xr::SessionState,
        target: xr::SessionState,
    ) -> xr::SessionState {
        let start = Instant::now();
        while state != target && !self.is_lost() {
            if start.elapsed() >= SESSION_STATE_TIMEOUT {
                warn!("timed out waiting for session state {target:?} (current: {state:?})");
                break;
            }
            match self.poll_events_impl(session_data) {
                Some(s) => state = s,
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        state
    }

    /// The current time, if the runtime can convert it (`Instance::now` panics without
    /// XR_KHR_convert_timespec_time).
    pub fn now(&self) -> Option<xr::Time> {
        if !self.enabled_extensions.khr_convert_timespec_time {
            return None;
        }
        self.instance.now().ok()
    }

    /// The runtime's recommended image size for each eye.
    pub fn recommended_eye_size(&self) -> (u32, u32) {
        let views = self.instance.enumerate_view_configuration_views(
            self.system_id,
            xr::ViewConfigurationType::PRIMARY_STEREO,
        );
        match self
            .check("xrEnumerateViewConfigurationViews", views)
            .as_deref()
        {
            Some([view, ..]) => (
                view.recommended_image_rect_width,
                view.recommended_image_rect_height,
            ),
            other => {
                if other.is_some() {
                    crate::warn_once!("The runtime reported no views");
                }
                // Apps can't handle failure here, and a zero size render target won't do either.
                (1440, 1600)
            }
        }
    }

    pub fn time_from_now(&self, seconds: f32) -> xr::Time {
        offset_time(
            self.now().unwrap_or_else(|| self.display_time.get()),
            seconds,
        )
    }
}

/// Opt-in: a runtime recenter (e.g. a long press of the Quest system button) also recenters the
/// standing universe, like SteamVR does for Quest headsets through Virtual Desktop. Off by
/// default, because on a calibrated room setup the stage is the play area and shouldn't move.
static RECENTER_STANDING: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    crate::quirks::env_override("XRIZER_RECENTER_STANDING").unwrap_or(false)
});

fn supports_user_presence(instance: &xr::Instance, system_id: xr::SystemId) -> bool {
    let mut presence = xr::sys::SystemUserPresencePropertiesEXT {
        ty: xr::sys::SystemUserPresencePropertiesEXT::TYPE,
        next: std::ptr::null_mut(),
        supports_user_presence: false.into(),
    };
    let mut properties = xr::sys::SystemProperties {
        ty: xr::sys::SystemProperties::TYPE,
        next: (&raw mut presence).cast(),
        // Zeroed like the openxr crate does for these out structs.
        ..unsafe { std::mem::zeroed() }
    };
    let result = unsafe {
        (instance.fp().get_system_properties)(instance.as_raw(), system_id, &mut properties)
    };
    if result != xr::sys::Result::SUCCESS {
        warn!("Couldn't query user presence support: {result}");
        return false;
    }
    presence.supports_user_presence.into()
}

#[derive(Default)]
struct FrameUpdates {
    /// When WaitGetPoses last ran.
    last_frame: Option<Instant>,
    /// When [OpenXrData::pump] last ran.
    last_pump: Option<Instant>,
}

pub struct FrameUpdateGuard<'a>(parking_lot::MutexGuard<'a, FrameUpdates>);

impl FrameUpdateGuard<'_> {
    /// Sleeps until `period` has passed since the last WaitGetPoses returned.
    pub fn pace(&self, period: Duration) {
        if let Some(last) = self.0.last_frame {
            std::thread::sleep(period.saturating_sub(last.elapsed()));
        }
    }
}

impl Drop for FrameUpdateGuard<'_> {
    fn drop(&mut self) {
        self.0.last_frame = Some(Instant::now());
    }
}

pub struct AtomicXrTime(AtomicI64);

impl AtomicXrTime {
    #[inline]
    pub fn set(&self, time: xr::Time) {
        self.0.store(time.as_nanos(), Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self) -> xr::Time {
        xr::Time::from_nanos(self.0.load(Ordering::Relaxed))
    }
}

pub type SessionGuard<'a> = RwLockReadGuard<'a, ManuallyDrop<SessionData>>;

pub struct SessionReadGuard(RwLock<ManuallyDrop<SessionData>>);
impl SessionReadGuard {
    /// Reads are recursive: plenty of paths call this while the same thread already holds a guard.
    /// A plain read would block behind a writer queued in between, and the writer waits for the
    /// outer guard, so both threads would hang.
    pub fn get(&self) -> SessionGuard<'_> {
        self.0.read_recursive()
    }
}

pub struct Session<G: xr::Graphics> {
    session: xr::Session<G>,
    swapchain_formats: Vec<G::Format>,
}
supported_apis_enum!(pub enum GraphicalSession: Session);
impl std::fmt::Display for GraphicalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphicalSession::Vulkan(_) => f.write_str("GraphicalSession::Vulkan"),
            GraphicalSession::OpenGL(_) => f.write_str("GraphicalSession::OpenGL"),
            GraphicalSession::Headless(_) => f.write_str("GraphicalSession::Headless"),
            #[cfg(test)]
            GraphicalSession::Fake(_) => f.write_str("GraphicalSession::Fake"),
        }
    }
}
supported_apis_enum!(pub enum FrameStream: xr::FrameStream);

// Implementing From results in a "conflicting implementations" error: https://github.com/rust-lang/rust/issues/85576
#[repr(transparent)]
#[derive(Deref)]
pub struct CreateInfo<G: xr::Graphics>(pub G::SessionCreateInfo);
supported_apis_enum!(pub enum SessionCreateInfo: CreateInfo);

impl SessionCreateInfo {
    pub fn from_info<G: xr::Graphics>(info: G::SessionCreateInfo) -> Self
    where
        Self: From<CreateInfo<G>>,
    {
        Self::from(CreateInfo(info))
    }
}

/// Apps pass negative seconds for past poses, and occasionally garbage. Runtimes extrapolate as
/// far as they're asked to, so keep it within a second.
fn offset_time(time: xr::Time, seconds: f32) -> xr::Time {
    let offset = if seconds.is_finite() {
        (seconds.clamp(-1.0, 1.0) as f64 * 1e9) as i64
    } else {
        0
    };
    xr::Time::from_nanos(time.as_nanos().saturating_add(offset))
}

/// Vsyncs are assumed at `display_time + k * period`. Returns the time since the last one at or
/// before `now`, and that vsync's index counted from time zero.
fn vsync_phase(now: i64, display_time: i64, period: i64) -> Option<(Duration, u64)> {
    if period <= 0 {
        return None;
    }
    let since = (now - display_time).rem_euclid(period);
    let count = (now - since).div_euclid(period);
    Some((Duration::from_nanos(since as u64), count.max(0) as u64))
}

pub struct SessionData {
    pub session: xr::Session<xr::AnyGraphics>,
    session_graphics: GraphicalSession,
    pub state: xr::SessionState,
    pub view_space: xr::Space,
    // The "reference" space is always equivalent to the reference space with an identity offset.
    // The "adjusted" space may have an offset, set by reset_tracking_space.
    // The adjusted spaces should be used for locating things - the reference spaces are only
    // needed for reset_tracking_space
    local_space_reference: xr::Space,
    local_space_adjusted: xr::Space,
    stage_space_reference: xr::Space,
    stage_space_adjusted: xr::Space,
    /// The pose the adjusted spaces were created with, in their reference space.
    local_offset: xr::Posef,
    stage_offset: xr::Posef,
    pub current_origin: vr::ETrackingUniverseOrigin,

    pub input_data: InputSessionData,
    pub comp_data: crate::compositor::CompositorSessionData,
    pub overlay_data: crate::overlay::OverlaySessionData,
    /// OpenXR requires graphics information before creating a session, but OpenVR clients don't
    /// have to provide that information until they actually submit a frame. Yet, we need some
    /// information only available behind a session (i.e., calling xrLocateViews for
    /// GetProjectionMatrix), so we will create a session with fake graphics info to appease OpenXR,
    /// that will be replaced with a real one after the application calls IVRSystem::Submit.
    /// When we're using the real session, this will be None.
    /// Note that it also important that this comes after all members which internally use a xr::Session
    /// \- structs are dropped in declaration order, and if we drop our temporary Vulkan data
    /// before the session, the runtime will likely be very unhappy.
    temp_vulkan: Option<VulkanData>,
    #[cfg(feature = "monado")]
    pub monado: Option<SafeMonado>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartError {
    /// The runtime stopped or lost the session and the application was told to quit, so no new
    /// session is started.
    Exiting,
    /// Creating the new session failed, so we went back to a temporary session.
    CreationFailed,
}

#[derive(Debug)]
#[allow(dead_code)] // The state isn't used, but it's printed
#[allow(clippy::enum_variant_names)]
pub enum SessionCreationError {
    SessionCreationFailed(xr::sys::Result),
    PollEventFailed(xr::sys::Result),
    BeginSessionFailed(xr::sys::Result),
    /// The runtime lost or ended the session before it became ready.
    SessionLost(xr::SessionState),
}

impl SessionCreationError {
    fn result(&self) -> xr::sys::Result {
        match *self {
            Self::SessionCreationFailed(e)
            | Self::PollEventFailed(e)
            | Self::BeginSessionFailed(e) => e,
            Self::SessionLost(_) => xr::sys::Result::ERROR_SESSION_LOST,
        }
    }
}

impl SessionData {
    pub(crate) fn new(
        instance: &xr::Instance,
        system_id: xr::SystemId,
        current_origin: vr::ETrackingUniverseOrigin,
        create_info: Option<&SessionCreateInfo>,
        headless: bool,
    ) -> Result<(Self, xr::FrameWaiter, FrameStream), SessionCreationError> {
        let info;
        let (temp_vulkan, info) = if headless {
            info = SessionCreateInfo::Headless(CreateInfo(xr::headless::SessionCreateInfo {}));
            (None, &info)
        } else if let Some(info) = create_info {
            if let SessionCreateInfo::Vulkan(info) = info {
                // Monado seems to (incorrectly) give validation errors unless we call this.
                let expected_pd = vk::PhysicalDevice::from_raw(
                    unsafe { instance.vulkan_graphics_device(system_id, info.instance) }
                        .map_err(SessionCreationError::SessionCreationFailed)?
                        as _,
                );
                let pd = vk::PhysicalDevice::from_raw(info.physical_device as _);

                if expected_pd != pd {
                    let inst = unsafe {
                        ash::Instance::load(
                            ash::Entry::load().unwrap().static_fn(),
                            vk::Instance::from_raw(info.instance as _),
                        )
                    };
                    let get_pd_name = |pd| {
                        let pd_properties = unsafe { inst.get_physical_device_properties(pd) };
                        pd_properties
                            .device_name_as_c_str()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|_| "<unknown>".to_string())
                    };

                    panic!(
                        "Application is using the wrong GPU (expected {}, got {})",
                        get_pd_name(expected_pd),
                        get_pd_name(pd)
                    );
                }
            }
            (None, info)
        } else {
            let vk = VulkanData::new_temporary(instance, system_id);
            info = SessionCreateInfo::from_info::<xr::Vulkan>(vk.session_create_info());
            (Some(vk), &info)
        };

        #[macros::any_graphics(SessionCreateInfo)]
        fn create_session<G: xr::Graphics>(
            info: &CreateInfo<G>,
            instance: &xr::Instance,
            system_id: xr::SystemId,
        ) -> xr::Result<(
            xr::Session<xr::AnyGraphics>,
            GraphicalSession,
            xr::FrameWaiter,
            FrameStream,
        )>
        where
            GraphicalSession: From<Session<G>>,
            FrameStream: From<xr::FrameStream<G>>,
        {
            info!(
                "Creating OpenXR session with graphics API {}",
                std::any::type_name::<G>()
            );
            // required to call
            instance.graphics_requirements::<G>(system_id)?;

            let (session, w, s) = unsafe { instance.create_session(system_id, &info.0) }?;
            let swapchain_formats = session.enumerate_swapchain_formats()?;
            Ok((
                session.clone().into_any_graphics(),
                Session {
                    session,
                    swapchain_formats,
                }
                .into(),
                w,
                s.into(),
            ))
        }

        let (session, session_graphics, waiter, stream) = info
            .with_any_graphics::<create_session>((instance, system_id))
            .map_err(SessionCreationError::SessionCreationFailed)?;

        info!("New session created!");
        let create_space = |ty| {
            session
                .create_reference_space(ty, xr::Posef::IDENTITY)
                .map_err(SessionCreationError::SessionCreationFailed)
        };
        let view_space = create_space(xr::ReferenceSpaceType::VIEW)?;
        let local_space_reference = create_space(xr::ReferenceSpaceType::LOCAL)?;
        let local_space_adjusted = create_space(xr::ReferenceSpaceType::LOCAL)?;
        let stage_space_reference = create_space(xr::ReferenceSpaceType::STAGE)?;
        let stage_space_adjusted = create_space(xr::ReferenceSpaceType::STAGE)?;

        let mut buf = xr::EventDataBuffer::new();
        // No timeout: the runtime may hold the session until the user puts on the headset.
        loop {
            match instance
                .poll_event(&mut buf)
                .map_err(SessionCreationError::PollEventFailed)?
            {
                Some(xr::Event::SessionStateChanged(state))
                    if state.session() == session.as_raw() =>
                {
                    match state.state() {
                        xr::SessionState::READY => break,
                        s @ (xr::SessionState::LOSS_PENDING | xr::SessionState::EXITING) => {
                            return Err(SessionCreationError::SessionLost(s));
                        }
                        _ => {}
                    }
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }

        info!(
            "OpenXR session state changed: {:?}",
            xr::SessionState::READY
        );
        session
            .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
            .map_err(SessionCreationError::BeginSessionFailed)?;
        info!("Began OpenXR session.");

        let input_data = InputSessionData::new(instance, system_id, &session);

        Ok((
            SessionData {
                temp_vulkan,
                session,
                session_graphics,
                state: xr::SessionState::READY,
                view_space,
                local_space_reference,
                local_space_adjusted,
                stage_space_reference,
                stage_space_adjusted,
                local_offset: xr::Posef::IDENTITY,
                stage_offset: xr::Posef::IDENTITY,
                input_data,
                comp_data: Default::default(),
                overlay_data: Default::default(),
                current_origin,
                #[cfg(feature = "monado")]
                monado: SafeMonado::safe_connect(),
            },
            waiter,
            stream,
        ))
    }

    pub fn create_swapchain<G: xr::Graphics>(
        &self,
        info: &xr::SwapchainCreateInfo<G>,
    ) -> xr::Result<xr::Swapchain<G>>
    where
        for<'a> &'a GraphicalSession: TryInto<&'a Session<G>, Error: std::fmt::Display>,
    {
        (&self.session_graphics)
            .try_into()
            .unwrap_or_else(|e| {
                panic!(
                    "Session was not using API {}: {e}",
                    std::any::type_name::<G>()
                )
            })
            .session
            .create_swapchain(info)
    }

    pub fn check_format<G: GraphicsBackend>(&self, info: &mut xr::SwapchainCreateInfo<G::Api>)
    where
        for<'a> &'a GraphicalSession: TryInto<&'a Session<G::Api>, Error: std::fmt::Display>,
        G::Format: PartialEq,
    {
        let formats: Vec<G::Format> = (&self.session_graphics)
            .try_into()
            .unwrap_or_else(|_| {
                panic!(
                    "Expected session API {}, but current session is using {}!",
                    std::any::type_name::<G>(),
                    self.session_graphics,
                )
            })
            .swapchain_formats
            .clone()
            .into_iter()
            .map(G::from_openxr_format)
            .collect();

        if !formats.contains(&G::from_openxr_format(info.format)) {
            let new_format = formats[0];
            warn!(
                "Requested to init swapchain with unsupported format {:?} - instead using {:?}",
                G::to_nice_format(G::from_openxr_format(info.format)),
                G::to_nice_format(new_format)
            );
            info.format = G::to_openxr_format(new_format);
        }
    }

    pub fn tracking_space(&self) -> &xr::Space {
        self.get_space_for_origin(self.current_origin)
    }

    #[inline]
    pub fn get_space_for_origin(&self, origin: vr::ETrackingUniverseOrigin) -> &xr::Space {
        if origin == vr::ETrackingUniverseOrigin::RawAndUncalibrated {
            crate::warn_unimplemented!("RawAndUncalibrated tracking space");
        }
        self.origin_space(origin)
    }

    #[inline]
    fn origin_space(&self, origin: vr::ETrackingUniverseOrigin) -> &xr::Space {
        match origin {
            vr::ETrackingUniverseOrigin::Seated => &self.local_space_adjusted,
            vr::ETrackingUniverseOrigin::Standing => &self.stage_space_adjusted,
            vr::ETrackingUniverseOrigin::RawAndUncalibrated => &self.stage_space_reference,
            _ => {
                crate::warn_once!("Unknown tracking universe {:?}, using raw", origin);
                &self.stage_space_reference
            }
        }
    }

    /// The pose of `origin`'s zero in the `base` universe, `None` if the runtime doesn't know how
    /// the two relate.
    fn locate_origin(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        base: vr::ETrackingUniverseOrigin,
        time: xr::Time,
    ) -> xr::Result<Option<xr::Posef>> {
        let location = self
            .origin_space(origin)
            .locate(self.origin_space(base), time)?;
        Ok(location
            .location_flags
            .contains(
                xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
            )
            .then_some(location.pose))
    }

    /// The zero pose resetting `origin` sets: the head's position and yaw in `origin`'s reference
    /// space. The standing zero stays on the floor. `None` if the headset isn't tracked.
    fn head_zero_pose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        time: xr::Time,
    ) -> xr::Result<Option<xr::Posef>> {
        let reference = match origin {
            vr::ETrackingUniverseOrigin::Seated => &self.local_space_reference,
            vr::ETrackingUniverseOrigin::Standing => &self.stage_space_reference,
            // RawAndUncalibrated has no zero to reset, nor do unknown universes.
            _ => return Ok(None),
        };
        let location = self.view_space.locate(reference, time)?;
        if !location.location_flags.contains(
            xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
        ) {
            return Ok(None);
        }
        let xr::Posef {
            mut position,
            orientation,
        } = location.pose;

        // Only set the rotation around the y axis
        let (twist, _) = swing_twist_decomposition(
            Quat::from_xyzw(orientation.x, orientation.y, orientation.z, orientation.w),
            Vec3::Y,
        )
        .unwrap_or_else(|| {
            warn!("Couldn't decompose rotation - using identity");
            (Quat::IDENTITY, Quat::IDENTITY)
        });
        if origin == vr::ETrackingUniverseOrigin::Standing {
            position.y = 0.0;
        }

        Ok(Some(xr::Posef {
            position,
            orientation: xr::Quaternionf {
                x: twist.x,
                y: twist.y,
                z: twist.z,
                w: twist.w,
            },
        }))
    }

    /// Moves `origin`'s zero to `offset`, a pose in its reference space.
    fn set_origin_offset(
        &mut self,
        origin: vr::ETrackingUniverseOrigin,
        offset: xr::Posef,
    ) -> xr::Result<()> {
        let (ty, space, stored) = match origin {
            vr::ETrackingUniverseOrigin::Seated => (
                xr::ReferenceSpaceType::LOCAL,
                &mut self.local_space_adjusted,
                &mut self.local_offset,
            ),
            vr::ETrackingUniverseOrigin::Standing => (
                xr::ReferenceSpaceType::STAGE,
                &mut self.stage_space_adjusted,
                &mut self.stage_offset,
            ),
            _ => return Ok(()),
        };
        *space = self.session.create_reference_space(ty, offset)?;
        *stored = offset;
        self.input_data.forget_poses();
        Ok(())
    }

    fn origin_offsets(&self) -> [(vr::ETrackingUniverseOrigin, xr::Posef); 2] {
        [
            (vr::ETrackingUniverseOrigin::Seated, self.local_offset),
            (vr::ETrackingUniverseOrigin::Standing, self.stage_offset),
        ]
    }

    #[inline]
    pub fn get_space_from_type(&self, ty: xr::ReferenceSpaceType) -> &xr::Space {
        match ty {
            xr::ReferenceSpaceType::VIEW => &self.view_space,
            xr::ReferenceSpaceType::LOCAL => &self.local_space_adjusted,
            xr::ReferenceSpaceType::STAGE => &self.stage_space_adjusted,
            other => panic!("Unsupported reference space type: {other:?}"),
        }
    }

    #[inline]
    pub fn current_origin_as_reference_space(&self) -> xr::ReferenceSpaceType {
        match self.current_origin {
            vr::ETrackingUniverseOrigin::Seated => xr::ReferenceSpaceType::LOCAL,
            vr::ETrackingUniverseOrigin::Standing => xr::ReferenceSpaceType::STAGE,
            // RawAndUncalibrated, set_tracking_space doesn't store other values
            _ => {
                crate::warn_unimplemented!("RawAndUncalibrated tracking space");
                xr::ReferenceSpaceType::STAGE
            }
        }
    }

    /// Returns true if this session is not using a temporary graphics setup.
    #[inline]
    pub fn is_real_session(&self) -> bool {
        self.temp_vulkan.is_none()
    }
}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Hand {
    Left = 1,
    Right,
}

impl TryFrom<vr::ETrackedControllerRole> for Hand {
    type Error = ();
    #[inline]
    fn try_from(value: vr::ETrackedControllerRole) -> Result<Self, Self::Error> {
        match value {
            vr::ETrackedControllerRole::LeftHand => Ok(Hand::Left),
            vr::ETrackedControllerRole::RightHand => Ok(Hand::Right),
            _ => Err(()),
        }
    }
}

impl From<Hand> for vr::ETrackedControllerRole {
    fn from(hand: Hand) -> Self {
        match hand {
            Hand::Left => vr::ETrackedControllerRole::LeftHand,
            Hand::Right => vr::ETrackedControllerRole::RightHand,
        }
    }
}

impl From<Hand> for xr::HandEXT {
    fn from(hand: Hand) -> Self {
        match hand {
            Hand::Left => xr::HandEXT::LEFT,
            Hand::Right => xr::HandEXT::RIGHT,
        }
    }
}

/// Taken from: https://github.com/bitshifter/glam-rs/issues/536
/// Decompose the rotation on to 2 parts.
///
/// 1. Twist - rotation around the "direction" vector
/// 2. Swing - rotation around axis that is perpendicular to "direction" vector
///
/// The rotation can be composed back by
/// `rotation = swing * twist`.
/// Order matters!
///
/// has singularity in case of swing_rotation close to 180 degrees rotation.
/// if the input quaternion is of non-unit length, the outputs are non-unit as well
/// otherwise, outputs are both unit
fn swing_twist_decomposition(rotation: Quat, axis: Vec3) -> Option<(Quat, Quat)> {
    let rotation_axis = rotation.xyz();
    let projection = rotation_axis.project_onto(axis);

    let twist = {
        let maybe_flipped_twist = Quat::from_vec4(projection.extend(rotation.w));
        if rotation_axis.dot(projection) < 0.0 {
            -maybe_flipped_twist
        } else {
            maybe_flipped_twist
        }
    };

    if twist.length_squared() != 0.0 {
        let swing = rotation * twist.conjugate();
        Some((twist.normalize(), swing))
    } else {
        None
    }
}

#[cfg(test)]
pub use tests::FakeCompositor;

#[cfg(test)]
mod tests {
    use super::{
        FrameStream, GraphicsBackend, OpenXrData, SessionCreateInfo, offset_time, vsync_phase,
    };
    use crate::clientcore::Injector;
    use openxr as xr;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    pub struct FakeCompositor {
        backend: crate::graphics_backends::VulkanData,
        barrier: Mutex<Option<Arc<Barrier>>>,
        restart_complete: AtomicBool,
    }
    impl FakeCompositor {
        pub fn new(xr: &OpenXrData<Self>) -> Self {
            Self::new_with_barrier(xr, None)
        }

        fn new_with_barrier(xr: &OpenXrData<Self>, barrier: Option<Arc<Barrier>>) -> Self {
            Self {
                backend: crate::graphics_backends::VulkanData::new_temporary(
                    &xr.instance,
                    xr.system_id,
                ),
                barrier: barrier.into(),
                restart_complete: false.into(),
            }
        }
    }
    impl openvr::InterfaceImpl for FakeCompositor {
        fn get_version(_: &CStr) -> Option<Box<dyn FnOnce(&Arc<Self>) -> *mut std::ffi::c_void>> {
            None
        }
        fn supported_versions() -> &'static [&'static CStr] {
            &[]
        }
    }
    impl super::Compositor for FakeCompositor {
        fn get_session_create_info(
            &self,
            _: crate::compositor::CompositorSessionData,
        ) -> SessionCreateInfo {
            SessionCreateInfo::from_info::<xr::Vulkan>(self.backend.session_create_info())
        }

        fn post_session_restart(
            &self,
            _: &crate::openxr_data::SessionData,
            _: openxr::FrameWaiter,
            _: FrameStream,
        ) {
            self.restart_complete.store(true, Ordering::Relaxed);
        }

        fn on_restart(&self) {
            if let Some(barrier) = self.barrier.lock().unwrap().take() {
                barrier.wait();
            }
        }
    }

    #[test]
    fn vsync_phase_extrapolates_from_display_time() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        let ns = |ms: i64| ms * 1_000_000;
        // 10ms period, a vsync at 1000ms
        assert_eq!(vsync_phase(ns(1003), ns(1000), ns(10)), Some((ms(3), 100)));
        // Display time is usually in the future
        assert_eq!(vsync_phase(ns(1003), ns(1020), ns(10)), Some((ms(3), 100)));
        assert_eq!(vsync_phase(ns(1020), ns(1020), ns(10)), Some((ms(0), 102)));
        assert_eq!(vsync_phase(ns(1029), ns(1020), ns(10)), Some((ms(9), 102)));
        assert_eq!(vsync_phase(ns(1003), ns(1000), 0), None);
    }

    /// Nothing advances the display time while the compositor doesn't wait for frames, which
    /// headless apps never do, so poses must be predicted for the current time rather than the
    /// time the app initialized at or last waited for a frame.
    #[test]
    fn display_time_advances_without_frame_loop() {
        crate::init_logging();
        fakexr::set_headless_supported(true);
        for (app_type, headless) in [
            (openvr::EVRApplicationType::Background, true),
            (openvr::EVRApplicationType::Other, false),
        ] {
            let data = OpenXrData::<FakeCompositor>::new(&Injector::default(), app_type).unwrap();
            assert_eq!(data.headless, headless);

            let first = data.get_display_time().as_nanos();
            std::thread::sleep(Duration::from_millis(2));
            let second = data.get_display_time().as_nanos();
            assert!(second > first, "{app_type:?}: display time did not advance");

            // While frames are waited for, the frame loop drives it...
            let frame_time = xr::Time::from_nanos(second + 1_000_000);
            data.set_frame_timing(frame_time, 1);
            assert_eq!(data.get_display_time(), frame_time);

            // ...until the app stops waiting for them.
            let paused = second - 2 * super::FRAME_LOOP_TIMEOUT.as_nanos() as i64;
            data.set_frame_timing(xr::Time::from_nanos(paused), 1);
            assert!(
                data.get_display_time().as_nanos() >= second,
                "{app_type:?}: display time stuck at the last frame"
            );
        }
        fakexr::set_headless_supported(false);
    }

    #[test]
    fn offset_time_takes_any_seconds() {
        let offset = |seconds| offset_time(xr::Time::from_nanos(1_000_000_000), seconds).as_nanos();
        assert_eq!(offset(0.5), 1_500_000_000);
        // Negative is a time in the past.
        assert_eq!(offset(-0.25), 750_000_000);
        assert_eq!(offset(f32::NAN), 1_000_000_000);
        assert_eq!(offset(f32::INFINITY), 1_000_000_000);
        assert_eq!(offset(f32::NEG_INFINITY), 1_000_000_000);
        assert_eq!(offset(f32::MAX), 2_000_000_000);
        assert_eq!(offset(f32::MIN), 0);
    }

    #[test]
    fn session_restart_contended() {
        crate::init_logging();
        let data = Arc::new(
            OpenXrData::<FakeCompositor>::new(
                &Injector::default(),
                openvr::EVRApplicationType::Other,
            )
            .unwrap(),
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let comp = Arc::new(FakeCompositor::new_with_barrier(
            &data,
            Some(barrier.clone()),
        ));
        data.compositor.set(Arc::downgrade(&comp));

        std::thread::scope(|scope| {
            {
                let barrier = barrier.clone();
                let data = data.clone();
                let comp = comp.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let _unused = data.session_data.get();
                    log::debug!("acquired");
                    assert!(comp.restart_complete.load(Ordering::Relaxed));
                });
            }

            scope.spawn(|| {
                data.restart_session().unwrap();
            });
        });

        drop(data); // Session must be dropped before Vulkan data.
        drop(comp);
    }

    /// A thread that reads the session again while it already holds a guard must not block behind
    /// a writer that queued up in between.
    #[test]
    fn nested_read_with_queued_writer() {
        crate::init_logging();
        let data = Arc::new(
            OpenXrData::<FakeCompositor>::new(
                &Injector::default(),
                openvr::EVRApplicationType::Other,
            )
            .unwrap(),
        );

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reader = {
            let data = data.clone();
            std::thread::spawn(move || {
                let outer = data.session_data.get();
                let writer = {
                    let data = data.clone();
                    std::thread::spawn(move || {
                        data.reset_tracking_space(openvr::ETrackingUniverseOrigin::Seated)
                    })
                };
                // Give the writer time to queue up behind the outer guard.
                std::thread::sleep(Duration::from_millis(100));
                drop(data.session_data.get());
                drop(outer);
                writer.join().unwrap();
                done_tx.send(()).unwrap();
            })
        };

        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("nested session read deadlocked against a queued writer");
        reader.join().unwrap();
    }

    /// Interfaces are constructed under the interface store lock, and restart_session looks
    /// interfaces up in the store while holding the session write lock. So constructing Input must
    /// not wait for the session lock.
    #[test]
    fn input_construction_does_not_wait_for_session() {
        crate::init_logging();
        let data = Arc::new(
            OpenXrData::<FakeCompositor>::new(
                &Injector::default(),
                openvr::EVRApplicationType::Other,
            )
            .unwrap(),
        );

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = data.session_data.0.write();
        let constructor = {
            let data = data.clone();
            std::thread::spawn(move || {
                let input = crate::input::Input::new(data);
                done_tx.send(()).unwrap();
                input
            })
        };
        let done = done_rx.recv_timeout(Duration::from_secs(10));
        drop(writer);
        drop(constructor.join().unwrap());
        done.expect("Input::new waited for the session lock");
    }

    /// The runtime going away (e.g. wivrn-server stopping) right before shutdown makes xrPollEvent
    /// fail. Shutdown must neither panic nor wait for state changes that will never come.
    #[test]
    fn shutdown_after_instance_loss() {
        crate::init_logging();
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();

        fakexr::lose_instance(data.instance.as_raw());
        let start = std::time::Instant::now();
        drop(data);
        assert!(start.elapsed() < super::SESSION_STATE_TIMEOUT);
    }

    fn head_pose(
        data: &OpenXrData<FakeCompositor>,
        origin: openvr::ETrackingUniverseOrigin,
    ) -> xr::Posef {
        let session = data.session_data.get();
        session
            .view_space
            .locate(
                session.get_space_for_origin(origin),
                data.get_display_time(),
            )
            .unwrap()
            .pose
    }

    fn assert_pose_eq(a: xr::Posef, b: xr::Posef) {
        let quat = |q: xr::Quaternionf| glam::Quat::from_xyzw(q.x, q.y, q.z, q.w);
        let vec = |v: xr::Vector3f| glam::Vec3::new(v.x, v.y, v.z);
        assert!(
            vec(a.position).abs_diff_eq(vec(b.position), 1e-5)
                && quat(a.orientation).angle_between(quat(b.orientation)) < 1e-3,
            "{a:?} != {b:?}"
        );
    }

    /// The head pose with a yaw and a pitch, 1.7 m up.
    fn standing_head() -> (xr::Posef, glam::Quat) {
        let pitch = glam::Quat::from_rotation_x(0.3);
        let rot = glam::Quat::from_rotation_y(1.0) * pitch;
        let pose = xr::Posef {
            position: xr::Vector3f {
                x: 0.5,
                y: 1.7,
                z: -2.0,
            },
            orientation: xr::Quaternionf {
                x: rot.x,
                y: rot.y,
                z: rot.z,
                w: rot.w,
            },
        };
        (pose, pitch)
    }

    fn pitch_only(pitch: glam::Quat, position: [f32; 3]) -> xr::Posef {
        xr::Posef {
            position: xr::Vector3f {
                x: position[0],
                y: position[1],
                z: position[2],
            },
            orientation: xr::Quaternionf {
                x: pitch.x,
                y: pitch.y,
                z: pitch.z,
                w: pitch.w,
            },
        }
    }

    #[test]
    fn reset_zero_pose_keeps_standing_floor() {
        use openvr::ETrackingUniverseOrigin as Origin;
        crate::init_logging();
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();
        let (head, pitch) = standing_head();
        let session = data.session_data.get().session.as_raw();
        fakexr::set_reference_space_pose(session, xr::ReferenceSpaceType::VIEW, head);

        data.reset_tracking_space(Origin::Standing);
        assert_pose_eq(
            head_pose(&data, Origin::Standing),
            pitch_only(pitch, [0.0, 1.7, 0.0]),
        );

        data.reset_tracking_space(Origin::Seated);
        assert_pose_eq(
            head_pose(&data, Origin::Seated),
            pitch_only(pitch, [0.0, 0.0, 0.0]),
        );
    }

    /// Zero poses the app set on the temporary session have to carry over to the real one.
    #[test]
    fn zero_poses_survive_session_restart() {
        use openvr::ETrackingUniverseOrigin as Origin;
        crate::init_logging();
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();
        let comp = Arc::new(FakeCompositor::new(&data));
        data.compositor.set(Arc::downgrade(&comp));

        let (head, pitch) = standing_head();
        let session = data.session_data.get().session.as_raw();
        fakexr::set_reference_space_pose(session, xr::ReferenceSpaceType::VIEW, head);
        data.reset_tracking_space(Origin::Standing);
        data.reset_tracking_space(Origin::Seated);

        data.restart_session().unwrap();
        let session = data.session_data.get().session.as_raw();
        fakexr::set_reference_space_pose(session, xr::ReferenceSpaceType::VIEW, head);
        assert_pose_eq(
            head_pose(&data, Origin::Standing),
            pitch_only(pitch, [0.0, 1.7, 0.0]),
        );
        assert_pose_eq(
            head_pose(&data, Origin::Seated),
            pitch_only(pitch, [0.0, 0.0, 0.0]),
        );

        drop(data); // Session must be dropped before Vulkan data.
        drop(comp);
    }

    /// A runtime recenter seen while ending the session replaces the app's zero poses on the new
    /// session, instead of the app's getting carried over.
    #[test]
    fn runtime_recenter_during_session_restart() {
        use openvr::ETrackingUniverseOrigin as Origin;
        crate::init_logging();
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();
        let comp = Arc::new(FakeCompositor::new(&data));
        data.compositor.set(Arc::downgrade(&comp));

        let (head, pitch) = standing_head();
        let session = data.session_data.get().session.as_raw();
        fakexr::set_reference_space_pose(session, xr::ReferenceSpaceType::VIEW, head);
        data.reset_tracking_space(Origin::Standing);
        data.reset_tracking_space(Origin::Seated);
        fakexr::reference_space_change_pending(
            session,
            xr::ReferenceSpaceType::LOCAL,
            xr::Time::from_nanos(0),
        );
        fakexr::reference_space_change_pending(
            session,
            xr::ReferenceSpaceType::STAGE,
            xr::Time::from_nanos(0),
        );

        data.restart_session().unwrap();
        let moved = xr::Posef {
            position: xr::Vector3f {
                x: -1.0,
                y: 1.2,
                z: 0.5,
            },
            ..head
        };
        let session = data.session_data.get().session.as_raw();
        fakexr::set_reference_space_pose(session, xr::ReferenceSpaceType::VIEW, moved);
        data.poll_events();
        assert_pose_eq(
            head_pose(&data, Origin::Seated),
            pitch_only(pitch, [0.0, 0.0, 0.0]),
        );
        assert_pose_eq(head_pose(&data, Origin::Standing), moved);

        drop(data);
        drop(comp);
    }

    /// XRIZER_RECENTER_STANDING: a runtime recenter also moves the standing zero under the head,
    /// keeping it on the floor.
    #[test]
    fn runtime_recenter_can_move_standing_zero() {
        use openvr::ETrackingUniverseOrigin as Origin;
        crate::init_logging();
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();
        let (head, pitch) = standing_head();
        let session = data.session_data.get().session.as_raw();
        fakexr::set_reference_space_pose(session, xr::ReferenceSpaceType::VIEW, head);
        fakexr::reference_space_change_pending(
            session,
            xr::ReferenceSpaceType::LOCAL,
            xr::Time::from_nanos(0),
        );

        data.poll_events_impl(&data.session_data.get());
        data.apply_pending_recenter(true);
        assert_pose_eq(
            head_pose(&data, Origin::Standing),
            pitch_only(pitch, [0.0, 1.7, 0.0]),
        );
        assert_pose_eq(
            head_pose(&data, Origin::Seated),
            pitch_only(pitch, [0.0, 0.0, 0.0]),
        );
    }

    /// Ending the session ourselves isn't the runtime taking input focus, and focus the runtime
    /// took is only handed back once the new session has it. Every change of the flag is an
    /// InputFocusCaptured or InputFocusReleased event.
    #[test]
    fn input_focus_across_session_restart() {
        use xr::SessionState as S;
        crate::init_logging();
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();
        let comp = Arc::new(FakeCompositor::new(&data));
        data.compositor.set(Arc::downgrade(&comp));
        let session = || data.session_data.get().session.as_raw();
        let set_state = |state| {
            fakexr::set_session_state(session(), state);
            data.poll_events();
        };
        let focus = || {
            for state in [S::SYNCHRONIZED, S::VISIBLE, S::FOCUSED] {
                set_state(state);
            }
        };

        focus();
        // Runtimes take focus away when we end the session.
        fakexr::set_session_state(session(), S::VISIBLE);
        data.restart_session().unwrap();
        assert!(!data.input_focus_captured());
        focus();
        assert!(!data.input_focus_captured());

        set_state(S::VISIBLE);
        assert!(data.input_focus_captured());
        data.restart_session().unwrap();
        set_state(S::SYNCHRONIZED);
        set_state(S::VISIBLE);
        assert!(data.input_focus_captured());
        set_state(S::FOCUSED);
        assert!(!data.input_focus_captured());

        drop(data);
        drop(comp);
    }

    #[test]
    fn setting_unchanged_tracking_space_does_not_wait_for_readers() {
        crate::init_logging();
        let data = Arc::new(
            OpenXrData::<FakeCompositor>::new(
                &Injector::default(),
                openvr::EVRApplicationType::Other,
            )
            .unwrap(),
        );
        let origin = data.get_tracking_space();

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let reader = {
            let data = data.clone();
            std::thread::spawn(move || {
                let _guard = data.session_data.get();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
        };
        held_rx.recv().unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let setter = {
            let data = data.clone();
            std::thread::spawn(move || {
                data.set_tracking_space(origin);
                done_tx.send(()).unwrap();
            })
        };
        let done = done_rx.recv_timeout(Duration::from_secs(10));
        release_tx.send(()).unwrap();
        reader.join().unwrap();
        setter.join().unwrap();
        done.expect("SetTrackingSpace waited for a reader");
        assert_eq!(data.get_tracking_space(), origin);
    }

    #[test]
    fn unknown_tracking_universe() {
        let data = OpenXrData::<FakeCompositor>::new(
            &Injector::default(),
            openvr::EVRApplicationType::Other,
        )
        .unwrap();
        let unknown = openvr::ETrackingUniverseOrigin(7);

        data.set_tracking_space(openvr::ETrackingUniverseOrigin::Seated);
        data.set_tracking_space(unknown);
        assert_eq!(
            data.get_tracking_space(),
            openvr::ETrackingUniverseOrigin::Seated
        );

        data.reset_tracking_space(unknown);
        let session = data.session_data.get();
        assert_eq!(
            session.get_space_for_origin(unknown).as_raw(),
            session.stage_space_reference.as_raw()
        );
    }
}
