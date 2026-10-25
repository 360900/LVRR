//! A fake Vulkan device that checks how the backend uses its objects. Submitted work stays pending
//! until the backend waits for its fence, and nothing a pending submission uses may be destroyed,
//! updated or re-recorded in the meantime. Violations are collected rather than panicking, since
//! panics can't unwind out of the Vulkan entry points.
#![allow(non_snake_case)]

use super::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_void};

#[derive(Default)]
struct Gpu {
    next_handle: u64,
    live: HashSet<u64>,
    signaled: HashSet<u64>,
    /// Command buffers and descriptor sets allocated from each pool.
    children: HashMap<u64, Vec<u64>>,
    /// Image views a descriptor set or framebuffer refers to.
    refs: HashMap<u64, Vec<u64>>,
    /// Objects used by each command buffer's last recording.
    uses: HashMap<u64, HashSet<u64>>,
    /// Command buffers submitted with each fence, until the backend waits for it.
    pending: HashMap<u64, Vec<u64>>,
    errors: Vec<String>,
    barriers: Vec<vk::ImageMemoryBarrier<'static>>,
    /// Width, height and attachment count of each framebuffer.
    framebuffers: Vec<(u32, u32, usize)>,
    render_areas: Vec<vk::Rect2D>,
    scissors: Vec<vk::Rect2D>,
    descriptor_layouts: Vec<vk::ImageLayout>,
    pipeline_samples: Vec<vk::SampleCountFlags>,
    bound_pipelines: Vec<vk::Pipeline>,
    push_constants: Vec<(vk::ShaderStageFlags, u32, Vec<u8>)>,
    draws: usize,
    clears: usize,
    /// Source and destination image and region of each recorded transfer.
    copies: Vec<(u64, u64, vk::ImageCopy)>,
    resolves: Vec<(u64, u64, vk::ImageResolve)>,
    blits: Vec<(u64, u64, vk::ImageBlit)>,
    /// The format of each image the backend created.
    image_formats: HashMap<u64, vk::Format>,
    /// Pending work never finishes, so waits for it time out.
    hung: bool,
    /// Fails the next submission of command buffers.
    fail_submit: bool,
}

thread_local! {
    static GPU: RefCell<Gpu> = RefCell::default();
}

fn gpu<T>(f: impl FnOnce(&mut Gpu) -> T) -> T {
    GPU.with_borrow_mut(f)
}

macro_rules! check {
    ($gpu:expr, $cond:expr, $($msg:tt)*) => {
        if !$cond {
            $gpu.errors.push(format!($($msg)*));
        }
    };
}

impl Gpu {
    fn create(&mut self) -> u64 {
        self.next_handle += 1;
        self.live.insert(self.next_handle);
        self.next_handle
    }

    fn allocate(&mut self, pool: u64) -> u64 {
        let handle = self.create();
        self.children.entry(pool).or_default().push(handle);
        handle
    }

    /// Fences, command buffers and the objects they use that the GPU may still access.
    fn in_flight(&self) -> HashSet<u64> {
        let bufs = self.pending.values().flatten();
        let used = bufs
            .clone()
            .flat_map(|buf| self.uses.get(buf).into_iter().flatten());
        let referenced = used
            .clone()
            .flat_map(|obj| self.refs.get(obj).into_iter().flatten());
        let pending = self.pending.keys().chain(bufs);
        pending.chain(used).chain(referenced).copied().collect()
    }

    fn destroy(&mut self, handle: u64) {
        if handle == 0 {
            return;
        }
        for child in self.children.remove(&handle).unwrap_or_default() {
            self.destroy(child);
        }
        let in_use = self.in_flight().contains(&handle);
        check!(
            self,
            !in_use,
            "destroyed {handle:#x} while the GPU may use it"
        );
        let known = self.live.remove(&handle);
        check!(self, known, "destroyed unknown {handle:#x}");
        self.refs.remove(&handle);
    }

    fn record(&mut self, buf: vk::CommandBuffer, obj: impl Handle) {
        let uses = self.uses.entry(buf.as_raw()).or_default();
        uses.insert(obj.as_raw());
    }
}

unsafe fn slice<'a, T>(ptr: *const T, len: u32) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len as usize) }
    }
}

macro_rules! create_destroy {
    ($($create:ident, $destroy:ident: $info:ident -> $handle:ident;)*) => {$(
        unsafe extern "system" fn $create(
            _: vk::Device,
            _: *const vk::$info<'_>,
            _: *const vk::AllocationCallbacks<'_>,
            out: *mut vk::$handle,
        ) -> vk::Result {
            unsafe { *out = vk::$handle::from_raw(gpu(Gpu::create)) };
            vk::Result::SUCCESS
        }

        unsafe extern "system" fn $destroy(
            _: vk::Device,
            handle: vk::$handle,
            _: *const vk::AllocationCallbacks<'_>,
        ) {
            gpu(|gpu| gpu.destroy(handle.as_raw()));
        }
    )*};
}

create_destroy! {
    vkCreateCommandPool, vkDestroyCommandPool: CommandPoolCreateInfo -> CommandPool;
    vkCreateRenderPass, vkDestroyRenderPass: RenderPassCreateInfo -> RenderPass;
    vkCreateShaderModule, vkDestroyShaderModule: ShaderModuleCreateInfo -> ShaderModule;
    vkCreateDescriptorSetLayout, vkDestroyDescriptorSetLayout:
        DescriptorSetLayoutCreateInfo -> DescriptorSetLayout;
    vkCreateDescriptorPool, vkDestroyDescriptorPool: DescriptorPoolCreateInfo -> DescriptorPool;
    vkCreatePipelineLayout, vkDestroyPipelineLayout: PipelineLayoutCreateInfo -> PipelineLayout;
    vkCreateImageView, vkDestroyImageView: ImageViewCreateInfo -> ImageView;
    vkCreateSampler, vkDestroySampler: SamplerCreateInfo -> Sampler;
    vkAllocateMemory, vkFreeMemory: MemoryAllocateInfo -> DeviceMemory;
}

unsafe extern "system" fn vkCreateImage(
    _: vk::Device,
    info: *const vk::ImageCreateInfo<'_>,
    _: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Image,
) -> vk::Result {
    let format = unsafe { (*info).format };
    gpu(|gpu| {
        let image = gpu.create();
        gpu.image_formats.insert(image, format);
        unsafe { *out = vk::Image::from_raw(image) };
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkDestroyImage(
    _: vk::Device,
    image: vk::Image,
    _: *const vk::AllocationCallbacks<'_>,
) {
    gpu(|gpu| gpu.destroy(image.as_raw()));
}

unsafe extern "system" fn vkGetImageMemoryRequirements(
    _: vk::Device,
    _: vk::Image,
    out: *mut vk::MemoryRequirements,
) {
    unsafe {
        *out = vk::MemoryRequirements {
            size: 1024,
            alignment: 1,
            memory_type_bits: 0b11,
        }
    };
}

unsafe extern "system" fn vkBindImageMemory(
    _: vk::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    _: vk::DeviceSize,
) -> vk::Result {
    // The memory is in use as long as the image is.
    gpu(|gpu| gpu.refs.insert(image.as_raw(), vec![memory.as_raw()]));
    vk::Result::SUCCESS
}

/// A host visible and a device local memory type.
unsafe extern "system" fn vkGetPhysicalDeviceMemoryProperties(
    _: vk::PhysicalDevice,
    out: *mut vk::PhysicalDeviceMemoryProperties,
) {
    let mut properties = vk::PhysicalDeviceMemoryProperties {
        memory_type_count: 2,
        memory_heap_count: 1,
        ..Default::default()
    };
    properties.memory_types[0].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE;
    properties.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    unsafe { *out = properties };
}

unsafe extern "system" fn vkAllocateCommandBuffers(
    _: vk::Device,
    info: *const vk::CommandBufferAllocateInfo<'_>,
    out: *mut vk::CommandBuffer,
) -> vk::Result {
    let info = unsafe { &*info };
    gpu(|gpu| {
        for i in 0..info.command_buffer_count as usize {
            let buf = gpu.allocate(info.command_pool.as_raw());
            unsafe { *out.add(i) = vk::CommandBuffer::from_raw(buf) };
        }
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkAllocateDescriptorSets(
    _: vk::Device,
    info: *const vk::DescriptorSetAllocateInfo<'_>,
    out: *mut vk::DescriptorSet,
) -> vk::Result {
    let info = unsafe { &*info };
    gpu(|gpu| {
        for i in 0..info.descriptor_set_count as usize {
            let set = gpu.allocate(info.descriptor_pool.as_raw());
            unsafe { *out.add(i) = vk::DescriptorSet::from_raw(set) };
        }
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkUpdateDescriptorSets(
    _: vk::Device,
    count: u32,
    writes: *const vk::WriteDescriptorSet<'_>,
    _: u32,
    _: *const vk::CopyDescriptorSet<'_>,
) {
    gpu(|gpu| {
        for write in unsafe { slice(writes, count) } {
            let set = write.dst_set.as_raw();
            let in_use = gpu.in_flight().contains(&set);
            check!(
                gpu,
                !in_use,
                "updated set {set:#x} while the GPU may use it"
            );
            let images = unsafe { slice(write.p_image_info, write.descriptor_count) };
            let views = images.iter().map(|i| i.image_view.as_raw()).collect();
            gpu.refs.insert(set, views);
            let layouts = images.iter().map(|i| i.image_layout);
            gpu.descriptor_layouts.extend(layouts);
        }
    });
}

unsafe extern "system" fn vkCreateFramebuffer(
    _: vk::Device,
    info: *const vk::FramebufferCreateInfo<'_>,
    _: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Framebuffer,
) -> vk::Result {
    let info = unsafe { &*info };
    let attachments = unsafe { slice(info.p_attachments, info.attachment_count) };
    gpu(|gpu| {
        let framebuffer = gpu.create();
        let views = attachments.iter().map(|a| a.as_raw()).collect();
        gpu.refs.insert(framebuffer, views);
        let size = (info.width, info.height, attachments.len());
        gpu.framebuffers.push(size);
        unsafe { *out = vk::Framebuffer::from_raw(framebuffer) };
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkDestroyFramebuffer(
    _: vk::Device,
    framebuffer: vk::Framebuffer,
    _: *const vk::AllocationCallbacks<'_>,
) {
    gpu(|gpu| gpu.destroy(framebuffer.as_raw()));
}

unsafe extern "system" fn vkDeviceWaitIdle(_: vk::Device) -> vk::Result {
    gpu(|gpu| gpu.errors.push("waited for the app's whole device".into()));
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkDestroyDevice(_: vk::Device, _: *const vk::AllocationCallbacks<'_>) {
    gpu(|gpu| gpu.errors.push("destroyed the app's device".into()));
}

unsafe extern "system" fn vkCreateGraphicsPipelines(
    _: vk::Device,
    _: vk::PipelineCache,
    count: u32,
    infos: *const vk::GraphicsPipelineCreateInfo<'_>,
    _: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Pipeline,
) -> vk::Result {
    for (i, info) in unsafe { slice(infos, count) }.iter().enumerate() {
        let samples = unsafe { &*info.p_multisample_state }.rasterization_samples;
        gpu(|gpu| {
            gpu.pipeline_samples.push(samples);
            unsafe { *out.add(i) = vk::Pipeline::from_raw(gpu.create()) };
        });
    }
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkDestroyPipeline(
    _: vk::Device,
    pipeline: vk::Pipeline,
    _: *const vk::AllocationCallbacks<'_>,
) {
    gpu(|gpu| gpu.destroy(pipeline.as_raw()));
}

unsafe extern "system" fn vkCreateFence(
    _: vk::Device,
    info: *const vk::FenceCreateInfo<'_>,
    _: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Fence,
) -> vk::Result {
    let flags = unsafe { &*info }.flags;
    gpu(|gpu| {
        let fence = gpu.create();
        if flags.contains(vk::FenceCreateFlags::SIGNALED) {
            gpu.signaled.insert(fence);
        }
        unsafe { *out = vk::Fence::from_raw(fence) };
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkDestroyFence(
    _: vk::Device,
    fence: vk::Fence,
    _: *const vk::AllocationCallbacks<'_>,
) {
    gpu(|gpu| gpu.destroy(fence.as_raw()));
}

/// Finishes the work submitted with the fences.
unsafe extern "system" fn vkWaitForFences(
    _: vk::Device,
    count: u32,
    fences: *const vk::Fence,
    _: vk::Bool32,
    _: u64,
) -> vk::Result {
    let fences = unsafe { slice(fences, count) };
    gpu(|gpu| {
        if gpu.hung && fences.iter().any(|f| gpu.pending.contains_key(&f.as_raw())) {
            return vk::Result::TIMEOUT;
        }
        for fence in fences {
            let fence = fence.as_raw();
            let finished = gpu.pending.remove(&fence).is_some() || gpu.signaled.contains(&fence);
            check!(
                gpu,
                finished,
                "waited for fence {fence:#x} that never signals"
            );
            gpu.signaled.insert(fence);
        }
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn vkResetFences(
    _: vk::Device,
    count: u32,
    fences: *const vk::Fence,
) -> vk::Result {
    gpu(|gpu| {
        for fence in unsafe { slice(fences, count) } {
            let pending = gpu.pending.contains_key(&fence.as_raw());
            check!(gpu, !pending, "reset a pending fence");
            gpu.signaled.remove(&fence.as_raw());
        }
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkQueueSubmit(
    _: vk::Queue,
    count: u32,
    submits: *const vk::SubmitInfo<'_>,
    fence: vk::Fence,
) -> vk::Result {
    gpu(|gpu| {
        if count > 0 && std::mem::take(&mut gpu.fail_submit) {
            return vk::Result::ERROR_OUT_OF_DEVICE_MEMORY;
        }
        let in_flight = gpu.in_flight();
        let fence = fence.as_raw();
        let signaled = gpu.signaled.contains(&fence);
        check!(gpu, !signaled, "submitted with a signaled fence");
        // Even an empty submission signals its fence.
        gpu.pending.entry(fence).or_default();
        for submit in unsafe { slice(submits, count) } {
            for buf in unsafe { slice(submit.p_command_buffers, submit.command_buffer_count) } {
                let buf = buf.as_raw();
                check!(
                    gpu,
                    !in_flight.contains(&buf),
                    "submitted a pending command buffer"
                );
                // Without a fence, the work never becomes waitable.
                gpu.pending.entry(fence).or_default().push(buf);
            }
        }
        vk::Result::SUCCESS
    })
}

unsafe extern "system" fn vkBeginCommandBuffer(
    buf: vk::CommandBuffer,
    _: *const vk::CommandBufferBeginInfo<'_>,
) -> vk::Result {
    gpu(|gpu| {
        let pending = gpu.in_flight().contains(&buf.as_raw());
        check!(gpu, !pending, "re-recorded a pending command buffer");
        gpu.uses.remove(&buf.as_raw());
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkEndCommandBuffer(_: vk::CommandBuffer) -> vk::Result {
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkCmdBindPipeline(
    buf: vk::CommandBuffer,
    _: vk::PipelineBindPoint,
    pipeline: vk::Pipeline,
) {
    gpu(|gpu| {
        gpu.record(buf, pipeline);
        gpu.bound_pipelines.push(pipeline);
    });
}

unsafe extern "system" fn vkCmdSetViewport(
    _: vk::CommandBuffer,
    _: u32,
    _: u32,
    _: *const vk::Viewport,
) {
}

unsafe extern "system" fn vkCmdSetScissor(
    _: vk::CommandBuffer,
    _: u32,
    count: u32,
    scissors: *const vk::Rect2D,
) {
    let scissors = unsafe { slice(scissors, count) };
    gpu(|gpu| gpu.scissors.extend_from_slice(scissors));
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn vkCmdBindDescriptorSets(
    buf: vk::CommandBuffer,
    _: vk::PipelineBindPoint,
    layout: vk::PipelineLayout,
    _: u32,
    count: u32,
    sets: *const vk::DescriptorSet,
    _: u32,
    _: *const u32,
) {
    let sets = unsafe { slice(sets, count) };
    gpu(|gpu| {
        gpu.record(buf, layout);
        for &set in sets {
            gpu.record(buf, set);
        }
    });
}

unsafe extern "system" fn vkCmdPushConstants(
    _: vk::CommandBuffer,
    _: vk::PipelineLayout,
    stages: vk::ShaderStageFlags,
    offset: u32,
    size: u32,
    values: *const c_void,
) {
    let values = unsafe { slice(values.cast::<u8>(), size) }.to_vec();
    gpu(|gpu| gpu.push_constants.push((stages, offset, values)));
}

unsafe extern "system" fn vkCmdBeginRenderPass(
    buf: vk::CommandBuffer,
    info: *const vk::RenderPassBeginInfo<'_>,
    _: vk::SubpassContents,
) {
    let info = unsafe { &*info };
    gpu(|gpu| {
        gpu.record(buf, info.render_pass);
        gpu.record(buf, info.framebuffer);
        gpu.render_areas.push(info.render_area);
    });
}

unsafe extern "system" fn vkCmdDraw(_: vk::CommandBuffer, _: u32, _: u32, _: u32, _: u32) {
    gpu(|gpu| gpu.draws += 1);
}

unsafe extern "system" fn vkCmdEndRenderPass(_: vk::CommandBuffer) {}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn vkCmdPipelineBarrier(
    _: vk::CommandBuffer,
    _: vk::PipelineStageFlags,
    _: vk::PipelineStageFlags,
    _: vk::DependencyFlags,
    _: u32,
    _: *const vk::MemoryBarrier<'_>,
    _: u32,
    _: *const vk::BufferMemoryBarrier<'_>,
    count: u32,
    barriers: *const vk::ImageMemoryBarrier<'_>,
) {
    let barriers = unsafe { slice(barriers, count) };
    let barriers = barriers.iter().map(|b| vk::ImageMemoryBarrier {
        p_next: std::ptr::null(),
        _marker: std::marker::PhantomData,
        ..*b
    });
    gpu(|gpu| gpu.barriers.extend(barriers));
}

unsafe extern "system" fn vkCmdCopyImage(
    buf: vk::CommandBuffer,
    src: vk::Image,
    _: vk::ImageLayout,
    dst: vk::Image,
    _: vk::ImageLayout,
    count: u32,
    regions: *const vk::ImageCopy,
) {
    let regions = unsafe { slice(regions, count) };
    gpu(|gpu| {
        gpu.record(buf, src);
        gpu.record(buf, dst);
        let (src, dst) = (src.as_raw(), dst.as_raw());
        gpu.copies.extend(regions.iter().map(|&r| (src, dst, r)));
    });
}

unsafe extern "system" fn vkCmdResolveImage(
    buf: vk::CommandBuffer,
    src: vk::Image,
    _: vk::ImageLayout,
    dst: vk::Image,
    _: vk::ImageLayout,
    count: u32,
    regions: *const vk::ImageResolve,
) {
    let regions = unsafe { slice(regions, count) };
    gpu(|gpu| {
        gpu.record(buf, src);
        gpu.record(buf, dst);
        let (src, dst) = (src.as_raw(), dst.as_raw());
        gpu.resolves.extend(regions.iter().map(|&r| (src, dst, r)));
    });
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn vkCmdBlitImage(
    buf: vk::CommandBuffer,
    src: vk::Image,
    _: vk::ImageLayout,
    dst: vk::Image,
    _: vk::ImageLayout,
    count: u32,
    regions: *const vk::ImageBlit,
    _: vk::Filter,
) {
    let regions = unsafe { slice(regions, count) };
    gpu(|gpu| {
        gpu.record(buf, src);
        gpu.record(buf, dst);
        let (src, dst) = (src.as_raw(), dst.as_raw());
        gpu.blits.extend(regions.iter().map(|&r| (src, dst, r)));
    });
}

unsafe extern "system" fn vkCmdClearColorImage(
    buf: vk::CommandBuffer,
    image: vk::Image,
    _: vk::ImageLayout,
    _: *const vk::ClearColorValue,
    _: u32,
    _: *const vk::ImageSubresourceRange,
) {
    gpu(|gpu| {
        gpu.record(buf, image);
        gpu.clears += 1;
    });
}

fn proc_addr(name: &CStr) -> *const c_void {
    macro_rules! lookup {
        ($($f:ident),* $(,)?) => {
            match name.to_bytes() {
                $(n if n == stringify!($f).as_bytes() => $f as *const c_void,)*
                _ => std::ptr::null(),
            }
        };
    }
    lookup![
        vkCreateCommandPool,
        vkDestroyCommandPool,
        vkCreateRenderPass,
        vkDestroyRenderPass,
        vkCreateShaderModule,
        vkDestroyShaderModule,
        vkCreateDescriptorSetLayout,
        vkDestroyDescriptorSetLayout,
        vkCreateDescriptorPool,
        vkDestroyDescriptorPool,
        vkCreatePipelineLayout,
        vkDestroyPipelineLayout,
        vkCreateImageView,
        vkDestroyImageView,
        vkCreateSampler,
        vkDestroySampler,
        vkCreateImage,
        vkDestroyImage,
        vkAllocateMemory,
        vkFreeMemory,
        vkGetImageMemoryRequirements,
        vkBindImageMemory,
        vkAllocateCommandBuffers,
        vkAllocateDescriptorSets,
        vkUpdateDescriptorSets,
        vkCreateFramebuffer,
        vkDestroyFramebuffer,
        vkDeviceWaitIdle,
        vkDestroyDevice,
        vkCreateGraphicsPipelines,
        vkDestroyPipeline,
        vkCreateFence,
        vkDestroyFence,
        vkWaitForFences,
        vkResetFences,
        vkQueueSubmit,
        vkBeginCommandBuffer,
        vkEndCommandBuffer,
        vkCmdBindPipeline,
        vkCmdSetViewport,
        vkCmdSetScissor,
        vkCmdBindDescriptorSets,
        vkCmdPushConstants,
        vkCmdBeginRenderPass,
        vkCmdDraw,
        vkCmdEndRenderPass,
        vkCmdPipelineBarrier,
        vkCmdCopyImage,
        vkCmdResolveImage,
        vkCmdBlitImage,
        vkCmdClearColorImage,
    ]
}

/// The app's instance, with what the backend uses of it.
fn instance_proc_addr(name: &CStr) -> *const c_void {
    match name.to_bytes() {
        b"vkGetPhysicalDeviceMemoryProperties" => {
            vkGetPhysicalDeviceMemoryProperties as *const c_void
        }
        _ => std::ptr::null(),
    }
}

/// Handles of objects the app owns.
const APP_HANDLE: u64 = 1 << 32;
const GAME_IMAGE: u64 = APP_HANDLE + 100;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const FULL_BOUNDS: vr::VRTextureBounds_t = vr::VRTextureBounds_t {
    uMin: 0.0,
    uMax: 1.0,
    vMin: 0.0,
    vMax: 1.0,
};

fn fake_vulkan() -> VulkanData {
    gpu(|gpu| *gpu = Gpu::default());
    let entry = new_entry();
    let instance =
        unsafe { ash::Instance::load_with(instance_proc_addr, vk::Instance::from_raw(1)) };
    VulkanData {
        _entry: entry,
        instance,
        physical_device: vk::PhysicalDevice::from_raw(APP_HANDLE),
        device: unsafe { ash::Device::load_with(proc_addr, vk::Device::from_raw(APP_HANDLE)) },
        queue: vk::Queue::from_raw(APP_HANDLE),
        queue_family_index: 0,
        real_data: None,
        owns_device: false,
        host_import_alignment: None,
    }
}

fn store_images(vk: &mut VulkanData, count: u64) {
    let images = (0..count).map(|i| APP_HANDLE + i).collect();
    vk.store_swapchain_images(images, FORMAT.as_raw() as u32);
}

fn texture(width: u32, height: u32) -> vr::VRVulkanTextureData_t {
    vr::VRVulkanTextureData_t {
        m_nImage: GAME_IMAGE,
        m_nWidth: width,
        m_nHeight: height,
        m_nFormat: FORMAT.as_raw() as u32,
        m_nSampleCount: 1,
        ..Default::default()
    }
}

fn copy_eye(vk: &VulkanData, texture: &vr::VRVulkanTextureData_t, image_index: usize) {
    vk.copy_texture_to_swapchain(
        vr::EVREye::Left,
        texture,
        vr::EColorSpace::Auto,
        FULL_BOUNDS,
        image_index,
        vr::EVRSubmitFlags::Default,
    );
}

fn multisampled(texture: vr::VRVulkanTextureData_t) -> vr::VRVulkanTextureData_t {
    vr::VRVulkanTextureData_t {
        m_nSampleCount: 4,
        ..texture
    }
}

/// A gamma encoded UNORM texture, which goes into an sRGB swapchain.
fn unorm(texture: vr::VRVulkanTextureData_t) -> vr::VRVulkanTextureData_t {
    vr::VRVulkanTextureData_t {
        m_nFormat: vk::Format::R8G8B8A8_UNORM.as_raw() as u32,
        ..texture
    }
}

fn offset(x: i32, y: i32, z: i32) -> vk::Offset3D {
    vk::Offset3D { x, y, z }
}

fn copy_overlay(
    vk: &mut VulkanData,
    texture: &vr::VRVulkanTextureData_t,
    bounds: vr::VRTextureBounds_t,
    image_index: usize,
) -> xr::Extent2Di {
    vk.copy_overlay_to_swapchain(texture, vr::EColorSpace::Auto, bounds, image_index)
}

/// Drops the backend and checks that it used and freed everything correctly.
fn finish(vk: VulkanData) {
    drop(vk);
    gpu(|gpu| {
        assert_eq!(gpu.errors, Vec::<String>::new());
        assert!(gpu.live.is_empty(), "leaked {:x?}", gpu.live);
    });
}

#[test]
fn backend_without_images_keeps_app_device() {
    // E.g. the backend of a thread that lost the race to set up the real session.
    finish(fake_vulkan());
}

#[test]
fn eye_copy_waits_for_previous_copy() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let texture = texture(16, 16);
    // The same image coming back while its last copy is still pending must not re-record it.
    copy_eye(&vk, &texture, 0);
    copy_eye(&vk, &texture, 0);
    finish(vk);
}

#[test]
fn eye_copy_barrier_covers_the_copy() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 1);
    copy_eye(&vk, &texture(16, 16), 0);
    gpu(|gpu| {
        let to_transfer = &gpu.barriers[0];
        assert_eq!(
            to_transfer.new_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL
        );
        assert_eq!(to_transfer.dst_access_mask, vk::AccessFlags::TRANSFER_WRITE);
    });
    finish(vk);
}

#[test]
fn swapchain_recreation_waits_for_pending_copies() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let texture = texture(16, 16);
    copy_eye(&vk, &texture, 1);
    store_images(&mut vk, 3);
    copy_eye(&vk, &texture, 2);
    finish(vk);
}

#[test]
fn copies_are_skipped_while_the_gpu_hangs() {
    let texture = texture(16, 16);
    let copies: [fn(&mut VulkanData, &vr::VRVulkanTextureData_t); 2] = [
        |vk, texture| copy_eye(vk, texture, 0),
        |vk, texture| {
            copy_overlay(vk, texture, FULL_BOUNDS, 0);
        },
    ];
    for copy in copies {
        let mut vk = fake_vulkan();
        store_images(&mut vk, 1);
        copy(&mut vk, &texture);
        gpu(|gpu| gpu.hung = true);
        copy(&mut vk, &texture);
        // What the hung copy uses gets leaked rather than freed.
        drop(vk);
        gpu(|gpu| assert_eq!(gpu.errors, Vec::<String>::new()));
    }
}

#[test]
fn failed_submit_does_not_block_the_next_copy() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 1);
    let texture = texture(16, 16);
    gpu(|gpu| gpu.fail_submit = true);
    copy_eye(&vk, &texture, 0);
    copy_eye(&vk, &texture, 0);
    finish(vk);
}

#[test]
fn wait_for_copies_finishes_pending_copies() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    copy_eye(&vk, &texture(16, 16), 1);
    vk.wait_for_copies();
    gpu(|gpu| assert!(gpu.pending.is_empty()));
    finish(vk);
}

#[test]
fn overlay_copy_draws_bounds_at_origin() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 3);
    let bounds = vr::VRTextureBounds_t {
        uMin: 0.5,
        uMax: 1.0,
        vMin: 0.75,
        vMax: 0.25,
    };
    let extent = copy_overlay(&mut vk, &texture(200, 100), bounds, 0);
    assert_eq!((extent.width, extent.height), (100, 50));

    let area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 100,
            height: 50,
        },
    };
    gpu(|gpu| {
        assert_eq!(gpu.render_areas, [area]);
        assert_eq!(gpu.scissors, [area]);
        // The texture is sampled, not used as an attachment.
        assert_eq!(gpu.framebuffers, [(100, 50, 1)]);
        assert_eq!(
            gpu.descriptor_layouts,
            [vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL]
        );
        assert_eq!(gpu.draws, 1);
        let layouts: Vec<_> = gpu
            .barriers
            .iter()
            .filter(|b| b.image.as_raw() == GAME_IMAGE)
            .map(|b| (b.old_layout, b.new_layout))
            .collect();
        assert_eq!(
            layouts,
            [
                (
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                ),
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL
                ),
            ]
        );
    });
    finish(vk);
}

#[test]
fn overlay_copies_keep_resources_until_finished() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 3);
    let texture = texture(64, 32);
    // Overlays can be updated many times per frame, so images come back while their last copy is
    // still pending.
    for i in 0..7 {
        copy_overlay(&mut vk, &texture, FULL_BOUNDS, i % 3);
    }
    // Recreating the swapchain replaces the pipeline.
    store_images(&mut vk, 2);
    copy_overlay(&mut vk, &texture, FULL_BOUNDS, 1);
    finish(vk);
}

#[test]
fn multisampled_overlay_gets_its_own_pipeline() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let multisampled = vr::VRVulkanTextureData_t {
        m_nSampleCount: 4,
        ..texture(16, 16)
    };
    copy_overlay(&mut vk, &texture(16, 16), FULL_BOUNDS, 0);
    copy_overlay(&mut vk, &multisampled, FULL_BOUNDS, 1);
    gpu(|gpu| {
        // Overlay swapchains are single sampled, whatever the texture.
        assert_eq!(gpu.pipeline_samples, [vk::SampleCountFlags::TYPE_1; 2]);
        assert_eq!(gpu.draws, 2);
        let [single, multi] = gpu.bound_pipelines[..] else {
            panic!("bound {:?}", gpu.bound_pipelines);
        };
        assert_ne!(single, multi);
    });
    finish(vk);
}

#[test]
fn overlay_decodes_srgb_only_for_gamma_encoded_unorm() {
    use vk::Format as F;
    use vr::EColorSpace as C;
    for (format, color_space, decode) in [
        (F::R8G8B8A8_UNORM, C::Auto, true),
        (F::B8G8R8A8_UNORM, C::Gamma, true),
        (F::R8G8B8A8_UNORM, C::Linear, false),
        // Sampling these already decodes.
        (F::R8G8B8A8_SRGB, C::Auto, false),
        (F::B8G8R8A8_SRGB, C::Gamma, false),
        // Auto means linear for formats without _SRGB variants.
        (F::R16G16B16A16_SFLOAT, C::Auto, false),
        (F::R16G16B16A16_SFLOAT, C::Gamma, true),
    ] {
        assert_eq!(
            holds_gamma(format, color_space),
            decode,
            "{format:?} {color_space:?}"
        );
    }
}

#[test]
fn overlay_shader_decodes_unorm_textures() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let unorm = vr::VRVulkanTextureData_t {
        m_nFormat: vk::Format::R8G8B8A8_UNORM.as_raw() as u32,
        ..texture(16, 16)
    };
    vk.copy_overlay_to_swapchain(&unorm, vr::EColorSpace::Auto, FULL_BOUNDS, 0);
    vk.copy_overlay_to_swapchain(&texture(16, 16), vr::EColorSpace::Auto, FULL_BOUNDS, 1);
    vk.copy_overlay_to_swapchain(&unorm, vr::EColorSpace::Linear, FULL_BOUNDS, 0);
    gpu(|gpu| {
        let pushed: Vec<_> = gpu
            .push_constants
            .iter()
            .filter(|(stages, ..)| *stages == vk::ShaderStageFlags::FRAGMENT)
            .map(|(_, offset, values)| (*offset, values.clone()))
            .collect();
        let flag = |decode: u32| (16, decode.to_ne_bytes().to_vec());
        assert_eq!(pushed, [flag(1), flag(0), flag(0)]);
    });
    finish(vk);
}

#[test]
fn teardown_after_device_lost() {
    use crate::clientcore::Injector;
    use crate::openxr_data::RealOpenXrData;

    let xr = RealOpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap();
    fakexr::vulkan::set_device_lost(true);
    // Drops the temporary session's VulkanData, which waits for its device.
    drop(xr);
}

#[test]
fn clear_when_copied_area_shrinks() {
    let extent = |width, height| vk::Extent2D { width, height };
    assert!(needs_clear(None, extent(10, 10)));
    assert!(!needs_clear(Some(extent(10, 10)), extent(10, 10)));
    assert!(!needs_clear(Some(extent(10, 10)), extent(20, 10)));
    assert!(needs_clear(Some(extent(10, 10)), extent(9, 10)));
    assert!(needs_clear(Some(extent(10, 10)), extent(20, 9)));
}

/// vkCmdResolveImage needs the same format on both sides.
#[test]
fn resolve_only_into_the_same_format() {
    use vk::Format as F;
    let (unorm, srgb) = (F::R8G8B8A8_UNORM, F::R8G8B8A8_SRGB);
    // Gamma encoded textures get an sRGB swapchain.
    assert_eq!(EyeCopy::new(unorm, true, srgb, true), EyeCopy::Copy);
    assert_eq!(EyeCopy::new(unorm, true, srgb, false), EyeCopy::Copy);
    assert_eq!(EyeCopy::new(srgb, false, srgb, true), EyeCopy::Resolve);
    assert_eq!(EyeCopy::new(unorm, false, unorm, true), EyeCopy::Resolve);
}

/// Copies keep the bytes and blits convert what the source's format reads, so data whose encoding
/// the source format doesn't tell goes through an image that does.
#[test]
fn eye_copies_keep_the_brightness() {
    use vk::Format as F;
    let (unorm, srgb) = (F::R8G8B8A8_UNORM, F::R8G8B8A8_SRGB);
    let (bgra_unorm, bgra_srgb) = (F::B8G8R8A8_UNORM, F::B8G8R8A8_SRGB);
    let float = F::R16G16B16A16_SFLOAT;
    for (texture, gamma, swapchain, expected) in [
        // Gamma encoded UNORM data in its sRGB variant is shown as it is.
        (unorm, true, srgb, EyeCopy::Copy),
        (bgra_unorm, true, bgra_srgb, EyeCopy::Copy),
        // Linear data in its own format.
        (unorm, false, unorm, EyeCopy::Copy),
        (srgb, false, srgb, EyeCopy::Copy),
        (float, false, float, EyeCopy::Copy),
        // Values that are linear as read convert into any substituted format.
        (srgb, false, bgra_srgb, EyeCopy::Blit),
        (srgb, false, float, EyeCopy::Blit),
        (unorm, false, srgb, EyeCopy::Blit),
        (float, false, srgb, EyeCopy::Blit),
        // Gamma encoded data for a linear swapchain is decoded by reading it as sRGB.
        (unorm, true, float, EyeCopy::CopyBlit(srgb)),
        (unorm, true, unorm, EyeCopy::CopyBlit(srgb)),
        (bgra_unorm, true, unorm, EyeCopy::CopyBlit(bgra_srgb)),
        // Gamma encoded data for an sRGB swapchain of another layout keeps its encoded values.
        (unorm, true, bgra_srgb, EyeCopy::BlitCopy(bgra_unorm)),
        (float, true, srgb, EyeCopy::BlitCopy(unorm)),
        (
            F::A2B10G10R10_UNORM_PACK32,
            true,
            bgra_srgb,
            EyeCopy::BlitCopy(bgra_unorm),
        ),
        // Nothing to decode with on either side.
        (float, true, float, EyeCopy::Blit),
    ] {
        assert_eq!(
            EyeCopy::new(texture, gamma, swapchain, false),
            expected,
            "{texture:?} gamma {gamma} into {swapchain:?}"
        );
    }
}

/// What ColorSpace_Auto, Gamma and Linear mean for the texture's values, per the OpenVR header:
/// Auto "assumes 'gamma' for 8-bit per component formats, otherwise 'linear'", Gamma is display
/// native data and Linear has been converted to linear.
#[test]
fn color_space_tells_whether_values_are_gamma_encoded() {
    use vk::Format as F;
    use vr::EColorSpace as C;
    for (format, color_space, gamma) in [
        (F::R8G8B8A8_UNORM, C::Auto, true),
        (F::B8G8R8A8_UNORM, C::Auto, true),
        (F::R8G8B8A8_UNORM, C::Gamma, true),
        (F::R8G8B8A8_UNORM, C::Linear, false),
        (F::B8G8R8A8_UNORM, C::Linear, false),
        // Values that aren't a color space count as Auto.
        (F::R8G8B8A8_UNORM, C(0xdead), true),
        // Reading these decodes, whatever the color space.
        (F::R8G8B8A8_SRGB, C::Auto, false),
        (F::B8G8R8A8_SRGB, C::Gamma, false),
        (F::R8G8B8A8_SRGB, C::Linear, false),
        // Other formats are linear unless the app says otherwise.
        (F::R16G16B16A16_SFLOAT, C::Auto, false),
        (F::R16G16B16A16_SFLOAT, C::Linear, false),
        (F::R16G16B16A16_SFLOAT, C::Gamma, true),
        (F::A2B10G10R10_UNORM_PACK32, C::Auto, false),
        (F::A2B10G10R10_UNORM_PACK32, C::Gamma, true),
    ] {
        assert_eq!(
            holds_gamma_with(format, color_space, false),
            gamma,
            "{format:?} {color_space:?}"
        );
    }
}

/// XRIZER_LINEAR_UNORM turns Auto for 8-bit UNORM textures into linear, nothing else.
#[test]
fn linear_unorm_only_changes_auto() {
    use vk::Format as F;
    use vr::EColorSpace as C;
    assert!(!holds_gamma_with(F::R8G8B8A8_UNORM, C::Auto, true));
    assert!(!holds_gamma_with(F::B8G8R8A8_UNORM, C(0xdead), true));
    assert!(holds_gamma_with(F::R8G8B8A8_UNORM, C::Gamma, true));
    assert!(holds_gamma_with(F::R16G16B16A16_SFLOAT, C::Gamma, true));
    assert!(!holds_gamma_with(F::R8G8B8A8_SRGB, C::Auto, true));
}

#[test]
fn swapchain_formats() {
    use vk::Format as F;
    use vr::EColorSpace as C;
    for (format, color_space, expected) in [
        // Gamma encoded UNORM data goes into the sRGB variant, which shows the bytes as they are.
        (F::R8G8B8A8_UNORM, C::Auto, F::R8G8B8A8_SRGB),
        (F::R8G8B8A8_UNORM, C::Gamma, F::R8G8B8A8_SRGB),
        (F::B8G8R8A8_UNORM, C::Auto, F::B8G8R8A8_SRGB),
        (F::A8B8G8R8_UNORM_PACK32, C::Gamma, F::A8B8G8R8_SRGB_PACK32),
        // Linear data keeps its format, which runtimes treat as linear.
        (F::R8G8B8A8_UNORM, C::Linear, F::R8G8B8A8_UNORM),
        (F::B8G8R8A8_UNORM, C::Linear, F::B8G8R8A8_UNORM),
        (F::R16G16B16A16_SFLOAT, C::Auto, F::R16G16B16A16_SFLOAT),
        (F::R32G32B32A32_SFLOAT, C::Linear, F::R32G32B32A32_SFLOAT),
        (
            F::A2B10G10R10_UNORM_PACK32,
            C::Auto,
            F::A2B10G10R10_UNORM_PACK32,
        ),
        // sRGB data stays sRGB.
        (F::R8G8B8A8_SRGB, C::Auto, F::R8G8B8A8_SRGB),
        (F::B8G8R8A8_SRGB, C::Gamma, F::B8G8R8A8_SRGB),
        (F::R8G8B8A8_SRGB, C::Linear, F::R8G8B8A8_SRGB),
        // Gamma encoded data in formats without an sRGB variant needs an sRGB swapchain too.
        (F::R16G16B16A16_SFLOAT, C::Gamma, F::R8G8B8A8_SRGB),
        (F::A2B10G10R10_UNORM_PACK32, C::Gamma, F::R8G8B8A8_SRGB),
        // Compressed formats can't be rendered to.
        (F::BC3_SRGB_BLOCK, C::Auto, F::R8G8B8A8_SRGB),
        (F::BC7_UNORM_BLOCK, C::Auto, F::R8G8B8A8_SRGB),
        (F::BC1_RGB_UNORM_BLOCK, C::Linear, F::R8G8B8A8_UNORM),
    ] {
        assert_eq!(
            swapchain_format(format, color_space),
            expected,
            "{format:?} {color_space:?}"
        );
    }
}

/// Without the format a swapchain wants, the runtime's closest one keeps the data's encoding.
#[test]
fn fallback_formats_keep_the_encoding() {
    use vk::Format as F;
    let fallback = |format: F, offered: &[F]| {
        let offered: Vec<u32> = offered.iter().map(|f| f.as_raw() as u32).collect();
        VulkanData::fallback_format(format.as_raw() as u32, &offered).map(|f| F::from_raw(f as i32))
    };
    // Monado's order puts 16-bit UNORM first.
    let monado = [
        F::R16G16B16A16_UNORM,
        F::R16G16B16A16_SFLOAT,
        F::R8G8B8A8_SRGB,
        F::B8G8R8A8_SRGB,
        F::R8G8B8A8_UNORM,
        F::B8G8R8A8_UNORM,
    ];
    assert_eq!(
        fallback(F::A8B8G8R8_SRGB_PACK32, &monado),
        Some(F::R8G8B8A8_SRGB)
    );
    assert_eq!(
        fallback(F::A8B8G8R8_UNORM_PACK32, &monado),
        Some(F::R8G8B8A8_UNORM)
    );
    assert_eq!(
        fallback(F::R32G32B32A32_SFLOAT, &monado),
        Some(F::R16G16B16A16_SFLOAT)
    );
    // sRGB data rather goes into a float format than into 8-bit linear storage.
    let no_srgb = [F::R8G8B8A8_UNORM, F::R16G16B16A16_SFLOAT];
    assert_eq!(
        fallback(F::R8G8B8A8_SRGB, &no_srgb),
        Some(F::R16G16B16A16_SFLOAT)
    );
    // Linear data in an sRGB format, which a blit encodes.
    assert_eq!(
        fallback(F::R8G8B8A8_UNORM, &[F::B8G8R8A8_SRGB]),
        Some(F::B8G8R8A8_SRGB)
    );
    // None of ours: the runtime's first.
    assert_eq!(fallback(F::R8G8B8A8_SRGB, &[F::R16G16B16A16_UNORM]), None);
}

/// A multisampled texture in the swapchain's format is resolved straight into the eye's layer.
#[test]
fn multisampled_eye_resolves_into_the_swapchain() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let texture = multisampled(texture(16, 8));
    for eye in [vr::EVREye::Left, vr::EVREye::Right] {
        vk.copy_texture_to_swapchain(
            eye,
            &texture,
            vr::EColorSpace::Auto,
            FULL_BOUNDS,
            1,
            vr::EVRSubmitFlags::Default,
        );
    }
    gpu(|gpu| {
        assert_eq!(gpu.resolves.len(), 2);
        for (eye, &(src, dst, region)) in gpu.resolves.iter().enumerate() {
            assert_eq!((src, dst), (GAME_IMAGE, APP_HANDLE + 1));
            assert_eq!(region.src_subresource.base_array_layer, 0);
            assert_eq!(region.dst_subresource.base_array_layer, eye as u32);
            assert_eq!(region.dst_offset, offset(0, 0, 0));
            assert_eq!((region.extent.width, region.extent.height), (16, 8));
        }
        assert!(gpu.copies.is_empty() && gpu.blits.is_empty());
    });
    finish(vk);
}

/// A multisampled texture of another format than the swapchain's is resolved into an image of its
/// own format, which is then copied.
#[test]
fn multisampled_eye_resolves_before_converting() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 1);
    let bounds = vr::VRTextureBounds_t {
        uMin: 0.5,
        ..FULL_BOUNDS
    };
    vk.copy_texture_to_swapchain(
        vr::EVREye::Right,
        &multisampled(unorm(texture(32, 16))),
        vr::EColorSpace::Gamma,
        bounds,
        0,
        vr::EVRSubmitFlags::Default,
    );
    gpu(|gpu| {
        let [(src, resolved, resolve)] = gpu.resolves[..] else {
            panic!("resolved {:?}", gpu.resolves);
        };
        assert_eq!(src, GAME_IMAGE);
        // Created by the backend, not the app's or the runtime's.
        assert!(gpu.live.contains(&resolved) && resolved < APP_HANDLE);
        assert_eq!(resolve.src_offset, offset(16, 0, 0));
        assert_eq!(resolve.dst_offset, offset(16, 0, 0));
        let [(copied, dst, copy)] = gpu.copies[..] else {
            panic!("copied {:?}", gpu.copies);
        };
        assert_eq!((copied, dst), (resolved, APP_HANDLE));
        assert_eq!(copy.src_offset, offset(16, 0, 0));
        assert_eq!(copy.dst_offset, offset(0, 0, 0));
        assert_eq!(copy.dst_subresource.base_array_layer, 1);
        assert_eq!((copy.extent.width, copy.extent.height), (16, 16));
    });
    finish(vk);
}

/// Blits take the far corner rather than the extent, also for a texture with both eyes.
#[test]
fn eye_blit_covers_the_bounds() {
    let mut vk = fake_vulkan();
    // The runtime doesn't support the texture's format.
    let images = vec![APP_HANDLE];
    vk.store_swapchain_images(images, vk::Format::B8G8R8A8_SRGB.as_raw() as u32);
    let bounds = vr::VRTextureBounds_t {
        uMin: 0.5,
        vMax: 0.5,
        ..FULL_BOUNDS
    };
    vk.copy_texture_to_swapchain(
        vr::EVREye::Right,
        &texture(64, 32),
        vr::EColorSpace::Auto,
        bounds,
        0,
        vr::EVRSubmitFlags::Default,
    );
    gpu(|gpu| {
        let [(src, dst, blit)] = gpu.blits[..] else {
            panic!("blitted {:?}", gpu.blits);
        };
        assert_eq!((src, dst), (GAME_IMAGE, APP_HANDLE));
        assert_eq!(blit.src_offsets, [offset(32, 0, 0), offset(64, 16, 1)]);
        assert_eq!(blit.dst_offsets, [offset(0, 0, 0), offset(32, 16, 1)]);
        assert_eq!(blit.dst_subresource.base_array_layer, 1);
    });
    finish(vk);
}

/// Copies `texture` into the right eye of a swapchain of `swapchain_format`.
fn copy_right_eye(
    swapchain_format: vk::Format,
    texture: &vr::VRVulkanTextureData_t,
    color_space: vr::EColorSpace,
    bounds: vr::VRTextureBounds_t,
) -> VulkanData {
    let mut vk = fake_vulkan();
    vk.store_swapchain_images(vec![APP_HANDLE], swapchain_format.as_raw() as u32);
    vk.copy_texture_to_swapchain(
        vr::EVREye::Right,
        texture,
        color_space,
        bounds,
        0,
        vr::EVRSubmitFlags::Default,
    );
    vk
}

fn with_format(
    format: vk::Format,
    texture: vr::VRVulkanTextureData_t,
) -> vr::VRVulkanTextureData_t {
    vr::VRVulkanTextureData_t {
        m_nFormat: format.as_raw() as u32,
        ..texture
    }
}

/// The layouts the barriers of `image` move it into, in order.
fn layouts(gpu: &Gpu, image: u64) -> Vec<vk::ImageLayout> {
    gpu.barriers
        .iter()
        .filter(|b| b.image.as_raw() == image)
        .map(|b| b.new_layout)
        .collect()
}

/// Linear UNORM data goes into a swapchain of its own format, which runtimes show as linear.
#[test]
fn linear_eye_is_copied_as_it_is() {
    let swapchain = swapchain_format(vk::Format::R8G8B8A8_UNORM, vr::EColorSpace::Linear);
    assert_eq!(swapchain, vk::Format::R8G8B8A8_UNORM);
    let texture = unorm(texture(16, 16));
    let vk = copy_right_eye(swapchain, &texture, vr::EColorSpace::Linear, FULL_BOUNDS);
    gpu(|gpu| {
        let [(src, dst, _)] = gpu.copies[..] else {
            panic!("copied {:?}", gpu.copies);
        };
        assert_eq!((src, dst), (GAME_IMAGE, APP_HANDLE));
        assert!(gpu.blits.is_empty() && gpu.image_formats.is_empty());
    });
    finish(vk);
}

/// Gamma encoded UNORM data for a linear swapchain is copied into an image of its sRGB variant,
/// which the blit into the swapchain decodes.
#[test]
fn gamma_eye_is_decoded_for_a_linear_swapchain() {
    let bounds = vr::VRTextureBounds_t {
        uMin: 0.5,
        ..FULL_BOUNDS
    };
    let vk = copy_right_eye(
        vk::Format::R16G16B16A16_SFLOAT,
        &unorm(texture(32, 16)),
        vr::EColorSpace::Auto,
        bounds,
    );
    gpu(|gpu| {
        let [(src, staging, copy)] = gpu.copies[..] else {
            panic!("copied {:?}", gpu.copies);
        };
        assert_eq!(src, GAME_IMAGE);
        assert_eq!(gpu.image_formats[&staging], vk::Format::R8G8B8A8_SRGB);
        assert_eq!(copy.src_offset, offset(16, 0, 0));
        assert_eq!(copy.dst_offset, offset(16, 0, 0));
        let [(blitted, dst, blit)] = gpu.blits[..] else {
            panic!("blitted {:?}", gpu.blits);
        };
        assert_eq!((blitted, dst), (staging, APP_HANDLE));
        assert_eq!(blit.src_offsets, [offset(16, 0, 0), offset(32, 16, 1)]);
        assert_eq!(blit.dst_offsets, [offset(0, 0, 0), offset(16, 16, 1)]);
        assert_eq!(blit.dst_subresource.base_array_layer, 1);
        assert_eq!(
            layouts(gpu, staging),
            [
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL
            ]
        );
    });
    finish(vk);
}

/// Gamma encoded float data keeps its encoded values on the way into an sRGB swapchain, rather
/// than getting encoded again.
#[test]
fn gamma_float_eye_is_not_encoded_again() {
    let format = vk::Format::R16G16B16A16_SFLOAT;
    let swapchain = swapchain_format(format, vr::EColorSpace::Gamma);
    let texture = with_format(format, texture(16, 16));
    let vk = copy_right_eye(swapchain, &texture, vr::EColorSpace::Gamma, FULL_BOUNDS);
    gpu(|gpu| {
        let [(src, staging, _)] = gpu.blits[..] else {
            panic!("blitted {:?}", gpu.blits);
        };
        assert_eq!(src, GAME_IMAGE);
        assert_eq!(gpu.image_formats[&staging], vk::Format::R8G8B8A8_UNORM);
        let [(copied, dst, copy)] = gpu.copies[..] else {
            panic!("copied {:?}", gpu.copies);
        };
        assert_eq!((copied, dst), (staging, APP_HANDLE));
        assert_eq!(copy.dst_subresource.base_array_layer, 1);
    });
    finish(vk);
}

/// Multisampled gamma encoded data is resolved in its own format before it's reinterpreted.
#[test]
fn multisampled_gamma_eye_is_resolved_then_decoded() {
    let vk = copy_right_eye(
        vk::Format::R16G16B16A16_SFLOAT,
        &multisampled(unorm(texture(16, 16))),
        vr::EColorSpace::Gamma,
        FULL_BOUNDS,
    );
    gpu(|gpu| {
        let [(src, resolved, _)] = gpu.resolves[..] else {
            panic!("resolved {:?}", gpu.resolves);
        };
        assert_eq!(src, GAME_IMAGE);
        assert_eq!(gpu.image_formats[&resolved], vk::Format::R8G8B8A8_UNORM);
        let [(copied, staging, _)] = gpu.copies[..] else {
            panic!("copied {:?}", gpu.copies);
        };
        assert_eq!(copied, resolved);
        assert_eq!(gpu.image_formats[&staging], vk::Format::R8G8B8A8_SRGB);
        let [(blitted, dst, _)] = gpu.blits[..] else {
            panic!("blitted {:?}", gpu.blits);
        };
        assert_eq!((blitted, dst), (staging, APP_HANDLE));
    });
    finish(vk);
}

/// The staging image is replaced when the texture's format changes, and freed with the swapchain,
/// but only once the copies using it are done.
#[test]
fn staging_images_outlive_their_copies() {
    let mut vk = fake_vulkan();
    let float = vk::Format::R16G16B16A16_SFLOAT.as_raw() as u32;
    vk.store_swapchain_images(vec![APP_HANDLE, APP_HANDLE + 1], float);
    let copy = |vk: &VulkanData, format, image_index| {
        vk.copy_texture_to_swapchain(
            vr::EVREye::Left,
            &with_format(format, texture(16, 16)),
            vr::EColorSpace::Gamma,
            FULL_BOUNDS,
            image_index,
            vr::EVRSubmitFlags::Default,
        );
    };
    let staging = || gpu(|gpu| gpu.copies.last().unwrap().1);

    copy(&vk, vk::Format::R8G8B8A8_UNORM, 0);
    let first = staging();
    copy(&vk, vk::Format::B8G8R8A8_UNORM, 1);
    let second = staging();
    assert_ne!(first, second);
    gpu(|gpu| {
        assert!(!gpu.live.contains(&first));
        assert_eq!(gpu.image_formats[&second], vk::Format::B8G8R8A8_SRGB);
    });

    // The swapchain is recreated while the copy is pending.
    vk.store_swapchain_images(vec![APP_HANDLE, APP_HANDLE + 1], float);
    gpu(|gpu| assert!(!gpu.live.contains(&second)));
    copy(&vk, vk::Format::R8G8B8A8_UNORM, 0);
    finish(vk);
}

/// SubmitWithArrayIndex picks the layer of the texture to read.
#[test]
fn eye_copy_reads_the_array_layer() {
    for texture in [texture(16, 16), multisampled(texture(16, 16))] {
        let mut vk = fake_vulkan();
        store_images(&mut vk, 1);
        let array = vr::VRVulkanTextureArrayData_t {
            _base: texture,
            m_unArrayIndex: 1,
            m_unArraySize: 2,
        };
        vk.copy_texture_to_swapchain(
            vr::EVREye::Left,
            std::ptr::from_ref(&array).cast(),
            vr::EColorSpace::Auto,
            FULL_BOUNDS,
            0,
            vr::EVRSubmitFlags::VulkanTextureWithArrayData,
        );
        gpu(|gpu| {
            let copies = gpu.copies.iter().map(|(.., c)| c.src_subresource);
            let resolves = gpu.resolves.iter().map(|(.., r)| r.src_subresource);
            let [source] = copies.chain(resolves).collect::<Vec<_>>()[..] else {
                panic!("copied {:?}, resolved {:?}", gpu.copies, gpu.resolves);
            };
            assert_eq!(source.base_array_layer, 1);
        });
        finish(vk);
    }
}

/// The image a multisampled texture is resolved into is replaced when the texture changes size,
/// and freed with the swapchain, but only once the copies using it are done.
#[test]
fn resolve_images_outlive_their_copies() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let copy = |vk: &VulkanData, texture, image_index| {
        vk.copy_texture_to_swapchain(
            vr::EVREye::Left,
            &multisampled(unorm(texture)),
            vr::EColorSpace::Gamma,
            FULL_BOUNDS,
            image_index,
            vr::EVRSubmitFlags::Default,
        );
    };
    let resolved = || gpu(|gpu| gpu.resolves.last().unwrap().1);

    copy(&vk, texture(16, 16), 0);
    let first = resolved();
    copy(&vk, texture(32, 32), 1);
    let second = resolved();
    assert_ne!(first, second);
    gpu(|gpu| assert!(!gpu.live.contains(&first)));

    // The swapchain is recreated while the copy is pending.
    store_images(&mut vk, 2);
    gpu(|gpu| assert!(!gpu.live.contains(&second)));
    copy(&vk, texture(32, 32), 0);
    finish(vk);
}

#[test]
fn reject_null_image() {
    let mut data = vr::VRVulkanTextureData_t {
        m_nImage: 0,
        m_pDevice: std::ptr::null_mut(),
        m_pPhysicalDevice: std::ptr::null_mut(),
        m_pInstance: std::ptr::null_mut(),
        m_pQueue: std::ptr::null_mut(),
        m_nQueueFamilyIndex: 0,
        m_nWidth: 1,
        m_nHeight: 1,
        m_nFormat: 0,
        m_nSampleCount: 1,
    };
    let texture = |data: &mut vr::VRVulkanTextureData_t| vr::Texture_t {
        handle: std::ptr::from_mut(data).cast(),
        eType: vr::ETextureType::Vulkan,
        eColorSpace: vr::EColorSpace::Auto,
    };
    let flags = vr::EVRSubmitFlags::Default;
    assert!(VulkanData::get_texture(&texture(&mut data), flags).is_none());
    data.m_nImage = 1;
    assert!(VulkanData::get_texture(&texture(&mut data), flags).is_some());
}
