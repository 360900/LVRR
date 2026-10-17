use log::debug;
use openvr as vr;
use std::ffi::CStr;
use std::os::raw::c_char;

#[derive(Default, macros::InterfaceImpl)]
#[interface = "IVRDebug"]
#[versions(001)]
pub struct Debug {
    vtables: Vtables,
}

impl vr::IVRDebug001_Interface for Debug {
    fn EmitVrProfilerEvent(&self, message: *const c_char) -> vr::EVRDebugError {
        if !message.is_null() {
            let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
            debug!("Emitting VR profiler event: {message}");
        }
        vr::EVRDebugError::Success
    }

    fn BeginVrProfilerEvent(
        &self,
        handle_out: *mut vr::VrProfilerEventHandle_t,
    ) -> vr::EVRDebugError {
        if let Some(handle_out) = unsafe { handle_out.as_mut() } {
            *handle_out = 1;
        }
        vr::EVRDebugError::Success
    }

    fn FinishVrProfilerEvent(
        &self,
        handle: vr::VrProfilerEventHandle_t,
        message: *const c_char,
    ) -> vr::EVRDebugError {
        if !message.is_null() {
            let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
            debug!("Finishing VR profiler event {handle}: {message}");
        }
        vr::EVRDebugError::Success
    }

    fn DriverDebugRequest(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        request: *const c_char,
        response_buffer: *mut c_char,
        response_buffer_size: u32,
    ) -> u32 {
        if !request.is_null() {
            let request = unsafe { CStr::from_ptr(request) }.to_string_lossy();
            debug!("Driver debug request for device {device_index}: {request}");
        }

        if response_buffer.is_null() || response_buffer_size == 0 {
            return 0;
        }

        unsafe {
            *response_buffer = 0;
        }
        // Just the null terminator.
        1
    }
}
