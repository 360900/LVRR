use crate::{
    clientcore::{Injected, Injector},
    compositor::{Compositor, is_usable_swapchain, is_valid_swapchain_info},
    graphics_backends::{GraphicsBackend, SupportedBackend, supported_apis_enum},
    openxr_data::{GraphicalSession, OpenXrData, Session, SessionData, SessionGuard},
    system::System,
};
use glam::{Mat3, Quat, Vec3, vec3};
use log::{debug, trace};
use openvr as vr;
use openxr as xr;
use slotmap::{Key, KeyData, SecondaryMap, SlotMap, new_key_type};
use std::f32::consts::{FRAC_PI_2, TAU};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::{
    collections::{HashMap, VecDeque},
    ops::Deref,
};

// OpenVR overlays are allowed to use ≥ 0
pub const SKYBOX_Z_ORDER: i64 = -1;

#[derive(macros::InterfaceImpl)]
#[interface = "IVROverlay"]
#[versions(
    028, 027, 026, 025, 024, 022, 021, 020, 019, 018, 017, 016, 014, 013, 012, 011, 010, 008, 007
)]
pub struct OverlayMan {
    vtables: Vtables,
    openxr: Arc<OpenXrData<Compositor>>,
    /// should only be externally accessed for testing
    pub(crate) compositor: Injected<Compositor>,
    system: Injected<System>,
    overlays: RwLock<SlotMap<OverlayKey, Overlay>>,
    key_to_overlay: RwLock<HashMap<CString, OverlayKey>>,
    skybox: RwLock<Vec<OverlayKey>>,
    /// Text returned by GetKeyboardText. There is no keyboard, so this is whatever the app
    /// passed in as the existing text.
    keyboard_text: Mutex<CString>,
}

#[derive(derive_more::Deref)]
struct RealSessionData<'a>(SessionGuard<'a>);

impl OverlayMan {
    pub fn new(openxr: Arc<OpenXrData<Compositor>>, injector: &Injector) -> Self {
        Self {
            vtables: Vtables::default(),
            openxr,
            compositor: injector.inject(),
            system: injector.inject(),
            overlays: Default::default(),
            key_to_overlay: Default::default(),
            skybox: Default::default(),
            keyboard_text: Default::default(),
        }
    }

    fn get_real_session_data(
        &self,
        texture: &vr::Texture_t,
        bounds: vr::VRTextureBounds_t,
    ) -> Result<RealSessionData<'_>, vr::EVROverlayError> {
        if !SupportedBackend::is_texture_type_supported(texture.eType) {
            log::warn!("Unsupported texture type: {:?}", texture.eType);
            return Err(vr::EVROverlayError::InvalidTexture);
        }
        if self.openxr.is_lost() {
            return Err(vr::EVROverlayError::RequestFailed);
        }

        // Overlays are only drawn by the scene app's frame loop. Without a compositor there is
        // nothing to restart the session.
        if !self.openxr.can_render() {
            crate::warn_once!(
                "{:?} applications can't show overlay textures",
                self.openxr.application_type
            );
            return Err(vr::EVROverlayError::RequestFailed);
        }

        if !self.openxr.session_data.get().is_real_session() {
            let Some(compositor) = self.compositor.get() else {
                crate::warn_once!("Can't show overlay textures before IVRCompositor is requested");
                return Err(vr::EVROverlayError::RequestFailed);
            };
            compositor
                .initialize_real_session(texture, bounds)
                .map_err(|_| vr::EVROverlayError::InvalidTexture)?;
        }
        Ok(RealSessionData(self.openxr.session_data.get()))
    }

    pub fn set_skybox(&self, textures: &[vr::Texture_t]) -> Result<(), vr::EVRCompositorError> {
        let Some(first) = textures.first() else {
            return Err(vr::EVRCompositorError::RequestFailed);
        };

        self.clear_skybox();

        let extensions = &self.openxr.enabled_extensions;
        // One texture is a lat-long panorama, two are one per eye.
        let equirect = textures.len() <= 2 && extensions.khr_composition_layer_equirect2;
        // Otherwise, like OpenComposite, show the first texture (the front of a cubemap) in front
        // of the user. rFactor2/GTR2 style loading screens are really just a static image there.
        let textures = if equirect { textures } else { &textures[..1] };

        // Skybox faces come in with V flipped compared to normal overlays.
        let bounds = vr::VRTextureBounds_t {
            uMin: 0.0,
            uMax: 1.0,
            vMin: 1.0,
            vMax: 0.0,
        };
        // This may restart the session, so it has to happen before taking the overlay locks: the
        // frame thread takes those while holding the session lock.
        let Ok(session_data) = self.get_real_session_data(first, bounds) else {
            log::warn!("Could not create swapchain for skybox texture; skipping");
            return Ok(());
        };

        let mut overlays = self.overlays.write().unwrap();
        let mut skybox = self.skybox.write().unwrap();

        let name = CString::new("__xrizer_skybox").unwrap();
        let mut keys = Vec::with_capacity(textures.len());
        for (index, texture) in textures.iter().enumerate() {
            let key = overlays.insert(Overlay::new(name.clone(), name.clone()));
            keys.push(key);
            let overlay = overlays.get_mut(key).unwrap();

            // Without an equirect layer, a curved surface a couple of meters in front of the user,
            // centered on them and wide enough to fill most of the forward view. The height
            // follows the texture's aspect.
            const DISTANCE: f32 = 2.0;
            overlay.width = 4.0;
            overlay.kind = if equirect {
                OverlayKind::Equirect {
                    eye: match (textures.len(), index) {
                        (1, _) => xr::EyeVisibility::BOTH,
                        (_, 0) => xr::EyeVisibility::LEFT,
                        _ => xr::EyeVisibility::RIGHT,
                    },
                }
            } else if extensions.khr_composition_layer_cylinder {
                OverlayKind::Curved {
                    curvature: overlay.width / (TAU * DISTANCE),
                }
            } else {
                OverlayKind::Quad
            };
            overlay.z_order = SKYBOX_Z_ORDER;
            // SteamVR draws the skybox opaque, whatever the texture's alpha says.
            overlay.flags = vr::VROverlayFlags::IgnoreTextureAlpha.0;
            overlay.bounds = bounds;
            overlay.visible = true;
            overlay.set_transform(
                TransformKind::Absolute(vr::ETrackingUniverseOrigin::Seated),
                vr::HmdMatrix34_t {
                    m: [
                        [1.0, 0.0, 0.0, 0.0],
                        [0.0, 1.0, 0.0, 0.0],
                        [0.0, 0.0, 1.0, -DISTANCE],
                    ],
                },
            );

            if overlay
                .set_texture(&self.openxr, key, &session_data, *texture)
                .is_err()
            {
                for key in keys {
                    overlays.remove(key);
                    session_data.overlay_data.remove(key);
                }
                log::warn!("Could not create swapchain for skybox texture; skipping");
                // Don't fail the game — just silently drop the skybox request.
                return Ok(());
            }
        }
        skybox.extend(keys);
        Ok(())
    }

    /// Waits for the copies into the overlay swapchains, which are destroyed with the session.
    pub fn wait_for_copies(&self) {
        #[macros::any_graphics(SupportedBackend)]
        fn wait<G: GraphicsBackend>(backend: &G) {
            backend.wait_for_copies();
        }

        for overlay in self.overlays.read().unwrap().values() {
            if let Some(backend) = &overlay.compositor {
                backend.with_any_graphics::<wait>(());
            }
        }
    }

    pub fn clear_skybox(&self) {
        let session = self.openxr.session_data.get();
        let mut overlays = self.overlays.write().unwrap();
        let mut skybox = self.skybox.write().unwrap();
        for key in skybox.drain(..) {
            overlays.remove(key);
            session.overlay_data.remove(key);
        }
    }

    pub fn get_layers<'a, G: xr::Graphics>(
        &self,
        session: &'a SessionData,
        render_skybox: bool,
    ) -> OverlayLayers<'a, G>
    where
        for<'b> &'b AnySwapchainMap: TryInto<&'b SwapchainMap<G>, Error: std::fmt::Display>,
    {
        let mut input = None;
        let overlays = self.overlays.read().unwrap();
        let swapchains_guard = session.overlay_data.swapchains.lock().unwrap();
        let Some(swapchains) = swapchains_guard.as_ref() else {
            return OverlayLayers {
                layers: Vec::new(),
                _swapchains: swapchains_guard,
            };
        };
        let swapchains: &SwapchainMap<G> = swapchains.try_into().unwrap_or_else(|e| {
            panic!(
                "Requested layers for API {}, but overlays are using a different API - {e}",
                std::any::type_name::<G>()
            )
        });

        let mut layers = Vec::with_capacity(overlays.len());
        for (key, overlay) in overlays.iter() {
            if !overlay.visible || (overlay.z_order == SKYBOX_Z_ORDER && !render_skybox) {
                continue;
            }
            let Some(rect) = overlay.rect else {
                continue;
            };

            // The rect can outlive the swapchain across a session restart.
            let Some(SwapchainData { swapchain, .. }) = swapchains.get(key) else {
                continue;
            };
            trace!("overlay rect: {rect:#?}");

            let tracking_space = session.get_space_for_origin(session.current_origin);
            let (space, pose) = match &overlay.transform {
                None => (
                    tracking_space,
                    xr::Posef {
                        position: xr::Vector3f {
                            x: 0.0,
                            y: 0.0,
                            z: -0.5,
                        },
                        orientation: xr::Quaternionf::IDENTITY,
                    },
                ),
                Some(transform) => match transform.kind {
                    TransformKind::Absolute(origin) => {
                        (session.get_space_for_origin(origin), transform.pose)
                    }
                    // The view space keeps head-locked overlays in place without any latency.
                    TransformKind::TrackedDeviceRelative(vr::k_unTrackedDeviceIndex_Hmd) => {
                        (&session.view_space, transform.pose)
                    }
                    TransformKind::TrackedDeviceRelative(device) => {
                        // Not drawn while its device isn't tracked.
                        let display_time = self.openxr.get_display_time();
                        let Some(device_pose) = input
                            .get_or_insert_with(|| self.openxr.input.get())
                            .as_ref()
                            .and_then(|i| i.get_device_pose(device, None, display_time, None))
                            .filter(|p| p.bPoseIsValid)
                        else {
                            continue;
                        };
                        (
                            tracking_space,
                            pose_mul(
                                &device_pose.mDeviceToAbsoluteTracking.into(),
                                &transform.pose,
                            ),
                        )
                    }
                    TransformKind::TrackedComponent(..) => continue,
                },
            };

            // OpenVR overlays have straight alpha unless they say otherwise.
            let premultiplied = overlay.has_flag(vr::VROverlayFlags::IsPremultiplied);
            let mut layer_flags = if premultiplied {
                xr::CompositionLayerFlags::EMPTY
            } else {
                xr::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA
            };
            if !overlay.has_flag(vr::VROverlayFlags::IgnoreTextureAlpha) {
                layer_flags |= xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;
            }

            macro_rules! layer_init {
                ($ty:ident) => {{
                    $ty::new()
                        .space(space)
                        .layer_flags(layer_flags)
                        .eye_visibility(xr::EyeVisibility::BOTH)
                        .sub_image(
                            xr::SwapchainSubImage::new()
                                .image_array_index(vr::EVREye::Left.0)
                                .swapchain(swapchain)
                                .image_rect(rect),
                        )
                }};
            }

            macro_rules! lifetime_extend {
                ($ty:ident, $layer:expr) => {{
                    fn lifetime_extend<'a, 'b: 'a, G: xr::Graphics>(
                        layer: $ty<'a, G>,
                    ) -> $ty<'b, G> {
                        // SAFETY: We need to remove the lifetimes to be able to return this layer.
                        // Internally, CompositionLayerQuad is using the raw OpenXR handles and PhantomData, not actual
                        // references, so returning it as long as we can guarantee the lifetimes of the space and
                        // swapchain is fine. The space lives as long as the SessionData, and the
                        // returned OverlayLayers keeps the swapchains locked.
                        unsafe { $ty::from_raw(layer.into_raw()) }
                    }

                    lifetime_extend($layer)
                }}
            }

            // OC-style size: width = widthMeters * scaleX, height = widthMeters * scaleY / aspect
            let aspect = if rect.extent.height > 0 {
                rect.extent.width as f32 / rect.extent.height as f32
            } else {
                1.0
            };
            let width = overlay.width * overlay.scale.0;
            let height = overlay.width * overlay.scale.1 / aspect;
            // The runtime rejects the whole frame if any layer is invalid.
            if !(is_valid_pose(&pose) && is_positive(width) && is_positive(height)) {
                crate::warn_once!("Skipping overlay {:?} with invalid placement", overlay.name);
                continue;
            }

            let layer = match overlay.kind {
                OverlayKind::Quad => {
                    use xr::CompositionLayerQuad;
                    let layer = layer_init!(CompositionLayerQuad)
                        .pose(pose)
                        .size(xr::Extent2Df { width, height });
                    OverlayLayerInner::Quad(lifetime_extend!(CompositionLayerQuad, layer))
                }
                // SetOverlayCurvature and set_skybox check for khr_composition_layer_cylinder
                OverlayKind::Curved { curvature } => {
                    // OpenVR: curvature = width / (2π * radius), so a curvature of 1 closes the
                    // cylinder. OpenXR wants an angle below 2π, which f32 2π isn't.
                    let angle = (TAU * curvature).min(TAU - 1e-4);
                    let radius = width / angle;
                    // The pose is the middle of the arc, which faces the cylinder's axis.
                    let (rot, pos) = to_glam(&pose);
                    let center = pos + rot.mul_vec3(Vec3::Z * radius);
                    if !(is_positive(angle) && is_positive(radius) && center.is_finite()) {
                        crate::warn_once!(
                            "Skipping overlay {:?} with invalid curvature",
                            overlay.name
                        );
                        continue;
                    }

                    use xr::CompositionLayerCylinderKHR;
                    let layer = layer_init!(CompositionLayerCylinderKHR)
                        .radius(radius)
                        .central_angle(angle)
                        .aspect_ratio(width / height)
                        .pose(xr::Posef {
                            orientation: pose.orientation,
                            position: xr::Vector3f {
                                x: center.x,
                                y: center.y,
                                z: center.z,
                            },
                        });
                    OverlayLayerInner::Cylinder(lifetime_extend!(
                        CompositionLayerCylinderKHR,
                        layer
                    ))
                }
                // set_skybox checks for khr_composition_layer_equirect2
                OverlayKind::Equirect { eye } => {
                    use xr::CompositionLayerEquirect2KHR;
                    // A radius of 0 is infinitely far away, like a skybox should be.
                    let layer = layer_init!(CompositionLayerEquirect2KHR)
                        .eye_visibility(eye)
                        .pose(pose)
                        .radius(0.0)
                        .central_horizontal_angle(TAU)
                        .upper_vertical_angle(FRAC_PI_2)
                        .lower_vertical_angle(-FRAC_PI_2);
                    OverlayLayerInner::Equirect2(lifetime_extend!(
                        CompositionLayerEquirect2KHR,
                        layer
                    ))
                }
            };
            let mut layer = OverlayLayer::from(layer);
            if let Some(alpha) = overlay.alpha {
                layer.set_alpha(alpha, premultiplied);
            }
            layers.push((overlay.z_order, layer));
        }

        // Sort by z_order asc
        layers.sort_by_key(|a| a.0);

        let sorted_layers: Vec<OverlayLayer<_>> = layers.into_iter().map(|(_, l)| l).collect();

        trace!("returning {} layers", sorted_layers.len());
        OverlayLayers {
            layers: sorted_layers,
            _swapchains: swapchains_guard,
        }
    }

    pub fn get_next_overlay_event(
        &self,
        event: *mut vr::VREvent_t,
        size: u32,
    ) -> Option<vr::VROverlayHandle_t> {
        if event.is_null() {
            return None;
        }
        let mut overlays = self.overlays.write().unwrap();
        let (key, queued) = overlays
            .iter_mut()
            .find_map(|(key, overlay)| Some((key, overlay.events.pop_front()?)))?;
        System::copy_event(&queued, event, size);
        Some(key.data().as_ffi())
    }

    /// We have no keyboard, so report it as done and closed immediately with the existing text
    /// unchanged. This lets games that wait for the keyboard continue instead of hanging.
    fn show_keyboard(
        &self,
        overlay: Option<vr::VROverlayHandle_t>,
        flags: u32,
        existing_text: *const c_char,
        user_value: u64,
    ) -> vr::EVROverlayError {
        let text = if existing_text.is_null() {
            CString::default()
        } else {
            unsafe { CStr::from_ptr(existing_text) }.to_owned()
        };
        debug!("no keyboard available, reporting done with existing text {text:?}");
        *self.keyboard_text.lock().unwrap() = text;

        let done = vr::VREvent_t {
            eventType: vr::EVREventType::KeyboardDone.0,
            trackedDeviceIndex: vr::k_unTrackedDeviceIndex_Hmd,
            eventAgeSeconds: 0.0,
            data: vr::VREvent_Data_t {
                keyboard: vr::VREvent_Keyboard_t {
                    cNewInput: [0; 8],
                    uUserValue: user_value,
                    overlayHandle: overlay.unwrap_or(0),
                },
            },
        };

        let closed = vr::VREvent_t {
            eventType: vr::EVREventType::KeyboardClosed.0,
            ..done
        };
        // Closed follows whatever closed the keyboard; there's no Done key to press if it's hidden.
        let events = if flags & vr::EKeyboardFlags::HideDoneKey.0 != 0 {
            &[closed][..]
        } else {
            &[done, closed]
        };

        if let Some(handle) = overlay {
            let mut overlays = self.overlays.write().unwrap();
            let Some(overlay) = overlays.get_mut(OverlayKey::from(KeyData::from_ffi(handle)))
            else {
                return vr::EVROverlayError::UnknownOverlay;
            };
            overlay.events.extend(events);
        } else {
            let system = self.system.force(|i| System::new(self.openxr.clone(), i));
            events.iter().for_each(|e| system.push_event(*e));
        }
        vr::EVROverlayError::None
    }
}

/// The overlay composition layers for a frame. Keeps the overlay swapchains locked until dropped, so
/// a texture update on another thread can't destroy a swapchain before the frame using it has ended.
pub struct OverlayLayers<'a, G: xr::Graphics> {
    layers: Vec<OverlayLayer<'a, G>>,
    _swapchains: MutexGuard<'a, Option<AnySwapchainMap>>,
}

impl<'a, G: xr::Graphics> Deref for OverlayLayers<'a, G> {
    type Target = [OverlayLayer<'a, G>];
    fn deref(&self) -> &Self::Target {
        &self.layers
    }
}

pub struct OverlayLayer<'a, G: xr::Graphics> {
    /// Only ever None during next_chain_insert
    layer: Option<OverlayLayerInner<'a, G>>,
    color_bias_khr: Option<Box<xr::sys::CompositionLayerColorScaleBiasKHR>>,
}

impl<G: xr::Graphics> OverlayLayer<'_, G> {
    fn set_alpha(&mut self, alpha: f32, premultiplied: bool) {
        // only one instance is stored, so this would cause segfault due to UAF
        debug_assert!(
            self.color_bias_khr.is_none(),
            "attempted to set_alpha on the same CompositorLayer twice!"
        );

        // Monado scales the color as submitted, without unpremultiplying it first, so
        // premultiplied colors have to be scaled along with the alpha.
        let rgb = if premultiplied { alpha } else { 1.0 };
        self.color_bias_khr = {
            let mut payload = Box::new(xr::sys::CompositionLayerColorScaleBiasKHR {
                ty: xr::StructureType::COMPOSITION_LAYER_COLOR_SCALE_BIAS_KHR,
                next: std::ptr::null(),
                color_bias: Default::default(),
                color_scale: xr::Color4f {
                    r: rgb,
                    g: rgb,
                    b: rgb,
                    a: alpha,
                },
            });

            let payload_ptr = payload.as_mut() as *mut _ as *mut xr::sys::BaseInStructure;
            unsafe { self.next_chain_insert(payload_ptr) };

            Some(payload)
        };
    }

    /// Insert the given item as the first element in the next chain.
    /// `item` must be a non-null pointer to a valid XrBaseInStructure object
    ///
    /// SAFETY: For lifetime guarantees, store item in Box inside CompositorLayer.
    unsafe fn next_chain_insert(&mut self, item: *mut xr::sys::BaseInStructure) {
        unsafe {
            let new_elem = item.as_mut().unwrap();
            self.layer = Some(match self.layer.take().unwrap() {
                OverlayLayerInner::Quad(quad) => {
                    let mut raw = quad.into_raw();
                    new_elem.next = raw.next as _;
                    raw.next = item as *const _;
                    OverlayLayerInner::Quad(xr::CompositionLayerQuad::from_raw(raw))
                }
                OverlayLayerInner::Cylinder(cylinder) => {
                    let mut raw = cylinder.into_raw();
                    new_elem.next = raw.next as _;
                    raw.next = item as *const _;
                    OverlayLayerInner::Cylinder(xr::CompositionLayerCylinderKHR::from_raw(raw))
                }
                OverlayLayerInner::Equirect2(equirect2) => {
                    let mut raw = equirect2.into_raw();
                    new_elem.next = raw.next as _;
                    raw.next = item as *const _;
                    OverlayLayerInner::Equirect2(xr::CompositionLayerEquirect2KHR::from_raw(raw))
                }
            });
        }
    }
}

impl<'a, G: xr::Graphics> From<OverlayLayerInner<'a, G>> for OverlayLayer<'a, G> {
    fn from(value: OverlayLayerInner<'a, G>) -> Self {
        Self {
            layer: Some(value),
            color_bias_khr: None,
        }
    }
}

impl<'a, G: xr::Graphics> Deref for OverlayLayer<'a, G> {
    type Target = xr::CompositionLayerBase<'a, G>;
    fn deref(&self) -> &Self::Target {
        self.layer.as_ref().unwrap().deref()
    }
}

pub enum OverlayLayerInner<'a, G: xr::Graphics> {
    Quad(xr::CompositionLayerQuad<'a, G>),
    // Curved overlays
    Cylinder(xr::CompositionLayerCylinderKHR<'a, G>),
    // Lat-long skybox
    Equirect2(xr::CompositionLayerEquirect2KHR<'a, G>),
}

impl<'a, G: xr::Graphics> Deref for OverlayLayerInner<'a, G> {
    type Target = xr::CompositionLayerBase<'a, G>;
    fn deref(&self) -> &Self::Target {
        match self {
            OverlayLayerInner::Quad(quad) => quad.deref(),
            OverlayLayerInner::Cylinder(cylinder) => cylinder.deref(),
            OverlayLayerInner::Equirect2(equirect2) => equirect2.deref(),
        }
    }
}

new_key_type!(
    pub(crate) struct OverlayKey;
);

pub(crate) struct SwapchainData<G: xr::Graphics> {
    swapchain: xr::Swapchain<G>,
    info: xr::SwapchainCreateInfo<G>,
    initial_format: G::Format,
}

pub(crate) type SwapchainMap<G> = SecondaryMap<OverlayKey, SwapchainData<G>>;
supported_apis_enum!(pub(crate) enum AnySwapchainMap: SwapchainMap);

#[derive(Default)]
pub struct OverlaySessionData {
    swapchains: Mutex<Option<AnySwapchainMap>>,
}

impl OverlaySessionData {
    /// Destroys an overlay's swapchain. The lock makes this wait for a frame still using it, and
    /// dropping the overlay's backend before makes it wait for the backend's copies into it.
    fn remove(&self, key: OverlayKey) {
        #[macros::any_graphics(AnySwapchainMap)]
        fn remove<G: xr::Graphics>(map: &mut SwapchainMap<G>, key: OverlayKey) {
            map.remove(key);
        }

        if let Some(map) = self.swapchains.lock().unwrap().as_mut() {
            map.with_any_graphics_mut::<remove>(key);
        }
    }
}

#[cfg(test)]
impl OverlaySessionData {
    pub fn swapchains_locked(&self) -> bool {
        self.swapchains.try_lock().is_err()
    }

    fn swapchain_count(&self) -> usize {
        #[macros::any_graphics(AnySwapchainMap)]
        fn len<G: xr::Graphics>(map: &SwapchainMap<G>) -> usize {
            map.len()
        }

        let swapchains = self.swapchains.lock().unwrap();
        swapchains
            .as_ref()
            .map_or(0, |map| map.with_any_graphics::<len>(()))
    }
}

fn is_positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

fn is_valid_pose(pose: &xr::Posef) -> bool {
    let (rotation, position) = to_glam(pose);
    position.is_finite() && rotation.is_normalized()
}

fn to_glam(pose: &xr::Posef) -> (Quat, Vec3) {
    let xr::Posef {
        position: p,
        orientation: o,
    } = pose;
    (Quat::from_xyzw(o.x, o.y, o.z, o.w), vec3(p.x, p.y, p.z))
}

fn from_glam(rotation: Quat, position: Vec3) -> xr::Posef {
    xr::Posef {
        position: xr::Vector3f {
            x: position.x,
            y: position.y,
            z: position.z,
        },
        orientation: xr::Quaternionf {
            x: rotation.x,
            y: rotation.y,
            z: rotation.z,
            w: rotation.w,
        },
    }
}

/// `b` relative to `a`.
fn pose_mul(a: &xr::Posef, b: &xr::Posef) -> xr::Posef {
    let ((a_rot, a_pos), (b_rot, b_pos)) = (to_glam(a), to_glam(b));
    from_glam((a_rot * b_rot).normalize(), a_pos + a_rot * b_pos)
}

/// Splits an overlay transform into its pose and the X and Y scale of its basis, which a pose can't
/// hold. The scale is the length of the basis vectors: the diagonal alone would shrink rotated
/// overlays.
fn decompose_transform(transform: &vr::HmdMatrix34_t) -> (xr::Posef, (f32, f32)) {
    let m = &transform.m;
    let column = |c: usize| Vec3::new(m[0][c], m[1][c], m[2][c]);
    let (x, y) = (column(0), column(1));
    // The rotation has to come from an orthonormal basis, or scale skews it. A degenerate basis
    // (such as a zero scale) has no rotation, and its overlay no area.
    let rotation = x
        .try_normalize()
        .and_then(|x| Some((x, (y - x * x.dot(y)).try_normalize()?)))
        .map_or(Quat::IDENTITY, |(x, y)| {
            Quat::from_mat3(&Mat3::from_cols(x, y, x.cross(y))).normalize()
        });
    let pose = from_glam(rotation, column(3));
    (pose, (x.length(), y.length()))
}

enum OverlayKind {
    Quad,
    Curved {
        curvature: f32,
    },
    /// A lat-long skybox
    Equirect {
        eye: xr::EyeVisibility,
    },
}

#[derive(Debug)]
enum TransformKind {
    Absolute(vr::ETrackingUniverseOrigin),
    TrackedDeviceRelative(vr::TrackedDeviceIndex_t),
    /// SteamVR draws these on the device's render model, and only while it draws the device itself,
    /// which it doesn't for a scene app. We never draw devices, so these aren't drawn either.
    TrackedComponent(vr::TrackedDeviceIndex_t, CString),
}

struct OverlayTransform {
    kind: TransformKind,
    /// As the app set it
    matrix: vr::HmdMatrix34_t,
    /// `matrix` without its scale, which goes into `Overlay::scale`
    pose: xr::Posef,
}

struct Overlay {
    key: CString,
    name: CString,
    /// Only allowed to be Some if KHR_composition_layer_color_scale_bias is active
    alpha: Option<f32>,
    width: f32,
    /// Scale factors of the transform matrix.
    /// Applied to the quad size like OpenComposite does, so games that encode
    /// non-unit scale in the transform get the size they expect.
    scale: (f32, f32),
    visible: bool,
    kind: OverlayKind,
    z_order: i64,
    bounds: vr::VRTextureBounds_t,
    /// From SetOverlayTextureColorSpace, used for textures that leave it on Auto.
    color_space: vr::EColorSpace,
    transform: Option<OverlayTransform>,
    compositor: Option<SupportedBackend>,
    rect: Option<xr::Rect2Di>,
    flags: u32,
    events: VecDeque<vr::VREvent_t>,
}

impl Overlay {
    fn new(key: CString, name: CString) -> Self {
        Self {
            key,
            name,
            alpha: None,
            width: 1.0,
            scale: (1.0, 1.0),
            visible: false,
            kind: OverlayKind::Quad,
            z_order: 0,
            bounds: vr::VRTextureBounds_t {
                uMin: 0.0,
                vMin: 0.0,
                uMax: 1.0,
                vMax: 1.0,
            },
            color_space: vr::EColorSpace::Auto,
            transform: None,
            compositor: None,
            rect: None,
            flags: 0,
            events: VecDeque::new(),
        }
    }

    fn set_texture(
        &mut self,
        openxr: &OpenXrData<Compositor>,
        key: OverlayKey,
        session_data: &RealSessionData<'_>,
        mut texture: vr::Texture_t,
    ) -> Result<(), vr::EVROverlayError> {
        if texture.eColorSpace == vr::EColorSpace::Auto {
            texture.eColorSpace = self.color_space;
        }
        let backend = match &self.compositor {
            Some(b) => b,
            None => self.compositor.insert(
                SupportedBackend::new(&texture, self.bounds)
                    .ok_or(vr::EVROverlayError::InvalidTexture)?,
            ),
        };

        #[macros::any_graphics(SupportedBackend)]
        fn create_swapchain_map<G: GraphicsBackend>(_: &G) -> AnySwapchainMap
        where
            AnySwapchainMap: From<SwapchainMap<G::Api>>,
        {
            SwapchainMap::<G::Api>::default().into()
        }

        let mut swapchains = session_data.overlay_data.swapchains.lock().unwrap();
        let swapchains =
            swapchains.get_or_insert_with(|| backend.with_any_graphics::<create_swapchain_map>(()));

        #[macros::any_graphics(SupportedBackend)]
        fn set_swapchain_texture<G: GraphicsBackend>(
            backend: &mut G,
            openxr: &OpenXrData<Compositor>,
            session_data: &SessionData,
            texture_bounds: vr::VRTextureBounds_t,
            map: &mut AnySwapchainMap,
            key: OverlayKey,
            texture: vr::Texture_t,
        ) -> Result<xr::Extent2Di, vr::EVROverlayError>
        where
            for<'a> &'a mut SwapchainMap<G::Api>:
                TryFrom<&'a mut AnySwapchainMap, Error: std::fmt::Display>,
            for<'a> &'a GraphicalSession: TryInto<&'a Session<G::Api>, Error: std::fmt::Display>,
            G::Format: Eq,
        {
            let map: &mut SwapchainMap<G::Api> = map.try_into().unwrap_or_else(|e| {
                panic!(
                    "Received different texture type for overlay than current ({}) - {e}",
                    std::any::type_name::<G::Api>()
                );
            });
            let Some(b_texture) = G::get_texture(&texture) else {
                debug!("received invalid overlay texture handle");
                return Err(vr::EVROverlayError::InvalidTexture);
            };
            let mut info =
                backend.swapchain_info_for_texture(b_texture, texture_bounds, texture.eColorSpace);
            // OpenComposite creates overlay swapchains with arraySize=1; some runtimes (e.g. Monado)
            // do not render quad layers correctly when a multi-array swapchain is used.
            info.array_size = 1;
            // Degenerate bounds or a texture without storage.
            if !is_valid_swapchain_info(&info) {
                debug!(
                    "overlay texture region is empty ({}x{})",
                    info.width, info.height
                );
                return Err(vr::EVROverlayError::InvalidTexture);
            }
            let failed = |call: &'static str| {
                move |e| {
                    openxr.handle_error(call, e);
                    vr::EVROverlayError::RequestFailed
                }
            };

            if !map.get(key).is_some_and(|data| {
                is_usable_swapchain(
                    &data.info,
                    G::from_openxr_format(data.initial_format),
                    &info,
                    G::from_openxr_format(info.format),
                )
            }) {
                let initial_format = info.format;
                session_data.check_format::<G>(&mut info);
                let swapchain = session_data
                    .create_swapchain(&info)
                    .map_err(failed("Creating overlay swapchain"))?;
                let images = swapchain
                    .enumerate_images()
                    .map_err(failed("Enumerating overlay swapchain images"))?;
                backend.store_swapchain_images(images, G::from_openxr_format(info.format));
                map.insert(
                    key,
                    SwapchainData {
                        swapchain,
                        info,
                        initial_format,
                    },
                );
            }

            let swapchain = &mut map[key].swapchain;
            let mut copy = || {
                let idx = swapchain.acquire_image()?;
                swapchain.wait_image(xr::Duration::INFINITE)?;
                let extent = backend.copy_overlay_to_swapchain(
                    b_texture,
                    texture.eColorSpace,
                    texture_bounds,
                    idx as usize,
                );
                swapchain.release_image()?;
                Ok(extent)
            };
            copy().map_err(|e| {
                // The swapchain might still hold an acquired image, so start over with a new one.
                map.remove(key);
                failed("Updating overlay swapchain")(e)
            })
        }

        // Bounds outside the texture would have the backends copy from outside of it.
        let b = self.bounds;
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        let bounds = vr::VRTextureBounds_t {
            uMin: clamp(b.uMin),
            vMin: clamp(b.vMin),
            uMax: clamp(b.uMax),
            vMax: clamp(b.vMax),
        };
        let backend = self.compositor.as_mut().unwrap();
        let extent = backend.with_any_graphics_mut::<set_swapchain_texture>((
            openxr,
            session_data,
            bounds,
            swapchains,
            key,
            texture,
        ))?;
        self.rect = Some(xr::Rect2Di {
            extent,
            offset: xr::Offset2Di::default(),
        });
        Ok(())
    }

    pub fn set_raw_texture(
        &mut self,
        _key: OverlayKey,
        _session_data: &SessionData,
        _texture: *mut c_void,
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
    ) -> Result<(), vr::EVROverlayError> {
        debug!(
            "setting overlay raw texture on {:?} with size {width}x{height} and {bytes_per_pixel} bytes per pixel",
            self.name
        );

        // let backend = self
        //     .compositor
        //     .get_or_insert_with(|| SupportedBackend::OpenGL(GlData::new()));

        // #[macros::any_graphics(SupportedBackend)]
        // fn create_swapchain_map<G: GraphicsBackend>(_: &G) -> AnySwapchainMap
        // where
        //     AnySwapchainMap: From<SwapchainMap<G::Api>>,
        // {
        //     SwapchainMap::<G::Api>::default().into()
        // }

        // let mut swapchains = session_data.overlay_data.swapchains.lock().unwrap();
        // let swapchains =
        //     swapchains.get_or_insert_with(|| backend.with_any_graphics::<create_swapchain_map>(()));

        // #[macros::any_graphics(SupportedBackend)]
        // fn set_swapchain_texture<G: GraphicsBackend>(
        //     backend: &mut G,
        //     session_data: &SessionData,
        //     texture_bounds: vr::VRTextureBounds_t,
        //     map: &mut AnySwapchainMap,
        //     key: OverlayKey,
        //     texture: *mut c_void,
        //     width: u32,
        //     height: u32,
        //     bytes_per_pixel: u32,
        // ) -> Result<xr::Extent2Di, vr::EVROverlayError>
        // where
        //     for<'a> &'a mut SwapchainMap<G::Api>:
        //         TryFrom<&'a mut AnySwapchainMap, Error: std::fmt::Display>,
        //     for<'a> &'a GraphicalSession: TryInto<&'a Session<G::Api>, Error: std::fmt::Display>,
        //     <G::Api as xr::Graphics>::Format: Eq,
        // {
        //     let map: &mut SwapchainMap<G::Api> = map.try_into().unwrap_or_else(|e| {
        //         panic!(
        //             "Received different texture type for overlay than current ({}) - {e}",
        //             std::any::type_name::<G::Api>()
        //         );
        //     });
        //     // let Some(b_texture) = G::get_texture(&texture) else {
        //     //     debug!("received invalid overlay texture handle");
        //     //     return Err(vr::EVROverlayError::InvalidTexture);
        //     // };
        //     // let b_texture = G::get_texture_from_raw(texture, width, height, bytes_per_pixel)
        //     //     .ok_or_else(|| {
        //     //         debug!("received invalid overlay raw texture");
        //     //         vr::EVROverlayError::InvalidTexture
        //     //     })?;
        //     let internal_format = match bytes_per_pixel {
        //         3 => gl::RGB8 as i32,
        //         4 => gl::RGBA8 as i32,
        //         _ => unreachable!(),
        //     };
        //     let format = match bytes_per_pixel {
        //         3 => gl::RGB,
        //         4 => gl::RGBA,
        //         _ => unreachable!(),
        //     };
        //     unsafe {
        //         gl::TexImage2D(
        //             gl::TEXTURE_2D,
        //             0,
        //             internal_format,
        //             width as i32,
        //             height as i32,
        //             0,
        //             format,
        //             gl::UNSIGNED_BYTE,
        //             texture.cast(),
        //         );
        //         gl::GenerateMipmap(gl::TEXTURE_2D);
        //         // get handle for TexImage2D
        //         // let tex_handle = gl::GetTexImage(gl::TEXTURE_2D, 0, format, gl::UNSIGNED_BYTE, texture.cast());
        //     }
        //     let tex_swapchain_info = backend.swapchain_info_for_texture(
        //         b_texture,
        //         texture_bounds,
        //         vr::EColorSpace::Auto,
        //     );
        //     let mut create_swapchain = || {
        //         let mut info = backend.swapchain_info_for_texture(
        //             b_texture,
        //             texture_bounds,
        //             vr::EColorSpace::Auto,
        //         );
        //         let initial_format = info.format;
        //         session_data.check_format::<G>(&mut info);
        //         let swapchain = session_data.create_swapchain(&info).unwrap();
        //         let images = swapchain
        //             .enumerate_images()
        //             .expect("Couldn't enumerate swapchain images");
        //         backend.store_swapchain_images(images, info.format);
        //         SwapchainData {
        //             swapchain,
        //             info,
        //             initial_format,
        //         }
        //     };
        //     let swapchain = {
        //         let data = map
        //             .entry(key)
        //             .unwrap()
        //             .or_insert_with(&mut create_swapchain);
        //         if !is_usable_swapchain(&data.info, data.initial_format, &tex_swapchain_info) {
        //             *data = create_swapchain();
        //         }
        //         &mut data.swapchain
        //     };
        //     let idx = swapchain.acquire_image().unwrap();
        //     swapchain.wait_image(xr::Duration::INFINITE).unwrap();
        //     let extent = backend.copy_overlay_to_swapchain(b_texture, texture_bounds, idx as usize);
        //     swapchain.release_image().unwrap();
        //     Ok(extent)
        // }
        // let backend = self.compositor.as_mut().unwrap();
        // let extent = backend.with_any_graphics_mut::<set_swapchain_texture>((
        //     session_data,
        //     self.bounds,
        //     swapchains,
        //     key,
        //     texture,
        //     width,
        //     height,
        //     bytes_per_pixel,
        // ))?;
        // self.rect = Some(xr::Rect2Di {
        //     extent,
        //     offset: xr::Offset2Di::default(),
        // });

        Ok(())
    }

    fn set_transform(&mut self, kind: TransformKind, matrix: vr::HmdMatrix34_t) {
        let (pose, scale) = decompose_transform(&matrix);
        debug!(
            "setting overlay transform of {:?} to {kind:?} {pose:?}",
            self.name
        );
        self.scale = scale;
        self.transform = Some(OverlayTransform { kind, matrix, pose });
    }

    fn has_flag(&self, flag: vr::VROverlayFlags) -> bool {
        self.flags & flag.0 != 0
    }

    pub fn set_flag(&mut self, flag: vr::VROverlayFlags, value: bool) {
        debug!(
            "setting overlay flag {flag:?} to {value} on {:?}",
            self.name
        );
        self.flags = if value {
            self.flags | flag.0
        } else {
            self.flags & !flag.0
        };
    }
    pub fn set_name(&mut self, name: CString) {
        debug!("setting overlay name to {name:?} on {:?}", self.name);
        self.name = name;
    }
}

impl OverlayMan {
    /// Copies a string field of an overlay into `buf` and returns the size it needs, including
    /// the null terminator. Nothing is copied if `buf` is too small.
    fn copy_overlay_string(
        &self,
        handle: vr::VROverlayHandle_t,
        field: impl FnOnce(&Overlay) -> &CStr,
        buf: *mut c_char,
        size: u32,
        error: *mut vr::EVROverlayError,
    ) -> u32 {
        let set_error = |e| {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = e;
            }
        };
        let overlays = self.overlays.read().unwrap();
        let Some(overlay) = overlays.get(OverlayKey::from(KeyData::from_ffi(handle))) else {
            set_error(vr::EVROverlayError::UnknownOverlay);
            return 0;
        };
        let data = field(overlay).to_bytes_with_nul();
        if buf.is_null() || (size as usize) < data.len() {
            set_error(vr::EVROverlayError::ArrayTooSmall);
        } else {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().cast(), buf, data.len()) };
            set_error(vr::EVROverlayError::None);
        }
        data.len() as u32
    }
}

macro_rules! get_overlay {
    (@impl $self:ident, $handle:expr, $overlay:ident, $lock:ident, $get:ident $(,$mut:ident)?) => {
        let $($mut)? overlays = $self.overlays.$lock().unwrap();
        let Some($overlay) = overlays.$get(OverlayKey::from(KeyData::from_ffi($handle))) else {
            return vr::EVROverlayError::UnknownOverlay;
        };
    };
    ($self:ident, $handle:expr, $overlay:ident) => {
        get_overlay!(@impl $self, $handle, $overlay, read, get);
    };
    ($self:ident, $handle:expr, mut $overlay:ident) => {
        get_overlay!(@impl $self, $handle, $overlay, write, get_mut, mut);
    };
}

impl vr::IVROverlay028_Interface for OverlayMan {
    fn CreateOverlay(
        &self,
        key: *const c_char,
        name: *const c_char,
        handle: *mut vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        if key.is_null() || name.is_null() || handle.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        let key = unsafe { CStr::from_ptr(key) };
        let name = unsafe { CStr::from_ptr(name) };

        let mut overlays = self.overlays.write().unwrap();
        let mut key_to_overlay = self.key_to_overlay.write().unwrap();
        if key_to_overlay.contains_key(key) {
            debug!("overlay key {key:?} is already in use");
            unsafe { handle.write(vr::k_ulOverlayHandleInvalid) };
            return vr::EVROverlayError::KeyInUse;
        }
        let ret_key = overlays.insert(Overlay::new(key.into(), name.into()));
        key_to_overlay.insert(key.into(), ret_key);

        unsafe {
            handle.write(ret_key.data().as_ffi());
        }

        debug!("created overlay {name:?} with key {key:?}");
        vr::EVROverlayError::None
    }

    fn CreateSubviewOverlay(
        &self,
        _parent_handle: vr::VROverlayHandle_t,
        subview_key: *const ::std::os::raw::c_char,
        subview_name: *const ::std::os::raw::c_char,
        subview_handle: *mut vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        self.CreateOverlay(subview_key, subview_name, subview_handle)
    }

    fn FindOverlay(
        &self,
        key: *const c_char,
        handle: *mut vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        if key.is_null() || handle.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        let key = unsafe { CStr::from_ptr(key) };
        let map = self.key_to_overlay.read().unwrap();
        if let Some(key) = map.get(key) {
            unsafe {
                handle.write(key.data().as_ffi());
            }
            vr::EVROverlayError::None
        } else {
            vr::EVROverlayError::UnknownOverlay
        }
    }

    fn ShowOverlay(&self, handle: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);

        debug!("showing overlay {:?}", overlay.name);
        overlay.visible = true;
        vr::EVROverlayError::None
    }

    fn HideOverlay(&self, handle: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);

        debug!("hiding overlay {:?}", overlay.name);
        overlay.visible = false;
        vr::EVROverlayError::None
    }

    fn SetOverlayAlpha(&self, handle: vr::VROverlayHandle_t, alpha: f32) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        if !self
            .openxr
            .enabled_extensions
            .khr_composition_layer_color_scale_bias
        {
            crate::warn_once!(
                "Cannot SetOverlayAlpha on {:?}: Runtime does not support KHR_composition_layer_color_scale_bias",
                overlay.name
            );
            return vr::EVROverlayError::None;
        }

        debug!(
            "overlay {:?} alpha {:.2} → {:.2}",
            overlay.name,
            overlay.alpha.unwrap_or(1.0),
            alpha
        );
        if alpha == 1.0 {
            overlay.alpha = None;
        } else {
            overlay.alpha = Some(alpha);
        }
        vr::EVROverlayError::None
    }

    fn SetOverlayWidthInMeters(
        &self,
        handle: vr::VROverlayHandle_t,
        width: f32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);

        debug!("setting overlay {:?} width to {width}", overlay.name);
        overlay.width = width;
        vr::EVROverlayError::None
    }

    fn SetOverlayTexture(
        &self,
        handle: vr::VROverlayHandle_t,
        texture: *const vr::Texture_t,
    ) -> vr::EVROverlayError {
        debug!("setting overlay texture");
        let bounds = {
            get_overlay!(self, handle, overlay);
            overlay.bounds
        };
        if texture.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        let texture = unsafe { texture.read() };

        // This may restart the session, so it has to happen before taking the overlay lock: the
        // frame thread takes that while holding the session lock.
        let data = match self.get_real_session_data(&texture, bounds) {
            Ok(data) => data,
            Err(e) => {
                debug!("failed to get real session data for overlay texture: {e:?}");
                return e;
            }
        };

        let key = OverlayKey::from(KeyData::from_ffi(handle));
        get_overlay!(self, handle, mut overlay);
        match overlay.set_texture(&self.openxr, key, &data, texture) {
            Err(e) => {
                debug!(
                    "failed to set overlay texture for {:?}: {e:?}",
                    overlay.name
                );
                e
            }
            Ok(_) => {
                debug!("set overlay texture for {:?}", overlay.name);
                vr::EVROverlayError::None
            }
        }
    }

    fn CloseMessageOverlay(&self) {
        crate::warn_unimplemented!("CloseMessageOverlay");
    }
    fn ShowMessageOverlay(
        &self,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
        _: *const c_char,
    ) -> vr::VRMessageOverlayResponse {
        crate::warn_unimplemented!("ShowMessageOverlay");
        vr::VRMessageOverlayResponse::ButtonPress_0
    }
    fn SetKeyboardPositionForOverlay(&self, _: vr::VROverlayHandle_t, _: vr::HmdRect2_t) {
        crate::warn_unimplemented!("SetKeyboardPositionForOverlay");
    }
    fn SetKeyboardTransformAbsolute(
        &self,
        _: vr::ETrackingUniverseOrigin,
        _: *const vr::HmdMatrix34_t,
    ) {
        crate::warn_unimplemented!("SetKeyboardTransformAbsolute");
    }
    fn HideKeyboard(&self) {}
    fn GetKeyboardText(&self, buf: *mut c_char, size: u32) -> u32 {
        let text = self.keyboard_text.lock().unwrap();
        let data = text.as_bytes_with_nul();
        if !buf.is_null() && size > 0 {
            let len = data.len().min(size as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr().cast(), buf, len);
                *buf.add(len - 1) = 0;
            }
        }
        data.len() as u32
    }
    fn ShowKeyboardForOverlay(
        &self,
        handle: vr::VROverlayHandle_t,
        _: vr::EGamepadTextInputMode,
        _: vr::EGamepadTextInputLineMode,
        flags: u32,
        _: *const c_char,
        _: u32,
        existing_text: *const c_char,
        user_value: u64,
    ) -> vr::EVROverlayError {
        self.show_keyboard(Some(handle), flags, existing_text, user_value)
    }
    fn ShowKeyboard(
        &self,
        _: vr::EGamepadTextInputMode,
        _: vr::EGamepadTextInputLineMode,
        flags: u32,
        _: *const c_char,
        _: u32,
        existing_text: *const c_char,
        user_value: u64,
    ) -> vr::EVROverlayError {
        self.show_keyboard(None, flags, existing_text, user_value)
    }
    fn GetPrimaryDashboardDevice(&self) -> vr::TrackedDeviceIndex_t {
        crate::warn_unimplemented!("GetPrimaryDashboardDevice");
        vr::k_unTrackedDeviceIndexInvalid
    }
    fn ShowDashboard(&self, _: *const c_char) {
        crate::warn_unimplemented!("ShowDashboard");
    }
    fn GetDashboardOverlaySceneProcess(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetDashboardOverlaySceneProcess");
        vr::EVROverlayError::RequestFailed
    }
    fn SetDashboardOverlaySceneProcess(
        &self,
        _: vr::VROverlayHandle_t,
        _: u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetDashboardOverlaySceneProcess");
        vr::EVROverlayError::RequestFailed
    }
    fn IsActiveDashboardOverlay(&self, _: vr::VROverlayHandle_t) -> bool {
        crate::warn_unimplemented!("IsActiveDashboardOverlay");
        false
    }
    fn IsDashboardVisible(&self) -> bool {
        false
    }
    fn CreateDashboardOverlay(
        &self,
        key: *const c_char,
        name: *const c_char,
        handle: *mut vr::VROverlayHandle_t,
        thumbnail_handle: *mut vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        let mut result = self.CreateOverlay(key, name, handle);
        if result == vr::EVROverlayError::None {
            let key = unsafe { CStr::from_ptr(key) };

            let mut concatenation = key.to_bytes().to_vec();
            concatenation.extend_from_slice(b".thumbnail\0");
            let thumbnail_key = CString::from_vec_with_nul(concatenation).unwrap();

            result = self.CreateOverlay(thumbnail_key.as_ptr(), name, thumbnail_handle);
            if result != vr::EVROverlayError::None {
                self.DestroyOverlay(unsafe { handle.read() });
            }
        }
        result
    }
    fn GetOverlayTextureSize(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut u32,
        _: *mut u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTextureSize");
        vr::EVROverlayError::RequestFailed
    }
    fn ReleaseNativeOverlayHandle(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut c_void,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("ReleaseNativeOverlayHandle");
        vr::EVROverlayError::RequestFailed
    }
    fn GetOverlayTexture(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut *mut c_void,
        _: *mut c_void,
        _: *mut u32,
        _: *mut u32,
        _: *mut u32,
        _: *mut vr::ETextureType,
        _: *mut vr::EColorSpace,
        _: *mut vr::VRTextureBounds_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTexture");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayFromFile(
        &self,
        _: vr::VROverlayHandle_t,
        _: *const c_char,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayFromFile");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayRaw(
        &self,
        handle: vr::VROverlayHandle_t,
        texture: *mut c_void,
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
    ) -> vr::EVROverlayError {
        let session_data = self.openxr.session_data.get();
        get_overlay!(self, handle, mut overlay);
        let key = OverlayKey::from(KeyData::from_ffi(handle));
        if let Err(error) =
            overlay.set_raw_texture(key, &session_data, texture, width, height, bytes_per_pixel)
        {
            return error;
        };
        vr::EVROverlayError::None
    }
    fn ClearOverlayTexture(&self, handle: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        // The swapchain stays for the next texture: destroying it here could free images that the
        // last copy into it is still writing to.
        overlay.rect = None;
        vr::EVROverlayError::None
    }
    fn ClearOverlayCursorPositionOverride(&self, _: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        crate::warn_unimplemented!("ClearOverlayCursorPositionOverride");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayCursorPositionOverride(
        &self,
        _: vr::VROverlayHandle_t,
        _: *const vr::HmdVector2_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayCursorPositionOverride");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayCursor(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayCursor");
        vr::EVROverlayError::RequestFailed
    }
    fn TriggerLaserMouseHapticVibration(
        &self,
        _: vr::VROverlayHandle_t,
        _: f32,
        _: f32,
        _: f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("TriggerLaserMouseHapticVibration");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayIntersectionMask(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::VROverlayIntersectionMaskPrimitive_t,
        _: u32,
        _: u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayIntersectionMask");
        vr::EVROverlayError::None
    }
    fn IsHoverTargetOverlay(&self, _: vr::VROverlayHandle_t) -> bool {
        crate::warn_unimplemented!("IsHoverTargetOverlay");
        false
    }
    fn ComputeOverlayIntersection(
        &self,
        _: vr::VROverlayHandle_t,
        _: *const vr::VROverlayIntersectionParams_t,
        _: *mut vr::VROverlayIntersectionResults_t,
    ) -> bool {
        crate::warn_unimplemented!("ComputeOverlayIntersection");
        false
    }
    fn SetOverlayMouseScale(
        &self,
        _: vr::VROverlayHandle_t,
        _: *const vr::HmdVector2_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayMouseScale");
        vr::EVROverlayError::RequestFailed
    }
    fn GetOverlayMouseScale(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::HmdVector2_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayMouseScale");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayInputMethod(
        &self,
        _: vr::VROverlayHandle_t,
        input_method: vr::VROverlayInputMethod,
    ) -> vr::EVROverlayError {
        if input_method == vr::VROverlayInputMethod::Mouse {
            crate::warn_unimplemented!("SetOverlayInputMethod::Mouse");
        } else if input_method == vr::VROverlayInputMethod::None {
            crate::warn_unimplemented!("SetOverlayInputMethod::None");
        }
        vr::EVROverlayError::None
    }
    fn GetOverlayInputMethod(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::VROverlayInputMethod,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayInputMethod");
        vr::EVROverlayError::RequestFailed
    }
    fn PollNextOverlayEvent(
        &self,
        handle: vr::VROverlayHandle_t,
        event: *mut vr::VREvent_t,
        size: u32,
    ) -> bool {
        if event.is_null() {
            return false;
        }
        let mut overlays = self.overlays.write().unwrap();
        let Some(queued) = overlays
            .get_mut(OverlayKey::from(KeyData::from_ffi(handle)))
            .and_then(|o| o.events.pop_front())
        else {
            return false;
        };
        System::copy_event(&queued, event, size);
        true
    }
    fn WaitFrameSync(&self, _: u32) -> vr::EVROverlayError {
        crate::warn_unimplemented!("WaitFrameSync");
        vr::EVROverlayError::None
    }
    fn GetTransformForOverlayCoordinates(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::ETrackingUniverseOrigin,
        _: vr::HmdVector2_t,
        _: *mut vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetTransformForOverlayCoordinates");
        vr::EVROverlayError::RequestFailed
    }
    fn IsOverlayVisible(&self, handle: vr::VROverlayHandle_t) -> bool {
        let overlays = self.overlays.read().unwrap();

        overlays
            .get(OverlayKey::from(KeyData::from_ffi(handle)))
            .map(|overlay| overlay.visible)
            .unwrap_or(false)
    }
    fn SetSubviewPosition(
        &self,
        _overlay_handle: vr::VROverlayHandle_t,
        _x: f32,
        _y: f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetSubviewPosition");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayTransformProjection(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::ETrackingUniverseOrigin,
        _: *const vr::HmdMatrix34_t,
        _: *const vr::VROverlayProjection_t,
        _: vr::EVREye,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTransformProjection");
        vr::EVROverlayError::RequestFailed
    }
    fn GetOverlayTransformCursor(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::HmdVector2_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTransformCursor");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayTransformCursor(
        &self,
        _: vr::VROverlayHandle_t,
        _: *const vr::HmdVector2_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTransformCursor");
        vr::EVROverlayError::RequestFailed
    }
    fn GetOverlayTransformTrackedDeviceComponent(
        &self,
        handle: vr::VROverlayHandle_t,
        out_device: *mut vr::TrackedDeviceIndex_t,
        buf: *mut c_char,
        size: u32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        let Some(TransformKind::TrackedComponent(device, component)) =
            overlay.transform.as_ref().map(|t| &t.kind)
        else {
            return vr::EVROverlayError::WrongTransformType;
        };
        let component = component.as_bytes_with_nul();
        if out_device.is_null() || buf.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        if (size as usize) < component.len() {
            return vr::EVROverlayError::ArrayTooSmall;
        }
        unsafe {
            out_device.write(*device);
            std::ptr::copy_nonoverlapping(component.as_ptr().cast(), buf, component.len());
        }
        vr::EVROverlayError::None
    }
    fn SetOverlayTransformTrackedDeviceComponent(
        &self,
        handle: vr::VROverlayHandle_t,
        device: vr::TrackedDeviceIndex_t,
        component: *const c_char,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        if component.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        if device >= vr::k_unMaxTrackedDeviceCount {
            return vr::EVROverlayError::InvalidTrackedDevice;
        }
        let component = unsafe { CStr::from_ptr(component) }.to_owned();
        overlay.set_transform(
            TransformKind::TrackedComponent(device, component),
            xr::Posef::IDENTITY.into(),
        );
        vr::EVROverlayError::None
    }
    fn GetOverlayTransformTrackedDeviceRelative(
        &self,
        handle: vr::VROverlayHandle_t,
        out_device: *mut vr::TrackedDeviceIndex_t,
        out_transform: *mut vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        let Some(OverlayTransform {
            kind: TransformKind::TrackedDeviceRelative(device),
            matrix,
            ..
        }) = &overlay.transform
        else {
            return vr::EVROverlayError::WrongTransformType;
        };
        if out_device.is_null() || out_transform.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        unsafe {
            out_device.write(*device);
            out_transform.write(*matrix);
        }
        vr::EVROverlayError::None
    }
    fn SetOverlayTransformTrackedDeviceRelative(
        &self,
        handle: vr::VROverlayHandle_t,
        device: vr::TrackedDeviceIndex_t,
        transform: *const vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        if transform.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        if device >= vr::k_unMaxTrackedDeviceCount {
            return vr::EVROverlayError::InvalidTrackedDevice;
        }
        overlay.set_transform(TransformKind::TrackedDeviceRelative(device), unsafe {
            transform.read()
        });
        vr::EVROverlayError::None
    }
    fn GetOverlayTransformAbsolute(
        &self,
        handle: vr::VROverlayHandle_t,
        out_origin: *mut vr::ETrackingUniverseOrigin,
        out_transform: *mut vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        let current_origin = self.openxr.get_tracking_space();
        get_overlay!(self, handle, overlay);
        let (origin, transform) = match &overlay.transform {
            Some(OverlayTransform {
                kind: TransformKind::Absolute(origin),
                matrix,
                ..
            }) => (*origin, *matrix),
            // if none, report the identity in the current tracking space
            None => (current_origin, xr::Posef::IDENTITY.into()),
            Some(_) => return vr::EVROverlayError::WrongTransformType,
        };
        if out_origin.is_null() || out_transform.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        unsafe {
            out_origin.write(origin);
            out_transform.write(transform);
        }
        vr::EVROverlayError::None
    }
    fn SetOverlayTransformAbsolute(
        &self,
        handle: vr::VROverlayHandle_t,
        origin: vr::ETrackingUniverseOrigin,
        transform: *const vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        if transform.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        overlay.set_transform(TransformKind::Absolute(origin), unsafe { transform.read() });
        vr::EVROverlayError::None
    }
    fn GetOverlayTransformType(
        &self,
        handle: vr::VROverlayHandle_t,
        ty: *mut vr::VROverlayTransformType,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        if ty.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        let kind = match overlay.transform.as_ref().map(|t| &t.kind) {
            None | Some(TransformKind::Absolute(_)) => vr::VROverlayTransformType::Absolute,
            Some(TransformKind::TrackedDeviceRelative(_)) => {
                vr::VROverlayTransformType::TrackedDeviceRelative
            }
            Some(TransformKind::TrackedComponent(..)) => {
                vr::VROverlayTransformType::TrackedComponent
            }
        };
        unsafe { ty.write(kind) };
        vr::EVROverlayError::None
    }
    fn GetOverlayTextureBounds(
        &self,
        handle: vr::VROverlayHandle_t,
        bounds: *mut vr::VRTextureBounds_t,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        if bounds.is_null() {
            vr::EVROverlayError::InvalidParameter
        } else {
            unsafe { bounds.write(overlay.bounds) };
            vr::EVROverlayError::None
        }
    }
    fn SetOverlayTextureBounds(
        &self,
        handle: vr::VROverlayHandle_t,
        bounds: *const vr::VRTextureBounds_t,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        if bounds.is_null() {
            vr::EVROverlayError::InvalidParameter
        } else {
            overlay.bounds = unsafe { bounds.read() };
            debug!("overlay {:?} {:?}", overlay.name, overlay.bounds);
            vr::EVROverlayError::None
        }
    }
    fn GetOverlayTextureColorSpace(
        &self,
        handle: vr::VROverlayHandle_t,
        color_space: *mut vr::EColorSpace,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        if color_space.is_null() {
            vr::EVROverlayError::InvalidParameter
        } else {
            unsafe { color_space.write(overlay.color_space) };
            vr::EVROverlayError::None
        }
    }
    fn SetOverlayTextureColorSpace(
        &self,
        handle: vr::VROverlayHandle_t,
        color_space: vr::EColorSpace,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        overlay.color_space = color_space;
        vr::EVROverlayError::None
    }
    fn GetOverlayPreCurvePitch(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayPreCurvePitch");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayPreCurvePitch(&self, _: vr::VROverlayHandle_t, _: f32) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayPreCurvePitch");
        vr::EVROverlayError::None
    }
    fn GetOverlayCurvature(
        &self,
        handle: vr::VROverlayHandle_t,
        value: *mut f32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        unsafe {
            *value = match overlay.kind {
                OverlayKind::Curved { curvature } => curvature,
                _ => 0.0,
            }
        }
        vr::EVROverlayError::None
    }
    fn SetOverlayCurvature(
        &self,
        handle: vr::VROverlayHandle_t,
        value: f32,
    ) -> vr::EVROverlayError {
        // All sanity checks must be made here
        if self
            .openxr
            .enabled_extensions
            .khr_composition_layer_cylinder
        {
            get_overlay!(self, handle, mut overlay);
            // Zero (or near-zero) curvature means "flat" in OpenVR semantics.
            // Treat as a plain quad rather than a degenerate cylinder (radius=inf).
            overlay.kind = if value > f32::EPSILON {
                OverlayKind::Curved {
                    curvature: value.min(1.0),
                }
            } else {
                OverlayKind::Quad
            };
        }
        vr::EVROverlayError::None
    }
    fn GetOverlayWidthInMeters(
        &self,
        handle: vr::VROverlayHandle_t,
        value: *mut f32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        unsafe {
            *value = overlay.width;
        }
        vr::EVROverlayError::None
    }
    fn GetOverlaySortOrder(
        &self,
        handle: vr::VROverlayHandle_t,
        value: *mut u32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        unsafe { *value = overlay.z_order as _ };
        vr::EVROverlayError::None
    }
    fn SetOverlaySortOrder(
        &self,
        handle: vr::VROverlayHandle_t,
        value: u32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        debug!(
            "overlay {:?} sort order {} → {}",
            overlay.name, overlay.z_order, value
        );
        overlay.z_order = value as _;
        vr::EVROverlayError::None
    }
    fn GetOverlayTexelAspect(&self, _: vr::VROverlayHandle_t, _: *mut f32) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTexelAspect");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayTexelAspect(&self, _: vr::VROverlayHandle_t, _: f32) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTexelAspect");
        vr::EVROverlayError::None
    }
    fn GetOverlayAlpha(
        &self,
        handle: vr::VROverlayHandle_t,
        value: *mut f32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        unsafe { *value = overlay.alpha.unwrap_or(1.0) };
        vr::EVROverlayError::None
    }

    fn GetOverlayColor(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut f32,
        _: *mut f32,
        _: *mut f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayColor");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayColor(
        &self,
        _: vr::VROverlayHandle_t,
        _: f32,
        _: f32,
        _: f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayColor");
        vr::EVROverlayError::None
    }
    fn GetOverlayFlags(
        &self,
        handle: vr::VROverlayHandle_t,
        flags: *mut u32,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        if flags.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        unsafe { flags.write(overlay.flags) };
        vr::EVROverlayError::None
    }
    fn GetOverlayFlag(
        &self,
        handle: vr::VROverlayHandle_t,
        flag: vr::VROverlayFlags,
        enabled: *mut bool,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        if enabled.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        unsafe { enabled.write(overlay.has_flag(flag)) };
        vr::EVROverlayError::None
    }
    fn SetOverlayFlag(
        &self,
        handle: vr::VROverlayHandle_t,
        flag: vr::VROverlayFlags,
        value: bool,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        overlay.set_flag(flag, value);
        vr::EVROverlayError::None
    }
    fn GetOverlayRenderingPid(&self, _: vr::VROverlayHandle_t) -> u32 {
        crate::warn_unimplemented!("GetOverlayRenderingPid");
        0
    }
    fn SetOverlayRenderingPid(&self, _: vr::VROverlayHandle_t, _: u32) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayRenderingPid");
        vr::EVROverlayError::RequestFailed
    }
    /// Return the name of the given error enum as a pointer to a static c string.
    fn GetOverlayErrorNameFromEnum(&self, e: vr::EVROverlayError) -> *const c_char {
        let res: &'static CStr = match e {
            vr::EVROverlayError::None => c"None",
            vr::EVROverlayError::UnknownOverlay => c"UnknownOverlay",
            vr::EVROverlayError::InvalidHandle => c"InvalidHandle",
            vr::EVROverlayError::PermissionDenied => c"PermissionDenied",
            vr::EVROverlayError::OverlayLimitExceeded => c"OverlayLimitExceeded",
            vr::EVROverlayError::WrongVisibilityType => c"WrongVisibilityType",
            vr::EVROverlayError::KeyTooLong => c"KeyTooLong",
            vr::EVROverlayError::NameTooLong => c"NameTooLong",
            vr::EVROverlayError::KeyInUse => c"KeyInUse",
            vr::EVROverlayError::WrongTransformType => c"WrongTransformType",
            vr::EVROverlayError::InvalidTrackedDevice => c"InvalidTrackedDevice",
            vr::EVROverlayError::InvalidParameter => c"InvalidParameter",
            vr::EVROverlayError::ThumbnailCantBeDestroyed => c"ThumbnailCantBeDestroyed",
            vr::EVROverlayError::ArrayTooSmall => c"ArrayTooSmall",
            vr::EVROverlayError::RequestFailed => c"RequestFailed",
            vr::EVROverlayError::InvalidTexture => c"InvalidTexture",
            vr::EVROverlayError::UnableToLoadFile => c"UnableToLoadFile",
            vr::EVROverlayError::KeyboardAlreadyInUse => c"KeyboardAlreadyInUse",
            vr::EVROverlayError::NoNeighbor => c"NoNeighbor",
            vr::EVROverlayError::TooManyMaskPrimitives => c"TooManyMaskPrimitives",
            vr::EVROverlayError::BadMaskPrimitive => c"BadMaskPrimitive",
            vr::EVROverlayError::TextureAlreadyLocked => c"TextureAlreadyLocked",
            vr::EVROverlayError::TextureLockCapacityReached => c"TextureLockCapacityReached",
            vr::EVROverlayError::TextureNotLocked => c"TextureNotLocked",
            vr::EVROverlayError::TimedOut => c"TimedOut",
            _ => c"Unknown",
        };
        res.as_ptr()
    }
    fn GetOverlayImageData(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
        _: *mut u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayImageData");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayName(
        &self,
        handle: vr::VROverlayHandle_t,
        name: *const c_char,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, mut overlay);
        overlay.set_name(unsafe { CStr::from_ptr(name).to_owned() });
        vr::EVROverlayError::None
    }
    fn GetOverlayName(
        &self,
        handle: vr::VROverlayHandle_t,
        buf: *mut c_char,
        size: u32,
        error: *mut vr::EVROverlayError,
    ) -> u32 {
        self.copy_overlay_string(handle, |o| &o.name, buf, size, error)
    }
    fn GetOverlayKey(
        &self,
        handle: vr::VROverlayHandle_t,
        buf: *mut c_char,
        size: u32,
        error: *mut vr::EVROverlayError,
    ) -> u32 {
        self.copy_overlay_string(handle, |o| &o.key, buf, size, error)
    }
    fn DestroyOverlay(&self, handle: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        let key = OverlayKey::from(KeyData::from_ffi(handle));

        let session = self.openxr.session_data.get();
        let mut overlays = self.overlays.write().unwrap();
        if let Some(overlay) = overlays.remove(key) {
            let mut map = self.key_to_overlay.write().unwrap();
            map.remove(&overlay.key);
            // Dropping the backend waits for its copies into the swapchain.
            drop(overlay);
            session.overlay_data.remove(key);
        }
        vr::EVROverlayError::None
    }
}

impl vr::IVROverlay026On027 for OverlayMan {
    fn SetOverlayTransformOverlayRelative(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::VROverlayHandle_t,
        _: *const vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTransformOverlayRelative");
        vr::EVROverlayError::None
    }
    fn GetOverlayTransformOverlayRelative(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::VROverlayHandle_t,
        _: *mut vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTransformOverlayRelative");
        vr::EVROverlayError::None
    }
}

impl vr::IVROverlay022On024 for OverlayMan {
    fn ShowKeyboardForOverlay(
        &self,
        handle: vr::VROverlayHandle_t,
        input_mode: vr::EGamepadTextInputMode,
        line_input_mode: vr::EGamepadTextInputLineMode,
        description: *const c_char,
        char_max: u32,
        existing_text: *const c_char,
        use_minimal_mode: bool,
        user_value: u64,
    ) -> vr::EVROverlayError {
        <Self as vr::IVROverlay024_Interface>::ShowKeyboardForOverlay(
            self,
            handle,
            input_mode,
            line_input_mode,
            if use_minimal_mode {
                vr::EKeyboardFlags::Minimal.0
            } else {
                0
            },
            description,
            char_max,
            existing_text,
            user_value,
        )
    }
    fn ShowKeyboard(
        &self,
        input_mode: vr::EGamepadTextInputMode,
        input_line_mode: vr::EGamepadTextInputLineMode,
        description: *const c_char,
        char_max: u32,
        existing_text: *const c_char,
        use_minimal_mode: bool,
        user_value: u64,
    ) -> vr::EVROverlayError {
        <Self as vr::IVROverlay024_Interface>::ShowKeyboard(
            self,
            input_mode,
            input_line_mode,
            if use_minimal_mode {
                vr::EKeyboardFlags::Minimal.0
            } else {
                0
            },
            description,
            char_max,
            existing_text,
            user_value,
        )
    }
    fn GetOverlayDualAnalogTransform(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::EDualAnalogWhich,
        _: *mut vr::HmdVector2_t,
        _: *mut f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayDualAnalogTransform");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayDualAnalogTransform(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::EDualAnalogWhich,
        _: *const vr::HmdVector2_t,
        _: f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayDualAnalogTransform");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayRenderModel(
        &self,
        _: vr::VROverlayHandle_t,
        _: *const c_char,
        _: *const vr::HmdColor_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayRenderModel");
        vr::EVROverlayError::RequestFailed
    }
    fn GetOverlayRenderModel(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut c_char,
        _: u32,
        _: *mut vr::HmdColor_t,
        _: *mut vr::EVROverlayError,
    ) -> u32 {
        crate::warn_unimplemented!("GetOverlayRenderModel");
        0
    }
}

impl vr::IVROverlay020On021 for OverlayMan {
    fn MoveGamepadFocusToNeighbor(
        &self,
        _: vr::EOverlayDirection,
        _: vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("MoveGamepadFocusToNeighbor");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayNeighbor(
        &self,
        _: vr::EOverlayDirection,
        _: vr::VROverlayHandle_t,
        _: vr::VROverlayHandle_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayNeighbor");
        vr::EVROverlayError::RequestFailed
    }
    fn SetGamepadFocusOverlay(&self, _: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetGamepadFocusOverlay");
        vr::EVROverlayError::RequestFailed
    }
    fn GetGamepadFocusOverlay(&self) -> vr::VROverlayHandle_t {
        crate::warn_unimplemented!("GetGamepadFocusOverlay");
        0
    }
    fn GetOverlayAutoCurveDistanceRangeInMeters(
        &self,
        _handle: vr::VROverlayHandle_t,
        _min_distance: *mut f32,
        _max_distance: *mut f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayAutoCurveDistanceRangeInMeters");
        vr::EVROverlayError::None
    }
    fn SetOverlayAutoCurveDistanceRangeInMeters(
        &self,
        _handle: vr::VROverlayHandle_t,
        _min_distance: f32,
        _max_distance: f32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayAutoCurveDistanceRangeInMeters");
        vr::EVROverlayError::None
    }
}

// The OpenVR commit messages mention that these functions just go through the standard overlay
// rendering path now.
impl vr::IVROverlay019On020 for OverlayMan {
    fn GetHighQualityOverlay(&self) -> vr::VROverlayHandle_t {
        crate::warn_unimplemented!("GetHighQualityOverlay");
        vr::k_ulOverlayHandleInvalid
    }
    fn SetHighQualityOverlay(&self, _: vr::VROverlayHandle_t) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetHighQualityOverlay");
        vr::EVROverlayError::None
    }
}

impl vr::IVROverlay017On018 for OverlayMan {
    fn HandleControllerOverlayInteractionAsMouse(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::TrackedDeviceIndex_t,
    ) -> bool {
        crate::warn_unimplemented!("HandleControllerOverlayInteractionAsMouse");
        false
    }
}

impl vr::IVROverlay013On014 for OverlayMan {
    fn GetOverlayTexture(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut *mut c_void,
        _: *mut c_void,
        _: *mut u32,
        _: *mut u32,
        _: *mut u32,
        _: *mut vr::EGraphicsAPIConvention,
        _: *mut vr::EColorSpace,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTexture");
        vr::EVROverlayError::RequestFailed
    }
}

impl vr::IVROverlay008On010 for OverlayMan {
    fn PollNextOverlayEvent(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::vr_0_9_14::VREvent_t,
    ) -> bool {
        crate::warn_unimplemented!("PollNextOverlayEvent");
        false
    }
}

impl vr::IVROverlay007On008 for OverlayMan {
    fn PollNextOverlayEvent(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::vr_0_9_12::VREvent_t,
    ) -> bool {
        crate::warn_unimplemented!("PollNextOverlayEvent");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clientcore::Injector, openxr_data::OpenXrData};
    use vr::IVROverlay028_Interface;

    fn overlay_man() -> OverlayMan {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        OverlayMan::new(xr, &Injector::default())
    }

    fn create(man: &OverlayMan, key: &CStr, name: &CStr) -> vr::VROverlayHandle_t {
        let mut handle = 0;
        assert_eq!(
            man.CreateOverlay(key.as_ptr(), name.as_ptr(), &mut handle),
            vr::EVROverlayError::None
        );
        handle
    }

    #[test]
    fn unknown_error_name() {
        let man = overlay_man();
        let name = |e| unsafe { CStr::from_ptr(man.GetOverlayErrorNameFromEnum(e)) };
        assert_eq!(name(vr::EVROverlayError::KeyInUse), c"KeyInUse");
        assert_eq!(name(vr::EVROverlayError(9999)), c"Unknown");
    }

    #[test]
    fn get_overlay_flags() {
        let man = overlay_man();
        let handle = create(&man, c"key", c"name");
        let flag = vr::VROverlayFlags::SendVRDiscreteScrollEvents;

        let mut enabled = true;
        assert_eq!(
            man.GetOverlayFlag(handle, flag, &mut enabled),
            vr::EVROverlayError::None
        );
        assert!(!enabled);

        assert_eq!(
            man.SetOverlayFlag(handle, flag, true),
            vr::EVROverlayError::None
        );
        assert_eq!(
            man.GetOverlayFlag(handle, flag, &mut enabled),
            vr::EVROverlayError::None
        );
        assert!(enabled);
        let mut flags = 0;
        assert_eq!(
            man.GetOverlayFlags(handle, &mut flags),
            vr::EVROverlayError::None
        );
        assert_eq!(flags, flag.0);

        assert_eq!(
            man.GetOverlayFlags(handle + 1, &mut flags),
            vr::EVROverlayError::UnknownOverlay
        );
    }

    #[test]
    fn overlay_key_in_use() {
        let man = overlay_man();
        let find = |key: &CStr| {
            let mut handle = 0;
            (man.FindOverlay(key.as_ptr(), &mut handle), handle)
        };
        let first = create(&man, c"key", c"name");
        let mut handle = 0;
        assert_eq!(
            man.CreateOverlay(c"key".as_ptr(), c"other".as_ptr(), &mut handle),
            vr::EVROverlayError::KeyInUse
        );
        assert_eq!(handle, vr::k_ulOverlayHandleInvalid);
        assert_eq!(find(c"key"), (vr::EVROverlayError::None, first));

        assert_eq!(man.DestroyOverlay(first), vr::EVROverlayError::None);
        assert_eq!(find(c"key").0, vr::EVROverlayError::UnknownOverlay);
        create(&man, c"key", c"name");

        // A dashboard overlay whose thumbnail can't be created isn't left behind.
        create(&man, c"dash.thumbnail", c"name");
        let (mut main, mut thumbnail) = (0, 0);
        assert_eq!(
            man.CreateDashboardOverlay(
                c"dash".as_ptr(),
                c"name".as_ptr(),
                &mut main,
                &mut thumbnail
            ),
            vr::EVROverlayError::KeyInUse
        );
        assert_eq!(find(c"dash").0, vr::EVROverlayError::UnknownOverlay);
    }

    #[test]
    fn get_overlay_key_and_name() {
        let man = overlay_man();
        let handle = create(&man, c"some.key", c"Some Name");
        let mut error = vr::EVROverlayError::RequestFailed;

        // Size query
        let len = man.GetOverlayKey(handle, std::ptr::null_mut(), 0, &mut error);
        assert_eq!(len, 9);
        assert_eq!(error, vr::EVROverlayError::ArrayTooSmall);

        let mut buf = [0 as c_char; 16];
        let len = man.GetOverlayKey(handle, buf.as_mut_ptr(), buf.len() as u32, &mut error);
        assert_eq!(len, 9);
        assert_eq!(error, vr::EVROverlayError::None);
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }, c"some.key");

        let len = man.GetOverlayName(handle, buf.as_mut_ptr(), buf.len() as u32, &mut error);
        assert_eq!(len, 10);
        assert_eq!(error, vr::EVROverlayError::None);
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }, c"Some Name");

        let len = man.GetOverlayName(handle + 1, buf.as_mut_ptr(), buf.len() as u32, &mut error);
        assert_eq!(len, 0);
        assert_eq!(error, vr::EVROverlayError::UnknownOverlay);
    }

    #[test]
    fn get_overlay_transform_type() {
        let man = overlay_man();
        let handle = create(&man, c"key", c"name");
        let mut ty = vr::VROverlayTransformType::Invalid;
        assert_eq!(
            man.GetOverlayTransformType(handle, &mut ty),
            vr::EVROverlayError::None
        );
        assert_eq!(ty, vr::VROverlayTransformType::Absolute);
    }

    fn transform_type(
        man: &OverlayMan,
        handle: vr::VROverlayHandle_t,
    ) -> vr::VROverlayTransformType {
        let mut ty = vr::VROverlayTransformType::Invalid;
        assert_eq!(
            man.GetOverlayTransformType(handle, &mut ty),
            vr::EVROverlayError::None
        );
        ty
    }

    #[test]
    fn transform_getters_follow_transform_type() {
        let man = overlay_man();
        let handle = create(&man, c"key", c"name");
        let offset = vr::HmdMatrix34_t {
            m: [
                [2.0, 0.0, 0.0, 0.0],
                [0.0, 2.0, 0.0, 0.1],
                [0.0, 0.0, 2.0, -1.0],
            ],
        };
        let (mut device, mut matrix, mut origin) =
            (99, Default::default(), vr::ETrackingUniverseOrigin::Seated);
        assert_eq!(
            man.SetOverlayTransformTrackedDeviceRelative(handle, 1, &offset),
            vr::EVROverlayError::None
        );
        assert_eq!(
            transform_type(&man, handle),
            vr::VROverlayTransformType::TrackedDeviceRelative
        );
        assert_eq!(
            man.GetOverlayTransformTrackedDeviceRelative(handle, &mut device, &mut matrix),
            vr::EVROverlayError::None
        );
        assert_eq!((device, matrix.m), (1, offset.m));
        assert_eq!(
            man.GetOverlayTransformAbsolute(handle, &mut origin, &mut matrix),
            vr::EVROverlayError::WrongTransformType
        );

        let mut name = [0 as c_char; 8];
        assert_eq!(
            man.SetOverlayTransformTrackedDeviceComponent(handle, 2, c"tip".as_ptr()),
            vr::EVROverlayError::None
        );
        assert_eq!(
            transform_type(&man, handle),
            vr::VROverlayTransformType::TrackedComponent
        );
        assert_eq!(
            man.GetOverlayTransformTrackedDeviceComponent(
                handle,
                &mut device,
                name.as_mut_ptr(),
                8
            ),
            vr::EVROverlayError::None
        );
        assert_eq!(device, 2);
        assert_eq!(unsafe { CStr::from_ptr(name.as_ptr()) }, c"tip");
        assert_eq!(
            man.GetOverlayTransformTrackedDeviceRelative(handle, &mut device, &mut matrix),
            vr::EVROverlayError::WrongTransformType
        );

        assert_eq!(
            man.SetOverlayTransformAbsolute(handle, vr::ETrackingUniverseOrigin::Standing, &offset),
            vr::EVROverlayError::None
        );
        assert_eq!(
            transform_type(&man, handle),
            vr::VROverlayTransformType::Absolute
        );
        assert_eq!(
            man.GetOverlayTransformAbsolute(handle, &mut origin, &mut matrix),
            vr::EVROverlayError::None
        );
        assert_eq!(
            (origin, matrix.m),
            (vr::ETrackingUniverseOrigin::Standing, offset.m)
        );

        assert_eq!(
            man.SetOverlayTransformTrackedDeviceRelative(
                handle,
                vr::k_unTrackedDeviceIndexInvalid,
                &offset
            ),
            vr::EVROverlayError::InvalidTrackedDevice
        );
    }

    /// Head-locked overlays (UE stereo layers, subtitles) must follow the head.
    #[test]
    fn hmd_relative_overlay_is_in_view_space() {
        let f = RealSessionFixture::new();
        let handle = f.textured_overlay(c"key");
        let view_space = f.xr.session_data.get().view_space.as_raw();
        let quad = || match f.layers(false).as_slice() {
            [TestLayer::Quad(quad, _)] => *quad,
            other => panic!("expected one quad, got {other:?}"),
        };

        let offset = vr::HmdMatrix34_t {
            m: [
                [2.0, 0.0, 0.0, 0.0],
                [0.0, 2.0, 0.0, -0.2],
                [0.0, 0.0, 2.0, -1.0],
            ],
        };
        assert_eq!(
            f.man.SetOverlayTransformTrackedDeviceRelative(
                handle,
                vr::k_unTrackedDeviceIndex_Hmd,
                &offset
            ),
            vr::EVROverlayError::None
        );
        let layer = quad();
        assert_eq!(layer.space, view_space);
        assert_eq!((layer.pose.position.y, layer.pose.position.z), (-0.2, -1.0));
        assert_eq!(layer.size.width, 2.0);
    }

    #[test]
    fn component_overlay_is_not_drawn() {
        let f = RealSessionFixture::new();
        let handle = f.textured_overlay(c"key");
        assert_eq!(f.layers(false).len(), 1);
        for device in [vr::k_unTrackedDeviceIndex_Hmd, 1] {
            assert_eq!(
                f.man
                    .SetOverlayTransformTrackedDeviceComponent(handle, device, c"tip".as_ptr()),
                vr::EVROverlayError::None
            );
            assert_eq!(f.layers(false).len(), 0);
        }
    }

    #[test]
    fn controller_relative_overlay_follows_controller() {
        use fakexr::UserPath::LeftHand;
        let f = RealSessionFixture::new();
        let input = Arc::new(crate::input::Input::new(f.xr.clone()));
        f.xr.input.set(Arc::downgrade(&input));
        // fakexr can't locate spaces in the stage space.
        f.xr.set_tracking_space(vr::ETrackingUniverseOrigin::Seated);
        let handle = f.textured_overlay(c"key");
        let offset = xr::Posef {
            position: xr::Vector3f {
                x: 0.0,
                y: 0.0,
                z: -0.1,
            },
            orientation: xr::Quaternionf::IDENTITY,
        };
        let left = 1;
        assert_eq!(
            f.man
                .SetOverlayTransformTrackedDeviceRelative(handle, left, &offset.into()),
            vr::EVROverlayError::None
        );
        // Not tracked yet
        assert_eq!(f.layers(false).len(), 0);

        let session = f.xr.session_data.get().session.as_raw();
        fakexr::set_interaction_profile(
            session,
            LeftHand,
            f.xr.instance
                .string_to_path("/interaction_profiles/khr/simple_controller")
                .unwrap(),
        );
        input.frame_start_update();
        f.xr.poll_events();
        let grip = xr::Posef {
            position: xr::Vector3f {
                x: 0.1,
                y: 1.0,
                z: -0.3,
            },
            orientation: xr::Quaternionf::IDENTITY,
        };
        fakexr::set_grip(session, LeftHand, grip);
        input.frame_start_update();

        let expected: xr::Posef = input
            .get_device_pose(left, None, f.xr.get_display_time(), None)
            .unwrap()
            .mDeviceToAbsoluteTracking
            .into();
        let layers = f.layers(false);
        let [TestLayer::Quad(quad, _)] = layers.as_slice() else {
            panic!("expected one quad, got {layers:?}");
        };
        let session = f.xr.session_data.get();
        assert_eq!(quad.space, session.tracking_space().as_raw());
        let ((rot, pos), (expected_rot, expected_pos)) =
            (to_glam(&quad.pose), to_glam(&pose_mul(&expected, &offset)));
        assert!(
            pos.abs_diff_eq(expected_pos, 1e-5),
            "{pos} != {expected_pos}"
        );
        assert!(
            rot.abs_diff_eq(expected_rot, 1e-5),
            "{rot} != {expected_rot}"
        );
        assert!(pos.distance(vec3(0.1, 1.0, -0.4)) < 0.2, "{pos}");
    }

    #[test]
    fn rotation_does_not_change_scale() {
        let rot = glam::Mat3::from_rotation_y(60f32.to_radians())
            * glam::Mat3::from_rotation_x(20f32.to_radians())
            * glam::Mat3::from_diagonal(Vec3::new(2.0, 0.5, 1.0));
        let c = rot.to_cols_array_2d();
        let transform = vr::HmdMatrix34_t {
            m: std::array::from_fn(|r| [c[0][r], c[1][r], c[2][r], 1.0]),
        };
        let (_, (x, y)) = decompose_transform(&transform);
        assert!((x - 2.0).abs() < 1e-5, "{x}");
        assert!((y - 0.5).abs() < 1e-5, "{y}");
    }

    #[test]
    fn scale_does_not_change_rotation() {
        let matrix = |m: glam::Mat3| {
            let c = m.to_cols_array_2d();
            vr::HmdMatrix34_t {
                m: std::array::from_fn(|r| [c[0][r], c[1][r], c[2][r], 1.0]),
            }
        };
        let rotation = |m| {
            let (xr::Posef { orientation: o, .. }, _) = decompose_transform(&matrix(m));
            Quat::from_xyzw(o.x, o.y, o.z, o.w)
        };

        let yaw = Quat::from_rotation_y(90f32.to_radians());
        let scaled =
            glam::Mat3::from_quat(yaw) * glam::Mat3::from_diagonal(Vec3::new(2.0, 0.5, 3.0));
        assert!(
            rotation(scaled).angle_between(yaw) < 1e-4,
            "{}",
            rotation(scaled)
        );

        // A zero scale is used to hide overlays; it must still give a valid rotation.
        assert_eq!(rotation(glam::Mat3::ZERO), Quat::IDENTITY);
    }

    fn keyboard_event(ty: vr::EVREventType, user_value: u64) -> impl Fn(&vr::VREvent_t) {
        move |event| {
            assert_eq!(event.eventType, ty.0);
            assert_eq!(unsafe { event.data.keyboard.uUserValue }, user_value);
        }
    }

    #[test]
    fn keyboard_reports_done_with_existing_text() {
        use vr::IVRSystem026_Interface;
        let man = overlay_man();
        let system = man.system.force(|i| System::new(man.openxr.clone(), i));
        let size = std::mem::size_of::<vr::VREvent_t>() as u32;
        let poll = || {
            let mut event: vr::VREvent_t = unsafe { std::mem::zeroed() };
            system.PollNextEvent(&mut event, size).then_some(event)
        };
        // The first poll sets up input, which reports the headset. The test looks at what follows.
        while poll().is_some() {}

        assert_eq!(
            man.ShowKeyboard(
                vr::EGamepadTextInputMode::Normal,
                vr::EGamepadTextInputLineMode::SingleLine,
                0,
                c"desc".as_ptr(),
                32,
                c"Adventurer".as_ptr(),
                1234,
            ),
            vr::EVROverlayError::None
        );
        keyboard_event(vr::EVREventType::KeyboardDone, 1234)(&poll().unwrap());
        keyboard_event(vr::EVREventType::KeyboardClosed, 1234)(&poll().unwrap());
        assert!(poll().is_none());

        let mut buf = [0 as c_char; 32];
        let len = man.GetKeyboardText(buf.as_mut_ptr(), buf.len() as u32);
        assert_eq!(len as usize, c"Adventurer".to_bytes_with_nul().len());
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }, c"Adventurer");

        // Truncated, but still null terminated.
        let mut small = [1 as c_char; 4];
        man.GetKeyboardText(small.as_mut_ptr(), small.len() as u32);
        assert_eq!(unsafe { CStr::from_ptr(small.as_ptr()) }, c"Adv");
    }

    #[test]
    fn skybox_locks_overlays_first() {
        let man = overlay_man();
        let overlays = man.overlays.write().unwrap();
        std::thread::scope(|s| {
            let clear = s.spawn(|| man.clear_skybox());
            std::thread::sleep(std::time::Duration::from_millis(100));
            let skybox_free = man.skybox.try_write().is_ok();
            drop(overlays);
            clear.join().unwrap();
            assert!(skybox_free, "skybox locked while waiting for the overlays");
        });
    }

    /// An overlay manager connected to a compositor, so overlay textures can set up the real
    /// session.
    struct RealSessionFixture {
        man: Arc<OverlayMan>,
        _comp: Arc<Compositor>,
        xr: Arc<OpenXrData<Compositor>>,
        // Session must be dropped before Vulkan data.
        vk: Arc<crate::graphics_backends::VulkanData>,
    }

    impl RealSessionFixture {
        fn new() -> Self {
            Self::with_app_type(vr::EVRApplicationType::Other)
        }

        fn with_app_type(app_type: vr::EVRApplicationType) -> Self {
            Self::build(app_type, |_| {})
        }

        /// fakexr doesn't check layers, so tests can pretend layer extensions are enabled.
        fn with_extensions(enable: impl FnOnce(&mut xr::ExtensionSet)) -> Self {
            Self::build(vr::EVRApplicationType::Other, enable)
        }

        fn build(
            app_type: vr::EVRApplicationType,
            enable: impl FnOnce(&mut xr::ExtensionSet),
        ) -> Self {
            let mut xr = OpenXrData::new(&Injector::default(), app_type).unwrap();
            enable(&mut xr.enabled_extensions);
            let xr = Arc::new(xr);
            let vk = Arc::new(crate::graphics_backends::VulkanData::new_temporary(
                &xr.instance,
                xr.system_id,
            ));
            let comp = Arc::new(Compositor::new(xr.clone(), &Injector::default()));
            xr.compositor.set(Arc::downgrade(&comp));
            let man = Arc::new(OverlayMan::new(xr.clone(), &Injector::default()));
            man.compositor.set(Arc::downgrade(&comp));
            Self {
                man,
                _comp: comp,
                xr,
                vk,
            }
        }

        fn texture(&self) -> vr::Texture_t {
            crate::compositor::FakeGraphicsData::texture(&self.vk)
        }

        /// Creates an overlay that is shown and has a texture.
        fn textured_overlay(&self, key: &CStr) -> vr::VROverlayHandle_t {
            let handle = create(&self.man, key, key);
            assert_eq!(
                self.man.SetOverlayTexture(handle, &self.texture()),
                vr::EVROverlayError::None
            );
            assert_eq!(self.man.ShowOverlay(handle), vr::EVROverlayError::None);
            handle
        }

        fn layers(&self, render_skybox: bool) -> Vec<TestLayer> {
            type Api = <crate::compositor::FakeGraphicsData as GraphicsBackend>::Api;
            let session = self.xr.session_data.get();
            self.man
                .get_layers::<Api>(&session, render_skybox)
                .iter()
                .map(|layer| {
                    let color_scale = layer.color_bias_khr.as_ref().map(|c| c.color_scale);
                    match layer.layer.as_ref().unwrap() {
                        OverlayLayerInner::Quad(l) => TestLayer::Quad(*l.as_raw(), color_scale),
                        OverlayLayerInner::Cylinder(l) => {
                            TestLayer::Cylinder(*l.as_raw(), color_scale)
                        }
                        OverlayLayerInner::Equirect2(l) => {
                            TestLayer::Equirect2(*l.as_raw(), color_scale)
                        }
                    }
                })
                .collect()
        }

        /// Reports whether the overlay locks were free when the session got restarted.
        fn overlay_locks_free_on_restart(&self) -> std::sync::mpsc::Receiver<bool> {
            let (tx, rx) = std::sync::mpsc::channel();
            let weak = Arc::downgrade(&self.man);
            fakexr::on_next_end(self.xr.session_data.get().session.as_raw(), move || {
                let man = weak.upgrade().unwrap();
                let free = man.overlays.try_read().is_ok() && man.skybox.try_read().is_ok();
                tx.send(free).unwrap();
            });
            rx
        }
    }

    /// The parts of an overlay layer the tests look at, along with its color scale.
    #[derive(Debug)]
    enum TestLayer {
        Quad(xr::sys::CompositionLayerQuad, Option<xr::Color4f>),
        Cylinder(xr::sys::CompositionLayerCylinderKHR, Option<xr::Color4f>),
        Equirect2(xr::sys::CompositionLayerEquirect2KHR, Option<xr::Color4f>),
    }

    impl TestLayer {
        fn flags(&self) -> xr::CompositionLayerFlags {
            match self {
                Self::Quad(l, _) => l.layer_flags,
                Self::Cylinder(l, _) => l.layer_flags,
                Self::Equirect2(l, _) => l.layer_flags,
            }
        }

        fn color_scale(&self) -> Option<[f32; 4]> {
            let (Self::Quad(_, c) | Self::Cylinder(_, c) | Self::Equirect2(_, c)) = self;
            c.map(|c| [c.r, c.g, c.b, c.a])
        }
    }

    #[test]
    fn layer_alpha_follows_overlay_flags() {
        use vr::VROverlayFlags as OverlayFlags;
        use xr::CompositionLayerFlags as Flags;

        let f = RealSessionFixture::with_extensions(|e| {
            e.khr_composition_layer_color_scale_bias = true
        });
        let handle = f.textured_overlay(c"key");
        let set_flag = |flag, value| {
            assert_eq!(
                f.man.SetOverlayFlag(handle, flag, value),
                vr::EVROverlayError::None
            );
        };
        let layer = || {
            let mut layers = f.layers(false);
            assert_eq!(layers.len(), 1);
            layers.pop().unwrap()
        };

        // OpenVR overlays have straight alpha by default.
        assert_eq!(
            layer().flags(),
            Flags::BLEND_TEXTURE_SOURCE_ALPHA | Flags::UNPREMULTIPLIED_ALPHA
        );
        assert_eq!(
            f.man.SetOverlayAlpha(handle, 0.5),
            vr::EVROverlayError::None
        );
        assert_eq!(layer().color_scale(), Some([1.0, 1.0, 1.0, 0.5]));

        set_flag(OverlayFlags::IsPremultiplied, true);
        assert_eq!(layer().flags(), Flags::BLEND_TEXTURE_SOURCE_ALPHA);
        assert_eq!(layer().color_scale(), Some([0.5; 4]));

        set_flag(OverlayFlags::IsPremultiplied, false);
        set_flag(OverlayFlags::IgnoreTextureAlpha, true);
        assert_eq!(layer().flags(), Flags::UNPREMULTIPLIED_ALPHA);
    }

    #[test]
    fn curvature_follows_openvr() {
        let f = RealSessionFixture::with_extensions(|e| e.khr_composition_layer_cylinder = true);
        let handle = f.textured_overlay(c"key");
        assert_eq!(
            f.man.SetOverlayWidthInMeters(handle, 4.0),
            vr::EVROverlayError::None
        );
        let cylinder = |curvature| {
            assert_eq!(
                f.man.SetOverlayCurvature(handle, curvature),
                vr::EVROverlayError::None
            );
            match f.layers(false).as_slice() {
                [TestLayer::Cylinder(c, _)] => *c,
                other => panic!("expected one cylinder, got {other:?}"),
            }
        };

        // curvature = width / (2π * radius), whatever the distance to the overlay.
        let c = cylinder(0.1);
        assert!((c.radius - 4.0 / (TAU * 0.1)).abs() < 1e-3, "{}", c.radius);
        assert!((c.central_angle - TAU * 0.1).abs() < 1e-5);
        assert_eq!(c.aspect_ratio, 4.0 / 4.0);
        // The middle of the arc stays where the overlay is.
        assert!((c.pose.position.z - (c.radius - 0.5)).abs() < 1e-4);

        // Monado rejects angles above 2π as a double, which f32 2π is.
        let c = cylinder(1.0);
        assert!(f64::from(c.central_angle) < std::f64::consts::TAU);
        assert!((c.radius * c.central_angle - 4.0).abs() < 1e-4);

        // Flat again
        assert_eq!(
            f.man.SetOverlayCurvature(handle, 0.0),
            vr::EVROverlayError::None
        );
        assert!(matches!(f.layers(false).as_slice(), [TestLayer::Quad(..)]));
    }

    #[test]
    fn empty_overlay_texture_is_rejected() {
        use crate::compositor::FakeGraphicsData;
        let f = RealSessionFixture::new();
        let shown = f.textured_overlay(c"shown");
        let new = create(&f.man, c"new", c"new");

        FakeGraphicsData::set_texture_size(0, 10);
        for handle in [shown, new] {
            assert_eq!(
                f.man.SetOverlayTexture(handle, &f.texture()),
                vr::EVROverlayError::InvalidTexture
            );
        }
        FakeGraphicsData::set_texture_size(10, 10);
        // The previous texture stays up.
        assert_eq!(f.layers(false).len(), 1);
    }

    /// A session restart takes the overlay swapchains with it. Overlays come back once they get a
    /// texture again, since the app's old textures may be gone by now.
    #[test]
    fn overlays_across_session_restart() {
        let f = RealSessionFixture::new();
        let first = f.textured_overlay(c"first");
        let second = f.textured_overlay(c"second");
        assert_eq!(f.layers(false).len(), 2);

        f.xr.restart_session().unwrap();
        assert_eq!(f.layers(false).len(), 0);
        for (overlay, count) in [(first, 1), (second, 2)] {
            assert_eq!(
                f.man.SetOverlayTexture(overlay, &f.texture()),
                vr::EVROverlayError::None
            );
            assert_eq!(f.layers(false).len(), count);
        }
    }

    #[test]
    fn removed_overlays_release_swapchains() {
        let f = RealSessionFixture::new();
        let cleared = f.textured_overlay(c"cleared");
        let destroyed = f.textured_overlay(c"destroyed");
        assert_eq!(f.man.set_skybox(&[f.texture()]), Ok(()));
        let swapchains = || f.xr.session_data.get().overlay_data.swapchain_count();
        assert_eq!(swapchains(), 3);

        // A cleared overlay keeps its swapchain for its next texture.
        assert_eq!(
            f.man.ClearOverlayTexture(cleared),
            vr::EVROverlayError::None
        );
        assert_eq!(swapchains(), 3);
        assert_eq!(f.layers(true).len(), 2);
        assert_eq!(f.man.DestroyOverlay(destroyed), vr::EVROverlayError::None);
        assert_eq!(swapchains(), 2);
        f.man.clear_skybox();
        assert_eq!(swapchains(), 1);
        assert_eq!(f.layers(true).len(), 0);

        assert_eq!(
            f.man.SetOverlayTexture(cleared, &f.texture()),
            vr::EVROverlayError::None
        );
        assert_eq!(swapchains(), 1);
        assert_eq!(f.layers(true).len(), 1);
    }

    #[test]
    fn skybox_layer_types() {
        let cylinder_and_equirect = |e: &mut xr::ExtensionSet| {
            e.khr_composition_layer_cylinder = true;
            e.khr_composition_layer_equirect2 = true;
        };

        // A lat-long panorama, infinitely far away all around the user
        let f = RealSessionFixture::with_extensions(cylinder_and_equirect);
        assert_eq!(f.man.set_skybox(&[f.texture()]), Ok(()));
        let layers = f.layers(true);
        let [TestLayer::Equirect2(layer, _)] = layers.as_slice() else {
            panic!("expected one equirect layer, got {layers:?}");
        };
        assert_eq!(layer.eye_visibility, xr::EyeVisibility::BOTH);
        assert_eq!(layer.radius, 0.0);
        assert_eq!(layer.central_horizontal_angle, TAU);
        assert_eq!(
            (layer.upper_vertical_angle, layer.lower_vertical_angle),
            (FRAC_PI_2, -FRAC_PI_2)
        );
        // Opaque
        assert!(
            !layer
                .layer_flags
                .contains(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
        );

        // A stereo pair
        assert_eq!(f.man.set_skybox(&[f.texture(), f.texture()]), Ok(()));
        let eyes: Vec<_> = f
            .layers(true)
            .iter()
            .map(|layer| match layer {
                TestLayer::Equirect2(l, _) => l.eye_visibility,
                other => panic!("expected an equirect layer, got {other:?}"),
            })
            .collect();
        assert_eq!(eyes.len(), 2);
        assert!(
            eyes.contains(&xr::EyeVisibility::LEFT) && eyes.contains(&xr::EyeVisibility::RIGHT)
        );

        // A cubemap only shows its front face.
        let cubemap = [f.texture(); 6];
        assert_eq!(f.man.set_skybox(&cubemap), Ok(()));
        assert!(matches!(
            f.layers(true).as_slice(),
            [TestLayer::Cylinder(..)]
        ));

        // Runtimes without these layers get a quad.
        let f = RealSessionFixture::new();
        assert_eq!(f.man.set_skybox(&[f.texture()]), Ok(()));
        assert!(matches!(f.layers(true).as_slice(), [TestLayer::Quad(..)]));
    }

    #[test]
    fn invalid_overlay_placement_is_skipped() {
        let f = RealSessionFixture::new();
        let handle = f.textured_overlay(c"key");
        assert_eq!(f.layers(false).len(), 1);

        assert_eq!(
            f.man.SetOverlayWidthInMeters(handle, 0.0),
            vr::EVROverlayError::None
        );
        assert_eq!(f.layers(false).len(), 0);

        assert_eq!(
            f.man.SetOverlayWidthInMeters(handle, 1.0),
            vr::EVROverlayError::None
        );
        let mut transform: vr::HmdMatrix34_t = xr::Posef::IDENTITY.into();
        transform.m[0][3] = f32::NAN;
        assert_eq!(
            f.man.SetOverlayTransformAbsolute(
                handle,
                vr::ETrackingUniverseOrigin::Standing,
                &transform
            ),
            vr::EVROverlayError::None
        );
        assert_eq!(f.layers(false).len(), 0);
    }

    /// The frame thread takes the overlay lock while holding the session lock, so the first
    /// overlay texture must not move to the real session while holding the overlay lock.
    #[test]
    fn overlay_texture_restarts_session_without_overlay_lock() {
        let f = RealSessionFixture::new();
        let handle = create(&f.man, c"key", c"name");

        let unlocked = f.overlay_locks_free_on_restart();
        assert_eq!(
            f.man.SetOverlayTexture(handle, &f.texture()),
            vr::EVROverlayError::None
        );
        let unlocked = unlocked.try_recv().expect("session was not restarted");
        assert!(unlocked, "overlay lock held while restarting the session");
    }

    #[test]
    fn skybox_restarts_session_without_overlay_lock() {
        let f = RealSessionFixture::new();

        let unlocked = f.overlay_locks_free_on_restart();
        assert_eq!(f.man.set_skybox(&[f.texture()]), Ok(()));
        let unlocked = unlocked.try_recv().expect("session was not restarted");
        assert!(unlocked, "overlay locks held while restarting the session");
        assert_eq!(f.man.skybox.read().unwrap().len(), 1);
    }

    /// Headless sessions can't create swapchains, which used to panic, and moving a Background
    /// app to a graphics session would take the display from the game.
    #[test]
    fn overlay_texture_from_non_scene_app_fails() {
        for headless in [true, false] {
            fakexr::set_headless_supported(headless);
            let f = RealSessionFixture::with_app_type(vr::EVRApplicationType::Background);
            fakexr::set_headless_supported(false);
            assert_eq!(f.xr.headless, headless);
            let handle = create(&f.man, c"key", c"name");

            let session = f.xr.session_data.get().session.as_raw();
            assert_eq!(
                f.man.SetOverlayTexture(handle, &f.texture()),
                vr::EVROverlayError::RequestFailed
            );
            assert_eq!(f.man.set_skybox(&[f.texture()]), Ok(()));
            assert!(f.man.skybox.read().unwrap().is_empty());
            assert_eq!(f.xr.session_data.get().session.as_raw(), session);
        }
    }

    /// Companion apps (VRCX) set overlay textures without ever requesting IVRCompositor, which
    /// used to panic.
    #[test]
    fn overlay_texture_without_compositor_fails() {
        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let vk = Arc::new(crate::graphics_backends::VulkanData::new_temporary(
            &xr.instance,
            xr.system_id,
        ));
        let man = OverlayMan::new(xr.clone(), &Injector::default());
        let handle = create(&man, c"key", c"name");

        let session = xr.session_data.get().session.as_raw();
        assert_eq!(
            man.SetOverlayTexture(handle, &crate::compositor::FakeGraphicsData::texture(&vk)),
            vr::EVROverlayError::RequestFailed
        );
        assert_eq!(xr.session_data.get().session.as_raw(), session);
        drop(man);
        drop(xr);
    }

    #[test]
    fn overlay_texture_color_space() {
        let f = RealSessionFixture::new();
        let handle = create(&f.man, c"key", c"name");
        let mut color_space = vr::EColorSpace::Linear;
        assert_eq!(
            f.man.GetOverlayTextureColorSpace(handle, &mut color_space),
            vr::EVROverlayError::None
        );
        assert_eq!(color_space, vr::EColorSpace::Auto);

        assert_eq!(
            f.man
                .SetOverlayTextureColorSpace(handle, vr::EColorSpace::Linear),
            vr::EVROverlayError::None
        );
        assert_eq!(
            f.man.GetOverlayTextureColorSpace(handle, &mut color_space),
            vr::EVROverlayError::None
        );
        assert_eq!(color_space, vr::EColorSpace::Linear);
        assert_eq!(
            f.man
                .SetOverlayTextureColorSpace(handle + 1, vr::EColorSpace::Gamma),
            vr::EVROverlayError::UnknownOverlay
        );

        // It applies to textures that leave the colour space on Auto.
        let copied_color_space = |texture| {
            assert_eq!(
                f.man.SetOverlayTexture(handle, &texture),
                vr::EVROverlayError::None
            );
            crate::compositor::FakeGraphicsData::overlay_color_space()
        };
        assert_eq!(copied_color_space(f.texture()), vr::EColorSpace::Linear);
        let gamma = vr::Texture_t {
            eColorSpace: vr::EColorSpace::Gamma,
            ..f.texture()
        };
        assert_eq!(copied_color_space(gamma), vr::EColorSpace::Gamma);
    }

    #[test]
    fn keyboard_for_overlay_queues_overlay_event() {
        let man = overlay_man();
        let handle = create(&man, c"key", c"name");
        let other = create(&man, c"other", c"other");
        assert_eq!(
            man.ShowKeyboardForOverlay(
                handle,
                vr::EGamepadTextInputMode::Normal,
                vr::EGamepadTextInputLineMode::SingleLine,
                0,
                c"desc".as_ptr(),
                32,
                std::ptr::null(),
                5,
            ),
            vr::EVROverlayError::None
        );

        let size = std::mem::size_of::<vr::VREvent_t>() as u32;
        let mut event: vr::VREvent_t = unsafe { std::mem::zeroed() };
        assert!(!man.PollNextOverlayEvent(other, &mut event, size));
        for ty in [
            vr::EVREventType::KeyboardDone,
            vr::EVREventType::KeyboardClosed,
        ] {
            assert!(man.PollNextOverlayEvent(handle, &mut event, size));
            keyboard_event(ty, 5)(&event);
            assert_eq!(unsafe { event.data.keyboard.overlayHandle }, handle);
        }
        assert!(!man.PollNextOverlayEvent(handle, &mut event, size));

        // Without a Done key, the keyboard can only be closed.
        assert_eq!(
            man.ShowKeyboardForOverlay(
                handle,
                vr::EGamepadTextInputMode::Normal,
                vr::EGamepadTextInputLineMode::SingleLine,
                vr::EKeyboardFlags::HideDoneKey.0,
                c"desc".as_ptr(),
                32,
                std::ptr::null(),
                6,
            ),
            vr::EVROverlayError::None
        );
        assert!(man.PollNextOverlayEvent(handle, &mut event, size));
        keyboard_event(vr::EVREventType::KeyboardClosed, 6)(&event);
        assert!(!man.PollNextOverlayEvent(handle, &mut event, size));

        let mut buf = [1 as c_char; 4];
        assert_eq!(man.GetKeyboardText(buf.as_mut_ptr(), buf.len() as u32), 1);
        assert_eq!(buf[0], 0);

        assert_eq!(
            man.ShowKeyboardForOverlay(
                vr::k_ulOverlayHandleInvalid,
                vr::EGamepadTextInputMode::Normal,
                vr::EGamepadTextInputLineMode::SingleLine,
                0,
                std::ptr::null(),
                32,
                std::ptr::null(),
                0,
            ),
            vr::EVROverlayError::UnknownOverlay
        );
    }
}
