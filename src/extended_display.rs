use crate::openxr_data::RealOpenXrData;
use openvr as vr;
use openxr as xr;
use std::sync::Arc;

#[derive(macros::InterfaceImpl)]
#[interface = "IVRExtendedDisplay"]
#[versions(001)]
pub struct ExtendedDisplay {
    vtables: Vtables,
    openxr: Arc<RealOpenXrData>,
}

impl ExtendedDisplay {
    pub fn new(openxr: Arc<RealOpenXrData>) -> Self {
        Self {
            vtables: Default::default(),
            openxr,
        }
    }

    /// Recommended size of a single eye.
    fn eye_size(&self) -> (u32, u32) {
        let views = self
            .openxr
            .instance
            .enumerate_view_configuration_views(
                self.openxr.system_id,
                xr::ViewConfigurationType::PRIMARY_STEREO,
            )
            .unwrap();
        (
            views[0].recommended_image_rect_width,
            views[0].recommended_image_rect_height,
        )
    }
}

// There is no real display window, so report a side by side layout of both eyes.
impl vr::IVRExtendedDisplay001_Interface for ExtendedDisplay {
    fn GetWindowBounds(&self, x: *mut i32, y: *mut i32, width: *mut u32, height: *mut u32) {
        let (eye_width, eye_height) = self.eye_size();
        if let Some(x) = unsafe { x.as_mut() } {
            *x = 0;
        }
        if let Some(y) = unsafe { y.as_mut() } {
            *y = 0;
        }
        if let Some(w) = unsafe { width.as_mut() } {
            *w = eye_width * 2;
        }
        if let Some(h) = unsafe { height.as_mut() } {
            *h = eye_height;
        }
    }

    fn GetEyeOutputViewport(
        &self,
        eye: vr::EVREye,
        x: *mut u32,
        y: *mut u32,
        width: *mut u32,
        height: *mut u32,
    ) {
        let (eye_width, eye_height) = self.eye_size();
        let eye_x = match eye {
            vr::EVREye::Left => 0,
            vr::EVREye::Right => eye_width,
        };
        if let Some(x) = unsafe { x.as_mut() } {
            *x = eye_x;
        }
        if let Some(y) = unsafe { y.as_mut() } {
            *y = 0;
        }
        if let Some(w) = unsafe { width.as_mut() } {
            *w = eye_width;
        }
        if let Some(h) = unsafe { height.as_mut() } {
            *h = eye_height;
        }
    }

    fn GetDXGIOutputInfo(&self, adapter_index: *mut i32, adapter_output_index: *mut i32) {
        crate::warn_unimplemented!("GetDXGIOutputInfo");
        if let Some(i) = unsafe { adapter_index.as_mut() } {
            *i = 0;
        }
        if let Some(i) = unsafe { adapter_output_index.as_mut() } {
            *i = 0;
        }
    }
}
