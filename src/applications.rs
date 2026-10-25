use crate::clientcore::{Injected, Injector};
use crate::compositor::Compositor;
use crate::input::Input;
use openvr as vr;
use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;
use std::sync::Mutex;

use log::{info, warn};
use std::process::Command;

#[derive(macros::InterfaceImpl)]
#[interface = "IVRApplications"]
#[versions(008, 007, 006, 005, 004, 003, 002)]
pub struct Applications {
    vtables: Vtables,
    /// Apps of the manifests added in this process, by app key, with their manifest. Nothing is
    /// installed for good, like SteamVR's temporary manifests.
    apps: Mutex<Vec<(CString, PathBuf)>>,
    /// The app key this process identified as.
    identity: Mutex<Option<CString>>,
    input: Injected<Input<Compositor>>,
}

/// The app key this process last identified as, for code without a handle to the interface.
static IDENTIFIED_KEY: Mutex<Option<String>> = Mutex::new(None);

/// The app key this process identified as with IdentifyApplication, if it did.
pub fn identified_app_key() -> Option<String> {
    IDENTIFIED_KEY.lock().unwrap().clone()
}

impl Applications {
    pub fn new(injector: &Injector) -> Self {
        Self {
            vtables: Default::default(),
            apps: Default::default(),
            identity: Default::default(),
            input: injector.inject(),
        }
    }

    fn is_installed(&self, key: &CStr) -> bool {
        self.apps
            .lock()
            .unwrap()
            .iter()
            .any(|(app, _)| **app == *key)
    }
}

/// The app keys a .vrmanifest lists.
fn manifest_app_keys(path: &std::path::Path) -> Result<Vec<CString>, vr::EVRApplicationError> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        warn!("Can't read application manifest {}: {e}", path.display());
        vr::EVRApplicationError::NoManifest
    })?;
    let invalid = |e: &dyn std::fmt::Display| {
        warn!("Invalid application manifest {}: {e}", path.display());
        vr::EVRApplicationError::InvalidManifest
    };
    let manifest: serde_json::Value = serde_json::from_str(&contents).map_err(|e| invalid(&e))?;
    let keys: Vec<_> = manifest["applications"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|app| CString::new(app["app_key"].as_str()?).ok())
        .collect();
    if keys.is_empty() {
        return Err(invalid(&"no applications with an app_key"));
    }
    Ok(keys)
}

/// Copies an app key to an app's buffer.
fn write_key(key: &CStr, buffer: *mut c_char, size: u32) -> vr::EVRApplicationError {
    let bytes = key.to_bytes_with_nul();
    if buffer.is_null() || (size as usize) < bytes.len() {
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        return vr::EVRApplicationError::BufferTooSmall;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), buffer, bytes.len()) };
    vr::EVRApplicationError::None
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
    fn GetApplicationProcessId(&self, key: *const c_char) -> u32 {
        if key.is_null() {
            return 0;
        }
        let key = unsafe { CStr::from_ptr(key) };
        match &*self.identity.lock().unwrap() {
            Some(identity) if **identity == *key => std::process::id(),
            _ => 0,
        }
    }
    fn IdentifyApplication(&self, pid: u32, key: *const c_char) -> vr::EVRApplicationError {
        if key.is_null() {
            return vr::EVRApplicationError::InvalidParameter;
        }
        let key = unsafe { CStr::from_ptr(key) };
        // Only this process is known here, which pid 0 also names. Launchers identifying the apps
        // they start are accepted, as nothing else would track them.
        if pid == 0 || pid == std::process::id() {
            info!("Application identified as {key:?}");
            *self.identity.lock().unwrap() = Some(key.to_owned());
            let key = key.to_string_lossy().into_owned();
            *IDENTIFIED_KEY.lock().unwrap() = Some(key.clone());
            // The bindings chosen for the app may differ for its key.
            if let Some(input) = self.input.get() {
                input.app_identified(&key);
            }
        }
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
        pid: u32,
        buffer: *mut c_char,
        size: u32,
    ) -> vr::EVRApplicationError {
        let identity = self.identity.lock().unwrap();
        match &*identity {
            Some(key) if pid == std::process::id() => write_key(key, buffer, size),
            _ => {
                if !buffer.is_null() && size > 0 {
                    unsafe { *buffer = 0 };
                }
                vr::EVRApplicationError::UnknownApplication
            }
        }
    }
    fn GetApplicationKeyByIndex(
        &self,
        index: u32,
        buffer: *mut c_char,
        size: u32,
    ) -> vr::EVRApplicationError {
        let apps = self.apps.lock().unwrap();
        match apps.get(index as usize) {
            Some((key, _)) => write_key(key, buffer, size),
            None => {
                if !buffer.is_null() && size > 0 {
                    unsafe { *buffer = 0 };
                }
                vr::EVRApplicationError::InvalidIndex
            }
        }
    }
    fn GetApplicationCount(&self) -> u32 {
        self.apps.lock().unwrap().len() as u32
    }
    fn IsApplicationInstalled(&self, key: *const c_char) -> bool {
        !key.is_null() && self.is_installed(unsafe { CStr::from_ptr(key) })
    }
    fn RemoveApplicationManifest(&self, path: *const c_char) -> vr::EVRApplicationError {
        if path.is_null() {
            return vr::EVRApplicationError::InvalidParameter;
        }
        let path = PathBuf::from(unsafe { CStr::from_ptr(path) }.to_string_lossy().as_ref());
        self.apps
            .lock()
            .unwrap()
            .retain(|(_, manifest)| *manifest != path);
        vr::EVRApplicationError::None
    }
    fn AddApplicationManifest(&self, path: *const c_char, _: bool) -> vr::EVRApplicationError {
        if path.is_null() {
            return vr::EVRApplicationError::InvalidParameter;
        }
        let path = PathBuf::from(unsafe { CStr::from_ptr(path) }.to_string_lossy().as_ref());
        let keys = match manifest_app_keys(&path) {
            Ok(keys) => keys,
            Err(e) => return e,
        };
        info!("Added application manifest {} ({keys:?})", path.display());
        let mut apps = self.apps.lock().unwrap();
        // Adding a manifest again replaces its apps, and the newest manifest of an app wins.
        apps.retain(|(key, manifest)| *manifest != path && !keys.contains(key));
        apps.extend(keys.into_iter().map(|key| (key, path.clone())));
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
    use vr::{IVRApplications006On007, IVRApplications008_Interface};

    /// Vivecraft adds its manifest, checks it's installed and identifies as its app.
    #[test]
    fn manifests_and_identity() {
        let apps = Applications::new(&Injector::default());
        let dir = std::env::temp_dir().join(format!("xrizer-manifest-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("app.vrmanifest");
        std::fs::write(
            &manifest,
            r#"{"source": "builtin", "applications": [{"app_key": "org.example.app", "launch_type": "binary"}]}"#,
        )
        .unwrap();
        let invalid = dir.join("invalid.vrmanifest");
        std::fs::write(&invalid, r#"{"applications": []}"#).unwrap();
        let path = CString::new(manifest.to_str().unwrap()).unwrap();
        let invalid_path = CString::new(invalid.to_str().unwrap()).unwrap();
        let key = c"org.example.app";

        assert!(!apps.IsApplicationInstalled(key.as_ptr()));
        let added = apps.AddApplicationManifest(path.as_ptr(), true);
        let rejected = apps.AddApplicationManifest(invalid_path.as_ptr(), true);
        let missing = apps.AddApplicationManifest(c"/nonexistent.vrmanifest".as_ptr(), true);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(added, vr::EVRApplicationError::None);
        assert_eq!(rejected, vr::EVRApplicationError::InvalidManifest);
        assert_eq!(missing, vr::EVRApplicationError::NoManifest);
        assert!(apps.IsApplicationInstalled(key.as_ptr()));
        assert_eq!(apps.GetApplicationCount(), 1);

        let pid = std::process::id();
        let mut buffer = [1 as c_char; 64];
        assert_eq!(
            apps.GetApplicationKeyByProcessId(pid, buffer.as_mut_ptr(), buffer.len() as u32),
            vr::EVRApplicationError::UnknownApplication
        );
        assert_eq!(
            apps.IdentifyApplication(pid, key.as_ptr()),
            vr::EVRApplicationError::None
        );
        assert_eq!(apps.GetApplicationProcessId(key.as_ptr()), pid);
        // For finding the bindings chosen in SteamVR.
        assert_eq!(identified_app_key().as_deref(), Some("org.example.app"));
        assert_eq!(
            apps.GetApplicationKeyByProcessId(pid, buffer.as_mut_ptr(), 4),
            vr::EVRApplicationError::BufferTooSmall
        );
        assert_eq!(
            apps.GetApplicationKeyByProcessId(pid, buffer.as_mut_ptr(), buffer.len() as u32),
            vr::EVRApplicationError::None
        );
        assert_eq!(unsafe { CStr::from_ptr(buffer.as_ptr()) }, key);

        // Another process's identity isn't ours, pid 0 is.
        apps.IdentifyApplication(pid + 1, c"org.example.launched".as_ptr());
        assert_eq!(apps.GetApplicationProcessId(key.as_ptr()), pid);
        apps.IdentifyApplication(0, c"org.example.other".as_ptr());
        assert_eq!(
            apps.GetApplicationProcessId(c"org.example.other".as_ptr()),
            pid
        );
        assert_eq!(apps.GetApplicationProcessId(key.as_ptr()), 0);

        assert_eq!(
            apps.RemoveApplicationManifest(path.as_ptr()),
            vr::EVRApplicationError::None
        );
        assert!(!apps.IsApplicationInstalled(key.as_ptr()));
    }

    #[test]
    fn unknown_enum_names() {
        let apps = Applications::new(&Injector::default());
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
