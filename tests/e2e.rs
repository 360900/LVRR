//! End-to-end smoke test against a real OpenXR runtime.
//!
//! Loads the built vrclient.so and drives it the way a scene application does:
//! Vulkan setup through IVRCompositor, a WaitGetPoses/Submit frame loop,
//! manifest based input, render models, an overlay and the misc interfaces.
//!
//! Skipped unless `XRIZER_E2E` is set, since it needs a running runtime
//! (`XR_RUNTIME_JSON`) and a Vulkan device:
//!
//! ```sh
//! XRIZER_E2E=1 XR_RUNTIME_JSON=... RUST_LOG=info cargo test --test e2e -- --nocapture
//! ```

use ash::vk::{self, Handle};
use libloading::{Library, Symbol};
use openvr as vr;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

const FRAMES: u32 = 120;
const FULL_BOUNDS: vr::VRTextureBounds_t = vr::VRTextureBounds_t {
    uMin: 0.0,
    vMin: 0.0,
    uMax: 1.0,
    vMax: 1.0,
};

/// Calls `method` through an OpenVR interface's C++ vtable.
macro_rules! vcall {
    ($this:expr, $method:ident $(, $arg:expr)* $(,)?) => {
        unsafe { ((*(*$this).vtable_).$method)($this $(, $arg)*) }
    };
}

/// Calls a vtable method that returns an OpenVR error enum and asserts it reports no error.
/// Every OpenVR error enum is 32 bits and uses 0 for its `None`/`Success` variant.
macro_rules! vcheck {
    ($this:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let err = vcall!($this, $method $(, $arg)*);
        let code: i32 = unsafe { std::mem::transmute_copy(&err) };
        assert!(code == 0, "{} failed: {err:?}", stringify!($method));
    }};
}

/// Only for plain C structs without enum fields, where all zero is a valid value.
fn zeroed<T>() -> T {
    unsafe { std::mem::zeroed() }
}

fn size<T>() -> u32 {
    size_of::<T>() as u32
}

fn c_string(buf: &[c_char]) -> String {
    assert!(buf.contains(&0), "string buffer is not NUL terminated");
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn section(name: &str) {
    println!("\n=== {name} ===");
}

#[derive(Default)]
struct Stats {
    n: u32,
    min: Option<Duration>,
    max: Duration,
    total: Duration,
}

impl Stats {
    fn add(&mut self, d: Duration) {
        self.n += 1;
        self.min = Some(self.min.map_or(d, |m| m.min(d)));
        self.max = self.max.max(d);
        self.total += d;
    }

    fn report(&self, name: &str) {
        let avg = self.total.checked_div(self.n).unwrap_or_default();
        let min = self.min.unwrap_or_default();
        println!(
            "timing {name}: n={} min={min:?} avg={avg:?} max={:?}",
            self.n, self.max
        );
    }
}

fn timed<R>(stats: &mut Stats, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let ret = f();
    stats.add(start.elapsed());
    ret
}

/// OpenVR string buffer protocol: call with a null buffer to get the size, then fill it.
fn string_buffer(get: impl Fn(*mut c_char, u32) -> u32) -> String {
    let len = get(null_mut(), 0);
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0 as c_char; len as usize];
    assert_eq!(get(buf.as_mut_ptr(), len), len);
    c_string(&buf)
}

fn string_prop(
    sys: *mut vr::IVRSystem022,
    device: vr::TrackedDeviceIndex_t,
    prop: vr::ETrackedDeviceProperty,
) -> Result<String, vr::ETrackedPropertyError> {
    let mut err = vr::ETrackedPropertyError::Success;
    let mut buf = vec![0 as c_char; vr::k_unMaxPropertyStringSize as usize];
    let len = vcall!(
        sys,
        IVRSystem_GetStringTrackedDeviceProperty,
        device,
        prop,
        buf.as_mut_ptr(),
        buf.len() as u32,
        &mut err
    );
    if err != vr::ETrackedPropertyError::Success {
        return Err(err);
    }
    assert!(len as usize <= buf.len());
    Ok(c_string(&buf))
}

fn drain_events(sys: *mut vr::IVRSystem022) -> Vec<u32> {
    let mut events = Vec::new();
    let mut event: vr::VREvent_t = zeroed();
    while vcall!(
        sys,
        IVRSystem_PollNextEvent,
        &mut event,
        size::<vr::VREvent_t>()
    ) {
        events.push(event.eventType);
        assert!(events.len() < 10_000, "event queue never drains");
    }
    events
}

struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

/// The Vulkan side of a game, set up the way the OpenVR Vulkan docs describe.
struct Vk {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family_index: u32,
    pool: vk::CommandPool,
    fence: vk::Fence,
}

impl Vk {
    fn new(sys: *mut vr::IVRSystem022, comp: *mut vr::IVRCompositor028) -> Self {
        let entry = unsafe { ash::Entry::load() }.expect("Vulkan loader not found");
        let split = |exts: String| -> Vec<CString> {
            exts.split_ascii_whitespace()
                .map(|e| CString::new(e).unwrap())
                .collect()
        };

        let instance_exts = string_buffer(|buf, len| {
            vcall!(
                comp,
                IVRCompositor_GetVulkanInstanceExtensionsRequired,
                buf,
                len
            )
        });
        println!("required instance extensions: {instance_exts:?}");
        let instance_exts = split(instance_exts);
        let ext_ptrs: Vec<*const c_char> = instance_exts.iter().map(|e| e.as_ptr()).collect();
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"xrizer e2e")
            .api_version(vk::API_VERSION_1_1);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&ext_ptrs),
                None,
            )
        }
        .expect("vkCreateInstance");

        let mut pd_raw = 0u64;
        vcall!(
            sys,
            IVRSystem_GetOutputDevice,
            &mut pd_raw,
            vr::ETextureType::Vulkan,
            instance.handle().as_raw() as *mut _
        );
        assert_ne!(pd_raw, 0, "GetOutputDevice returned no physical device");
        let physical_device = vk::PhysicalDevice::from_raw(pd_raw);
        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        println!("output device: {:?}", props.device_name_as_c_str());

        let device_exts = string_buffer(|buf, len| {
            vcall!(
                comp,
                IVRCompositor_GetVulkanDeviceExtensionsRequired,
                pd_raw as *mut _,
                buf,
                len
            )
        });
        println!("required device extensions: {device_exts:?}");
        let device_exts = split(device_exts);
        let ext_ptrs: Vec<*const c_char> = device_exts.iter().map(|e| e.as_ptr()).collect();

        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let queue_family_index = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .expect("no graphics queue family") as u32;
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&[1.0]);
        let device = unsafe {
            instance.create_device(
                physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(std::slice::from_ref(&queue_info))
                    .enabled_extension_names(&ext_ptrs),
                None,
            )
        }
        .expect("vkCreateDevice");
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family_index)
                    .flags(vk::CommandPoolCreateFlags::TRANSIENT),
                None,
            )
        }
        .unwrap();
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }.unwrap();

        Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue,
            queue_family_index,
            pool,
            fence,
        }
    }

    fn create_image(&self, width: u32, height: u32) -> Image {
        let image = unsafe {
            self.device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_SRGB)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | vk::ImageUsageFlags::TRANSFER_SRC
                            | vk::ImageUsageFlags::TRANSFER_DST
                            | vk::ImageUsageFlags::SAMPLED,
                    )
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .unwrap();
        let reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };
        let memory_type_index = (0..mem_props.memory_type_count)
            .find(|i| {
                reqs.memory_type_bits & (1 << i) != 0
                    && mem_props.memory_types[*i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .expect("no device local memory type");
        let memory = unsafe {
            self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(memory_type_index),
                None,
            )
        }
        .unwrap();
        unsafe { self.device.bind_image_memory(image, memory, 0) }.unwrap();
        Image {
            image,
            memory,
            width,
            height,
        }
    }

    /// Clears the image and leaves it in TRANSFER_SRC_OPTIMAL, as OpenVR expects submitted
    /// Vulkan textures to be.
    fn clear(&self, image: &Image, color: [f32; 4]) {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |old, new, src, dst| {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(src)
                .dst_access_mask(dst)
                .old_layout(old)
                .new_layout(new)
                .image(image.image)
                .subresource_range(range)
        };
        let buf = unsafe {
            self.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .unwrap()[0];
        unsafe {
            self.device
                .begin_command_buffer(
                    buf,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .unwrap();
            self.device.cmd_pipeline_barrier(
                buf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
            );
            self.device.cmd_clear_color_image(
                buf,
                image.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: color },
                &[range],
            );
            self.device.cmd_pipeline_barrier(
                buf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::SHADER_READ,
                )],
            );
            self.device.end_command_buffer(buf).unwrap();
            self.device
                .queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default().command_buffers(&[buf])],
                    self.fence,
                )
                .unwrap();
            self.device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .unwrap();
            self.device.reset_fences(&[self.fence]).unwrap();
            self.device.free_command_buffers(self.pool, &[buf]);
        }
    }

    fn texture_data(&self, image: &Image) -> vr::VRVulkanTextureData_t {
        vr::VRVulkanTextureData_t {
            m_nImage: image.image.as_raw(),
            m_pDevice: self.device.handle().as_raw() as *mut _,
            m_pPhysicalDevice: self.physical_device.as_raw() as *mut _,
            m_pInstance: self.instance.handle().as_raw() as *mut _,
            m_pQueue: self.queue.as_raw() as *mut _,
            m_nQueueFamilyIndex: self.queue_family_index,
            m_nWidth: image.width,
            m_nHeight: image.height,
            m_nFormat: vk::Format::R8G8B8A8_SRGB.as_raw() as u32,
            m_nSampleCount: 1,
        }
    }

    fn destroy(self, images: Vec<Image>) {
        unsafe {
            self.device.device_wait_idle().unwrap();
            for image in images {
                self.device.destroy_image(image.image, None);
                self.device.free_memory(image.memory, None);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn texture(data: &vr::VRVulkanTextureData_t) -> vr::Texture_t {
    vr::Texture_t {
        handle: std::ptr::from_ref(data).cast_mut().cast(),
        eType: vr::ETextureType::Vulkan,
        eColorSpace: vr::EColorSpace::Auto,
    }
}

struct Interfaces {
    core: *mut vr::IVRClientCore003,
    system: *mut vr::IVRSystem022,
    compositor: *mut vr::IVRCompositor028,
    input: *mut vr::IVRInput010,
    render_models: *mut vr::IVRRenderModels006,
    overlay: *mut vr::IVROverlay027,
    settings: *mut vr::IVRSettings003,
    chaperone: *mut vr::IVRChaperone004,
    applications: *mut vr::IVRApplications007,
}

impl Interfaces {
    fn get<T>(core: *mut vr::IVRClientCore003, version: &CStr) -> *mut T {
        let mut err = vr::EVRInitError::Unknown;
        let ptr = vcall!(
            core,
            IVRClientCore_GetGenericInterface,
            version.as_ptr(),
            &mut err
        );
        assert_eq!(err, vr::EVRInitError::None, "{version:?}");
        assert!(!ptr.is_null(), "{version:?} returned null");
        vcheck!(
            core,
            IVRClientCore_IsInterfaceVersionValid,
            version.as_ptr()
        );
        ptr.cast()
    }

    fn new(core: *mut vr::IVRClientCore003) -> Self {
        Self {
            core,
            system: Self::get(core, vr::IVRSystem022::VERSION),
            compositor: Self::get(core, vr::IVRCompositor028::VERSION),
            input: Self::get(core, vr::IVRInput010::VERSION),
            render_models: Self::get(core, vr::IVRRenderModels006::VERSION),
            overlay: Self::get(core, vr::IVROverlay027::VERSION),
            settings: Self::get(core, vr::IVRSettings003::VERSION),
            chaperone: Self::get(core, vr::IVRChaperone004::VERSION),
            applications: Self::get(core, vr::IVRApplications007::VERSION),
        }
    }
}

struct Actions {
    set: vr::VRActionSetHandle_t,
    bool_act: vr::VRActionHandle_t,
    vec1: vr::VRActionHandle_t,
    vec2: vr::VRActionHandle_t,
    pose: vr::VRActionHandle_t,
    skeleton_left: vr::VRActionHandle_t,
    vibration: vr::VRActionHandle_t,
    hands: [vr::VRInputValueHandle_t; 2],
}

impl Actions {
    fn load(input: *mut vr::IVRInput010) -> Self {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/input_data/actions.json");
        let manifest = CString::new(manifest.to_str().unwrap()).unwrap();
        vcheck!(input, IVRInput_SetActionManifestPath, manifest.as_ptr());

        let mut set = vr::k_ulInvalidActionSetHandle;
        vcheck!(
            input,
            IVRInput_GetActionSetHandle,
            c"/actions/set1".as_ptr(),
            &mut set
        );
        assert_ne!(set, vr::k_ulInvalidActionSetHandle);
        let action = |name: &CStr| {
            let mut handle = vr::k_ulInvalidActionHandle;
            vcheck!(input, IVRInput_GetActionHandle, name.as_ptr(), &mut handle);
            assert_ne!(handle, vr::k_ulInvalidActionHandle, "{name:?}");
            handle
        };
        let source = |path: &CStr| {
            let mut handle = vr::k_ulInvalidInputValueHandle;
            vcheck!(
                input,
                IVRInput_GetInputSourceHandle,
                path.as_ptr(),
                &mut handle
            );
            assert_ne!(handle, vr::k_ulInvalidInputValueHandle, "{path:?}");
            handle
        };

        Self {
            set,
            bool_act: action(c"/actions/set1/in/BoolAct"),
            vec1: action(c"/actions/set1/in/Vec1Act"),
            vec2: action(c"/actions/set1/in/Vec2Act"),
            pose: action(c"/actions/set1/in/Pose"),
            skeleton_left: action(c"/actions/set1/in/SkellyL"),
            vibration: action(c"/actions/set1/in/Vib"),
            hands: [source(c"/user/hand/left"), source(c"/user/hand/right")],
        }
    }

    fn update(&self, input: *mut vr::IVRInput010) {
        let mut active = vr::VRActiveActionSet_t {
            ulActionSet: self.set,
            ..Default::default()
        };
        vcheck!(
            input,
            IVRInput_UpdateActionState,
            &mut active,
            size::<vr::VRActiveActionSet_t>(),
            1
        );
    }

    /// Reads every action type for both hands. Returns the hand pose data.
    fn read(&self, input: *mut vr::IVRInput010) -> [vr::InputPoseActionData_t; 2] {
        let mut poses = [vr::InputPoseActionData_t::default(); 2];
        for (hand, pose) in self.hands.iter().zip(&mut poses) {
            let mut digital = vr::InputDigitalActionData_t::default();
            vcheck!(
                input,
                IVRInput_GetDigitalActionData,
                self.bool_act,
                &mut digital,
                size::<vr::InputDigitalActionData_t>(),
                *hand
            );
            for action in [self.vec1, self.vec2] {
                let mut analog = vr::InputAnalogActionData_t::default();
                vcheck!(
                    input,
                    IVRInput_GetAnalogActionData,
                    action,
                    &mut analog,
                    size::<vr::InputAnalogActionData_t>(),
                    *hand
                );
            }
            vcheck!(
                input,
                IVRInput_GetPoseActionDataRelativeToNow,
                self.pose,
                vr::ETrackingUniverseOrigin::Standing,
                0.0,
                pose,
                size::<vr::InputPoseActionData_t>(),
                *hand
            );
        }
        poses
    }
}

#[test]
#[cfg_attr(miri, ignore)]
fn scene_app() {
    if std::env::var_os("XRIZER_E2E").is_none() {
        println!("XRIZER_E2E not set, skipping end-to-end test");
        return;
    }

    let path = test_cdylib::build_current_project();
    let lib = unsafe { Library::new(path) }.unwrap();
    let factory: Symbol<unsafe extern "C-unwind" fn(*const c_char, *mut i32) -> *mut c_void> =
        unsafe { lib.get(b"VRClientCoreFactory\0") }.unwrap();

    section("init");
    let mut code = -1;
    let core = unsafe { factory(vr::IVRClientCore003::VERSION.as_ptr(), &mut code) }
        .cast::<vr::IVRClientCore003>();
    assert!(!core.is_null());
    assert_eq!(code, 0);
    vcheck!(
        core,
        IVRClientCore_Init,
        vr::EVRApplicationType::Scene,
        std::ptr::null()
    );
    assert!(vcall!(core, IVRClientCore_BIsHmdPresent));
    let unknown = vcall!(
        core,
        IVRClientCore_IsInterfaceVersionValid,
        c"IVRSystem_999".as_ptr()
    );
    assert_ne!(unknown, vr::EVRInitError::None);
    let i = Interfaces::new(core);
    let (sys, comp, input) = (i.system, i.compositor, i.input);

    section("system");
    let (mut width, mut height) = (0, 0);
    vcall!(
        sys,
        IVRSystem_GetRecommendedRenderTargetSize,
        &mut width,
        &mut height
    );
    println!("recommended render target size: {width}x{height}");
    assert!(width > 0 && height > 0);

    let mut eye_x = [0.0; 2];
    for eye in [vr::EVREye::Left, vr::EVREye::Right] {
        let [mut l, mut r, mut t, mut b] = [0.0f32; 4];
        vcall!(
            sys,
            IVRSystem_GetProjectionRaw,
            eye,
            &mut l,
            &mut r,
            &mut t,
            &mut b
        );
        println!("{eye:?} projection raw: left={l} right={r} top={t} bottom={b}");
        assert!([l, r, t, b].iter().all(|v| v.is_finite()));
        assert!(l < 0.0 && r > 0.0, "{eye:?} horizontal fov is degenerate");
        assert!(t != b, "{eye:?} vertical fov is degenerate");

        let proj = vcall!(sys, IVRSystem_GetProjectionMatrix, eye, 0.1, 100.0);
        assert!(proj.m.iter().flatten().all(|v| v.is_finite()));
        assert!(proj.m[0][0] > 0.0 && proj.m[1][1] > 0.0 && proj.m[3][2] == -1.0);

        let eye_to_head = vcall!(sys, IVRSystem_GetEyeToHeadTransform, eye);
        println!("{eye:?} eye to head: {:?}", eye_to_head.m);
        assert!(eye_to_head.m.iter().flatten().all(|v| v.is_finite()));
        assert!(eye_to_head.m[0][0] != 0.0);
        eye_x[eye.0 as usize] = eye_to_head.m[0][3];
    }
    assert!(eye_x[0] < eye_x[1], "eyes are not separated: {eye_x:?}");

    use vr::ETrackedDeviceProperty as Prop;
    const HMD: u32 = vr::k_unTrackedDeviceIndex_Hmd;
    const HMD_IDENTITY: [Prop; 4] = [
        Prop::TrackingSystemName_String,
        Prop::ModelNumber_String,
        Prop::ManufacturerName_String,
        Prop::SerialNumber_String,
    ];
    for prop in HMD_IDENTITY {
        let value = string_prop(sys, HMD, prop);
        println!("hmd {prop:?}: {value:?}");
        assert!(value.is_ok_and(|v| !v.is_empty()), "hmd {prop:?}");
    }
    let mut err = vr::ETrackedPropertyError::Success;
    for prop in [Prop::DisplayFrequency_Float, Prop::UserIpdMeters_Float] {
        let value = vcall!(
            sys,
            IVRSystem_GetFloatTrackedDeviceProperty,
            HMD,
            prop,
            &mut err
        );
        println!("hmd {prop:?}: {value} ({err:?})");
        assert_eq!(err, vr::ETrackedPropertyError::Success);
        assert!(value > 0.0, "hmd {prop:?}");
    }
    let class = vcall!(
        sys,
        IVRSystem_GetInt32TrackedDeviceProperty,
        HMD,
        Prop::DeviceClass_Int32,
        &mut err
    );
    // SteamVR answers 1 (HMD) here; xrizer currently reports UnknownProperty.
    println!("hmd DeviceClass_Int32: {class} ({err:?})");
    let prox = vcall!(
        sys,
        IVRSystem_GetBoolTrackedDeviceProperty,
        HMD,
        Prop::ContainsProximitySensor_Bool,
        &mut err
    );
    println!("hmd ContainsProximitySensor_Bool: {prox} ({err:?})");

    for eye in [vr::EVREye::Left, vr::EVREye::Right] {
        let mesh = vcall!(
            sys,
            IVRSystem_GetHiddenAreaMesh,
            eye,
            vr::EHiddenAreaMeshType::Standard
        );
        println!(
            "{eye:?} hidden area mesh: {} triangles",
            mesh.unTriangleCount
        );
        assert!(mesh.unTriangleCount == 0 || !mesh.pVertexData.is_null());
    }
    println!("events before first frame: {:?}", drain_events(sys));

    section("vulkan");
    let vk = Vk::new(sys, comp);
    let eyes = [
        vk.create_image(width, height),
        vk.create_image(width, height),
    ];
    let overlay_image = vk.create_image(256, 256);
    vk.clear(&overlay_image, [1.0, 1.0, 0.0, 1.0]);
    let overlay_data = vk.texture_data(&overlay_image);
    let overlay_texture = texture(&overlay_data);

    section("input setup");
    let actions = Actions::load(input);
    let mut level = vr::EVRSkeletalTrackingLevel::Estimated;
    vcheck!(
        input,
        IVRInput_GetSkeletalTrackingLevel,
        actions.skeleton_left,
        &mut level
    );
    let mut bones = 0;
    vcheck!(
        input,
        IVRInput_GetBoneCount,
        actions.skeleton_left,
        &mut bones
    );
    println!("left skeleton: tracking level {level:?}, {bones} bones");
    assert_eq!(bones, 31);

    section("overlay setup");
    let mut overlay = vr::k_ulOverlayHandleInvalid;
    vcheck!(
        i.overlay,
        IVROverlay_CreateOverlay,
        c"xrizer.e2e".as_ptr(),
        c"xrizer e2e".as_ptr(),
        &mut overlay
    );
    assert_ne!(overlay, vr::k_ulOverlayHandleInvalid);
    vcheck!(i.overlay, IVROverlay_SetOverlayWidthInMeters, overlay, 0.5);
    let transform = vr::HmdMatrix34_t {
        m: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 1.5],
            [0.0, 0.0, 1.0, -1.0],
        ],
    };
    vcheck!(
        i.overlay,
        IVROverlay_SetOverlayTransformAbsolute,
        overlay,
        vr::ETrackingUniverseOrigin::Standing,
        &transform
    );

    section("frame loop");
    vcall!(
        comp,
        IVRCompositor_SetTrackingSpace,
        vr::ETrackingUniverseOrigin::Standing
    );
    assert_eq!(
        vcall!(comp, IVRCompositor_GetTrackingSpace),
        vr::ETrackingUniverseOrigin::Standing
    );
    println!(
        "CanRenderScene={} IsFullscreen={}",
        vcall!(comp, IVRCompositor_CanRenderScene),
        vcall!(comp, IVRCompositor_IsFullscreen)
    );

    let mut wait_stats = Stats::default();
    let mut submit_stats = Stats::default();
    let mut poses = [vr::TrackedDevicePose_t::default(); vr::k_unMaxTrackedDeviceCount as usize];
    let mut controllers = [vr::k_unTrackedDeviceIndexInvalid; 2];
    let mut controllers_found_at = None;
    let mut hand_poses_valid_at = None;
    let mut hmd_pose_valid_at = None;
    let mut events = Vec::new();

    for frame in 0..FRAMES {
        let err = timed(&mut wait_stats, || {
            vcall!(
                comp,
                IVRCompositor_WaitGetPoses,
                poses.as_mut_ptr(),
                poses.len() as u32,
                null_mut(),
                0
            )
        });
        assert_eq!(
            err,
            vr::EVRCompositorError::None,
            "WaitGetPoses frame {frame}"
        );
        let hmd = &poses[HMD as usize];
        if hmd.bPoseIsValid && hmd_pose_valid_at.is_none() {
            hmd_pose_valid_at = Some(frame);
            println!(
                "frame {frame}: hmd pose valid, {:?}",
                hmd.mDeviceToAbsoluteTracking.m
            );
        }
        assert!(hmd.bDeviceIsConnected, "frame {frame}: hmd not connected");
        if frame >= 5 {
            assert!(hmd.bPoseIsValid, "frame {frame}: hmd pose not valid");
            assert_eq!(hmd.eTrackingResult, vr::ETrackingResult::Running_OK);
        }
        events.extend(drain_events(sys));

        if controllers_found_at.is_none() {
            controllers = [
                vr::ETrackedControllerRole::LeftHand,
                vr::ETrackedControllerRole::RightHand,
            ]
            .map(|role| vcall!(sys, IVRSystem_GetTrackedDeviceIndexForControllerRole, role));
            if controllers
                .iter()
                .all(|c| *c != vr::k_unTrackedDeviceIndexInvalid)
            {
                controllers_found_at = Some(frame);
                println!("frame {frame}: controllers at indices {controllers:?}");
            }
        }

        actions.update(input);
        let hand_poses = actions.read(input);
        if hand_poses.iter().all(|p| p.bActive && p.pose.bPoseIsValid)
            && hand_poses_valid_at.is_none()
        {
            hand_poses_valid_at = Some(frame);
            let left = &hand_poses[0];
            println!(
                "frame {frame}: hand poses valid, left origin 0x{:x} at {:?}",
                left.activeOrigin, left.pose.mDeviceToAbsoluteTracking.m[0]
            );
            let mut info: vr::InputOriginInfo_t = zeroed();
            vcheck!(
                input,
                IVRInput_GetOriginTrackedDeviceInfo,
                left.activeOrigin,
                &mut info,
                size::<vr::InputOriginInfo_t>()
            );
            println!(
                "left origin: device {} path 0x{:x} component {:?}",
                info.trackedDeviceIndex,
                info.devicePath,
                c_string(&info.rchRenderModelComponentName)
            );
            assert_eq!(info.devicePath, actions.hands[0]);
            assert_eq!(info.trackedDeviceIndex, controllers[0]);
            vcheck!(
                input,
                IVRInput_TriggerHapticVibrationAction,
                actions.vibration,
                0.0,
                0.1,
                160.0,
                1.0,
                actions.hands[0]
            );
        }

        let shade = (frame % 60) as f32 / 60.0;
        vk.clear(&eyes[0], [shade, 0.0, 1.0 - shade, 1.0]);
        vk.clear(&eyes[1], [0.0, shade, 1.0 - shade, 1.0]);
        for (eye, image) in [vr::EVREye::Left, vr::EVREye::Right].into_iter().zip(&eyes) {
            let data = vk.texture_data(image);
            let tex = texture(&data);
            let err = timed(&mut submit_stats, || {
                vcall!(
                    comp,
                    IVRCompositor_Submit,
                    eye,
                    &tex,
                    &FULL_BOUNDS,
                    vr::EVRSubmitFlags::Default
                )
            });
            assert_eq!(
                err,
                vr::EVRCompositorError::None,
                "Submit frame {frame} {eye:?}"
            );
        }
        vcall!(comp, IVRCompositor_PostPresentHandoff);

        if frame == 40 {
            vcheck!(
                i.overlay,
                IVROverlay_SetOverlayTexture,
                overlay,
                &overlay_texture
            );
            vcheck!(i.overlay, IVROverlay_ShowOverlay, overlay);
            assert!(vcall!(i.overlay, IVROverlay_IsOverlayVisible, overlay));
        }
        if frame == 80 {
            vcheck!(i.overlay, IVROverlay_HideOverlay, overlay);
            assert!(!vcall!(i.overlay, IVROverlay_IsOverlayVisible, overlay));
        }
    }

    wait_stats.report("WaitGetPoses");
    submit_stats.report("Submit");
    println!("events during frame loop: {events:?}");
    println!("hmd pose valid at frame {hmd_pose_valid_at:?}");
    println!("controllers found at frame {controllers_found_at:?}");
    println!("hand poses valid at frame {hand_poses_valid_at:?}");
    assert!(controllers_found_at.is_some(), "controllers never appeared");
    assert!(
        hand_poses_valid_at.is_some(),
        "hand poses never became valid"
    );

    section("compositor stats");
    let mut timing = vr::Compositor_FrameTiming {
        m_nSize: size::<vr::Compositor_FrameTiming>(),
        ..Default::default()
    };
    assert!(vcall!(comp, IVRCompositor_GetFrameTiming, &mut timing, 0));
    // Compositor_FrameTiming is packed, so copy the fields out before formatting them.
    let (index, interval, presents) = (
        timing.m_nFrameIndex,
        timing.m_flClientFrameIntervalMs,
        timing.m_nNumFramePresents,
    );
    println!("frame timing: index={index} interval={interval}ms presents={presents}");
    assert!(index > 0 && interval > 0.0);
    let mut stats = vr::Compositor_CumulativeStats::default();
    vcall!(
        comp,
        IVRCompositor_GetCumulativeStats,
        &mut stats,
        size::<vr::Compositor_CumulativeStats>()
    );
    println!(
        "cumulative stats: pid={} presents={} dropped={}",
        stats.m_nPid, stats.m_nNumFramePresents, stats.m_nNumDroppedFrames
    );
    let remaining = vcall!(comp, IVRCompositor_GetFrameTimeRemaining);
    println!("frame time remaining: {remaining}s");
    assert!(remaining.is_finite() && remaining >= 0.0);

    section("devices");
    for index in 0..vr::k_unMaxTrackedDeviceCount {
        if vcall!(sys, IVRSystem_IsTrackedDeviceConnected, index) {
            let class = vcall!(sys, IVRSystem_GetTrackedDeviceClass, index);
            let role = vcall!(sys, IVRSystem_GetControllerRoleForTrackedDeviceIndex, index);
            println!("device {index}: {class:?} {role:?}");
            assert_ne!(class, vr::ETrackedDeviceClass::Invalid);
        }
    }
    for (index, hand) in controllers.iter().zip(["left", "right"]) {
        assert!(vcall!(sys, IVRSystem_IsTrackedDeviceConnected, *index));
        assert_eq!(
            vcall!(sys, IVRSystem_GetTrackedDeviceClass, *index),
            vr::ETrackedDeviceClass::Controller
        );
        for prop in [
            Prop::ControllerType_String,
            Prop::ModelNumber_String,
            Prop::SerialNumber_String,
        ] {
            println!("{hand} {prop:?}: {:?}", string_prop(sys, *index, prop));
        }
        let mut state = vr::VRControllerState_t::default();
        let ok = vcall!(
            sys,
            IVRSystem_GetControllerState,
            *index,
            &mut state,
            size::<vr::VRControllerState_t>()
        );
        // VRControllerState_t is packed, so copy the fields out before formatting them.
        let (packet, pressed, touched, axis0) = (
            state.unPacketNum,
            state.ulButtonPressed,
            state.ulButtonTouched,
            state.rAxis[0],
        );
        println!(
            "{hand} legacy state: ok={ok} packet={packet} pressed=0x{pressed:x} touched=0x{touched:x} axis0={axis0:?}"
        );
        // xrizer deliberately returns false here once an action manifest is loaded.
        if !ok {
            println!("{hand} GetControllerState returned false with an action manifest loaded");
        }
    }
    // Games that sniff the HMD identity once at startup get the values from before the
    // controllers were known; show whether they change once the profile is resolved.
    for prop in HMD_IDENTITY {
        let value = string_prop(sys, HMD, prop);
        println!("hmd {prop:?} after frame loop: {value:?}");
    }

    section("render models");
    let rm = i.render_models;
    let count = vcall!(rm, IVRRenderModels_GetRenderModelCount);
    println!("{count} render models");
    assert!(count > 0);
    for index in 0..count {
        let name = string_buffer(|buf, len| {
            vcall!(rm, IVRRenderModels_GetRenderModelName, index, buf, len)
        });
        assert!(!name.is_empty());
        println!("  {name}");
    }
    let load = |name: &CStr| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut model = null_mut();
            match vcall!(
                rm,
                IVRRenderModels_LoadRenderModel_Async,
                name.as_ptr(),
                &mut model
            ) {
                vr::EVRRenderModelError::Loading => {
                    assert!(Instant::now() < deadline, "{name:?} never finished loading");
                    std::thread::sleep(Duration::from_millis(10));
                }
                vr::EVRRenderModelError::None => return Ok(model),
                err => return Err(err),
            }
        }
    };
    for (index, hand) in controllers.iter().zip(["left", "right"]) {
        let name = string_prop(sys, *index, Prop::RenderModelName_String).unwrap();
        let cname = CString::new(name.clone()).unwrap();
        let components = vcall!(rm, IVRRenderModels_GetComponentCount, cname.as_ptr());
        match load(&cname) {
            Ok(model) => {
                let m = unsafe { &*model };
                println!(
                    "{hand} render model {name:?}: {} vertices, {} triangles, {components} components",
                    m.unVertexCount, m.unTriangleCount
                );
                assert!(m.unVertexCount > 0 && m.unTriangleCount > 0);
                vcall!(rm, IVRRenderModels_FreeRenderModel, model);
            }
            Err(err) => println!(
                "{hand} render model {name:?}: load failed with {err:?} ({components} components)"
            ),
        }
    }
    let generic = load(c"generic_controller").expect("generic_controller");
    let m = unsafe { &*generic };
    println!(
        "generic_controller: {} vertices, {} triangles",
        m.unVertexCount, m.unTriangleCount
    );
    assert!(m.unVertexCount > 0 && m.unTriangleCount > 0);
    vcall!(rm, IVRRenderModels_FreeRenderModel, generic);

    section("settings / chaperone / applications");
    let mut err = vr::EVRSettingsError::None;
    let scale = vcall!(
        i.settings,
        IVRSettings_GetFloat,
        c"steamvr".as_ptr(),
        c"supersampleScale".as_ptr(),
        &mut err
    );
    println!("steamvr/supersampleScale: {scale} ({err:?})");
    let fade = vcall!(
        i.settings,
        IVRSettings_GetBool,
        c"steamvr".as_ptr(),
        c"forceFadeOnBadTracking".as_ptr(),
        &mut err
    );
    println!("steamvr/forceFadeOnBadTracking: {fade} ({err:?})");

    let state = vcall!(i.chaperone, IVRChaperone_GetCalibrationState);
    let (mut x, mut z) = (0.0, 0.0);
    let has_size = vcall!(i.chaperone, IVRChaperone_GetPlayAreaSize, &mut x, &mut z);
    let mut rect: vr::HmdQuad_t = zeroed();
    let has_rect = vcall!(i.chaperone, IVRChaperone_GetPlayAreaRect, &mut rect);
    println!("chaperone: {state:?} size={has_size} ({x}x{z}) rect={has_rect} {rect:?}");
    assert!(has_size && x > 0.0 && z > 0.0);

    let mut key = [0 as c_char; 256];
    let err = vcall!(
        i.applications,
        IVRApplications_GetApplicationKeyByProcessId,
        std::process::id(),
        key.as_mut_ptr(),
        key.len() as u32
    );
    println!("application key: {err:?} {:?}", c_string(&key));
    let scene_pid = vcall!(i.applications, IVRApplications_GetCurrentSceneProcessId);
    println!("scene process id: {scene_pid}");
    assert_eq!(scene_pid, std::process::id());

    section("cleanup");
    vcheck!(i.overlay, IVROverlay_DestroyOverlay, overlay);
    vcall!(i.core, IVRClientCore_Cleanup);
    let [left, right] = eyes;
    vk.destroy(vec![left, right, overlay_image]);
    // The library has leaked state and background threads by design; a game process just exits.
    std::mem::forget(lib);
    println!("done");
}
