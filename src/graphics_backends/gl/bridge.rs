//! OpenGL apps on another GPU than the runtime, e.g. the iGPU of a hybrid laptop, whose images the
//! runtime can't import. The session is a Vulkan one on the runtime's device instead: [GlData]
//! copies the app's textures into textures of ours, which are read back into host memory that
//! the runtime's device copies into the swapchain images.

use super::{GlData, GlTexture, cvt};
use crate::graphics_backends::{GraphicsBackend, VulkanData};
use ash::vk::{self, Handle};
use gl::types::{GLenum, GLint, GLuint};
use openvr as vr;
use openxr as xr;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};
use xr::sys::Handle as _;

/// A Vulkan session, as its own type so it isn't taken for the session of a Vulkan app.
pub enum GlBridgeApi {}

impl xr::Graphics for GlBridgeApi {
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
        instance: &xr::Instance,
        system: xr::SystemId,
    ) -> xr::Result<Self::Requirements> {
        xr::Vulkan::requirements(instance, system)
    }

    unsafe fn create_session(
        instance: &xr::Instance,
        system: xr::SystemId,
        info: &Self::SessionCreateInfo,
    ) -> xr::Result<xr::sys::Session> {
        unsafe { xr::Vulkan::create_session(instance, system, info) }
    }

    fn enumerate_swapchain_images(swapchain: &xr::Swapchain<Self>) -> xr::Result<Vec<u64>> {
        let enumerate = swapchain.instance().fp().enumerate_swapchain_images;
        let mut count = 0;
        cvt(unsafe { enumerate(swapchain.as_raw(), 0, &mut count, ptr::null_mut()) })?;
        let mut images = vec![
            xr::sys::SwapchainImageVulkanKHR {
                ty: xr::sys::SwapchainImageVulkanKHR::TYPE,
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

/// A swapchain format, the format of our textures with the same bytes per pixel and how to read
/// them back.
#[derive(Copy, Clone)]
struct Format {
    vk: vk::Format,
    gl: GLenum,
    read: (GLenum, GLenum),
    size: u32,
}

const fn format(vk: vk::Format, gl: GLenum, read: (GLenum, GLenum), size: u32) -> Format {
    Format { vk, gl, read, size }
}

/// glReadPixels doesn't decode sRGB, so the bytes are copied as they are.
const FORMATS: [Format; 6] = [
    format(
        vk::Format::R8G8B8A8_SRGB,
        gl::SRGB8_ALPHA8,
        (gl::RGBA, gl::UNSIGNED_BYTE),
        4,
    ),
    format(
        vk::Format::B8G8R8A8_SRGB,
        gl::SRGB8_ALPHA8,
        (gl::BGRA, gl::UNSIGNED_BYTE),
        4,
    ),
    format(
        vk::Format::R8G8B8A8_UNORM,
        gl::RGBA8,
        (gl::RGBA, gl::UNSIGNED_BYTE),
        4,
    ),
    format(
        vk::Format::B8G8R8A8_UNORM,
        gl::RGBA8,
        (gl::BGRA, gl::UNSIGNED_BYTE),
        4,
    ),
    format(
        vk::Format::R16G16B16A16_SFLOAT,
        gl::RGBA16F,
        (gl::RGBA, gl::HALF_FLOAT),
        8,
    ),
    format(
        vk::Format::A2B10G10R10_UNORM_PACK32,
        gl::RGB10_A2,
        (gl::RGBA, gl::UNSIGNED_INT_2_10_10_10_REV),
        4,
    ),
];

/// The runtime's device, shared by the scene and overlay backends since they copy into the
/// swapchains of the same session.
struct Device {
    vk: VulkanData,
    /// Submits need the queue externally synchronized.
    queue: Mutex<()>,
}

/// The device stays alive until another instance needs one: the session and its swapchains are
/// destroyed after the backends, and the runtime still uses the device then.
fn shared_device(instance: &xr::Instance) -> Result<Arc<Device>, String> {
    static DEVICE: Mutex<Option<(u64, Arc<Device>)>> = Mutex::new(None);
    let mut shared = DEVICE.lock().unwrap_or_else(PoisonError::into_inner);
    let handle = instance.as_raw().into_raw();
    if let Some((h, device)) = shared.as_ref()
        && *h == handle
    {
        return Ok(device.clone());
    }

    let system = instance
        .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
        .map_err(|e| format!("Failed to get the system: {e}"))?;
    let device = Arc::new(Device {
        vk: VulkanData::try_new_temporary_with(instance, system, true)?,
        queue: Mutex::new(()),
    });
    *shared = Some((handle, device.clone()));
    Ok(device)
}

/// Far longer than any copy takes, so only a hung or lost GPU hits it.
const COPY_TIMEOUT_NS: u64 = 1_000_000_000;

/// A layer read into a slot's pixel buffer, not uploaded yet.
struct Pending {
    sync: gl::types::GLsync,
    image: vk::Image,
    layer: u32,
    extent: xr::Extent2Di,
}

/// One layer of a swapchain image on its way to the runtime's GPU: GL reads it into a pixel
/// buffer in host memory of ours, which the runtime's device imports. Without either, the
/// pixel buffer is copied into host memory of the runtime's device.
struct Slot {
    /// Page aligned memory the pixel buffer uses, see GL_AMD_pinned_memory.
    host: Option<(*mut c_void, usize)>,
    pbo: GLuint,
    /// Where the CPU sees the pixel buffer's contents.
    pbo_mapped: *mut c_void,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Where `memory` is mapped if it isn't the pixel buffer's.
    copy_to: Option<*mut c_void>,
    cmd: vk::CommandBuffer,
    /// Signaled once the last upload has finished reading `memory`.
    fence: vk::Fence,
    pending: Option<Pending>,
}
// SAFETY: The pointers and the GL sync are only used while the slot is locked.
unsafe impl Send for Slot {}

const fn align_up(size: vk::DeviceSize, alignment: vk::DeviceSize) -> vk::DeviceSize {
    size.div_ceil(alignment) * alignment
}

/// GL_AMD_pinned_memory
const EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD: GLenum = 0x9160;

impl Slot {
    fn new(vk: &VulkanData, pool: vk::CommandPool, size: vk::DeviceSize) -> Result<Self, String> {
        let mut slot = Self {
            host: None,
            pbo: 0,
            pbo_mapped: ptr::null_mut(),
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            copy_to: None,
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            pending: None,
        };
        match slot.init(vk, pool, size) {
            Ok(()) => Ok(slot),
            Err(e) => {
                slot.destroy(&vk.device, pool, true);
                Err(e)
            }
        }
    }

    fn init(
        &mut self,
        vk: &VulkanData,
        pool: vk::CommandPool,
        size: vk::DeviceSize,
    ) -> Result<(), String> {
        let pinned = vk.host_import_alignment.is_some()
            && super::has_gl_extension(c"GL_AMD_pinned_memory")
            && self
                .create_pinned(vk, size)
                .inspect_err(|e| crate::warn_once!("Can't pin memory for OpenGL frames: {}", e))
                .is_ok();
        let imported = pinned
            && self
                .import(vk)
                .inspect_err(|e| {
                    crate::warn_once!("Copying OpenGL frames on the CPU: {}", e);
                    self.free_vk(&vk.device);
                })
                .is_ok();
        if !pinned {
            self.create_persistent(size)?;
        }
        if !imported {
            self.allocate_copy(vk, size)?;
        }

        let device = &vk.device;
        unsafe {
            self.cmd = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(|e| format!("Failed to allocate command buffer: {e}"))?[0];
            self.fence = device
                .create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
                .map_err(|e| format!("Failed to create fence: {e}"))?;
        }
        Ok(())
    }

    /// A pixel buffer in memory of ours, which the runtime's device can import.
    fn create_pinned(&mut self, vk: &VulkanData, size: vk::DeviceSize) -> Result<(), String> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as vk::DeviceSize;
        let alignment = vk.host_import_alignment.unwrap_or(page).max(page);
        let len = align_up(size, alignment) as usize;
        // mmap aligns to pages, which imports need at most.
        if !alignment.is_multiple_of(page) || alignment > page {
            return Err(format!("unsupported import alignment {alignment}"));
        }
        let host = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if host == libc::MAP_FAILED {
            return Err("mmap failed".to_owned());
        }
        self.host = Some((host, len));
        self.pbo_mapped = host;
        unsafe {
            let mut bound = 0;
            gl::GetIntegerv(gl::PIXEL_PACK_BUFFER_BINDING, &mut bound);
            gl::GenBuffers(1, &mut self.pbo);
            gl::BindBuffer(EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, self.pbo);
            gl::BufferData(
                EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD,
                len as _,
                host,
                gl::STREAM_READ,
            );
            // A buffer that couldn't be pinned has no storage.
            let mut stored = 0;
            gl::GetBufferParameteriv(
                EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD,
                gl::BUFFER_SIZE,
                &mut stored,
            );
            gl::BindBuffer(EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, 0);
            gl::BindBuffer(gl::PIXEL_PACK_BUFFER, bound as GLuint);
            if stored as usize != len {
                self.free_gl();
                return Err("the driver didn't pin it".to_owned());
            }
        }
        Ok(())
    }

    /// A pixel buffer mapped for as long as it exists.
    fn create_persistent(&mut self, size: vk::DeviceSize) -> Result<(), String> {
        if !gl::BufferStorage::is_loaded() {
            return Err("glBufferStorage is missing".to_owned());
        }
        unsafe {
            let mut bound = 0;
            gl::GetIntegerv(gl::PIXEL_PACK_BUFFER_BINDING, &mut bound);
            gl::GenBuffers(1, &mut self.pbo);
            gl::BindBuffer(gl::PIXEL_PACK_BUFFER, self.pbo);
            let access = gl::MAP_READ_BIT | gl::MAP_PERSISTENT_BIT | gl::MAP_COHERENT_BIT;
            // Client storage asks for cached system memory, which the CPU copies from.
            gl::BufferStorage(
                gl::PIXEL_PACK_BUFFER,
                size as _,
                ptr::null(),
                access | gl::CLIENT_STORAGE_BIT,
            );
            self.pbo_mapped = gl::MapBufferRange(gl::PIXEL_PACK_BUFFER, 0, size as _, access);
            gl::BindBuffer(gl::PIXEL_PACK_BUFFER, bound as GLuint);
        }
        if self.pbo_mapped.is_null() {
            return Err("Failed to map pixel buffer".to_owned());
        }
        Ok(())
    }

    fn create_buffer(
        &mut self,
        device: &ash::Device,
        size: vk::DeviceSize,
        external: bool,
    ) -> Result<vk::MemoryRequirements, String> {
        let mut external_info = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
        let mut info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if external {
            info = info.push_next(&mut external_info);
        }
        unsafe {
            self.buffer = device
                .create_buffer(&info, None)
                .map_err(|e| format!("Failed to create staging buffer: {e}"))?;
            Ok(device.get_buffer_memory_requirements(self.buffer))
        }
    }

    /// Makes `buffer` use the pixel buffer's memory.
    fn import(&mut self, vk: &VulkanData) -> Result<(), String> {
        let device = &vk.device;
        let Some((host, len)) = self.host else {
            return Err("no memory of ours".to_owned());
        };
        let size = len as vk::DeviceSize;
        let handle_type = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
        let mut host_props = vk::MemoryHostPointerPropertiesEXT::default();
        unsafe {
            (ash::ext::external_memory_host::Device::new(&vk.instance, device)
                .fp()
                .get_memory_host_pointer_properties_ext)(
                device.handle(),
                handle_type,
                host,
                &mut host_props,
            )
            .result()
        }
        .map_err(|e| format!("Can't import the pixel buffer: {e}"))?;
        let requirements = self.create_buffer(device, size, true)?;
        let types = requirements.memory_type_bits & host_props.memory_type_bits;
        if types == 0 || requirements.size > size {
            return Err("No memory type to import the pixel buffer with".to_owned());
        }
        let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(handle_type)
            .host_pointer(host);
        unsafe {
            self.memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(size)
                        .memory_type_index(types.trailing_zeros())
                        .push_next(&mut import),
                    None,
                )
                .map_err(|e| format!("Failed to import the pixel buffer: {e}"))?;
            device
                .bind_buffer_memory(self.buffer, self.memory, 0)
                .map_err(|e| format!("Failed to bind imported memory: {e}"))
        }
    }

    /// Makes `buffer` use host memory of its own, which reads are copied into.
    fn allocate_copy(&mut self, vk: &VulkanData, size: vk::DeviceSize) -> Result<(), String> {
        let device = &vk.device;
        let requirements = self.create_buffer(device, size, false)?;
        let properties = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let usable = |device_local: bool| {
            properties
                .memory_types_as_slice()
                .iter()
                .enumerate()
                .position(|(i, ty)| {
                    requirements.memory_type_bits & (1 << i) != 0
                        && ty.property_flags.contains(wanted)
                        && ty
                            .property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                            == device_local
                })
        };
        // System memory over mapped VRAM, which the CPU writes into across the bus.
        let memory_type = usable(false)
            .or_else(|| usable(true))
            .ok_or_else(|| "No host visible memory for staging".to_owned())?;
        unsafe {
            self.memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type as u32),
                    None,
                )
                .map_err(|e| format!("Failed to allocate staging memory: {e}"))?;
            device
                .bind_buffer_memory(self.buffer, self.memory, 0)
                .map_err(|e| format!("Failed to bind staging memory: {e}"))?;
            self.copy_to = Some(
                device
                    .map_memory(self.memory, 0, size, vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("Failed to map staging memory: {e}"))?,
            );
        }
        Ok(())
    }

    fn free_vk(&mut self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
        self.buffer = vk::Buffer::null();
        self.memory = vk::DeviceMemory::null();
        self.copy_to = None;
    }

    fn wait(&self, device: &ash::Device) -> bool {
        let result = unsafe { device.wait_for_fences(&[self.fence], true, COPY_TIMEOUT_NS) };
        if let Err(e) = result {
            crate::warn_once!("Failed to wait for an upload to the runtime's GPU: {}", e);
        }
        result.is_ok()
    }

    /// Drops the pending read, which needs the GL context.
    fn discard(&mut self) {
        if let Some(pending) = self.pending.take() {
            unsafe { gl::DeleteSync(pending.sync) };
        }
    }

    fn free_gl(&mut self) {
        // Deleting a mapped buffer unmaps it.
        unsafe { gl::DeleteBuffers(1, &self.pbo) };
        self.pbo = 0;
        self.pbo_mapped = ptr::null_mut();
    }

    /// The slot must not be in use by the runtime's device anymore. GL objects, and the memory
    /// they use, are only freed if `gl_current`. The memory Vulkan imported is freed first.
    fn destroy(mut self, device: &ash::Device, pool: vk::CommandPool, gl_current: bool) {
        unsafe {
            device.destroy_fence(self.fence, None);
            if self.cmd != vk::CommandBuffer::null() {
                device.free_command_buffers(pool, &[self.cmd]);
            }
        }
        self.free_vk(device);
        if gl_current {
            self.discard();
            self.free_gl();
            if let Some((host, len)) = self.host.take() {
                unsafe { libc::munmap(host, len) };
            }
        }
    }
}

/// The current swapchain's images, our textures standing in for them and the slots to upload
/// their layers with.
#[derive(Default)]
struct Swapchain {
    images: Vec<vk::Image>,
    textures: Vec<GLuint>,
    target: GLenum,
    extent: xr::Extent2Di,
    format: Option<Format>,
    slots: Vec<Mutex<Slot>>,
}

/// What [GraphicsBackend::store_swapchain_images] needs from the create info.
#[derive(Copy, Clone)]
struct Created {
    width: u32,
    height: u32,
    array_size: u32,
}

pub struct GlBridgeData {
    gl: GlData,
    device: Arc<Device>,
    pool: vk::CommandPool,
    /// From [GraphicsBackend::swapchain_created], for the images stored next.
    created: Option<Created>,
    swapchain: Swapchain,
}

impl GlBridgeData {
    pub(crate) fn new(mut gl: GlData, instance: &xr::Instance) -> Option<Self> {
        gl.flip = true;
        let device = shared_device(instance)
            .inspect_err(|e| log::error!("Can't copy OpenGL frames to the runtime's GPU: {e}"))
            .ok()?;
        let pool = unsafe {
            device.vk.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.vk.queue_family_index),
                None,
            )
        }
        .inspect_err(|e| log::error!("Failed to create command pool: {e}"))
        .ok()?;
        Some(Self {
            gl,
            device,
            pool,
            created: None,
            swapchain: Default::default(),
        })
    }

    /// Waits for the uploads and frees the swapchain's resources.
    fn destroy_swapchain(&mut self) {
        let device = &self.device.vk.device;
        let swapchain = std::mem::take(&mut self.swapchain);
        // GL objects aren't shared with other contexts, in another one these could be the app's.
        let gl_current = self.gl.session_info.is_current();
        for slot in swapchain.slots {
            let slot = slot.into_inner().unwrap_or_else(PoisonError::into_inner);
            // A slot still in use is leaked rather than freed under the GPU.
            if slot.wait(device) {
                slot.destroy(device, self.pool, gl_current);
            }
        }
        if !swapchain.textures.is_empty() && gl_current {
            unsafe {
                gl::DeleteTextures(swapchain.textures.len() as _, swapchain.textures.as_ptr())
            };
        }
    }

    /// Creates our textures with `info`'s size and layers, keeping the app's bindings.
    fn create_textures(
        count: usize,
        target: GLenum,
        format: GLenum,
        info: &xr::Extent2Di,
        layers: u32,
    ) -> Vec<GLuint> {
        let mut textures = vec![0; count];
        unsafe {
            let binding = if target == gl::TEXTURE_2D_ARRAY {
                gl::TEXTURE_BINDING_2D_ARRAY
            } else {
                gl::TEXTURE_BINDING_2D
            };
            let mut bound = 0;
            gl::GetIntegerv(binding, &mut bound);
            gl::GenTextures(count as _, textures.as_mut_ptr());
            for &texture in &textures {
                gl::BindTexture(target, texture);
                if target == gl::TEXTURE_2D_ARRAY {
                    gl::TexStorage3D(target, 1, format, info.width, info.height, layers as _);
                } else {
                    gl::TexStorage2D(target, 1, format, info.width, info.height);
                }
            }
            gl::BindTexture(target, bound as GLuint);
        }
        textures
    }

    /// Starts reading the top-left `extent` of layer `layer` of image `image_index`, which
    /// [GlData] just copied into. Once every layer is read, they're uploaded together, so the app
    /// renders one eye while the other is read.
    fn upload(&self, image_index: usize, layer: usize, extent: xr::Extent2Di) {
        let swapchain = &self.swapchain;
        let Some(format) = swapchain.format else {
            return;
        };
        let (Some(&image), Some(slot)) = (
            swapchain.images.get(image_index),
            swapchain.slots.get(layer),
        ) else {
            return;
        };
        let extent = xr::Extent2Di {
            width: extent.width.clamp(0, swapchain.extent.width),
            height: extent.height.clamp(0, swapchain.extent.height),
        };
        if extent.width == 0 || extent.height == 0 {
            return;
        }

        // Reads for another image are left over from a frame that didn't submit every layer,
        // and that image may be the runtime's again.
        for other in &swapchain.slots {
            let mut other = other.lock().unwrap_or_else(PoisonError::into_inner);
            if other.pending.as_ref().is_some_and(|p| p.image != image) {
                other.discard();
            }
        }

        {
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.pending.is_some() {
                self.finish(&mut slot, format);
            }
            // The last upload from this slot has to be done before its memory is written again.
            if !slot.wait(&self.device.vk.device) {
                return;
            }
            unsafe {
                self.gl.read_image(
                    image_index,
                    swapchain.target,
                    layer as GLint,
                    extent,
                    format.read,
                    slot.pbo,
                );
                let sync = gl::FenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0);
                gl::Flush();
                slot.pending = Some(Pending {
                    sync,
                    image,
                    layer: layer as u32,
                    extent,
                });
            }
        }

        let ready = swapchain.slots.iter().all(|slot| {
            slot.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pending
                .is_some()
        });
        if ready {
            self.finish_all();
        }
    }

    fn finish_all(&self) {
        let Some(format) = self.swapchain.format else {
            return;
        };
        for slot in &self.swapchain.slots {
            self.finish(
                &mut slot.lock().unwrap_or_else(PoisonError::into_inner),
                format,
            );
        }
    }

    /// Waits for the slot's pending read and uploads it.
    fn finish(&self, slot: &mut Slot, format: Format) {
        let Some(pending) = slot.pending.take() else {
            return;
        };
        let status = unsafe {
            let status =
                gl::ClientWaitSync(pending.sync, gl::SYNC_FLUSH_COMMANDS_BIT, COPY_TIMEOUT_NS);
            gl::DeleteSync(pending.sync);
            status
        };
        if status != gl::ALREADY_SIGNALED && status != gl::CONDITION_SATISFIED {
            crate::warn_once!(
                "Reading an OpenGL frame failed or timed out ({:#x})",
                status
            );
            return;
        }

        let extent = pending.extent;
        if let Some(copy_to) = slot.copy_to {
            let size = (extent.width as u32 * extent.height as u32 * format.size) as usize;
            unsafe { ptr::copy_nonoverlapping(slot.pbo_mapped.cast::<u8>(), copy_to.cast(), size) };
        }

        let device = &self.device.vk.device;
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: pending.layer,
            layer_count: 1,
        };
        let barrier = |src_access, dst_access, old_layout, new_layout| vk::ImageMemoryBarrier {
            src_access_mask: src_access,
            dst_access_mask: dst_access,
            old_layout,
            new_layout,
            src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            image: pending.image,
            subresource_range: range,
            ..Default::default()
        };
        let region = vk::BufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: pending.layer,
                layer_count: 1,
            },
            image_offset: vk::Offset3D::default(),
            image_extent: vk::Extent3D {
                width: extent.width as u32,
                height: extent.height as u32,
                depth: 1,
            },
        };

        let recorded = unsafe {
            device
                .reset_command_buffer(slot.cmd, vk::CommandBufferResetFlags::empty())
                .and_then(|()| {
                    device.begin_command_buffer(
                        slot.cmd,
                        &vk::CommandBufferBeginInfo::default()
                            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )
                })
                .and_then(|()| {
                    device.cmd_pipeline_barrier(
                        slot.cmd,
                        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier(
                            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        )],
                    );
                    device.cmd_copy_buffer_to_image(
                        slot.cmd,
                        slot.buffer,
                        pending.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    device.cmd_pipeline_barrier(
                        slot.cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier(
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::empty(),
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        )],
                    );
                    device.end_command_buffer(slot.cmd)
                })
                .and_then(|()| device.reset_fences(&[slot.fence]))
        };
        if let Err(e) = recorded {
            crate::warn_once!("Failed to record an upload to the runtime's GPU: {}", e);
            return;
        }

        // Host writes before the submit are visible to it, the memory is coherent.
        let _queue = self
            .device
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&slot.cmd));
        if let Err(e) = unsafe { device.queue_submit(self.device.vk.queue, &[submit], slot.fence) }
        {
            crate::warn_once!("Failed to submit an upload to the runtime's GPU: {}", e);
            // An empty submit signals the fence after earlier work, so the next wait ends.
            let _ = unsafe { device.queue_submit(self.device.vk.queue, &[], slot.fence) };
        }
    }
}

impl Drop for GlBridgeData {
    fn drop(&mut self) {
        self.destroy_swapchain();
        unsafe { self.device.vk.device.destroy_command_pool(self.pool, None) };
    }
}

impl GraphicsBackend for GlBridgeData {
    type Api = GlBridgeApi;
    type OpenVrTexture = GlTexture;
    type Format = u32;
    type NiceFormat = vk::Format;

    fn from_openxr_format(format: u32) -> Self::Format {
        format
    }

    fn to_openxr_format(format: Self::Format) -> u32 {
        format
    }

    fn to_nice_format(format: Self::Format) -> Self::NiceFormat {
        vk::Format::from_raw(format as _)
    }

    fn session_create_info(&self) -> xr::vulkan::SessionCreateInfo {
        self.device.vk.session_create_info()
    }

    fn fallback_format(format: u32, offered: &[u32]) -> Option<u32> {
        // GlData's copies convert into any of these.
        crate::graphics_backends::vulkan::fallback_formats(vk::Format::from_raw(format as _))
            .iter()
            .filter(|format| FORMATS.iter().any(|f| f.vk == **format))
            .map(|format| format.as_raw() as u32)
            .find(|format| offered.contains(format))
    }

    fn get_texture(texture: &vr::Texture_t, flags: vr::EVRSubmitFlags) -> Option<GlTexture> {
        GlData::get_texture(texture, flags)
    }

    fn array_layer(&self, texture: GlTexture, index: u32) -> Option<GlTexture> {
        self.gl.array_layer(texture, index)
    }

    fn raw_swapchain_info_for_texture(
        &self,
        texture: GlTexture,
        bounds: vr::VRTextureBounds_t,
        color_space: vr::EColorSpace,
    ) -> xr::SwapchainCreateInfo<GlBridgeApi> {
        let info = self
            .gl
            .raw_swapchain_info_for_texture(texture, bounds, color_space);
        // Other formats are converted by GlData's copies.
        let format = FORMATS
            .iter()
            .find(|f| f.gl == info.format)
            .unwrap_or(&FORMATS[2]);
        xr::SwapchainCreateInfo {
            create_flags: info.create_flags,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                | xr::SwapchainUsageFlags::TRANSFER_DST,
            format: format.vk.as_raw() as u32,
            sample_count: info.sample_count,
            width: info.width,
            height: info.height,
            face_count: info.face_count,
            array_size: info.array_size,
            mip_count: info.mip_count,
        }
    }

    fn swapchain_created(&mut self, info: &xr::SwapchainCreateInfo<GlBridgeApi>) {
        self.created = Some(Created {
            width: info.width,
            height: info.height,
            array_size: info.array_size,
        });
    }

    fn store_swapchain_images(&mut self, images: Vec<u64>, format: Self::Format) {
        self.destroy_swapchain();
        let Some(info) = self.created.take() else {
            crate::warn_once!("Swapchain images stored without their create info");
            return;
        };
        let Some(&format) = FORMATS.iter().find(|f| f.vk.as_raw() as u32 == format) else {
            crate::warn_once!(
                "Can't copy OpenGL frames into swapchains of format {:?}",
                vk::Format::from_raw(format as _)
            );
            return;
        };

        let extent = xr::Extent2Di {
            width: info.width as i32,
            height: info.height as i32,
        };
        let layers = info.array_size.max(1);
        // Overlay swapchains have one layer, which GlData expects as a 2D texture like runtimes
        // allocate them.
        let target = if layers > 1 {
            gl::TEXTURE_2D_ARRAY
        } else {
            gl::TEXTURE_2D
        };
        let size = vk::DeviceSize::from(info.width)
            * vk::DeviceSize::from(info.height)
            * vk::DeviceSize::from(format.size);
        let slots: Result<Vec<_>, String> = (0..layers)
            .map(|_| Slot::new(&self.device.vk, self.pool, size).map(Mutex::new))
            .collect();
        let slots = match slots {
            Ok(slots) => slots,
            Err(e) => {
                crate::warn_once!("Can't copy OpenGL frames to the runtime's GPU: {}", e);
                return;
            }
        };

        let textures = Self::create_textures(images.len(), target, format.gl, &extent, layers);
        self.gl.store_swapchain_images(textures.clone(), format.gl);
        self.swapchain = Swapchain {
            images: images.into_iter().map(vk::Image::from_raw).collect(),
            textures,
            target,
            extent,
            format: Some(format),
            slots,
        };
    }

    fn copy_texture_to_swapchain(
        &self,
        eye: vr::EVREye,
        texture: GlTexture,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
        submit_flags: vr::EVRSubmitFlags,
    ) -> xr::Extent2Di {
        if self.swapchain.format.is_none() {
            return Default::default();
        }
        let extent = self.gl.copy_texture_to_swapchain(
            eye,
            texture,
            color_space,
            bounds,
            image_index,
            submit_flags,
        );
        self.upload(image_index, usize::from(eye == vr::EVREye::Right), extent);
        extent
    }

    fn copy_overlay_to_swapchain(
        &mut self,
        texture: GlTexture,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
    ) -> xr::Extent2Di {
        if self.swapchain.format.is_none() {
            return Default::default();
        }
        let extent = self
            .gl
            .copy_overlay_to_swapchain(texture, color_space, bounds, image_index);
        self.upload(image_index, 0, extent);
        extent
    }

    fn wait_for_copies(&self) {
        if self.gl.session_info.is_current() {
            self.finish_all();
        }
        for slot in &self.swapchain.slots {
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            // Without the context the read can't be finished, or its sync deleted.
            slot.pending = None;
            slot.wait(&self.device.vk.device);
        }
    }
}
