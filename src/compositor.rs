use crate::{
    AtomicF64,
    clientcore::{Injected, Injector},
    graphics_backends::{GraphicsBackend, SupportedBackend, supported_backends_enum},
    input::Input,
    openxr_data::{self, FrameStream, OpenXrData, RestartError, SessionCreateInfo, SessionData},
    overlay::OverlayMan,
    system::System,
    tracy_span,
};

use log::{debug, info, trace, warn};
use openvr as vr;
use openxr as xr;
use std::mem::offset_of;
use std::sync::{
    Arc, Mutex, Once,
    atomic::{AtomicU32, Ordering},
};
use std::time::Instant;
use std::{ffi::c_char, ops::Deref};

#[derive(Default)]
pub struct CompositorSessionData(Mutex<Option<DynFrameController>>);

#[derive(macros::InterfaceImpl)]
#[interface = "IVRCompositor"]
#[versions(
    029, 028, 027, 026, 024, 022, 021, 020, 019, 018, 017, 016, 015, 014, 013, 012, 011, 010, 009
)]
pub struct Compositor {
    vtables: Vtables,
    openxr: Arc<OpenXrData<Self>>,
    input: Injected<Input<Self>>,
    system: Injected<System>,
    /// Stores the backend data in between session restarts.
    tmp_backend: Mutex<Option<AnyTempBackendData>>,
    /// Serializes setting up the real session. Never take it while holding a session guard.
    real_session_init: Mutex<()>,
    overlays: Injected<OverlayMan>,
    metrics: FrameMetrics,
    timing_mode: Mutex<vr::EVRCompositorTimingMode>,
    frame_state: Mutex<FrameState>,
    focused: Once,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum FrameState {
    Waited,
    Begun,
    Submitted,
}

impl FrameState {
    fn advance_to(&mut self, new: Self) -> bool {
        let old = *self;
        let allowed: bool = match old {
            Self::Waited => new == Self::Begun,
            Self::Begun => new != Self::Begun,
            Self::Submitted => new == Self::Waited,
        };
        if allowed {
            *self = new;
            trace!("advanced frame state from {old:?} to {new:?}");
        } else {
            warn!("Tried to advance frame state from {old:?} to {new:?}, which is not allowed");
        }
        allowed
    }
}

struct FrameMetrics {
    system_start: Instant,
    index: AtomicU32,
    time: AtomicF64,
}

struct TempBackendData<G: GraphicsBackend> {
    backend: G,
    swapchain_create_info: Option<xr::SwapchainCreateInfo<G::Api>>,
}
supported_backends_enum!(enum AnyTempBackendData: TempBackendData);

impl Compositor {
    pub fn new(openxr: Arc<OpenXrData<Self>>, injector: &Injector) -> Self {
        Self {
            vtables: Default::default(),
            openxr,
            input: injector.inject(),
            system: injector.inject(),
            tmp_backend: Mutex::default(),
            real_session_init: Mutex::default(),
            overlays: injector.inject(),
            metrics: FrameMetrics {
                system_start: Instant::now(),
                index: 0.into(),
                time: 0.0.into(),
            },
            timing_mode: vr::EVRCompositorTimingMode::Implicit.into(),
            frame_state: FrameState::Submitted.into(),
            focused: Once::new(),
        }
    }

    /// Returns false if xrWaitFrame didn't block and won't, so the caller has to pace the app.
    fn maybe_wait_frame(&self, session_data: &SessionData) -> bool {
        tracy_span!();
        if self.openxr.is_lost() {
            return false;
        }
        let mut frame_lock = { session_data.comp_data.0.lock().unwrap() };
        {
            let mut state = self.frame_state.lock().unwrap();
            // The waited frame failed to begin. Waiting again before it was begun would block
            // forever.
            if *state == FrameState::Waited {
                return false;
            }
            state.advance_to(FrameState::Waited);
        }
        let Some(ctrl) = frame_lock.as_mut() else {
            debug!("no frame controller - not starting frame");
            // The temporary session is replaced on the first Submit, unless that can't happen.
            return self.openxr.restart_blocked().is_none();
        };

        #[macros::any_graphics(DynFrameController)]
        fn wait_frame<G: GraphicsBackend + 'static>(
            ctrl: &mut FrameController<G>,
        ) -> xr::Result<(xr::Time, i64)> {
            ctrl.wait_frame()
        }

        let Some((display_time, display_period)) = self.openxr.check(
            "Waiting for frame",
            ctrl.with_any_graphics_mut::<wait_frame>(()),
        ) else {
            // No frame was waited, so the next WaitGetPoses waits again.
            *self.frame_state.lock().unwrap() = FrameState::Submitted;
            return false;
        };
        self.openxr.set_frame_timing(display_time, display_period);
        true
    }

    fn maybe_begin_frame(&self, session_data: &SessionData) {
        tracy_span!();
        if self.openxr.is_lost() {
            return;
        }
        let mut frame_lock = { session_data.comp_data.0.lock().unwrap() };
        if !self
            .frame_state
            .lock()
            .unwrap()
            .advance_to(FrameState::Begun)
        {
            debug!("not starting frame - already begun or submitted");
            return;
        };
        let Some(ctrl) = frame_lock.as_mut() else {
            debug!("no frame controller - not starting frame");
            return;
        };

        #[macros::any_graphics(DynFrameController)]
        fn begin_frame<G: GraphicsBackend + 'static>(
            ctrl: &mut FrameController<G>,
        ) -> xr::Result<()> {
            ctrl.begin_frame()
        }

        if self
            .openxr
            .check(
                "Beginning frame",
                ctrl.with_any_graphics_mut::<begin_frame>(()),
            )
            .is_none()
        {
            // The frame is still waited, so the next WaitGetPoses tries to begin it again.
            *self.frame_state.lock().unwrap() = FrameState::Waited;
        }
    }

    pub fn initialize_real_session(
        &self,
        texture: &vr::Texture_t,
        bounds: vr::VRTextureBounds_t,
    ) -> Result<(), vr::EVRCompositorError> {
        let restart_error = |e| match e {
            RestartError::Exiting => vr::EVRCompositorError::DoNotHaveFocus,
            RestartError::CreationFailed => vr::EVRCompositorError::InvalidTexture,
        };
        if let Some(e) = self.openxr.restart_blocked() {
            return Err(restart_error(e));
        }
        // openxr panics instead of failing when asked for OpenGL requirements without it.
        if texture.eType == vr::ETextureType::OpenGL
            && !self.openxr.enabled_extensions.khr_opengl_enable
        {
            crate::warn_once!("OpenGL texture submitted, but the runtime has no OpenGL support");
            return Err(vr::EVRCompositorError::InvalidTexture);
        }
        let backend =
            SupportedBackend::new(texture, bounds).ok_or(vr::EVRCompositorError::InvalidTexture)?;

        // Submit and the overlay calls can race to get here. Restarting again would destroy the
        // swapchains created in the first real session.
        let _init = self.real_session_init.lock().unwrap();
        if self
            .openxr
            .session_data
            .get()
            .comp_data
            .0
            .lock()
            .unwrap()
            .is_some()
        {
            debug!("real session already set up by another thread");
            return Ok(());
        }

        #[macros::any_graphics(SupportedBackend)]
        fn swapchain_info<G: GraphicsBackend>(
            backend: G,
            texture: &vr::Texture_t,
            bounds: vr::VRTextureBounds_t,
        ) -> Result<AnyTempBackendData, vr::EVRCompositorError>
        where
            AnyTempBackendData: From<TempBackendData<G>>,
        {
            let b_texture =
                G::get_texture(texture).ok_or(vr::EVRCompositorError::InvalidTexture)?;
            let info = backend.swapchain_info_for_texture(b_texture, bounds, texture.eColorSpace);
            Ok(TempBackendData {
                backend,
                swapchain_create_info: Some(info),
            }
            .into())
        }

        let tmp_backend = backend
            .with_any_graphics_owned::<swapchain_info>((texture, bounds))
            .inspect_err(|_| debug!("received invalid texture handle, not restarting session"))?;
        *self.tmp_backend.lock().unwrap() = Some(tmp_backend);

        info!("Creating real backend for texture type {:?}", texture.eType);
        self.openxr.restart_session().map_err(|e| {
            // No session will be created with it anymore.
            self.tmp_backend.lock().unwrap().take();
            restart_error(e)
        })
    }
}

fn fill_vk_extensions_buffer(extensions: String, buffer: *mut c_char, buffer_size: u32) -> u32 {
    let bytes = unsafe {
        std::slice::from_raw_parts(extensions.as_ptr() as *const c_char, extensions.len())
    };

    if !buffer.is_null() && buffer_size as usize > bytes.len() {
        let buffer = unsafe { std::slice::from_raw_parts_mut(buffer, buffer_size as usize) };
        buffer[0..bytes.len()].copy_from_slice(bytes);
        buffer[bytes.len()] = 0;
    }

    bytes.len() as u32 + 1
}

impl openxr_data::Compositor for Compositor {
    fn get_session_create_info(&self, data: CompositorSessionData) -> SessionCreateInfo {
        #[macros::any_graphics(AnyTempBackendData)]
        fn info<G: GraphicsBackend>(data: &TempBackendData<G>) -> SessionCreateInfo
        where
            SessionCreateInfo: From<openxr_data::CreateInfo<G::Api>>,
        {
            SessionCreateInfo::from_info::<G::Api>(data.backend.session_create_info())
        }

        self.tmp_backend
            .lock()
            .unwrap()
            .get_or_insert_with(|| {
                #[macros::any_graphics(DynFrameController)]
                fn take<G: GraphicsBackend>(ctrl: FrameController<G>) -> AnyTempBackendData
                where
                    AnyTempBackendData: From<TempBackendData<G>>,
                {
                    TempBackendData {
                        backend: ctrl.backend,
                        swapchain_create_info: ctrl.swapchain_data.map(|d| d.info),
                    }
                    .into()
                }
                data.0
                    .lock()
                    .unwrap()
                    .take()
                    .expect("one of tmp backend or frame controller should be setup")
                    .with_any_graphics_owned::<take>(())
            })
            .with_any_graphics::<info>(())
    }

    fn post_session_restart(
        &self,
        session_data: &SessionData,
        waiter: xr::FrameWaiter,
        stream: FrameStream,
    ) {
        // This function is called while a write lock is called on the session, and as such should
        // not use self.openxr.session_data.get().

        let backend_data = self
            .tmp_backend
            .lock()
            .unwrap()
            .take()
            .expect("tmp backend should be set before post_session_restart is called");

        #[macros::any_graphics(AnyTempBackendData)]
        fn new_frame_controller<G: GraphicsBackend + 'static>(
            data: TempBackendData<G>,
            openxr: &OpenXrData<Compositor>,
            session_data: &SessionData,
            waiter: xr::FrameWaiter,
            stream: FrameStream,
        ) -> DynFrameController
        where
            for<'a> &'a openxr_data::GraphicalSession:
                TryInto<&'a openxr_data::Session<G::Api>, Error: std::fmt::Display>,
            FrameStream: TryInto<xr::FrameStream<G::Api>>,
            DynFrameController: From<FrameController<G>>,
            G::Format: PartialEq + std::fmt::Debug,
        {
            FrameController::new(
                openxr,
                session_data,
                waiter,
                stream.try_into().unwrap_or_else(|_| unreachable!()),
                data.backend,
                data.swapchain_create_info,
            )
            .into()
        }

        *session_data.comp_data.0.lock().unwrap() = Some(
            backend_data.with_any_graphics_owned::<new_frame_controller>((
                &self.openxr,
                session_data,
                waiter,
                stream,
            )),
        );

        let old_state = std::mem::replace(
            &mut *self.frame_state.lock().unwrap(),
            FrameState::Submitted,
        );

        trace!("returning to {old_state:?} frame state");
        match old_state {
            FrameState::Submitted => {}
            FrameState::Waited => {
                self.maybe_wait_frame(session_data);
            }
            FrameState::Begun => {
                if self.maybe_wait_frame(session_data) {
                    self.maybe_begin_frame(session_data);
                }
            }
        }
    }
}

#[allow(non_snake_case)]
impl vr::IVRCompositor029_Interface for Compositor {
    fn GetPosesForFrame(
        &self,
        _id: u32,
        pose_array: *mut vr::TrackedDevicePose_t,
        pose_array_count: u32,
    ) -> vr::EVRCompositorError {
        if pose_array.is_null() || pose_array_count == 0 {
            return vr::EVRCompositorError::RequestFailed;
        }

        self.GetLastPoses(pose_array, pose_array_count, std::ptr::null_mut(), 0)
    }
    fn GetLastPosePredictionIDs(
        &self,
        render_pose_prediction_id: *mut u32,
        game_pose_prediction_id: *mut u32,
    ) -> vr::EVRCompositorError {
        let index = self.metrics.index.load(Ordering::Relaxed);
        if !render_pose_prediction_id.is_null() {
            unsafe {
                render_pose_prediction_id.write(index);
            }
        }

        if !game_pose_prediction_id.is_null() {
            unsafe {
                game_pose_prediction_id.write(index);
            }
        }
        vr::EVRCompositorError::None
    }
    fn GetCompositorBenchmarkResults(
        &self,
        _pBenchmarkResults: *mut vr::Compositor_BenchmarkResults,
        _nSizeOfBenchmarkResults: u32,
    ) -> bool {
        crate::warn_unimplemented!("GetCompositorBenchmarkResults");
        false
    }
    fn ClearStageOverride(&self) {}
    fn SetStageOverride_Async(
        &self,
        _pchRenderModelPath: *const std::ffi::c_char,
        _pTransform: *const vr::HmdMatrix34_t,
        _pRenderSettings: *const vr::Compositor_StageRenderSettings,
        _nSizeOfRenderSettings: u32,
    ) -> vr::EVRCompositorError {
        crate::warn_unimplemented!("SetStageOverride_Async");
        vr::EVRCompositorError::None
    }
    fn IsCurrentSceneFocusAppLoading(&self) -> bool {
        false
    }
    fn IsMotionSmoothingSupported(&self) -> bool {
        false
    }
    fn IsMotionSmoothingEnabled(&self) -> bool {
        false
    }
    fn SubmitExplicitTimingData(&self) -> vr::EVRCompositorError {
        if *self.timing_mode.lock().unwrap() == vr::EVRCompositorTimingMode::Implicit {
            return vr::EVRCompositorError::RequestFailed;
        }

        if !self.focused.is_completed() {
            // SteamVR doesn't seem to return an error in this case...
            return vr::EVRCompositorError::None;
        }

        let session_data = self.openxr.session_data.get();
        self.maybe_begin_frame(&session_data);
        vr::EVRCompositorError::None
    }
    fn SetExplicitTimingMode(&self, timing_mode: vr::EVRCompositorTimingMode) {
        if !matches!(
            timing_mode,
            vr::EVRCompositorTimingMode::Implicit
                | vr::EVRCompositorTimingMode::Explicit_RuntimePerformsPostPresentHandoff
                | vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff
        ) {
            crate::warn_once!("Ignoring unknown timing mode {:?}", timing_mode);
            return;
        }
        debug!("Setting timing mode to {timing_mode:?}");
        *self.timing_mode.lock().unwrap() = timing_mode;
    }

    fn GetVulkanDeviceExtensionsRequired(
        &self,
        _physical_device: *mut vr::VkPhysicalDevice_T,
        buffer: *mut std::ffi::c_char,
        buffer_size: u32,
    ) -> u32 {
        let exts = self
            .openxr
            .instance
            .vulkan_legacy_device_extensions(self.openxr.system_id)
            .unwrap();
        log::debug!("required device extensions: {exts}");
        fill_vk_extensions_buffer(exts, buffer, buffer_size)
    }

    fn GetVulkanInstanceExtensionsRequired(
        &self,
        buffer: *mut std::ffi::c_char,
        buffer_size: u32,
    ) -> u32 {
        let exts = self
            .openxr
            .instance
            .vulkan_legacy_instance_extensions(self.openxr.system_id)
            .unwrap();
        log::debug!("required instance extensions: {exts}");
        fill_vk_extensions_buffer(exts, buffer, buffer_size)
    }

    fn UnlockGLSharedTextureForAccess(&self, _glSharedTextureHandle: vr::glSharedTextureHandle_t) {
        crate::warn_unimplemented!("UnlockGLSharedTextureForAccess");
    }
    fn LockGLSharedTextureForAccess(&self, _glSharedTextureHandle: vr::glSharedTextureHandle_t) {
        crate::warn_unimplemented!("LockGLSharedTextureForAccess");
    }
    fn ReleaseSharedGLTexture(
        &self,
        _glTextureId: vr::glUInt_t,
        _glSharedTextureHandle: vr::glSharedTextureHandle_t,
    ) -> bool {
        crate::warn_unimplemented!("ReleaseSharedGLTexture");
        false
    }
    fn GetMirrorTextureGL(
        &self,
        _eEye: vr::EVREye,
        pglTextureId: *mut vr::glUInt_t,
        pglSharedTextureHandle: *mut vr::glSharedTextureHandle_t,
    ) -> vr::EVRCompositorError {
        crate::warn_unimplemented!("GetMirrorTextureGL");
        if let Some(id) = unsafe { pglTextureId.as_mut() } {
            *id = 0;
        }
        if let Some(handle) = unsafe { pglSharedTextureHandle.as_mut() } {
            *handle = std::ptr::null_mut();
        }
        vr::EVRCompositorError::IncompatibleVersion
    }
    fn ReleaseMirrorTextureD3D11(&self, _pD3D11ShaderResourceView: *mut std::ffi::c_void) {
        crate::warn_unimplemented!("ReleaseMirrorTextureD3D11");
    }
    fn GetMirrorTextureD3D11(
        &self,
        _eEye: vr::EVREye,
        _pD3D11DeviceOrResource: *mut std::ffi::c_void,
        _ppD3D11ShaderResourceView: *mut *mut std::ffi::c_void,
    ) -> vr::EVRCompositorError {
        crate::warn_unimplemented!("GetMirrorTextureD3D11");
        vr::EVRCompositorError::IncompatibleVersion
    }
    fn SuspendRendering(&self, bSuspend: bool) {
        #[macros::any_graphics(DynFrameController)]
        fn set_suspend_render<G: GraphicsBackend + 'static>(
            ctrl: &mut FrameController<G>,
            app_suspend_render: bool,
        ) {
            ctrl.app_suspend_render = app_suspend_render;
        }

        self.openxr
            .session_data
            .get()
            .comp_data
            .0
            .lock()
            .unwrap()
            .iter_mut()
            .for_each(|ctrl| ctrl.with_any_graphics_mut::<set_suspend_render>(bSuspend));
    }
    fn ForceReconnectProcess(&self) {
        crate::warn_unimplemented!("ForceReconnectProcess");
    }
    fn ForceInterleavedReprojectionOn(&self, _: bool) {
        crate::warn_unimplemented!("ForceInterleavedReprojectionOn");
    }
    fn ShouldAppRenderWithLowResources(&self) -> bool {
        // TODO
        false
    }
    fn CompositorDumpImages(&self) {
        crate::warn_unimplemented!("CompositorDumpImages");
    }
    fn IsMirrorWindowVisible(&self) -> bool {
        false
    }
    fn HideMirrorWindow(&self) {
        crate::warn_unimplemented!("HideMirrorWindow");
    }
    fn ShowMirrorWindow(&self) {
        crate::warn_unimplemented!("ShowMirrorWindow");
    }
    fn CanRenderScene(&self) -> bool {
        true
    }
    fn GetLastFrameRenderer(&self) -> u32 {
        std::process::id()
    }
    fn GetCurrentSceneFocusProcess(&self) -> u32 {
        std::process::id()
    }
    fn IsFullscreen(&self) -> bool {
        true
    }
    fn CompositorQuit(&self) {
        crate::warn_unimplemented!("CompositorQuit");
    }
    fn CompositorGoToBack(&self) {
        crate::warn_unimplemented!("CompositorGoToBack");
    }
    fn CompositorBringToFront(&self) {
        crate::warn_unimplemented!("CompositorBringToFront");
    }
    fn ClearSkyboxOverride(&self) {
        if let Some(overlays) = self.overlays.get() {
            overlays.clear_skybox();
        }
    }
    fn SetSkyboxOverride(
        &self,
        pTextures: *const vr::Texture_t,
        unTextureCount: u32,
    ) -> vr::EVRCompositorError {
        let overlays = self
            .overlays
            .force(|injector| OverlayMan::new(self.openxr.clone(), injector));
        if pTextures.is_null() {
            return vr::EVRCompositorError::RequestFailed;
        }
        match unTextureCount {
            1..=2 => {
                if !self
                    .openxr
                    .enabled_extensions
                    .khr_composition_layer_equirect2
                {
                    log::info!("Could not set skybox: khr_composition_layer_equirect2 unsupported");
                    return vr::EVRCompositorError::None;
                }
                log::debug!("Setting new equirect skybox");
            }
            6 => {
                log::debug!("Setting new box skybox");
            }
            _ => {
                log::warn!("Invalid number of skybox textures: {unTextureCount}");
                return vr::EVRCompositorError::RequestFailed;
            }
        }

        let textures = unsafe { std::slice::from_raw_parts(pTextures, unTextureCount as _) };
        if let Err(e) = overlays.set_skybox(textures) {
            e
        } else {
            vr::EVRCompositorError::None
        }
    }
    fn GetCurrentGridAlpha(&self) -> f32 {
        0.0
    }
    fn FadeGrid(&self, _fSeconds: f32, bFadeGridIn: bool) {
        #[macros::any_graphics(DynFrameController)]
        fn set_fade_grid<G: GraphicsBackend + 'static>(
            ctrl: &mut FrameController<G>,
            app_fade_grid: bool,
        ) {
            ctrl.app_fade_grid = app_fade_grid;
        }

        self.openxr
            .session_data
            .get()
            .comp_data
            .0
            .lock()
            .unwrap()
            .iter_mut()
            .for_each(|ctrl| ctrl.with_any_graphics_mut::<set_fade_grid>(bFadeGridIn));
    }
    fn GetCurrentFadeColor(&self, _bBackground: bool) -> vr::HmdColor_t {
        vr::HmdColor_t::default()
    }
    fn FadeToColor(
        &self,
        _fSeconds: f32,
        _fRed: f32,
        _fGreen: f32,
        _fBlue: f32,
        _fAlpha: f32,
        _bBackground: bool,
    ) {
        crate::warn_unimplemented!("FadeToColor");
    }
    fn GetCumulativeStats(
        &self,
        pStats: *mut vr::Compositor_CumulativeStats,
        nStatsSizeInBytes: u32,
    ) {
        crate::warn_unimplemented!("GetCumulativeStats");
        if !pStats.is_null() {
            let len = (nStatsSizeInBytes as usize).min(size_of::<vr::Compositor_CumulativeStats>());
            unsafe { pStats.cast::<u8>().write_bytes(0, len) };
        }
    }
    fn GetFrameTimeRemaining(&self) -> f32 {
        let Some((since, _)) = self.openxr.vsync_timing() else {
            return 0.0;
        };
        let period = self.openxr.display_period_nanos.load(Ordering::Relaxed);
        std::time::Duration::from_nanos(period.max(0) as u64)
            .saturating_sub(since)
            .as_secs_f32()
    }
    fn GetFrameTimings(&self, pTiming: *mut vr::Compositor_FrameTiming, nFrames: u32) -> u32 {
        if pTiming.is_null() || nFrames == 0 {
            return 0;
        }

        // Only the first entry's size is set by the application, the rest is inferred from it.
        let size = unsafe { (&raw const (*pTiming).m_nSize).read_unaligned() } as usize;
        for i in 0..nFrames {
            let entry = unsafe { pTiming.cast::<u8>().add(i as usize * size) }
                .cast::<vr::Compositor_FrameTiming>();
            if i > 0 {
                unsafe { (&raw mut (*entry).m_nSize).write_unaligned(size as u32) };
            }
            // Frames are returned from oldest to newest.
            if !<Self as vr::IVRCompositor029_Interface>::GetFrameTiming(
                self,
                entry,
                nFrames - 1 - i,
            ) {
                return i;
            }
        }
        nFrames
    }
    fn GetFrameTiming(&self, timing: *mut vr::Compositor_FrameTiming, _frames_ago: u32) -> bool {
        if timing.is_null() || !timing.is_aligned() {
            return false;
        }

        let size = unsafe { (&raw const (*timing).m_nSize).read() } as usize;
        fn ptr_size<T>(_: *const T) -> usize {
            std::mem::size_of::<T>()
        }
        if size
            < offset_of!(vr::Compositor_FrameTiming, m_HmdPose)
                + ptr_size(unsafe { &raw const (*timing).m_HmdPose })
        {
            return false;
        }
        // We're using raw pointers here because the Compositor_FrameTiming struct can be a
        // varaible size, so we don't want to create a reference to a struct with an incorrect
        // (to us) size, because that would be Undefined Behavior.
        macro_rules! set {
            ($member:ident, $value:expr) => {{
                let ptr = &raw mut (*timing).$member;
                ptr.write_unaligned($value)
            }};
        }

        unsafe {
            // TODO: These values are copy/pasted from OpenComposite, determine if real values are
            // necessary/better
            set!(m_nFrameIndex, self.metrics.index.load(Ordering::Relaxed));
            set!(m_nNumFramePresents, 1);
            set!(m_nNumMisPresented, 0);
            set!(m_nReprojectionFlags, 0);
            set!(m_flSystemTimeInSeconds, self.metrics.time.load());
            set!(m_flPreSubmitGpuMs, 8.0);
            set!(m_flPostSubmitGpuMs, 1.0);
            set!(m_flTotalRenderGpuMs, 9.0);

            set!(m_flCompositorRenderGpuMs, 1.5);
            set!(m_flCompositorRenderCpuMs, 3.0);
            set!(m_flCompositorIdleCpuMs, 0.1);

            set!(
                m_flClientFrameIntervalMs,
                1000.0 / self.openxr.get_refresh_rate()
            );
            set!(m_flPresentCallCpuMs, 0.0);
            set!(m_flWaitForPresentCpuMs, 0.0);
            set!(m_flSubmitFrameMs, 0.0);

            set!(m_flWaitGetPosesCalledMs, 0.0);
            set!(m_flNewPosesReadyMs, 0.0);
            set!(m_flNewFrameReadyMs, 0.0); // second call to IVRCompositor::Submit
            set!(m_flCompositorUpdateStartMs, 0.0);
            set!(m_flCompositorUpdateEndMs, 0.0);
            set!(m_flCompositorRenderStartMs, 0.0);
        }

        true
    }
    fn PostPresentHandoff(&self) {
        #[macros::any_graphics(DynFrameController)]
        fn end_frame<G: GraphicsBackend + 'static>(
            ctrl: &mut FrameController<G>,
            session_data: &SessionData,
            system: &System,
            display_time: xr::Time,
            overlays: Option<&OverlayMan>,
            max_layers: usize,
        ) -> xr::Result<()>
        where
            for<'b> &'b crate::overlay::AnySwapchainMap:
                TryInto<&'b crate::overlay::SwapchainMap<G::Api>, Error: std::fmt::Display>,
        {
            ctrl.end_frame(session_data, system, display_time, overlays, max_layers)
        }

        if self.openxr.is_lost() {
            return;
        }
        let session_data = self.openxr.session_data.get();
        let mut frame_lock = session_data.comp_data.0.lock().unwrap();
        let Some(ctrl) = frame_lock.as_mut() else {
            debug!("no frame controller - not presenting frame");
            return;
        };

        if *self.frame_state.lock().unwrap() != FrameState::Begun {
            return;
        }

        trace!("presenting frame");
        let system = self.system.force(|i| System::new(self.openxr.clone(), i));
        // Even if the frame took long enough for get_display_time to use the current time.
        let display_time = self.openxr.waited_display_time();
        let overlays = self.overlays.get();

        let result = ctrl.with_any_graphics_mut::<end_frame>((
            &session_data,
            &system,
            display_time,
            overlays.as_deref(),
            self.openxr.max_layer_count,
        ));
        // A frame that failed to end is discarded by the next xrBeginFrame.
        self.openxr.check("Ending frame", result);

        self.frame_state
            .lock()
            .unwrap()
            .advance_to(FrameState::Submitted);

        self.metrics.index.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .time
            .store(self.metrics.system_start.elapsed().as_secs_f64());
        #[cfg(feature = "tracing")]
        {
            tracy_client::frame_mark();
        }
    }
    fn ClearLastSubmittedFrame(&self) {
        crate::warn_unimplemented!("ClearLastSubmittedFrame");
    }
    fn SubmitWithArrayIndex(
        &self,
        _eEye: vr::EVREye,
        _pTexture: *const vr::Texture_t,
        _unTextureArrayIndex: u32,
        _pBounds: *const vr::VRTextureBounds_t,
        _nSubmitFlags: vr::EVRSubmitFlags,
    ) -> vr::EVRCompositorError {
        crate::warn_unimplemented!("SubmitWithArrayIndex");
        vr::EVRCompositorError::IncompatibleVersion
    }

    fn GetSubmitTexture(
        &self,
        _: *mut vr::Texture_t,
        __bindgen_vtable: *mut bool,
        _: vr::EVRCompositorTextureUsage,
        _: *const vr::Texture_t,
        _: *const vr::VRTextureBounds_t,
        _: vr::EVRSubmitFlags,
    ) -> vr::EVRCompositorError {
        crate::warn_unimplemented!("GetSubmitTexture");
        vr::EVRCompositorError::IncompatibleVersion
    }

    fn Submit(
        &self,
        eye: vr::EVREye,
        texture: *const vr::Texture_t,
        bounds: *const vr::VRTextureBounds_t,
        submit_flags: vr::EVRSubmitFlags,
    ) -> vr::EVRCompositorError {
        if !self.openxr.can_render() {
            crate::warn_once!(
                "{:?} application tried to submit a frame",
                self.openxr.application_type
            );
            return vr::EVRCompositorError::IsNotSceneApplication;
        }

        let bounds = unsafe { bounds.as_ref() }
            .copied()
            .unwrap_or(vr::VRTextureBounds_t {
                uMin: 0.0,
                vMin: 0.0,
                uMax: 1.0,
                vMax: 1.0,
            });

        // Superhot passes crazy bounds on startup.
        if !bounds.valid() {
            return vr::EVRCompositorError::InvalidBounds;
        }

        let Some(texture) = (unsafe { texture.as_ref() }) else {
            return vr::EVRCompositorError::InvalidTexture;
        };

        if !self.focused.is_completed() || self.openxr.is_lost() {
            return vr::EVRCompositorError::DoNotHaveFocus;
        }

        let mut session_lock = self.openxr.session_data.get();
        let mut frame_lock = session_lock.comp_data.0.lock().unwrap();

        let ctrl = match frame_lock.as_mut() {
            Some(ctrl) => ctrl,
            None => {
                drop(frame_lock);
                drop(session_lock);

                if let Err(e) = self.initialize_real_session(texture, bounds) {
                    return e;
                }
                info!("Received game texture, restarted session with new data");

                session_lock = self.openxr.session_data.get();
                frame_lock = session_lock.comp_data.0.lock().unwrap();
                // Another thread's restart may have gone back to a temporary session meanwhile.
                let Some(ctrl) = frame_lock.as_mut() else {
                    return vr::EVRCompositorError::InvalidTexture;
                };
                ctrl
            }
        };

        #[macros::any_graphics(DynFrameController)]
        fn submit<G: GraphicsBackend + 'static>(
            ctrl: &mut FrameController<G>,
            openxr: &OpenXrData<Compositor>,
            session_data: &SessionData,
            eye: vr::EVREye,
            texture: &vr::Texture_t,
            bounds: vr::VRTextureBounds_t,
            flags: vr::EVRSubmitFlags,
        ) -> xr::Result<(), vr::EVRCompositorError>
        where
            for<'d> &'d openxr_data::GraphicalSession:
                TryInto<&'d openxr_data::Session<G::Api>, Error: std::fmt::Display>,
            G::Format: Eq + std::fmt::Debug,
        {
            let real_texture =
                G::get_texture(texture).ok_or(vr::EVRCompositorError::InvalidTexture)?;
            ctrl.submit_impl(
                openxr,
                session_data,
                eye,
                real_texture,
                texture.eColorSpace,
                bounds,
                flags,
            )
        }

        if let Err(e) = ctrl.with_any_graphics_mut::<submit>((
            &self.openxr,
            &session_lock,
            eye,
            texture,
            bounds,
            submit_flags,
        )) {
            return e;
        }
        vr::EVRCompositorError::None
    }

    fn GetLastPoseForTrackedDeviceIndex(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        output_pose: *mut vr::TrackedDevicePose_t,
        output_game_pose: *mut vr::TrackedDevicePose_t,
    ) -> vr::EVRCompositorError {
        if device_index >= vr::k_unMaxTrackedDeviceCount {
            return vr::EVRCompositorError::IndexOutOfRange;
        }
        if output_pose.is_null() && output_game_pose.is_null() {
            return vr::EVRCompositorError::RequestFailed;
        }

        let input = self.input.force(|_| Input::new(self.openxr.clone()));

        // Like GetLastPoses, a slot without a device gets an invalid, disconnected pose.
        let pose = input
            .get_device_pose(device_index, None, self.openxr.get_display_time(), None)
            .unwrap_or_default();

        for output in [output_pose, output_game_pose] {
            if !output.is_null() {
                unsafe { output.write(pose) };
            }
        }

        vr::EVRCompositorError::None
    }
    fn GetLastPoses(
        &self,
        render_pose_array: *mut vr::TrackedDevicePose_t,
        render_pose_count: u32,
        game_pose_array: *mut vr::TrackedDevicePose_t,
        game_pose_count: u32,
    ) -> vr::EVRCompositorError {
        tracy_span!("GetLastPoses impl");
        if self.openxr.is_lost() {
            for (poses, count) in [
                (render_pose_array, render_pose_count),
                (game_pose_array, game_pose_count),
            ] {
                if !poses.is_null() {
                    unsafe { std::slice::from_raw_parts_mut(poses, count as usize) }
                        .fill(Default::default());
                }
            }
            return vr::EVRCompositorError::DoNotHaveFocus;
        }
        let time = self.openxr.get_display_time();
        // Both arrays are optional and sized independently. Not entirely sure how the game poses
        // are supposed to differ from the render poses, but a lot of games use the game pose array
        // for controller positions.
        for (array, count) in [
            (render_pose_array, render_pose_count),
            (game_pose_array, game_pose_count),
        ] {
            if array.is_null() || count == 0 {
                continue;
            }
            let poses = unsafe { std::slice::from_raw_parts_mut(array, count as usize) };
            self.input
                .force(|_| Input::new(self.openxr.clone()))
                .get_poses(poses, None, time);
        }

        vr::EVRCompositorError::None
    }

    fn WaitGetPoses(
        &self,
        render_pose_array: *mut vr::TrackedDevicePose_t,
        render_pose_count: u32,
        game_pose_array: *mut vr::TrackedDevicePose_t,
        game_pose_count: u32,
    ) -> vr::EVRCompositorError {
        tracy_span!("WaitGetPoses impl");
        let _frame_update = self.openxr.frame_update();
        // This should be called every frame - we must regularly poll events
        self.openxr.poll_events();
        self.focused.call_once(|| {});
        let paced = {
            let session_data = self.openxr.session_data.get();
            let timing_mode = *self.timing_mode.lock().unwrap();
            if matches!(
                timing_mode,
                vr::EVRCompositorTimingMode::Implicit
                    | vr::EVRCompositorTimingMode::Explicit_RuntimePerformsPostPresentHandoff
            ) && *self.frame_state.lock().unwrap() == FrameState::Begun
            {
                self.PostPresentHandoff();
            }

            if *self.frame_state.lock().unwrap() == FrameState::Waited {
                // Auto-flush pending overlay-only frame: begin it and immediately
                // submit so overlay layers reach the compositor for games that
                // never call Submit / PostPresentHandoff (e.g. menu-only frames).
                // Normal games that submit each frame don't end up in Waited
                // state here, so this is transparent to them.
                self.maybe_begin_frame(&session_data);
                if *self.frame_state.lock().unwrap() == FrameState::Begun {
                    self.PostPresentHandoff();
                }
            }
            let paced = self.maybe_wait_frame(&session_data);

            if paced && timing_mode == vr::EVRCompositorTimingMode::Implicit {
                self.maybe_begin_frame(&session_data);
            }
            paced
        };
        if !paced {
            // Keep the app from spinning, e.g. after the session was lost and it ignores Quit.
            let period = self.openxr.display_period_nanos.load(Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_nanos(
                period.clamp(1_000_000, 100_000_000) as u64,
            ));
        }
        if self.openxr.is_lost() {
            return self.GetLastPoses(
                render_pose_array,
                render_pose_count,
                game_pose_array,
                game_pose_count,
            );
        }
        if let Some(system) = self.system.get() {
            system.reset_views();
        }
        if let Some(input) = self.input.get() {
            input.frame_start_update();
        }

        self.GetLastPoses(
            render_pose_array,
            render_pose_count,
            game_pose_array,
            game_pose_count,
        )
    }

    fn GetTrackingSpace(&self) -> vr::ETrackingUniverseOrigin {
        self.openxr.get_tracking_space()
    }

    fn SetTrackingSpace(&self, origin: vr::ETrackingUniverseOrigin) {
        self.openxr.set_tracking_space(origin);
    }
}

impl vr::IVRCompositor021On022 for Compositor {
    fn SetExplicitTimingMode(&self, explicit: bool) {
        let mode = if explicit {
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff
        } else {
            vr::EVRCompositorTimingMode::Implicit
        };

        <Self as vr::IVRCompositor028_Interface>::SetExplicitTimingMode(self, mode);
    }
}

/// Fills the fields shared by the 0.9.15 to 1.0.1 frame timing layouts.
macro_rules! scene_render_timing {
    ($c:ident, $($ty:ident)::+ { $($extra:tt)* }) => {
        $($ty)::+ {
            m_nSize: 0,
            m_nFrameIndex: $c.m_nFrameIndex,
            m_nNumFramePresents: $c.m_nNumFramePresents,
            m_nNumDroppedFrames: $c.m_nNumDroppedFrames,
            m_flSystemTimeInSeconds: $c.m_flSystemTimeInSeconds,
            m_flSceneRenderGpuMs: $c.m_flPreSubmitGpuMs,
            m_flTotalRenderGpuMs: $c.m_flTotalRenderGpuMs,
            m_flCompositorRenderGpuMs: $c.m_flCompositorRenderGpuMs,
            m_flCompositorRenderCpuMs: $c.m_flCompositorRenderCpuMs,
            m_flCompositorIdleCpuMs: $c.m_flCompositorIdleCpuMs,
            m_flClientFrameIntervalMs: $c.m_flClientFrameIntervalMs,
            m_flPresentCallCpuMs: $c.m_flPresentCallCpuMs,
            m_flWaitForPresentCpuMs: $c.m_flWaitForPresentCpuMs,
            m_flSubmitFrameMs: $c.m_flSubmitFrameMs,
            m_flWaitGetPosesCalledMs: $c.m_flWaitGetPosesCalledMs,
            m_flNewPosesReadyMs: $c.m_flNewPosesReadyMs,
            m_flNewFrameReadyMs: $c.m_flNewFrameReadyMs,
            m_flCompositorUpdateStartMs: $c.m_flCompositorUpdateStartMs,
            m_flCompositorUpdateEndMs: $c.m_flCompositorUpdateEndMs,
            m_flCompositorRenderStartMs: $c.m_flCompositorRenderStartMs,
            m_HmdPose: $c.m_HmdPose,
            $($extra)*
            ..Default::default()
        }
    };
}

impl Compositor {
    /// GetFrameTiming for older interface versions. `T` is that version's
    /// Compositor_FrameTiming, which starts with a u32 size like all of them.
    fn legacy_frame_timing<T>(
        &self,
        timing: *mut T,
        frames_ago: u32,
        convert: impl FnOnce(&vr::Compositor_FrameTiming) -> T,
    ) -> bool {
        if timing.is_null() {
            return false;
        }
        let size = unsafe { timing.cast::<u32>().read_unaligned() };
        if (size as usize) < std::mem::size_of::<T>() {
            return false;
        }

        let mut current = vr::Compositor_FrameTiming {
            m_nSize: std::mem::size_of::<vr::Compositor_FrameTiming>() as u32,
            ..Default::default()
        };
        if !<Self as vr::IVRCompositor029_Interface>::GetFrameTiming(self, &mut current, frames_ago)
        {
            return false;
        }
        unsafe {
            timing.write_unaligned(convert(&current));
            timing.cast::<u32>().write_unaligned(size);
        }
        true
    }
}

impl vr::IVRCompositor016On017 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut vr::vr_1_0_3::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            vr::vr_1_0_3::Compositor_FrameTiming {
                m_nSize: 0,
                m_nFrameIndex: c.m_nFrameIndex,
                m_nNumFramePresents: c.m_nNumFramePresents,
                m_nNumDroppedFrames: c.m_nNumDroppedFrames,
                m_nReprojectionFlags: c.m_nReprojectionFlags,
                m_flSystemTimeInSeconds: c.m_flSystemTimeInSeconds,
                m_flPreSubmitGpuMs: c.m_flPreSubmitGpuMs,
                m_flPostSubmitGpuMs: c.m_flPostSubmitGpuMs,
                m_flTotalRenderGpuMs: c.m_flTotalRenderGpuMs,
                m_flCompositorRenderGpuMs: c.m_flCompositorRenderGpuMs,
                m_flCompositorRenderCpuMs: c.m_flCompositorRenderCpuMs,
                m_flCompositorIdleCpuMs: c.m_flCompositorIdleCpuMs,
                m_flClientFrameIntervalMs: c.m_flClientFrameIntervalMs,
                m_flPresentCallCpuMs: c.m_flPresentCallCpuMs,
                m_flWaitForPresentCpuMs: c.m_flWaitForPresentCpuMs,
                m_flSubmitFrameMs: c.m_flSubmitFrameMs,
                m_flWaitGetPosesCalledMs: c.m_flWaitGetPosesCalledMs,
                m_flNewPosesReadyMs: c.m_flNewPosesReadyMs,
                m_flNewFrameReadyMs: c.m_flNewFrameReadyMs,
                m_flCompositorUpdateStartMs: c.m_flCompositorUpdateStartMs,
                m_flCompositorUpdateEndMs: c.m_flCompositorUpdateEndMs,
                m_flCompositorRenderStartMs: c.m_flCompositorRenderStartMs,
                m_HmdPose: c.m_HmdPose,
            }
        })
    }
}

impl vr::IVRCompositor015On016 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut openvr::vr_1_0_1::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            scene_render_timing!(
                c,
                vr::vr_1_0_1::Compositor_FrameTiming {
                    m_nReprojectionFlags: c.m_nReprojectionFlags,
                }
            )
        })
    }
    fn RequestScreenshot(
        &self,
        _screenshot_type: vr::EVRScreenshotType,
        _dest_file_name: *const std::ffi::c_char,
        _vr_dest_file_name: *const std::ffi::c_char,
    ) -> vr::EVRCompositorError {
        crate::warn_unimplemented!("RequestScreenshot (v1.0.1)");
        vr::EVRCompositorError::RequestFailed
    }
    fn GetCurrentScreenshotType(&self) -> vr::EVRScreenshotType {
        vr::EVRScreenshotType::None
    }
}

impl vr::IVRCompositor014On015 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut vr::vr_0_9_20::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            scene_render_timing!(
                c,
                vr::vr_0_9_20::Compositor_FrameTiming {
                    m_nReprojectionFlags: c.m_nReprojectionFlags,
                }
            )
        })
    }
}
impl vr::IVRCompositor013On014 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut openvr::vr_0_9_19::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            scene_render_timing!(c, vr::vr_0_9_19::Compositor_FrameTiming {})
        })
    }
}

impl vr::IVRCompositor012On013 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut openvr::vr_0_9_17::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            scene_render_timing!(c, vr::vr_0_9_17::Compositor_FrameTiming {})
        })
    }
}

impl vr::IVRCompositor011On012 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut openvr::vr_0_9_15::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            scene_render_timing!(c, vr::vr_0_9_15::Compositor_FrameTiming {})
        })
    }
}

impl vr::IVRCompositor010On011 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut vr::vr_0_9_14::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            vr::vr_0_9_14::Compositor_FrameTiming {
                size: 0,
                frameStart: c.m_flSystemTimeInSeconds,
                droppedFrames: c.m_nNumDroppedFrames,
                frameIndex: c.m_nFrameIndex,
                pose: c.m_HmdPose,
                m_flFrameIntervalMs: c.m_flClientFrameIntervalMs,
                m_flSceneRenderGpuMs: c.m_flPreSubmitGpuMs,
                m_flCompositorRenderCpuMs: c.m_flCompositorRenderCpuMs,
                m_flCompositorRenderGpuMs: c.m_flCompositorRenderGpuMs,
                m_flPresentCallCpuMs: c.m_flPresentCallCpuMs,
                m_nPresents: c.m_nNumFramePresents,
                ..Default::default()
            }
        })
    }
}

impl vr::IVRCompositor009On010 for Compositor {
    fn GetFrameTiming(
        &self,
        timing: *mut vr::vr_0_9_12::Compositor_FrameTiming,
        frames_ago: u32,
    ) -> bool {
        self.legacy_frame_timing(timing, frames_ago, |c| {
            vr::vr_0_9_12::Compositor_FrameTiming {
                size: 0,
                frameStart: c.m_flSystemTimeInSeconds,
                droppedFrames: c.m_nNumDroppedFrames,
                frameIndex: c.m_nFrameIndex,
                pose: c.m_HmdPose,
                m_flFrameIntervalMs: c.m_flClientFrameIntervalMs,
                m_flSceneRenderGpuMs: c.m_flPreSubmitGpuMs,
                m_flCompositorRenderCpuMs: c.m_flCompositorRenderCpuMs,
                m_flCompositorRenderGpuMs: c.m_flCompositorRenderGpuMs,
                m_flPresentCallCpuMs: c.m_flPresentCallCpuMs,
                ..Default::default()
            }
        })
    }
}

#[derive(Copy, Clone, Default)]
struct SubmittedEye {
    extent: xr::Extent2Di,
    flip_vertically: bool,
}

struct SwapchainData<G: xr::Graphics> {
    swapchain: xr::Swapchain<G>,
    info: xr::SwapchainCreateInfo<G>,
    initial_format: G::Format,
}

struct FrameController<G: GraphicsBackend> {
    stream: xr::FrameStream<G::Api>,
    waiter: xr::FrameWaiter,
    swapchain_data: Option<SwapchainData<G::Api>>,
    image_index: usize,
    image_acquired: bool,
    should_render: bool,
    app_suspend_render: bool,
    app_fade_grid: bool,
    eyes_submitted: [Option<SubmittedEye>; 2],
    submitting_null: bool,
    backend: G,
}
supported_backends_enum!(enum DynFrameController: FrameController);

impl<G: GraphicsBackend> FrameController<G> {
    fn init_swapchain(
        session_data: &SessionData,
        create_info: &mut xr::SwapchainCreateInfo<G::Api>,
        backend: &mut G,
    ) -> xr::Result<(xr::Swapchain<G::Api>, G::Format)>
    where
        for<'a> &'a openxr_data::GraphicalSession:
            TryInto<&'a openxr_data::Session<G::Api>, Error: std::fmt::Display>,
        G::Format: PartialEq + std::fmt::Debug,
    {
        assert!(
            is_valid_swapchain_info(create_info),
            "Recreating swapchain with invalid dimensions {}x{}",
            create_info.width,
            create_info.height
        );

        let initial_format = G::from_openxr_format(create_info.format);
        session_data.check_format::<G>(create_info);

        let swapchain = session_data
            .create_swapchain(create_info)
            .inspect_err(|err| {
                debug!(
                    "Failed to create swapchain: {err} (info: {:#?})",
                    [
                        ("create_flags", format!("{:?}", create_info.create_flags)),
                        ("width", create_info.width.to_string()),
                        ("height", create_info.height.to_string()),
                        ("sample_count", create_info.sample_count.to_string())
                    ]
                )
            })?;

        let images = swapchain.enumerate_images()?;

        let format = G::from_openxr_format(create_info.format);
        backend.store_swapchain_images(images, format);
        debug!(
            "Created new swapchain: {}x{}, format = {:?}",
            create_info.width, create_info.height, format
        );

        Ok((swapchain, initial_format))
    }

    fn new(
        openxr: &OpenXrData<Compositor>,
        session_data: &SessionData,
        waiter: xr::FrameWaiter,
        stream: xr::FrameStream<G::Api>,
        mut backend: G,
        create_info: Option<xr::SwapchainCreateInfo<G::Api>>,
    ) -> Self
    where
        for<'a> &'a openxr_data::GraphicalSession:
            TryInto<&'a openxr_data::Session<G::Api>, Error: std::fmt::Display>,
        G::Format: PartialEq + std::fmt::Debug,
    {
        // Without a swapchain, the next Submit creates one.
        let swapchain_data = create_info
            .filter(is_valid_swapchain_info)
            .and_then(|mut info| {
                let (swapchain, initial_format) = openxr.check(
                    "Creating swapchain",
                    Self::init_swapchain(session_data, &mut info, &mut backend),
                )?;
                let format = G::to_openxr_format(initial_format);
                Some(SwapchainData {
                    swapchain,
                    info,
                    initial_format: format,
                })
            });

        Self {
            stream,
            waiter,
            swapchain_data,
            image_index: 0,
            image_acquired: false,
            should_render: false,
            app_suspend_render: false,
            app_fade_grid: false,
            eyes_submitted: Default::default(),
            submitting_null: false,
            backend,
        }
    }

    fn recreate_swapchain(
        &mut self,
        session_data: &SessionData,
        mut create_info: xr::SwapchainCreateInfo<G::Api>,
    ) -> xr::Result<()>
    where
        for<'a> &'a openxr_data::GraphicalSession:
            TryInto<&'a openxr_data::Session<G::Api>, Error: std::fmt::Display>,
        G::Format: PartialEq + std::fmt::Debug,
    {
        let (swapchain, initial_format) =
            Self::init_swapchain(session_data, &mut create_info, &mut self.backend)?;
        let format = G::to_openxr_format(initial_format);
        self.swapchain_data = Some(SwapchainData {
            swapchain,
            info: create_info,
            initial_format: format,
        });
        // The old image went away with the old swapchain.
        self.image_acquired = false;
        self.eyes_submitted = Default::default();
        self.acquire_swapchain_image()
    }

    fn acquire_swapchain_image(&mut self) -> xr::Result<()> {
        let swapchain = &mut self
            .swapchain_data
            .as_mut()
            .expect("Can't acquire swapchain image with no swapchain!")
            .swapchain;

        self.image_index = swapchain.acquire_image()? as usize;

        trace!("waiting image");
        {
            tracy_span!("wait swapchain image");
            swapchain.wait_image(xr::Duration::INFINITE)?;
        }

        self.image_acquired = true;
        Ok(())
    }

    fn wait_frame(&mut self) -> xr::Result<(xr::Time, i64)> {
        let frame_state = {
            tracy_span!("wait frame");
            self.waiter.wait()?
        };
        self.should_render = frame_state.should_render && !self.app_suspend_render;
        Ok((
            frame_state.predicted_display_time,
            frame_state.predicted_display_period.as_nanos(),
        ))
    }

    fn begin_frame(&mut self) -> xr::Result<()> {
        if self.image_acquired {
            tracy_span!("release old swapchain image");
            self.image_acquired = false;
            self.swapchain_data
                .as_mut()
                .expect("Image is acquired, yet we have no swapchain?")
                .swapchain
                .release_image()?;
        }

        if self.swapchain_data.is_some() {
            self.acquire_swapchain_image()?;
        }

        {
            tracy_span!("begin frame");
            self.stream.begin()?;
        }
        self.eyes_submitted = [None; 2];
        self.submitting_null = false;
        trace!("frame begin");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_impl(
        &mut self,
        openxr: &OpenXrData<Compositor>,
        session_data: &SessionData,
        eye: vr::EVREye,
        texture: G::OpenVrTexture,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        submit_flags: vr::EVRSubmitFlags,
    ) -> Result<(), vr::EVRCompositorError>
    where
        for<'b> &'b openxr_data::GraphicalSession:
            TryInto<&'b openxr_data::Session<G::Api>, Error: std::fmt::Display>,
        G::Format: PartialEq + std::fmt::Debug,
    {
        if !matches!(eye, vr::EVREye::Left | vr::EVREye::Right) {
            return Err(vr::EVRCompositorError::IndexOutOfRange);
        }

        // No Man's Sky does this.
        if self.eyes_submitted[eye.0 as usize].is_some() {
            return Err(vr::EVRCompositorError::AlreadySubmitted);
        }

        self.eyes_submitted[eye.0 as usize] = if self.should_render {
            // Make sure our image dimensions haven't changed.
            let new_info = self
                .backend
                .swapchain_info_for_texture(texture, bounds, color_space);

            if is_valid_swapchain_info(&new_info) {
                assert!(
                    !self.submitting_null,
                    "App submitted a null texture and a normal texture in the same frame"
                );

                if !self.swapchain_data.as_ref().is_some_and(|data| {
                    is_usable_swapchain(
                        &data.info,
                        G::from_openxr_format(data.initial_format),
                        &new_info,
                        G::from_openxr_format(new_info.format),
                    )
                }) {
                    info!("recreating swapchain (for {eye:?})");
                    if let Err(e) = self.recreate_swapchain(session_data, new_info) {
                        openxr.handle_error("Recreating swapchain", e);
                        return Err(vr::EVRCompositorError::RequestFailed);
                    }
                }

                Some(SubmittedEye {
                    extent: self.backend.copy_texture_to_swapchain(
                        eye,
                        texture,
                        color_space,
                        bounds,
                        self.image_index,
                        submit_flags,
                    ),
                    flip_vertically: bounds.vertically_flipped(),
                })
            } else {
                trace!("submitting null this frame");
                self.submitting_null = true;
                Some(Default::default())
            }
        } else {
            Some(Default::default())
        };

        trace!("submitted {eye:?}");
        if self.eyes_submitted.iter().all(|eye| eye.is_some()) {
            // If the session was restarted while no frame was begun, nothing has been acquired from the new swapchain yet, so there is nothing to release
            if self.image_acquired
                && let Some(data) = self.swapchain_data.as_mut()
            {
                trace!("releasing image");
                openxr.check("Releasing swapchain image", data.swapchain.release_image());
            }
            self.image_acquired = false;
        }

        Ok(())
    }

    fn end_frame(
        &mut self,
        session_data: &SessionData,
        system: &System,
        display_time: xr::Time,
        overlays: Option<&OverlayMan>,
        max_layers: usize,
    ) -> xr::Result<()>
    where
        for<'b> &'b crate::overlay::AnySwapchainMap:
            TryInto<&'b crate::overlay::SwapchainMap<G::Api>, Error: std::fmt::Display>,
    {
        let mut proj_layer_views = Vec::new();

        if self.should_render
            && !self.submitting_null
            && self.eyes_submitted.iter().all(|eye| eye.is_some())
            // Without located views there's no FOV to show the image with.
            && let Some(crate::system::ViewData { flags, views }) =
                system.located_views(session_data.current_origin_as_reference_space())
        {
            let swapchain_data = self
                .swapchain_data
                .as_ref()
                .expect("Swapchain data unexpectedly invalid on submit");

            proj_layer_views = views
                .into_iter()
                .enumerate()
                .map(|(eye_index, view)| {
                    let pose = xr::Posef {
                        orientation: if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
                            view.pose.orientation
                        } else {
                            xr::Quaternionf::IDENTITY
                        },
                        position: if flags.contains(xr::ViewStateFlags::POSITION_VALID) {
                            view.pose.position
                        } else {
                            xr::Vector3f::default()
                        },
                    };

                    let SubmittedEye {
                        extent,
                        flip_vertically,
                    } = self.eyes_submitted[eye_index]
                        .unwrap_or_else(|| panic!("Eye {eye_index} has not been submitted!"));
                    let mut fov = view.fov;
                    if flip_vertically {
                        std::mem::swap(&mut fov.angle_up, &mut fov.angle_down);
                    }

                    let sub_image = xr::SwapchainSubImage::new()
                        .swapchain(&swapchain_data.swapchain)
                        .image_array_index(eye_index as u32)
                        .image_rect(xr::Rect2Di {
                            extent,
                            offset: xr::Offset2Di::default(),
                        });

                    xr::CompositionLayerProjectionView::new()
                        .fov(fov)
                        .pose(pose)
                        .sub_image(sub_image)
                })
                .collect()
        }

        let mut proj_layer = None;
        if !proj_layer_views.is_empty() {
            trace!("projection layer present");
            proj_layer = Some(
                xr::CompositionLayerProjection::new()
                    .space(session_data.tracking_space())
                    .views(&proj_layer_views),
            );
        }

        let mut layers: Vec<&xr::CompositionLayerBase<_>> = Vec::new();
        if let Some(l) = proj_layer.as_ref() {
            layers.push(l);
        }
        let overlay_layers;
        if let Some(overlay_man) = overlays {
            overlay_layers = overlay_man.get_layers(session_data, self.app_fade_grid);
            // The layers are sorted by z order, so this drops the bottom ones.
            let excess = (layers.len() + overlay_layers.len())
                .saturating_sub(max_layers)
                .min(overlay_layers.len());
            if excess > 0 {
                crate::warn_once!(
                    "Dropping {} overlays over the runtime's layer limit",
                    excess
                );
            }
            layers.extend(overlay_layers[excess..].iter().map(Deref::deref));
        }

        self.stream
            .end(display_time, xr::EnvironmentBlendMode::OPAQUE, &layers)?;

        trace!("frame submitted");
        Ok(())
    }
}

pub fn is_usable_swapchain<G: xr::Graphics, F>(
    current: &xr::SwapchainCreateInfo<G>,
    creation_format: F,
    new: &xr::SwapchainCreateInfo<G>,
    new_format: F,
) -> bool
where
    F: PartialEq,
{
    creation_format == new_format
        && current.width >= new.width
        && current.height >= new.height
        && current.array_size == new.array_size
        && current.sample_count == new.sample_count
}

fn is_valid_swapchain_info<G: xr::Graphics>(info: &xr::SwapchainCreateInfo<G>) -> bool {
    info.width > 0 && info.height > 0
}

#[cfg(test)]
pub use tests::FakeGraphicsData;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics_backends::{GraphicsBackend, VulkanData};
    use std::cell::Cell;
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::thread_local;
    use vr::EVRCompositorError::*;
    use vr::IVRCompositor028_Interface;

    pub struct FakeGraphicsData {
        vk: Arc<VulkanData>,
        swapchain_format: Option<u32>,
    }
    thread_local! {
        static SWAPCHAIN_WIDTH: Cell<u32> = const { Cell::new(10) };
        static SWAPCHAIN_HEIGHT: Cell<u32> = const { Cell::new(10) };
        static SWAPCHAIN_FORMAT: Cell<u32> = const { Cell::new(0) };
        static SWAPCHAIN_SAMPLE_COUNT: Cell<u32> = const { Cell::new(1) };
    }

    pub enum FakeApi {}
    impl xr::Graphics for FakeApi {
        type Requirements = xr::vulkan::Requirements;
        type SessionCreateInfo = xr::vulkan::SessionCreateInfo;
        type Format = <xr::Vulkan as xr::Graphics>::Format;
        type SwapchainImage = <xr::Vulkan as xr::Graphics>::SwapchainImage;

        fn raise_format(x: i64) -> Self::Format {
            xr::Vulkan::raise_format(x)
        }

        fn lower_format(x: Self::Format) -> i64 {
            xr::Vulkan::lower_format(x)
        }

        fn requirements(
            instance: &openxr::Instance,
            system: openxr::SystemId,
        ) -> openxr::Result<Self::Requirements> {
            xr::Vulkan::requirements(instance, system)
        }

        unsafe fn create_session(
            instance: &openxr::Instance,
            system: openxr::SystemId,
            info: &Self::SessionCreateInfo,
        ) -> openxr::Result<openxr::sys::Session> {
            unsafe { xr::Vulkan::create_session(instance, system, info) }
        }

        fn enumerate_swapchain_images(
            _: &openxr::Swapchain<Self>,
        ) -> openxr::Result<Vec<Self::SwapchainImage>> {
            Ok(Vec::new())
        }
    }

    impl GraphicsBackend for FakeGraphicsData {
        type Api = FakeApi;
        type OpenVrTexture = <VulkanData as GraphicsBackend>::OpenVrTexture;
        type Format = u32;
        type NiceFormat = <VulkanData as GraphicsBackend>::NiceFormat;

        fn from_openxr_format(format: <Self::Api as openxr::Graphics>::Format) -> Self::Format {
            format as _
        }

        fn to_openxr_format(format: Self::Format) -> <Self::Api as openxr::Graphics>::Format {
            format as _
        }

        fn to_nice_format(format: <Self::Api as openxr::Graphics>::Format) -> Self::NiceFormat {
            VulkanData::to_nice_format(format)
        }

        fn session_create_info(&self) -> <Self::Api as openxr::Graphics>::SessionCreateInfo {
            self.vk.session_create_info()
        }

        fn get_texture(texture: &openvr::Texture_t) -> Option<Self::OpenVrTexture> {
            VulkanData::get_texture(texture)
        }

        fn raw_swapchain_info_for_texture(
            &self,
            _: Self::OpenVrTexture,
            _: openvr::VRTextureBounds_t,
            _: openvr::EColorSpace,
        ) -> openxr::SwapchainCreateInfo<Self::Api> {
            xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::EMPTY,
                format: SWAPCHAIN_FORMAT.get(),
                sample_count: SWAPCHAIN_SAMPLE_COUNT.get(),
                width: SWAPCHAIN_WIDTH.get(),
                height: SWAPCHAIN_HEIGHT.get(),
                face_count: 1,
                array_size: 2,
                mip_count: 1,
            }
        }

        fn store_swapchain_images(
            &mut self,
            _images: Vec<<Self::Api as openxr::Graphics>::SwapchainImage>,
            format: u32,
        ) {
            self.swapchain_format.replace(format);
        }

        fn copy_texture_to_swapchain(
            &self,
            _eye: openvr::EVREye,
            _texture: Self::OpenVrTexture,
            _color_space: vr::EColorSpace,
            _bounds: openvr::VRTextureBounds_t,
            _image_index: usize,
            _submit_flags: openvr::EVRSubmitFlags,
        ) -> openxr::Extent2Di {
            xr::Extent2Di::default()
        }

        fn copy_overlay_to_swapchain(
            &mut self,
            _texture: Self::OpenVrTexture,
            _bounds: openvr::VRTextureBounds_t,
            _image_index: usize,
        ) -> openxr::Extent2Di {
            xr::Extent2Di::default()
        }
    }

    impl FakeGraphicsData {
        pub fn texture(data: &Arc<VulkanData>) -> vr::Texture_t {
            vr::Texture_t {
                eType: vr::ETextureType::Reserved,
                handle: Arc::into_raw(data.clone()) as _,
                eColorSpace: vr::EColorSpace::Auto,
            }
        }

        pub fn new(texture: &vr::Texture_t) -> Self {
            assert_eq!(texture.eType, vr::ETextureType::Reserved);
            let ptr = texture.handle as *const VulkanData;
            let vk = unsafe {
                Arc::increment_strong_count(ptr);
                Arc::from_raw(ptr)
            };
            Self {
                vk,
                swapchain_format: Option::None,
            }
        }
    }

    struct Fixture {
        comp: Arc<Compositor>,
        vk: Arc<VulkanData>,
    }

    impl Fixture {
        fn new() -> Self {
            let xr = Arc::new(
                OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap(),
            );
            let vk = Arc::new(VulkanData::new_temporary(&xr.instance, xr.system_id));
            let comp = Arc::new(Compositor::new(xr.clone(), &Injector::default()));
            xr.compositor.set(Arc::downgrade(&comp));
            crate::init_logging();

            Self { comp, vk }
        }

        fn wait_get_poses(&self) -> vr::EVRCompositorError {
            self.comp
                .WaitGetPoses(std::ptr::null_mut(), 0, std::ptr::null_mut(), 0)
        }

        fn submit(&self, eye: vr::EVREye) -> vr::EVRCompositorError {
            self.comp.Submit(
                eye,
                &FakeGraphicsData::texture(&self.vk),
                std::ptr::null(),
                vr::EVRSubmitFlags::Default,
            )
        }

        fn ensure_real_session(&self, explicit: bool) {
            // synchronize session
            assert_eq!(self.wait_get_poses(), None);
            if explicit {
                assert_eq!(self.comp.SubmitExplicitTimingData(), None);
            }
            assert_eq!(self.submit(vr::EVREye::Left), None);
            assert_eq!(self.submit(vr::EVREye::Right), None);
            if explicit {
                self.comp.PostPresentHandoff();
            } else {
                assert_eq!(self.wait_get_poses(), None);
            }

            let data = self.comp.openxr.session_data.get();
            let lock = data.comp_data.0.lock().unwrap();
            let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
                panic!("Frame controller was not set up or not faked!");
            };
            assert!(!ctrl.should_render);
        }

        fn overlay_man(&self) -> Arc<OverlayMan> {
            let overlays = Arc::new(OverlayMan::new(
                self.comp.openxr.clone(),
                &Injector::default(),
            ));
            self.comp.overlays.set(Arc::downgrade(&overlays));
            overlays.compositor.set(Arc::downgrade(&self.comp));
            overlays
        }

        #[track_caller]
        fn check_frame_state(&self, state: fakexr::FrameState) {
            let session = self.comp.openxr.session_data.get().session.as_raw();
            assert_eq!(fakexr::session_frame_state(session), state);
        }

        /// Sets up the System that receives the session's Quit events.
        fn system(&self) -> Arc<System> {
            let system = Arc::new(System::new(self.comp.openxr.clone(), &Injector::default()));
            self.comp.openxr.system.set(Arc::downgrade(&system));
            system
        }

        fn raw_session(&self) -> xr::sys::Session {
            self.comp.openxr.session_data.get().session.as_raw()
        }
    }

    /// Drains the queued events and returns bConnectionLost of each Quit.
    fn quit_events(system: &System) -> Vec<bool> {
        use vr::IVRSystem023_Interface;
        let mut event = vr::VREvent_t::default();
        let mut quits = Vec::new();
        while system.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Standing,
            &mut event,
            std::mem::size_of_val(&event) as u32,
            std::ptr::null_mut(),
        ) {
            if event.eventType == vr::EVREventType::Quit.0 {
                quits.push(unsafe { event.data.process.bConnectionLost });
            }
        }
        quits
    }

    /// Submitting from a headless session used to restart it with the texture's graphics API and
    /// panic, since headless sessions can't be anything but headless.
    #[test]
    fn non_scene_app_submit_is_rejected() {
        crate::init_logging();
        for headless in [true, false] {
            fakexr::set_headless_supported(headless);
            let xr = Arc::new(
                OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Background).unwrap(),
            );
            fakexr::set_headless_supported(false);
            assert_eq!(xr.headless, headless);
            let vk = Arc::new(VulkanData::new_temporary(&xr.instance, xr.system_id));
            let comp = Arc::new(Compositor::new(xr.clone(), &Injector::default()));
            xr.compositor.set(Arc::downgrade(&comp));
            let f = Fixture { comp, vk };

            let session = xr.session_data.get().session.as_raw();
            assert_eq!(f.wait_get_poses(), None);
            assert_eq!(f.submit(vr::EVREye::Left), IsNotSceneApplication);
            assert_eq!(xr.session_data.get().session.as_raw(), session);
            assert!(xr.session_data.get().comp_data.0.lock().unwrap().is_none());
        }
    }

    #[test]
    fn bad_bounds() {
        let f = Fixture::new();

        let bad_bounds = [
            // bound greater than 1 (Superhot does this)
            vr::VRTextureBounds_t {
                vMin: 0.0,
                vMax: 1.1,
                uMin: 0.0,
                uMax: 1.0,
            },
            // bound greater than 1
            vr::VRTextureBounds_t {
                vMin: 0.0,
                vMax: 1.0,
                uMin: 0.0,
                uMax: 1.1,
            },
            // negative bound
            vr::VRTextureBounds_t {
                vMin: -0.1,
                vMax: 1.0,
                uMin: 0.0,
                uMax: 1.0,
            },
            // min/max equal
            vr::VRTextureBounds_t {
                vMin: 1.0,
                vMax: 1.0,
                uMin: 0.0,
                uMax: 1.0,
            },
        ];

        for bound in bad_bounds {
            assert_eq!(
                f.comp.Submit(
                    vr::EVREye::Left,
                    &FakeGraphicsData::texture(&f.vk),
                    &bound,
                    vr::EVRSubmitFlags::Default,
                ),
                InvalidBounds,
                "Bound didn't return InvalidBounds: {bound:?}"
            );
        }
    }

    #[test]
    fn allow_flipped_bounds() {
        let Fixture { comp, .. } = Fixture::new();

        assert_ne!(
            comp.Submit(
                vr::EVREye::Left,
                std::ptr::null(),
                &vr::VRTextureBounds_t {
                    uMin: 0.0,
                    vMin: 1.0,
                    uMax: 1.0,
                    vMax: 0.0
                },
                vr::EVRSubmitFlags::Default
            ),
            InvalidBounds
        );
    }

    #[test]
    fn error_on_multiple_same_eye_submit() {
        let f = Fixture::new();

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);
        assert_eq!(f.submit(vr::EVREye::Left), AlreadySubmitted);

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
    }

    #[test]
    fn unknown_enum_values() {
        let f = Fixture::new();
        f.comp.SetExplicitTimingMode(vr::EVRCompositorTimingMode(7));
        assert_eq!(
            *f.comp.timing_mode.lock().unwrap(),
            vr::EVRCompositorTimingMode::Implicit
        );

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye(2)), IndexOutOfRange);
        assert_eq!(f.submit(vr::EVREye(u32::MAX)), IndexOutOfRange);

        // An uninitialized color space is treated like Auto.
        let mut texture = FakeGraphicsData::texture(&f.vk);
        texture.eColorSpace = vr::EColorSpace(0xdead);
        assert_eq!(
            f.comp.Submit(
                vr::EVREye::Left,
                &texture,
                std::ptr::null(),
                vr::EVRSubmitFlags::Default
            ),
            None
        );
        assert_eq!(f.submit(vr::EVREye::Right), None);
    }

    #[test]
    fn allow_waitgetposes_without_submit() {
        let f = Fixture::new();

        // Enable frame controller
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.wait_get_poses(), None);
    }

    #[test]
    fn wait_get_poses_fills_every_slot() {
        const COUNT: usize = vr::k_unMaxTrackedDeviceCount as usize;
        let garbage = vr::TrackedDevicePose_t {
            mDeviceToAbsoluteTracking: vr::HmdMatrix34_t { m: [[1.0; 4]; 3] },
            eTrackingResult: vr::ETrackingResult::Running_OK,
            bPoseIsValid: true,
            bDeviceIsConnected: true,
            ..Default::default()
        };
        let assert_hmd = |pose: &vr::TrackedDevicePose_t| {
            assert!(pose.bPoseIsValid);
            assert!(pose.bDeviceIsConnected);
        };
        // only the HMD and both (unbound) controllers exist
        let assert_no_device = |poses: &[vr::TrackedDevicePose_t]| {
            for (i, pose) in poses.iter().enumerate().skip(3) {
                assert!(!pose.bPoseIsValid, "slot {i}");
                assert!(!pose.bDeviceIsConnected, "slot {i}");
                assert_eq!(
                    pose.eTrackingResult,
                    vr::ETrackingResult::Uninitialized,
                    "slot {i}"
                );
                assert_eq!(pose.mDeviceToAbsoluteTracking.m, [[0.0; 4]; 3], "slot {i}");
            }
        };

        let f = Fixture::new();
        let mut render = [garbage; COUNT];
        let mut game = [garbage; COUNT];

        // render array only
        assert_eq!(
            f.comp
                .WaitGetPoses(render.as_mut_ptr(), COUNT as u32, std::ptr::null_mut(), 0),
            None
        );
        assert_hmd(&render[0]);
        assert_no_device(&render);

        // game array only
        assert_eq!(
            f.comp
                .WaitGetPoses(std::ptr::null_mut(), 0, game.as_mut_ptr(), COUNT as u32),
            None
        );
        assert_hmd(&game[0]);
        assert_no_device(&game);

        // game array larger than the render array
        let mut render = [garbage; 1];
        let mut game = [garbage; COUNT];
        assert_eq!(
            f.comp
                .WaitGetPoses(render.as_mut_ptr(), 1, game.as_mut_ptr(), COUNT as u32),
            None
        );
        assert_hmd(&render[0]);
        assert_hmd(&game[0]);
        assert_no_device(&game);

        // null array with a count
        let mut game = [garbage; COUNT];
        assert_eq!(
            f.comp
                .WaitGetPoses(std::ptr::null_mut(), 16, game.as_mut_ptr(), COUNT as u32),
            None
        );
        assert_hmd(&game[0]);
        assert_no_device(&game);
    }

    #[test]
    fn last_pose_for_device_without_a_device() {
        let f = Fixture::new();
        let mut pose = vr::TrackedDevicePose_t {
            bPoseIsValid: true,
            bDeviceIsConnected: true,
            ..Default::default()
        };
        assert_eq!(
            f.comp
                .GetLastPoseForTrackedDeviceIndex(10, &mut pose, std::ptr::null_mut()),
            None
        );
        assert!(!pose.bPoseIsValid);
        assert!(!pose.bDeviceIsConnected);

        assert_eq!(
            f.comp.GetLastPoseForTrackedDeviceIndex(
                vr::k_unMaxTrackedDeviceCount,
                &mut pose,
                std::ptr::null_mut()
            ),
            IndexOutOfRange
        );
    }

    #[test]
    fn get_frame_timings() {
        let f = Fixture::new();
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        let size = std::mem::size_of::<vr::Compositor_FrameTiming>() as u32;
        let mut timings: [vr::Compositor_FrameTiming; 3] =
            std::array::from_fn(|_| Default::default());
        // Only the first entry's size is set, the rest is inferred from it.
        timings[0].m_nSize = size;
        assert_eq!(
            vr::IVRCompositor029_Interface::GetFrameTimings(&*f.comp, timings.as_mut_ptr(), 3),
            3
        );
        assert_eq!(timings[1].m_nSize, size);
        assert_eq!(timings[2].m_nSize, size);

        assert_eq!(
            vr::IVRCompositor029_Interface::GetFrameTimings(&*f.comp, std::ptr::null_mut(), 3),
            0
        );

        timings[0].m_nSize = 0;
        assert_eq!(
            vr::IVRCompositor029_Interface::GetFrameTimings(&*f.comp, timings.as_mut_ptr(), 3),
            0
        );
    }

    #[test]
    fn recreate_swapchain() {
        let f = Fixture::new();
        f.ensure_real_session(false);

        let get_swapchain_width = || {
            let data = f.comp.openxr.session_data.get();
            let lock = data.comp_data.0.lock().unwrap();
            let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
                panic!("Frame controller was not set up or not faked!");
            };
            ctrl.swapchain_data
                .as_ref()
                .expect("swapchain info missing")
                .info
                .width
        };

        let old_width = get_swapchain_width();
        SWAPCHAIN_WIDTH.set(40);
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);
        let new_width = get_swapchain_width();
        assert_ne!(old_width, new_width);

        assert_eq!(f.wait_get_poses(), None);
        SWAPCHAIN_WIDTH.set(20);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        let newer_width = get_swapchain_width();
        assert_eq!(newer_width, new_width);
    }

    #[test]
    fn get_frame_timing() {
        let f = Fixture::new();
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        let mut timing = MaybeUninit::new(vr::Compositor_FrameTiming::default());
        unsafe {
            (&raw mut (*timing.as_mut_ptr()).m_nSize)
                .write(std::mem::size_of::<vr::Compositor_FrameTiming>() as u32);
        }
        assert!(f.comp.GetFrameTiming(timing.as_mut_ptr(), 1));
        let small_size = std::mem::offset_of!(vr::Compositor_FrameTiming, m_HmdPose)
            + std::mem::size_of::<vr::TrackedDevicePose_t>();
        unsafe {
            (&raw mut (*timing.as_mut_ptr()).m_nSize).write(small_size as u32);
        }
        assert!(f.comp.GetFrameTiming(timing.as_mut_ptr(), 1));
        unsafe {
            (&raw mut (*timing.as_mut_ptr()).m_nSize).write(0);
        }
        assert!(!f.comp.GetFrameTiming(timing.as_mut_ptr(), 1));
    }

    #[test]
    fn get_frame_timing_legacy() {
        let f = Fixture::new();
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);
        let interval = 1000.0 / f.comp.openxr.get_refresh_rate();

        let mut timing = vr::vr_1_0_3::Compositor_FrameTiming {
            m_nSize: std::mem::size_of::<vr::vr_1_0_3::Compositor_FrameTiming>() as u32,
            ..Default::default()
        };
        assert!(vr::IVRCompositor016On017::GetFrameTiming(
            &*f.comp,
            &mut timing,
            0
        ));
        let (size, frame_interval) = (timing.m_nSize, timing.m_flClientFrameIntervalMs);
        assert_eq!(
            size as usize,
            std::mem::size_of::<vr::vr_1_0_3::Compositor_FrameTiming>()
        );
        assert_eq!(frame_interval, interval);

        let mut timing = vr::vr_0_9_17::Compositor_FrameTiming {
            m_nSize: std::mem::size_of::<vr::vr_0_9_17::Compositor_FrameTiming>() as u32,
            ..Default::default()
        };
        assert!(vr::IVRCompositor012On013::GetFrameTiming(
            &*f.comp,
            &mut timing,
            0
        ));
        let frame_interval = timing.m_flClientFrameIntervalMs;
        assert_eq!(frame_interval, interval);

        let mut timing = vr::vr_0_9_12::Compositor_FrameTiming {
            size: std::mem::size_of::<vr::vr_0_9_12::Compositor_FrameTiming>() as u32,
            ..Default::default()
        };
        assert!(vr::IVRCompositor009On010::GetFrameTiming(
            &*f.comp,
            &mut timing,
            0
        ));
        let frame_interval = timing.m_flFrameIntervalMs;
        assert_eq!(frame_interval, interval);

        let mut timing = vr::vr_0_9_14::Compositor_FrameTiming {
            size: std::mem::size_of::<vr::vr_0_9_14::Compositor_FrameTiming>() as u32,
            ..Default::default()
        };
        assert!(vr::IVRCompositor010On011::GetFrameTiming(
            &*f.comp,
            &mut timing,
            0
        ));
        let frame_interval = timing.m_flFrameIntervalMs;
        assert_eq!(frame_interval, interval);

        // Too small for the struct the version expects.
        let mut timing = vr::vr_1_0_3::Compositor_FrameTiming {
            m_nSize: 4,
            ..Default::default()
        };
        assert!(!vr::IVRCompositor016On017::GetFrameTiming(
            &*f.comp,
            &mut timing,
            0
        ));
        assert!(!vr::IVRCompositor016On017::GetFrameTiming(
            &*f.comp,
            std::ptr::null_mut(),
            0
        ));
    }

    #[test]
    fn zero_sample_count_texture() {
        // Games submit sample count 0 when MSAA is disabled (e.g. HL2VR with
        // antialiasing set to None) - it should be treated as 1, not crash
        // swapchain creation.
        let f = Fixture::new();
        SWAPCHAIN_SAMPLE_COUNT.set(0);

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        assert_eq!(f.wait_get_poses(), None);
        {
            let data = f.comp.openxr.session_data.get();
            let lock = data.comp_data.0.lock().unwrap();
            let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
                panic!("Frame controller was not set up or not faked!");
            };
            let swapchain_data = ctrl.swapchain_data.as_ref().expect("no swapchain created");
            assert_eq!(swapchain_data.info.sample_count, 1);
        }
        SWAPCHAIN_SAMPLE_COUNT.set(1);
    }

    #[test]
    fn views_never_located() {
        let f = Fixture::new();
        fakexr::set_locate_views_error(Some(xr::sys::Result::ERROR_RUNTIME_FAILURE));
        f.ensure_real_session(false);
        let frame = || {
            assert_eq!(f.submit(vr::EVREye::Left), None);
            assert_eq!(f.submit(vr::EVREye::Right), None);
            assert_eq!(f.wait_get_poses(), None);
            let session = f.comp.openxr.session_data.get().session.as_raw();
            fakexr::last_layer_count(session)
        };

        // The session renders from the second frame on. Without views, there's no FOV for the
        // projection layer.
        assert_eq!(frame(), 0);
        assert_eq!(frame(), 0);
        fakexr::set_locate_views_error(Option::None);
        assert_eq!(frame(), 1);
    }

    #[test]
    fn zero_dims_texture() {
        let f = Fixture::new();
        SWAPCHAIN_WIDTH.set(0);
        SWAPCHAIN_HEIGHT.set(0);

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        assert_eq!(f.wait_get_poses(), None);
        {
            let data = f.comp.openxr.session_data.get();
            let lock = data.comp_data.0.lock().unwrap();
            let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
                panic!("Frame controller was not set up or not faked!");
            };
            assert!(ctrl.swapchain_data.is_none());
            assert!(!ctrl.should_render);
        }
        SWAPCHAIN_WIDTH.set(10);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        assert_eq!(f.wait_get_poses(), None);
        {
            let data = f.comp.openxr.session_data.get();
            let lock = data.comp_data.0.lock().unwrap();
            let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
                panic!("Frame controller was not set up or not faked!");
            };
            assert!(ctrl.swapchain_data.is_none());
            assert!(ctrl.should_render);
        }
        SWAPCHAIN_HEIGHT.set(10);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        assert_eq!(f.wait_get_poses(), None);
        {
            let data = f.comp.openxr.session_data.get();
            let lock = data.comp_data.0.lock().unwrap();
            let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
                panic!("Frame controller was not set up or not faked!");
            };
            assert!(ctrl.swapchain_data.is_some());
            assert!(ctrl.should_render);
        }
    }

    #[test]
    fn vulkan_extensions() {
        let f = Fixture::new();

        fn tst(func: impl Fn(*mut c_char, u32) -> u32, dbg: &str) {
            // Normal flow
            let size = func(std::ptr::null_mut(), 0);
            let mut exts = vec![0; size as usize];
            func(exts.as_mut_ptr(), exts.len() as u32);

            let data = unsafe { CStr::from_ptr(exts.as_ptr()) };
            assert_eq!(data, c"VK_foo VK_bar", "{dbg}");

            // Oversized buffer
            let mut exts = vec![0; size as usize * 2];
            func(exts.as_mut_ptr(), exts.len() as u32);

            let data = unsafe { CStr::from_ptr(exts.as_ptr()) };
            assert_eq!(data, c"VK_foo VK_bar", "{dbg}");

            // Undersized buffer - should not crash
            let mut exts = vec![0];
            func(exts.as_mut_ptr(), exts.len() as u32);
        }

        tst(
            |buf, size| f.comp.GetVulkanInstanceExtensionsRequired(buf, size),
            "instance exts",
        );
        tst(
            |buf, size| {
                f.comp
                    .GetVulkanDeviceExtensionsRequired(std::ptr::null_mut(), buf, size)
            },
            "device exts",
        );
    }

    #[test]
    fn unsupported_format() {
        let f = Fixture::new();
        SWAPCHAIN_FORMAT.set(1);
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);
        let data = f.comp.openxr.session_data.get();
        let lock = data.comp_data.0.lock().unwrap();
        let DynFrameController::Fake(ctrl) = lock.as_ref().unwrap() else {
            panic!("Frame controller was not set up or not faked!");
        };
        let data = ctrl
            .swapchain_data
            .as_ref()
            .expect("Swapchain data is missing");
        assert_eq!(data.initial_format, 1);
        assert_eq!(data.info.format, 0);
    }

    #[test]
    fn explicit_timing() {
        let f = Fixture::new();
        f.ensure_real_session(false);

        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );
        assert_eq!(f.wait_get_poses(), None);
        f.check_frame_state(fakexr::FrameState::Waited);

        assert_eq!(f.comp.SubmitExplicitTimingData(), None);
        f.check_frame_state(fakexr::FrameState::Begun);

        assert_eq!(f.submit(vr::EVREye::Left), None);
        f.check_frame_state(fakexr::FrameState::Begun);
        assert_eq!(f.submit(vr::EVREye::Right), None);
        f.check_frame_state(fakexr::FrameState::Begun);

        f.comp.PostPresentHandoff();
        f.check_frame_state(fakexr::FrameState::Ended);
    }

    #[test]
    fn explicit_timing_no_submit() {
        let f = Fixture::new();
        f.ensure_real_session(false);

        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.comp.SubmitExplicitTimingData(), None);
        f.comp.PostPresentHandoff();
        f.check_frame_state(fakexr::FrameState::Ended);
    }

    #[test]
    fn explicit_timing_multiple_waitgetposes() {
        let f = Fixture::new();
        f.ensure_real_session(false);
        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );

        assert_eq!(f.wait_get_poses(), None);
        f.check_frame_state(fakexr::FrameState::Waited);
        assert_eq!(f.wait_get_poses(), None);
        f.check_frame_state(fakexr::FrameState::Waited);
    }

    #[test]
    fn explicit_timing_multiple_submit_explicit_timing_data() {
        let f = Fixture::new();
        f.ensure_real_session(false);
        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );

        assert_eq!(f.comp.SubmitExplicitTimingData(), None);
        f.check_frame_state(fakexr::FrameState::Begun);
        assert_eq!(f.comp.SubmitExplicitTimingData(), None);
        f.check_frame_state(fakexr::FrameState::Begun);
    }

    #[test]
    fn explicit_timing_session_restart_after_waitgetposes() {
        let f = Fixture::new();
        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );

        f.ensure_real_session(true);
        f.comp.openxr.restart_session().unwrap();
        assert_eq!(f.wait_get_poses(), None);

        f.check_frame_state(fakexr::FrameState::Waited);
    }

    #[test]
    fn explicit_timing_submit_without_begun_frame() {
        let f = Fixture::new();
        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );

        // The first Submit restarts the session while no frame has been begun, so the new swapchain has no acquired image when both eyes have been submitted
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        // The next frame should proceed as usual
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.comp.SubmitExplicitTimingData(), None);
        f.check_frame_state(fakexr::FrameState::Begun);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);
        f.comp.PostPresentHandoff();
        f.check_frame_state(fakexr::FrameState::Ended);
    }

    #[test]
    fn explicit_timing_unfocused() {
        let f = Fixture::new();
        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );

        f.comp.SubmitExplicitTimingData();
        assert_eq!(f.submit(vr::EVREye::Left), DoNotHaveFocus);
        assert_eq!(f.submit(vr::EVREye::Right), DoNotHaveFocus);

        assert_eq!(f.wait_get_poses(), None);
        f.comp.SubmitExplicitTimingData();
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);
    }

    #[test]
    fn submit_overlay_without_projection_layer() {
        use crate::overlay::OverlayMan;
        use vr::IVROverlay027_Interface;

        let f = Fixture::new();
        let overlays = Arc::new(OverlayMan::new(f.comp.openxr.clone(), &Injector::default()));
        f.comp.overlays.set(Arc::downgrade(&overlays));
        overlays.compositor.set(Arc::downgrade(&f.comp));

        let mut overlay = 0;
        assert_eq!(
            overlays.CreateOverlay(
                c"test_overlay".as_ptr(),
                c"TestOverlay".as_ptr(),
                &mut overlay
            ),
            vr::EVROverlayError::None
        );

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(
            overlays.SetOverlayTexture(overlay, &FakeGraphicsData::texture(&f.vk)),
            vr::EVROverlayError::None
        );
        f.check_frame_state(fakexr::FrameState::Begun);
        assert_eq!(overlays.ShowOverlay(overlay), vr::EVROverlayError::None);
        f.comp.PostPresentHandoff();
        f.check_frame_state(fakexr::FrameState::Ended);
    }

    /// Submit and the overlay calls can race to set up the real session. The one that comes second
    /// must not restart it again.
    #[test]
    fn real_session_set_up_once() {
        let f = Fixture::new();
        let texture = FakeGraphicsData::texture(&f.vk);
        let bounds = vr::VRTextureBounds_t {
            uMin: 0.0,
            vMin: 0.0,
            uMax: 1.0,
            vMax: 1.0,
        };
        assert_eq!(f.comp.initialize_real_session(&texture, bounds), Ok(()));

        let (tx, rx) = std::sync::mpsc::channel();
        fakexr::on_next_end(
            f.comp.openxr.session_data.get().session.as_raw(),
            move || {
                let _ = tx.send(());
            },
        );
        assert_eq!(f.comp.initialize_real_session(&texture, bounds), Ok(()));
        assert!(rx.try_recv().is_err(), "real session was restarted");
    }

    /// Overlays keep their rect across a session restart, but the new session has no swapchain
    /// for them until they get a new texture.
    #[test]
    fn overlay_without_swapchain_after_restart() {
        use vr::IVROverlay027_Interface;

        let f = Fixture::new();
        let overlays = f.overlay_man();

        let create = |key: &CStr| {
            let mut overlay = 0;
            assert_eq!(
                overlays.CreateOverlay(key.as_ptr(), key.as_ptr(), &mut overlay),
                vr::EVROverlayError::None
            );
            assert_eq!(overlays.ShowOverlay(overlay), vr::EVROverlayError::None);
            overlay
        };
        let set_texture = |overlay| {
            assert_eq!(
                overlays.SetOverlayTexture(overlay, &FakeGraphicsData::texture(&f.vk)),
                vr::EVROverlayError::None
            );
        };
        let first = create(c"first");
        let second = create(c"second");

        assert_eq!(f.wait_get_poses(), None);
        set_texture(first);
        set_texture(second);
        f.comp.PostPresentHandoff();

        f.comp.openxr.restart_session().unwrap();
        assert_eq!(f.wait_get_poses(), None);
        set_texture(first);
        f.check_frame_state(fakexr::FrameState::Begun);
        f.comp.PostPresentHandoff();
        f.check_frame_state(fakexr::FrameState::Ended);
    }

    /// The overlay layers refer to the overlay swapchains, so another thread must not be able to
    /// replace them until xrEndFrame is done.
    #[test]
    fn overlay_swapchains_locked_during_end_frame() {
        use vr::IVROverlay027_Interface;

        let f = Fixture::new();
        let overlays = f.overlay_man();
        let mut overlay = 0;
        assert_eq!(
            overlays.CreateOverlay(c"key".as_ptr(), c"key".as_ptr(), &mut overlay),
            vr::EVROverlayError::None
        );
        assert_eq!(overlays.ShowOverlay(overlay), vr::EVROverlayError::None);

        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(
            overlays.SetOverlayTexture(overlay, &FakeGraphicsData::texture(&f.vk)),
            vr::EVROverlayError::None
        );
        assert_eq!(f.wait_get_poses(), None);
        f.check_frame_state(fakexr::FrameState::Begun);

        let (tx, rx) = std::sync::mpsc::channel();
        let openxr = Arc::downgrade(&f.comp.openxr);
        fakexr::on_next_end_frame(
            f.comp.openxr.session_data.get().session.as_raw(),
            move || {
                let openxr = openxr.upgrade().unwrap();
                let locked = openxr.session_data.get().overlay_data.swapchains_locked();
                tx.send(locked).unwrap();
            },
        );
        f.comp.PostPresentHandoff();
        let locked = rx.try_recv().expect("frame was not ended");
        assert!(locked, "overlay swapchains unlocked during xrEndFrame");
    }

    #[test]
    fn compositor_stub_queries() {
        use vr::IVRCompositor029_Interface as C29;
        let f = Fixture::new();
        let c = &*f.comp;

        assert!(!C29::IsMotionSmoothingSupported(c));
        assert!(!C29::IsMotionSmoothingEnabled(c));
        assert!(!C29::IsMirrorWindowVisible(c));
        assert_eq!(C29::GetLastFrameRenderer(c), std::process::id());
        assert_eq!(C29::GetCurrentSceneFocusProcess(c), std::process::id());
        for background in [false, true] {
            let color = C29::GetCurrentFadeColor(c, background);
            assert_eq!([color.r, color.g, color.b, color.a], [0.0; 4]);
        }
    }

    #[test]
    fn compositor_stub_noops() {
        use vr::IVRCompositor029_Interface as C29;
        let f = Fixture::new();
        let c = &*f.comp;

        C29::UnlockGLSharedTextureForAccess(c, std::ptr::null_mut());
        C29::LockGLSharedTextureForAccess(c, std::ptr::null_mut());
        assert!(!C29::ReleaseSharedGLTexture(c, 1, std::ptr::null_mut()));
        C29::ReleaseMirrorTextureD3D11(c, std::ptr::null_mut());
        C29::ForceReconnectProcess(c);
        C29::CompositorDumpImages(c);
        C29::HideMirrorWindow(c);
        C29::ShowMirrorWindow(c);
        C29::CompositorQuit(c);
    }

    #[test]
    fn get_mirror_texture_gl_zeroes_outputs() {
        use vr::IVRCompositor029_Interface as C29;
        let f = Fixture::new();
        let c = &*f.comp;
        let eye = vr::EVREye::Left;
        let handle: vr::glSharedTextureHandle_t = std::ptr::dangling_mut();

        let (mut tex, mut shared) = (7, handle);
        assert_eq!(
            C29::GetMirrorTextureGL(c, eye, &mut tex, &mut shared),
            IncompatibleVersion
        );
        assert_eq!(tex, 0);
        assert!(shared.is_null());

        // each output pointer is optional
        let (mut tex, mut shared) = (7, handle);
        assert_eq!(
            C29::GetMirrorTextureGL(c, eye, &mut tex, std::ptr::null_mut()),
            IncompatibleVersion
        );
        assert_eq!(tex, 0);
        assert_eq!(
            C29::GetMirrorTextureGL(c, eye, std::ptr::null_mut(), &mut shared),
            IncompatibleVersion
        );
        assert!(shared.is_null());
        let (null_tex, null_shared) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(
            C29::GetMirrorTextureGL(c, eye, null_tex, null_shared),
            IncompatibleVersion
        );
    }

    #[test]
    fn get_cumulative_stats_zeroes_requested_bytes() {
        use vr::IVRCompositor029_Interface as C29;
        // The canary sits right after the stats and must never be written.
        #[repr(C)]
        struct Buffer {
            stats: vr::Compositor_CumulativeStats,
            canary: u64,
        }
        fn buffer() -> Buffer {
            let mut buf = MaybeUninit::<Buffer>::uninit();
            unsafe {
                buf.as_mut_ptr().write_bytes(0xff, 1);
                buf.assume_init()
            }
        }
        let size = size_of::<vr::Compositor_CumulativeStats>();
        let f = Fixture::new();
        let c = &*f.comp;

        // a size larger than the struct is clamped to the struct
        let mut buf = buffer();
        C29::GetCumulativeStats(c, &mut buf.stats, size_of::<Buffer>() as u32);
        assert_eq!(buf.stats.m_nPid, 0);
        assert_eq!(buf.stats.m_nNumFrameSubmits, 0);
        assert_eq!(buf.stats.m_flSumCompositorCPUTimeMS, 0.0);
        assert_eq!(buf.canary, u64::MAX);

        // an older, smaller struct only has its own bytes zeroed
        let mut buf = buffer();
        C29::GetCumulativeStats(c, &mut buf.stats, 8);
        assert_eq!(buf.stats.m_nPid, 0);
        assert_eq!(buf.stats.m_nNumFramePresents, 0);
        assert_eq!(buf.stats.m_nNumDroppedFrames, u32::MAX);

        C29::GetCumulativeStats(c, std::ptr::null_mut(), size as u32);
    }

    #[test]
    fn legacy_screenshot_stubs() {
        let f = Fixture::new();

        assert_eq!(
            <Compositor as vr::IVRCompositor015On016>::RequestScreenshot(
                &f.comp,
                vr::EVRScreenshotType::Stereo,
                c"/tmp/shot.png".as_ptr(),
                c"/tmp/shot_vr.png".as_ptr()
            ),
            RequestFailed
        );
        assert_eq!(
            <Compositor as vr::IVRCompositor015On016>::RequestScreenshot(
                &f.comp,
                vr::EVRScreenshotType::None,
                std::ptr::null(),
                std::ptr::null()
            ),
            RequestFailed
        );
        assert_eq!(
            <Compositor as vr::IVRCompositor015On016>::GetCurrentScreenshotType(&f.comp),
            vr::EVRScreenshotType::None
        );
    }

    /// The session being lost mid frame (e.g. the WiVRn connection dropping) must not abort the
    /// game: it is told to quit once, and the frame calls fail until it does.
    #[test]
    fn session_lost_mid_frame() {
        let f = Fixture::new();
        let system = f.system();
        f.ensure_real_session(false);
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        // xrEndFrame fails before the loss event is polled.
        fakexr::lose_session(f.raw_session());
        f.comp.PostPresentHandoff();
        assert!(f.comp.openxr.is_lost());

        let valid = vr::TrackedDevicePose_t {
            bPoseIsValid: true,
            ..Default::default()
        };
        let mut poses = [valid; 2];
        let mut game_poses = [valid; 1];
        assert_eq!(
            f.comp
                .WaitGetPoses(poses.as_mut_ptr(), 2, game_poses.as_mut_ptr(), 1),
            DoNotHaveFocus
        );
        assert!(poses.iter().chain(&game_poses).all(|p| !p.bPoseIsValid));
        assert_eq!(f.submit(vr::EVREye::Left), DoNotHaveFocus);
        f.comp.PostPresentHandoff();
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert_eq!(quit_events(&system), [true]);
    }

    /// A loss first seen when Submit recreates the swapchain for a resized eye texture.
    #[test]
    fn session_lost_on_swapchain_recreation() {
        let f = Fixture::new();
        let system = f.system();
        f.ensure_real_session(false);
        assert_eq!(f.wait_get_poses(), None);

        fakexr::lose_session(f.raw_session());
        SWAPCHAIN_WIDTH.set(40);
        assert_eq!(f.submit(vr::EVREye::Left), RequestFailed);
        assert!(f.comp.openxr.is_lost());
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert_eq!(quit_events(&system), [true]);
    }

    /// A loss first seen when acquiring the swapchain image to begin the frame.
    #[test]
    fn session_lost_on_frame_begin() {
        let f = Fixture::new();
        let system = f.system();
        f.comp.SetExplicitTimingMode(
            vr::EVRCompositorTimingMode::Explicit_ApplicationPerformsPostPresentHandoff,
        );
        f.ensure_real_session(true);
        assert_eq!(f.wait_get_poses(), None);

        fakexr::lose_session(f.raw_session());
        assert_eq!(f.comp.SubmitExplicitTimingData(), None);
        assert!(f.comp.openxr.is_lost());
        f.comp.PostPresentHandoff();
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert_eq!(quit_events(&system), [true]);
    }

    /// A loss first seen when SetOverlayTexture creates the overlay's swapchain.
    #[test]
    fn session_lost_on_overlay_texture() {
        use vr::IVROverlay027_Interface;

        let f = Fixture::new();
        let system = f.system();
        let overlays = f.overlay_man();
        f.ensure_real_session(false);
        let mut overlay = 0;
        assert_eq!(
            overlays.CreateOverlay(c"overlay".as_ptr(), c"overlay".as_ptr(), &mut overlay),
            vr::EVROverlayError::None
        );

        fakexr::lose_session(f.raw_session());
        assert_eq!(
            overlays.SetOverlayTexture(overlay, &FakeGraphicsData::texture(&f.vk)),
            vr::EVROverlayError::RequestFailed
        );
        assert!(f.comp.openxr.is_lost());
        assert_eq!(quit_events(&system), [true]);
    }

    /// WiVRn stops the app and loses its session if it doesn't quit in time. The app is only told
    /// to quit once.
    #[test]
    fn session_stopped_then_lost() {
        let f = Fixture::new();
        let system = f.system();
        f.ensure_real_session(false);

        f.comp
            .openxr
            .session_data
            .get()
            .session
            .request_exit()
            .unwrap();
        assert_eq!(f.wait_get_poses(), None);
        fakexr::lose_session(f.raw_session());
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert!(f.comp.openxr.is_lost());
        assert_eq!(quit_events(&system), [false]);
    }

    /// A loss first seen when the frame's views are located at the end of the frame, because
    /// locating them at its start failed.
    #[test]
    fn session_lost_on_locate_views() {
        let f = Fixture::new();
        let system = f.system();
        f.ensure_real_session(false);
        fakexr::set_locate_views_error(Some(xr::sys::Result::ERROR_RUNTIME_FAILURE));
        assert_eq!(f.wait_get_poses(), None);
        fakexr::set_locate_views_error(Option::None);
        assert_eq!(f.submit(vr::EVREye::Left), None);
        assert_eq!(f.submit(vr::EVREye::Right), None);

        fakexr::lose_session(f.raw_session());
        f.comp.PostPresentHandoff();
        assert!(f.comp.openxr.is_lost());
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert_eq!(quit_events(&system), [true]);
    }

    #[test]
    fn instance_lost() {
        let f = Fixture::new();
        let system = f.system();
        f.ensure_real_session(false);

        fakexr::lose_instance(f.comp.openxr.instance.as_raw());
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert_eq!(quit_events(&system), [true]);
    }

    /// The runtime asking the app to quit before its first frame, while it's still on the
    /// temporary session, must reach the app. Submit then must not start a new session.
    #[test]
    fn stopped_on_temporary_session() {
        let f = Fixture::new();
        let system = f.system();

        f.comp
            .openxr
            .session_data
            .get()
            .session
            .request_exit()
            .unwrap();
        assert_eq!(f.wait_get_poses(), None);
        assert_eq!(quit_events(&system), [false]);
        assert_eq!(f.submit(vr::EVREye::Left), DoNotHaveFocus);
        assert!(!f.comp.openxr.session_data.get().is_real_session());

        // No frame is ever waited, so WaitGetPoses has to keep a game ignoring Quit from spinning.
        let start = Instant::now();
        assert_eq!(f.wait_get_poses(), None);
        assert!(start.elapsed() >= std::time::Duration::from_millis(1));
    }

    /// A runtime rejecting the real session (e.g. its graphics binding) must leave the game running
    /// on the temporary session, without tearing that down again on every Submit.
    #[test]
    fn real_session_creation_failure() {
        let f = Fixture::new();
        assert_eq!(f.wait_get_poses(), None);

        fakexr::fail_next_session_creation(xr::sys::Result::ERROR_GRAPHICS_DEVICE_INVALID);
        assert_eq!(f.submit(vr::EVREye::Left), InvalidTexture);
        assert!(!f.comp.openxr.session_data.get().is_real_session());

        let (tx, rx) = std::sync::mpsc::channel();
        fakexr::on_next_end(f.raw_session(), move || {
            let _ = tx.send(());
        });
        let start = Instant::now();
        assert_eq!(f.wait_get_poses(), None);
        assert!(start.elapsed() >= std::time::Duration::from_millis(1));
        assert_eq!(f.submit(vr::EVREye::Left), InvalidTexture);
        assert!(rx.try_recv().is_err(), "temporary session was restarted");
    }

    /// A runtime lost while the real session is created must tell the game to quit.
    #[test]
    fn session_lost_on_real_session_creation() {
        let f = Fixture::new();
        let system = f.system();
        assert_eq!(f.wait_get_poses(), None);

        fakexr::fail_next_session_creation(xr::sys::Result::ERROR_INSTANCE_LOST);
        assert_eq!(f.submit(vr::EVREye::Left), InvalidTexture);
        assert!(f.comp.openxr.is_lost());
        assert_eq!(quit_events(&system), [true]);
        assert_eq!(f.wait_get_poses(), DoNotHaveFocus);
        assert_eq!(f.submit(vr::EVREye::Left), DoNotHaveFocus);
    }

    /// More visible overlays than the runtime's layer limit must not make xrEndFrame fail.
    #[test]
    fn overlays_over_layer_limit() {
        use vr::IVROverlay027_Interface;

        let f = Fixture::new();
        let overlays = f.overlay_man();
        assert_eq!(f.wait_get_poses(), None);
        for i in 0..=f.comp.openxr.max_layer_count {
            let key = std::ffi::CString::new(format!("overlay{i}")).unwrap();
            let mut overlay = 0;
            assert_eq!(
                overlays.CreateOverlay(key.as_ptr(), key.as_ptr(), &mut overlay),
                vr::EVROverlayError::None
            );
            assert_eq!(overlays.ShowOverlay(overlay), vr::EVROverlayError::None);
            assert_eq!(
                overlays.SetOverlayTexture(overlay, &FakeGraphicsData::texture(&f.vk)),
                vr::EVROverlayError::None
            );
        }
        f.check_frame_state(fakexr::FrameState::Begun);
        f.comp.PostPresentHandoff();
        f.check_frame_state(fakexr::FrameState::Ended);
    }

    #[test]
    fn late_action_manifest_does_not_quit() {
        use crate::input::Input;
        use crate::system::System;
        use vr::{IVRInput010_Interface, IVRSystem023_Interface};

        let f = Fixture::new();
        let xr = f.comp.openxr.clone();
        let input = Arc::new(Input::new(xr.clone()));
        xr.input.set(Arc::downgrade(&input));
        let system = Arc::new(System::new(xr.clone(), &Injector::default()));
        xr.system.set(Arc::downgrade(&system));

        f.ensure_real_session(false);
        input.frame_start_update(); // attach legacy actions

        // Attaching an action manifest now forces a session restart.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/input_data/actions.json\0"
        );
        assert_eq!(
            input.SetActionManifestPath(path.as_ptr() as _),
            vr::EVRInputError::None
        );
        xr.poll_events();

        let mut event = vr::VREvent_t::default();
        while system.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Standing,
            &mut event,
            std::mem::size_of_val(&event) as u32,
            std::ptr::null_mut(),
        ) {
            assert_ne!(event.eventType, vr::EVREventType::Quit.0);
        }
    }
}
