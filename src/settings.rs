use log::debug;
use openvr as vr;
use openvr::EVRSettingsError;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Mutex;

#[derive(Debug)]
enum Value {
    Bool(bool),
    Int32(i32),
    Float(f32),
    Str(Cow<'static, CStr>),
}

impl Value {
    fn to_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Int32(i) => Some(*i != 0),
            Self::Float(f) => Some(*f != 0.0),
            Self::Str(_) => None,
        }
    }

    fn to_i32(&self) -> Option<i32> {
        match self {
            Self::Bool(b) => Some(*b as i32),
            Self::Int32(i) => Some(*i),
            Self::Float(f) => Some(*f as i32),
            Self::Str(_) => None,
        }
    }

    fn to_f32(&self) -> Option<f32> {
        match self {
            Self::Bool(b) => Some(*b as i32 as f32),
            Self::Int32(i) => Some(*i as f32),
            Self::Float(f) => Some(*f),
            Self::Str(_) => None,
        }
    }

    fn to_str(&self) -> Option<&CStr> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

use Value::*;

const fn string(value: &'static CStr) -> Value {
    Str(Cow::Borrowed(value))
}

/// SteamVR's defaults (resources/settings/default.vrsettings) for the keys the OpenVR headers
/// name. SteamVR reports UnsetSettingHasNoDefault for any other key that wasn't set.
static DEFAULTS: &[(&str, &str, Value)] = &[
    ("steamvr", "contrast", Float(0.0)),
    ("steamvr", "requireHmd", Bool(true)),
    ("steamvr", "forcedDriver", string(c"")),
    ("steamvr", "forcedHmd", string(c"")),
    ("steamvr", "displayDebug", Bool(false)),
    ("steamvr", "debugProcessPipe", string(c"")),
    ("steamvr", "enableDistortion", Bool(true)),
    ("steamvr", "displayDebugX", Int32(0)),
    ("steamvr", "displayDebugY", Int32(0)),
    ("steamvr", "allowDisplayLockedMode", Bool(false)),
    ("steamvr", "loglevel", Int32(3)),
    ("steamvr", "ipd", Float(0.063)),
    ("steamvr", "ipdOffset", Float(0.0)),
    ("steamvr", "background", string(c"")),
    ("steamvr", "backgroundUseDomeProjection", Bool(false)),
    ("steamvr", "backgroundCameraHeight", Float(1.6)),
    ("steamvr", "backgroundDomeRadius", Float(0.0)),
    ("steamvr", "environment", string(c"")),
    ("steamvr", "hdcp14legacyCompatibility", Bool(false)),
    ("steamvr", "gridColor", string(c"")),
    ("steamvr", "playAreaColor", string(c"")),
    ("steamvr", "showStage", Bool(false)),
    ("steamvr", "drawTrackingReferences", Bool(true)),
    ("steamvr", "activateMultipleDrivers", Bool(false)),
    ("steamvr", "usingSpeakers", Bool(false)),
    ("steamvr", "speakersForwardYawOffsetDegrees", Float(0.0)),
    ("steamvr", "basestationPowerManagement", Int32(0)),
    ("steamvr", "ShowBaseStationPowerManagementTip", Int32(1)),
    ("steamvr", "neverKillProcesses", Bool(false)),
    ("steamvr", "renderTargetMultiplier", Float(1.0)),
    ("steamvr", "maxRecommendedResolution", Int32(8192)),
    ("steamvr", "supersampleScale", Float(1.0)),
    ("steamvr", "allowSupersampleFiltering", Bool(true)),
    ("steamvr", "supersampleManualOverride", Bool(false)),
    ("steamvr", "motionSmoothing", Bool(true)),
    ("steamvr", "forceFadeOnBadTracking", Bool(true)),
    ("steamvr", "mirrorView", Int32(0)),
    ("steamvr", "showLegacyMirrorView", Bool(false)),
    ("steamvr", "mirrorViewDisplayMode", Int32(1)),
    ("steamvr", "mirrorViewEye", Int32(1)),
    ("steamvr", "mirrorViewGeometryMaximized", Bool(false)),
    ("steamvr", "showPerfGraph", Bool(false)),
    ("steamvr", "startMonitorFromAppLaunch", Bool(true)),
    ("steamvr", "startCompositorFromAppLaunch", Bool(true)),
    ("steamvr", "startDashboardFromAppLaunch", Bool(true)),
    ("steamvr", "startOverlayAppsFromDashboard", Bool(true)),
    ("steamvr", "enableHomeApp", Bool(false)),
    ("steamvr", "setInitialDefaultHomeApp", Bool(false)),
    ("steamvr", "CycleBackgroundImageTimeSec", Int32(-1)),
    ("steamvr", "retailDemo", Bool(false)),
    ("steamvr", "inputBindingUI", Bool(true)),
    ("steamvr", "legacyInputRebinding", Bool(true)),
    ("steamvr", "debugInputBinding", Bool(false)),
    ("steamvr", "doNotFadeToGrid", Bool(false)),
    ("steamvr", "enableLinuxVulkanAsync", Bool(false)),
    ("steamvr", "enableSafeMode", Bool(false)),
    ("steamvr", "preferredRefreshRate", Float(90.0)),
    ("steamvr", "hmdDisplayColorGainR", Float(1.0)),
    ("steamvr", "hmdDisplayColorGainG", Float(1.0)),
    ("steamvr", "hmdDisplayColorGainB", Float(1.0)),
    (
        "steamvr",
        "customIconStyle",
        Str(Cow::Borrowed(
            c"gradientCorners:#5CBD76,#07AB9C,#6053F6,#0973FA",
        )),
    ),
    (
        "steamvr",
        "customOffIconStyle",
        string(c"gradient:#514f54-#424449"),
    ),
    ("steamvr", "trackingLossColor", string(c"#4F5a64")),
    ("steamvr", "startColor", string(c"#4F5a64")),
    ("steamvr", "globalActionSetPriority", Bool(false)),
    ("steamvr", "blockOculusSDKOnOpenVRLaunchOption", Bool(false)),
    ("steamvr", "blockOculusSDKOnAllLaunches", Bool(false)),
    ("steamvr", "overlayRenderQuality_2", Int32(0)),
    ("steamvr", "displayPortTrainingMode", Int32(0)),
    ("steamvr", "disableKeyboardPrivacy", Bool(false)),
    ("openxr", "metaUnityPluginCompatibility", Int32(0)),
    ("direct_mode", "count", Int32(4)),
    ("direct_mode", "enable", Bool(true)),
    ("userinterface", "StatusAlwaysOnTop", Bool(false)),
    (
        "userinterface",
        "HidePopupsWhenStatusMinimized",
        Bool(false),
    ),
    ("userinterface", "screenshots", Bool(true)),
    ("userinterface", "screenshotType", Int32(2)),
    ("userinterface", "vrmStatusCheckInterval", Int32(1000)),
    ("userinterface", "vrmCheckForSteam", Bool(true)),
    ("notifications", "DoNotDisturb", Bool(false)),
    ("keyboard", "TutorialCompletions", Int32(0)),
    ("keyboard", "ScaleX", Float(1.1)),
    ("keyboard", "ScaleY", Float(1.1)),
    ("keyboard", "OffsetLeftX", Float(0.0)),
    ("keyboard", "OffsetRightX", Float(0.0)),
    ("keyboard", "OffsetY", Float(0.0)),
    ("keyboard", "Smoothing", Bool(false)),
    ("perfcheck", "perfGraphInHMD", Bool(false)),
    ("perfcheck", "allowTimingStore", Bool(false)),
    ("perfcheck", "saveTimingsOnExit", Bool(false)),
    ("perfcheck", "perfTestData", Float(0.0)),
    ("collisionBounds", "CollisionBoundsStyle", Int32(0)),
    (
        "collisionBounds",
        "CollisionBoundsGroundPerimeterOn",
        Bool(false),
    ),
    (
        "collisionBounds",
        "CollisionBoundsCenterMarkerOn",
        Bool(false),
    ),
    ("collisionBounds", "CollisionBoundsPlaySpaceOn", Bool(false)),
    ("collisionBounds", "CollisionBoundsFadeDistance", Float(0.7)),
    ("collisionBounds", "CollisionBoundsWallHeight", Float(2.4)),
    ("collisionBounds", "CollisionBoundsColorGammaR", Int32(0)),
    ("collisionBounds", "CollisionBoundsColorGammaG", Int32(255)),
    ("collisionBounds", "CollisionBoundsColorGammaB", Int32(255)),
    ("collisionBounds", "CollisionBoundsColorGammaA", Int32(153)),
    ("collisionBounds", "enableDriverBoundsImport", Bool(true)),
    ("camera", "enableCamera", Bool(false)),
    ("camera", "showOnController", Bool(false)),
    ("camera", "enableCameraForCollisionBounds", Bool(false)),
    ("camera", "roomView", Int32(0)),
    ("camera", "roomViewStyle", Int32(0)),
    ("camera", "cameraBoundsColorGammaR", Int32(0)),
    ("camera", "cameraBoundsColorGammaG", Int32(255)),
    ("camera", "cameraBoundsColorGammaB", Int32(255)),
    ("camera", "cameraBoundsColorGammaA", Int32(153)),
    ("camera", "cameraBoundsStrength", Int32(78)),
    ("audio", "enablePlaybackMirror", Bool(false)),
    ("audio", "playbackMirrorDevice", string(c"")),
    ("audio", "playbackMirrorDeviceName", string(c"")),
    (
        "audio",
        "enablePlaybackMirrorIndependentVolume",
        Bool(false),
    ),
    ("audio", "viveHDMIGain", Bool(true)),
    ("audio", "dualSpeakerAndJackOutput", Bool(false)),
    ("audio", "muteMicMonitor", Bool(false)),
    ("power", "powerOffOnExit", Bool(true)),
    ("power", "turnOffScreensTimeout", Float(5.0)),
    ("power", "turnOffControllersTimeout", Float(300.0)),
    ("power", "returnToWatchdogTimeout", Float(0.0)),
    ("power", "pauseCompositorOnStandby", Bool(true)),
    ("power", "autoLaunchSteamVROnButtonPress", Bool(true)),
    ("power", "overrideWindowsPowerScheme", Bool(false)),
    ("dashboard", "enableDashboard", Bool(true)),
    ("dashboard", "arcadeMode", Bool(false)),
    ("dashboard", "dashboardScale", Float(1.0)),
    ("dashboard", "desktopScale", Float(1.1)),
    ("dashboard", "webUI", Bool(true)),
    ("dashboard", "standaloneSystemLayer", Bool(false)),
    ("dashboard", "allowSteamOverlays", Bool(true)),
    ("dashboard", "allowVRGamepadUI", Bool(true)),
    ("dashboard", "allowVRGamepadUIViaGamescope", Bool(false)),
    ("dashboard", "steamMatchesHMDFramerate", Bool(true)),
    ("dashboard", "overlayBacksideColor", string(c"#27272CB3")),
    ("dashboard", "grabHandleAcceleration", Float(1.0)),
];

#[derive(Default, macros::InterfaceImpl)]
#[interface = "IVRSettings"]
#[versions(003, 002, 001)]
pub struct Settings {
    vtables: Vtables,
    /// Values set by the app. Unlike SteamVR, these aren't saved.
    values: Mutex<HashMap<(String, String), Value>>,
}

fn key(section: *const c_char, key: *const c_char) -> Option<(String, String)> {
    if section.is_null() || key.is_null() {
        return None;
    }
    let section = unsafe { CStr::from_ptr(section) }.to_string_lossy();
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    Some((section.into_owned(), key.into_owned()))
}

fn set_error(error: *mut EVRSettingsError, value: EVRSettingsError) {
    if let Some(error) = unsafe { error.as_mut() } {
        *error = value;
    }
}

/// Copies `value` into the app's buffer, truncated to fit.
fn write_string(value: &CStr, out: *mut c_char, out_len: u32) {
    if out.is_null() || out_len == 0 {
        return;
    }
    let bytes = value.to_bytes();
    let len = bytes.len().min(out_len as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), out, len);
        *out.add(len) = 0;
    }
}

impl Settings {
    fn set(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: Value,
        error: *mut EVRSettingsError,
    ) {
        let Some(key) = key(section, settings_key) else {
            set_error(error, EVRSettingsError::WriteFailed);
            return;
        };
        debug!("Setting {}/{} to {value:?}", key.0, key.1);
        self.values.lock().unwrap().insert(key, value);
        set_error(error, EVRSettingsError::None);
    }

    /// The value the app set, or else SteamVR's default.
    fn get<T>(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        convert: impl FnOnce(&Value) -> Option<T>,
    ) -> Option<T> {
        let key = key(section, settings_key)?;
        let values = self.values.lock().unwrap();
        let value = values.get(&key).or_else(|| {
            DEFAULTS
                .iter()
                .find(|(s, k, _)| (*s, *k) == (&*key.0, &*key.1))
                .map(|(_, _, value)| value)
        });
        debug!("Getting {}/{}: {value:?}", key.0, key.1);
        value.and_then(convert)
    }

    /// For the getters without a default argument.
    fn get_or_unset<T: Default>(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        error: *mut EVRSettingsError,
        convert: impl FnOnce(&Value) -> Option<T>,
    ) -> T {
        let value = self.get(section, settings_key, convert);
        set_error(
            error,
            match value {
                Some(_) => EVRSettingsError::None,
                None => EVRSettingsError::UnsetSettingHasNoDefault,
            },
        );
        value.unwrap_or_default()
    }

    fn remove(&self, mut matches: impl FnMut(&(String, String)) -> bool) {
        self.values.lock().unwrap().retain(|key, _| !matches(key));
    }
}

impl vr::IVRSettings003_Interface for Settings {
    fn GetSettingsErrorNameFromEnum(&self, error: EVRSettingsError) -> *const c_char {
        #[allow(unreachable_patterns)]
        let error: &'static CStr = match error {
            EVRSettingsError::None => c"",
            EVRSettingsError::IPCFailed => c"IPC Failed",
            EVRSettingsError::WriteFailed => c"Write Failed",
            EVRSettingsError::ReadFailed => c"Read Failed",
            EVRSettingsError::JsonParseFailed => c"JSON Parse Failed",
            EVRSettingsError::UnsetSettingHasNoDefault => c"Unset setting has no default",
            EVRSettingsError::AccessDenied => c"Access denied",
            _ => c"Unknown error",
        };
        error.as_ptr()
    }

    fn SetBool(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: bool,
        error: *mut EVRSettingsError,
    ) {
        self.set(section, settings_key, Bool(value), error);
    }

    fn SetInt32(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: i32,
        error: *mut EVRSettingsError,
    ) {
        self.set(section, settings_key, Int32(value), error);
    }

    fn SetFloat(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: f32,
        error: *mut EVRSettingsError,
    ) {
        self.set(section, settings_key, Float(value), error);
    }

    fn SetString(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: *const c_char,
        error: *mut EVRSettingsError,
    ) {
        if value.is_null() {
            set_error(error, EVRSettingsError::WriteFailed);
            return;
        }
        let value = unsafe { CStr::from_ptr(value) }.to_owned();
        self.set(section, settings_key, Str(value.into()), error);
    }

    fn GetBool(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        error: *mut EVRSettingsError,
    ) -> bool {
        self.get_or_unset(section, settings_key, error, Value::to_bool)
    }

    fn GetInt32(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        error: *mut EVRSettingsError,
    ) -> i32 {
        self.get_or_unset(section, settings_key, error, Value::to_i32)
    }

    fn GetFloat(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        error: *mut EVRSettingsError,
    ) -> f32 {
        self.get_or_unset(section, settings_key, error, Value::to_f32)
    }

    fn GetString(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: *mut c_char,
        value_len: u32,
        error: *mut EVRSettingsError,
    ) {
        let string = self.get_or_unset(section, settings_key, error, |v| {
            v.to_str().map(CStr::to_owned)
        });
        write_string(&string, value, value_len);
    }

    fn RemoveSection(&self, section: *const c_char, error: *mut EVRSettingsError) {
        if !section.is_null() {
            let section = unsafe { CStr::from_ptr(section) }.to_string_lossy();
            debug!("Removing section {section}");
            self.remove(|(s, _)| *s == section);
        }
        set_error(error, EVRSettingsError::None);
    }

    fn RemoveKeyInSection(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        error: *mut EVRSettingsError,
    ) {
        if let Some(key) = key(section, settings_key) {
            debug!("Removing {}/{}", key.0, key.1);
            self.remove(|k| *k == key);
        }
        set_error(error, EVRSettingsError::None);
    }
}

impl vr::IVRSettings002On003 for Settings {
    fn Sync(&self, _force: bool, error: *mut EVRSettingsError) -> bool {
        set_error(error, EVRSettingsError::None);
        false
    }
}

// IVRSettings_001 callers pass the value to use for settings without one.
impl vr::IVRSettings001On002 for Settings {
    fn GetBool(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        default_value: bool,
        error: *mut EVRSettingsError,
    ) -> bool {
        set_error(error, EVRSettingsError::None);
        self.get(section, settings_key, Value::to_bool)
            .unwrap_or(default_value)
    }

    fn GetInt32(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        default_value: i32,
        error: *mut EVRSettingsError,
    ) -> i32 {
        set_error(error, EVRSettingsError::None);
        self.get(section, settings_key, Value::to_i32)
            .unwrap_or(default_value)
    }

    fn GetFloat(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        default_value: f32,
        error: *mut EVRSettingsError,
    ) -> f32 {
        set_error(error, EVRSettingsError::None);
        self.get(section, settings_key, Value::to_f32)
            .unwrap_or(default_value)
    }

    fn GetString(
        &self,
        section: *const c_char,
        settings_key: *const c_char,
        value: *mut c_char,
        value_len: u32,
        default_value: *const c_char,
        error: *mut EVRSettingsError,
    ) {
        set_error(error, EVRSettingsError::None);
        let string = self
            .get(section, settings_key, |v| v.to_str().map(CStr::to_owned))
            .or_else(|| {
                (!default_value.is_null()).then(|| unsafe { CStr::from_ptr(default_value) }.into())
            })
            .unwrap_or_default();
        write_string(&string, value, value_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use vr::{IVRSettings001On002, IVRSettings003_Interface};

    fn get_string(
        settings: &Settings,
        section: &CStr,
        key: &CStr,
        len: usize,
    ) -> (CString, EVRSettingsError) {
        let mut err = EVRSettingsError::IPCFailed;
        let mut buf = vec![1 as c_char; len];
        IVRSettings003_Interface::GetString(
            settings,
            section.as_ptr(),
            key.as_ptr(),
            buf.as_mut_ptr(),
            len as u32,
            &mut err,
        );
        (unsafe { CStr::from_ptr(buf.as_ptr()) }.into(), err)
    }

    #[test]
    fn steamvr_defaults() {
        let settings = Settings::default();
        let mut err = EVRSettingsError::IPCFailed;

        let scale = IVRSettings003_Interface::GetFloat(
            &settings,
            c"steamvr".as_ptr(),
            c"supersampleScale".as_ptr(),
            &mut err,
        );
        assert_eq!((scale, err), (1.0, EVRSettingsError::None));

        err = EVRSettingsError::IPCFailed;
        let smoothing = IVRSettings003_Interface::GetBool(
            &settings,
            c"steamvr".as_ptr(),
            c"motionSmoothing".as_ptr(),
            &mut err,
        );
        assert_eq!((smoothing, err), (true, EVRSettingsError::None));

        // Stored as an int, read as a float.
        let alpha = IVRSettings003_Interface::GetFloat(
            &settings,
            c"collisionBounds".as_ptr(),
            c"CollisionBoundsColorGammaA".as_ptr(),
            &mut err,
        );
        assert_eq!((alpha, err), (153.0, EVRSettingsError::None));

        let unknown = IVRSettings003_Interface::GetFloat(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            &mut err,
        );
        assert_eq!(
            (unknown, err),
            (0.0, EVRSettingsError::UnsetSettingHasNoDefault)
        );
        assert_eq!(
            get_string(&settings, c"steamvr", c"notASetting", 8),
            (
                CString::default(),
                EVRSettingsError::UnsetSettingHasNoDefault
            )
        );

        let null = IVRSettings003_Interface::GetInt32(
            &settings,
            std::ptr::null(),
            std::ptr::null(),
            &mut err,
        );
        assert_eq!((null, err), (0, EVRSettingsError::UnsetSettingHasNoDefault));
    }

    #[test]
    fn set_and_remove() {
        let settings = Settings::default();
        let mut err = EVRSettingsError::IPCFailed;
        let get_scale = |err: &mut EVRSettingsError| {
            IVRSettings003_Interface::GetFloat(
                &settings,
                c"steamvr".as_ptr(),
                c"supersampleScale".as_ptr(),
                err,
            )
        };

        IVRSettings003_Interface::SetFloat(
            &settings,
            c"steamvr".as_ptr(),
            c"supersampleScale".as_ptr(),
            1.5,
            &mut err,
        );
        assert_eq!(err, EVRSettingsError::None);
        assert_eq!(get_scale(&mut err), 1.5);
        IVRSettings003_Interface::RemoveKeyInSection(
            &settings,
            c"steamvr".as_ptr(),
            c"supersampleScale".as_ptr(),
            &mut err,
        );
        assert_eq!(get_scale(&mut err), 1.0);

        IVRSettings003_Interface::SetString(
            &settings,
            c"my_app".as_ptr(),
            c"name".as_ptr(),
            c"hello".as_ptr(),
            &mut err,
        );
        assert_eq!(
            get_string(&settings, c"my_app", c"name", 8),
            (c"hello".into(), EVRSettingsError::None)
        );
        // Truncated to fit.
        assert_eq!(
            get_string(&settings, c"my_app", c"name", 3),
            (c"he".into(), EVRSettingsError::None)
        );
        IVRSettings003_Interface::RemoveSection(&settings, c"my_app".as_ptr(), &mut err);
        assert_eq!(
            get_string(&settings, c"my_app", c"name", 8).1,
            EVRSettingsError::UnsetSettingHasNoDefault
        );
    }

    #[test]
    fn v001_falls_back_to_caller_default() {
        let settings = Settings::default();
        let mut err = EVRSettingsError::IPCFailed;

        let value = IVRSettings001On002::GetFloat(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            2.5,
            &mut err,
        );
        assert_eq!((value, err), (2.5, EVRSettingsError::None));
        let scale = IVRSettings001On002::GetFloat(
            &settings,
            c"steamvr".as_ptr(),
            c"supersampleScale".as_ptr(),
            2.5,
            &mut err,
        );
        assert_eq!(scale, 1.0);
        assert!(IVRSettings001On002::GetBool(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            true,
            &mut err,
        ));

        let mut buf = [1 as c_char; 8];
        IVRSettings001On002::GetString(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            c"abc".as_ptr(),
            &mut err,
        );
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }, c"abc");
        IVRSettings001On002::GetString(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
            &mut err,
        );
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr()) }, c"");

        IVRSettings003_Interface::SetInt32(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            7,
            &mut err,
        );
        let value = IVRSettings001On002::GetInt32(
            &settings,
            c"steamvr".as_ptr(),
            c"notASetting".as_ptr(),
            3,
            &mut err,
        );
        assert_eq!((value, err), (7, EVRSettingsError::None));
    }
}
