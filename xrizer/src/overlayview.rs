use openvr as vr;

#[derive(Default, macros::InterfaceImpl)]
#[interface = "IVROverlayView"]
#[versions(003)]
pub struct OverlayView {
    vtables: Vtables,
}

impl vr::IVROverlayView003_Interface for OverlayView {
    fn IsViewingPermitted(&self, _: vr::VROverlayHandle_t) -> bool {
        crate::warn_unimplemented!("IsViewingPermitted");
        false
    }
    fn PostOverlayEvent(&self, _: vr::VROverlayHandle_t, _: *const vr::VREvent_t) {
        crate::warn_unimplemented!("PostOverlayEvent");
    }
    fn ReleaseOverlayView(&self, _: *mut vr::VROverlayView_t) -> vr::EVROverlayError {
        vr::EVROverlayError::InvalidHandle
    }
    fn AcquireOverlayView(
        &self,
        _: vr::VROverlayHandle_t,
        _: *mut vr::VRNativeDevice_t,
        _: *mut vr::VROverlayView_t,
        _: u32,
    ) -> vr::EVROverlayError {
        crate::warn_unimplemented!("AcquireOverlayView");
        vr::EVROverlayError::InvalidHandle
    }
}
