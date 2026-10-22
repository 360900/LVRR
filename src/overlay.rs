use crate::{
    clientcore::{Injected, Injector},
    compositor::{Compositor, is_usable_swapchain},
    graphics_backends::{GraphicsBackend, SupportedBackend, supported_apis_enum},
    openxr_data::{GraphicalSession, OpenXrData, Session, SessionData, SessionGuard},
    system::System,
};
use glam::{Quat, Vec3, vec3};
use log::{debug, trace};
use openvr as vr;
use openxr as xr;
use slotmap::{Key, KeyData, SecondaryMap, SlotMap, new_key_type};
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
    028, 027, 026, 025, 024, 022, 021, 020, 019, 018, 017, 016, 014, 013, 012, 011, 010, 007
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

        if !self.openxr.session_data.get().is_real_session()
            && self
                .compositor
                .get()
                .expect("Need to restart session, but compositor hasn't been set up...")
                .initialize_real_session(texture, bounds)
                .is_err()
        {
            Err(vr::EVROverlayError::InvalidTexture)
        } else {
            Ok(RealSessionData(self.openxr.session_data.get()))
        }
    }

    pub fn set_skybox(&self, textures: &[vr::Texture_t]) -> Result<(), vr::EVRCompositorError> {
        // Port of OpenComposite's SetSkyboxOverride behaviour: submit the first
        // provided texture as a flat quad in front of the user. This is designed
        // around rFactor2/GTR2 style loading screens where the "skybox" is really
        // just a static image the game expects to be displayed while it prepares
        // the scene. Handles both the 1..=2 (equirect) and 6 (cubemap) counts by
        // simply taking pTextures[0] in either case.
        let Some(texture) = textures.first() else {
            return Err(vr::EVRCompositorError::RequestFailed);
        };

        self.clear_skybox();

        // Skybox faces come in with V flipped compared to normal overlays.
        let bounds = vr::VRTextureBounds_t {
            uMin: 0.0,
            uMax: 1.0,
            vMin: 1.0,
            vMax: 0.0,
        };
        // This may restart the session, so it has to happen before taking the overlay locks: the
        // frame thread takes those while holding the session lock.
        let Ok(session_data) = self.get_real_session_data(texture, bounds) else {
            log::warn!("Could not create swapchain for skybox texture; skipping");
            return Ok(());
        };

        let mut overlays = self.overlays.write().unwrap();
        let mut skybox = self.skybox.write().unwrap();

        let name = CString::new("__xrizer_skybox").unwrap();
        let key = overlays.insert(Overlay::new(name.clone(), name));
        let overlay = overlays.get_mut(key).unwrap();

        overlay.kind = OverlayKind::Curved { curvature: 1.0 };
        overlay.z_order = SKYBOX_Z_ORDER;
        overlay.scale = (1.0, 1.0);
        overlay.bounds = bounds;
        // Wide enough to fill most of the forward view; height derived from
        // texture aspect. Distance is set via the transform below and used as
        // the cylinder radius (see get_layers).
        overlay.width = 4.0;
        overlay.visible = true;
        // Curved surface a couple of meters in front of the user. The distance
        // becomes the cylinder radius, so the "forward" texture wraps around
        // the user's head rather than sitting on a flat plane.
        overlay.transform = Some((
            vr::ETrackingUniverseOrigin::Seated,
            vr::HmdMatrix34_t {
                m: [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, -2.0],
                ],
            },
        ));

        match overlay.set_texture(key, session_data, *texture) {
            Ok(()) => {
                skybox.push(key);
                Ok(())
            }
            Err(_) => {
                overlays.remove(key);
                log::warn!("Could not create swapchain for skybox texture; skipping");
                // Don't fail the game — just silently drop the skybox request.
                Ok(())
            }
        }
    }

    pub fn clear_skybox(&self) {
        let mut overlays = self.overlays.write().unwrap();
        let mut skybox = self.skybox.write().unwrap();
        for key in skybox.drain(..) {
            overlays.remove(key);
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
        let mut overlays = self.overlays.write().unwrap();
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
        for (key, overlay) in overlays.iter_mut() {
            if !overlay.visible {
                continue;
            }
            // Skybox is always shown (independent of app_fade_grid) so games that
            // never enable the fade grid still get their loading-screen image.
            let _ = render_skybox;
            let Some(rect) = overlay.rect else {
                continue;
            };

            // The rect can outlive the swapchain across a session restart.
            let Some(SwapchainData { swapchain, .. }) = swapchains.get(key) else {
                continue;
            };
            let space = session.get_space_for_origin(
                overlay
                    .transform
                    .as_ref()
                    .map(|(o, _)| *o)
                    .unwrap_or(session.current_origin),
            );

            trace!("overlay rect: {rect:#?}");

            let pose = overlay
                .transform
                .as_ref()
                .map(|(_, t)| (*t).into())
                .unwrap_or(xr::Posef {
                    position: xr::Vector3f {
                        x: 0.0,
                        y: 0.0,
                        z: -0.5,
                    },
                    orientation: xr::Quaternionf::IDENTITY,
                });

            macro_rules! layer_init {
                ($ty:ident) => {{
                    $ty::new()
                        .space(space)
                        .layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
                        .eye_visibility(xr::EyeVisibility::BOTH)
                        .sub_image(
                            xr::SwapchainSubImage::new()
                                .image_array_index(vr::EVREye::Left as u32)
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

            match overlay.kind {
                OverlayKind::Quad => {
                    use xr::CompositionLayerQuad;
                    // OC-style size: width = widthMeters * scaleX,
                    //                height = widthMeters * scaleY / aspect
                    let aspect = if rect.extent.height > 0 {
                        rect.extent.width as f32 / rect.extent.height as f32
                    } else {
                        1.0
                    };
                    let quad_w = overlay.width * overlay.scale.0;
                    let quad_h = overlay.width * overlay.scale.1 / aspect;
                    let layer = layer_init!(CompositionLayerQuad)
                        .pose(pose)
                        .size(xr::Extent2Df {
                            width: quad_w,
                            height: quad_h,
                        });

                    let layer = lifetime_extend!(CompositionLayerQuad, layer);
                    let mut layer = OverlayLayer::from(OverlayLayerInner::Quad(layer));
                    overlay.alpha.iter().for_each(|a| layer.set_alpha(*a));
                    layers.push((overlay.z_order, layer));
                }
                // SetOverlayCurvature checks for khr_composition_layer_cylinder
                OverlayKind::Curved { curvature: _ } => {
                    // OpenVR curvature semantics don't map cleanly to OpenXR's
                    // cylinder radius. Instead, curve the overlay around the
                    // user's origin in the overlay's reference space: use the
                    // distance from origin to the requested pose position as
                    // the cylinder radius. This produces a natural "curved
                    // screen" effect wrapping around the user's head.
                    let width = overlay.width * overlay.scale.0;
                    let pos = vec3(pose.position.x, pose.position.y, pose.position.z);
                    let distance = pos.length().max(0.01);
                    let radius = distance;
                    let rot = Quat::from_xyzw(
                        pose.orientation.x,
                        pose.orientation.y,
                        pose.orientation.z,
                        pose.orientation.w,
                    );

                    let center = pos + rot.mul_vec3(Vec3::Z * radius);
                    let angle = 2.0 * (width / (2.0 * radius));

                    use xr::CompositionLayerCylinderKHR;
                    let layer = layer_init!(CompositionLayerCylinderKHR)
                        .radius(radius)
                        .central_angle(angle)
                        .aspect_ratio(rect.extent.width as f32 / rect.extent.height as f32)
                        .pose(xr::Posef {
                            orientation: pose.orientation,
                            position: xr::Vector3f {
                                x: center.x,
                                y: center.y,
                                z: center.z,
                            },
                        });

                    let layer = lifetime_extend!(CompositionLayerCylinderKHR, layer);
                    let mut layer = OverlayLayer::from(OverlayLayerInner::Cylinder(layer));
                    overlay.alpha.iter().for_each(|a| layer.set_alpha(*a));
                    layers.push((overlay.z_order, layer));
                }
            }
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

    /// We have no keyboard, so report it as done immediately with the existing text unchanged.
    /// This lets games that wait for the keyboard continue instead of hanging.
    fn show_keyboard(
        &self,
        overlay: Option<vr::VROverlayHandle_t>,
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

        let event = vr::VREvent_t {
            eventType: vr::EVREventType::KeyboardDone as u32,
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

        if let Some(handle) = overlay {
            let mut overlays = self.overlays.write().unwrap();
            let Some(overlay) = overlays.get_mut(OverlayKey::from(KeyData::from_ffi(handle)))
            else {
                return vr::EVROverlayError::UnknownOverlay;
            };
            overlay.events.push_back(event);
        } else {
            self.system
                .force(|i| System::new(self.openxr.clone(), i))
                .push_event(event);
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
    fn set_alpha(&mut self, alpha: f32) {
        // only one instance is stored, so this would cause segfault due to UAF
        debug_assert!(
            self.color_bias_khr.is_none(),
            "attempted to set_alpha on the same CompositorLayer twice!"
        );

        self.color_bias_khr = {
            let mut payload = Box::new(xr::sys::CompositionLayerColorScaleBiasKHR {
                ty: xr::StructureType::COMPOSITION_LAYER_COLOR_SCALE_BIAS_KHR,
                next: std::ptr::null(),
                color_bias: Default::default(),
                color_scale: xr::Color4f {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
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
    // Skybox (reserved for KHR_composition_layer_equirect2)
    #[allow(dead_code)]
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

/// The X and Y scale of an overlay transform: the lengths of its basis vectors. The diagonal alone
/// would shrink rotated overlays.
fn transform_scale(transform: &vr::HmdMatrix34_t) -> (f32, f32) {
    let m = &transform.m;
    let column = |c: usize| Vec3::new(m[0][c], m[1][c], m[2][c]).length();
    (column(0), column(1))
}

enum OverlayKind {
    Quad,
    Curved { curvature: f32 },
}

struct Overlay {
    key: CString,
    name: CString,
    /// Only allowed to be Some if KHR_composition_layer_color_scale_bias is active
    alpha: Option<f32>,
    width: f32,
    /// Scale factors from the transform matrix diagonal (m[0][0], m[1][1]).
    /// Applied to the quad size like OpenComposite does, so games that encode
    /// non-unit scale in the transform get the size they expect.
    scale: (f32, f32),
    visible: bool,
    kind: OverlayKind,
    z_order: i64,
    bounds: vr::VRTextureBounds_t,
    transform: Option<(vr::ETrackingUniverseOrigin, vr::HmdMatrix34_t)>,
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
            transform: None,
            compositor: None,
            rect: None,
            flags: 0,
            events: VecDeque::new(),
        }
    }

    fn set_texture(
        &mut self,
        key: OverlayKey,
        session_data: RealSessionData<'_>,
        texture: vr::Texture_t,
    ) -> Result<(), vr::EVROverlayError> {
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
            let mut tex_swapchain_info =
                backend.swapchain_info_for_texture(b_texture, texture_bounds, texture.eColorSpace);
            // OpenComposite creates overlay swapchains with arraySize=1; some runtimes (e.g. Monado)
            // do not render quad layers correctly when a multi-array swapchain is used.
            tex_swapchain_info.array_size = 1;
            let mut create_swapchain = || {
                let mut info = backend.swapchain_info_for_texture(
                    b_texture,
                    texture_bounds,
                    texture.eColorSpace,
                );
                info.array_size = 1;
                let initial_format = info.format;
                session_data.check_format::<G>(&mut info);
                let swapchain = session_data.create_swapchain(&info).unwrap();
                let images = swapchain
                    .enumerate_images()
                    .expect("Couldn't enumerate swapchain images");
                backend.store_swapchain_images(images, G::from_openxr_format(info.format));
                SwapchainData {
                    swapchain,
                    info,
                    initial_format,
                }
            };
            let swapchain = {
                let data = map
                    .entry(key)
                    .unwrap()
                    .or_insert_with(&mut create_swapchain);
                if !is_usable_swapchain(
                    &data.info,
                    G::from_openxr_format(data.initial_format),
                    &tex_swapchain_info,
                    G::from_openxr_format(tex_swapchain_info.format),
                ) {
                    *data = create_swapchain();
                }
                &mut data.swapchain
            };
            let idx = swapchain.acquire_image().unwrap();
            swapchain.wait_image(xr::Duration::INFINITE).unwrap();

            let extent = backend.copy_overlay_to_swapchain(b_texture, texture_bounds, idx as usize);
            swapchain.release_image().unwrap();

            Ok(extent)
        }

        let backend = self.compositor.as_mut().unwrap();
        let extent = backend.with_any_graphics_mut::<set_swapchain_texture>((
            &session_data,
            self.bounds,
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

    pub fn set_flag(&mut self, flag: vr::VROverlayFlags, value: bool) {
        debug!(
            "setting overlay flag {flag:?} to {value} on {:?}",
            self.name
        );
        self.flags = if value {
            self.flags | flag as u32
        } else {
            self.flags & !(flag as u32)
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
        let key = unsafe { CStr::from_ptr(key) };
        let name = unsafe { CStr::from_ptr(name) };

        if handle.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }

        let mut overlays = self.overlays.write().unwrap();
        let ret_key = overlays.insert(Overlay::new(key.into(), name.into()));
        let mut key_to_overlay = self.key_to_overlay.write().unwrap();
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
        if handle.is_null() {
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
        match overlay.set_texture(key, data, texture) {
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
        _: u32,
        _: *const c_char,
        _: u32,
        existing_text: *const c_char,
        user_value: u64,
    ) -> vr::EVROverlayError {
        self.show_keyboard(Some(handle), existing_text, user_value)
    }
    fn ShowKeyboard(
        &self,
        _: vr::EGamepadTextInputMode,
        _: vr::EGamepadTextInputLineMode,
        _: u32,
        _: *const c_char,
        _: u32,
        existing_text: *const c_char,
        user_value: u64,
    ) -> vr::EVROverlayError {
        self.show_keyboard(None, existing_text, user_value)
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
        _: vr::VROverlayHandle_t,
        _: *mut vr::TrackedDeviceIndex_t,
        _: *mut c_char,
        _: u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTransformTrackedDeviceComponent");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayTransformTrackedDeviceComponent(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::TrackedDeviceIndex_t,
        _: *const c_char,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTransformTrackedDeviceComponent");
        vr::EVROverlayError::RequestFailed
    }
    fn GetOverlayTransformTrackedDeviceRelative(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::TrackedDeviceIndex_t,
        _: *mut vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTransformTrackedDeviceRelative");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayTransformTrackedDeviceRelative(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::TrackedDeviceIndex_t,
        _: *const vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTransformTrackedDeviceRelative");
        vr::EVROverlayError::None
    }
    fn GetOverlayTransformAbsolute(
        &self,
        handle: vr::VROverlayHandle_t,
        out_origin: *mut vr::ETrackingUniverseOrigin,
        out_transform: *mut vr::HmdMatrix34_t,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, overlay);
        if out_origin.is_null() || out_transform.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        // relative transforms are unimplemented, so stored transform is always absolute
        let (origin, transform) = if let Some((origin, transform)) = overlay.transform {
            (origin, transform)
        } else {
            // if none, report the identity in the current tracking space
            (self.openxr.get_tracking_space(), xr::Posef::IDENTITY.into())
        };
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
            vr::EVROverlayError::InvalidParameter
        } else {
            let transform = unsafe { transform.read() };
            // Preserve the scale before decomposing to a pose which discards it.
            let scale = transform_scale(&transform);
            let xr_transform: xr::Posef = transform.into();
            let o = xr_transform.orientation;
            let q = Quat::from_xyzw(o.x, o.y, o.z, o.w).normalize();
            let transform = xr::Posef {
                position: xr_transform.position,
                orientation: xr::Quaternionf {
                    x: q.x,
                    y: q.y,
                    z: q.z,
                    w: q.w,
                },
            };
            overlay.scale = scale;
            overlay.transform = Some((origin, transform.into()));
            debug!(
                "set overlay transform origin to {origin:?} for {:?} ({transform:?})",
                overlay.name
            );
            vr::EVROverlayError::None
        }
    }
    fn GetOverlayTransformType(
        &self,
        handle: vr::VROverlayHandle_t,
        ty: *mut vr::VROverlayTransformType,
    ) -> vr::EVROverlayError {
        get_overlay!(self, handle, _overlay);
        if ty.is_null() {
            return vr::EVROverlayError::InvalidParameter;
        }
        // relative transforms are unimplemented, so stored transform is always absolute
        unsafe { ty.write(vr::VROverlayTransformType::Absolute) };
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
        _: vr::VROverlayHandle_t,
        _: *mut vr::EColorSpace,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("GetOverlayTextureColorSpace");
        vr::EVROverlayError::RequestFailed
    }
    fn SetOverlayTextureColorSpace(
        &self,
        _: vr::VROverlayHandle_t,
        _: vr::EColorSpace,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("SetOverlayTextureColorSpace");
        vr::EVROverlayError::RequestFailed
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
            if value <= f32::EPSILON {
                overlay.kind = OverlayKind::Quad;
            } else {
                overlay.kind = OverlayKind::Curved {
                    curvature: value.clamp(0.0, 1.0),
                };
            }
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
        unsafe { enabled.write(overlay.flags & flag as u32 != 0) };
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

        let mut overlays = self.overlays.write().unwrap();
        if let Some(overlay) = overlays.remove(key) {
            let mut map = self.key_to_overlay.write().unwrap();
            map.remove(&overlay.key);
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

impl vr::IVROverlay007On010 for OverlayMan {
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
        assert_eq!(flags, flag as u32);

        assert_eq!(
            man.GetOverlayFlags(handle + 1, &mut flags),
            vr::EVROverlayError::UnknownOverlay
        );
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

    #[test]
    fn rotation_does_not_change_scale() {
        let rot = glam::Mat3::from_rotation_y(60f32.to_radians())
            * glam::Mat3::from_rotation_x(20f32.to_radians())
            * glam::Mat3::from_diagonal(Vec3::new(2.0, 0.5, 1.0));
        let c = rot.to_cols_array_2d();
        let transform = vr::HmdMatrix34_t {
            m: std::array::from_fn(|r| [c[0][r], c[1][r], c[2][r], 1.0]),
        };
        let (x, y) = transform_scale(&transform);
        assert!((x - 2.0).abs() < 1e-5, "{x}");
        assert!((y - 0.5).abs() < 1e-5, "{y}");
    }

    fn keyboard_done_event(user_value: u64) -> impl Fn(&vr::VREvent_t) {
        move |event| {
            assert_eq!(event.eventType, vr::EVREventType::KeyboardDone as u32);
            assert_eq!(unsafe { event.data.keyboard.uUserValue }, user_value);
        }
    }

    #[test]
    fn keyboard_reports_done_with_existing_text() {
        use vr::IVRSystem026_Interface;
        let man = overlay_man();
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

        let system = man.system.get().unwrap();
        let mut event: vr::VREvent_t = unsafe { std::mem::zeroed() };
        assert!(system.PollNextEvent(&mut event, std::mem::size_of::<vr::VREvent_t>() as u32));
        keyboard_done_event(1234)(&event);

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

    /// The frame thread takes the overlay lock while holding the session lock, so the first
    /// overlay texture must not move to the real session while holding the overlay lock.
    #[test]
    fn overlay_texture_restarts_session_without_overlay_lock() {
        use crate::compositor::FakeGraphicsData;
        use crate::graphics_backends::VulkanData;

        let xr =
            Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap());
        let vk = Arc::new(VulkanData::new_temporary(&xr.instance, xr.system_id));
        let comp = Arc::new(Compositor::new(xr.clone(), &Injector::default()));
        xr.compositor.set(Arc::downgrade(&comp));
        let man = Arc::new(OverlayMan::new(xr.clone(), &Injector::default()));
        man.compositor.set(Arc::downgrade(&comp));
        let handle = create(&man, c"key", c"name");

        let (tx, rx) = std::sync::mpsc::channel();
        let weak = Arc::downgrade(&man);
        fakexr::on_next_end(xr.session_data.get().session.as_raw(), move || {
            let man = weak.upgrade().unwrap();
            tx.send(man.overlays.try_read().is_ok()).unwrap();
        });
        assert_eq!(
            man.SetOverlayTexture(handle, &FakeGraphicsData::texture(&vk)),
            vr::EVROverlayError::None
        );
        let unlocked = rx.try_recv().expect("session was not restarted");
        assert!(unlocked, "overlay lock held while restarting the session");

        // Session must be dropped before Vulkan data.
        drop(man);
        drop(comp);
        drop(xr);
        drop(vk);
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
        assert!(man.PollNextOverlayEvent(handle, &mut event, size));
        keyboard_done_event(5)(&event);
        assert_eq!(unsafe { event.data.keyboard.overlayHandle }, handle);
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
