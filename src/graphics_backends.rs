mod gl;
mod headless;
mod vulkan;

use crate::compositor::GpuDone;
use derive_more::{From, TryInto};
pub use gl::{GlBridgeData, GlData};
pub use headless::HeadlessData;
use openvr as vr;
use openxr as xr;
use std::fmt::Debug;
pub use vulkan::VulkanData;

pub trait GraphicsBackend: Into<SupportedBackend> {
    type Api: xr::Graphics + 'static;
    type OpenVrTexture: Copy;
    type Format: Copy + PartialEq + Debug;
    type NiceFormat: Debug;

    fn from_openxr_format(format: <Self::Api as xr::Graphics>::Format) -> Self::Format;

    fn to_openxr_format(format: Self::Format) -> <Self::Api as xr::Graphics>::Format;

    fn to_nice_format(format: Self::Format) -> Self::NiceFormat;

    fn session_create_info(&self) -> <Self::Api as xr::Graphics>::SessionCreateInfo;

    /// Whether the graphics context the session was created with is current on this thread.
    /// GL runtimes make it current to destroy swapchains if it isn't, which crashes once the app
    /// has closed it.
    fn is_context_current(&self) -> bool {
        true
    }

    /// Returns None if the texture is invalid. Submit flags can change what the handle is.
    fn get_texture(
        texture: &vr::Texture_t,
        flags: vr::EVRSubmitFlags,
    ) -> Option<Self::OpenVrTexture>;

    /// Whether the texture belongs to the device the session was set up with.
    fn is_texture_on_device(&self, _texture: Self::OpenVrTexture) -> bool {
        true
    }

    /// Layer `index` of an array texture, for SubmitWithArrayIndex. None if there is no such
    /// layer. Textures without layers only have layer 0, which is shown like with Submit.
    fn array_layer(&self, texture: Self::OpenVrTexture, index: u32) -> Option<Self::OpenVrTexture> {
        (index == 0).then_some(texture)
    }

    /// Backend implementation for swapchain_info_for_texture. Callers should
    /// use swapchain_info_for_texture instead, which fixes up invalid values
    /// games are known to submit.
    fn raw_swapchain_info_for_texture(
        &self,
        texture: Self::OpenVrTexture,
        bounds: vr::VRTextureBounds_t,
        color_space: vr::EColorSpace,
    ) -> xr::SwapchainCreateInfo<Self::Api>;

    fn swapchain_info_for_texture(
        &self,
        texture: Self::OpenVrTexture,
        bounds: vr::VRTextureBounds_t,
        color_space: vr::EColorSpace,
    ) -> xr::SwapchainCreateInfo<Self::Api> {
        let mut info = self.raw_swapchain_info_for_texture(texture, bounds, color_space);
        // Multisampled textures are resolved into the swapchain, and Monado and WiVRn reject
        // multisampled swapchains. Games also submit a sample count of 0 when MSAA is disabled
        // (e.g. Half-Life 2: VR with antialiasing set to None).
        info.sample_count = 1;
        info
    }

    /// The format to use when the runtime doesn't offer `format`, out of the `offered` ones in the
    /// runtime's order of preference. None takes the runtime's first.
    fn fallback_format(_format: Self::Format, _offered: &[Self::Format]) -> Option<Self::Format> {
        None
    }

    /// Called with the create info of a new swapchain, before its images are stored.
    fn swapchain_created(&mut self, _info: &xr::SwapchainCreateInfo<Self::Api>) {}

    fn store_swapchain_images(
        &mut self,
        images: Vec<<Self::Api as xr::Graphics>::SwapchainImage>,
        format: Self::Format,
    );

    fn copy_texture_to_swapchain(
        &self,
        eye: vr::EVREye,
        texture: Self::OpenVrTexture,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
        submit_flags: vr::EVRSubmitFlags,
    ) -> xr::Extent2Di;

    fn copy_overlay_to_swapchain(
        &mut self,
        texture: Self::OpenVrTexture,
        color_space: vr::EColorSpace,
        bounds: vr::VRTextureBounds_t,
        image_index: usize,
    ) -> xr::Extent2Di;

    /// Waits for the copies into the swapchain images to finish, so the swapchain can be destroyed.
    fn wait_for_copies(&self) {}

    /// Called once the app submitted every eye of a frame, while its queue or context is ours.
    /// Backends that can tell when the GPU finished the work submitted so far report it to
    /// `done` later, without waiting for it here.
    fn frame_submitted(&mut self, _done: GpuDone) {}
}

#[derive(macros::Backends, TryInto, From)]
#[try_into(owned, ref)]
#[allow(clippy::large_enum_variant)]
pub enum SupportedBackend {
    Vulkan(VulkanData),
    OpenGL(GlData),
    GlBridge(GlBridgeData),
    Headless(HeadlessData),
    #[cfg(test)]
    Fake(crate::compositor::FakeGraphicsData),
}

impl<B> GraphicsEnum<B> for SupportedBackend
where
    B: GraphicsBackend + TryFrom<Self>,
{
    type Inner = B;
}

#[allow(clippy::single_component_path_imports)]
pub(crate) use supported_apis_enum;
#[allow(clippy::single_component_path_imports)]
pub(crate) use supported_backends_enum;

// These traits are used to commit some type crimes, see macros::any_graphics
// None of them should be manually implemented
pub trait GraphicsEnum<G>: Sized {
    type Inner: TryFrom<Self>;
}

pub trait WithAnyGraphicsParams {
    type Args;
    type Ret;
}

pub trait WithAnyGraphics<G>: WithAnyGraphicsParams {
    type GraphicsEnum: GraphicsEnum<G>;
    fn with_any_graphics(
        inner: &<Self::GraphicsEnum as GraphicsEnum<G>>::Inner,
        args: Self::Args,
    ) -> Self::Ret;
}

pub trait WithAnyGraphicsMut<G>: WithAnyGraphicsParams {
    type GraphicsEnum: GraphicsEnum<G>;
    fn with_any_graphics(
        inner: &mut <Self::GraphicsEnum as GraphicsEnum<G>>::Inner,
        args: Self::Args,
    ) -> Self::Ret;
}

pub trait WithAnyGraphicsOwned<G>: WithAnyGraphicsParams {
    type GraphicsEnum: GraphicsEnum<G>;
    fn with_any_graphics(
        inner: <Self::GraphicsEnum as GraphicsEnum<G>>::Inner,
        args: Self::Args,
    ) -> Self::Ret;
}

impl SupportedBackend {
    pub fn is_texture_type_supported(texture_type: vr::ETextureType) -> bool {
        match texture_type {
            vr::ETextureType::Vulkan | vr::ETextureType::OpenGL => true,
            #[cfg(test)]
            vr::ETextureType::Reserved => true,
            _ => false,
        }
    }

    /// Returns None for texture types we don't support and for textures without a handle.
    pub fn new(texture: &vr::Texture_t, instance: &xr::Instance) -> Option<Self> {
        if texture.handle.is_null() {
            return None;
        }
        match texture.eType {
            vr::ETextureType::Vulkan => {
                let vk_texture = unsafe { &*(texture.handle as *const vr::VRVulkanTextureData_t) };
                let handles_set = !vk_texture.m_pInstance.is_null()
                    && !vk_texture.m_pPhysicalDevice.is_null()
                    && !vk_texture.m_pDevice.is_null()
                    && !vk_texture.m_pQueue.is_null();
                handles_set.then(|| Self::Vulkan(VulkanData::new(vk_texture)))
            }
            vr::ETextureType::OpenGL => {
                let gl = GlData::new(instance)?;
                if gl.on_runtime_device(instance) && gl.runtime_accepts_context() {
                    Some(Self::OpenGL(gl))
                } else {
                    GlBridgeData::new(gl, instance).map(Self::GlBridge)
                }
            }
            #[cfg(test)]
            vr::ETextureType::Reserved => Some(Self::Fake(
                crate::compositor::FakeGraphicsData::new(texture),
            )),
            _ => None,
        }
    }
}
