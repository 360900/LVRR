//! IVRApplications: the apps SteamVR would know, from the manifests added in this process, the
//! ones registered with SteamVR (its `appconfig.json`, which lists Steam's `steamapps.vrmanifest`
//! of installed VR games) and the ones registered with xrizer; launching them; and the identity
//! of this process.

use crate::clientcore::{Injected, Injector};
use crate::compositor::Compositor;
use crate::input::{Input, host_path, isolated_env_var, steam_config_dirs, xrizer_config_dir};
use crate::openxr_data::OpenXrData;
use log::{debug, info, warn};
use openvr as vr;
use serde_json::Value;
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(macros::InterfaceImpl)]
#[interface = "IVRApplications"]
#[versions(008, 007, 006, 005, 004, 003, 002)]
pub struct Applications {
    vtables: Vtables,
    apps: Mutex<Registry>,
    /// The app key this process identified as.
    identity: Mutex<Option<CString>>,
    /// Processes this app registered as its own.
    subprocesses: Mutex<Vec<u32>>,
    /// The apps launched from here that may still run.
    launched: Mutex<Vec<(String, Child)>>,
    /// The arguments of MIME type launches of this app, by handle minus one.
    launch_args: Mutex<Vec<String>>,
    input: Injected<Input<Compositor>>,
    openxr: Arc<OpenXrData<Compositor>>,
}

/// The app key this process last identified as, for code without a handle to the interface.
static IDENTIFIED_KEY: Mutex<Option<String>> = Mutex::new(None);

/// The app key this process identified as with IdentifyApplication, if it did.
pub fn identified_app_key() -> Option<String> {
    IDENTIFIED_KEY.lock().unwrap().clone()
}

/// An app of a .vrmanifest.
#[derive(Clone)]
struct App {
    key: String,
    /// The manifest's directory, which its relative paths count from.
    dir: PathBuf,
    /// The manifest's `source`.
    source: Option<String>,
    /// The app's entry.
    fields: serde_json::Map<String, Value>,
}

impl App {
    fn string(&self, name: &str) -> Option<&str> {
        self.fields.get(name)?.as_str()
    }

    fn flag(&self, name: &str) -> bool {
        self.fields
            .get(name)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// A path of the manifest, made absolute from the manifest's directory.
    fn path(&self, name: &str) -> Option<PathBuf> {
        let path = self.string(name)?;
        if path.contains("://") {
            return Some(PathBuf::from(path));
        }
        Some(self.dir.join(path))
    }

    /// A localized string, in US English or else the first language that has it.
    fn localized(&self, name: &str) -> Option<&str> {
        let strings = self.fields.get("strings")?.as_object()?;
        strings
            .get("en_us")
            .into_iter()
            .chain(strings.values())
            .find_map(|language| language.get(name)?.as_str())
    }

    fn mime_types(&self) -> Vec<&str> {
        self.fields
            .get("mime_types")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    }

    fn is_dashboard_overlay(&self) -> bool {
        self.flag("is_dashboard_overlay")
    }
}

/// Where the apps come from: manifests added here and the registered ones, read again when a
/// file changes.
#[derive(Default)]
struct Registry {
    /// The manifests added in this process, newest last.
    added: Vec<PathBuf>,
    apps: Vec<App>,
    /// The files the apps were read from, with their modification times.
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Registry {
    /// The manifests to read: those added here, then those registered with xrizer and SteamVR,
    /// then Steam's list of installed VR games.
    fn manifests(&self) -> Vec<PathBuf> {
        let mut paths = self.added.clone();
        let registries = registry_file().into_iter().chain(
            steam_config_dirs(&isolated_env_var)
                .into_iter()
                .flat_map(|dir| [dir.join("appconfig.json"), dir.join("steamapps.vrmanifest")]),
        );
        for registry in registries {
            let registry = host_path(&registry);
            if registry.extension().is_some_and(|ext| ext == "json") {
                paths.extend(
                    registered_manifests(&registry)
                        .into_iter()
                        .map(|p| host_path(&p)),
                );
            } else {
                paths.push(registry);
            }
        }
        let mut seen = Vec::new();
        paths.retain(|path| {
            let new = !seen.contains(path);
            seen.push(path.clone());
            new
        });
        paths
    }

    /// Reads the manifests again if any changed since.
    fn refresh(&mut self) {
        let stamps: Vec<_> = self
            .manifests()
            .into_iter()
            .map(|path| {
                let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                (path, modified)
            })
            .collect();
        if stamps == self.stamps {
            return;
        }
        let mut apps: Vec<App> = Vec::new();
        for (path, _) in &stamps {
            for app in read_manifest(path).unwrap_or_default() {
                // The first manifest listing a key wins, so the ones added here do.
                if !apps.iter().any(|known| known.key == app.key) {
                    apps.push(app);
                }
            }
        }
        debug!("{} applications known", apps.len());
        self.apps = apps;
        self.stamps = stamps;
    }

    fn get(&self, key: &str) -> Option<&App> {
        self.apps.iter().find(|app| app.key == key)
    }
}

/// `$XDG_CONFIG_HOME/xrizer/appconfig.json`, the manifests registered with xrizer, in the format
/// of SteamVR's.
fn registry_file() -> Option<PathBuf> {
    Some(xrizer_config_dir()?.join("appconfig.json"))
}

/// The `manifest_paths` of an `appconfig.json`.
fn registered_manifests(path: &Path) -> Vec<PathBuf> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<Value>(&data) else {
        warn!("Ignoring {}: not JSON", path.display());
        return Vec::new();
    };
    json["manifest_paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect()
}

/// Registers or forgets a manifest in xrizer's `appconfig.json`.
fn register_manifest(path: &Path, add: bool) {
    let Some(file) = registry_file() else {
        return;
    };
    let mut paths = registered_manifests(&file);
    paths.retain(|known| known != path);
    if add {
        paths.push(path.to_path_buf());
    }
    let json = serde_json::json!({ "manifest_paths": paths });
    let write = || {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&file, serde_json::to_string_pretty(&json).unwrap())
    };
    if let Err(e) = write() {
        warn!("Couldn't write {}: {e}", file.display());
    }
}

/// The apps a .vrmanifest lists.
fn read_manifest(path: &Path) -> Result<Vec<App>, vr::EVRApplicationError> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        warn!("Can't read application manifest {}: {e}", path.display());
        vr::EVRApplicationError::NoManifest
    })?;
    let invalid = |e: &dyn std::fmt::Display| {
        warn!("Invalid application manifest {}: {e}", path.display());
        vr::EVRApplicationError::InvalidManifest
    };
    let manifest: Value = serde_json::from_str(&contents).map_err(|e| invalid(&e))?;
    let source = manifest["source"].as_str().map(str::to_owned);
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let apps: Vec<App> = manifest["applications"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|app| {
            let fields = app.as_object()?;
            let key = fields.get("app_key")?.as_str()?;
            Some(App {
                key: key.to_owned(),
                dir: dir.clone(),
                source: source.clone(),
                fields: fields.clone(),
            })
        })
        .collect();
    if apps.is_empty() {
        return Err(invalid(&"no applications with an app_key"));
    }
    Ok(apps)
}

/// The per-app settings SteamVR keeps in `vrappconfig/<key>.vrappconfig`.
fn steamvr_app_config(key: &str) -> Option<Value> {
    steam_config_dirs(&isolated_env_var)
        .into_iter()
        .map(|dir| host_path(&dir.join("vrappconfig").join(format!("{key}.vrappconfig"))))
        .find_map(|path| serde_json::from_slice(&std::fs::read(path).ok()?).ok())
}

/// Copies a string to an app's buffer, clearing it when it's too small, and returns the size
/// the string needs.
fn write_string(value: &str, buffer: *mut c_char, size: u32) -> u32 {
    let Ok(value) = CString::new(value) else {
        return 0;
    };
    let bytes = value.to_bytes_with_nul();
    if buffer.is_null() || (size as usize) < bytes.len() {
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
    } else {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), buffer, bytes.len()) };
    }
    bytes.len() as u32
}

/// Copies an app key to an app's buffer.
fn write_key(key: &str, buffer: *mut c_char, size: u32) -> vr::EVRApplicationError {
    let needed = write_string(key, buffer, size);
    if needed == 0 || buffer.is_null() || size < needed {
        vr::EVRApplicationError::BufferTooSmall
    } else {
        vr::EVRApplicationError::None
    }
}

fn set_error(error: *mut vr::EVRApplicationError, value: vr::EVRApplicationError) {
    if let Some(error) = unsafe { error.as_mut() } {
        *error = value;
    }
}

fn str_arg<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

impl Applications {
    pub fn new(openxr: Arc<OpenXrData<Compositor>>, injector: &Injector) -> Self {
        Self {
            vtables: Default::default(),
            apps: Default::default(),
            identity: Default::default(),
            subprocesses: Default::default(),
            launched: Default::default(),
            launch_args: Default::default(),
            input: injector.inject(),
            openxr,
        }
    }

    /// The app of `key`, as of its manifest now.
    fn app(&self, key: &str) -> Option<App> {
        let mut apps = self.apps.lock().unwrap();
        apps.refresh();
        apps.get(key).cloned()
    }

    fn own_key(&self) -> Option<String> {
        self.identity
            .lock()
            .unwrap()
            .as_ref()
            .map(|key| key.to_string_lossy().into_owned())
    }

    /// The pid of a launched app still running, forgetting the ones that exited.
    fn launched_pid(&self, key: &str) -> Option<u32> {
        let mut launched = self.launched.lock().unwrap();
        launched.retain_mut(|(_, child)| child.try_wait().is_ok_and(|exited| exited.is_none()));
        launched
            .iter()
            .find(|(launched, _)| launched == key)
            .map(|(_, child)| child.id())
    }

    /// Starts `app` with `args` after its manifest's arguments, like SteamVR starts an app from
    /// its manifest, and remembers the process.
    fn launch(&self, app: &App, args: &str) -> vr::EVRApplicationError {
        if app.flag("is_template") {
            return vr::EVRApplicationError::IsTemplate;
        }
        if self.own_key().as_deref() == Some(&app.key) || self.launched_pid(&app.key).is_some() {
            return vr::EVRApplicationError::ApplicationAlreadyRunning;
        }
        let launch_type = app.string("launch_type").unwrap_or("binary");
        let spawned = match launch_type {
            "binary" => {
                let Some(binary) = app
                    .path("binary_path_linux")
                    .or_else(|| app.path("binary_path"))
                else {
                    warn!("{} has no binary for this platform", app.key);
                    return vr::EVRApplicationError::LaunchFailed;
                };
                let working_directory = app
                    .path("working_directory")
                    .or_else(|| binary.parent().map(Path::to_path_buf));
                let mut command = Command::new(&binary);
                command.args(app.string("arguments").unwrap_or("").split_whitespace());
                if !args.is_empty() {
                    command.arg(args);
                }
                if let Some(dir) = working_directory {
                    command.current_dir(dir);
                }
                info!("Launching {} ({})", app.key, binary.display());
                command.spawn()
            }
            "url" => {
                let Some(url) = app.string("url") else {
                    warn!("{} has no url", app.key);
                    return vr::EVRApplicationError::LaunchFailed;
                };
                info!("Launching {} ({url})", app.key);
                // Steam handles its own URLs if xdg-open isn't there.
                Command::new("xdg-open")
                    .arg(url)
                    .spawn()
                    .or_else(|_| Command::new("steam").arg(url).spawn())
            }
            other => {
                warn!("Can't launch {}: launch type {other}", app.key);
                return vr::EVRApplicationError::LaunchFailed;
            }
        };
        match spawned {
            Ok(child) => {
                if launch_type == "binary" {
                    self.launched.lock().unwrap().push((app.key.clone(), child));
                }
                vr::EVRApplicationError::None
            }
            Err(e) => {
                warn!("Couldn't launch {}: {e}", app.key);
                vr::EVRApplicationError::LaunchFailed
            }
        }
    }

    /// A scene app launched from a scene app replaces it: "The existing scene application will
    /// exit and then the new application will start."
    fn quit_for(&self, app: &App) {
        if self.openxr.application_type == vr::EVRApplicationType::Scene
            && !app.is_dashboard_overlay()
        {
            info!("Quitting for {}", app.key);
            self.openxr.push_quit(false);
        }
    }

    fn list_updated(&self) {
        self.openxr
            .push_event(vr::EVREventType::ApplicationListUpdated, Default::default());
    }
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
        let (Some(binary_path), Some(arguments), Some(working_directory)) = (
            str_arg(binary_path),
            str_arg(arguments),
            str_arg(working_directory),
        ) else {
            return vr::EVRApplicationError::InvalidParameter;
        };
        info!(
            "LaunchInternalProcess: {binary_path:?}, ARGS: {arguments:?}, WD: {working_directory:?}"
        );
        let mut command = Command::new(binary_path);
        command.args(arguments.split_whitespace());
        // "If working directory is NULL or "" the directory portion of the binary path will be
        // the working directory."
        let working_directory = if working_directory.is_empty() {
            Path::new(binary_path).parent().map(Path::to_path_buf)
        } else {
            Some(PathBuf::from(working_directory))
        };
        if let Some(dir) = working_directory.filter(|dir| dir.is_dir()) {
            command.current_dir(dir);
        }
        match command.spawn() {
            Ok(child) => {
                // The new process is part of this app.
                self.subprocesses.lock().unwrap().push(child.id());
                vr::EVRApplicationError::None
            }
            Err(e) => {
                warn!("Failed to launch internal process: {e}");
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
    /// Nothing is ever starting or in room setup here, so a launch can go ahead.
    fn PerformApplicationPrelaunchCheck(&self, key: *const c_char) -> vr::EVRApplicationError {
        match str_arg(key).and_then(|key| self.app(key)) {
            Some(_) => vr::EVRApplicationError::None,
            None => vr::EVRApplicationError::UnknownApplication,
        }
    }
    fn GetSceneApplicationState(&self) -> vr::EVRSceneApplicationState {
        self.openxr.scene_application_state()
    }
    /// Launches happen at once, so no app is ever starting.
    fn GetStartingApplication(&self, buffer: *mut c_char, size: u32) -> vr::EVRApplicationError {
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        vr::EVRApplicationError::NoApplication
    }
    fn GetApplicationLaunchArguments(&self, handle: u32, buffer: *mut c_char, size: u32) -> u32 {
        let args = self.launch_args.lock().unwrap();
        match handle
            .checked_sub(1)
            .and_then(|index| args.get(index as usize))
        {
            Some(args) => write_string(args, buffer, size),
            None => {
                if !buffer.is_null() && size > 0 {
                    unsafe { *buffer = 0 };
                }
                0
            }
        }
    }
    fn GetApplicationsThatSupportMimeType(
        &self,
        mime_type: *const c_char,
        buffer: *mut c_char,
        size: u32,
    ) -> u32 {
        let Some(mime_type) = str_arg(mime_type) else {
            return 0;
        };
        let keys = {
            let mut apps = self.apps.lock().unwrap();
            apps.refresh();
            apps.apps
                .iter()
                .filter(|app| app.mime_types().contains(&mime_type))
                .map(|app| app.key.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        write_string(&keys, buffer, size)
    }
    fn GetApplicationSupportedMimeTypes(
        &self,
        key: *const c_char,
        buffer: *mut c_char,
        size: u32,
    ) -> bool {
        let Some(app) = str_arg(key).and_then(|key| self.app(key)) else {
            return false;
        };
        let types = app.mime_types().join(",");
        let needed = write_string(&types, buffer, size);
        !types.is_empty() && !buffer.is_null() && size >= needed
    }
    fn GetDefaultApplicationForMimeType(
        &self,
        mime_type: *const c_char,
        buffer: *mut c_char,
        size: u32,
    ) -> bool {
        let Some(key) = str_arg(mime_type).and_then(|mime| crate::settings::get_str("mime", mime))
        else {
            return false;
        };
        write_key(&key, buffer, size) == vr::EVRApplicationError::None
    }
    /// Kept in xrizer's settings, in a `mime` section.
    fn SetDefaultApplicationForMimeType(
        &self,
        key: *const c_char,
        mime_type: *const c_char,
    ) -> vr::EVRApplicationError {
        let (Some(key), Some(mime_type)) = (str_arg(key), str_arg(mime_type)) else {
            return vr::EVRApplicationError::InvalidParameter;
        };
        if self.app(key).is_none() {
            return vr::EVRApplicationError::UnknownApplication;
        }
        crate::settings::set_str("mime", mime_type, key);
        vr::EVRApplicationError::None
    }
    /// What the app set, else what SteamVR has for the app.
    fn GetApplicationAutoLaunch(&self, key: *const c_char) -> bool {
        let Some(key) = str_arg(key) else {
            return false;
        };
        crate::settings::get_bool(key, "autolaunch")
            .or_else(|| steamvr_app_config(key)?["autolaunch"].as_bool())
            .unwrap_or(false)
    }
    /// Kept in xrizer's settings, in the app's section. Nothing here starts apps on its own.
    fn SetApplicationAutoLaunch(
        &self,
        key: *const c_char,
        auto_launch: bool,
    ) -> vr::EVRApplicationError {
        let Some(key) = str_arg(key) else {
            return vr::EVRApplicationError::InvalidParameter;
        };
        let Some(app) = self.app(key) else {
            return vr::EVRApplicationError::UnknownApplication;
        };
        // "This is only valid for applications which return true for IsDashboardOverlay"
        if !app.is_dashboard_overlay() {
            return vr::EVRApplicationError::InvalidApplication;
        }
        crate::settings::set_bool(key, "autolaunch", auto_launch);
        vr::EVRApplicationError::None
    }
    /// No property is a uint64 in this version of OpenVR.
    fn GetApplicationPropertyUint64(
        &self,
        key: *const c_char,
        _: vr::EVRApplicationProperty,
        error: *mut vr::EVRApplicationError,
    ) -> u64 {
        let known = str_arg(key).and_then(|key| self.app(key)).is_some();
        set_error(
            error,
            if known {
                vr::EVRApplicationError::UnknownProperty
            } else {
                vr::EVRApplicationError::UnknownApplication
            },
        );
        0
    }
    fn GetApplicationPropertyBool(
        &self,
        key: *const c_char,
        property: vr::EVRApplicationProperty,
        error: *mut vr::EVRApplicationError,
    ) -> bool {
        use vr::EVRApplicationProperty as Prop;
        let Some(app) = str_arg(key).and_then(|key| self.app(key)) else {
            set_error(error, vr::EVRApplicationError::UnknownApplication);
            return false;
        };
        let name = match property {
            Prop::IsDashboardOverlay_Bool => "is_dashboard_overlay",
            Prop::IsTemplate_Bool => "is_template",
            Prop::IsInstanced_Bool => "is_instanced",
            Prop::IsInternal_Bool => "is_internal",
            Prop::WantsCompositorPauseInStandby_Bool => "wants_compositor_pause_in_standby",
            Prop::IsHidden_Bool => "is_hidden",
            _ => {
                set_error(error, vr::EVRApplicationError::UnknownProperty);
                return false;
            }
        };
        set_error(error, vr::EVRApplicationError::None);
        app.flag(name)
    }
    fn GetApplicationPropertyString(
        &self,
        key: *const c_char,
        property: vr::EVRApplicationProperty,
        buffer: *mut c_char,
        size: u32,
        error: *mut vr::EVRApplicationError,
    ) -> u32 {
        use vr::EVRApplicationProperty as Prop;
        if !buffer.is_null() && size > 0 {
            unsafe { *buffer = 0 };
        }
        let Some(app) = str_arg(key).and_then(|key| self.app(key)) else {
            set_error(error, vr::EVRApplicationError::UnknownApplication);
            return 0;
        };
        let path = |name| app.path(name).map(|p| p.to_string_lossy().into_owned());
        let value = match property {
            Prop::Name_String => app.localized("name").map(str::to_owned),
            Prop::Description_String => app.localized("description").map(str::to_owned),
            Prop::LaunchType_String => app.string("launch_type").map(str::to_owned),
            Prop::WorkingDirectory_String => path("working_directory"),
            Prop::BinaryPath_String => path("binary_path_linux").or_else(|| path("binary_path")),
            Prop::Arguments_String => app.string("arguments").map(str::to_owned),
            Prop::URL_String => app.string("url").map(str::to_owned),
            Prop::NewsURL_String => app.string("news_url").map(str::to_owned),
            Prop::ImagePath_String => path("image_path"),
            Prop::ImagePathCapsule_String => path("image_path_capsule"),
            Prop::Source_String => app.source.clone(),
            Prop::ActionManifestURL_String => path("action_manifest_path"),
            _ => {
                set_error(error, vr::EVRApplicationError::UnknownProperty);
                return 0;
            }
        };
        let Some(value) = value else {
            set_error(error, vr::EVRApplicationError::PropertyNotSet);
            return 0;
        };
        let needed = write_string(&value, buffer, size);
        set_error(
            error,
            if buffer.is_null() || size < needed {
                vr::EVRApplicationError::BufferTooSmall
            } else {
                vr::EVRApplicationError::None
            },
        );
        needed
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
    /// This process if it's the app, else a process launched from here that still runs.
    fn GetApplicationProcessId(&self, key: *const c_char) -> u32 {
        let Some(key) = str_arg(key) else {
            return 0;
        };
        if self.own_key().as_deref() == Some(key) {
            return std::process::id();
        }
        self.launched_pid(key).unwrap_or(0)
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
    /// Launches happen at once, so there's never one to cancel.
    fn CancelApplicationLaunch(&self, _: *const c_char) -> bool {
        false
    }
    fn LaunchDashboardOverlay(&self, key: *const c_char) -> vr::EVRApplicationError {
        let Some(app) = str_arg(key).and_then(|key| self.app(key)) else {
            return vr::EVRApplicationError::UnknownApplication;
        };
        // "This call is only valid for dashboard overlay applications."
        if !app.is_dashboard_overlay() {
            return vr::EVRApplicationError::InvalidApplication;
        }
        self.launch(&app, "")
    }
    /// The default app for the type gets the arguments: on its command line when it's started,
    /// or, when it's this process, through an ApplicationMimeTypeLoad event and
    /// GetApplicationLaunchArguments.
    fn LaunchApplicationFromMimeType(
        &self,
        mime_type: *const c_char,
        args: *const c_char,
    ) -> vr::EVRApplicationError {
        let (Some(mime_type), Some(args)) = (str_arg(mime_type), str_arg(args)) else {
            return vr::EVRApplicationError::InvalidParameter;
        };
        let Some(key) = crate::settings::get_str("mime", mime_type) else {
            return vr::EVRApplicationError::NoApplication;
        };
        if self.own_key().as_deref() == Some(&key) {
            let handle = {
                let mut launch_args = self.launch_args.lock().unwrap();
                launch_args.push(args.to_owned());
                launch_args.len() as u32
            };
            self.openxr.push_event(
                vr::EVREventType::ApplicationMimeTypeLoad,
                vr::VREvent_Data_t {
                    applicationLaunch: vr::VREvent_ApplicationLaunch_t {
                        pid: std::process::id(),
                        unArgsHandle: handle,
                    },
                },
            );
            return vr::EVRApplicationError::None;
        }
        let Some(app) = self.app(&key) else {
            return vr::EVRApplicationError::UnknownApplication;
        };
        let result = self.launch(&app, args);
        if result == vr::EVRApplicationError::None {
            self.quit_for(&app);
        }
        result
    }
    /// Templates are their own apps with keys of their own, which nothing here keeps.
    fn LaunchTemplateApplication(
        &self,
        _: *const c_char,
        _: *const c_char,
        _: *const vr::AppOverrideKeys_t,
        _: u32,
    ) -> vr::EVRApplicationError {
        crate::warn_unimplemented!("LaunchTemplateApplication");
        vr::EVRApplicationError::NotImplemented
    }
    fn LaunchApplication(&self, key: *const c_char) -> vr::EVRApplicationError {
        let Some(app) = str_arg(key).and_then(|key| self.app(key)) else {
            return vr::EVRApplicationError::UnknownApplication;
        };
        // "This call is not valid for dashboard overlay applications."
        if app.is_dashboard_overlay() {
            return vr::EVRApplicationError::InvalidApplication;
        }
        let result = self.launch(&app, "");
        if result == vr::EVRApplicationError::None {
            self.quit_for(&app);
        }
        result
    }
    fn GetApplicationKeyByProcessId(
        &self,
        pid: u32,
        buffer: *mut c_char,
        size: u32,
    ) -> vr::EVRApplicationError {
        let own = pid == std::process::id() || self.subprocesses.lock().unwrap().contains(&pid);
        let key = if own {
            self.own_key()
        } else {
            let mut launched = self.launched.lock().unwrap();
            launched.retain_mut(|(_, child)| child.try_wait().is_ok_and(|e| e.is_none()));
            launched
                .iter()
                .find(|(_, child)| child.id() == pid)
                .map(|(key, _)| key.clone())
        };
        match key {
            Some(key) => write_key(&key, buffer, size),
            None => {
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
        let mut apps = self.apps.lock().unwrap();
        apps.refresh();
        match apps.apps.get(index as usize) {
            Some(app) => write_key(&app.key, buffer, size),
            None => {
                if !buffer.is_null() && size > 0 {
                    unsafe { *buffer = 0 };
                }
                vr::EVRApplicationError::InvalidIndex
            }
        }
    }
    fn GetApplicationCount(&self) -> u32 {
        let mut apps = self.apps.lock().unwrap();
        apps.refresh();
        apps.apps.len() as u32
    }
    fn IsApplicationInstalled(&self, key: *const c_char) -> bool {
        str_arg(key).and_then(|key| self.app(key)).is_some()
    }
    fn RemoveApplicationManifest(&self, path: *const c_char) -> vr::EVRApplicationError {
        let Some(path) = str_arg(path) else {
            return vr::EVRApplicationError::InvalidParameter;
        };
        let path = PathBuf::from(path);
        self.apps
            .lock()
            .unwrap()
            .added
            .retain(|added| *added != path);
        register_manifest(&path, false);
        self.list_updated();
        vr::EVRApplicationError::None
    }
    /// A manifest added for good is registered in xrizer's `appconfig.json`, like SteamVR
    /// registers it in its own; a temporary one lasts this process.
    fn AddApplicationManifest(
        &self,
        path: *const c_char,
        temporary: bool,
    ) -> vr::EVRApplicationError {
        let Some(path) = str_arg(path) else {
            return vr::EVRApplicationError::InvalidParameter;
        };
        let path = PathBuf::from(path);
        let apps = match read_manifest(&path) {
            Ok(apps) => apps,
            Err(e) => return e,
        };
        let keys: Vec<_> = apps.iter().map(|app| app.key.as_str()).collect();
        info!("Added application manifest {} ({keys:?})", path.display());
        {
            let mut registry = self.apps.lock().unwrap();
            // Adding a manifest again moves it to the front, where the newest wins.
            registry.added.retain(|added| *added != path);
            registry.added.insert(0, path.clone());
        }
        if !temporary {
            register_manifest(&path, true);
        }
        self.list_updated();
        vr::EVRApplicationError::None
    }

    /// "Registers a new subprocess launched by the calling application."
    fn RegisterSubprocess(&self, pid: u32) -> vr::EVRApplicationError {
        self.subprocesses.lock().unwrap().push(pid);
        vr::EVRApplicationError::None
    }
}

impl vr::IVRApplications006On007 for Applications {
    /// Launches happen at once: nothing is ever in transition.
    fn GetTransitionState(&self) -> vr::EVRApplicationTransitionState {
        vr::EVRApplicationTransitionState::None
    }
    fn GetApplicationsTransitionStateNameFromEnum(
        &self,
        state: vr::EVRApplicationTransitionState,
    ) -> *const ::std::os::raw::c_char {
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
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vr::{IVRApplications006On007, IVRApplications008_Interface};

    fn test_openxr() -> Arc<OpenXrData<Compositor>> {
        Arc::new(OpenXrData::new(&Injector::default(), vr::EVRApplicationType::Other).unwrap())
    }

    /// A home with a Steam install whose SteamVR knows one game, and xrizer's config directory,
    /// all under a directory of their own for this thread.
    fn fixture(name: &str) -> (PathBuf, Applications) {
        let dir = std::env::temp_dir().join(format!("xrizer-apps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("home/.steam/steam/config");
        std::fs::create_dir_all(config.join("vrappconfig")).unwrap();
        let steam_manifest = config.join("steamapps.vrmanifest");
        std::fs::write(
            &steam_manifest,
            r#"{"source": "steam", "applications": [{
                "app_key": "steam.app.42",
                "launch_type": "url",
                "url": "steam://launch/42/VR",
                "strings": {"de_de": {"name": "Spiel"}, "en_us": {"name": "Game", "description": "A game"}},
                "image_path": "https://example.com/42.jpg",
                "action_manifest_path": "actions.json"
            }]}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("appconfig.json"),
            serde_json::json!({"manifest_paths": [steam_manifest]}).to_string(),
        )
        .unwrap();
        std::fs::write(
            config.join("vrappconfig/steam.app.42.vrappconfig"),
            r#"{"autolaunch": true, "last_launch_time": "1"}"#,
        )
        .unwrap();
        crate::input::TEST_ENV.with_borrow_mut(|env| {
            env.insert("HOME", dir.join("home").into());
            env.insert("XDG_CONFIG_HOME", dir.join("xdg").into());
        });
        (dir, Applications::new(test_openxr(), &Injector::default()))
    }

    fn key_at(apps: &Applications, index: u32) -> Result<String, vr::EVRApplicationError> {
        let mut buffer = [0 as c_char; vr::k_unMaxApplicationKeyLength as usize];
        match apps.GetApplicationKeyByIndex(index, buffer.as_mut_ptr(), buffer.len() as u32) {
            vr::EVRApplicationError::None => Ok(unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_str()
                .unwrap()
                .to_owned()),
            e => Err(e),
        }
    }

    fn string_property(
        apps: &Applications,
        key: &CStr,
        property: vr::EVRApplicationProperty,
    ) -> Result<String, vr::EVRApplicationError> {
        let mut buffer = [1 as c_char; 512];
        let mut error = vr::EVRApplicationError::IPCFailed;
        let len = apps.GetApplicationPropertyString(
            key.as_ptr(),
            property,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut error,
        );
        match error {
            vr::EVRApplicationError::None => {
                let value = unsafe { CStr::from_ptr(buffer.as_ptr()) };
                assert_eq!(len as usize, value.to_bytes_with_nul().len());
                Ok(value.to_str().unwrap().to_owned())
            }
            e => {
                assert_eq!(buffer[0], 0);
                Err(e)
            }
        }
    }

    /// Vivecraft adds its manifest, checks it's installed and identifies as its app.
    #[test]
    fn manifests_and_identity() {
        let (dir, apps) = fixture("identity");
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
        assert!(apps.IsApplicationInstalled(c"steam.app.42".as_ptr()));
        assert_eq!(apps.GetApplicationCount(), 1);
        let added = apps.AddApplicationManifest(path.as_ptr(), true);
        let rejected = apps.AddApplicationManifest(invalid_path.as_ptr(), true);
        let missing = apps.AddApplicationManifest(c"/nonexistent.vrmanifest".as_ptr(), true);
        assert_eq!(added, vr::EVRApplicationError::None);
        assert_eq!(rejected, vr::EVRApplicationError::InvalidManifest);
        assert_eq!(missing, vr::EVRApplicationError::NoManifest);
        assert!(apps.IsApplicationInstalled(key.as_ptr()));
        // Added manifests come first.
        assert_eq!(apps.GetApplicationCount(), 2);
        assert_eq!(key_at(&apps, 0).as_deref(), Ok("org.example.app"));
        assert_eq!(key_at(&apps, 1).as_deref(), Ok("steam.app.42"));
        assert_eq!(key_at(&apps, 2), Err(vr::EVRApplicationError::InvalidIndex));

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
        // A registered subprocess is part of the app.
        assert_eq!(
            apps.RegisterSubprocess(pid + 7),
            vr::EVRApplicationError::None
        );
        assert_eq!(
            apps.GetApplicationKeyByProcessId(pid + 7, buffer.as_mut_ptr(), buffer.len() as u32),
            vr::EVRApplicationError::None
        );

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

        // A manifest added for good is known to the next process too, until it's removed.
        assert_eq!(
            apps.AddApplicationManifest(path.as_ptr(), false),
            vr::EVRApplicationError::None
        );
        let later = Applications::new(test_openxr(), &Injector::default());
        assert!(later.IsApplicationInstalled(key.as_ptr()));
        assert_eq!(
            later.RemoveApplicationManifest(path.as_ptr()),
            vr::EVRApplicationError::None
        );
        assert!(!later.IsApplicationInstalled(key.as_ptr()));
        assert!(
            apps.IsApplicationInstalled(key.as_ptr()),
            "still added here"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn properties() {
        use vr::EVRApplicationProperty as Prop;
        let (dir, apps) = fixture("properties");
        let manifest = dir.join("tool/tool.vrmanifest");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            r#"{"applications": [{
                "app_key": "org.example.tool",
                "launch_type": "binary",
                "binary_path_linux": "bin/tool",
                "arguments": "--overlay",
                "is_dashboard_overlay": true,
                "mime_types": ["vr/thing", "vr/other"],
                "strings": {"fr_fr": {"name": "Outil"}}
            }]}"#,
        )
        .unwrap();
        let path = CString::new(manifest.to_str().unwrap()).unwrap();
        assert_eq!(
            apps.AddApplicationManifest(path.as_ptr(), true),
            vr::EVRApplicationError::None
        );

        let game = c"steam.app.42";
        let property = |key, property| string_property(&apps, key, property);
        assert_eq!(property(game, Prop::Name_String).as_deref(), Ok("Game"));
        assert_eq!(
            property(game, Prop::Description_String).as_deref(),
            Ok("A game")
        );
        assert_eq!(
            property(game, Prop::LaunchType_String).as_deref(),
            Ok("url")
        );
        assert_eq!(
            property(game, Prop::URL_String).as_deref(),
            Ok("steam://launch/42/VR")
        );
        assert_eq!(property(game, Prop::Source_String).as_deref(), Ok("steam"));
        assert_eq!(
            property(game, Prop::ImagePath_String).as_deref(),
            Ok("https://example.com/42.jpg")
        );
        // Relative paths count from the manifest.
        assert_eq!(
            property(game, Prop::ActionManifestURL_String).as_deref(),
            Ok(dir
                .join("home/.steam/steam/config/actions.json")
                .to_str()
                .unwrap())
        );
        assert_eq!(
            property(game, Prop::BinaryPath_String),
            Err(vr::EVRApplicationError::PropertyNotSet)
        );
        assert_eq!(
            property(game, Prop(1234)),
            Err(vr::EVRApplicationError::UnknownProperty)
        );
        assert_eq!(
            property(c"nope", Prop::Name_String),
            Err(vr::EVRApplicationError::UnknownApplication)
        );

        let tool = c"org.example.tool";
        assert_eq!(property(tool, Prop::Name_String).as_deref(), Ok("Outil"));
        assert_eq!(
            property(tool, Prop::BinaryPath_String).as_deref(),
            Ok(dir.join("tool/bin/tool").to_str().unwrap())
        );
        assert_eq!(
            property(tool, Prop::Arguments_String).as_deref(),
            Ok("--overlay")
        );
        assert_eq!(
            property(tool, Prop::Source_String),
            Err(vr::EVRApplicationError::PropertyNotSet)
        );
        let mut error = vr::EVRApplicationError::IPCFailed;
        assert!(apps.GetApplicationPropertyBool(
            tool.as_ptr(),
            Prop::IsDashboardOverlay_Bool,
            &mut error
        ));
        assert_eq!(error, vr::EVRApplicationError::None);
        assert!(!apps.GetApplicationPropertyBool(tool.as_ptr(), Prop::IsTemplate_Bool, &mut error));
        assert_eq!(error, vr::EVRApplicationError::None);
        assert!(!apps.GetApplicationPropertyBool(tool.as_ptr(), Prop::Name_String, &mut error));
        assert_eq!(error, vr::EVRApplicationError::UnknownProperty);
        assert_eq!(
            apps.GetApplicationPropertyUint64(tool.as_ptr(), Prop::Name_String, &mut error),
            0
        );
        assert_eq!(error, vr::EVRApplicationError::UnknownProperty);
        // The size a value needs, without a buffer.
        let needed = apps.GetApplicationPropertyString(
            game.as_ptr(),
            Prop::Name_String,
            std::ptr::null_mut(),
            0,
            &mut error,
        );
        assert_eq!(
            (needed, error),
            (5, vr::EVRApplicationError::BufferTooSmall)
        );

        // MIME types, and the default app for one.
        let mut buffer = [1 as c_char; 128];
        assert!(apps.GetApplicationSupportedMimeTypes(tool.as_ptr(), buffer.as_mut_ptr(), 128));
        assert_eq!(
            unsafe { CStr::from_ptr(buffer.as_ptr()) },
            c"vr/thing,vr/other"
        );
        assert!(!apps.GetApplicationSupportedMimeTypes(game.as_ptr(), buffer.as_mut_ptr(), 128));
        let len =
            apps.GetApplicationsThatSupportMimeType(c"vr/other".as_ptr(), buffer.as_mut_ptr(), 128);
        assert_eq!(len, c"org.example.tool".to_bytes_with_nul().len() as u32);
        assert_eq!(unsafe { CStr::from_ptr(buffer.as_ptr()) }, tool);
        assert!(!apps.GetDefaultApplicationForMimeType(
            c"vr/thing".as_ptr(),
            buffer.as_mut_ptr(),
            128
        ));
        assert_eq!(
            apps.SetDefaultApplicationForMimeType(c"nope".as_ptr(), c"vr/thing".as_ptr()),
            vr::EVRApplicationError::UnknownApplication
        );
        assert_eq!(
            apps.SetDefaultApplicationForMimeType(tool.as_ptr(), c"vr/thing".as_ptr()),
            vr::EVRApplicationError::None
        );
        assert!(apps.GetDefaultApplicationForMimeType(
            c"vr/thing".as_ptr(),
            buffer.as_mut_ptr(),
            128
        ));
        assert_eq!(unsafe { CStr::from_ptr(buffer.as_ptr()) }, tool);

        // Auto launch: SteamVR's setting for the game, ours once set, overlays only.
        assert!(apps.GetApplicationAutoLaunch(game.as_ptr()));
        assert!(!apps.GetApplicationAutoLaunch(tool.as_ptr()));
        assert_eq!(
            apps.SetApplicationAutoLaunch(game.as_ptr(), false),
            vr::EVRApplicationError::InvalidApplication
        );
        assert_eq!(
            apps.SetApplicationAutoLaunch(tool.as_ptr(), true),
            vr::EVRApplicationError::None
        );
        assert!(apps.GetApplicationAutoLaunch(tool.as_ptr()));
        assert_eq!(
            crate::settings::get_bool("org.example.tool", "autolaunch"),
            Some(true)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Launches start the app's binary with its arguments; the app launched for a MIME type gets
    /// the item too, or an event when it's this process.
    #[test]
    fn launches() {
        let (dir, apps) = fixture("launches");
        let script = dir.join("launched.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/args.txt\"\nsleep 2\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = dir.join("apps.vrmanifest");
        std::fs::write(
            &manifest,
            r#"{"applications": [
                {"app_key": "org.example.game", "launch_type": "binary", "binary_path_linux": "launched.sh", "arguments": "--vr  --fast", "mime_types": ["vr/item"]},
                {"app_key": "org.example.overlay", "launch_type": "binary", "binary_path_linux": "launched.sh", "is_dashboard_overlay": true},
                {"app_key": "org.example.template", "launch_type": "binary", "binary_path_linux": "launched.sh", "is_template": true},
                {"app_key": "org.example.broken", "launch_type": "binary", "binary_path_linux": "missing.sh"},
                {"app_key": "org.example.odd", "launch_type": "magic"}
            ]}"#,
        )
        .unwrap();
        let path = CString::new(manifest.to_str().unwrap()).unwrap();
        assert_eq!(
            apps.AddApplicationManifest(path.as_ptr(), true),
            vr::EVRApplicationError::None
        );
        let (game, overlay) = (c"org.example.game", c"org.example.overlay");

        assert_eq!(
            apps.LaunchApplication(c"nope".as_ptr()),
            vr::EVRApplicationError::UnknownApplication
        );
        assert_eq!(
            apps.LaunchApplication(c"org.example.template".as_ptr()),
            vr::EVRApplicationError::IsTemplate
        );
        assert_eq!(
            apps.LaunchApplication(c"org.example.broken".as_ptr()),
            vr::EVRApplicationError::LaunchFailed
        );
        assert_eq!(
            apps.LaunchApplication(c"org.example.odd".as_ptr()),
            vr::EVRApplicationError::LaunchFailed
        );
        assert_eq!(
            apps.LaunchApplication(overlay.as_ptr()),
            vr::EVRApplicationError::InvalidApplication
        );
        assert_eq!(
            apps.LaunchDashboardOverlay(game.as_ptr()),
            vr::EVRApplicationError::InvalidApplication
        );
        assert_eq!(
            apps.PerformApplicationPrelaunchCheck(game.as_ptr()),
            vr::EVRApplicationError::None
        );

        assert_eq!(apps.GetApplicationProcessId(game.as_ptr()), 0);
        assert_eq!(
            apps.LaunchApplication(game.as_ptr()),
            vr::EVRApplicationError::None
        );
        let pid = apps.GetApplicationProcessId(game.as_ptr());
        assert_ne!(pid, 0);
        let mut buffer = [1 as c_char; 64];
        assert_eq!(
            apps.GetApplicationKeyByProcessId(pid, buffer.as_mut_ptr(), 64),
            vr::EVRApplicationError::None
        );
        assert_eq!(unsafe { CStr::from_ptr(buffer.as_ptr()) }, game);
        assert_eq!(
            apps.LaunchApplication(game.as_ptr()),
            vr::EVRApplicationError::ApplicationAlreadyRunning
        );
        // The launched process got the manifest's arguments.
        let args = dir.join("args.txt");
        let started = std::time::Instant::now();
        while !args.exists() && started.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&args).unwrap().trim(),
            "--vr --fast"
        );
        assert_eq!(
            apps.LaunchDashboardOverlay(overlay.as_ptr()),
            vr::EVRApplicationError::None
        );

        // A MIME type launch of this process hands over its arguments.
        assert_eq!(
            apps.LaunchApplicationFromMimeType(c"vr/item".as_ptr(), c"thing.vr".as_ptr()),
            vr::EVRApplicationError::NoApplication
        );
        assert_eq!(
            apps.SetDefaultApplicationForMimeType(game.as_ptr(), c"vr/item".as_ptr()),
            vr::EVRApplicationError::None
        );
        assert_eq!(
            apps.LaunchApplicationFromMimeType(c"vr/item".as_ptr(), c"thing.vr".as_ptr()),
            vr::EVRApplicationError::ApplicationAlreadyRunning
        );
        apps.IdentifyApplication(0, game.as_ptr());
        assert_eq!(
            apps.LaunchApplicationFromMimeType(c"vr/item".as_ptr(), c"thing.vr".as_ptr()),
            vr::EVRApplicationError::None
        );
        assert_eq!(
            apps.GetApplicationLaunchArguments(1, buffer.as_mut_ptr(), 64),
            9
        );
        assert_eq!(unsafe { CStr::from_ptr(buffer.as_ptr()) }, c"thing.vr");
        assert_eq!(
            apps.GetApplicationLaunchArguments(2, buffer.as_mut_ptr(), 64),
            0
        );
        assert_eq!(buffer[0], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_enum_names() {
        let apps = Applications::new(test_openxr(), &Injector::default());
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
        assert_eq!(
            apps.GetTransitionState(),
            vr::EVRApplicationTransitionState::None
        );
        assert!(!apps.IsQuitUserPromptRequested());
    }
}
