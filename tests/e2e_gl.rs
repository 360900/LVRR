//! End-to-end smoke test of an OpenGL scene application against a real OpenXR runtime.
//!
//! Each eye gets a texture with a different color in its top and bottom half, so a mirror of
//! the runtime shows whether the frames arrive the right way up. `XRIZER_E2E_GL_RENDERER` picks
//! the GPU by a part of its GL_RENDERER string, e.g. another one than the runtime's to test
//! copying frames between GPUs. `XRIZER_E2E_GL_FRAMES` sets how many frames are submitted and
//! `XRIZER_E2E_GL_SIZE` (e.g. `2064x2162`) the size of the eye textures.
//!
//! ```sh
//! XRIZER_E2E=1 XR_RUNTIME_JSON=... XRIZER_E2E_GL_RENDERER=Intel cargo test --test e2e_gl -- --nocapture
//! ```

use libloading::{Library, Symbol};
use openvr as vr;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::{null, null_mut};

macro_rules! vcall {
    ($this:expr, $method:ident $(, $arg:expr)* $(,)?) => {
        unsafe { ((*(*$this).vtable_).$method)($this $(, $arg)*) }
    };
}

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglConfig = *mut c_void;
type EglDevice = *mut c_void;

const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_BIT: i32 = 0x0008;
const EGL_NONE: i32 = 0x3038;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;
const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: i32 = 0x1;

struct Egl {
    _lib: Library,
}

impl Egl {
    /// Makes a surfaceless GL 4.5 context current on the first device whose renderer contains
    /// `renderer`.
    fn current_context(renderer: Option<&str>) -> Self {
        unsafe {
            let lib = Library::new("libEGL.so.1").expect("libEGL not found");
            let get_proc_address: unsafe extern "C" fn(*const c_char) -> *const c_void =
                *lib.get(b"eglGetProcAddress\0").unwrap();
            let ext = |name: &CStr| {
                let f = get_proc_address(name.as_ptr());
                assert!(!f.is_null(), "{name:?} missing");
                f
            };
            let query_devices: unsafe extern "C" fn(i32, *mut EglDevice, *mut i32) -> u32 =
                std::mem::transmute(ext(c"eglQueryDevicesEXT"));
            let get_platform_display: unsafe extern "C" fn(
                u32,
                EglDevice,
                *const i32,
            ) -> EglDisplay = std::mem::transmute(ext(c"eglGetPlatformDisplayEXT"));
            let initialize: unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> u32 =
                *lib.get(b"eglInitialize\0").unwrap();
            let bind_api: unsafe extern "C" fn(u32) -> u32 = *lib.get(b"eglBindAPI\0").unwrap();
            let choose_config: unsafe extern "C" fn(
                EglDisplay,
                *const i32,
                *mut EglConfig,
                i32,
                *mut i32,
            ) -> u32 = *lib.get(b"eglChooseConfig\0").unwrap();
            let create_context: unsafe extern "C" fn(
                EglDisplay,
                EglConfig,
                EglContext,
                *const i32,
            ) -> EglContext = *lib.get(b"eglCreateContext\0").unwrap();
            let make_current: unsafe extern "C" fn(
                EglDisplay,
                *mut c_void,
                *mut c_void,
                EglContext,
            ) -> u32 = *lib.get(b"eglMakeCurrent\0").unwrap();

            let mut devices = [null_mut(); 16];
            let mut count = 0;
            assert_ne!(query_devices(16, devices.as_mut_ptr(), &mut count), 0);
            gl::load_with(|f| {
                let f = std::ffi::CString::new(f).unwrap();
                get_proc_address(f.as_ptr())
            });
            for &device in &devices[..count as usize] {
                let display = get_platform_display(EGL_PLATFORM_DEVICE_EXT, device, null());
                if display.is_null() || initialize(display, null_mut(), null_mut()) == 0 {
                    continue;
                }
                bind_api(EGL_OPENGL_API);
                let mut config = null_mut();
                let mut configs = 0;
                let attribs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE];
                // Device displays may have no configs, contexts don't need one with
                // EGL_KHR_no_config_context.
                if choose_config(display, attribs.as_ptr(), &mut config, 1, &mut configs) == 0
                    || configs == 0
                {
                    config = null_mut();
                }
                let attribs = [
                    EGL_CONTEXT_MAJOR_VERSION,
                    4,
                    EGL_CONTEXT_MINOR_VERSION,
                    5,
                    EGL_CONTEXT_OPENGL_PROFILE_MASK,
                    EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                    EGL_NONE,
                ];
                let context = create_context(display, config, null_mut(), attribs.as_ptr());
                if context.is_null() || make_current(display, null_mut(), null_mut(), context) == 0
                {
                    continue;
                }
                let name = CStr::from_ptr(gl::GetString(gl::RENDERER).cast()).to_string_lossy();
                if renderer.is_none_or(|r| name.contains(r)) {
                    println!("GL renderer: {name}");
                    return Self { _lib: lib };
                }
                make_current(display, null_mut(), null_mut(), null_mut());
            }
            panic!("no GL device matching {renderer:?}");
        }
    }
}

/// A `width`x`height` texture whose bottom rows are `bottom` and top rows are `top`, in GL's
/// bottom-up row order.
fn texture(width: i32, height: i32, bottom: [u8; 4], top: [u8; 4]) -> u32 {
    let pixels: Vec<u8> = (0..height)
        .flat_map(|row| {
            let color = if row < height / 2 { bottom } else { top };
            std::iter::repeat_n(color, width as usize).flatten()
        })
        .collect();
    let mut name = 0;
    unsafe {
        gl::GenTextures(1, &mut name);
        gl::BindTexture(gl::TEXTURE_2D, name);
        gl::TexStorage2D(gl::TEXTURE_2D, 1, gl::RGBA8, width, height);
        gl::TexSubImage2D(
            gl::TEXTURE_2D,
            0,
            0,
            0,
            width,
            height,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            pixels.as_ptr().cast(),
        );
        gl::BindTexture(gl::TEXTURE_2D, 0);
    }
    name
}

#[test]
fn gl_scene_app() {
    if std::env::var_os("XRIZER_E2E").is_none() {
        println!("XRIZER_E2E not set, skipping end-to-end test");
        return;
    }
    let renderer = std::env::var("XRIZER_E2E_GL_RENDERER").ok();
    let frames: u32 = std::env::var("XRIZER_E2E_GL_FRAMES")
        .ok()
        .and_then(|f| f.parse().ok())
        .unwrap_or(120);
    let _egl = Egl::current_context(renderer.as_deref());

    let path = test_cdylib::build_current_project();
    let lib = unsafe { Library::new(path) }.unwrap();
    let factory: Symbol<unsafe extern "C-unwind" fn(*const c_char, *mut i32) -> *mut c_void> =
        unsafe { lib.get(b"VRClientCoreFactory\0") }.unwrap();
    let mut code = -1;
    let core = unsafe { factory(vr::IVRClientCore003::VERSION.as_ptr(), &mut code) }
        .cast::<vr::IVRClientCore003>();
    assert!(!core.is_null());
    assert_eq!(
        vcall!(
            core,
            IVRClientCore_Init,
            vr::EVRApplicationType::Scene,
            null()
        ),
        vr::EVRInitError::None
    );
    let get = |version: &CStr| {
        let mut err = vr::EVRInitError::Unknown;
        let ptr = vcall!(
            core,
            IVRClientCore_GetGenericInterface,
            version.as_ptr(),
            &mut err
        );
        assert_eq!(err, vr::EVRInitError::None, "{version:?}");
        ptr
    };
    let sys = get(vr::IVRSystem022::VERSION).cast::<vr::IVRSystem022>();
    let comp = get(vr::IVRCompositor028::VERSION).cast::<vr::IVRCompositor028>();

    let (mut width, mut height) = (0, 0);
    vcall!(
        sys,
        IVRSystem_GetRecommendedRenderTargetSize,
        &mut width,
        &mut height
    );
    // Larger textures than recommended to measure copies, e.g. 2064x2162 for a Quest 3.
    if let Some((w, h)) = std::env::var("XRIZER_E2E_GL_SIZE").ok().and_then(|s| {
        s.split_once('x')
            .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
    }) {
        (width, height) = (w, h);
    }
    let (width, height) = (width as i32, height as i32);
    // Left: red below green, right: blue below yellow.
    let eyes = [
        texture(width, height, [255, 0, 0, 255], [0, 255, 0, 255]),
        texture(width, height, [0, 0, 255, 255], [255, 255, 0, 255]),
    ];
    let bounds = vr::VRTextureBounds_t {
        uMin: 0.0,
        vMin: 0.0,
        uMax: 1.0,
        vMax: 1.0,
    };

    println!("eye size: {width}x{height}");
    let mut poses = [vr::TrackedDevicePose_t::default(); vr::k_unMaxTrackedDeviceCount as usize];
    let mut submit_time = [std::time::Duration::ZERO; 2];
    let mut wait_time = std::time::Duration::ZERO;
    let loop_start = std::time::Instant::now();
    for frame in 0..frames {
        let start = std::time::Instant::now();
        let err = vcall!(
            comp,
            IVRCompositor_WaitGetPoses,
            poses.as_mut_ptr(),
            poses.len() as u32,
            null_mut(),
            0
        );
        wait_time += start.elapsed();
        assert_eq!(
            err,
            vr::EVRCompositorError::None,
            "WaitGetPoses frame {frame}"
        );
        for (eye, name) in [vr::EVREye::Left, vr::EVREye::Right].into_iter().zip(eyes) {
            let tex = vr::Texture_t {
                handle: name as usize as *mut c_void,
                eType: vr::ETextureType::OpenGL,
                eColorSpace: vr::EColorSpace::Gamma,
            };
            let start = std::time::Instant::now();
            let err = vcall!(
                comp,
                IVRCompositor_Submit,
                eye,
                &tex,
                &bounds,
                vr::EVRSubmitFlags::Default
            );
            submit_time[eye.0 as usize] += start.elapsed();
            assert_eq!(
                err,
                vr::EVRCompositorError::None,
                "Submit frame {frame} {eye:?}"
            );
        }
    }

    let frames_run = frames.max(1);
    println!(
        "average frame {:?}, WaitGetPoses {:?}, Submit left {:?}, right {:?}",
        loop_start.elapsed() / frames_run,
        wait_time / frames_run,
        submit_time[0] / frames_run,
        submit_time[1] / frames_run,
    );
    if frames > 2 {
        // Timestamp queries are read a frame or more later.
        let mut timing = vr::Compositor_FrameTiming {
            m_nSize: size_of::<vr::Compositor_FrameTiming>() as u32,
            ..Default::default()
        };
        assert!(vcall!(comp, IVRCompositor_GetFrameTiming, &mut timing, 1));
        let gpu = timing.m_flTotalRenderGpuMs;
        println!("gpu time of the frame before the last: {gpu}ms");
        assert!(gpu > 0.0 && gpu < 1000.0);
    }
    vcall!(core, IVRClientCore_Cleanup);
    unsafe { gl::DeleteTextures(2, eyes.as_ptr()) };
    // The library has leaked state and background threads by design; a game process just exits.
    std::mem::forget(lib);
}
