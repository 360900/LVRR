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
    /// Command buffers allocated from each pool.
    children: HashMap<u64, Vec<u64>>,
    /// Objects used by each command buffer's last recording.
    uses: HashMap<u64, HashSet<u64>>,
    /// Command buffers submitted with each fence, until the backend waits for it.
    pending: HashMap<u64, Vec<u64>>,
    errors: Vec<String>,
    barriers: Vec<vk::ImageMemoryBarrier<'static>>,
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

    /// Fences, command buffers and the objects they use that the GPU may still access.
    fn in_flight(&self) -> HashSet<u64> {
        let bufs = self.pending.values().flatten();
        let used = bufs
            .clone()
            .flat_map(|buf| self.uses.get(buf).into_iter().flatten());
        self.pending
            .keys()
            .chain(bufs)
            .chain(used)
            .copied()
            .collect()
    }

    fn destroy(&mut self, handle: u64) {
        if handle == 0 {
            return;
        }
        for child in self.children.remove(&handle).unwrap_or_default() {
            self.destroy(child);
        }
        check!(
            self,
            !self.in_flight().contains(&handle),
            "destroyed {handle:#x} while the GPU may use it"
        );
        check!(
            self,
            self.live.remove(&handle),
            "destroyed unknown {handle:#x}"
        );
    }

    fn record(&mut self, buf: vk::CommandBuffer, obj: impl Handle) {
        self.uses
            .entry(buf.as_raw())
            .or_default()
            .insert(obj.as_raw());
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
}

unsafe extern "system" fn vkAllocateCommandBuffers(
    _: vk::Device,
    info: *const vk::CommandBufferAllocateInfo<'_>,
    out: *mut vk::CommandBuffer,
) -> vk::Result {
    let info = unsafe { &*info };
    gpu(|gpu| {
        for i in 0..info.command_buffer_count as usize {
            let buf = gpu.create();
            gpu.children
                .entry(info.command_pool.as_raw())
                .or_default()
                .push(buf);
            unsafe { *out.add(i) = vk::CommandBuffer::from_raw(buf) };
        }
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkCreateFence(
    _: vk::Device,
    info: *const vk::FenceCreateInfo<'_>,
    _: *const vk::AllocationCallbacks<'_>,
    out: *mut vk::Fence,
) -> vk::Result {
    let signaled = unsafe { &*info }
        .flags
        .contains(vk::FenceCreateFlags::SIGNALED);
    gpu(|gpu| {
        let fence = gpu.create();
        if signaled {
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
            check!(
                gpu,
                gpu.pending.remove(&fence).is_some() || gpu.signaled.contains(&fence),
                "waiting for fence {fence:#x} that never signals"
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
        check!(
            gpu,
            !gpu.signaled.contains(&fence),
            "submitted with a signaled fence"
        );
        for submit in unsafe { slice(submits, count) } {
            for buf in unsafe { slice(submit.p_command_buffers, submit.command_buffer_count) } {
                check!(
                    gpu,
                    !in_flight.contains(&buf.as_raw()),
                    "submitted a pending command buffer"
                );
                // Without a fence, the work never becomes waitable.
                gpu.pending.entry(fence).or_default().push(buf.as_raw());
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
        check!(
            gpu,
            !gpu.in_flight().contains(&buf.as_raw()),
            "re-recorded a pending command buffer"
        );
        gpu.uses.remove(&buf.as_raw());
    });
    vk::Result::SUCCESS
}

unsafe extern "system" fn vkEndCommandBuffer(_: vk::CommandBuffer) -> vk::Result {
    vk::Result::SUCCESS
}

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
    gpu(|gpu| {
        gpu.barriers
            .extend(barriers.iter().map(|b| vk::ImageMemoryBarrier {
                p_next: std::ptr::null(),
                _marker: std::marker::PhantomData,
                ..*b
            }))
    });
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
        vkAllocateCommandBuffers,
        vkCreateFence,
        vkDestroyFence,
        vkWaitForFences,
        vkResetFences,
        vkQueueSubmit,
        vkBeginCommandBuffer,
        vkEndCommandBuffer,
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
fn teardown_after_device_lost() {
    use crate::clientcore::Injector;
    use crate::openxr_data::RealOpenXrData;

    let xr = RealOpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap();
    fakexr::vulkan::set_device_lost(true);
    // Drops the temporary session's VulkanData, which waits for its device.
    drop(xr);
}
