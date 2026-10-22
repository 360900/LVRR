use openvr as vr;
use std::ffi::c_char;

use log::info;
use std::process::Command;

#[derive(Default, macros::InterfaceImpl)]
#[interface = "IVRApplications"]
#[versions(008, 007, 006, 005, 004, 003, 002)]
pub struct Applications {
    vtables: Vtables,
}

impl vr::IVRApplications008_Interface for Applications {
    fn GetCurrentSceneProcessId(&self) -> u32 {
        // xrizer runs in the scene application itself
        std::process::id()
    }
    fn LaunchInternalProcess(
        &self,
        binary_path: *const c_char,
        arguments: *const c_char,
        working_directory: *const c_char,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("LaunchInternalProcess");

        if binary_path.is_null() || arguments.is_null() || working_directory.is_null() {
            return vr::EVRApplicationError::InvalidParameter;
        }

        let binary_path = unsafe { std::ffi::CStr::from_ptr(binary_path) }
            .to_str()
            .unwrap_or("")
            .to_owned();
        let arguments = unsafe { std::ffi::CStr::from_ptr(arguments) }
            .to_str()
            .unwrap_or("")
            .to_owned();
        let working_directory = unsafe { std::ffi::CStr::from_ptr(working_directory) }
            .to_str()
            .unwrap_or("")
            .to_owned();

        info!(
            "LaunchInternalProcess called: {:?}, ARGS: {:?}, WD: {:?}",
            binary_path, arguments, working_directory
        );

        let process = Command::new(binary_path)
            .args(arguments.split_whitespace())
            .current_dir(working_directory)
            .spawn();

        match process {
            Ok(_) => vr::EVRApplicationError::None,
            Err(e) => {
                info!("Failed to launch internal process: {}", e);
                vr::EVRApplicationError::LaunchFailed
            }
        }
    }
    fn GetSceneApplicationStateNameFromEnum(
        &self,
        state: vr::EVRSceneApplicationState,
    ) -> *const c_char {
        match state {
            vr::EVRSceneApplicationState::None => c"None".as_ptr(),
            vr::EVRSceneApplicationState::Starting => c"Starting".as_ptr(),
            vr::EVRSceneApplicationState::Quitting => c"Quitting".as_ptr(),
            vr::EVRSceneApplicationState::Running => c"Running".as_ptr(),
            vr::EVRSceneApplicationState::Waiting => c"Waiting".as_ptr(),
            _ => c"Unknown".as_ptr(),
        }
    }
    fn PerformApplicationPrelaunchCheck(&self, _: *const c_char) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("PerformApplicationPrelaunchCheck");
        vr::EVRApplicationError::None
    }
    fn GetSceneApplicationState(&self) -> vr::EVRSceneApplicationState {
        // xrizer runs in the scene application itself
        vr::EVRSceneApplicationState::Running
    }
    fn GetStartingApplication(&self, buffer: *mut c_char, size: u32) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("GetStartingApplication");
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        vr::EVRApplicationError::None
    }
    fn GetApplicationLaunchArguments(&self, _: u32, buffer: *mut c_char, size: u32) -> u32 {
        crate::warn_unimplemented!("GetApplicationLaunchArguments");
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        0
    }
    fn GetApplicationsThatSupportMimeType(&self, _: *const c_char, _: *mut c_char, _: u32) -> u32 {
        crate::warn_unimplemented!("GetApplicationsThatSupportMimeType");
        0
    }
    fn GetApplicationSupportedMimeTypes(&self, _: *const c_char, _: *mut c_char, _: u32) -> bool {
        crate::warn_unimplemented!("GetApplicationSupportedMimeTypes");
        false
    }
    fn GetDefaultApplicationForMimeType(&self, _: *const c_char, _: *mut c_char, _: u32) -> bool {
        crate::warn_unimplemented!("GetDefaultApplicationForMimeType");
        false
    }
    fn SetDefaultApplicationForMimeType(
        &self,
        _: *const c_char,
        _: *const c_char,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("SetDefaultApplicationForMimeType");
        vr::EVRApplicationError::None
    }
    fn GetApplicationAutoLaunch(&self, _: *const c_char) -> bool {
        crate::warn_unimplemented!("GetApplicationAutoLaunch");
        false
    }
    fn SetApplicationAutoLaunch(&self, _: *const c_char, _: bool) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("SetApplicationAutoLaunch");
        vr::EVRApplicationError::None
    }
    fn GetApplicationPropertyUint64(
        &self,
        _: *const c_char,
        _: vr::EVRApplicationProperty,
        error: *mut vr::EVRApplicationError,
    ) -> u64 {
        crate::warn_unimplemented!("GetApplicationPropertyUint64");
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::EVRApplicationError::UnknownApplication;
        }
        0
    }
    fn GetApplicationPropertyBool(
        &self,
        _: *const c_char,
        _: vr::EVRApplicationProperty,
        error: *mut vr::EVRApplicationError,
    ) -> bool {
        crate::warn_unimplemented!("GetApplicationPropertyBool");
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::EVRApplicationError::UnknownApplication;
        }
        false
    }
    fn GetApplicationPropertyString(
        &self,
        _: *const c_char,
        _: vr::EVRApplicationProperty,
        buffer: *mut c_char,
        size: u32,
        error: *mut vr::EVRApplicationError,
    ) -> u32 {
        crate::warn_unimplemented!("GetApplicationPropertyString");
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::EVRApplicationError::UnknownApplication;
        }
        0
    }
    fn GetApplicationsErrorNameFromEnum(&self, error: vr::EVRApplicationError) -> *const c_char {
        match error {
            vr::EVRApplicationError::None => c"None".as_ptr(),
            vr::EVRApplicationError::AppKeyAlreadyExists => c"AppKeyAlreadyExists".as_ptr(),
            vr::EVRApplicationError::NoManifest => c"NoManifest".as_ptr(),
            vr::EVRApplicationError::NoApplication => c"NoApplication".as_ptr(),
            vr::EVRApplicationError::InvalidIndex => c"InvalidIndex".as_ptr(),
            vr::EVRApplicationError::UnknownApplication => c"UnknownApplication".as_ptr(),
            vr::EVRApplicationError::IPCFailed => c"IPCFailed".as_ptr(),
            vr::EVRApplicationError::ApplicationAlreadyRunning => {
                c"ApplicationAlreadyRunning".as_ptr()
            }
            vr::EVRApplicationError::InvalidManifest => c"InvalidManifest".as_ptr(),
            vr::EVRApplicationError::InvalidApplication => c"InvalidApplication".as_ptr(),
            vr::EVRApplicationError::LaunchFailed => c"LaunchFailed".as_ptr(),
            vr::EVRApplicationError::ApplicationAlreadyStarting => {
                c"ApplicationAlreadyStarting".as_ptr()
            }
            vr::EVRApplicationError::LaunchInProgress => c"LaunchInProgress".as_ptr(),
            vr::EVRApplicationError::OldApplicationQuitting => c"OldApplicationQuitting".as_ptr(),
            vr::EVRApplicationError::TransitionAborted => c"TransitionAborted".as_ptr(),
            vr::EVRApplicationError::IsTemplate => c"IsTemplate".as_ptr(),
            vr::EVRApplicationError::SteamVRIsExiting => c"SteamVRIsExiting".as_ptr(),
            vr::EVRApplicationError::WaitingForChaperone => c"WaitingForChaperone".as_ptr(),
            vr::EVRApplicationError::BufferTooSmall => c"BufferTooSmall".as_ptr(),
            vr::EVRApplicationError::PropertyNotSet => c"PropertyNotSet".as_ptr(),
            vr::EVRApplicationError::UnknownProperty => c"UnknownProperty".as_ptr(),
            vr::EVRApplicationError::InvalidParameter => c"InvalidParameter".as_ptr(),
            vr::EVRApplicationError::NotImplemented => c"NotImplemented".as_ptr(),
            _ => c"Unknown".as_ptr(),
        }
    }
    fn GetApplicationProcessId(&self, _: *const c_char) -> u32 {
        crate::warn_unimplemented!("GetApplicationProcessId");
        0
    }
    fn IdentifyApplication(&self, _: u32, _: *const c_char) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("IdentifyApplication");
        vr::EVRApplicationError::None
    }
    fn CancelApplicationLaunch(&self, _: *const c_char) -> bool {
        crate::warn_unimplemented!("CancelApplicationLaunch");
        false
    }
    fn LaunchDashboardOverlay(&self, _: *const c_char) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("LaunchDashboardOverlay");
        vr::EVRApplicationError::LaunchFailed
    }
    fn LaunchApplicationFromMimeType(
        &self,
        _: *const c_char,
        _: *const c_char,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("LaunchApplicationFromMimeType");
        vr::EVRApplicationError::LaunchFailed
    }
    fn LaunchTemplateApplication(
        &self,
        _: *const c_char,
        _: *const c_char,
        _: *const vr::AppOverrideKeys_t,
        _: u32,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("LaunchTemplateApplication");
        vr::EVRApplicationError::LaunchFailed
    }
    fn LaunchApplication(&self, _: *const c_char) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("LaunchApplication");
        vr::EVRApplicationError::LaunchFailed
    }
    fn GetApplicationKeyByProcessId(
        &self,
        _: u32,
        buffer: *mut c_char,
        size: u32,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("GetApplicationKeyByProcessId");
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        vr::EVRApplicationError::UnknownApplication
    }
    fn GetApplicationKeyByIndex(
        &self,
        _: u32,
        buffer: *mut c_char,
        size: u32,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("GetApplicationKeyByIndex");
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        vr::EVRApplicationError::UnknownApplication
    }
    fn GetApplicationCount(&self) -> u32 {
        crate::warn_unimplemented!("GetApplicationCount");
        0
    }
    fn IsApplicationInstalled(&self, _: *const c_char) -> bool {
        crate::warn_unimplemented!("IsApplicationInstalled");
        false
    }
    fn RemoveApplicationManifest(&self, _: *const c_char) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("RemoveApplicationManifest");
        vr::EVRApplicationError::None
    }
    fn AddApplicationManifest(&self, _: *const c_char, _: bool) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("AddApplicationManifest");
        vr::EVRApplicationError::None
    }

    fn RegisterSubprocess(&self, _: u32) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("RegisterSubprocess");
        vr::EVRApplicationError::None
    }
}

impl vr::IVRApplications006On007 for Applications {
    fn GetTransitionState(&self) -> vr::EVRApplicationTransitionState {
        crate::warn_unimplemented!("GetTransitionState");
        vr::EVRApplicationTransitionState::None
    }
    fn GetApplicationsTransitionStateNameFromEnum(
        &self,
        state: vr::EVRApplicationTransitionState,
    ) -> *const ::std::os::raw::c_char {
        crate::warn_unimplemented!("GetApplicationsTransitionStateNameFromEnum");
        match state {
            vr::EVRApplicationTransitionState::None => c"None".as_ptr(),
            vr::EVRApplicationTransitionState::OldAppQuitSent => c"OldAppQuitSent".as_ptr(),
            vr::EVRApplicationTransitionState::WaitingForExternalLaunch => {
                c"WaitingForExternalLaunch".as_ptr()
            }
            vr::EVRApplicationTransitionState::NewAppLaunched => c"NewAppLaunched".as_ptr(),
            _ => c"Unknown".as_ptr(),
        }
    }
    fn IsQuitUserPromptRequested(&self) -> bool {
        crate::warn_unimplemented!("IsQuitUserPromptRequested");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use vr::{IVRApplications006On007, IVRApplications008_Interface};

    #[test]
    fn unknown_enum_names() {
        let apps = Applications::default();
        let name = |ptr| unsafe { CStr::from_ptr(ptr) };

        let error = |e| name(apps.GetApplicationsErrorNameFromEnum(vr::EVRApplicationError(e)));
        assert_eq!(error(vr::EVRApplicationError::IPCFailed.0), c"IPCFailed");
        assert_eq!(error(5), c"Unknown");
        let state = vr::EVRSceneApplicationState(99);
        assert_eq!(
            name(apps.GetSceneApplicationStateNameFromEnum(state)),
            c"Unknown"
        );
        let state = vr::EVRApplicationTransitionState(5);
        assert_eq!(
            name(apps.GetApplicationsTransitionStateNameFromEnum(state)),
            c"Unknown"
        );
    }
}
