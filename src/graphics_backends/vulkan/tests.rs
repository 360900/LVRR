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
    draws: usize,
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
    gpu(|gpu| {
        for fence in unsafe { slice(fences, count) } {
            let fence = fence.as_raw();
            let finished = gpu.pending.remove(&fence).is_some() || gpu.signaled.contains(&fence);
            check!(
                gpu,
                finished,
                "waited for fence {fence:#x} that never signals"
            );
            gpu.signaled.insert(fence);
        }
    });
    vk::Result::SUCCESS
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
        let in_flight = gpu.in_flight();
        let fence = fence.as_raw();
        let signaled = gpu.signaled.contains(&fence);
        check!(gpu, !signaled, "submitted with a signaled fence");
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
    });
    vk::Result::SUCCESS
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
    gpu(|gpu| gpu.record(buf, pipeline));
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
    _: vk::ShaderStageFlags,
    _: u32,
    _: u32,
    _: *const c_void,
) {
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
    _: u32,
    _: *const vk::ImageCopy,
) {
    gpu(|gpu| {
        gpu.record(buf, src);
        gpu.record(buf, dst);
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
        vkAllocateCommandBuffers,
        vkAllocateDescriptorSets,
        vkUpdateDescriptorSets,
        vkCreateFramebuffer,
        vkDestroyFramebuffer,
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
    ]
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
    let instance = unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(1)) };
    VulkanData {
        _entry: entry,
        instance,
        physical_device: vk::PhysicalDevice::from_raw(APP_HANDLE),
        device: unsafe { ash::Device::load_with(proc_addr, vk::Device::from_raw(APP_HANDLE)) },
        queue: vk::Queue::from_raw(APP_HANDLE),
        queue_family_index: 0,
        real_data: None,
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

fn copy_overlay(
    vk: &mut VulkanData,
    texture: &vr::VRVulkanTextureData_t,
    bounds: vr::VRTextureBounds_t,
    image_index: usize,
) -> xr::Extent2Di {
    vk.copy_overlay_to_swapchain(texture, bounds, image_index)
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
        assert_eq!(gpu.pipeline_samples, [vk::SampleCountFlags::TYPE_1]);
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
fn multisampled_overlay_is_cleared() {
    let mut vk = fake_vulkan();
    store_images(&mut vk, 2);
    let texture = vr::VRVulkanTextureData_t {
        m_nSampleCount: 4,
        ..texture(16, 16)
    };
    copy_overlay(&mut vk, &texture, FULL_BOUNDS, 0);
    gpu(|gpu| {
        assert_eq!(gpu.render_areas.len(), 1);
        assert_eq!(gpu.draws, 0);
        assert!(gpu.barriers.is_empty());
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
