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

use crate::input::{BoundPoseType, ProfileData};
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

/// Whether GetComponentState and RenderModelHasComponent know this component.
fn has_component(render_model_name: &CStr, component_name: &CStr) -> bool {
    if component_name == DEFAULT_COMPONENT {
        get_render_model_data(render_model_name).is_some()
    } else {
        // Models without a mesh, like the hand tracking one, still have the pose components.
        pose_component(component_name).is_some()
            && (get_render_model_data(render_model_name).is_some()
                || profile_for_render_model(render_model_name).is_some())
    }
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

#[derive(Default, macros::InterfaceImpl)]
#[interface = "IVRRenderModels"]
#[versions(006, 005, 004, 002)]
pub struct RenderModels {
    vtables: Vtables,
    data: Mutex<RenderModelData>,
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
    fn GetRenderModelOriginalPath(
        &self,
        _: *const std::ffi::c_char,
        buffer: *mut std::ffi::c_char,
        buffer_size: u32,
        error: *mut vr::EVRRenderModelError,
    ) -> u32 {
        crate::warn_unimplemented!("GetRenderModelOriginalPath");
        if !buffer.is_null() && buffer_size > 0 {
            unsafe { *buffer = 0 };
        }
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::EVRRenderModelError::NotSupported;
        }
        0
    }
    fn GetRenderModelThumbnailURL(
        &self,
        _: *const std::ffi::c_char,
        buffer: *mut std::ffi::c_char,
        buffer_size: u32,
        error: *mut vr::EVRRenderModelError,
    ) -> u32 {
        crate::warn_unimplemented!("GetRenderModelThumbnailURL");
        if !buffer.is_null() && buffer_size > 0 {
            unsafe { *buffer = 0 };
        }
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::EVRRenderModelError::NotSupported;
        }
        0
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
    fn GetComponentState(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        _: *const vr::VRControllerState_t,
        _: *const vr::RenderModel_ControllerMode_State_t,
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

        if !has_component(render_model_name, component_name) {
            return false;
        }

        // Our meshes are static and have their offsets baked in, so only the pose components
        // aren't at the device's raw pose.
        if let Some(transform) = pose_component(component_name)
            .and_then(|ty| pose_component_transform(render_model_name, ty))
        {
            state.mTrackingToComponentRenderModel = hmd_matrix(transform);
            state.mTrackingToComponentLocal = hmd_matrix(transform);
        }
        state.uProperties =
            (vr::EVRComponentProperty::IsVisible | vr::EVRComponentProperty::IsStatic).0;

        true
    }
    fn GetComponentStateForDevicePath(
        &self,
        render_model_name: *const std::ffi::c_char,
        component_name: *const std::ffi::c_char,
        _: vr::VRInputValueHandle_t,
        _: *const vr::RenderModel_ControllerMode_State_t,
        state: *mut vr::RenderModel_ComponentState_t,
    ) -> bool {
        // static models only, don't care about state
        self.GetComponentState(
            render_model_name,
            component_name,
            ptr::null(),
            ptr::null(),
            state,
        )
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

        if component_name != DEFAULT_COMPONENT || get_render_model_data(render_model_name).is_none()
        {
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
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> u64 {
        crate::warn_unimplemented!("GetComponentButtonMask");
        0
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
        if render_model_name.count_bytes() == 0 {
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

        let tex = OwnedTexture::new_dummy();
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
        let Some(bytes) = get_render_model_data(render_model_name) else {
            warn!("render model name {render_model_name:?} did not resolve to a valid resource");
            return vr::EVRRenderModelError::InvalidArg;
        };

        let mut model_data = match OwnedRenderModel::load(render_model_name, bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!("failed to load rendermodel {render_model_name:?}: {e:?}");
                return e;
            }
        };
        if let Some(transform) = fallback_mesh_transform(render_model_name) {
            model_data.transform(transform);
        }

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

            // not needed as we don't use textures
            let texcoord = [0.; 2];

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

        Ok(Self { verts, indices })
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
            diffuseTextureId: 0, // dummy texture
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
            assert_eq!(
                state.mTrackingToComponentRenderModel.m,
                state.mTrackingToComponentLocal.m
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
