use super::GraphicsBackend;
use ash::vk::{self, Handle};
use log::warn;
use openvr as vr;
use openxr as xr;
use std::collections::HashSet;
use std::ffi::{CString, c_char};
use std::sync::{LazyLock, Mutex};

struct RealSessionData {
    images: Vec<vk::Image>,
    format: vk::Format,
    pool: vk::CommandPool,
    bufs: Vec<vk::CommandBuffer>,
    /// One per command buffer, signaled once its last submission has finished.
    fences: Vec<vk::Fence>,
    overlay_pipeline: Option<PipelineData>,
    /// The extent last copied to each eye of each image, if any.
    eye_extents: Mutex<Vec<[Option<vk::Extent2D>; 2]>>,
}

impl RealSessionData {
    fn destroy(self, device: &ash::Device) {
        // Leak rather than free anything a stuck copy may still use.
        if !wait_for_fences(device, &self.fences) {
            return;
        }
        unsafe {
            if let Some(pipeline) = &self.overlay_pipeline {
                pipeline.destroy(device);
            }
            for &fence in &self.fences {
                device.destroy_fence(fence, None);
            }
            device.destroy_command_pool(self.pool, None);
        }
    }
}

/// Far longer than any frame takes, so only a hung or lost GPU hits it.
const COPY_TIMEOUT_NS: u64 = 1_000_000_000;

/// Waits for our own submissions only: vkQueueWaitIdle and vkDeviceWaitIdle would need the app's
/// queues to be externally synchronized. Returns false if they may still be pending.
fn wait_for_fences(device: &ash::Device, fences: &[vk::Fence]) -> bool {
    let result = unsafe { device.wait_for_fences(fences, true, COPY_TIMEOUT_NS) };
    if let Err(e) = result {
        crate::warn_once!("Failed to wait for Vulkan copies to finish: {}", e);
    }
    result.is_ok()
}

pub struct VulkanData {
    _entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
    real_data: Option<RealSessionData>,
    /// Whether we created the instance and device for the temporary session. Otherwise they
    /// belong to the app, even before any swapchain images are stored.
    owns_device: bool,
}

impl Drop for VulkanData {
    fn drop(&mut self) {
        if let Some(data) = self.real_data.take() {
            data.destroy(&self.device);
        }
        if self.owns_device {
            unsafe {
                // The device may already be lost when the app shuts down. Destroying objects is
                // still allowed then.
                if let Err(e) = self.device.device_wait_idle() {
                    warn!("vkDeviceWaitIdle failed: {e}");
                }
                self.device.destroy_device(None);
                self.instance.destroy_instance(None);
            }
        }
    }
}

impl GraphicsBackend for VulkanData {
    type Api = xr::Vulkan;
    type OpenVrTexture = *const vr::VRVulkanTextureData_t;
    type Format = u32;
    type NiceFormat = vk::Format;

    fn from_openxr_format(format: u32) -> Self::Format {
        format as _
    }

    fn to_openxr_format(format: Self::Format) -> <Self::Api as xr::Graphics>::Format {
        format as _
    }

    #[inline]
    fn to_nice_format(format: Self::Format) -> Self::NiceFormat {
        vk::Format::from_raw(format as _)
    }

    fn session_create_info(&self) -> <Self::Api as openxr::Graphics>::SessionCreateInfo {
        let queue_families = unsafe {
            self.instance
                .get_physical_device_queue_family_properties(self.physical_device)
        };
        let family = queue_families[self.queue_family_index as usize];
        let queue_index = (0..family.queue_count)
            .find(|idx| {
                let queue = unsafe { self.device.get_device_queue(self.queue_family_index, *idx) };
                queue == self.queue
            })
            .unwrap_or_else(|| {
                panic!(
                    "Could not find queue index for queue {:?} in family {}",
                    self.queue, self.queue_family_index
                )
            });

        xr::vulkan::SessionCreateInfo {
            instance: self.instance.handle().as_raw() as _,
            physical_device: self.physical_device.as_raw() as _,
            device: self.device.handle().as_raw() as _,
            queue_family_index: self.queue_family_index,
            queue_index,
        }
    }

    fn get_texture(texture: &vr::Texture_t, _: vr::EVRSubmitFlags) -> Option<Self::OpenVrTexture> {
        (texture.eType == vr::ETextureType::Vulkan && !texture.handle.is_null())
            .then_some(texture.handle.cast())
    }
    fn store_swapchain_images(&mut self, images: Vec<u64>, format: Self::Format) {
        let images: Vec<vk::Image> = images.into_iter().map(vk::Image::from_raw).collect();
        let pool = unsafe {
            self.device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(self.queue_family_index),
                    None,
                )
                .unwrap()
        };
        let bufs = unsafe {
            self.device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        // We have to copy 2 eyes per swapchain image
                        .command_buffer_count(images.len() as u32 * 2),
                )
                .unwrap()
        };
        let fences = bufs
            .iter()
            .map(|_| unsafe {
                self.device
                    .create_fence(
                        &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                        None,
                    )
                    .unwrap()
            })
            .collect();

        let eye_extents = Mutex::new(vec![[None; 2]; images.len()]);
        if let Some(data) = self.real_data.replace(RealSessionData {
            images,
            format: vk::Format::from_raw(format as _),
            pool,
            bufs,
            fences,
            overlay_pipeline: Default::default(),
            eye_extents,
        }) {
            data.destroy(&self.device);
        }
    }

    fn raw_swapchain_info_for_texture(
        &self,
        texture: *const vr::VRVulkanTextureData_t,
        bounds: vr::VRTextureBounds_t,
        color_space: vr::EColorSpace,
    ) -> xr::SwapchainCreateInfo<Self::Api> {
        let texture = unsafe { texture.as_ref() }.unwrap();
        let (extent, _) = texture_extent_from_bounds(texture, bounds);
        xr::SwapchainCreateInfo {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                | xr::SwapchainUsageFlags::TRANSFER_DST,
            format: get_colorspace_corrected_format(
                vk::Format::from_raw(texture.m_nFormat as _),
                color_space,
            )
            .as_raw() as _,
            sample_count: texture.m_nSampleCount,
            width: extent.width,
            height: extent.height,
            face_count: 1,
            array_size: 2,
            mip_count: 1,
        }
    }

    fn copy_texture_to_swapchain(
        &self,
        eye: vr::EVREye,
        texture: *const vr::VRVulkanTextureData_t,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
        submit_flags: vr::EVRSubmitFlags,
    ) -> xr::Extent2Di {
        let (texture, array_data) =
            if (submit_flags & vr::EVRSubmitFlags::VulkanTextureWithArrayData).0 > 0 {
                let data = unsafe { &*texture.cast::<vr::VRVulkanTextureArrayData_t>() };
                (&data._base, Some(data))
            } else {
                (unsafe { &*texture }, None)
            };

        let data = self.real_data.as_ref().unwrap();
        let swapchain_image = data.images[image_index];
        let slot = 2 * image_index + eye.0 as usize;
        let buf = data.bufs[slot];

        let (extent, offset) = texture_extent_from_bounds(texture, bounds);
        log::trace!("{eye:?} extent: {extent:?} | bounds: {bounds:?}");
        let clear = {
            let mut eye_extents = data.eye_extents.lock().unwrap();
            let last = &mut eye_extents[image_index][eye as usize];
            let new = vk::Extent2D {
                width: extent.width,
                height: extent.height,
            };
            needs_clear(last.replace(new), new)
        };

        self.record_commands(slot, || unsafe {
            // transition swapchain image to TRANSFER_DST
            let swapchain_res = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: eye.0,
                layer_count: 1,
            };

            self.device.cmd_pipeline_barrier(
                buf,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier {
                    src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                    old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    image: swapchain_image,
                    subresource_range: swapchain_res,
                    ..Default::default()
                }],
            );

            if clear {
                self.device.cmd_clear_color_image(
                    buf,
                    swapchain_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                    &[swapchain_res],
                );
                self.device.cmd_pipeline_barrier(
                    buf,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier {
                        src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                        dst_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                        old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        image: swapchain_image,
                        subresource_range: swapchain_res,
                        ..Default::default()
                    }],
                );
            }

            // transfer image
            let subresource = vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            };

            let game_image = vk::Image::from_raw(texture.m_nImage);
            let game_layer = array_data.map(|d| d.m_unArrayIndex).unwrap_or(0);

            let copy = vk::ImageResolve {
                src_subresource: vk::ImageSubresourceLayers {
                    base_array_layer: game_layer,
                    ..subresource
                },
                src_offset: offset,
                dst_subresource: vk::ImageSubresourceLayers {
                    base_array_layer: eye.0,
                    ..subresource
                },
                dst_offset: vk::Offset3D::default(),
                extent,
            };

            let game_format = get_colorspace_corrected_format(
                vk::Format::from_raw(texture.m_nFormat as _),
                color_space,
            );
            if texture.m_nSampleCount > 1 {
                self.device.cmd_resolve_image(
                    buf,
                    game_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    swapchain_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy],
                );
            } else if data.format != game_format {
                let end_img_offset = vk::Offset3D {
                    x: extent.width as _,
                    y: extent.height as _,
                    z: 1,
                };
                self.device.cmd_blit_image(
                    buf,
                    game_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    swapchain_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::ImageBlit {
                        src_subresource: copy.src_subresource,
                        src_offsets: [copy.src_offset, end_img_offset],
                        dst_subresource: copy.dst_subresource,
                        dst_offsets: [copy.dst_offset, end_img_offset],
                    }],
                    vk::Filter::NEAREST,
                );
            } else {
                self.device.cmd_copy_image(
                    buf,
                    game_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    swapchain_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[
                        // SAFETY: ImageResolve and ImageCopy have the same fields and layout.
                        #[allow(unused_unsafe, clippy::missing_transmute_annotations)]
                        unsafe {
                            std::mem::transmute(copy)
                        },
                    ],
                );
            }

            // transition swapchain image back to OPTIMAL
            self.device.cmd_pipeline_barrier(
                buf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[vk::ImageMemoryBarrier {
                    src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                    dst_access_mask: vk::AccessFlags::empty(),
                    old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    new_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    image: swapchain_image,
                    subresource_range: swapchain_res,
                    ..Default::default()
                }],
            );
        });

        xr::Extent2Di {
            width: extent.width as _,
            height: extent.height as _,
        }
    }

    fn copy_overlay_to_swapchain(
        &mut self,
        texture: *const vr::VRVulkanTextureData_t,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
    ) -> xr::Extent2Di {
        let texture = unsafe { texture.as_ref() }.unwrap();
        // The swapchain only holds the bounds' sub-rect, which the vertex shader picks from the
        // texture, so the destination always starts at the origin.
        let (extent, _) = texture_extent_from_bounds(texture, bounds);
        let rect = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: extent.width,
                height: extent.height,
            },
        };
        let xr_extent = xr::Extent2Di {
            width: extent.width as _,
            height: extent.height as _,
        };
        // Nothing to draw, and Vulkan doesn't allow empty framebuffers or viewports.
        if extent.width == 0 || extent.height == 0 {
            return xr_extent;
        }
        let data = self.real_data.as_mut().unwrap();
        let buf = data.bufs[image_index];
        // The image's resources can only be replaced once its previous copy has finished.
        if !wait_for_fences(&self.device, &data.fences[image_index..=image_index]) {
            return xr_extent;
        }
        let pipeline_data = data
            .overlay_pipeline
            .get_or_insert_with(|| PipelineData::new(&self.device, data.format, &data.images));

        let game_image = vk::Image::from_raw(texture.m_nImage);
        let game_range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let slot = &mut pipeline_data.slots[image_index];
        unsafe {
            self.device.destroy_framebuffer(slot.framebuffer, None);
            self.device.destroy_image_view(slot.source, None);
            slot.source = self
                .device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(game_image)
                        .format(vk::Format::from_raw(texture.m_nFormat as _))
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .components(vk::ComponentMapping::default())
                        .subresource_range(game_range),
                    None,
                )
                .unwrap();
            slot.framebuffer = self
                .device
                .create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(pipeline_data.renderpass)
                        .attachments(&[slot.target])
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
                .unwrap();
            self.device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(slot.set)
                    .dst_binding(0)
                    .dst_array_element(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&[vk::DescriptorImageInfo {
                        sampler: pipeline_data.sampler,
                        image_view: slot.source,
                        image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    }])],
                &[],
            );
        }

        let pipeline = if texture.m_nSampleCount > 1 {
            pipeline_data.multisampled_pipeline
        } else {
            pipeline_data.pipeline
        };
        let PipelineData {
            layout, renderpass, ..
        } = *pipeline_data;
        let OverlaySlot {
            set, framebuffer, ..
        } = *slot;
        let decode_srgb =
            overlay_needs_srgb_decode(vk::Format::from_raw(texture.m_nFormat as _), color_space);
        let clear = [vk::ClearValue::default()];
        let render_pass_info = vk::RenderPassBeginInfo::default()
            .render_pass(renderpass)
            .framebuffer(framebuffer)
            .render_area(rect)
            .clear_values(&clear);
        // OpenVR hands over textures in TRANSFER_SRC_OPTIMAL, which can't be sampled.
        let to_sampled = vk::ImageMemoryBarrier {
            src_access_mask: vk::AccessFlags::MEMORY_WRITE,
            dst_access_mask: vk::AccessFlags::SHADER_READ,
            old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            image: game_image,
            subresource_range: game_range,
            ..Default::default()
        };
        let to_transfer_src = vk::ImageMemoryBarrier {
            src_access_mask: vk::AccessFlags::empty(),
            dst_access_mask: vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
            old_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            ..to_sampled
        };

        self.record_commands(image_index, || unsafe {
            let device = &self.device;
            device.cmd_pipeline_barrier(
                buf,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_sampled],
            );
            device.cmd_bind_pipeline(buf, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_set_viewport(
                buf,
                0,
                &[vk::Viewport {
                    width: extent.width as f32,
                    height: extent.height as f32,
                    x: 0.0,
                    y: 0.0,
                    min_depth: 0.0,
                    max_depth: 0.0,
                }],
            );
            device.cmd_set_scissor(buf, 0, &[rect]);
            device.cmd_bind_descriptor_sets(
                buf,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[set],
                &[],
            );
            let pc = [bounds.uMin, bounds.uMax, bounds.vMin, bounds.vMax];
            device.cmd_push_constants(
                buf,
                layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                pc.align_to().1,
            );
            device.cmd_push_constants(
                buf,
                layout,
                vk::ShaderStageFlags::FRAGMENT,
                DECODE_SRGB_OFFSET,
                &u32::from(decode_srgb).to_ne_bytes(),
            );
            device.cmd_begin_render_pass(buf, &render_pass_info, vk::SubpassContents::INLINE);
            device.cmd_draw(buf, 4, 1, 0, 0);
            device.cmd_end_render_pass(buf);
            device.cmd_pipeline_barrier(
                buf,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer_src],
            );
        });

        xr_extent
    }

    fn wait_for_copies(&self) {
        if let Some(data) = &self.real_data {
            wait_for_fences(&self.device, &data.fences);
        }
    }
}
impl VulkanData {
    /// Records and submits `bufs[idx]` once its previous submission has finished, or skips the
    /// copy if that never happens. Failures (e.g. a lost device) are only logged: the frame is lost
    /// either way, and the app finds out about a lost device from its own calls.
    pub fn record_commands(&self, idx: usize, cmds: impl FnOnce()) {
        let data = self.real_data.as_ref().unwrap();
        let (buf, fence) = (data.bufs[idx], data.fences[idx]);
        if !wait_for_fences(&self.device, &[fence]) {
            return;
        }
        // The fence stays signaled up to here, so a failure doesn't block later copies.
        let recorded = unsafe {
            self.device
                .begin_command_buffer(
                    buf,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .and_then(|()| {
                    cmds();
                    self.device.end_command_buffer(buf)
                })
                .and_then(|()| self.device.reset_fences(&[fence]))
        };
        if let Err(e) = recorded {
            crate::warn_once!("Failed to record Vulkan copy: {}", e);
            return;
        }

        unsafe {
            let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&buf));
            if let Err(e) = self.device.queue_submit(self.queue, &[submit], fence) {
                crate::warn_once!("Failed to submit Vulkan copy: {}", e);
                // A failed submit leaves the fence unsignaled. An empty one signals it after the
                // queue's earlier work, so the next copy into this image doesn't wait forever.
                let _ = self.device.queue_submit(self.queue, &[], fence);
            }
        }
    }

    pub fn new(data: &vr::VRVulkanTextureData_t) -> Self {
        let entry = new_entry();
        let instance = unsafe {
            ash::Instance::load(
                entry.static_fn(),
                vk::Instance::from_raw(data.m_pInstance as _),
            )
        };
        let device = unsafe {
            ash::Device::load(
                instance.fp_v1_0(),
                vk::Device::from_raw(data.m_pDevice as _),
            )
        };

        Self {
            _entry: entry,
            instance,
            physical_device: vk::PhysicalDevice::from_raw(data.m_pPhysicalDevice as _),
            device,
            queue: vk::Queue::from_raw(data.m_pQueue as _),
            queue_family_index: data.m_nQueueFamilyIndex,
            real_data: Default::default(),
            owns_device: false,
        }
    }

    pub fn new_temporary(xr_instance: &xr::Instance, system_id: xr::SystemId) -> Self {
        let entry = new_entry();

        let inst_exts = xr_instance
            .vulkan_legacy_instance_extensions(system_id)
            .unwrap();
        let inst_exts: Vec<CString> = inst_exts
            .split_ascii_whitespace()
            .map(|ext| CString::new(ext).unwrap())
            .collect();
        let inst_exts: Vec<*const c_char> = inst_exts.iter().map(|ext| ext.as_ptr()).collect();

        let instance = unsafe {
            entry
                .create_instance(
                    &vk::InstanceCreateInfo::default()
                        .application_info(
                            &vk::ApplicationInfo::default()
                                .api_version(vk::API_VERSION_1_0)
                                .application_name(c"XRizer temporary session"),
                        )
                        .enabled_extension_names(&inst_exts),
                    None,
                )
                .expect("Failed to create temporary Vulkan instance")
        };

        let physical_device = vk::PhysicalDevice::from_raw(unsafe {
            xr_instance
                .vulkan_graphics_device(system_id, instance.handle().as_raw() as _)
                .expect("Failed to get temporary Vulkan physical device") as _
        });
        let dev_exts = xr_instance
            .vulkan_legacy_device_extensions(system_id)
            .unwrap();
        let dev_exts: Vec<CString> = dev_exts
            .split_ascii_whitespace()
            .map(|ext| CString::new(ext).unwrap())
            .collect();
        let dev_exts: Vec<*const c_char> = dev_exts.iter().map(|ext| ext.as_ptr()).collect();

        // find whatever graphics queue family
        let queue_family_index =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) }
                .into_iter()
                .enumerate()
                .find_map(|(idx, family)| {
                    (family.queue_flags.contains(vk::QueueFlags::GRAPHICS)).then_some(idx)
                })
                .unwrap() as u32;

        let device = unsafe {
            instance
                .create_device(
                    physical_device,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(std::slice::from_ref(
                            &vk::DeviceQueueCreateInfo::default()
                                .queue_family_index(queue_family_index)
                                .queue_priorities(&[1.0]),
                        ))
                        .enabled_extension_names(&dev_exts),
                    None,
                )
                .expect("Could not create temporary vulkan device")
        };

        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue,
            queue_family_index,
            real_data: Default::default(),
            owns_device: true,
        }
    }
}

/// Overlay resources for one swapchain image.
#[derive(Clone, Copy)]
struct OverlaySlot {
    target: vk::ImageView,
    set: vk::DescriptorSet,
    /// The texture view and framebuffer of the last copy into the image, which may only be
    /// destroyed once that copy has finished.
    source: vk::ImageView,
    framebuffer: vk::Framebuffer,
}

/// The fragment shader's push constant follows the vertex shader's texture bounds.
const DECODE_SRGB_OFFSET: u32 = std::mem::size_of::<[f32; 4]>() as u32;

struct PipelineData {
    pipeline: vk::Pipeline,
    /// Averages the samples of multisampled textures, which can't be sampled normally.
    multisampled_pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    renderpass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    slots: Vec<OverlaySlot>,
}

impl PipelineData {
    fn new(device: &ash::Device, target_format: vk::Format, images: &[vk::Image]) -> Self {
        // The texture is only sampled, so the swapchain image is the only attachment. Overlay
        // swapchains are always single sampled.
        let samples = vk::SampleCountFlags::TYPE_1;
        let attachment = vk::AttachmentDescription {
            format: target_format,
            samples,
            load_op: vk::AttachmentLoadOp::CLEAR,
            store_op: vk::AttachmentStoreOp::STORE,
            initial_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            ..Default::default()
        };

        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&[vk::AttachmentReference {
                attachment: 0,
                layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            }]);
        let info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&attachment))
            .subpasses(std::slice::from_ref(&subpass));

        let renderpass = unsafe { device.create_render_pass(&info, None).unwrap() };

        let load_module = |stage, bytes| {
            struct ShaderModule<'a>(&'a ash::Device, vk::ShaderModule);
            impl Drop for ShaderModule<'_> {
                fn drop(&mut self) {
                    unsafe {
                        self.0.destroy_shader_module(self.1, None);
                    }
                }
            }

            let module = unsafe {
                device
                    .create_shader_module(
                        &vk::ShaderModuleCreateInfo::default()
                            .code(&ash::util::read_spv(&mut std::io::Cursor::new(bytes)).unwrap()),
                        None,
                    )
                    .unwrap()
            };

            (
                ShaderModule(device, module),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(stage)
                    .module(module)
                    .name(c"main"),
            )
        };

        let (_vert_module, vert_stage) = load_module(
            vk::ShaderStageFlags::VERTEX,
            &include_bytes!(concat!(env!("OUT_DIR"), "/vert_overlay.spv"))[..],
        );
        let (_frag_module, frag_stage) = load_module(
            vk::ShaderStageFlags::FRAGMENT,
            &include_bytes!(concat!(env!("OUT_DIR"), "/frag_overlay.spv"))[..],
        );
        let (_frag_ms_module, frag_ms_stage) = load_module(
            vk::ShaderStageFlags::FRAGMENT,
            &include_bytes!(concat!(env!("OUT_DIR"), "/frag_overlay_ms.spv"))[..],
        );

        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);

        let set_layout = unsafe {
            device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default()
                        .bindings(std::slice::from_ref(&binding)),
                    None,
                )
                .unwrap()
        };
        // One set per image, so a set is never updated while a pending copy uses it.
        let pool = unsafe {
            device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(images.len() as u32)
                        .pool_sizes(&[vk::DescriptorPoolSize {
                            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                            descriptor_count: images.len() as u32,
                        }]),
                    None,
                )
                .unwrap()
        };
        let sets = unsafe {
            device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&vec![set_layout; images.len()]),
                )
                .unwrap()
        };

        let push_constants = [
            vk::PushConstantRange {
                stage_flags: vk::ShaderStageFlags::VERTEX,
                offset: 0,
                size: DECODE_SRGB_OFFSET,
            },
            vk::PushConstantRange {
                stage_flags: vk::ShaderStageFlags::FRAGMENT,
                offset: DECODE_SRGB_OFFSET,
                size: std::mem::size_of::<u32>() as u32,
            },
        ];
        let pipeline_layout = unsafe {
            device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(std::slice::from_ref(&set_layout))
                        .push_constant_ranges(&push_constants),
                    None,
                )
                .unwrap()
        };

        let stages = [vert_stage, frag_stage];
        let ms_stages = [vert_stage, frag_ms_stage];
        let input_state = Default::default();
        let assembly_state = vk::PipelineInputAssemblyStateCreateInfo {
            topology: vk::PrimitiveTopology::TRIANGLE_STRIP,
            ..Default::default()
        };
        let tess_state = Default::default();
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rast_state = vk::PipelineRasterizationStateCreateInfo::default()
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0)
            .depth_bias_enable(false);
        let multi_state =
            vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(samples);
        let depth_state = vk::PipelineDepthStencilStateCreateInfo::default();
        let blend = vk::PipelineColorBlendAttachmentState {
            blend_enable: vk::FALSE,
            color_write_mask: vk::ColorComponentFlags::RGBA,
            ..Default::default()
        };
        let blend_state = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend));
        let d_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&d_states);

        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&input_state)
            .input_assembly_state(&assembly_state)
            .tessellation_state(&tess_state)
            .viewport_state(&viewport_state)
            .rasterization_state(&rast_state)
            .multisample_state(&multi_state)
            .depth_stencil_state(&depth_state)
            .color_blend_state(&blend_state)
            .dynamic_state(&dynamic)
            .render_pass(renderpass)
            .subpass(0)
            .layout(pipeline_layout)
            .base_pipeline_handle(vk::Pipeline::null())
            .base_pipeline_index(-1);

        let pipelines = unsafe {
            device
                .create_graphics_pipelines(
                    vk::PipelineCache::null(),
                    &[pipeline_info, pipeline_info.stages(&ms_stages)],
                    None,
                )
                .unwrap()
        };

        let slots = images
            .iter()
            .zip(sets)
            .map(|(&img, set)| {
                let target = unsafe {
                    device
                        .create_image_view(
                            &vk::ImageViewCreateInfo::default()
                                .image(img)
                                .view_type(vk::ImageViewType::TYPE_2D)
                                .format(target_format)
                                .components(vk::ComponentMapping::default())
                                .subresource_range(vk::ImageSubresourceRange {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    base_mip_level: 0,
                                    level_count: 1,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                }),
                            None,
                        )
                        .unwrap()
                };
                OverlaySlot {
                    target,
                    set,
                    source: vk::ImageView::null(),
                    framebuffer: vk::Framebuffer::null(),
                }
            })
            .collect();

        let sampler = unsafe {
            device
                .create_sampler(&vk::SamplerCreateInfo::default(), None)
                .unwrap()
        };

        Self {
            pipeline: pipelines[0],
            multisampled_pipeline: pipelines[1],
            layout: pipeline_layout,
            renderpass,
            set_layout,
            pool,
            sampler,
            slots,
        }
    }

    fn destroy(&self, device: &ash::Device) {
        unsafe {
            for slot in &self.slots {
                device.destroy_framebuffer(slot.framebuffer, None);
                device.destroy_image_view(slot.source, None);
                device.destroy_image_view(slot.target, None);
            }
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline(self.multisampled_pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_render_pass(self.renderpass, None);
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}

#[inline]
fn get_colorspace_corrected_format(format: vk::Format, color_space: vr::EColorSpace) -> vk::Format {
    static UNSUPPORTED: LazyLock<Mutex<HashSet<vk::Format>>> = LazyLock::new(Mutex::default);
    // https://github.com/ValveSoftware/openvr/wiki/Vulkan#image-formats
    match color_space {
        vr::EColorSpace::Linear => {
            crate::warn_unimplemented!("Linear colorspace");
            format
        }
        // Auto, Gamma, and Auto for values that aren't a color space
        _ => match format {
            vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => vk::Format::R8G8B8A8_SRGB,
            vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => vk::Format::B8G8R8A8_SRGB,
            vk::Format::BC3_SRGB_BLOCK => format,
            _ => {
                if UNSUPPORTED.lock().unwrap().insert(format) {
                    warn!("Unhandled texture format: {format:?}");
                }
                format
            }
        },
    }
}

/// Whether the overlay shader has to decode sRGB to output linear colour.
fn overlay_needs_srgb_decode(format: vk::Format, color_space: vr::EColorSpace) -> bool {
    use vk::Format as F;
    match format {
        // Sampling these already decodes.
        F::R8G8B8A8_SRGB
        | F::B8G8R8A8_SRGB
        | F::A8B8G8R8_SRGB_PACK32
        | F::BC1_RGB_SRGB_BLOCK
        | F::BC1_RGBA_SRGB_BLOCK
        | F::BC2_SRGB_BLOCK
        | F::BC3_SRGB_BLOCK
        | F::BC7_SRGB_BLOCK => false,
        // ColorSpace_Auto treats formats with _SRGB variants as gamma encoded.
        F::R8G8B8A8_UNORM
        | F::B8G8R8A8_UNORM
        | F::A8B8G8R8_UNORM_PACK32
        | F::BC1_RGB_UNORM_BLOCK
        | F::BC1_RGBA_UNORM_BLOCK
        | F::BC2_UNORM_BLOCK
        | F::BC3_UNORM_BLOCK
        | F::BC7_UNORM_BLOCK => color_space != vr::EColorSpace::Linear,
        _ => color_space == vr::EColorSpace::Gamma,
    }
}

/// Whether an eye's layer of a swapchain image has to be cleared before copying `new` to it.
/// Runtimes may show the pixels outside of the copied area when reprojecting, which are undefined
/// in a new image and left over from a larger copy otherwise.
fn needs_clear(last: Option<vk::Extent2D>, new: vk::Extent2D) -> bool {
    last.is_none_or(|last| new.width < last.width || new.height < last.height)
}

fn texture_extent_from_bounds(
    texture: &vr::VRVulkanTextureData_t,
    bounds: vr::VRTextureBounds_t,
) -> (vk::Extent3D, vk::Offset3D) {
    let width_min = bounds.uMin * texture.m_nWidth as f32;
    let width_max = bounds.uMax * texture.m_nWidth as f32;
    let height_min = bounds.vMin * texture.m_nHeight as f32;
    let height_max = bounds.vMax * texture.m_nHeight as f32;

    (
        vk::Extent3D {
            width: (width_max - width_min).abs() as u32,
            height: (height_max - height_min).abs() as u32,
            depth: 1,
        },
        vk::Offset3D {
            x: width_min.min(width_max) as i32,
            y: height_min.min(height_max) as i32,
            z: 0,
        },
    )
}

fn new_entry() -> ash::Entry {
    #[cfg(not(test))]
    unsafe {
        ash::Entry::load().unwrap()
    }

    #[cfg(test)]
    unsafe {
        ash::Entry::from_static_fn(ash::StaticFn {
            get_instance_proc_addr: fakexr::vulkan::get_instance_proc_addr,
        })
    }
}

#[cfg(test)]
mod tests;
