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
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// How long to wait for the runtime to move the session to a requested state.
const SESSION_STATE_TIMEOUT: Duration = Duration::from_secs(3);

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
    pub display_time: AtomicXrTime,
    pub display_period_nanos: AtomicI64,
    pub enabled_extensions: xr::ExtensionSet,
    /// The interaction profiles that were bound when we probed at init - see
    /// [crate::input::probe_interaction_profiles].
    pub probed_profiles: crate::input::ProbedProfiles,
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
    EnumeratingExtensionsFailed(xr::sys::Result),
    InstanceCreationFailed(xr::sys::Result),
    SystemCreationFailed(xr::sys::Result),
    SessionCreationFailed(SessionCreationError),
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

        #[cfg(all(not(test), feature = "static-openxr"))]
        let entry = xr::Entry::linked(&platform_info).expect("Failed to linked OpenXR loader");

        #[cfg(all(not(test), not(feature = "static-openxr")))]
        let entry = unsafe { xr::Entry::load(&platform_info) }.unwrap_or_else(|e| {
            let hint = if cfg!(target_pointer_width = "32") {
                "this is the 32-bit xrizer, which needs a 32-bit OpenXR loader \
                (e.g. lib32-openxr) - alternatively, build with --features static-openxr"
            } else {
                "is libopenxr-loader installed?"
            };
            panic!("Failed to load OpenXR loader — {hint}: {e:?}");
        });

        #[cfg(test)]
        let entry = unsafe {
            xr::Entry::from_get_instance_proc_addr(fakexr::get_instance_proc_addr, &platform_info)
        }
        .unwrap();

        let supported_exts = entry
            .enumerate_extensions()
            .map_err(InitError::EnumeratingExtensionsFailed)?;

        let headless = match application_type {
            vr::EVRApplicationType::Utility => {
                supported_exts.mnd_headless && supported_exts.khr_convert_timespec_time
            }
            vr::EVRApplicationType::Background => {
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

        let mut exts = xr::ExtensionSet::default();
        if headless {
            exts.mnd_headless = supported_exts.mnd_headless;
            exts.khr_convert_timespec_time = supported_exts.khr_convert_timespec_time;
        }
        exts.khr_vulkan_enable = supported_exts.khr_vulkan_enable;
        exts.khr_opengl_enable = supported_exts.khr_opengl_enable;
        exts.khr_convert_timespec_time = supported_exts.khr_convert_timespec_time;
        exts.ext_hand_tracking = supported_exts.ext_hand_tracking;
        exts.ext_hand_tracking_data_source = supported_exts.ext_hand_tracking_data_source;
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

        if supported_exts
            .other
            .contains(&b"XR_VALVE_frame_controller_interaction".to_vec())
        {
            exts.other
                .push(b"XR_VALVE_frame_controller_interaction".to_vec());
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

        let display_time = if exts.khr_convert_timespec_time {
            instance.now().map(|t| t.as_nanos()).unwrap_or(1i64)
        } else {
            1i64
        };

        Ok(Self {
            _entry: entry,
            instance,
            system_id,
            session_data,
            display_time: AtomicXrTime(display_time.into()), // This will get replaced on the first WaitGetPoses
            display_period_nanos: 11111111.into(), // This will get replaced on the first WaitGetPoses
            enabled_extensions: exts,
            probed_profiles,
            headless,
            exit_requested: false.into(),
            exiting: false.into(),
            lost: false.into(),
            input: injector.inject(),
            compositor: injector.inject(),
            system: injector.inject(),
        })
    }

    pub fn poll_events(&self) {
        if self.is_lost() {
            return;
        }
        let data = self.session_data.get();
        if let Some(state) = self.poll_events_impl(&data) {
            drop(data);
            self.session_data.0.write().state = state;
        }
    }

    fn poll_events_impl(&self, session_data: &SessionData) -> Option<xr::SessionState> {
        let mut buf = xr::EventDataBuffer::new();
        let mut state = None;
        while let Some(event) = self
            .check("xrPollEvent", self.instance.poll_event(&mut buf))
            .flatten()
        {
            match event {
                xr::Event::SessionStateChanged(event) => {
                    // Events for an older session (i.e. the probe session or a session we
                    // already replaced) are stale.
                    if event.session() != session_data.session.as_raw() {
                        debug!(
                            "ignoring state change {:?} for stale session",
                            event.state()
                        );
                        continue;
                    }
                    state = Some(event.state());
                    info!("OpenXR session state changed: {:?}", event.state());
                    let exit_requested = self.exit_requested.load(Ordering::Relaxed);
                    match event.state() {
                        xr::SessionState::STOPPING if exit_requested => {
                            debug!("session stopping at our request, not pushing Quit");
                        }
                        xr::SessionState::STOPPING
                            if cfg!(test) || session_data.temp_vulkan.is_none() =>
                        {
                            self.push_quit(false)
                        }
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
                        input.interaction_profile_changed(session_data, &self.enabled_extensions);
                    }
                }
                _ => {
                    info!("unknown event");
                }
            }
        }

        state
    }

    /// Whether the runtime lost the session or instance.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    /// Passes through a successful result. A failure is logged, and if it means the runtime lost
    /// the session or instance, the session is marked lost and the application told to quit.
    pub fn check<T>(&self, call: &str, result: xr::Result<T>) -> Option<T> {
        result.map_err(|e| self.handle_error(call, e)).ok()
    }

    /// See [Self::check].
    pub fn handle_error(&self, call: &str, error: xr::sys::Result) {
        match error {
            xr::sys::Result::ERROR_SESSION_LOST | xr::sys::Result::ERROR_INSTANCE_LOST => {
                self.mark_lost(format_args!("{call} failed: {error}"))
            }
            _ => warn!("{call} failed: {error}"),
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
            eventType: vr::EVREventType::Quit as u32,
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
        self.exiting
            .load(Ordering::Relaxed)
            .then_some(RestartError::Exiting)
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
        let comp = self
            .compositor
            .get()
            .expect("Session is being restarted, but compositor has not been set up!");

        let info = comp.get_session_create_info(std::mem::take(&mut session_guard.comp_data));

        // We need to destroy the old session before creating the new one.
        let _ = unsafe { ManuallyDrop::take(&mut *session_guard) };

        let (session, waiter, stream) = SessionData::new(
            &self.instance,
            self.system_id,
            origin,
            Some(&info),
            self.headless,
        )
        .expect("Failed to initalize new session");

        comp.post_session_restart(&session, waiter, stream);

        if let Some(input) = self.input.get() {
            input.post_session_restart(&session);
        }

        *session_guard = ManuallyDrop::new(session);
        Ok(())
    }

    pub fn set_tracking_space(&self, space: vr::ETrackingUniverseOrigin) {
        // The SteamVR Unity plugin sets the same space every frame. Don't make it wait for the
        // write lock, which takes up to a frame while WaitGetPoses holds a read across xrWaitFrame.
        if self.session_data.get().current_origin != space {
            self.session_data.0.write().current_origin = space;
        }
    }

    pub fn get_tracking_space(&self) -> vr::ETrackingUniverseOrigin {
        self.session_data.get().current_origin
    }

    pub fn get_display_time(&self) -> xr::Time {
        if self.headless {
            self.instance.now().unwrap()
        } else {
            self.display_time.get()
        }
    }

    pub fn reset_tracking_space(&self, origin: vr::ETrackingUniverseOrigin) {
        let mut guard = self.session_data.0.write();
        let SessionData {
            session,
            view_space,
            local_space_reference,
            local_space_adjusted,
            stage_space_reference,
            stage_space_adjusted,
            ..
        } = &mut **guard;

        let reset_space = |ref_space, adjusted_space: &mut xr::Space, ty| {
            let xr::Posef {
                position,
                orientation,
            } = view_space
                .locate(ref_space, self.get_display_time())
                .unwrap()
                .pose;

            // Only set the rotation around the y axis
            let (twist, _) = swing_twist_decomposition(
                Quat::from_xyzw(orientation.x, orientation.y, orientation.z, orientation.w),
                Vec3::Y,
            )
            .unwrap_or_else(|| {
                warn!("Couldn't decompose rotation - using identity");
                (Quat::IDENTITY, Quat::IDENTITY)
            });

            *adjusted_space = session
                .create_reference_space(
                    ty,
                    xr::Posef {
                        position,
                        orientation: xr::Quaternionf {
                            x: twist.x,
                            y: twist.y,
                            z: twist.z,
                            w: twist.w,
                        },
                    },
                )
                .unwrap();
        };

        match origin {
            vr::ETrackingUniverseOrigin::RawAndUncalibrated => {
                // RawAndUncalibrated has no calibration to reset
            }
            vr::ETrackingUniverseOrigin::Standing => reset_space(
                stage_space_reference,
                stage_space_adjusted,
                xr::ReferenceSpaceType::STAGE,
            ),
            vr::ETrackingUniverseOrigin::Seated => reset_space(
                local_space_reference,
                local_space_adjusted,
                xr::ReferenceSpaceType::LOCAL,
            ),
        };
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
        if !self.enabled_extensions.khr_convert_timespec_time {
            return None;
        }
        let now = self.instance.now().ok()?;
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

    pub fn time_from_now(&self, seconds: f32) -> xr::Time {
        match self.instance.now() {
            Ok(time) => xr::Time::from_nanos(
                time.as_nanos() + std::time::Duration::from_secs_f32(seconds).as_nanos() as i64,
            ),
            _ => self.display_time.get(),
        }
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
}

#[derive(Debug)]
#[allow(dead_code)] // Results aren't used, but they're printed
#[allow(clippy::enum_variant_names)]
pub enum SessionCreationError {
    SessionCreationFailed(xr::sys::Result),
    PollEventFailed(xr::sys::Result),
    BeginSessionFailed(xr::sys::Result),
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
                    unsafe { instance.vulkan_graphics_device(system_id, info.instance) }.unwrap()
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
            let _ = instance.graphics_requirements::<G>(system_id).unwrap();

            unsafe { instance.create_session(system_id, &info.0) }.map(|(session, w, s)| {
                let swapchain_formats = session
                    .enumerate_swapchain_formats()
                    .expect("Couldn't enumerate session swapchain formats!");
                (
                    session.clone().into_any_graphics(),
                    Session {
                        session,
                        swapchain_formats,
                    }
                    .into(),
                    w,
                    s.into(),
                )
            })
        }

        let (session, session_graphics, waiter, stream) = info
            .with_any_graphics::<create_session>((instance, system_id))
            .map_err(SessionCreationError::SessionCreationFailed)?;

        info!("New session created!");
        let view_space = session
            .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
            .unwrap();
        let [local_space_reference, local_space_adjusted] = std::array::from_fn(|_| {
            session
                .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
                .unwrap()
        });
        let [stage_space_reference, stage_space_adjusted] = std::array::from_fn(|_| {
            session
                .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
                .unwrap()
        });

        let mut buf = xr::EventDataBuffer::new();
        // No timeout: the runtime may hold the session until the user puts on the headset.
        loop {
            match instance
                .poll_event(&mut buf)
                .map_err(SessionCreationError::PollEventFailed)?
            {
                Some(xr::Event::SessionStateChanged(state))
                    if state.session() == session.as_raw()
                        && state.state() == xr::SessionState::READY =>
                {
                    break;
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

        let input_data = InputSessionData::new(instance, &session);

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
        match origin {
            vr::ETrackingUniverseOrigin::Seated => &self.local_space_adjusted,
            vr::ETrackingUniverseOrigin::Standing => &self.stage_space_adjusted,
            vr::ETrackingUniverseOrigin::RawAndUncalibrated => {
                crate::warn_unimplemented!("RawAndUncalibrated tracking space");
                &self.stage_space_reference
            }
        }
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
            vr::ETrackingUniverseOrigin::RawAndUncalibrated => {
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
    use super::{FrameStream, GraphicsBackend, OpenXrData, SessionCreateInfo, vsync_phase};
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
}
