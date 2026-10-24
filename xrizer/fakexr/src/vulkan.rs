use ash::vk::{self, Handle};
use openxr_sys as xr_sys;
use paste::paste;
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char};

thread_local! {
    static DEVICE_LOST: Cell<bool> = const { Cell::new(false) };
}

/// Makes devices report VK_ERROR_DEVICE_LOST on this thread.
pub fn set_device_lost(lost: bool) {
    DEVICE_LOST.set(lost);
}

macro_rules! get_fn {
    ($name:expr => $($fn:ident),+) => {
        match $name {
            $(
                x if x == unsafe {
                    const {
                        CStr::from_bytes_with_nul_unchecked(concat!("vk", stringify!($fn), "\0").as_bytes())
                    }
                } => paste! {
                    Some(unsafe { std::mem::transmute([<$fn:snake>] as vk::[<PFN_vk $fn>]) })
                },
            )+
            _ => None
        }
    }
}

thread_local! {
    static DEVICE_EXTENSIONS: RefCell<Vec<&'static CStr>> =
        RefCell::new(vec![c"VK_foo", c"VK_bar"]);
}

/// Sets the extensions physical devices support on this thread. They support all the extensions
/// the runtime requires by default.
pub fn set_device_extensions(extensions: &[&'static CStr]) {
    DEVICE_EXTENSIONS.set(extensions.to_vec());
}

pub fn session_create_info() -> xr_sys::GraphicsBindingVulkanKHR {
    xr_sys::GraphicsBindingVulkanKHR {
        ty: xr_sys::GraphicsBindingVulkanKHR::TYPE,
        next: std::ptr::null(),
        instance: std::ptr::null(),
        physical_device: std::ptr::null(),
        device: std::ptr::null(),
        queue_family_index: 0,
        queue_index: 0,
    }
}

#[allow(clippy::missing_transmute_annotations, clippy::missing_safety_doc)]
pub unsafe extern "system" fn get_instance_proc_addr(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let name = unsafe { CStr::from_ptr(name) };

    if instance.is_null() {
        get_fn![name => CreateInstance]
    } else {
        get_fn![name =>
            GetPhysicalDeviceQueueFamilyProperties,
            EnumerateDeviceExtensionProperties,
            CreateDevice,
            GetDeviceProcAddr,
            GetDeviceQueue,
            DestroyInstance
        ]
    }
}
#[allow(clippy::missing_transmute_annotations)]
extern "system" fn get_device_proc_addr(
    device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if device.is_null() {
        None
    } else {
        let name = unsafe { CStr::from_ptr(name) };
        get_fn![name => GetDeviceQueue, DeviceWaitIdle, DestroyDevice]
    }
}

struct Instance;

#[repr(C)]
pub(crate) struct Device {
    debug: u64,
}

impl Device {
    const DEBUG_VAL: u64 = 946;
    pub fn validate(ptr: u64) -> bool {
        if ptr == 0 {
            return false;
        }
        let debug = ptr as *const u64;
        let val = unsafe { *debug };
        val == Device::DEBUG_VAL
    }
}

extern "system" fn create_instance(
    _: *const vk::InstanceCreateInfo<'_>,
    _: *const vk::AllocationCallbacks<'_>,
    instance: *mut vk::Instance,
) -> vk::Result {
    let inst = Box::new(Instance);
    unsafe {
        *instance = vk::Instance::from_raw(Box::into_raw(inst) as u64);
    }
    vk::Result::SUCCESS
}

extern "system" fn destroy_instance(instance: vk::Instance, _: *const vk::AllocationCallbacks<'_>) {
    drop(unsafe { Box::from_raw(instance.as_raw() as *mut Instance) });
}

extern "system" fn enumerate_device_extension_properties(
    _: vk::PhysicalDevice,
    _: *const c_char,
    count: *mut u32,
    properties: *mut vk::ExtensionProperties,
) -> vk::Result {
    let extensions = DEVICE_EXTENSIONS.with_borrow(Clone::clone);
    if properties.is_null() {
        unsafe { *count = extensions.len() as u32 };
        return vk::Result::SUCCESS;
    }
    let properties = unsafe { std::slice::from_raw_parts_mut(properties, *count as usize) };
    for (property, extension) in properties.iter_mut().zip(&extensions) {
        *property = vk::ExtensionProperties::default()
            .extension_name(extension)
            .unwrap();
    }
    if properties.len() < extensions.len() {
        return vk::Result::INCOMPLETE;
    }
    unsafe { *count = extensions.len() as u32 };
    vk::Result::SUCCESS
}

extern "system" fn create_device(
    _: vk::PhysicalDevice,
    info: *const vk::DeviceCreateInfo,
    _: *const vk::AllocationCallbacks<'_>,
    device: *mut vk::Device,
) -> vk::Result {
    let info = unsafe { &*info };
    let supported = DEVICE_EXTENSIONS.with_borrow(Clone::clone);
    let requested = match info.enabled_extension_count {
        0 => &[][..],
        count => unsafe { std::slice::from_raw_parts(info.pp_enabled_extension_names, count as _) },
    };
    if requested
        .iter()
        .any(|ext| !supported.contains(&unsafe { CStr::from_ptr(*ext) }))
    {
        return vk::Result::ERROR_EXTENSION_NOT_PRESENT;
    }
    let d = Box::new(Device {
        debug: Device::DEBUG_VAL,
    });
    unsafe {
        *device = vk::Device::from_raw(Box::into_raw(d) as u64);
    }
    vk::Result::SUCCESS
}

extern "system" fn destroy_device(device: vk::Device, _: *const vk::AllocationCallbacks<'_>) {
    drop(unsafe { Box::from_raw(device.as_raw() as *mut Device) });
}

extern "system" fn get_device_queue(
    _: vk::Device,
    queue_family_index: u32,
    queue_index: u32,
    queue: *mut vk::Queue,
) {
    if queue_index == 0 && queue_family_index == 0 {
        unsafe {
            *queue = vk::Queue::from_raw(1);
        }
    }
}

extern "system" fn device_wait_idle(_: vk::Device) -> vk::Result {
    if DEVICE_LOST.get() {
        vk::Result::ERROR_DEVICE_LOST
    } else {
        vk::Result::SUCCESS
    }
}

extern "system" fn get_physical_device_queue_family_properties(
    _physical_device: vk::PhysicalDevice,
    queue_family_property_count: *mut u32,
    queue_family_properties: *mut vk::QueueFamilyProperties,
) {
    unsafe { *queue_family_property_count = 1 };
    if let Some(props) = unsafe { queue_family_properties.as_mut() } {
        *props = vk::QueueFamilyProperties {
            queue_count: 1,
            queue_flags: vk::QueueFlags::GRAPHICS,
            ..Default::default()
        };
    }
}

pub(crate) mod xr {
    use openxr_sys as xr;
    use std::ffi::{CStr, c_char};

    pub extern "system" fn get_vulkan_instance_extensions_k_h_r(
        _: xr::Instance,
        _: xr::SystemId,
        buffer_capacity_input: u32,
        buffer_count_output: *mut u32,
        buffer: *mut c_char,
    ) -> xr::Result {
        static EXTS: &CStr = c"VK_foo VK_bar";
        static LEN: usize = EXTS.count_bytes() + 1;

        if !buffer_count_output.is_null() {
            unsafe { *buffer_count_output = LEN as u32 };
        }
        if buffer_capacity_input >= LEN as u32 {
            let buf =
                unsafe { std::slice::from_raw_parts_mut(buffer, buffer_capacity_input as usize) };
            let bytes = EXTS.to_bytes_with_nul();
            let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr() as _, bytes.len()) };
            buf[..LEN].copy_from_slice(bytes);
        } else if buffer_capacity_input > 0 {
            return xr::Result::ERROR_SIZE_INSUFFICIENT;
        }
        xr::Result::SUCCESS
    }

    pub extern "system" fn get_vulkan_device_extensions_k_h_r(
        instance: xr::Instance,
        system_id: xr::SystemId,
        buffer_capacity_input: u32,
        buffer_count_output: *mut u32,
        buffer: *mut c_char,
    ) -> xr::Result {
        get_vulkan_instance_extensions_k_h_r(
            instance,
            system_id,
            buffer_capacity_input,
            buffer_count_output,
            buffer,
        )
    }

    pub extern "system" fn get_vulkan_graphics_device_k_h_r(
        _: xr::Instance,
        _: xr::SystemId,
        vulkan_instance: xr::platform::VkInstance,
        _: *mut xr::platform::VkPhysicalDevice,
    ) -> xr::Result {
        // Monado can't enumerate physical devices without an instance.
        if vulkan_instance.is_null() {
            return xr::Result::ERROR_RUNTIME_FAILURE;
        }
        xr::Result::SUCCESS
    }

    pub extern "system" fn get_vulkan_graphics_requirements_k_h_r(
        _: xr::Instance,
        _: xr::SystemId,
        graphics_requirements: *mut xr::GraphicsRequirementsVulkanKHR,
    ) -> xr::Result {
        unsafe {
            *graphics_requirements = xr::GraphicsRequirementsVulkanKHR {
                ty: xr::GraphicsRequirementsVulkanKHR::TYPE,
                next: std::ptr::null_mut(),
                min_api_version_supported: xr::Version::new(1, 0, 0),
                max_api_version_supported: xr::Version::new(1, 0, 0),
            }
        };
        xr::Result::SUCCESS
    }
}
