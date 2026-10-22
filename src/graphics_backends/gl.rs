use super::GraphicsBackend;
use derive_more::Deref;
use gl::types::{GLenum, GLint, GLuint};
use glutin_glx_sys::{
    Success,
    glx::{self, Glx},
};
use libc::{dlopen, dlsym};
use log::warn;
use openvr as vr;
use openxr as xr;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::{LazyLock, Mutex, Once, OnceLock, PoisonError};
use xr::sys::Handle as _;
use xr::sys::platform::{EGLConfig, EGLContext, EGLDisplay, EglGetProcAddressMNDX};

/// None if GLX isn't installed (EGL-only systems).
static GLX: LazyLock<Option<GlxLib>> = LazyLock::new(|| {
    // Without glvnd, GLX is part of libGL.
    let Some(lib) = Library::open(c"libGLX.so.0", 0).or_else(|| Library::open(c"libGL.so.1", 0))
    else {
        warn!("Neither libGLX.so.0 nor libGL.so.1 found, GLX contexts won't be detected");
        return None;
    };
    Some(GlxLib {
        glx: Glx::load_with(|func| {
            let func = CString::new(func).unwrap();
            lib.get(&func).unwrap_or(ptr::null())
        }),
        // Both link libX11.
        x_free: unsafe { lib.get_fn(c"XFree") },
    })
});

#[derive(Deref)]
struct GlxLib {
    #[deref]
    glx: Glx,
    x_free: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
}
// SAFETY: Only holds function pointers.
unsafe impl Send for GlxLib {}
unsafe impl Sync for GlxLib {}

const EGL_CONFIG_ID: i32 = 0x3028;
const EGL_NONE: i32 = 0x3038;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_CONTEXT_CLIENT_TYPE: i32 = 0x3097;
const EGL_OPENGL_API: i32 = 0x30A2;
const EGL_TRUE: u32 = 1;

struct Egl {
    get_proc_address: EglGetProcAddressMNDX,
    get_current_display: unsafe extern "C" fn() -> EGLDisplay,
    get_current_context: unsafe extern "C" fn() -> EGLContext,
    query_context: unsafe extern "C" fn(EGLDisplay, EGLContext, i32, *mut i32) -> u32,
    choose_config:
        unsafe extern "C" fn(EGLDisplay, *const i32, *mut EGLConfig, i32, *mut i32) -> u32,
    query_string: unsafe extern "C" fn(EGLDisplay, i32) -> *const c_char,
}

impl Egl {
    /// The app's libEGL. We don't load it ourselves: without it, no EGL context can be current.
    fn get() -> Option<&'static Self> {
        static EGL: OnceLock<Egl> = OnceLock::new();
        if let Some(egl) = EGL.get() {
            return Some(egl);
        }
        let lib = Library::open(c"libEGL.so.1", libc::RTLD_NOLOAD)?;
        let egl = unsafe {
            Self {
                get_proc_address: lib.get_fn(c"eglGetProcAddress")?,
                get_current_display: lib.get_fn(c"eglGetCurrentDisplay")?,
                get_current_context: lib.get_fn(c"eglGetCurrentContext")?,
                query_context: lib.get_fn(c"eglQueryContext")?,
                choose_config: lib.get_fn(c"eglChooseConfig")?,
                query_string: lib.get_fn(c"eglQueryString")?,
            }
        };
        Some(EGL.get_or_init(|| egl))
    }

    fn current_context(&self) -> EGLContext {
        unsafe { (self.get_current_context)() }
    }

    unsafe fn session_info(&self) -> Option<SessionCreateInfo> {
        unsafe {
            let display = (self.get_current_display)();
            let context = self.current_context();
            let mut client_type = 0;
            (self.query_context)(display, context, EGL_CONTEXT_CLIENT_TYPE, &mut client_type);
            // GLES contexts get GLES swapchain images, which we can't handle.
            if client_type != EGL_OPENGL_API {
                crate::warn_once!(
                    "Current EGL context is not a desktop OpenGL context (client type {:#x})",
                    client_type
                );
                return None;
            }

            Some(SessionCreateInfo::Egl {
                get_proc_address: self.get_proc_address,
                display,
                config: self.config(display, context)?,
                context,
            })
        }
    }

    /// The runtime creates its own context sharing with the app's, which needs its config.
    unsafe fn config(&self, display: EGLDisplay, context: EGLContext) -> Option<EGLConfig> {
        unsafe {
            let mut id = 0;
            if (self.query_context)(display, context, EGL_CONFIG_ID, &mut id) == EGL_TRUE && id != 0
            {
                let attribs = [EGL_CONFIG_ID, id, EGL_NONE];
                let mut config = ptr::null_mut();
                let mut count = 0;
                if (self.choose_config)(display, attribs.as_ptr(), &mut config, 1, &mut count)
                    == EGL_TRUE
                    && count > 0
                {
                    return Some(config);
                }
            }

            // Contexts created without a config report none.
            let exts = (self.query_string)(display, EGL_EXTENSIONS);
            if !exts.is_null() && has_extension(CStr::from_ptr(exts), "EGL_KHR_no_config_context") {
                Some(ptr::null_mut()) // EGL_NO_CONFIG_KHR
            } else {
                crate::warn_once!("Couldn't find the config of the current EGL context");
                None
            }
        }
    }
}

fn has_extension(extensions: &CStr, name: &str) -> bool {
    extensions
        .to_bytes()
        .split(|c| *c == b' ')
        .any(|ext| ext == name.as_bytes())
}

/// [xr::OpenGL] plus the EGL binding from XR_MNDX_egl_enable, which openxrs lacks. Monado
/// enumerates OpenGL swapchain images for desktop GL EGL contexts too.
pub enum GlApi {}

/// The app's GL context, which the runtime shares swapchain images with.
#[derive(Copy, Clone)]
pub enum SessionCreateInfo {
    Xlib {
        x_display: *mut xr::sys::platform::Display,
        visualid: u32,
        glx_fb_config: xr::sys::platform::GLXFBConfig,
        glx_drawable: xr::sys::platform::GLXDrawable,
        glx_context: xr::sys::platform::GLXContext,
    },
    Egl {
        get_proc_address: EglGetProcAddressMNDX,
        display: EGLDisplay,
        config: EGLConfig,
        context: EGLContext,
    },
}
// SAFETY: These are handles owned by the app, we never dereference them.
unsafe impl Send for SessionCreateInfo {}
unsafe impl Sync for SessionCreateInfo {}

impl SessionCreateInfo {
    /// Captures the context current on this thread.
    fn current(instance: &xr::Instance) -> Option<Self> {
        if let Some(glx) = GLX.as_ref() {
            let glx_context = unsafe { glx.GetCurrentContext() };
            if !glx_context.is_null() {
                return Some(unsafe { xlib_session_info(glx, glx_context) });
            }
        }

        let Some(egl) = Egl::get().filter(|egl| !egl.current_context().is_null()) else {
            crate::warn_once!("No GLX or EGL context is current, can't use OpenGL textures");
            return None;
        };
        if instance.exts().mndx_egl_enable.is_none() {
            crate::warn_once!(
                "App uses an EGL context, but the runtime doesn't support XR_MNDX_egl_enable"
            );
            return None;
        }
        unsafe { egl.session_info() }
    }

    /// Whether this context is current on the calling thread with the same drawables, so the
    /// runtime can use it without making it current. Only reads thread state, so this is safe
    /// after the app has closed its display.
    fn is_current(&self) -> bool {
        match *self {
            Self::Xlib {
                x_display,
                glx_drawable,
                glx_context,
                ..
            } => GLX.as_ref().is_some_and(|glx| unsafe {
                glx.GetCurrentContext().cast_mut() == glx_context
                    && glx.GetCurrentDisplay().cast() == x_display
                    && glx.GetCurrentDrawable() == glx_drawable
                    && glx.GetCurrentReadDrawable() == glx_drawable
            }),
            Self::Egl {
                display, context, ..
            } => Egl::get().is_some_and(|egl| unsafe {
                egl.current_context() == context && (egl.get_current_display)() == display
            }),
        }
    }

    fn get_proc_address(&self, name: &str) -> *const c_void {
        let name = CString::new(name).unwrap();
        match self {
            Self::Xlib { .. } => GLX.as_ref().map_or(ptr::null(), |glx| unsafe {
                glx.GetProcAddress(name.as_ptr().cast()).cast()
            }),
            Self::Egl {
                get_proc_address, ..
            } => unsafe { get_proc_address(name.as_ptr()) }.map_or(ptr::null(), |f| f as _),
        }
    }
}

unsafe fn xlib_session_info(
    glx: &GlxLib,
    glx_context: glx::types::GLXContext,
) -> SessionCreateInfo {
    unsafe {
        let x_display = glx.GetCurrentDisplay();
        let glx_drawable = glx.GetCurrentDrawable();
        let fbconfig = get_fbconfig(glx, x_display, glx_context);
        let visualid = get_visualid(glx, x_display, fbconfig);

        SessionCreateInfo::Xlib {
            x_display: x_display.cast(),
            glx_fb_config: fbconfig.map(|p| p.cast_mut()).unwrap_or_else(|| {
                warn!("No fbconfig found.");
                1 as _
            }),
            visualid: visualid.unwrap_or(1),
            glx_drawable,
            glx_context: glx_context.cast_mut(),
        }
    }
}

unsafe fn get_fbconfig(
    glx: &GlxLib,
    display: *mut glx::types::Display,
    glx_context: glx::types::GLXContext,
) -> Option<glx::types::GLXFBConfig> {
    unsafe {
        let mut config_id = 0;
        let ret = glx.QueryContext(display, glx_context, glx::FBCONFIG_ID as _, &mut config_id);
        if ret != Success as i32 {
            warn!("Failed to get fbconfig id from context (error code {ret})");
            return None;
        }

        let mut screen = 0;
        let ret = glx.QueryContext(display, glx_context, glx::SCREEN as _, &mut screen);
        if ret != Success as i32 {
            warn!("Failed to get GLX screen for context (error code {ret})");
            return None;
        }

        let attrs = [glx::FBCONFIG_ID, config_id as _, glx::NONE];
        let mut items = 0;
        let cfgs = glx.ChooseFBConfig(display, screen, attrs.as_ptr() as _, &mut items);
        if cfgs.is_null() {
            return None;
        }
        // The configs belong to the display, only the array is ours.
        let cfg = (items > 0).then(|| *cfgs);
        if let Some(x_free) = glx.x_free {
            x_free(cfgs.cast());
        }
        cfg
    }
}

unsafe fn get_visualid(
    glx: &Glx,
    display: *mut glx::types::Display,
    cfg: Option<glx::types::GLXFBConfig>,
) -> Option<u32> {
    unsafe {
        let mut visualid = 0;
        let ret = glx.GetFBConfigAttrib(display, cfg?, glx::VISUAL_ID as _, &mut visualid);
        if ret != Success as i32 || visualid == 0 {
            warn!("No visual available from fbconfig.");
            None
        } else {
            Some(visualid as u32)
        }
    }
}

impl xr::Graphics for GlApi {
    type Requirements = <xr::OpenGL as xr::Graphics>::Requirements;
    type SessionCreateInfo = SessionCreateInfo;
    type Format = u32;
    type SwapchainImage = u32;

    fn raise_format(x: i64) -> u32 {
        xr::OpenGL::raise_format(x)
    }

    fn lower_format(x: u32) -> i64 {
        xr::OpenGL::lower_format(x)
    }

    fn requirements(
        instance: &xr::Instance,
        system: xr::SystemId,
    ) -> xr::Result<Self::Requirements> {
        // Required before creating EGL sessions too.
        xr::OpenGL::requirements(instance, system)
    }

    unsafe fn create_session(
        instance: &xr::Instance,
        system: xr::SystemId,
        info: &Self::SessionCreateInfo,
    ) -> xr::Result<xr::sys::Session> {
        match *info {
            SessionCreateInfo::Xlib {
                x_display,
                visualid,
                glx_fb_config,
                glx_drawable,
                glx_context,
            } => unsafe {
                xr::OpenGL::create_session(
                    instance,
                    system,
                    &xr::opengl::SessionCreateInfo::Xlib {
                        x_display,
                        visualid,
                        glx_fb_config,
                        glx_drawable,
                        glx_context,
                    },
                )
            },
            SessionCreateInfo::Egl {
                get_proc_address,
                display,
                config,
                context,
            } => {
                let binding = xr::sys::GraphicsBindingEGLMNDX {
                    ty: xr::sys::GraphicsBindingEGLMNDX::TYPE,
                    next: ptr::null(),
                    get_proc_address: Some(get_proc_address),
                    display,
                    config,
                    context,
                };
                let info = xr::sys::SessionCreateInfo {
                    ty: xr::sys::SessionCreateInfo::TYPE,
                    next: (&raw const binding).cast(),
                    create_flags: Default::default(),
                    system_id: system,
                };
                let mut session = xr::sys::Session::NULL;
                cvt(unsafe {
                    (instance.fp().create_session)(instance.as_raw(), &info, &mut session)
                })?;
                Ok(session)
            }
        }
    }

    fn enumerate_swapchain_images(swapchain: &xr::Swapchain<Self>) -> xr::Result<Vec<u32>> {
        let enumerate = swapchain.instance().fp().enumerate_swapchain_images;
        let mut count = 0;
        cvt(unsafe { enumerate(swapchain.as_raw(), 0, &mut count, ptr::null_mut()) })?;
        let mut images = vec![
            xr::sys::SwapchainImageOpenGLKHR {
                ty: xr::sys::SwapchainImageOpenGLKHR::TYPE,
                next: ptr::null_mut(),
                image: 0,
            };
            count as usize
        ];
        cvt(unsafe {
            enumerate(
                swapchain.as_raw(),
                count,
                &mut count,
                images.as_mut_ptr().cast(),
            )
        })?;
        Ok(images[..count as usize].iter().map(|i| i.image).collect())
    }
}

fn cvt(result: xr::sys::Result) -> xr::Result<()> {
    if result.into_raw() < 0 {
        Err(result)
    } else {
        Ok(())
    }
}

pub struct GlData {
    session_info: SessionCreateInfo,
    images: Vec<u32>,
    format: u32,
    read_fbo: u32,
    draw_fbo: u32,
    resolved: Mutex<Resolved>,
}

/// Single-sample copy of a multisampled renderbuffer, see [GlData::resolve].
#[derive(Default)]
struct Resolved {
    renderbuffer: GLuint,
    width: GLint,
    height: GLint,
    format: GLenum,
}

impl GlData {
    /// Returns None if no usable GL context is current.
    pub(crate) fn new(instance: &xr::Instance) -> Option<Self> {
        if instance.exts().khr_opengl_enable.is_none() {
            crate::warn_once!("Runtime doesn't support XR_KHR_opengl_enable");
            return None;
        }

        // Grab the session info on creation - this makes us resilient against session restarts,
        // which could result in us trying to grab the context from a different thread
        let session_info = SessionCreateInfo::current(instance)?;

        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            gl::load_with(|f| session_info.get_proc_address(f));

            if log::log_enabled!(log::Level::Debug) {
                unsafe {
                    gl::DebugMessageCallback(Some(debug_message), std::ptr::null());
                    gl::Enable(gl::DEBUG_OUTPUT);
                }
            }
        });

        let mut fbos = [0; 2];
        unsafe {
            gl::GenFramebuffers(fbos.len() as i32, fbos.as_mut_ptr());
        }

        Some(GlData {
            session_info,
            images: Default::default(),
            format: 0,
            read_fbo: fbos[0],
            draw_fbo: fbos[1],
            resolved: Default::default(),
        })
    }
}

impl Drop for GlData {
    fn drop(&mut self) {
        // Framebuffers aren't shared between contexts, in another one these could be the app's.
        if self.session_info.is_current() {
            let resolved = self
                .resolved
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner);
            unsafe {
                gl::DeleteFramebuffers(2, [self.read_fbo, self.draw_fbo].as_ptr());
                gl::DeleteRenderbuffers(1, &resolved.renderbuffer);
            }
        }
    }
}

impl GraphicsBackend for GlData {
    type Api = GlApi;
    type OpenVrTexture = GlTexture;
    type Format = u32;
    type NiceFormat = u32;

    fn from_openxr_format(format: u32) -> Self::Format {
        format as _
    }

    fn to_openxr_format(format: Self::Format) -> <Self::Api as xr::Graphics>::Format {
        format as _
    }

    fn to_nice_format(format: Self::Format) -> Self::NiceFormat {
        format
    }

    fn session_create_info(&self) -> <Self::Api as openxr::Graphics>::SessionCreateInfo {
        self.session_info
    }

    fn is_context_current(&self) -> bool {
        self.session_info.is_current()
    }

    #[inline]
    fn get_texture(texture: &vr::Texture_t, flags: vr::EVRSubmitFlags) -> Option<GlTexture> {
        let has_flag = |flag: vr::EVRSubmitFlags| (flags & flag).0 != 0;
        let target = if has_flag(vr::EVRSubmitFlags::GlRenderBuffer) {
            gl::RENDERBUFFER
        } else if has_flag(vr::EVRSubmitFlags::GlArrayTexture) {
            gl::TEXTURE_2D_ARRAY
        } else {
            gl::TEXTURE_2D
        };
        Some(GlTexture {
            name: texture.handle as usize as GLuint,
            target,
        })
    }

    #[inline]
    fn store_swapchain_images(
        &mut self,
        images: Vec<<Self::Api as xr::Graphics>::SwapchainImage>,
        format: Self::Format,
    ) {
        self.images = images;
        self.format = format;
    }

    #[inline]
    fn raw_swapchain_info_for_texture(
        &self,
        texture: Self::OpenVrTexture,
        bounds: vr::VRTextureBounds_t,
        color_space: vr::EColorSpace,
    ) -> xr::SwapchainCreateInfo<Self::Api> {
        let info = texture.info();
        let xr::Rect2Di { extent, .. } = info.rect(bounds);

        xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            // The blit path renders into the swapchain images.
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                | xr::SwapchainUsageFlags::TRANSFER_DST,
            format: swapchain_format(info.format, color_space),
            sample_count: 1,
            width: extent.width as u32,
            height: extent.height as u32,
            face_count: 1,
            array_size: 2,
            mip_count: 1,
        }
    }

    fn copy_texture_to_swapchain(
        &self,
        eye: vr::EVREye,
        texture: Self::OpenVrTexture,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
        _submit_flags: vr::EVRSubmitFlags,
    ) -> xr::Extent2Di {
        let dst = GlTexture {
            name: self.images[image_index],
            target: gl::TEXTURE_2D_ARRAY,
        };
        let layer = GLint::from(eye == vr::EVREye::Right);
        self.copy(texture, layer, color_space, bounds, dst, layer)
    }

    fn copy_overlay_to_swapchain(
        &mut self,
        texture: Self::OpenVrTexture,
        _color_space: vr::EColorSpace,
        bounds: openvr::VRTextureBounds_t,
        image_index: usize,
    ) -> openxr::Extent2Di {
        // Overlay swapchains have a single layer (see Overlay::set_texture), which runtimes
        // allocate as 2D textures rather than arrays.
        let dst = GlTexture {
            name: self.images[image_index],
            target: gl::TEXTURE_2D,
        };
        self.copy(texture, 0, vr::EColorSpace::Auto, bounds, dst, 0)
    }
}

/// A texture or renderbuffer name and its target.
#[derive(Copy, Clone)]
pub struct GlTexture {
    name: GLuint,
    target: GLenum,
}

struct TextureInfo {
    width: GLint,
    height: GLint,
    layers: GLint,
    format: GLenum,
    samples: GLint,
}

impl TextureInfo {
    fn rect(&self, bounds: vr::VRTextureBounds_t) -> xr::Rect2Di {
        let width_min = bounds.uMin * self.width as f32;
        let width_max = bounds.uMax * self.width as f32;
        let height_min = bounds.vMin * self.height as f32;
        let height_max = bounds.vMax * self.height as f32;

        xr::Rect2Di {
            extent: xr::Extent2Di {
                width: (width_max - width_min).abs() as i32,
                height: (height_max - height_min).abs() as i32,
            },
            offset: xr::Offset2Di {
                x: width_min.min(width_max) as i32,
                y: height_min.min(height_max) as i32,
            },
        }
    }
}

impl GlTexture {
    /// Queries the texture, leaving the app's bindings as they were.
    fn info(self) -> TextureInfo {
        unsafe {
            if self.target == gl::RENDERBUFFER {
                let mut bound = 0;
                gl::GetIntegerv(gl::RENDERBUFFER_BINDING, &mut bound);
                gl::BindRenderbuffer(gl::RENDERBUFFER, self.name);
                let get = |param| {
                    let mut value = 0;
                    gl::GetRenderbufferParameteriv(gl::RENDERBUFFER, param, &mut value);
                    value
                };
                let info = TextureInfo {
                    width: get(gl::RENDERBUFFER_WIDTH),
                    height: get(gl::RENDERBUFFER_HEIGHT),
                    layers: 1,
                    format: get(gl::RENDERBUFFER_INTERNAL_FORMAT) as GLenum,
                    samples: get(gl::RENDERBUFFER_SAMPLES),
                };
                gl::BindRenderbuffer(gl::RENDERBUFFER, bound as GLuint);
                info
            } else {
                let array = self.target == gl::TEXTURE_2D_ARRAY;
                let binding = if array {
                    gl::TEXTURE_BINDING_2D_ARRAY
                } else {
                    gl::TEXTURE_BINDING_2D
                };
                let mut bound = 0;
                gl::GetIntegerv(binding, &mut bound);
                gl::BindTexture(self.target, self.name);
                let get = |param| {
                    let mut value = 0;
                    gl::GetTexLevelParameteriv(self.target, 0, param, &mut value);
                    value
                };
                let info = TextureInfo {
                    width: get(gl::TEXTURE_WIDTH),
                    height: get(gl::TEXTURE_HEIGHT),
                    layers: if array { get(gl::TEXTURE_DEPTH) } else { 1 },
                    format: get(gl::TEXTURE_INTERNAL_FORMAT) as GLenum,
                    samples: 0,
                };
                gl::BindTexture(self.target, bound as GLuint);
                info
            }
        }
    }

    unsafe fn attach(self, framebuffer: GLenum, layer: GLint) {
        unsafe {
            match self.target {
                gl::RENDERBUFFER => gl::FramebufferRenderbuffer(
                    framebuffer,
                    gl::COLOR_ATTACHMENT0,
                    gl::RENDERBUFFER,
                    self.name,
                ),
                gl::TEXTURE_2D_ARRAY => gl::FramebufferTextureLayer(
                    framebuffer,
                    gl::COLOR_ATTACHMENT0,
                    self.name,
                    0,
                    layer,
                ),
                target => gl::FramebufferTexture2D(
                    framebuffer,
                    gl::COLOR_ATTACHMENT0,
                    target,
                    self.name,
                    0,
                ),
            }
        }
    }
}

impl GlData {
    /// Copies `src` (layer `eye` if it's an array) into layer `dst_layer` of the swapchain
    /// image `dst`.
    fn copy(
        &self,
        src: GlTexture,
        eye: GLint,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        dst: GlTexture,
        dst_layer: GLint,
    ) -> xr::Extent2Di {
        let info = src.info();
        let rect = info.rect(bounds);
        let (src, src_layer) = if info.samples > 0 {
            (unsafe { self.resolve(src, &info) }, 0)
        } else {
            // Plain Submit has no layer index, so assume stereo array textures have a layer per
            // eye. Not verified against SteamVR.
            (src, if eye < info.layers { eye } else { 0 })
        };

        match copy_method(info.format, color_space, self.format) {
            CopyMethod::Copy => unsafe {
                gl::CopyImageSubData(
                    src.name,
                    src.target,
                    0, // level
                    rect.offset.x,
                    rect.offset.y,
                    src_layer,
                    dst.name,
                    dst.target,
                    0, // level
                    0, // x
                    0, // y
                    dst_layer,
                    rect.extent.width,
                    rect.extent.height,
                    1,
                );
            },
            CopyMethod::Blit { srgb } => unsafe {
                self.blit(src, src_layer, rect, dst, dst_layer, srgb)
            },
        }

        rect.extent
    }

    /// Resolves a multisampled renderbuffer into a single-sample one with the same size and
    /// format, since resolving blits can't convert formats or move the rect.
    unsafe fn resolve(&self, src: GlTexture, info: &TextureInfo) -> GlTexture {
        let mut resolved = self.resolved.lock().unwrap();
        unsafe {
            if resolved.renderbuffer == 0 {
                gl::GenRenderbuffers(1, &mut resolved.renderbuffer);
            }
            let dst = GlTexture {
                name: resolved.renderbuffer,
                target: gl::RENDERBUFFER,
            };
            if (resolved.width, resolved.height, resolved.format)
                != (info.width, info.height, info.format)
            {
                let mut bound = 0;
                gl::GetIntegerv(gl::RENDERBUFFER_BINDING, &mut bound);
                gl::BindRenderbuffer(gl::RENDERBUFFER, dst.name);
                gl::RenderbufferStorage(gl::RENDERBUFFER, info.format, info.width, info.height);
                gl::BindRenderbuffer(gl::RENDERBUFFER, bound as GLuint);
                (resolved.width, resolved.height, resolved.format) =
                    (info.width, info.height, info.format);
            }

            let rect = xr::Rect2Di {
                offset: Default::default(),
                extent: xr::Extent2Di {
                    width: info.width,
                    height: info.height,
                },
            };
            self.blit(src, 0, rect, dst, 0, true);
            dst
        }
    }

    /// Blits `rect` of `src` to the origin of `dst`, with GL_FRAMEBUFFER_SRGB set to `srgb`.
    /// Leaves the app's framebuffer bindings and state as they were.
    unsafe fn blit(
        &self,
        src: GlTexture,
        src_layer: GLint,
        rect: xr::Rect2Di,
        dst: GlTexture,
        dst_layer: GLint,
        srgb: bool,
    ) {
        unsafe {
            let [mut read_fbo, mut draw_fbo] = [0; 2];
            gl::GetIntegerv(gl::READ_FRAMEBUFFER_BINDING, &mut read_fbo);
            gl::GetIntegerv(gl::DRAW_FRAMEBUFFER_BINDING, &mut draw_fbo);
            // Both apply to blits.
            let framebuffer_srgb = set_enabled(gl::FRAMEBUFFER_SRGB, srgb);
            let scissor_test = set_enabled(gl::SCISSOR_TEST, false);

            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, self.read_fbo);
            src.attach(gl::READ_FRAMEBUFFER, src_layer);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, self.draw_fbo);
            dst.attach(gl::DRAW_FRAMEBUFFER, dst_layer);
            gl::BlitFramebuffer(
                rect.offset.x,
                rect.offset.y,
                rect.offset.x + rect.extent.width,
                rect.offset.y + rect.extent.height,
                0,
                0,
                rect.extent.width,
                rect.extent.height,
                gl::COLOR_BUFFER_BIT,
                gl::NEAREST,
            );

            set_enabled(gl::SCISSOR_TEST, scissor_test);
            set_enabled(gl::FRAMEBUFFER_SRGB, framebuffer_srgb);
            gl::BindFramebuffer(gl::READ_FRAMEBUFFER, read_fbo as GLuint);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, draw_fbo as GLuint);
        }
    }
}

/// Sets `cap` and returns whether it was enabled before.
unsafe fn set_enabled(cap: GLenum, enabled: bool) -> bool {
    unsafe {
        let was_enabled = gl::IsEnabled(cap) == gl::TRUE;
        if enabled {
            gl::Enable(cap);
        } else {
            gl::Disable(cap);
        }
        was_enabled
    }
}

#[derive(Debug, PartialEq)]
enum CopyMethod {
    /// Raw copy into the same format, or into its sRGB twin when the data is gamma-encoded.
    Copy,
    /// Converting blit with GL_FRAMEBUFFER_SRGB set to `srgb`.
    Blit { srgb: bool },
}

fn copy_method(
    format: GLenum,
    color_space: vr::EColorSpace,
    swapchain_format: GLenum,
) -> CopyMethod {
    let gamma = holds_gamma(format, color_space);
    if format == swapchain_format
        || (format == gl::RGBA8 && swapchain_format == gl::SRGB8_ALPHA8 && gamma)
    {
        CopyMethod::Copy
    } else {
        // Gamma-encoded data in a linear format must not be encoded again.
        CopyMethod::Blit { srgb: !gamma }
    }
}

/// Runtimes rarely support 3-channel formats (Vulkan drivers can't render to them), and unsized
/// formats aren't swapchain formats at all.
fn sized_format(format: GLenum) -> GLenum {
    match format {
        gl::RGBA | gl::RGB | gl::RGB8 => gl::RGBA8,
        gl::SRGB_ALPHA | gl::SRGB | gl::SRGB8 => gl::SRGB8_ALPHA8,
        gl::RGB16F => gl::RGBA16F,
        gl::RGB16 => gl::RGBA16,
        other => other,
    }
}

/// Whether the texture holds gamma-encoded data in a format GL treats as linear.
fn holds_gamma(format: GLenum, color_space: vr::EColorSpace) -> bool {
    let format = sized_format(format);
    match color_space {
        vr::EColorSpace::Linear => false,
        vr::EColorSpace::Gamma => format != gl::SRGB8_ALPHA8,
        // Auto: 8-bit formats are gamma, everything else linear.
        _ => format == gl::RGBA8,
    }
}

/// Gamma-encoded data goes into an sRGB swapchain whatever its format, since runtimes treat every
/// other format as linear and would encode it again.
fn swapchain_format(format: GLenum, color_space: vr::EColorSpace) -> GLenum {
    if holds_gamma(format, color_space) {
        gl::SRGB8_ALPHA8
    } else {
        sized_format(format)
    }
}

extern "system" fn debug_message(
    source: gl::types::GLenum,
    ty: gl::types::GLenum,
    id: gl::types::GLuint,
    severity: gl::types::GLenum,
    _: gl::types::GLsizei,
    message: *const c_char,
    _: *mut c_void,
) {
    let source = match source {
        gl::DEBUG_SOURCE_API => "OpenGL Api",
        gl::DEBUG_SOURCE_OTHER => "Other",
        _ => "<unknown>",
    };

    let ty = match ty {
        gl::DEBUG_TYPE_ERROR => "Error",
        gl::DEBUG_TYPE_DEPRECATED_BEHAVIOR => "Deprecated Behavior",
        gl::DEBUG_TYPE_UNDEFINED_BEHAVIOR => "Undefined Behavior",
        gl::DEBUG_TYPE_PORTABILITY => "Portability Issue",
        gl::DEBUG_TYPE_PERFORMANCE => "Performance Issue",
        gl::DEBUG_TYPE_OTHER => "Other",
        _ => "<unknown>",
    };

    let severity = match severity {
        gl::DEBUG_SEVERITY_HIGH => "High",
        gl::DEBUG_SEVERITY_MEDIUM => "Medium",
        gl::DEBUG_SEVERITY_LOW => "Low",
        gl::DEBUG_SEVERITY_NOTIFICATION => "Notification",
        _ => "<unknown>",
    };
    let message = unsafe { CStr::from_ptr(message) };
    log::debug!("(severity: {severity}, id: {id}) {ty} message from {source}: {message:?}");
}

/// A dlopen handle that is never closed.
struct Library(*mut c_void);
impl Library {
    fn open(name: &CStr, flags: c_int) -> Option<Self> {
        let handle = unsafe { dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL | flags) };
        (!handle.is_null()).then_some(Self(handle))
    }

    fn get(&self, symbol: &CStr) -> Option<*const c_void> {
        let symbol = unsafe { dlsym(self.0, symbol.as_ptr()) };
        (!symbol.is_null()).then_some(symbol.cast_const())
    }

    /// # Safety
    /// `F` must be the function pointer type of `symbol`.
    unsafe fn get_fn<F: Copy>(&self, symbol: &CStr) -> Option<F> {
        const { assert!(size_of::<F>() == size_of::<*const c_void>()) };
        self.get(symbol)
            .map(|f| unsafe { std::mem::transmute_copy::<*const c_void, F>(&f) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clientcore::Injector;
    use crate::openxr_data::{self, FrameStream, OpenXrData};
    use std::sync::Arc;

    /// Restarts sessions with an EGL context.
    struct EglCompositor;
    impl vr::InterfaceImpl for EglCompositor {
        fn get_version(_: &CStr) -> Option<Box<dyn FnOnce(&Arc<Self>) -> *mut c_void>> {
            None
        }
        fn supported_versions() -> &'static [&'static CStr] {
            &[]
        }
    }
    impl openxr_data::Compositor for EglCompositor {
        fn get_session_create_info(
            &self,
            _: crate::compositor::CompositorSessionData,
        ) -> openxr_data::SessionCreateInfo {
            unsafe extern "system" fn get_proc_address(
                _: *const c_char,
            ) -> Option<unsafe extern "system" fn()> {
                None
            }
            openxr_data::SessionCreateInfo::from_info::<GlApi>(SessionCreateInfo::Egl {
                get_proc_address,
                display: 1 as _,
                config: ptr::null_mut(),
                context: 2 as _,
            })
        }

        fn post_session_restart(
            &self,
            _: &openxr_data::SessionData,
            _: xr::FrameWaiter,
            _: FrameStream,
        ) {
        }
    }

    #[test]
    fn egl_session() {
        crate::init_logging();
        let xr =
            OpenXrData::<EglCompositor>::new(&Injector::default(), vr::EVRApplicationType::Scene)
                .unwrap();
        assert!(xr.enabled_extensions.mndx_egl_enable);
        let comp = Arc::new(EglCompositor);
        xr.compositor.set(Arc::downgrade(&comp));

        xr.restart_session();
        let session = xr.session_data.get();
        assert!(session.is_real_session());
        let swapchain = session
            .create_swapchain::<GlApi>(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::TRANSFER_DST,
                format: 0,
                sample_count: 1,
                width: 1,
                height: 1,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })
            .unwrap();
        assert_eq!(swapchain.enumerate_images(), Ok(vec![]));
    }

    #[test]
    fn swapchain_formats() {
        use vr::EColorSpace as Space;
        for (format, color_space, expected) in [
            (gl::RGBA8, Space::Auto, gl::SRGB8_ALPHA8),
            (gl::RGBA8, Space::Gamma, gl::SRGB8_ALPHA8),
            (gl::RGBA8, Space::Linear, gl::RGBA8),
            (gl::RGBA, Space::Auto, gl::SRGB8_ALPHA8),
            (gl::RGBA, Space::Linear, gl::RGBA8),
            (gl::RGB, Space::Gamma, gl::SRGB8_ALPHA8),
            (gl::RGB8, Space::Auto, gl::SRGB8_ALPHA8),
            (gl::RGB8, Space::Linear, gl::RGBA8),
            (gl::SRGB8, Space::Linear, gl::SRGB8_ALPHA8),
            (gl::SRGB_ALPHA, Space::Auto, gl::SRGB8_ALPHA8),
            (gl::SRGB8_ALPHA8, Space::Gamma, gl::SRGB8_ALPHA8),
            (gl::RGBA16F, Space::Auto, gl::RGBA16F),
            (gl::RGBA16F, Space::Gamma, gl::SRGB8_ALPHA8),
            (gl::RGB16F, Space::Linear, gl::RGBA16F),
            (gl::RGB10_A2, Space::Auto, gl::RGB10_A2),
            (gl::RGB10_A2, Space::Gamma, gl::SRGB8_ALPHA8),
            (gl::RGBA16, Space::Gamma, gl::SRGB8_ALPHA8),
            (gl::RGB565, Space::Gamma, gl::SRGB8_ALPHA8),
        ] {
            assert_eq!(
                swapchain_format(format, color_space),
                expected,
                "{format:#x} {color_space:?}"
            );
        }
    }

    #[test]
    fn copy_methods() {
        use vr::EColorSpace as Space;
        let blit = |srgb| CopyMethod::Blit { srgb };
        for (format, color_space, swapchain_format, expected) in [
            (gl::RGBA8, Space::Auto, gl::SRGB8_ALPHA8, CopyMethod::Copy),
            (gl::RGBA8, Space::Gamma, gl::SRGB8_ALPHA8, CopyMethod::Copy),
            (gl::RGBA8, Space::Linear, gl::RGBA8, CopyMethod::Copy),
            (
                gl::SRGB8_ALPHA8,
                Space::Linear,
                gl::SRGB8_ALPHA8,
                CopyMethod::Copy,
            ),
            // Linear data in an sRGB fallback swapchain gets encoded.
            (gl::RGBA8, Space::Linear, gl::SRGB8_ALPHA8, blit(true)),
            (gl::RGBA16F, Space::Auto, gl::SRGB8_ALPHA8, blit(true)),
            // Gamma data in a linear format is passed through.
            (gl::RGBA, Space::Auto, gl::SRGB8_ALPHA8, blit(false)),
            (gl::RGB8, Space::Gamma, gl::SRGB8_ALPHA8, blit(false)),
            (gl::RGB10_A2, Space::Gamma, gl::SRGB8_ALPHA8, blit(false)),
            (gl::RGBA16F, Space::Gamma, gl::SRGB8_ALPHA8, blit(false)),
            // Only if the runtime has no sRGB format, nothing can be done about it then.
            (gl::RGBA8, Space::Auto, gl::RGBA16, blit(false)),
            // sRGB data in a linear fallback swapchain gets decoded.
            (gl::SRGB8_ALPHA8, Space::Auto, gl::RGBA16, blit(true)),
        ] {
            assert_eq!(
                copy_method(format, color_space, swapchain_format),
                expected,
                "{format:#x} {color_space:?} {swapchain_format:#x}"
            );
        }
    }

    #[test]
    fn submit_flags_pick_texture_target() {
        use vr::EVRSubmitFlags as Flags;
        let texture = vr::Texture_t {
            handle: 5 as _,
            eType: vr::ETextureType::OpenGL,
            eColorSpace: vr::EColorSpace::Auto,
        };
        let get = |flags| GlData::get_texture(&texture, flags).unwrap();
        assert_eq!(get(Flags::Default).name, 5);
        assert_eq!(get(Flags::Default).target, gl::TEXTURE_2D);
        assert_eq!(get(Flags::GlArrayTexture).target, gl::TEXTURE_2D_ARRAY);
        assert_eq!(
            get(Flags::GlRenderBuffer | Flags::LensDistortionAlreadyApplied).target,
            gl::RENDERBUFFER
        );
    }

    #[test]
    fn egl_extension_list() {
        let exts = c"EGL_KHR_no_config_context_foo EGL_KHR_no_config_context EGL_EXT_bar";
        assert!(has_extension(exts, "EGL_KHR_no_config_context"));
        assert!(!has_extension(exts, "EGL_KHR_no_config"));
        assert!(!has_extension(c"", "EGL_KHR_no_config_context"));
    }
}
