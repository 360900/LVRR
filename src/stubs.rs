//! Interfaces nothing here can serve. Apps get them anyway, so one that doesn't check for null
//! keeps running, and every call reports that nothing is available rather than faking success.

use crate::clientcore::{Injected, Injector};
use crate::overlay::OverlayMan;
use log::debug;
use openvr as vr;
use std::borrow::Cow;
use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;

pub use control_panel::ControlPanel;
pub use driver_manager::DriverManager;
pub use io_buffer::IoBuffer;
pub use ipc_resource_manager::IpcResourceManagerClient;
pub use notifications::Notifications;
pub use resources::Resources;
pub use spatial_anchors::SpatialAnchors;
pub use tracked_camera::TrackedCamera;

/// Writes `value` to an out parameter the app may have left null.
fn write_out<T>(ptr: *mut T, value: T) {
    if let Some(out) = unsafe { ptr.as_mut() } {
        *out = value;
    }
}

/// Writes an empty string to a string buffer, if the app gave one.
fn write_empty(buffer: *mut c_char, size: u32) {
    if !buffer.is_null() && size > 0 {
        unsafe { *buffer = 0 };
    }
}

/// A string the app passed, for logging.
fn app_str<'a>(ptr: *const c_char) -> Cow<'a, str> {
    if ptr.is_null() {
        "(null)".into()
    } else {
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy()
    }
}

mod tracked_camera {
    use super::*;

    /// INVALID_TRACKED_CAMERA_HANDLE, a macro in the headers.
    const INVALID_HANDLE: vr::TrackedCameraHandle_t = 0;

    /// No device has a camera, like a headset without one in SteamVR.
    #[derive(macros::InterfaceImpl)]
    #[interface = "IVRTrackedCamera"]
    #[versions(006, 005, 004, 003, 001)]
    pub struct TrackedCamera {
        vtables: Vtables,
        /// Only stored and returned back to the app.
        tracking_space: Mutex<vr::ETrackingUniverseOrigin>,
    }

    impl Default for TrackedCamera {
        fn default() -> Self {
            Self {
                vtables: Default::default(),
                tracking_space: Mutex::new(vr::ETrackingUniverseOrigin::Seated),
            }
        }
    }

    impl TrackedCamera {
        fn unavailable() {
            crate::warn_once!("IVRTrackedCamera: no device has a camera");
        }

        /// Clears a frame header the app asked for, if it's one this header fits.
        fn clear_frame_header(header: *mut vr::CameraVideoStreamFrameHeader_t, size: u32) {
            if size as usize >= std::mem::size_of::<vr::CameraVideoStreamFrameHeader_t>() {
                write_out(header, Default::default());
            }
        }
    }

    impl vr::IVRTrackedCamera006_Interface for TrackedCamera {
        fn GetCameraErrorNameFromEnum(&self, error: vr::EVRTrackedCameraError) -> *const c_char {
            crate::enum_name("VRTrackedCameraError_", error)
        }
        fn HasCamera(
            &self,
            _: vr::TrackedDeviceIndex_t,
            has_camera: *mut bool,
        ) -> vr::EVRTrackedCameraError {
            Self::unavailable();
            write_out(has_camera, false);
            vr::EVRTrackedCameraError::None
        }
        fn GetCameraFrameSize(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: vr::EVRTrackedCameraFrameType,
            width: *mut u32,
            height: *mut u32,
            frame_buffer_size: *mut u32,
        ) -> vr::EVRTrackedCameraError {
            Self::unavailable();
            write_out(width, 0);
            write_out(height, 0);
            write_out(frame_buffer_size, 0);
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        }
        fn GetCameraIntrinsics(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: u32,
            _: vr::EVRTrackedCameraFrameType,
            focal_length: *mut vr::HmdVector2_t,
            center: *mut vr::HmdVector2_t,
        ) -> vr::EVRTrackedCameraError {
            Self::unavailable();
            write_out(focal_length, Default::default());
            write_out(center, Default::default());
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        }
        fn GetCameraProjection(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: u32,
            _: vr::EVRTrackedCameraFrameType,
            _: f32,
            _: f32,
            projection: *mut vr::HmdMatrix44_t,
        ) -> vr::EVRTrackedCameraError {
            Self::unavailable();
            write_out(projection, Default::default());
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        }
        fn AcquireVideoStreamingService(
            &self,
            _: vr::TrackedDeviceIndex_t,
            handle: *mut vr::TrackedCameraHandle_t,
        ) -> vr::EVRTrackedCameraError {
            Self::unavailable();
            write_out(handle, INVALID_HANDLE);
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        }
        fn ReleaseVideoStreamingService(
            &self,
            _: vr::TrackedCameraHandle_t,
        ) -> vr::EVRTrackedCameraError {
            // No handle was ever given out.
            vr::EVRTrackedCameraError::InvalidHandle
        }
        fn GetVideoStreamFrameBuffer(
            &self,
            _: vr::TrackedCameraHandle_t,
            _: vr::EVRTrackedCameraFrameType,
            _: *mut c_void,
            _: u32,
            header: *mut vr::CameraVideoStreamFrameHeader_t,
            header_size: u32,
        ) -> vr::EVRTrackedCameraError {
            Self::clear_frame_header(header, header_size);
            vr::EVRTrackedCameraError::InvalidHandle
        }
        fn GetVideoStreamTextureSize(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: vr::EVRTrackedCameraFrameType,
            bounds: *mut vr::VRTextureBounds_t,
            width: *mut u32,
            height: *mut u32,
        ) -> vr::EVRTrackedCameraError {
            Self::unavailable();
            write_out(bounds, Default::default());
            write_out(width, 0);
            write_out(height, 0);
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        }
        fn GetVideoStreamTextureD3D11(
            &self,
            _: vr::TrackedCameraHandle_t,
            _: vr::EVRTrackedCameraFrameType,
            _: *mut c_void,
            shader_resource_view: *mut *mut c_void,
            header: *mut vr::CameraVideoStreamFrameHeader_t,
            header_size: u32,
        ) -> vr::EVRTrackedCameraError {
            write_out(shader_resource_view, std::ptr::null_mut());
            Self::clear_frame_header(header, header_size);
            vr::EVRTrackedCameraError::InvalidHandle
        }
        fn GetVideoStreamTextureGL(
            &self,
            _: vr::TrackedCameraHandle_t,
            _: vr::EVRTrackedCameraFrameType,
            texture_id: *mut vr::glUInt_t,
            header: *mut vr::CameraVideoStreamFrameHeader_t,
            header_size: u32,
        ) -> vr::EVRTrackedCameraError {
            write_out(texture_id, 0);
            Self::clear_frame_header(header, header_size);
            vr::EVRTrackedCameraError::InvalidHandle
        }
        fn ReleaseVideoStreamTextureGL(
            &self,
            _: vr::TrackedCameraHandle_t,
            _: vr::glUInt_t,
        ) -> vr::EVRTrackedCameraError {
            vr::EVRTrackedCameraError::InvalidHandle
        }
        fn SetCameraTrackingSpace(&self, universe: vr::ETrackingUniverseOrigin) {
            *self.tracking_space.lock().unwrap() = universe;
        }
        fn GetCameraTrackingSpace(&self) -> vr::ETrackingUniverseOrigin {
            *self.tracking_space.lock().unwrap()
        }
    }

    impl vr::IVRTrackedCamera004On005 for TrackedCamera {
        fn GetCameraIntrinsics(
            &self,
            device_index: vr::TrackedDeviceIndex_t,
            frame_type: vr::EVRTrackedCameraFrameType,
            focal_length: *mut vr::HmdVector2_t,
            center: *mut vr::HmdVector2_t,
        ) -> vr::EVRTrackedCameraError {
            <Self as vr::IVRTrackedCamera006_Interface>::GetCameraIntrinsics(
                self,
                device_index,
                0,
                frame_type,
                focal_length,
                center,
            )
        }
        fn GetCameraProjection(
            &self,
            device_index: vr::TrackedDeviceIndex_t,
            frame_type: vr::EVRTrackedCameraFrameType,
            z_near: f32,
            z_far: f32,
            projection: *mut vr::HmdMatrix44_t,
        ) -> vr::EVRTrackedCameraError {
            <Self as vr::IVRTrackedCamera006_Interface>::GetCameraProjection(
                self,
                device_index,
                0,
                frame_type,
                z_near,
                z_far,
                projection,
            )
        }
    }

    // The 0.9.x API: bools for success, and frames handed out by pointer.
    impl vr::IVRTrackedCamera001On003 for TrackedCamera {
        fn HasCamera(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            Self::unavailable();
            false
        }
        fn GetCameraFirmwareDescription(
            &self,
            _: vr::TrackedDeviceIndex_t,
            buffer: *mut c_char,
            size: u32,
        ) -> bool {
            Self::unavailable();
            write_empty(buffer, size);
            false
        }
        fn GetCameraFrameDimensions(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: vr::ECameraVideoStreamFormat,
            width: *mut u32,
            height: *mut u32,
        ) -> bool {
            Self::unavailable();
            write_out(width, 0);
            write_out(height, 0);
            false
        }
        fn SetCameraVideoStreamFormat(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: vr::ECameraVideoStreamFormat,
        ) -> bool {
            Self::unavailable();
            false
        }
        fn GetCameraVideoStreamFormat(
            &self,
            _: vr::TrackedDeviceIndex_t,
        ) -> vr::ECameraVideoStreamFormat {
            Self::unavailable();
            vr::ECameraVideoStreamFormat::FORMAT_UNKNOWN
        }
        fn EnableCameraForStreaming(&self, _: vr::TrackedDeviceIndex_t, _: bool) -> bool {
            Self::unavailable();
            false
        }
        fn StartVideoStream(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            Self::unavailable();
            false
        }
        fn StopVideoStream(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            false
        }
        fn IsVideoStreamActive(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            false
        }
        fn GetVideoStreamElapsedTime(&self, _: vr::TrackedDeviceIndex_t) -> f32 {
            // What a stopped stream reports.
            0.0
        }
        fn GetVideoStreamFrame(
            &self,
            _: vr::TrackedDeviceIndex_t,
        ) -> *const vr::CameraVideoStreamFrame_t {
            // No frame available.
            std::ptr::null()
        }
        fn ReleaseVideoStreamFrame(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: *const vr::CameraVideoStreamFrame_t,
        ) -> bool {
            false
        }
        fn SetAutoExposure(&self, _: vr::TrackedDeviceIndex_t, _: bool) -> bool {
            false
        }
        fn PauseVideoStream(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            false
        }
        fn ResumeVideoStream(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            false
        }
        fn IsVideoStreamPaused(&self, _: vr::TrackedDeviceIndex_t) -> bool {
            false
        }
        fn GetCameraDistortion(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: f32,
            _: f32,
            output_u: *mut f32,
            output_v: *mut f32,
        ) -> bool {
            Self::unavailable();
            write_out(output_u, 0.0);
            write_out(output_v, 0.0);
            false
        }
        fn GetCameraProjection(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: f32,
            _: f32,
            _: f32,
            _: f32,
            projection: *mut vr::HmdMatrix44_t,
        ) -> bool {
            Self::unavailable();
            write_out(projection, Default::default());
            false
        }
    }
}

mod resources {
    use super::*;

    /// SteamVR's shared resources (icons, sounds, driver files) don't exist here.
    #[derive(Default, macros::InterfaceImpl)]
    #[interface = "IVRResources"]
    #[versions(001)]
    pub struct Resources {
        vtables: Vtables,
    }

    impl vr::IVRResources001_Interface for Resources {
        fn LoadSharedResource(&self, name: *const c_char, _: *mut c_char, _: u32) -> u32 {
            crate::warn_once!("IVRResources: no shared resources are available");
            debug!("Shared resource not found: {}", app_str(name));
            // Nothing loaded, so no buffer size to report.
            0
        }
        fn GetResourceFullPath(
            &self,
            name: *const c_char,
            type_directory: *const c_char,
            path_buffer: *mut c_char,
            size: u32,
        ) -> u32 {
            crate::warn_once!("IVRResources: no shared resources are available");
            debug!(
                "Resource path not found: {} in {}",
                app_str(name),
                app_str(type_directory)
            );
            write_empty(path_buffer, size);
            0
        }
    }
}

mod notifications {
    use super::*;
    use openvr::IVROverlay028_Interface;

    /// Nothing draws notifications yet, so none can be queued.
    #[derive(macros::InterfaceImpl)]
    #[interface = "IVRNotifications"]
    #[versions(002)]
    pub struct Notifications {
        vtables: Vtables,
        overlays: Injected<OverlayMan>,
    }

    impl Notifications {
        pub fn new(injector: &Injector) -> Self {
            Self {
                vtables: Default::default(),
                overlays: injector.inject(),
            }
        }

        /// Whether the app created `handle` through IVROverlay.
        fn is_overlay(&self, handle: vr::VROverlayHandle_t) -> bool {
            let mut flags = 0;
            self.overlays.get().is_some_and(|overlays| {
                overlays.GetOverlayFlags(handle, &mut flags) != vr::EVROverlayError::UnknownOverlay
            })
        }
    }

    impl vr::IVRNotifications002_Interface for Notifications {
        fn CreateNotification(
            &self,
            overlay: vr::VROverlayHandle_t,
            _: u64,
            _: vr::EVRNotificationType,
            text: *const c_char,
            _: vr::EVRNotificationStyle,
            _: *const vr::NotificationBitmap_t,
            notification_id: *mut vr::VRNotificationId,
        ) -> vr::EVRNotificationError {
            crate::warn_once!("IVRNotifications: notifications can't be shown");
            write_out(notification_id, 0);
            if !self.is_overlay(overlay) {
                return vr::EVRNotificationError::InvalidOverlayHandle;
            }
            debug!(
                "Notification for overlay {overlay} not shown: {:?}",
                app_str(text)
            );
            vr::EVRNotificationError::NotificationQueueFull
        }
        fn RemoveNotification(&self, _: vr::VRNotificationId) -> vr::EVRNotificationError {
            // No id was ever given out.
            vr::EVRNotificationError::InvalidNotificationId
        }
    }
}

mod driver_manager {
    use super::*;

    /// OpenXR runtimes have no SteamVR drivers to list.
    #[derive(Default, macros::InterfaceImpl)]
    #[interface = "IVRDriverManager"]
    #[versions(001)]
    pub struct DriverManager {
        vtables: Vtables,
    }

    impl vr::IVRDriverManager001_Interface for DriverManager {
        fn GetDriverCount(&self) -> u32 {
            crate::warn_once!("IVRDriverManager: there are no drivers");
            0
        }
        fn GetDriverName(&self, _: vr::DriverId_t, value: *mut c_char, size: u32) -> u32 {
            crate::warn_once!("IVRDriverManager: there are no drivers");
            write_empty(value, size);
            0
        }
        fn GetDriverHandle(&self, name: *const c_char) -> vr::DriverHandle_t {
            crate::warn_once!("IVRDriverManager: there are no drivers");
            debug!("No driver named {}", app_str(name));
            vr::k_ulInvalidDriverHandle
        }
        fn IsEnabled(&self, _: vr::DriverId_t) -> bool {
            false
        }
    }
}

mod spatial_anchors {
    use super::*;

    /// No driver here persists anchors.
    #[derive(Default, macros::InterfaceImpl)]
    #[interface = "IVRSpatialAnchors"]
    #[versions(001)]
    pub struct SpatialAnchors {
        vtables: Vtables,
    }

    impl SpatialAnchors {
        fn unavailable() {
            crate::warn_once!("IVRSpatialAnchors: spatial anchors are unavailable");
        }
    }

    impl vr::IVRSpatialAnchors001_Interface for SpatialAnchors {
        fn CreateSpatialAnchorFromDescriptor(
            &self,
            _: *const c_char,
            handle: *mut vr::SpatialAnchorHandle_t,
        ) -> vr::EVRSpatialAnchorError {
            Self::unavailable();
            write_out(handle, vr::k_ulInvalidSpatialAnchorHandle);
            vr::EVRSpatialAnchorError::PermanentlyUnavailable
        }
        fn CreateSpatialAnchorFromPose(
            &self,
            _: vr::TrackedDeviceIndex_t,
            _: vr::ETrackingUniverseOrigin,
            _: *mut vr::SpatialAnchorPose_t,
            handle: *mut vr::SpatialAnchorHandle_t,
        ) -> vr::EVRSpatialAnchorError {
            Self::unavailable();
            write_out(handle, vr::k_ulInvalidSpatialAnchorHandle);
            vr::EVRSpatialAnchorError::PermanentlyUnavailable
        }
        fn GetSpatialAnchorPose(
            &self,
            _: vr::SpatialAnchorHandle_t,
            _: vr::ETrackingUniverseOrigin,
            pose: *mut vr::SpatialAnchorPose_t,
        ) -> vr::EVRSpatialAnchorError {
            // No handle was ever given out.
            write_out(pose, Default::default());
            vr::EVRSpatialAnchorError::UnknownHandle
        }
        fn GetSpatialAnchorDescriptor(
            &self,
            _: vr::SpatialAnchorHandle_t,
            descriptor: *mut c_char,
            size_in_out: *mut u32,
        ) -> vr::EVRSpatialAnchorError {
            let size = unsafe { size_in_out.as_ref() }.map_or(0, |size| *size);
            write_empty(descriptor, size);
            write_out(size_in_out, 0);
            vr::EVRSpatialAnchorError::UnknownHandle
        }
    }
}

mod io_buffer {
    use super::*;

    /// vrserver's shared memory buffers don't exist here.
    #[derive(Default, macros::InterfaceImpl)]
    #[interface = "IVRIOBuffer"]
    #[versions(002, 001)]
    pub struct IoBuffer {
        vtables: Vtables,
    }

    impl vr::IVRIOBuffer002_Interface for IoBuffer {
        fn Open(
            &self,
            path: *const c_char,
            _: vr::EIOBufferMode,
            _: u32,
            _: u32,
            buffer: *mut vr::IOBufferHandle_t,
        ) -> vr::EIOBufferError {
            crate::warn_once!("IVRIOBuffer: IO buffers are unavailable");
            debug!("IO buffer not opened: {}", app_str(path));
            write_out(buffer, vr::k_ulInvalidIOBufferHandle);
            vr::EIOBufferError::OperationFailed
        }
        fn Close(&self, _: vr::IOBufferHandle_t) -> vr::EIOBufferError {
            // No handle was ever given out.
            vr::EIOBufferError::InvalidHandle
        }
        fn Read(
            &self,
            _: vr::IOBufferHandle_t,
            _: *mut c_void,
            _: u32,
            read: *mut u32,
        ) -> vr::EIOBufferError {
            write_out(read, 0);
            vr::EIOBufferError::InvalidHandle
        }
        fn Write(&self, _: vr::IOBufferHandle_t, _: *mut c_void, _: u32) -> vr::EIOBufferError {
            vr::EIOBufferError::InvalidHandle
        }
        fn PropertyContainer(&self, _: vr::IOBufferHandle_t) -> vr::PropertyContainerHandle_t {
            vr::k_ulInvalidPropertyContainer
        }
        fn HasReaders(&self, _: vr::IOBufferHandle_t) -> bool {
            false
        }
    }
}

mod ipc_resource_manager {
    use super::*;

    /// vrserver's shared Vulkan resources don't exist here.
    #[derive(Default, macros::InterfaceImpl)]
    #[interface = "IVRIPCResourceManagerClient"]
    #[versions(003, 002)]
    pub struct IpcResourceManagerClient {
        vtables: Vtables,
    }

    impl IpcResourceManagerClient {
        fn unavailable() {
            crate::warn_once!("IVRIPCResourceManagerClient: shared resources are unavailable");
        }
    }

    impl vr::IVRIPCResourceManagerClient003_Interface for IpcResourceManagerClient {
        fn NewSharedVulkanImage(
            &self,
            _: u32,
            _: u32,
            _: u32,
            _: bool,
            _: bool,
            _: bool,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            shared_handle: *mut vr::SharedTextureHandle_t,
        ) -> bool {
            Self::unavailable();
            write_out(shared_handle, 0);
            false
        }
        fn NewSharedVulkanBuffer(
            &self,
            _: u32,
            _: u32,
            shared_handle: *mut vr::SharedTextureHandle_t,
        ) -> bool {
            Self::unavailable();
            write_out(shared_handle, 0);
            false
        }
        fn NewSharedVulkanSemaphore(
            &self,
            _: bool,
            shared_handle: *mut vr::SharedTextureHandle_t,
        ) -> bool {
            Self::unavailable();
            write_out(shared_handle, 0);
            false
        }
        fn RefResource(&self, _: vr::SharedTextureHandle_t, new_ipc_handle: *mut u64) -> bool {
            write_out(new_ipc_handle, 0);
            false
        }
        fn UnrefResource(&self, _: vr::SharedTextureHandle_t) -> bool {
            false
        }
        fn GetDmabufFormats(&self, count: *mut u32, _: *mut u32) -> bool {
            Self::unavailable();
            write_out(count, 0);
            false
        }
        fn GetDmabufModifiers(
            &self,
            _: vr::EVRApplicationType,
            _: u32,
            count: *mut u32,
            _: *mut u64,
        ) -> bool {
            Self::unavailable();
            write_out(count, 0);
            false
        }
        fn ImportDmabuf(
            &self,
            _: vr::EVRApplicationType,
            _: *mut vr::DmabufAttributes_t,
            shared_handle: *mut vr::SharedTextureHandle_t,
        ) -> bool {
            Self::unavailable();
            write_out(shared_handle, 0);
            false
        }
        fn ReceiveSharedFd(&self, _: u64, fd: *mut std::os::raw::c_int) -> bool {
            // Not 0, which is a real file descriptor.
            write_out(fd, -1);
            false
        }
    }

    impl vr::IVRIPCResourceManagerClient002On003 for IpcResourceManagerClient {
        fn NewSharedVulkanImage(
            &self,
            _: u32,
            _: u32,
            _: u32,
            _: bool,
            _: bool,
            _: bool,
            _: u32,
            _: u32,
            shared_handle: *mut vr::SharedTextureHandle_t,
        ) -> bool {
            Self::unavailable();
            write_out(shared_handle, 0);
            false
        }
    }
}

mod control_panel {
    use super::*;

    /// The 0.9.12 control panel. IVRControlPanel_006, a different undocumented interface, is in
    /// misc_unknown.rs.
    #[derive(Default, macros::InterfaceImpl)]
    #[interface = "IVRControlPanel"]
    #[versions(001)]
    pub struct ControlPanel {
        vtables: Vtables,
    }

    impl ControlPanel {
        fn unavailable() {
            crate::warn_once!("IVRControlPanel_001: there are no drivers or displays to control");
        }
    }

    impl vr::IVRControlPanel001_Interface for ControlPanel {
        fn GetDriverCount(&self) -> u32 {
            Self::unavailable();
            0
        }
        fn GetDriverId(&self, _: u32, buffer: *mut c_char, size: u32) -> u32 {
            Self::unavailable();
            write_empty(buffer, size);
            0
        }
        fn GetDriverDisplayCount(&self, _: *const c_char) -> u32 {
            Self::unavailable();
            0
        }
        fn GetDriverDisplayId(
            &self,
            _: *const c_char,
            _: u32,
            buffer: *mut c_char,
            size: u32,
        ) -> u32 {
            Self::unavailable();
            write_empty(buffer, size);
            0
        }
        fn GetDriverDisplayModelNumber(
            &self,
            _: *const c_char,
            _: *const c_char,
            buffer: *mut c_char,
            size: u32,
        ) -> u32 {
            Self::unavailable();
            write_empty(buffer, size);
            0
        }
        fn GetDriverDisplaySerialNumber(
            &self,
            _: *const c_char,
            _: *const c_char,
            buffer: *mut c_char,
            size: u32,
        ) -> u32 {
            Self::unavailable();
            write_empty(buffer, size);
            0
        }
        fn GetCurrentDisplayInterface(&self, version: *const c_char) -> *mut c_void {
            crate::warn_unimplemented!("IVRControlPanel_001::GetCurrentDisplayInterface");
            debug!("Display interface {} not handed out", app_str(version));
            std::ptr::null_mut()
        }
        fn LoadSharedResource(&self, name: *const c_char, _: *mut c_char, _: u32) -> u32 {
            crate::warn_once!("IVRControlPanel_001: no shared resources are available");
            debug!("Shared resource not found: {}", app_str(name));
            0
        }
        fn GetIPD(&self) -> f32 {
            crate::warn_unimplemented!("IVRControlPanel_001::GetIPD");
            0.0
        }
        fn SetIPD(&self, _: f32) {
            crate::warn_unimplemented!("IVRControlPanel_001::SetIPD");
        }
        fn GetCurrentCompositorInterface(&self, version: *const c_char) -> *mut c_void {
            crate::warn_unimplemented!("IVRControlPanel_001::GetCurrentCompositorInterface");
            debug!("Compositor interface {} not handed out", app_str(version));
            std::ptr::null_mut()
        }
        fn QuitProcess(&self, _: u32) -> bool {
            crate::warn_unimplemented!("IVRControlPanel_001::QuitProcess");
            false
        }
        fn StartVRProcess(
            &self,
            executable: *const c_char,
            _: *mut *const c_char,
            _: u32,
            _: *const c_char,
        ) -> u32 {
            crate::warn_unimplemented!("IVRControlPanel_001::StartVRProcess");
            debug!("Not starting {}", app_str(executable));
            0
        }
        fn SetMasterProcessToThis(&self) {
            crate::warn_unimplemented!("IVRControlPanel_001::SetMasterProcessToThis");
        }
        fn StartAutolaunchOverlays(&self) {
            crate::warn_unimplemented!("IVRControlPanel_001::StartAutolaunchOverlays");
        }
        fn ForceQuitProcess(&self, _: u32) -> bool {
            crate::warn_unimplemented!("IVRControlPanel_001::ForceQuitProcess");
            false
        }
        fn AbortTransition(&self) {
            crate::warn_unimplemented!("IVRControlPanel_001::AbortTransition");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::{null, null_mut};

    fn name(ptr: *const c_char) -> &'static CStr {
        unsafe { CStr::from_ptr(ptr) }
    }

    /// A buffer filled with garbage, to see that an empty string gets written.
    fn garbage() -> [c_char; 8] {
        [b'x' as c_char; 8]
    }

    #[test]
    fn tracked_camera_has_none() {
        use vr::IVRTrackedCamera006_Interface;
        type Camera = TrackedCamera;

        let camera = Camera::default();
        assert_eq!(
            name(
                camera.GetCameraErrorNameFromEnum(
                    vr::EVRTrackedCameraError::NotSupportedForThisDevice
                )
            ),
            c"VRTrackedCameraError_NotSupportedForThisDevice"
        );
        assert_eq!(
            name(camera.GetCameraErrorNameFromEnum(vr::EVRTrackedCameraError(12345))),
            c"Unknown"
        );

        let mut has_camera = true;
        assert_eq!(
            camera.HasCamera(0, &mut has_camera),
            vr::EVRTrackedCameraError::None
        );
        assert!(!has_camera);
        assert_eq!(
            camera.HasCamera(0, null_mut()),
            vr::EVRTrackedCameraError::None
        );

        let (mut width, mut height, mut size) = (1, 1, 1);
        assert_eq!(
            camera.GetCameraFrameSize(
                0,
                vr::EVRTrackedCameraFrameType::Distorted,
                &mut width,
                &mut height,
                &mut size
            ),
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        );
        assert_eq!((width, height, size), (0, 0, 0));

        let mut focal = vr::HmdVector2_t { v: [1.0, 1.0] };
        let mut center = focal;
        assert_eq!(
            camera.GetCameraIntrinsics(
                0,
                0,
                vr::EVRTrackedCameraFrameType::Undistorted,
                &mut focal,
                &mut center
            ),
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        );
        assert_eq!(focal.v, [0.0, 0.0]);
        assert_eq!(center.v, [0.0, 0.0]);

        let mut handle = 42;
        assert_eq!(
            camera.AcquireVideoStreamingService(0, &mut handle),
            vr::EVRTrackedCameraError::NotSupportedForThisDevice
        );
        assert_eq!(handle, 0);
        assert_eq!(
            camera.ReleaseVideoStreamingService(42),
            vr::EVRTrackedCameraError::InvalidHandle
        );

        let mut header = vr::CameraVideoStreamFrameHeader_t {
            nWidth: 640,
            ..Default::default()
        };
        assert_eq!(
            camera.GetVideoStreamFrameBuffer(
                42,
                vr::EVRTrackedCameraFrameType::Distorted,
                null_mut(),
                0,
                &mut header,
                std::mem::size_of::<vr::CameraVideoStreamFrameHeader_t>() as u32
            ),
            vr::EVRTrackedCameraError::InvalidHandle
        );
        assert_eq!(header.nWidth, 0);
        // A header of another size isn't touched.
        header.nWidth = 640;
        camera.GetVideoStreamFrameBuffer(
            42,
            vr::EVRTrackedCameraFrameType::Distorted,
            null_mut(),
            0,
            &mut header,
            4,
        );
        assert_eq!(header.nWidth, 640);

        let mut texture = 7;
        assert_eq!(
            camera.GetVideoStreamTextureGL(
                42,
                vr::EVRTrackedCameraFrameType::Distorted,
                &mut texture,
                null_mut(),
                0
            ),
            vr::EVRTrackedCameraError::InvalidHandle
        );
        assert_eq!(texture, 0);
        let mut view = std::ptr::dangling_mut::<c_void>();
        assert_eq!(
            camera.GetVideoStreamTextureD3D11(
                42,
                vr::EVRTrackedCameraFrameType::Distorted,
                null_mut(),
                &mut view,
                null_mut(),
                0
            ),
            vr::EVRTrackedCameraError::InvalidHandle
        );
        assert!(view.is_null());

        assert_eq!(
            camera.GetCameraTrackingSpace(),
            vr::ETrackingUniverseOrigin::Seated
        );
        camera.SetCameraTrackingSpace(vr::ETrackingUniverseOrigin::Standing);
        assert_eq!(
            camera.GetCameraTrackingSpace(),
            vr::ETrackingUniverseOrigin::Standing
        );
    }

    #[test]
    fn tracked_camera_old_versions() {
        let camera = TrackedCamera::default();
        {
            use vr::IVRTrackedCamera004On005;
            let mut projection = vr::HmdMatrix44_t { m: [[1.0; 4]; 4] };
            assert_eq!(
                camera.GetCameraProjection(
                    0,
                    vr::EVRTrackedCameraFrameType::Distorted,
                    0.1,
                    100.0,
                    &mut projection
                ),
                vr::EVRTrackedCameraError::NotSupportedForThisDevice
            );
            assert_eq!(projection.m, [[0.0; 4]; 4]);
            assert_eq!(
                camera.GetCameraIntrinsics(
                    0,
                    vr::EVRTrackedCameraFrameType::Distorted,
                    null_mut(),
                    null_mut()
                ),
                vr::EVRTrackedCameraError::NotSupportedForThisDevice
            );
        }
        {
            use vr::IVRTrackedCamera001On003;
            assert!(!camera.HasCamera(0));
            let mut buffer = garbage();
            assert!(!camera.GetCameraFirmwareDescription(0, buffer.as_mut_ptr(), 8));
            assert_eq!(name(buffer.as_ptr()), c"");
            let (mut width, mut height) = (1, 1);
            assert!(!camera.GetCameraFrameDimensions(
                0,
                vr::ECameraVideoStreamFormat::FORMAT_RGB24,
                &mut width,
                &mut height
            ));
            assert_eq!((width, height), (0, 0));
            assert_eq!(
                camera.GetCameraVideoStreamFormat(0),
                vr::ECameraVideoStreamFormat::FORMAT_UNKNOWN
            );
            assert!(!camera.StartVideoStream(0));
            assert!(!camera.IsVideoStreamActive(0));
            assert_eq!(camera.GetVideoStreamElapsedTime(0), 0.0);
            assert!(camera.GetVideoStreamFrame(0).is_null());
            assert!(!camera.ReleaseVideoStreamFrame(0, null()));
            let (mut u, mut v) = (1.0, 1.0);
            assert!(!camera.GetCameraDistortion(0, 0.5, 0.5, &mut u, &mut v));
            assert_eq!((u, v), (0.0, 0.0));
            assert!(!camera.GetCameraProjection(0, 640.0, 480.0, 0.1, 100.0, null_mut()));
        }
    }

    #[test]
    fn resources_have_nothing() {
        use vr::IVRResources001_Interface;
        let resources = Resources::default();
        let mut buffer = garbage();
        assert_eq!(
            resources.LoadSharedResource(c"icons/x.png".as_ptr(), buffer.as_mut_ptr(), 8),
            0
        );
        assert_eq!(
            resources.GetResourceFullPath(
                c"x.png".as_ptr(),
                c"icons".as_ptr(),
                buffer.as_mut_ptr(),
                8
            ),
            0
        );
        assert_eq!(name(buffer.as_ptr()), c"");
        assert_eq!(resources.LoadSharedResource(null(), null_mut(), 0), 0);
        assert_eq!(
            resources.GetResourceFullPath(null(), null(), null_mut(), 0),
            0
        );
    }

    #[test]
    fn notifications_need_an_overlay() {
        use vr::IVRNotifications002_Interface;
        // No IVROverlay was requested, so no handle is one of ours.
        let notifications = Notifications::new(&Injector::default());
        let mut id = 5;
        assert_eq!(
            notifications.CreateNotification(
                1,
                0,
                vr::EVRNotificationType::Transient,
                c"hi".as_ptr(),
                vr::EVRNotificationStyle::None,
                null(),
                &mut id
            ),
            vr::EVRNotificationError::InvalidOverlayHandle
        );
        assert_eq!(id, 0);
        assert_eq!(
            notifications.CreateNotification(
                0,
                0,
                vr::EVRNotificationType::Persistent,
                null(),
                vr::EVRNotificationStyle::None,
                null(),
                null_mut()
            ),
            vr::EVRNotificationError::InvalidOverlayHandle
        );
        assert_eq!(
            notifications.RemoveNotification(1),
            vr::EVRNotificationError::InvalidNotificationId
        );
    }

    #[test]
    fn driver_manager_has_no_drivers() {
        use vr::IVRDriverManager001_Interface;
        let drivers = DriverManager::default();
        assert_eq!(drivers.GetDriverCount(), 0);
        let mut buffer = garbage();
        assert_eq!(drivers.GetDriverName(0, buffer.as_mut_ptr(), 8), 0);
        assert_eq!(name(buffer.as_ptr()), c"");
        assert_eq!(drivers.GetDriverName(0, null_mut(), 0), 0);
        assert_eq!(
            drivers.GetDriverHandle(c"lighthouse".as_ptr()),
            vr::k_ulInvalidDriverHandle
        );
        assert_eq!(drivers.GetDriverHandle(null()), vr::k_ulInvalidDriverHandle);
        assert!(!drivers.IsEnabled(0));
    }

    #[test]
    fn spatial_anchors_are_unavailable() {
        use vr::IVRSpatialAnchors001_Interface;
        let anchors = SpatialAnchors::default();
        let mut handle = 5;
        assert_eq!(
            anchors.CreateSpatialAnchorFromDescriptor(c"desc".as_ptr(), &mut handle),
            vr::EVRSpatialAnchorError::PermanentlyUnavailable
        );
        assert_eq!(handle, vr::k_ulInvalidSpatialAnchorHandle);
        handle = 5;
        let mut pose = vr::SpatialAnchorPose_t {
            mAnchorToAbsoluteTracking: vr::HmdMatrix34_t { m: [[1.0; 4]; 3] },
        };
        assert_eq!(
            anchors.CreateSpatialAnchorFromPose(
                0,
                vr::ETrackingUniverseOrigin::Standing,
                &mut pose,
                &mut handle
            ),
            vr::EVRSpatialAnchorError::PermanentlyUnavailable
        );
        assert_eq!(handle, vr::k_ulInvalidSpatialAnchorHandle);
        assert_eq!(
            anchors.GetSpatialAnchorPose(5, vr::ETrackingUniverseOrigin::Standing, &mut pose),
            vr::EVRSpatialAnchorError::UnknownHandle
        );
        assert_eq!(pose.mAnchorToAbsoluteTracking.m, [[0.0; 4]; 3]);
        let mut buffer = garbage();
        let mut size = 8;
        assert_eq!(
            anchors.GetSpatialAnchorDescriptor(5, buffer.as_mut_ptr(), &mut size),
            vr::EVRSpatialAnchorError::UnknownHandle
        );
        assert_eq!(size, 0);
        assert_eq!(name(buffer.as_ptr()), c"");
        assert_eq!(
            anchors.GetSpatialAnchorDescriptor(5, null_mut(), null_mut()),
            vr::EVRSpatialAnchorError::UnknownHandle
        );
    }

    #[test]
    fn io_buffers_are_unavailable() {
        use vr::IVRIOBuffer002_Interface;
        let buffers = IoBuffer::default();
        let mut handle = 5;
        assert_eq!(
            buffers.Open(c"/x".as_ptr(), vr::EIOBufferMode::Read, 1, 1, &mut handle),
            vr::EIOBufferError::OperationFailed
        );
        assert_eq!(handle, vr::k_ulInvalidIOBufferHandle);
        assert_eq!(
            buffers.Open(null(), vr::EIOBufferMode::Create, 1, 1, null_mut()),
            vr::EIOBufferError::OperationFailed
        );
        assert_eq!(buffers.Close(5), vr::EIOBufferError::InvalidHandle);
        let mut read = 5;
        assert_eq!(
            buffers.Read(5, null_mut(), 0, &mut read),
            vr::EIOBufferError::InvalidHandle
        );
        assert_eq!(read, 0);
        assert_eq!(
            buffers.Write(5, null_mut(), 0),
            vr::EIOBufferError::InvalidHandle
        );
        assert_eq!(
            buffers.PropertyContainer(5),
            vr::k_ulInvalidPropertyContainer
        );
        assert!(!buffers.HasReaders(5));
    }

    #[test]
    fn ipc_resources_are_unavailable() {
        let client = IpcResourceManagerClient::default();
        let mut handle = 5;
        {
            use vr::IVRIPCResourceManagerClient003_Interface;
            assert!(!client.NewSharedVulkanImage(
                0,
                1,
                1,
                true,
                false,
                false,
                1,
                1,
                0,
                0,
                &mut handle
            ));
            assert_eq!(handle, 0);
            handle = 5;
            assert!(!client.NewSharedVulkanBuffer(1, 0, &mut handle));
            assert_eq!(handle, 0);
            assert!(!client.NewSharedVulkanSemaphore(false, null_mut()));
            let mut ipc_handle = 5;
            assert!(!client.RefResource(1, &mut ipc_handle));
            assert_eq!(ipc_handle, 0);
            assert!(!client.UnrefResource(1));
            let mut count = 5;
            assert!(!client.GetDmabufFormats(&mut count, null_mut()));
            assert_eq!(count, 0);
            count = 5;
            assert!(!client.GetDmabufModifiers(
                vr::EVRApplicationType::Scene,
                0,
                &mut count,
                null_mut()
            ));
            assert_eq!(count, 0);
            handle = 5;
            assert!(!client.ImportDmabuf(vr::EVRApplicationType::Scene, null_mut(), &mut handle));
            assert_eq!(handle, 0);
            let mut fd = 5;
            assert!(!client.ReceiveSharedFd(1, &mut fd));
            assert_eq!(fd, -1);
        }
        {
            use vr::IVRIPCResourceManagerClient002On003;
            handle = 5;
            assert!(!client.NewSharedVulkanImage(0, 1, 1, true, false, false, 1, 1, &mut handle));
            assert_eq!(handle, 0);
        }
    }

    #[test]
    fn control_panel_has_nothing() {
        use vr::IVRControlPanel001_Interface;
        let panel = ControlPanel::default();
        assert_eq!(panel.GetDriverCount(), 0);
        let mut buffer = garbage();
        assert_eq!(panel.GetDriverId(0, buffer.as_mut_ptr(), 8), 0);
        assert_eq!(name(buffer.as_ptr()), c"");
        assert_eq!(panel.GetDriverDisplayCount(c"lighthouse".as_ptr()), 0);
        buffer = garbage();
        assert_eq!(
            panel.GetDriverDisplayId(c"lighthouse".as_ptr(), 0, buffer.as_mut_ptr(), 8),
            0
        );
        assert_eq!(name(buffer.as_ptr()), c"");
        buffer = garbage();
        assert_eq!(
            panel.GetDriverDisplayModelNumber(null(), null(), buffer.as_mut_ptr(), 8),
            0
        );
        assert_eq!(name(buffer.as_ptr()), c"");
        assert_eq!(
            panel.GetDriverDisplaySerialNumber(null(), null(), null_mut(), 0),
            0
        );
        assert!(
            panel
                .GetCurrentDisplayInterface(c"IVRSystem_009".as_ptr())
                .is_null()
        );
        assert!(
            panel
                .GetCurrentCompositorInterface(c"IVRCompositor_009".as_ptr())
                .is_null()
        );
        assert_eq!(panel.LoadSharedResource(c"x".as_ptr(), null_mut(), 0), 0);
        assert_eq!(panel.GetIPD(), 0.0);
        panel.SetIPD(0.064);
        assert_eq!(panel.GetIPD(), 0.0);
        assert!(!panel.QuitProcess(1));
        assert!(!panel.ForceQuitProcess(1));
        assert_eq!(
            panel.StartVRProcess(c"/bin/true".as_ptr(), null_mut(), 0, null()),
            0
        );
        panel.SetMasterProcessToThis();
        panel.StartAutolaunchOverlays();
        panel.AbortTransition();
    }
}
