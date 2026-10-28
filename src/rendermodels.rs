mod components;

use std::{
    collections::{BTreeMap, HashMap},
    ffi::{CStr, CString},
    io::Read,
    ptr,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

use glam::{Mat4, Vec3};
use log::{debug, warn};
use lz4_flex::frame::FrameDecoder;
use obj::{IndexTuple, ObjData};
use openvr as vr;
use openxr as xr;

use crate::clientcore::{Injected, Injector};
use crate::compositor::Compositor;
use crate::input::{BoundPoseType, ComponentInput, DynSubpath, Input, ProfileData};
use crate::openxr_data::Hand;

// all render models will use this color
const RENDER_MODEL_COLOR: [u8; 4] = [32, 32, 32, 255];

// use a single static component for all rendermodels
static DEFAULT_COMPONENT: &CStr = c"xrizer_component";

// SteamVR registers these standard pose components for each controller,
// for all render models. The OpenVR wiki on Input Profiles says:
// "All tracked devices also get two pose components registered regardless
// of what render model they use: /pose/raw and /pose/tip"
// Some games (for example, the BigWalkVR mod) mark a controller pose as
// invalid if GetComponentState fails for these components.
static POSE_COMPONENTS: &[(&CStr, BoundPoseType)] = &[
    (c"tip", BoundPoseType::Tip),
    (c"base", BoundPoseType::Base),
    (c"handgrip", BoundPoseType::Handgrip),
    (c"grip", BoundPoseType::Grip),
];

fn pose_component(component_name: &CStr) -> Option<BoundPoseType> {
    POSE_COMPONENTS
        .iter()
        .find(|(name, _)| *name == component_name)
        .map(|(_, ty)| *ty)
}

/// Whether a mesh can be loaded for the model: SteamVR's, if it's installed, or a bundled one.
fn has_model(render_model_name: &CStr) -> bool {
    get_render_model_data(render_model_name).is_some()
        || render_model_name
            .to_str()
            .ok()
            .and_then(steamvr::find)
            .is_some()
}

/// SteamVR's component of the model, if its JSON lists it.
fn steamvr_component(
    render_model_name: &CStr,
    component_name: &CStr,
) -> Option<(std::sync::Arc<components::ModelInfo>, usize)> {
    let info = steamvr::components(render_model_name)?;
    let name = component_name.to_str().ok()?;
    let index = info.components.iter().position(|(n, _)| n == name)?;
    Some((info, index))
}

/// Whether GetComponentState and RenderModelHasComponent know this component.
fn has_component(render_model_name: &CStr, component_name: &CStr) -> bool {
    if steamvr_component(render_model_name, component_name).is_some() {
        true
    } else if component_name == DEFAULT_COMPONENT {
        has_model(render_model_name)
    } else {
        // Models without a mesh, like the hand tracking one, still have the pose components.
        pose_component(component_name).is_some()
            && (has_model(render_model_name)
                || profile_for_render_model(render_model_name).is_some())
    }
}

/// The state of a render model's component, None if the model or component is unknown.
///
/// SteamVR's components move with the controller's input, read through `read` for the
/// component's hand (`hand`, or the one whose profile names the model). The bundled meshes are
/// static with their offsets baked in, so only the pose components have a frame of their own.
pub(crate) fn component_state(
    render_model_name: &CStr,
    component_name: &CStr,
    hand: Option<Hand>,
    read: &dyn Fn(Hand, DynSubpath) -> ComponentInput,
    scroll: bool,
) -> Option<vr::RenderModel_ComponentState_t> {
    if let Some((info, index)) = steamvr_component(render_model_name, component_name) {
        let hand =
            hand.or_else(|| profile_for_render_model(render_model_name).map(|(_, hand)| hand));
        let read_input = |subpath: DynSubpath| {
            let Some(hand) = hand else {
                return ComponentInput::default();
            };
            let input = read(hand, subpath);
            // SteamVR names the left Touch controller's menu button system, which OpenXR calls
            // menu.
            if !input.active && subpath == DynSubpath::System {
                read(hand, DynSubpath::Menu)
            } else {
                input
            }
        };
        return Some(info.components[index].1.state(&read_input, scroll));
    }
    if !has_component(render_model_name, component_name) {
        return None;
    }
    let transform = pose_component(component_name)
        .and_then(|ty| pose_component_transform(render_model_name, ty))
        .map_or(xr::Posef::IDENTITY.into(), hmd_matrix);
    // See components::Component::state for the two transforms.
    Some(vr::RenderModel_ComponentState_t {
        mTrackingToComponentRenderModel: xr::Posef::IDENTITY.into(),
        mTrackingToComponentLocal: transform,
        uProperties: (vr::EVRComponentProperty::IsVisible | vr::EVRComponentProperty::IsStatic).0,
    })
}

/// The transform from the raw pose to a pose component: the profile's transform for that pose
/// type, as for pose actions bound to it.
fn pose_component_transform(render_model_name: &CStr, ty: BoundPoseType) -> Option<Mat4> {
    let (profile, hand) = profile_for_render_model(render_model_name)?;
    profile.pose_transformation(hand, ty)
}

/// The profile and hand that report this render model name.
fn profile_for_render_model(render_model_name: &CStr) -> Option<(ProfileData, Hand)> {
    profile_for_model(model_name(render_model_name)?)
}

fn profile_for_model(name: &str) -> Option<(ProfileData, Hand)> {
    ProfileData::for_render_model(|reported| model_name(reported) == Some(name))
}

/// A fallback mesh was made for the raw pose of the profile it belongs to, so it has to be moved
/// to the raw pose of the profile reporting `render_model_name`.
fn fallback_mesh_transform(render_model_name: &CStr) -> Option<Mat4> {
    let name = model_name(render_model_name)?;
    if RENDER_MODELS.contains_key(name) {
        return None;
    }
    let (profile, hand) = profile_for_model(name)?;
    let (mesh_profile, mesh_hand) = profile_for_model(fallback_model(name)?)?;
    // Raw poses are the grip pose times the profile's hand offset.
    Some(profile.hand_offset(hand).inverse() * mesh_profile.hand_offset(mesh_hand))
}

fn hmd_matrix(mat: Mat4) -> vr::HmdMatrix34_t {
    // OpenVR matrices are row major, glam's are column major.
    let rows = mat.transpose().to_cols_array_2d();
    vr::HmdMatrix34_t {
        m: [rows[0], rows[1], rows[2]],
    }
}

struct RenderModelObjLz4 {
    /// lz4-compressed obj data
    bytes: &'static [u8],
    flipped: bool,
}

static RENDER_MODELS: LazyLock<BTreeMap<&'static str, RenderModelObjLz4>> = LazyLock::new(|| {
    let oculus_quest2_controller_left =
        include_bytes!("../resources/rendermodels/oculus_quest2_controller_left.obj.lz4");
    let oculus_quest_plus_controller_left =
        include_bytes!("../resources/rendermodels/oculus_quest_plus_controller_left.obj.lz4");
    let valve_controller_knu_1_0_left =
        include_bytes!("../resources/rendermodels/valve_controller_knu_1_0_left.obj.lz4");
    let vive_focus3_controller_left =
        include_bytes!("../resources/rendermodels/vive_focus3_controller_left.obj.lz4");

    [
        (
            "generic_controller",
            RenderModelObjLz4 {
                bytes: include_bytes!("../resources/rendermodels/generic_controller.obj.lz4"),
                flipped: false,
            },
        ),
        (
            "oculus_quest2_controller_left",
            RenderModelObjLz4 {
                bytes: oculus_quest2_controller_left,
                flipped: false,
            },
        ),
        (
            "oculus_quest2_controller_right",
            RenderModelObjLz4 {
                bytes: oculus_quest2_controller_left,
                flipped: true,
            },
        ),
        (
            "oculus_quest_plus_controller_left",
            RenderModelObjLz4 {
                bytes: oculus_quest_plus_controller_left,
                flipped: false,
            },
        ),
        (
            "oculus_quest_plus_controller_right",
            RenderModelObjLz4 {
                bytes: oculus_quest_plus_controller_left,
                flipped: true,
            },
        ),
        (
            "valve_controller_knu_1_0_left",
            RenderModelObjLz4 {
                bytes: valve_controller_knu_1_0_left,
                flipped: false,
            },
        ),
        (
            "valve_controller_knu_1_0_right",
            RenderModelObjLz4 {
                bytes: valve_controller_knu_1_0_left,
                flipped: true,
            },
        ),
        (
            "vive_focus3_controller_left",
            RenderModelObjLz4 {
                bytes: vive_focus3_controller_left,
                flipped: false,
            },
        ),
        (
            "vive_focus3_controller_right",
            RenderModelObjLz4 {
                bytes: vive_focus3_controller_left,
                flipped: true,
            },
        ),
        (
            "vr_controller_vive_1_5",
            RenderModelObjLz4 {
                bytes: include_bytes!("../resources/rendermodels/vr_controller_vive_1_5.obj.lz4"),
                flipped: false,
            },
        ),
        (
            "vr_tracker_vive_3_0",
            RenderModelObjLz4 {
                bytes: include_bytes!("../resources/rendermodels/vr_tracker_vive_3_0.obj.lz4"),
                flipped: false,
            },
        ),
    ]
    .into()
});

/// The model name without the `{driver}` prefix.
fn model_name(render_model_name: &CStr) -> Option<&str> {
    let name = render_model_name.to_str().ok().or_else(|| {
        warn!(
            "render_model_name is not a valid UTF-8 string: {:?}",
            render_model_name
        );
        None
    })?;

    Some(
        name.strip_prefix('{')
            .and_then(|s| s.split_once('}').map(|(_driver, model)| model))
            .unwrap_or(name),
    )
}

/// The embedded model closest to the controllers our profiles report models for, but which we
/// don't ship. Hand tracking gets none: a controller drawn on a bare hand would be worse.
fn fallback_model(model_name: &str) -> Option<&'static str> {
    match model_name {
        // The Steam Frame profile uses the Index controllers' raw pose.
        "frame_controller_left" => Some("valve_controller_knu_1_0_left"),
        "frame_controller_right" => Some("valve_controller_knu_1_0_right"),
        // Ringed like the Quest 2 ones, whose raw pose the Pico profiles use.
        "pico_4_controller_left" => Some("oculus_quest2_controller_left"),
        "pico_4_controller_right" => Some("oculus_quest2_controller_right"),
        // Ringless like the Quest 3 ones.
        "oculus_quest_pro_controller_left" => Some("oculus_quest_plus_controller_left"),
        "oculus_quest_pro_controller_right" => Some("oculus_quest_plus_controller_right"),
        // The Cosmos profile uses the Focus 3's raw pose.
        "vive_cosmos_controller_left" => Some("vive_focus3_controller_left"),
        "vive_cosmos_controller_right" => Some("vive_focus3_controller_right"),
        // WMR reports the path the driver extracted the model to.
        name if name.ends_with("\\controller.obj") => Some("generic_controller"),
        _ => None,
    }
}

fn get_render_model_data(render_model_name: &CStr) -> Option<&RenderModelObjLz4> {
    let name = model_name(render_model_name)?;
    RENDER_MODELS
        .get(name)
        .or_else(|| RENDER_MODELS.get(fallback_model(name)?))
}

#[derive(macros::InterfaceImpl)]
#[interface = "IVRRenderModels"]
#[versions(006, 005, 004, 002)]
pub struct RenderModels {
    vtables: Vtables,
    data: Mutex<RenderModelData>,
    input: Injected<Input<Compositor>>,
}

impl RenderModels {
    pub fn new(injector: &Injector) -> Self {
        Self {
            vtables: Default::default(),
            data: Default::default(),
            input: injector.inject(),
        }
    }

    /// Reads a component's input from the input interface, at rest without one.
    fn read_input(&self) -> impl Fn(Hand, DynSubpath) -> ComponentInput + '_ {
        move |hand, subpath| {
            self.input.get().map_or(ComponentInput::default(), |input| {
                input.component_input(hand, subpath)
            })
        }
    }
}

impl Default for RenderModels {
    fn default() -> Self {
        Self::new(&Injector::default())
    }
}

#[derive(Default)]
struct RenderModelData {
    loaded_model_data: HashMap<usize, OwnedRenderModel>,
    loaded_textures: HashMap<usize, OwnedTexture>,
}

#[allow(non_snake_case)]
impl vr::IVRRenderModels006_Interface for RenderModels {
    fn GetRenderModelErrorNameFromEnum(
        &self,
        error: vr::EVRRenderModelError,
    ) -> *const std::ffi::c_char {
        crate::enum_name("VRRenderModelError_", error)
    }
    /// The OBJ file of SteamVR's model. The bundled models have no path.
    fn GetRenderModelOriginalPath(
        &self,
        render_model_name: *const std::ffi::c_char,
        buffer: *mut std::ffi::c_char,
        buffer_size: u32,
        error: *mut vr::EVRRenderModelError,
    ) -> u32 {
        let path = steamvr_model(render_model_name)
            .and_then(|model| CString::new(model.obj().to_string_lossy().into_owned()).ok());
        write_string_result(path, buffer, buffer_size, error)
    }
    /// A file URL of the thumbnail SteamVR's model names.
    fn GetRenderModelThumbnailURL(
        &self,
        render_model_name: *const std::ffi::c_char,
        buffer: *mut std::ffi::c_char,
        buffer_size: u32,
        error: *mut vr::EVRRenderModelError,
    ) -> u32 {
        let url = steamvr_model(render_model_name)
            .and_then(|model| model.thumbnail())
            .and_then(|path| CString::new(format!("file://{}", path.to_string_lossy())).ok());
        write_string_result(url, buffer, buffer_size, error)
    }
    fn RenderModelHasComponent(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
    ) -> bool {
        if render_model_name.is_null() || component_name.is_null() {
            debug!(
                "RenderModelHasComponent called with null ptr(s): rm={render_model_name:?} comp={component_name:?}"
            );
            return false;
        }

        let render_model_name = unsafe { CStr::from_ptr(render_model_name) };
        let component_name = unsafe { CStr::from_ptr(component_name) };

        has_component(render_model_name, component_name)
    }
    /// The input is ours, so the controller state the app passes isn't read.
    fn GetComponentState(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        _: *const vr::VRControllerState_t,
        mode_state: *const vr::RenderModel_ControllerMode_State_t,
        state: *mut vr::RenderModel_ComponentState_t,
    ) -> bool {
        self.component_state(render_model_name, component_name, None, mode_state, state)
    }
    fn GetComponentStateForDevicePath(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        device_path: vr::VRInputValueHandle_t,
        mode_state: *const vr::RenderModel_ControllerMode_State_t,
        state: *mut vr::RenderModel_ComponentState_t,
    ) -> bool {
        let hand = self
            .input
            .get()
            .and_then(|input| input.hand_for_input_handle(device_path));
        self.component_state(render_model_name, component_name, hand, mode_state, state)
    }
    fn GetComponentRenderModelName(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        component_render_model_name: *mut std::ffi::c_char,
        component_render_model_name_len: u32,
    ) -> u32 {
        if render_model_name.is_null() || component_name.is_null() {
            debug!(
                "GetComponentRenderModelName called with null ptr(s): rm={render_model_name:?} comp={component_name:?}"
            );
            return 0;
        }

        let render_model_name = unsafe { CStr::from_ptr(render_model_name) };
        let component_name = unsafe { CStr::from_ptr(component_name) };

        // SteamVR's parts are named after their files, under the model's name.
        if let Some((info, index)) = steamvr_component(render_model_name, component_name) {
            let Some(stem) = info.components[index].1.file_stem() else {
                return 0;
            };
            let Ok(name) = CString::new(format!("{}/{stem}", render_model_name.to_string_lossy()))
            else {
                return 0;
            };
            return cstr_write_out(
                &name,
                component_render_model_name,
                component_render_model_name_len,
            );
        }

        if component_name != DEFAULT_COMPONENT || !has_model(render_model_name) {
            // unsupported model or component
            return 0;
        }

        cstr_write_out(
            render_model_name,
            component_render_model_name,
            component_render_model_name_len,
        )
    }

    fn GetComponentButtonMask(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
    ) -> u64 {
        if render_model_name.is_null() || component_name.is_null() {
            return 0;
        }
        let render_model_name = unsafe { CStr::from_ptr(render_model_name) };
        let component_name = unsafe { CStr::from_ptr(component_name) };
        // The bundled models only have the whole model and pose components, none of which are
        // buttons.
        steamvr_component(render_model_name, component_name)
            .map_or(0, |(info, index)| info.components[index].1.button_mask())
    }
    fn GetComponentName(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_index: u32,
        component_name: *mut std::ffi::c_char,
        component_name_len: u32,
    ) -> u32 {
        if render_model_name.is_null() {
            return 0;
        }

        if let Some(info) = steamvr::components(unsafe { CStr::from_ptr(render_model_name) }) {
            let Some((name, _)) = info.components.get(component_index as usize) else {
                return 0;
            };
            let Ok(name) = CString::new(name.as_str()) else {
                return 0;
            };
            return cstr_write_out(&name, component_name, component_name_len);
        }

        // even for unknown render models, always act as if there's a valid component
        // needed for titles like Derail Valley to function
        if component_index > 0 {
            return 0;
        }
        cstr_write_out(DEFAULT_COMPONENT, component_name, component_name_len)
    }
    fn GetComponentCount(&self, render_model_name: *const std::ffi::c_char) -> u32 {
        if render_model_name.is_null() {
            return 0;
        }
        let render_model_name = unsafe { CStr::from_ptr(render_model_name) };
        if let Some(info) = steamvr::components(render_model_name) {
            info.components.len() as u32
        } else if render_model_name.count_bytes() == 0 {
            0
        } else {
            1 // DEFAULT_COMPONENT
        }
    }
    fn GetRenderModelCount(&self) -> u32 {
        RENDER_MODELS.len() as _
    }
    fn GetRenderModelName(
        &self,
        index: u32,
        render_model_name: *mut std::ffi::c_char,
        render_model_name_len: u32,
    ) -> u32 {
        let Some((render_model, _)) = RENDER_MODELS.iter().nth(index as usize) else {
            return 0;
        };

        let Ok(cstr) = CString::from_str(render_model) else {
            return 0;
        };

        cstr_write_out(&cstr, render_model_name, render_model_name_len)
    }
    fn FreeTextureD3D11(&self, _: *mut std::ffi::c_void) {
        crate::warn_unimplemented!("FreeTextureD3D11");
    }
    fn LoadIntoTextureD3D11_Async(
        &self,
        _: vr::TextureID_t,
        _: *mut std::ffi::c_void,
    ) -> vr::EVRRenderModelError {
        crate::warn_unimplemented!("LoadIntoTextureD3D11_Async");
        vr::EVRRenderModelError::NotSupported
    }
    fn LoadTextureD3D11_Async(
        &self,
        _: vr::TextureID_t,
        _: *mut std::ffi::c_void,
        _: *mut *mut std::ffi::c_void,
    ) -> vr::EVRRenderModelError {
        crate::warn_unimplemented!("LoadTextureD3D11_Async");
        vr::EVRRenderModelError::NotSupported
    }
    fn FreeTexture(&self, texture_map: *mut vr::RenderModel_TextureMap_t) {
        if texture_map.is_null() {
            return;
        }

        let removed = self
            .data
            .lock()
            .unwrap()
            .loaded_textures
            .remove(&(texture_map as usize));

        // only free if it was ours
        if removed.is_some() {
            unsafe {
                drop(Box::from_raw(texture_map));
            }
        } else {
            warn!("FreeTexture called with unknown pointer {texture_map:?}");
        }
    }
    fn LoadTexture_Async(
        &self,
        tex_id: vr::TextureID_t,
        out_texture_map: *mut *mut vr::RenderModel_TextureMap_t,
    ) -> vr::EVRRenderModelError {
        if out_texture_map.is_null() {
            warn!("LoadTexture_Async called with null out_texture_map");
            return vr::EVRRenderModelError::InvalidArg;
        }

        unsafe {
            *out_texture_map = std::ptr::null_mut();
        }

        if tex_id < 0 {
            warn!("LoadTexture_Async called with invalid tex_id {tex_id:?}");
            return vr::EVRRenderModelError::InvalidTexture;
        }

        // SteamVR's textures; 0 is the bundled models' placeholder. A texture that doesn't
        // decode gets the placeholder too, rather than a model the app won't draw.
        let tex = steamvr::texture_path(tex_id)
            .and_then(|path| {
                steamvr::load_texture(&path).or_else(|| {
                    warn!("Couldn't load {}, using a plain texture", path.display());
                    None
                })
            })
            .unwrap_or_else(OwnedTexture::new_dummy);
        let texture_map_t = tex.to_texture_map_t();
        let ptr = Box::into_raw(texture_map_t);

        self.data
            .lock()
            .unwrap()
            .loaded_textures
            .insert(ptr as usize, tex);

        unsafe {
            *out_texture_map = ptr;
        }

        vr::EVRRenderModelError::None
    }
    fn FreeRenderModel(&self, render_model: *mut vr::RenderModel_t) {
        if render_model.is_null() {
            return;
        }

        let removed = {
            let mut data = self.data.lock().unwrap();
            data.loaded_model_data.remove(&(render_model as usize))
        };

        // only free if it was ours
        if removed.is_some() {
            unsafe {
                drop(Box::from_raw(render_model));
            }
        } else {
            warn!("FreeRenderModel called with unknown pointer {render_model:?}");
        }
    }
    fn LoadRenderModel_Async(
        &self,
        render_model_name: *const std::ffi::c_char,
        out_render_model: *mut *mut vr::RenderModel_t,
    ) -> vr::EVRRenderModelError {
        if out_render_model.is_null() {
            warn!("LoadRenderModel_Async called with null out_render_model");
            return vr::EVRRenderModelError::InvalidArg;
        }
        unsafe {
            *out_render_model = std::ptr::null_mut();
        }

        if render_model_name.is_null() {
            warn!("LoadRenderModel_Async called with null render_model_name");
            return vr::EVRRenderModelError::InvalidArg;
        }

        let render_model_name = unsafe { CStr::from_ptr(render_model_name) };
        // SteamVR's model with its texture when it's installed, else a bundled mesh.
        let steamvr = steamvr_model(render_model_name.as_ptr()).and_then(|model| {
            steamvr::load(&model)
                .inspect_err(|e| warn!("Couldn't load SteamVR's {render_model_name:?}: {e:?}"))
                .ok()
        });
        let model_data = match steamvr {
            Some(model) => model,
            None => match load_bundled_model(render_model_name) {
                Ok(model) => model,
                Err(e) => return e,
            },
        };

        let render_model_box = model_data.to_render_model_t();
        let ptr = Box::into_raw(render_model_box);

        {
            let mut data = self.data.lock().unwrap();
            data.loaded_model_data.insert(ptr as usize, model_data);
        }

        unsafe {
            *out_render_model = ptr;
        }
        vr::EVRRenderModelError::None
    }
}

impl RenderModels {
    /// GetComponentState with the hand known, if it is.
    fn component_state(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        hand: Option<Hand>,
        mode_state: *const vr::RenderModel_ControllerMode_State_t,
        state: *mut vr::RenderModel_ComponentState_t,
    ) -> bool {
        let Some(state) = (unsafe { state.as_mut() }) else {
            return false;
        };
        // "If the pchRenderModelName or pchComponentName is invalid, this will return false (and
        // transforms will be set to identity)."
        state.mTrackingToComponentRenderModel = xr::Posef::IDENTITY.into();
        state.mTrackingToComponentLocal = xr::Posef::IDENTITY.into();
        state.uProperties = 0;

        if render_model_name.is_null() || component_name.is_null() {
            debug!(
                "GetComponentState called with null ptr(s): rm={render_model_name:?} comp={component_name:?}"
            );
            return false;
        }

        let render_model_name = unsafe { CStr::from_ptr(render_model_name) };
        let component_name = unsafe { CStr::from_ptr(component_name) };
        let scroll = unsafe { mode_state.as_ref() }.is_some_and(|mode| mode.bScrollWheelVisible);

        match component_state(
            render_model_name,
            component_name,
            hand,
            &self.read_input(),
            scroll,
        ) {
            Some(component_state) => {
                *state = component_state;
                true
            }
            None => false,
        }
    }
}

/// A bundled mesh, with the fallback meshes moved to the device they stand in for.
fn load_bundled_model(
    render_model_name: &CStr,
) -> Result<OwnedRenderModel, vr::EVRRenderModelError> {
    let Some(bytes) = get_render_model_data(render_model_name) else {
        warn!("render model name {render_model_name:?} did not resolve to a valid resource");
        return Err(vr::EVRRenderModelError::InvalidArg);
    };
    let mut model_data = OwnedRenderModel::load(render_model_name, bytes)
        .inspect_err(|e| warn!("failed to load rendermodel {render_model_name:?}: {e:?}"))?;
    if let Some(transform) = fallback_mesh_transform(render_model_name) {
        model_data.transform(transform);
    }
    Ok(model_data)
}

/// SteamVR's files for the model named by the app, if it's installed and has it.
fn steamvr_model(render_model_name: *const std::ffi::c_char) -> Option<steamvr::Model> {
    if render_model_name.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(render_model_name) }.to_str().ok()?;
    steamvr::find(name)
}

/// Writes `value` for the string getters, or an empty string and NotSupported without one.
fn write_string_result(
    value: Option<CString>,
    buffer: *mut std::ffi::c_char,
    buffer_size: u32,
    error: *mut vr::EVRRenderModelError,
) -> u32 {
    let Some(value) = value else {
        if !buffer.is_null() && buffer_size > 0 {
            unsafe { *buffer = 0 };
        }
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::EVRRenderModelError::NotSupported;
        }
        return 0;
    };
    if let Some(error) = unsafe { error.as_mut() } {
        *error = vr::EVRRenderModelError::None;
    }
    cstr_write_out(&value, buffer, buffer_size)
}

// Pre-async interface. Our loads complete immediately, so these just wrap the async versions.
#[allow(non_snake_case)]
impl vr::IVRRenderModels002On004 for RenderModels {
    fn LoadRenderModel(
        &self,
        render_model_name: *const std::ffi::c_char,
        out_render_model: *mut *mut vr::RenderModel_t,
    ) -> bool {
        vr::IVRRenderModels006_Interface::LoadRenderModel_Async(
            self,
            render_model_name,
            out_render_model,
        ) == vr::EVRRenderModelError::None
    }
    fn LoadTexture(
        &self,
        tex_id: vr::TextureID_t,
        out_texture_map: *mut *mut vr::RenderModel_TextureMap_t,
    ) -> bool {
        vr::IVRRenderModels006_Interface::LoadTexture_Async(self, tex_id, out_texture_map)
            == vr::EVRRenderModelError::None
    }
    fn GetComponentState(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        controller_state: *const vr::VRControllerState_t,
        state: *mut vr::RenderModel_ComponentState_t,
    ) -> bool {
        vr::IVRRenderModels006_Interface::GetComponentState(
            self,
            render_model_name,
            component_name,
            controller_state,
            ptr::null(),
            state,
        )
    }
}

#[allow(dead_code)]
struct OwnedTexture {
    width: u16,
    height: u16,
    // Not a Box: moving a Box asserts unique access to its contents, which would invalidate the
    // pointer handed out to the app.
    data: Vec<u8>,
}

impl OwnedTexture {
    /// creates a 1x1 dummy texture
    fn new_dummy() -> Self {
        let data = RENDER_MODEL_COLOR.to_vec();
        Self {
            data,
            width: 1,
            height: 1,
        }
    }
    fn to_texture_map_t(&self) -> Box<vr::RenderModel_TextureMap_t> {
        Box::new(vr::RenderModel_TextureMap_t {
            unWidth: self.width,
            unHeight: self.height,
            unMipLevels: 1,
            rubTextureMapData: self.data.as_ptr(),
            format: openvr::EVRRenderModelTextureFormat::RGBA8_SRGB,
        })
    }
}

struct OwnedRenderModel {
    // Vecs for the same reason as OwnedTexture::data.
    verts: Vec<vr::RenderModel_Vertex_t>,
    indices: Vec<u16>,
    /// The diffuse texture for LoadTexture_Async, 0 for the placeholder.
    texture: vr::TextureID_t,
}

impl OwnedRenderModel {
    fn load(
        name: &CStr,
        model_data_obj_lz4: &RenderModelObjLz4,
    ) -> Result<Self, vr::EVRRenderModelError> {
        let mut decompressed_bytes = vec![];
        FrameDecoder::new(model_data_obj_lz4.bytes)
            .read_to_end(&mut decompressed_bytes)
            .map_err(|e| {
                warn!("could not decompress model {name:?}: {e:?}");
                vr::EVRRenderModelError::InvalidModel
            })?;

        let obj = ObjData::load_buf(decompressed_bytes.as_slice()).map_err(|e| {
            warn!("OBJ load failed for {name:?}: {e:?}");
            vr::EVRRenderModelError::InvalidModel
        })?;

        Self::new_from_obj(&obj, model_data_obj_lz4.flipped)
    }

    fn new_from_obj(obj: &ObjData, flipped: bool) -> Result<Self, vr::EVRRenderModelError> {
        let mut verts: Vec<vr::RenderModel_Vertex_t> = Vec::new();
        let mut indices: Vec<u16> = Vec::new();
        let mut vertex_map: HashMap<IndexTuple, u16> = HashMap::new();

        let mut map_vertex_index = |idx: IndexTuple| -> Result<u16, vr::EVRRenderModelError> {
            if let Some(&i) = vertex_map.get(&idx) {
                return Ok(i);
            }

            let mut position = *obj
                .position
                .get(idx.0)
                .ok_or(vr::EVRRenderModelError::InvalidModel)?;

            if flipped {
                position[0] *= -1.0;
            }

            // OBJ texture coordinates start at the bottom of the image, OpenVR's at the top row
            // of the texture data.
            let texcoord = idx
                .1
                .and_then(|i| obj.texture.get(i))
                .map_or([0.0; 2], |[u, v]| [*u, 1.0 - v]);

            // will generate normals later
            let normal = [0.0, 0.0, 0.0];

            let new_index =
                u16::try_from(verts.len()).map_err(|_| vr::EVRRenderModelError::TooManyVertices)?;

            verts.push(vr::RenderModel_Vertex_t {
                vPosition: vr::HmdVector3_t { v: position },
                vNormal: vr::HmdVector3_t { v: normal },
                rfTextureCoord: texcoord,
            });

            vertex_map.insert(idx, new_index);
            Ok(new_index)
        };

        for object in &obj.objects {
            for group in &object.groups {
                for poly in &group.polys {
                    let poly_indices = &poly.0;

                    if poly_indices.len() < 3 {
                        warn!("Discarding polygon with less than 3 indices.");
                        continue;
                    }

                    if poly_indices.len() > 4 {
                        warn!(
                            "OBJ has {}-gon; using fan triangulation (may be wrong for concave faces)",
                            poly_indices.len()
                        );
                    }

                    let i0 = map_vertex_index(poly_indices[0])?;

                    for i in 1..(poly_indices.len() - 1) {
                        let i1 = map_vertex_index(poly_indices[i])?;
                        let i2 = map_vertex_index(poly_indices[i + 1])?;

                        if flipped {
                            // reverse winding to keep normals/front-faces correct
                            indices.extend([i0, i2, i1]);
                        } else {
                            indices.extend([i0, i1, i2]);
                        }
                    }
                }
            }
        }

        let mut normals = vec![Vec3::ZERO; verts.len()];

        for tri in indices.as_chunks::<3>().0 {
            let ia = tri[0] as usize;
            let ib = tri[1] as usize;
            let ic = tri[2] as usize;

            let a = Vec3::from_array(verts[ia].vPosition.v);
            let b = Vec3::from_array(verts[ib].vPosition.v);
            let c = Vec3::from_array(verts[ic].vPosition.v);

            // area-weighted face normal
            let n = (b - a).cross(c - a);

            // skip degenerate triangles
            if n.length_squared() == 0.0 {
                continue;
            }

            normals[ia] += n;
            normals[ib] += n;
            normals[ic] += n;
        }

        for (v, n) in verts.iter_mut().zip(normals) {
            let n = if n.length_squared() > 0.0 {
                n.normalize()
            } else {
                Vec3::Z // fallback
            };
            v.vNormal = vr::HmdVector3_t { v: n.to_array() };
        }

        Ok(Self {
            verts,
            indices,
            texture: 0,
        })
    }

    fn transform(&mut self, transform: Mat4) {
        for vert in &mut self.verts {
            let position = Vec3::from_array(vert.vPosition.v);
            vert.vPosition.v = transform.transform_point3(position).to_array();
            let normal = Vec3::from_array(vert.vNormal.v);
            vert.vNormal.v = transform.transform_vector3(normal).to_array();
        }
    }

    fn to_render_model_t(&self) -> Box<vr::RenderModel_t> {
        Box::new(vr::RenderModel_t {
            rVertexData: self.verts.as_ptr(),
            unVertexCount: self.verts.len() as _,
            rIndexData: self.indices.as_ptr(),
            unTriangleCount: (self.indices.len() / 3) as _,
            diffuseTextureId: self.texture,
        })
    }
}

/// Returns the buffer size the string needs, which callers tell apart from 0 for "no such item".
fn cstr_write_out(cstr: &CStr, out_ptr: *mut std::ffi::c_char, out_len: u32) -> u32 {
    let bytes = unsafe { std::slice::from_raw_parts(cstr.as_ptr(), cstr.count_bytes() + 1) };
    if out_ptr.is_null() || out_len == 0 {
        return bytes.len() as u32; // querying required buffer length
    }
    let out = unsafe { std::slice::from_raw_parts_mut(out_ptr, out_len as usize) };
    if out.len() < bytes.len() {
        // doesn't fit, but don't leave the app reading garbage
        out[0] = 0;
    } else {
        out[..bytes.len()].copy_from_slice(bytes);
    }

    bytes.len() as u32
}

/// SteamVR's own render models and textures, used when a SteamVR install is found. The names
/// apps ask for are its: `name`, `{driver}name` or `{driver}/rendermodels/name`, kept in
/// `resources/rendermodels/<name>/` of SteamVR or of a driver.
mod steamvr {
    use super::components::ModelInfo;
    use super::{ObjData, OwnedRenderModel, OwnedTexture};
    use log::{info, warn};
    use openvr as vr;
    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};

    fn find_install() -> Option<PathBuf> {
        let install = crate::input::steamvr_install(&crate::input::isolated_env_var);
        match &install {
            Some(dir) => info!("Using SteamVR's render models in {}", dir.display()),
            None => info!("No SteamVR install found, using the bundled render models"),
        }
        install
    }

    /// The install, looked for once. Tests set up their own.
    fn install() -> Option<PathBuf> {
        #[cfg(not(test))]
        {
            static INSTALL: LazyLock<Option<PathBuf>> = LazyLock::new(find_install);
            INSTALL.clone()
        }
        #[cfg(test)]
        find_install()
    }

    /// A model's directory and the stem of its files, or of one of its component's.
    #[derive(Clone, Debug, PartialEq)]
    pub(super) struct Model {
        dir: PathBuf,
        stem: String,
    }

    /// The parsed JSON of each model, by file; None where it's missing or broken.
    static INFOS: LazyLock<Mutex<HashMap<PathBuf, Option<Arc<ModelInfo>>>>> =
        LazyLock::new(Default::default);

    impl Model {
        pub fn obj(&self) -> PathBuf {
            self.dir.join(format!("{}.obj", self.stem))
        }

        /// The model's JSON, read once.
        pub fn info(&self) -> Option<Arc<ModelInfo>> {
            let path = self
                .dir
                .join(format!("{}.json", self.dir.file_name()?.to_str()?));
            INFOS
                .lock()
                .unwrap()
                .entry(path.clone())
                .or_insert_with(|| {
                    let json = std::fs::read(&path).ok()?;
                    match ModelInfo::parse(&json) {
                        Ok(info) => Some(Arc::new(info)),
                        Err(e) => {
                            warn!("Couldn't parse {}: {e}", path.display());
                            None
                        }
                    }
                })
                .clone()
        }

        /// The thumbnail the model's JSON names.
        pub fn thumbnail(&self) -> Option<PathBuf> {
            let path = self.dir.join(self.info()?.thumbnail.as_deref()?);
            path.is_file().then_some(path)
        }
    }

    /// The components of SteamVR's model named by the app, if its JSON lists any.
    pub(super) fn components(render_model_name: &CStr) -> Option<Arc<ModelInfo>> {
        let model = find(render_model_name.to_str().ok()?)?;
        let info = model.info()?;
        (!info.components.is_empty()).then_some(info)
    }

    /// Splits `render_model_name` into the driver, model and component it names.
    fn parse(render_model_name: &str) -> Option<(Option<&str>, &str, Option<&str>)> {
        let (driver, rest) = match render_model_name.strip_prefix('{') {
            Some(rest) => {
                let (driver, rest) = rest.split_once('}')?;
                (Some(driver), rest)
            }
            None => (None, render_model_name),
        };
        let rest = rest.strip_prefix("/rendermodels/").unwrap_or(rest);
        let (model, component) = match rest.split_once('/') {
            Some((model, component)) => (model, Some(component)),
            None => (rest, None),
        };
        // Names, not paths.
        let plain = |name: &str| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                && !name.starts_with('.')
        };
        if !plain(model) || !driver.is_none_or(plain) || !component.is_none_or(plain) {
            return None;
        }
        Some((driver, model, component))
    }

    /// What each name resolved to in each install, as apps ask every frame.
    static FOUND: LazyLock<Mutex<FoundModels>> = LazyLock::new(Default::default);
    type FoundModels = HashMap<(PathBuf, String), Option<Model>>;

    pub(super) fn find(render_model_name: &str) -> Option<Model> {
        let install = install()?;
        FOUND
            .lock()
            .unwrap()
            .entry((install.clone(), render_model_name.to_owned()))
            .or_insert_with(|| find_in(&install, render_model_name))
            .clone()
    }

    fn find_in(install: &Path, render_model_name: &str) -> Option<Model> {
        let (driver, model, component) = parse(render_model_name)?;
        let in_driver = |driver: PathBuf| driver.join("resources/rendermodels").join(model);
        let dirs: Vec<PathBuf> = match driver {
            Some(driver) => vec![in_driver(install.join("drivers").join(driver))],
            None => std::iter::once(install.join("resources/rendermodels").join(model))
                .chain(
                    std::fs::read_dir(install.join("drivers"))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|driver| in_driver(driver.path())),
                )
                .collect(),
        };
        let dir = dirs
            .into_iter()
            .find(|dir| dir.join(format!("{model}.obj")).is_file())?;
        let found = Model {
            dir,
            stem: component.unwrap_or(model).to_owned(),
        };
        found.obj().is_file().then_some(found)
    }

    /// The model's mesh, with its diffuse texture registered for LoadTexture_Async.
    pub(super) fn load(model: &Model) -> Result<OwnedRenderModel, vr::EVRRenderModelError> {
        let path = model.obj();
        let bytes = std::fs::read(&path).map_err(|e| {
            warn!("Couldn't read {}: {e}", path.display());
            vr::EVRRenderModelError::InvalidModel
        })?;
        let obj = ObjData::load_buf(bytes.as_slice()).map_err(|e| {
            warn!("Couldn't parse {}: {e:?}", path.display());
            vr::EVRRenderModelError::InvalidModel
        })?;
        let mut mesh = OwnedRenderModel::new_from_obj(&obj, false)?;
        // The diffuse map of the first material library naming one.
        let texture = obj.material_libs.iter().find_map(|lib| {
            let mtl = std::fs::read_to_string(model.dir.join(&lib.filename)).ok()?;
            mtl.lines().find_map(|line| {
                let file = line.trim().strip_prefix("map_Kd")?.trim();
                (!file.is_empty()).then(|| model.dir.join(file))
            })
        });
        if let Some(texture) = texture {
            mesh.texture = texture_id(texture);
        }
        Ok(mesh)
    }

    /// The textures handed out so far, by id minus one; 0 is the placeholder.
    static TEXTURES: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(Default::default);

    fn texture_id(path: PathBuf) -> vr::TextureID_t {
        let mut textures = TEXTURES.lock().unwrap();
        let index = textures
            .iter()
            .position(|known| *known == path)
            .unwrap_or_else(|| {
                textures.push(path);
                textures.len() - 1
            });
        (index + 1) as vr::TextureID_t
    }

    pub(super) fn texture_path(id: vr::TextureID_t) -> Option<PathBuf> {
        let index = usize::try_from(id).ok()?.checked_sub(1)?;
        TEXTURES.lock().unwrap().get(index).cloned()
    }

    /// The texture as RGBA8, top row first.
    pub(super) fn load_texture(path: &Path) -> Option<OwnedTexture> {
        let bytes = std::fs::read(path)
            .inspect_err(|e| warn!("Couldn't read {}: {e}", path.display()))
            .ok()?;
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        let (width, height, data) = match extension.as_str() {
            "png" => decode_png(&bytes)?,
            "tga" => decode_tga(&bytes)?,
            other => {
                warn!("Unsupported texture format {other} of {}", path.display());
                return None;
            }
        };
        Some(OwnedTexture {
            width: u16::try_from(width).ok()?,
            height: u16::try_from(height).ok()?,
            data,
        })
    }

    fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .inspect_err(|e| warn!("Couldn't decode PNG: {e}"))
            .ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader
            .next_frame(&mut buf)
            .inspect_err(|e| warn!("Couldn't decode PNG: {e}"))
            .ok()?;
        let pixels = &buf[..info.buffer_size()];
        let rgba = match info.color_type {
            png::ColorType::Rgba => pixels.to_vec(),
            png::ColorType::Rgb => pixels
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::GrayscaleAlpha => pixels
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            png::ColorType::Indexed => {
                warn!("Palette PNG wasn't expanded");
                return None;
            }
        };
        Some((info.width, info.height, rgba))
    }

    /// Uncompressed and run length encoded true color TGA, which SteamVR's models use.
    fn decode_tga(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let header = bytes.get(..18)?;
        let id_length = header[0] as usize;
        let (color_map_type, image_type) = (header[1], header[2]);
        let width = u16::from_le_bytes([header[12], header[13]]) as usize;
        let height = u16::from_le_bytes([header[14], header[15]]) as usize;
        let (depth, descriptor) = (header[16], header[17]);
        if color_map_type != 0 || !matches!(image_type, 2 | 10) || !matches!(depth, 24 | 32) {
            warn!("Unsupported TGA: type {image_type}, {depth} bits, color map {color_map_type}");
            return None;
        }
        let channels = depth as usize / 8;
        let total = width * height;
        let data = bytes.get(18 + id_length..)?;
        let mut pixels = Vec::with_capacity(total * 4);
        let push = |pixels: &mut Vec<u8>, px: &[u8]| {
            pixels.extend([px[2], px[1], px[0], if channels == 4 { px[3] } else { 255 }]);
        };
        if image_type == 2 {
            for px in data.get(..total * channels)?.chunks_exact(channels) {
                push(&mut pixels, px);
            }
        } else {
            let (mut at, mut count) = (0, 0);
            while count < total {
                let packet = *data.get(at)?;
                at += 1;
                let run = (packet & 0x7f) as usize + 1;
                if packet & 0x80 != 0 {
                    let px = data.get(at..at + channels)?;
                    at += channels;
                    for _ in 0..run {
                        push(&mut pixels, px);
                    }
                } else {
                    for _ in 0..run {
                        push(&mut pixels, data.get(at..at + channels)?);
                        at += channels;
                    }
                }
                count += run;
            }
            pixels.truncate(total * 4);
        }
        // Rows start at the bottom unless the descriptor says top.
        if descriptor & 0x20 == 0 {
            let row = width * 4;
            let mut flipped = Vec::with_capacity(pixels.len());
            for r in (0..height).rev() {
                flipped.extend_from_slice(&pixels[r * row..(r + 1) * row]);
            }
            pixels = flipped;
        }
        Some((width as u32, height as u32, pixels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vr::IVRRenderModels006_Interface;

    #[test]
    fn get_render_model_data_able_to_resolve_all_elements() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash_bytes(bytes: &[u8]) -> u64 {
            let mut h = DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        }

        for (name, expected) in RENDER_MODELS.iter() {
            let asset_name = CString::new(*name).unwrap();
            let asset =
                get_render_model_data(asset_name.as_c_str()).expect("plain name should resolve");
            assert_eq!(
                asset.bytes.len(),
                expected.bytes.len(),
                "resolved bytes should match embedded resource length for {name}"
            );
            assert_eq!(
                hash_bytes(asset.bytes),
                hash_bytes(expected.bytes),
                "resolved bytes should point at the embedded resource for {name}"
            );

            let asset_name = CString::new(format!("{{some_driver}}{name}")).unwrap();
            let asset =
                get_render_model_data(asset_name.as_c_str()).expect("braced name should resolve");
            assert_eq!(
                asset.bytes.len(),
                expected.bytes.len(),
                "braced name should resolve to the same embedded resource length for {name}"
            );
            assert_eq!(
                hash_bytes(asset.bytes),
                hash_bytes(expected.bytes),
                "braced name should resolve to the same embedded resource for {name}"
            );
        }
    }

    #[test]
    fn cstr_write_out_contract() {
        // no pointer → no deref, return required size
        let s = c"hello";
        let required = cstr_write_out(s, ptr::null_mut(), 0);
        assert_eq!(required, s.to_bytes_with_nul().len() as u32);

        // pointer, but buffer too small → empty string, return required size
        let mut small = vec![1 as std::ffi::c_char; (required as usize).saturating_sub(1)];
        let wrote = cstr_write_out(s, small.as_mut_ptr(), small.len() as u32);
        assert_eq!(wrote, required);
        assert_eq!(small[0], 0);

        // pointer with enhough buf size → deref, return size
        let mut buf = vec![0 as std::ffi::c_char; required as usize];
        let wrote = cstr_write_out(s, buf.as_mut_ptr(), buf.len() as u32);
        assert_eq!(wrote, required);

        let out = unsafe { CStr::from_ptr(buf.as_ptr()) };
        assert_eq!(out.to_bytes_with_nul(), s.to_bytes_with_nul());
    }

    #[test]
    fn error_names() {
        let rm = RenderModels::default();
        let name = unsafe {
            CStr::from_ptr(rm.GetRenderModelErrorNameFromEnum(vr::EVRRenderModelError::InvalidArg))
        };
        assert_eq!(name, c"VRRenderModelError_InvalidArg");
    }

    #[test]
    fn render_model_count_and_iter_name() {
        let rm = RenderModels::default();

        assert_eq!(rm.GetRenderModelCount() as usize, RENDER_MODELS.len());

        for (idx, (expected, _bytes)) in RENDER_MODELS.iter().enumerate() {
            let required = rm.GetRenderModelName(idx as u32, ptr::null_mut(), 0);
            assert!(required > 0);

            let mut buf = vec![0 as std::ffi::c_char; required as usize];
            let wrote = rm.GetRenderModelName(idx as u32, buf.as_mut_ptr(), buf.len() as u32);
            assert_eq!(wrote, required);

            let out = unsafe { CStr::from_ptr(buf.as_ptr()) };
            assert_eq!(out.to_str().unwrap(), *expected);
            assert_eq!(out.to_bytes_with_nul().len(), required as usize);
        }

        // out of bounds → 0
        let mut buf = [0 as std::ffi::c_char; 8];
        assert_eq!(
            rm.GetRenderModelName(
                RENDER_MODELS.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32
            ),
            0
        );
    }

    #[test]
    fn get_component_count_contract() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();

        // return 0 if render_model_name is null
        assert_eq!(rm.GetComponentCount(ptr::null()), 0);

        let empty_cstr = c"";

        // return 0 for empty cstr
        assert_eq!(rm.GetComponentCount(empty_cstr.as_ptr()), 0);

        // return 1 component for non-null render_model_name (Derail Valley fix)
        assert_eq!(rm.GetComponentCount(some_model.as_ptr()), 1);
    }

    #[test]
    fn get_component_name_contract() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();
        let mut buf = vec![32];

        // return 0 if render_model_name is null
        assert_eq!(rm.GetComponentName(ptr::null(), 0, ptr::null_mut(), 0), 0);

        // return 0 if render_model_name is null, even if out ptr is non-null
        assert_eq!(
            rm.GetComponentName(ptr::null(), 0, buf.as_mut_ptr(), buf.len() as u32),
            0
        );

        // component_index 0 returns DEFAULT_COMPONENT
        let required = rm.GetComponentName(some_model.as_ptr(), 0, ptr::null_mut(), 0);
        assert!(required > 0);
        let mut buf = vec![0 as std::ffi::c_char; required as usize];
        let wrote = rm.GetComponentName(some_model.as_ptr(), 0, buf.as_mut_ptr(), buf.len() as u32);
        assert_eq!(wrote, required);
        let out = unsafe { CStr::from_ptr(buf.as_ptr()) };
        assert_eq!(out, DEFAULT_COMPONENT);

        // component_index > 0 returns 0
        let mut buf2 = [0 as std::ffi::c_char; 32];
        assert_eq!(
            rm.GetComponentName(ptr::null(), 1, buf2.as_mut_ptr(), buf2.len() as u32),
            0
        );
    }

    #[test]
    fn render_model_has_component_contract() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();

        // true for known model + DEFAULT_COMPONENT
        assert!(rm.RenderModelHasComponent(some_model.as_ptr(), DEFAULT_COMPONENT.as_ptr()));

        // false for unknown component
        assert!(!rm.RenderModelHasComponent(some_model.as_ptr(), c"not_xrizer_component".as_ptr()));

        // false for unknown model
        assert!(!rm.RenderModelHasComponent(c"unknown_model".as_ptr(), DEFAULT_COMPONENT.as_ptr()));
    }

    #[test]
    fn component_render_model_name_contract() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();
        let mut buf = Vec::with_capacity(64);

        // return 0 if render_model_name is null
        assert_eq!(
            rm.GetComponentRenderModelName(
                ptr::null(),
                DEFAULT_COMPONENT.as_ptr(),
                ptr::null_mut(),
                0
            ),
            0
        );

        // return 0 if component_name is null
        assert_eq!(
            rm.GetComponentRenderModelName(some_model.as_ptr(), ptr::null(), ptr::null_mut(), 0),
            0
        );

        // return 0 if render_model_name is null, even if out ptr is non-null
        assert_eq!(
            rm.GetComponentRenderModelName(
                ptr::null(),
                DEFAULT_COMPONENT.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32
            ),
            0
        );

        // return 0 if component_name is null, even if out ptr is non-null
        assert_eq!(
            rm.GetComponentRenderModelName(
                some_model.as_ptr(),
                ptr::null(),
                buf.as_mut_ptr(),
                buf.len() as u32
            ),
            0
        );

        // valid component echoes back the render_model_name
        let required = rm.GetComponentRenderModelName(
            some_model.as_ptr(),
            DEFAULT_COMPONENT.as_ptr(),
            ptr::null_mut(),
            0,
        );
        assert!(required > 0);
        let mut buf = vec![0 as std::ffi::c_char; required as usize];
        let wrote = rm.GetComponentRenderModelName(
            some_model.as_ptr(),
            DEFAULT_COMPONENT.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        );
        assert_eq!(wrote, required);
        let out = unsafe { CStr::from_ptr(buf.as_ptr()) };
        assert_eq!(out, some_model.as_c_str());

        // invalid component returns 0
        assert_eq!(
            rm.GetComponentRenderModelName(
                some_model.as_ptr(),
                c"not_xrizer_component".as_ptr(),
                ptr::null_mut(),
                0
            ),
            0
        );

        // unknown model returns 0
        assert_eq!(
            rm.GetComponentRenderModelName(
                c"unknown_model".as_ptr(),
                DEFAULT_COMPONENT.as_ptr(),
                ptr::null_mut(),
                0
            ),
            0
        );
    }

    #[test]
    fn get_component_state_contract() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();

        // null safety
        let mut state = unsafe { std::mem::zeroed::<vr::RenderModel_ComponentState_t>() };
        assert!(!rm.GetComponentState(
            ptr::null(),
            DEFAULT_COMPONENT.as_ptr(),
            ptr::null(),
            ptr::null(),
            &mut state
        ));
        assert!(!rm.GetComponentState(
            some_model.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            &mut state
        ));
        assert!(!rm.GetComponentState(
            some_model.as_ptr(),
            DEFAULT_COMPONENT.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null_mut()
        ));

        // valid call
        let mut state = unsafe { std::mem::zeroed::<vr::RenderModel_ComponentState_t>() };
        let ok = rm.GetComponentState(
            some_model.as_ptr(),
            DEFAULT_COMPONENT.as_ptr(),
            ptr::null(),
            ptr::null(),
            &mut state,
        );
        assert!(ok);

        let ident = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];
        assert_eq!(state.mTrackingToComponentRenderModel.m, ident);
        assert_eq!(state.mTrackingToComponentLocal.m, ident);

        let visible = vr::EVRComponentProperty::IsVisible.0;
        let is_static = vr::EVRComponentProperty::IsStatic.0;
        assert_ne!(state.uProperties & visible, 0);
        assert_ne!(state.uProperties & is_static, 0);
    }

    #[test]
    fn get_component_state_supports_standard_pose_components() {
        // SteamVR registers standard pose components (for example, "tip")
        // for each controller. Some games (for example, the BigWalkVR mod)
        // mark a controller pose as invalid if GetComponentState fails for
        // these components. Thus GetComponentState must succeed for known
        // render models.
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();

        for (component, _) in POSE_COMPONENTS {
            let mut state = unsafe { std::mem::zeroed::<vr::RenderModel_ComponentState_t>() };
            assert!(
                rm.GetComponentState(
                    some_model.as_ptr(),
                    component.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    &mut state,
                ),
                "GetComponentState failed for standard pose component {component:?}"
            );
            assert!(rm.RenderModelHasComponent(some_model.as_ptr(), component.as_ptr()));

            // GetComponentState must fail for unknown models
            assert!(!rm.GetComponentState(
                c"unknown_model".as_ptr(),
                component.as_ptr(),
                ptr::null(),
                ptr::null(),
                &mut state,
            ));
        }

        // GetComponentState must fail for unknown component names
        let mut state = unsafe { std::mem::zeroed::<vr::RenderModel_ComponentState_t>() };
        assert!(!rm.GetComponentState(
            some_model.as_ptr(),
            c"not_a_real_component".as_ptr(),
            ptr::null(),
            ptr::null(),
            &mut state,
        ));
    }

    #[test]
    fn get_component_state_failure_writes_identity() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();
        let ident = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];

        for (model, component) in [
            (c"unknown_model".as_ptr(), c"tip".as_ptr()),
            (some_model.as_ptr(), c"not_a_real_component".as_ptr()),
            (ptr::null(), c"tip".as_ptr()),
        ] {
            let mut state = vr::RenderModel_ComponentState_t {
                mTrackingToComponentRenderModel: vr::HmdMatrix34_t { m: [[7.0; 4]; 3] },
                mTrackingToComponentLocal: vr::HmdMatrix34_t { m: [[7.0; 4]; 3] },
                uProperties: u32::MAX,
            };
            assert!(!rm.GetComponentState(model, component, ptr::null(), ptr::null(), &mut state));
            assert_eq!(state.mTrackingToComponentRenderModel.m, ident);
            assert_eq!(state.mTrackingToComponentLocal.m, ident);
            assert_eq!(state.uProperties, 0);
        }
    }

    #[test]
    fn pose_components_use_profile_transforms() {
        let rm = RenderModels::default();
        let get_state = |model: &CStr, component: &CStr| {
            let mut state = unsafe { std::mem::zeroed::<vr::RenderModel_ComponentState_t>() };
            assert!(rm.GetComponentStateForDevicePath(
                model.as_ptr(),
                component.as_ptr(),
                0,
                ptr::null(),
                &mut state,
            ));
            // Nothing to draw at the pose, so the render model transform stays identity.
            assert_eq!(
                state.mTrackingToComponentRenderModel.m,
                vr::HmdMatrix34_t::from(xr::Posef::IDENTITY).m
            );
            state.mTrackingToComponentLocal.m
        };

        // Touch tip: -37.4 degrees about X, mirrored offset per hand.
        let tip = get_state(c"oculus_quest2_controller_right", c"tip");
        let translation = [tip[0][3], tip[1][3], tip[2][3]];
        assert_eq!(translation, [-0.016694, -0.02522, 0.024687]);
        let (sin, cos) = (-37.4_f32).to_radians().sin_cos();
        assert!((tip[1][1] - cos).abs() < 1e-5, "{tip:?}");
        assert!((tip[2][1] - sin).abs() < 1e-5, "{tip:?}");
        let tip = get_state(c"oculus_quest2_controller_left", c"tip");
        assert_eq!(tip[0][3], 0.016694);

        // The driver prefix doesn't matter.
        let handgrip = get_state(
            c"{indexcontroller}valve_controller_knu_1_0_left",
            c"handgrip",
        );
        let index_handgrip = get_state(c"valve_controller_knu_1_0_left", c"handgrip");
        assert_eq!(handgrip, index_handgrip);
        assert_ne!(handgrip[2][3], 0.0);
    }

    #[test]
    fn profile_render_models_resolve() {
        let mut names = Vec::new();
        ProfileData::for_render_model(|name| {
            names.push(name.to_owned());
            false
        });
        assert!(!names.is_empty());

        let rm = RenderModels::default();
        for name in &names {
            assert!(
                rm.RenderModelHasComponent(name.as_ptr(), c"tip".as_ptr()),
                "{name:?} has no tip"
            );

            let has_mesh = rm.GetComponentRenderModelName(
                name.as_ptr(),
                DEFAULT_COMPONENT.as_ptr(),
                ptr::null_mut(),
                0,
            ) > 0;
            // No controller mesh for tracked hands.
            let hand = name.to_bytes().ends_with(b"shuttlecock");
            assert_eq!(has_mesh, !hand, "{name:?}");
        }

        let wmr = names
            .iter()
            .find(|name| name.to_bytes().ends_with(b"controller.obj"))
            .unwrap();
        let mut model: *mut vr::RenderModel_t = ptr::null_mut();
        assert_eq!(
            rm.LoadRenderModel_Async(wmr.as_ptr(), &mut model),
            vr::EVRRenderModelError::None
        );
        rm.FreeRenderModel(model);
    }

    #[test]
    fn fallback_mesh_sits_at_the_grip() {
        let rm = RenderModels::default();
        let load = |name: &CStr| {
            let mut model: *mut vr::RenderModel_t = ptr::null_mut();
            assert_eq!(
                rm.LoadRenderModel_Async(name.as_ptr(), &mut model),
                vr::EVRRenderModelError::None
            );
            let verts = unsafe {
                std::slice::from_raw_parts((*model).rVertexData, (*model).unVertexCount as usize)
            }
            .iter()
            .map(|v| Vec3::from_array(v.vPosition.v))
            .collect::<Vec<_>>();
            rm.FreeRenderModel(model);
            verts
        };

        let wmr = c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1642_1118_1\\controller.obj";
        let (profile, hand) = profile_for_render_model(wmr).unwrap();
        // Simple controllers' raw pose is the grip, where the generic mesh is drawn for them.
        let generic = load(c"generic_controller");
        let at_wmr_raw = load(wmr);
        assert_eq!(generic.len(), at_wmr_raw.len());
        for (generic, at_raw) in generic.iter().zip(at_wmr_raw) {
            let at_grip = profile.hand_offset(hand).transform_point3(at_raw);
            assert!(at_grip.abs_diff_eq(*generic, 1e-5), "{at_grip} {generic}");
        }
    }

    /// With SteamVR installed, its models load with their textures: from its own directory
    /// and from its drivers', by every spelling of the name, components included.
    #[test]
    fn steamvr_models_and_textures() {
        use_fake_steam();
        let rm = RenderModels::default();
        let load = |name: &CStr| {
            let mut out: *mut vr::RenderModel_t = ptr::null_mut();
            assert_eq!(
                rm.LoadRenderModel_Async(name.as_ptr(), &mut out),
                vr::EVRRenderModelError::None,
                "{name:?}"
            );
            unsafe { *out }
        };
        let texture = |id: vr::TextureID_t| {
            let mut out: *mut vr::RenderModel_TextureMap_t = ptr::null_mut();
            assert_eq!(
                rm.LoadTexture_Async(id, &mut out),
                vr::EVRRenderModelError::None
            );
            let map = unsafe { *out };
            let pixels = unsafe {
                std::slice::from_raw_parts(
                    map.rubTextureMapData,
                    map.unWidth as usize * map.unHeight as usize * 4,
                )
            };
            ((map.unWidth, map.unHeight), pixels.to_vec())
        };
        // Red, green over blue, white; the PNG and the TGA hold the same image.
        let image = (
            (2, 2),
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        );

        let model = load(c"test_controller");
        assert_eq!((model.unVertexCount, model.unTriangleCount), (4, 2));
        assert!(model.diffuseTextureId > 0);
        let verts = unsafe { std::slice::from_raw_parts(model.rVertexData, 4) };
        // OBJ texture coordinates count from the bottom, OpenVR's from the top.
        assert_eq!(verts[0].rfTextureCoord, [0.0, 1.0]);
        assert_eq!(verts[2].rfTextureCoord, [1.0, 0.0]);
        assert_eq!(texture(model.diffuseTextureId), image);

        // A driver's model, with a TGA texture, by every spelling.
        for name in [
            c"{testdrv}drv_model",
            c"{testdrv}/rendermodels/drv_model",
            c"drv_model",
        ] {
            let model = load(name);
            assert_eq!(model.unVertexCount, 4, "{name:?}");
            assert_eq!(texture(model.diffuseTextureId), image, "{name:?}");
        }

        // A component's mesh, and the model's files.
        assert_eq!(
            load(c"test_controller/test_controller_body").unVertexCount,
            4
        );
        let string = |thumbnail: bool| {
            let mut buf = [0 as std::ffi::c_char; 512];
            let mut err = vr::EVRRenderModelError::NotSupported;
            let get = if thumbnail {
                vr::IVRRenderModels006_Interface::GetRenderModelThumbnailURL
            } else {
                vr::IVRRenderModels006_Interface::GetRenderModelOriginalPath
            };
            let len = get(
                &rm,
                c"test_controller".as_ptr(),
                buf.as_mut_ptr(),
                512,
                &mut err,
            );
            assert!(len > 0);
            assert_eq!(err, vr::EVRRenderModelError::None);
            unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        let path = string(false);
        assert!(
            path.ends_with("/test_controller/test_controller.obj"),
            "{path}"
        );
        let url = string(true);
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.ends_with("/test_controller_thumbnail.png"), "{url}");
        assert!(
            rm.RenderModelHasComponent(c"test_controller".as_ptr(), DEFAULT_COMPONENT.as_ptr())
        );

        // Models SteamVR doesn't have fall back to the bundled ones, and paths aren't names.
        let bundled = load(c"{indexcontroller}valve_controller_knu_1_0_left");
        assert_eq!(bundled.diffuseTextureId, 0);
        let mut out: *mut vr::RenderModel_t = ptr::null_mut();
        for name in [
            c"../../etc/passwd",
            c"{../x}drv_model",
            c"test_controller/../x",
        ] {
            assert_eq!(
                rm.LoadRenderModel_Async(name.as_ptr(), &mut out),
                vr::EVRRenderModelError::InvalidArg,
                "{name:?}"
            );
        }
        forget_fake_steam();
    }

    /// Points the SteamVR lookup at the test install for the current thread.
    fn use_fake_steam() {
        crate::input::TEST_ENV.with_borrow_mut(|env| {
            env.insert(
                "STEAM_COMPAT_CLIENT_INSTALL_PATH",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/input_data/fake_steam").into(),
            )
        });
    }

    fn forget_fake_steam() {
        crate::input::TEST_ENV
            .with_borrow_mut(|env| env.remove("STEAM_COMPAT_CLIENT_INSTALL_PATH"));
    }

    #[test]
    fn steamvr_components_move_with_input() {
        use crate::input::{ComponentInput, DynSubpath};
        use std::collections::HashMap;
        use vr::EVRComponentProperty as Prop;

        use_fake_steam();
        let rm = RenderModels::default();
        let model = c"test_controller";

        // Every component the JSON lists, in name order, and each part's mesh under the model.
        let names: Vec<String> = (0..rm.GetComponentCount(model.as_ptr()))
            .map(|index| {
                let mut buf = [0 as std::ffi::c_char; 64];
                assert!(rm.GetComponentName(model.as_ptr(), index, buf.as_mut_ptr(), 64) > 0);
                unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "body",
                "button_a",
                "button_enter",
                "dpad",
                "scroll_wheel",
                "shuttle",
                "thumbstick",
                "tip",
                "trackpad",
                "trackpad_touch",
                "trigger"
            ]
        );
        let component_model = |component: &CStr| {
            let mut buf = [0 as std::ffi::c_char; 128];
            let len = rm.GetComponentRenderModelName(
                model.as_ptr(),
                component.as_ptr(),
                buf.as_mut_ptr(),
                128,
            );
            (len > 0).then(|| {
                unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
        };
        assert_eq!(
            component_model(c"trigger").as_deref(),
            Some("test_controller/test_controller_trigger")
        );
        assert_eq!(component_model(c"tip"), None);
        let mut out: *mut vr::RenderModel_t = ptr::null_mut();
        assert_eq!(
            rm.LoadRenderModel_Async(
                c"test_controller/test_controller_trigger".as_ptr(),
                &mut out
            ),
            vr::EVRRenderModelError::None
        );
        rm.FreeRenderModel(out);
        assert!(rm.RenderModelHasComponent(model.as_ptr(), c"trigger".as_ptr()));
        assert!(rm.RenderModelHasComponent(model.as_ptr(), DEFAULT_COMPONENT.as_ptr()));
        assert!(!rm.RenderModelHasComponent(model.as_ptr(), c"nope".as_ptr()));
        assert_eq!(
            rm.GetComponentButtonMask(model.as_ptr(), c"trigger".as_ptr()),
            1 << vr::EVRButtonId::SteamVR_Trigger.0
        );
        assert_eq!(
            rm.GetComponentButtonMask(model.as_ptr(), c"button_a".as_ptr()),
            1 << vr::EVRButtonId::A.0
        );
        assert_eq!(
            rm.GetComponentButtonMask(model.as_ptr(), c"thumbstick".as_ptr()),
            1 << vr::EVRButtonId::Axis0.0
        );
        assert_eq!(
            rm.GetComponentButtonMask(model.as_ptr(), c"tip".as_ptr()),
            0
        );

        // The state for given inputs, through the free function with a fake reader.
        type Matrix = [[f32; 4]; 3];
        let state = |component: &CStr,
                     inputs: &[(DynSubpath, ComponentInput)],
                     scroll: bool|
         -> (Matrix, Matrix, u32) {
            let inputs: HashMap<DynSubpath, ComponentInput> = inputs.iter().copied().collect();
            let read = |hand: Hand, subpath: DynSubpath| {
                assert_eq!(hand, Hand::Left);
                inputs.get(&subpath).copied().unwrap_or_default()
            };
            let state = component_state(model, component, Some(Hand::Left), &read, scroll)
                .unwrap_or_else(|| panic!("no state for {component:?}"));
            (
                state.mTrackingToComponentRenderModel.m,
                state.mTrackingToComponentLocal.m,
                state.uProperties,
            )
        };
        let identity = vr::HmdMatrix34_t::from(xr::Posef::IDENTITY).m;
        let close = |a: Matrix, b: Matrix| {
            a.iter()
                .flatten()
                .zip(b.iter().flatten())
                .all(|(a, b)| (a - b).abs() < 1e-5)
        };
        let input = |value: f32, pressed: bool, touched: bool| ComponentInput {
            active: true,
            value,
            xy: glam::Vec2::ZERO,
            pressed,
            touched,
        };
        let xy_input = |x: f32, y: f32, pressed: bool, touched: bool| ComponentInput {
            active: true,
            value: 0.0,
            xy: glam::Vec2::new(x, y),
            pressed,
            touched,
        };
        let moved = |transform: Mat4| hmd_matrix(transform).m;

        // Static parts, including a pose one and a motion we don't know.
        let is_static = (Prop::IsVisible | Prop::IsStatic).0;
        assert_eq!(state(c"body", &[], false), (identity, identity, is_static));
        let (render, local, props) = state(c"tip", &[], false);
        assert_eq!(render, identity);
        assert_eq!([local[0][3], local[1][3], local[2][3]], [0.0, -0.01, 0.02]);
        assert_eq!(props, is_static);
        assert_eq!(state(c"shuttle", &[], false).2, is_static);

        // rotate_xyz is applied Z, Y, X: the Vive trackpad's local frame has -Z along the pad's
        // normal, which its motion frame tilts 6.55 degrees back from +Y.
        let (_, local, _) = state(c"trackpad", &[], false);
        let out = [-local[0][2], -local[1][2], -local[2][2]];
        let (sin, cos) = 6.55_f32.to_radians().sin_cos();
        assert!(
            out[0].abs() < 1e-5 && (out[1] - cos).abs() < 1e-5 && (out[2] - sin).abs() < 1e-5,
            "{out:?}"
        );

        // A trigger at rest, half pulled and clicked: rotation about X through the pivot.
        let (render, local, props) = state(c"trigger", &[], false);
        assert_eq!(render, identity);
        assert_eq!(props, Prop::IsVisible.0);
        assert_eq!([local[0][3], local[1][3], local[2][3]], [0.0, -0.04, 0.06]);
        let rotated = |degrees: f32, pivot: Vec3| {
            moved(
                Mat4::from_translation(pivot)
                    * Mat4::from_rotation_x(degrees.to_radians())
                    * Mat4::from_translation(-pivot),
            )
        };
        let pivot = Vec3::new(0.0, -0.02, 0.05);
        let (render, local2, props) = state(
            c"trigger",
            &[(DynSubpath::Trigger, input(0.5, false, true))],
            false,
        );
        assert!(close(render, rotated(-9.0, pivot)), "{render:?}");
        assert_eq!(local2, local, "the local frame doesn't move with the part");
        assert_eq!(props, (Prop::IsVisible | Prop::IsTouched).0);
        let (render, _, props) = state(
            c"trigger",
            &[(DynSubpath::Trigger, input(0.95, true, true))],
            false,
        );
        assert!(close(render, rotated(-20.0, pivot)), "{render:?}");
        assert_eq!(
            props,
            (Prop::IsVisible | Prop::IsTouched | Prop::IsPressed).0
        );

        // A button sinks along its axis while pressed.
        let (render, _, props) = state(
            c"button_a",
            &[(DynSubpath::A, input(1.0, true, true))],
            false,
        );
        assert!(close(
            render,
            moved(Mat4::from_translation(Vec3::new(0.0, -0.002, 0.0)))
        ));
        assert_eq!(
            props,
            (Prop::IsVisible | Prop::IsTouched | Prop::IsPressed).0
        );
        assert_eq!(state(c"button_a", &[], false).0, identity);
        // SteamVR's system button of the left Touch controller is OpenXR's menu.
        let (render, _, _) = state(
            c"button_enter",
            &[(DynSubpath::Menu, input(1.0, true, true))],
            false,
        );
        assert!(close(
            render,
            moved(Mat4::from_translation(Vec3::new(0.0, -0.001, 0.0)))
        ));

        // A stick pushed right tilts about its Z, pushed forward about its X, and sinks when
        // clicked.
        let tilt = |x: f32, y: f32, degrees: f32, center: Vec3, sink: f32| {
            moved(
                Mat4::from_translation(center + Vec3::new(0.0, sink, 0.0))
                    * Mat4::from_rotation_x((-degrees * y).to_radians())
                    * Mat4::from_rotation_z((-degrees * x).to_radians())
                    * Mat4::from_translation(-center),
            )
        };
        let center = Vec3::new(0.0, 0.0, 0.05);
        let (render, _, _) = state(
            c"thumbstick",
            &[(DynSubpath::Thumbstick, xy_input(1.0, 0.0, false, true))],
            false,
        );
        assert!(
            close(render, tilt(1.0, 0.0, 20.0, center, 0.0)),
            "{render:?}"
        );
        let (render, _, props) = state(
            c"thumbstick",
            &[(DynSubpath::Thumbstick, xy_input(0.0, 0.5, true, true))],
            false,
        );
        assert!(
            close(render, tilt(0.0, 0.5, 20.0, center, -0.001)),
            "{render:?}"
        );
        assert_eq!(
            props,
            (Prop::IsVisible | Prop::IsTouched | Prop::IsPressed).0
        );
        // A pad only tilts under a press, toward the touch.
        let touched_pad = [(DynSubpath::Trackpad, xy_input(1.0, 0.0, false, true))];
        assert_eq!(state(c"trackpad", &touched_pad, false).0, identity);
        let pressed_pad = [(DynSubpath::Trackpad, xy_input(1.0, 0.0, true, true))];
        assert_ne!(state(c"trackpad", &pressed_pad, false).0, identity);
        // The dpad is a stick driven by its four buttons.
        let (render, _, props) = state(
            c"dpad",
            &[(DynSubpath::DpadLeft, input(1.0, true, true))],
            false,
        );
        let center = Vec3::new(0.0, 0.0, 0.04);
        assert!(
            close(render, tilt(-1.0, 0.0, 7.0, center, -0.001)),
            "{render:?}"
        );
        assert_eq!(
            props,
            (Prop::IsVisible | Prop::IsTouched | Prop::IsPressed).0
        );

        // The touch marker shows where the pad is touched, and the scroll wheel in scroll mode
        // only, where the pad hides.
        assert_eq!(state(c"trackpad_touch", &[], false).2, 0);
        let (render, _, props) = state(
            c"trackpad_touch",
            &[(DynSubpath::Trackpad, xy_input(0.5, 1.0, false, true))],
            false,
        );
        let marker = moved(Mat4::from_translation(Vec3::new(0.005, 0.0, -0.02)));
        assert!(close(render, marker), "{render:?}");
        assert_eq!(props, (Prop::IsVisible | Prop::IsTouched).0);
        assert_eq!(state(c"trackpad", &[], false).2, Prop::IsVisible.0);
        assert_eq!(state(c"trackpad", &[], true).2, 0);
        assert_eq!(state(c"scroll_wheel", &[], false).2, 0);
        let (render, _, props) = state(
            c"scroll_wheel",
            &[(DynSubpath::Trackpad, xy_input(0.0, 0.5, false, true))],
            true,
        );
        assert_eq!(
            props,
            (Prop::IsVisible | Prop::IsTouched | Prop::IsScrolled).0
        );
        assert!(
            close(render, rotated(-20.0, Vec3::new(0.0, 0.0, 0.05))),
            "{render:?}"
        );
        // Untouched, the wheel doesn't follow the pad's resting value.
        let (render, _, _) = state(
            c"scroll_wheel",
            &[(DynSubpath::Trackpad, xy_input(0.0, 0.5, false, false))],
            true,
        );
        assert_eq!(render, identity);

        // Through the interface, with no input interface the parts rest; the scroll mode comes
        // from the app's state.
        let mut out = vr::RenderModel_ComponentState_t::default();
        assert!(rm.GetComponentState(
            model.as_ptr(),
            c"trigger".as_ptr(),
            ptr::null(),
            ptr::null(),
            &mut out
        ));
        assert_eq!(out.mTrackingToComponentRenderModel.m, identity);
        let mode = vr::RenderModel_ControllerMode_State_t {
            bScrollWheelVisible: true,
        };
        assert!(rm.GetComponentStateForDevicePath(
            model.as_ptr(),
            c"scroll_wheel".as_ptr(),
            0,
            &mode,
            &mut out
        ));
        assert_eq!(out.uProperties, (Prop::IsVisible | Prop::IsScrolled).0);
        forget_fake_steam();
    }

    /// The models our profiles name, from a real SteamVR install:
    /// `STEAM_COMPAT_CLIENT_INSTALL_PATH=~/.steam/steam cargo test -- --ignored real_steamvr`.
    #[test]
    #[ignore = "needs a SteamVR install"]
    fn real_steamvr_models() {
        let steam = std::env::var("STEAM_COMPAT_CLIENT_INSTALL_PATH").expect("no Steam path");
        crate::input::TEST_ENV
            .with_borrow_mut(|env| env.insert("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam.into()));
        let rm = RenderModels::default();
        // With the components SteamVR's JSON lists, if it has one.
        for (name, components) in [
            (c"{indexcontroller}valve_controller_knu_1_0_left", 21),
            (c"{indexcontroller}valve_controller_knu_1_0_right", 21),
            (c"oculus_quest2_controller_left", 16),
            (c"oculus_quest_plus_controller_right", 16),
            (c"oculus_quest_pro_controller_left", 16),
            (c"vr_controller_vive_1_5", 24),
            (c"generic_controller", 1),
            (c"generic_hmd", 1),
            (c"vr_tracker_vive_3_0", 5),
            (c"{frame_controller}frame_controller_left", 16),
            (c"{vrlink}/rendermodels/shuttlecock", 1),
            (c"vive_focus3_controller_right", 14),
            (c"pico_4_controller_left", 15),
        ] {
            let mut out: *mut vr::RenderModel_t = ptr::null_mut();
            assert_eq!(
                rm.LoadRenderModel_Async(name.as_ptr(), &mut out),
                vr::EVRRenderModelError::None,
                "{name:?}"
            );
            let model = unsafe { *out };
            assert!(
                model.unTriangleCount > 100,
                "{name:?}: {}",
                model.unTriangleCount
            );
            let mut tex: *mut vr::RenderModel_TextureMap_t = ptr::null_mut();
            assert_eq!(
                rm.LoadTexture_Async(model.diffuseTextureId, &mut tex),
                vr::EVRRenderModelError::None
            );
            let tex = unsafe { *tex };
            println!(
                "{name:?}: {} triangles, texture {} {}x{}",
                model.unTriangleCount, model.diffuseTextureId, tex.unWidth, tex.unHeight
            );
            assert!(
                model.diffuseTextureId > 0 && tex.unWidth > 1,
                "{name:?} has no texture"
            );

            // Every component has a state, and every part a mesh that loads.
            let count = rm.GetComponentCount(name.as_ptr());
            assert_eq!(count, components, "{name:?}");
            for index in 0..count {
                let mut buf = [0 as std::ffi::c_char; 128];
                assert!(rm.GetComponentName(name.as_ptr(), index, buf.as_mut_ptr(), 128) > 0);
                let component = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_owned();
                let mut state = vr::RenderModel_ComponentState_t::default();
                assert!(
                    rm.GetComponentState(
                        name.as_ptr(),
                        component.as_ptr(),
                        ptr::null(),
                        ptr::null(),
                        &mut state
                    ),
                    "{name:?}/{component:?}"
                );
                let mut part = [0 as std::ffi::c_char; 256];
                let len = rm.GetComponentRenderModelName(
                    name.as_ptr(),
                    component.as_ptr(),
                    part.as_mut_ptr(),
                    256,
                );
                let part = (len > 0).then(|| unsafe { CStr::from_ptr(part.as_ptr()) }.to_owned());
                if let Some(part) = &part {
                    let mut out: *mut vr::RenderModel_t = ptr::null_mut();
                    assert_eq!(
                        rm.LoadRenderModel_Async(part.as_ptr(), &mut out),
                        vr::EVRRenderModelError::None,
                        "{part:?}"
                    );
                    rm.FreeRenderModel(out);
                }
                println!(
                    "  {component:?}: mesh {part:?}, properties {:#x}, buttons {:#x}",
                    state.uProperties,
                    rm.GetComponentButtonMask(name.as_ptr(), component.as_ptr())
                );
            }
        }
    }

    #[test]
    fn load_and_free_texture_contract() {
        let rm = RenderModels::default();

        // null out ptr → InvalidArg
        assert_eq!(
            rm.LoadTexture_Async(0, ptr::null_mut()),
            vr::EVRRenderModelError::InvalidArg
        );

        // negative tex id → InvalidTexture
        let mut out: *mut vr::RenderModel_TextureMap_t = ptr::null_mut();
        assert_eq!(
            rm.LoadTexture_Async(-1, &mut out as *mut _),
            vr::EVRRenderModelError::InvalidTexture
        );
        assert!(out.is_null());

        // valid
        let mut out: *mut vr::RenderModel_TextureMap_t = ptr::null_mut();
        assert_eq!(
            rm.LoadTexture_Async(0, &mut out as *mut _),
            vr::EVRRenderModelError::None
        );
        assert!(!out.is_null());

        {
            let data = rm.data.lock().unwrap();
            assert!(data.loaded_textures.contains_key(&(out as usize)));
        }

        rm.FreeTexture(out);

        {
            let data = rm.data.lock().unwrap();
            assert!(!data.loaded_textures.contains_key(&(out as usize)));
        }

        // freeing already freed element → noop
        rm.FreeTexture(out);

        // does not dereference garbage pointer
        let garbage_pointer = 111111usize as *mut vr::RenderModel_TextureMap_t;
        rm.FreeTexture(garbage_pointer);
    }

    #[test]
    fn load_and_free_render_model_contract() {
        let rm = RenderModels::default();
        let some_model = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();

        // null out ptr → InvalidArg
        assert_eq!(
            rm.LoadRenderModel_Async(some_model.as_ptr(), ptr::null_mut()),
            vr::EVRRenderModelError::InvalidArg
        );

        // null name → InvalidArg
        let mut out: *mut vr::RenderModel_t = ptr::null_mut();
        assert_eq!(
            rm.LoadRenderModel_Async(ptr::null(), &mut out as *mut _),
            vr::EVRRenderModelError::InvalidArg
        );

        // unknown name → InvalidArg
        let mut out: *mut vr::RenderModel_t = ptr::null_mut();
        assert_eq!(
            rm.LoadRenderModel_Async(c"unknown_model".as_ptr(), &mut out as *mut _),
            vr::EVRRenderModelError::InvalidArg
        );
        assert!(out.is_null());

        // valid
        let name = RENDER_MODELS.keys().next().unwrap();
        let braced = CString::new(format!("{{driver}}{name}")).unwrap();
        let mut out: *mut vr::RenderModel_t = ptr::null_mut();
        assert_eq!(
            rm.LoadRenderModel_Async(braced.as_ptr(), &mut out as *mut _),
            vr::EVRRenderModelError::None
        );
        assert!(!out.is_null());

        {
            let data = rm.data.lock().unwrap();
            assert!(data.loaded_model_data.contains_key(&(out as usize)));
        }

        rm.FreeRenderModel(out);

        {
            let data = rm.data.lock().unwrap();
            assert!(!data.loaded_model_data.contains_key(&(out as usize)));
        }

        // freeing already freed element → noop
        rm.FreeRenderModel(out);

        // does not dereference garbage pointer
        let garbage_pointer = 111111usize as *mut vr::RenderModel_t;
        rm.FreeRenderModel(garbage_pointer);
    }

    // Parses every embedded model, which takes 15 minutes under miri and touches no unsafe code.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn embedded_models_parse_and_have_reasonable_geometry() {
        for (name, asset) in RENDER_MODELS.iter() {
            let c_name = CString::new(*name).unwrap();
            let model = OwnedRenderModel::load(c_name.as_c_str(), asset)
                .unwrap_or_else(|e| panic!("failed to load embedded model {name}: {e:?}"));

            assert!(!model.verts.is_empty(), "{name}: has no vertex buffer");
            assert!(!model.indices.is_empty(), "{name}: has no index buffer");
            assert_eq!(
                model.indices.len() % 3,
                0,
                "{name}: indices not a multiple of 3"
            );

            let vlen = model.verts.len();
            assert!(
                model.indices.iter().all(|&i| (i as usize) < vlen),
                "{name}: found out-of-range index"
            );

            for (vi, v) in model.verts.iter().enumerate() {
                let p = v.vPosition.v;
                let n = v.vNormal.v;

                assert!(
                    p.iter().all(|f| f.is_finite()),
                    "{name}: vertex {vi} has non-finite position"
                );
                assert!(
                    n.iter().all(|f| f.is_finite()),
                    "{name}: vertex {vi} has non-finite normal"
                );

                let len2 = n[0] * n[0] + n[1] * n[1] + n[2] * n[2];
                assert!(
                    (len2 - 1.0).abs() <= 1e-2,
                    "{name}: vertex {vi} normal not ~unit length (len^2={len2})"
                );
            }

            // RenderModel_t contract: triangle count matches indices/3
            let rm_t = model.to_render_model_t();
            assert_eq!(
                rm_t.unTriangleCount as usize,
                model.indices.len() / 3,
                "{name}: triangle count mismatch"
            );
            assert_eq!(
                rm_t.unVertexCount as usize,
                model.verts.len(),
                "{name}: vertex count mismatch"
            );
        }
    }

    #[test]
    fn sync_load_002() {
        use vr::IVRRenderModels002On004;
        let rm = RenderModels::default();
        let name = CString::new(*RENDER_MODELS.keys().next().unwrap()).unwrap();

        let mut model: *mut vr::RenderModel_t = ptr::null_mut();
        assert!(!IVRRenderModels002On004::LoadRenderModel(
            &rm,
            c"unknown_model".as_ptr(),
            &mut model
        ));
        assert!(IVRRenderModels002On004::LoadRenderModel(
            &rm,
            name.as_ptr(),
            &mut model
        ));
        assert!(!model.is_null());

        let mut tex: *mut vr::RenderModel_TextureMap_t = ptr::null_mut();
        assert!(IVRRenderModels002On004::LoadTexture(
            &rm,
            unsafe { (*model).diffuseTextureId },
            &mut tex
        ));
        assert!(!tex.is_null());

        rm.FreeTexture(tex);
        rm.FreeRenderModel(model);
        let data = rm.data.lock().unwrap();
        assert!(data.loaded_model_data.is_empty());
        assert!(data.loaded_textures.is_empty());
    }
}
