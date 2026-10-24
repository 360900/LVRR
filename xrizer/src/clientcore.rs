use crate::{
    applications::Applications,
    chaperone::Chaperone,
    chaperone_setup::ChaperoneSetup,
    compositor::Compositor,
    debug::Debug,
    extended_display::ExtendedDisplay,
    headset_view::HeadsetView,
    input::Input,
    misc_unknown::UnknownInterfaces,
    openxr_data::{self, InitError, OpenXrData, RealOpenXrData},
    overlay::OverlayMan,
    overlayview::OverlayView,
    rendermodels::RenderModels,
    screenshots::Screenshots,
    settings::Settings,
    system::System,
};

use openvr::{
    self as vr, IVRClientCore003_Interface, IVRInput010_Interface, Inherits, InterfaceImpl,
    VtableWrapper,
};

use log::{debug, error, info, warn};
use openxr as xr;
use serde::Deserialize;
use std::any::{Any, TypeId};
use std::collections::{HashMap, hash_map::Entry};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

type ErasedInterface = dyn Any + Sync + Send;

#[repr(transparent)]
#[derive(Default)]
struct InterfaceStore(HashMap<TypeId, Arc<ErasedInterface>>);

impl InterfaceStore {
    fn get<T: 'static + InterfaceImpl>(&self) -> Option<Arc<T>> {
        self.0
            .get(&TypeId::of::<T>())
            .map(|i| i.clone().downcast().unwrap())
    }

    fn entry<T: 'static + InterfaceImpl>(&mut self) -> Entry<'_, TypeId, Arc<ErasedInterface>> {
        self.0.entry(TypeId::of::<T>())
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

pub enum Vtable {
    V2(VtableWrapper<vr::IVRClientCore002, ClientCore>),
    V3(VtableWrapper<vr::IVRClientCore003, ClientCore>),
}

unsafe impl Sync for Vtable {}
unsafe impl Send for Vtable {}

pub struct ClientCore {
    pub base: OnceLock<Vtable>,
    interface_store: Arc<Mutex<InterfaceStore>>,
    openxr: RwLock<Option<Arc<RealOpenXrData>>>,
    /// The last [probe_hmd] result and when it was taken.
    hmd_present: Mutex<Option<(Instant, bool)>>,
}

/// How long a [probe_hmd] result is reused. Apps may poll VR_IsHmdPresent, and each probe
/// creates an OpenXR instance.
const HMD_PRESENT_CACHE_TIME: Duration = Duration::from_secs(2);

/// Whether an OpenXR runtime is reachable and has a head-mounted display, like Init would need.
fn probe_hmd() -> bool {
    let platform_info = ();
    let entry = openxr_data::load_entry().map_err(|e| format!("{e:?}"));

    let result = entry.and_then(|entry| {
        let instance = match entry.create_instance(
            &xr::ApplicationInfo {
                application_name: "XRizer",
                engine_name: "XRizer",
                ..Default::default()
            },
            &xr::ExtensionSet::default(),
            &[],
            &platform_info,
        ) {
            Ok(instance) => instance,
            // The loader allows one instance per process, so one exists already: Init is running
            // or something still holds the last one. Either way the runtime is up.
            Err(xr::sys::Result::ERROR_LIMIT_REACHED) => return Ok(()),
            Err(e) => return Err(format!("instance creation failed: {e}")),
        };
        instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .map(drop)
            .map_err(|e| format!("no HMD system: {e}"))
    });

    match result {
        Ok(()) => true,
        Err(e) => {
            debug!("reporting no HMD present: {e}");
            false
        }
    }
}

/// The error SteamVR reports in the same situation, so apps and Proton can tell a missing
/// runtime or headset apart from a broken one.
fn init_error_code(
    error: &InitError,
    application_type: vr::EVRApplicationType,
) -> vr::EVRInitError {
    let background = application_type == vr::EVRApplicationType::Background;
    match error {
        // There's no loader, or it found no usable runtime.
        InitError::LoaderUnavailable(_) | InitError::EnumeratingExtensionsFailed(_)
            if background =>
        {
            vr::EVRInitError::Init_NoServerForBackgroundApp
        }
        InitError::LoaderUnavailable(_) | InitError::EnumeratingExtensionsFailed(_) => {
            vr::EVRInitError::Init_InstallationNotFound
        }
        // The runtime's service isn't running. SteamVR doesn't start its server for
        // background apps (Proton's helper) either, and says so with this code.
        InitError::InstanceCreationFailed(xr::sys::Result::ERROR_RUNTIME_UNAVAILABLE)
            if background =>
        {
            vr::EVRInitError::Init_NoServerForBackgroundApp
        }
        InitError::SystemCreationFailed(
            xr::sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE
            | xr::sys::Result::ERROR_FORM_FACTOR_UNSUPPORTED,
        ) => vr::EVRInitError::Init_HmdNotFound,
        // Overlay and Utility apps need a headless session.
        InitError::HeadlessUnsupported(_) => vr::EVRInitError::Init_InvalidApplicationType,
        _ => vr::EVRInitError::Init_VRServiceStartupFailed,
    }
}

impl ClientCore {
    pub fn new(version: &CStr) -> Option<Arc<Self>> {
        crate::init_logging();

        if ![c"IVRClientCore_003", c"IVRClientCore_002"].contains(&version) {
            error!("Application requested unknown ClientCore version: {version:?}");
            return None;
        }

        info!("Creating ClientCore version {version:?}");
        let ret = Arc::new(Self {
            base: OnceLock::new(),
            interface_store: Default::default(),
            openxr: RwLock::default(),
            hmd_present: Mutex::default(),
        });

        #[allow(clippy::redundant_guards)]
        let base = match version {
            x if x == c"IVRClientCore_003" => {
                Vtable::V3(<Self as Inherits<vr::IVRClientCore003>>::new_wrapped(&ret))
            }
            x if x == c"IVRClientCore_002" => {
                Vtable::V2(<Self as Inherits<vr::IVRClientCore002>>::new_wrapped(&ret))
            }
            _ => unreachable!(),
        };

        assert!(ret.base.set(base).is_ok());
        Some(ret)
    }

    fn try_interface<T, InitFn>(&self, version: &CStr, init: InitFn) -> Option<*mut c_void>
    where
        T: InterfaceImpl + 'static,
        InitFn: FnOnce(&Injector) -> T,
    {
        let get_interface = T::get_version(version)?;
        let mut store = self.interface_store.lock().unwrap();
        let item = match store.entry::<T>() {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let injector = Injector {
                    store: self.interface_store.clone(),
                };
                let item = Arc::new(init(&injector));
                entry.insert(item).clone()
            }
        }
        .downcast()
        .unwrap();
        Some(get_interface(&item))
    }
}

impl vr::IVRClientCore002On003 for ClientCore {
    fn Init(&self, app_type: vr::EVRApplicationType) -> vr::EVRInitError {
        <Self as vr::IVRClientCore003_Interface>::Init(self, app_type, std::ptr::null())
    }
}

impl IVRClientCore003_Interface for ClientCore {
    fn Init(
        &self,
        application_type: vr::EVRApplicationType,
        startup_info: *const c_char,
    ) -> vr::EVRInitError {
        let manifest_path = (!startup_info.is_null())
            .then(|| {
                let info = unsafe { CStr::from_ptr(startup_info) };

                // The startup info is undocumented, but is used in Proton:
                // https://github.com/ValveSoftware/Proton/blob/1a73b04e6cdf29297c6a79a4098ba17e2bf18872/vrclient_x64/json_converter.cpp#L244
                #[derive(Deserialize)]
                struct StartupInfo {
                    action_manifest_path: CString,
                }

                match serde_json::from_slice::<StartupInfo>(info.to_bytes()) {
                    Ok(info) => Some(info.action_manifest_path),
                    Err(e) => {
                        warn!("Failed to parse startup info: {e:?}");
                        None
                    }
                }
            })
            .flatten();

        if let Some(data) = self.openxr.read().unwrap().as_ref() {
            warn!("ClientCore::Init already called");
            if let Some(path) = manifest_path {
                data.input
                    .force(|_| Input::new(data.clone()))
                    .SetActionManifestPath(path.as_ptr());
            }

            return vr::EVRInitError::None;
        }

        if !matches!(
            application_type,
            vr::EVRApplicationType::Scene // Standard apps
            | vr::EVRApplicationType::Background // Proton
            | vr::EVRApplicationType::Other // Config utilities
            | vr::EVRApplicationType::Overlay // Overlays and tracking tools, headless
            | vr::EVRApplicationType::Utility // Headless
        ) {
            error!("Unsupported application type: {application_type:?}");
            return vr::EVRInitError::Init_InvalidApplicationType;
        }

        match OpenXrData::new(
            &Injector {
                store: self.interface_store.clone(),
            },
            application_type,
        ) {
            Ok(data) => {
                let data = Arc::new(data);
                if let Some(path) = manifest_path {
                    data.input
                        .force(|_| Input::new(data.clone()))
                        .SetActionManifestPath(path.as_ptr());
                }
                *self.openxr.write().unwrap() = Some(data);

                vr::EVRInitError::None
            }
            Err(e) => {
                error!("Creating OpenXR data failed: {e:?}");
                init_error_code(&e, application_type)
            }
        }
    }
    fn Cleanup(&self) {
        self.interface_store.lock().unwrap().clear();

        let Some(data) = self.openxr.write().unwrap().take() else {
            warn!("ClientCore::Cleanup called without a prior successful Init");
            return;
        };

        // Panicking here would abort the app, so just report it. Whatever is still holding a
        // reference keeps the OpenXR data alive until it lets go; the data gets destroyed then.
        let count = Arc::strong_count(&data);
        if count != 1 {
            error!(
                "OpenXR data still has {count} strong references during Cleanup (expected 1) - \
                 something is still using it. Releasing our reference anyway."
            );
        }
    }
    fn GetIDForVRInitError(&self, error: vr::EVRInitError) -> *const c_char {
        crate::enum_name("VRInitError_", error)
    }
    fn GetEnglishStringForHmdError(&self, error: vr::EVRInitError) -> *const c_char {
        debug!("GetEnglishStringForHmdError({error:?})");
        // SteamVR's text for the errors we return; like SteamVR, fall back to the symbol.
        let text: &CStr = match error {
            vr::EVRInitError::None => c"No Error (0)",
            vr::EVRInitError::Init_InstallationNotFound => c"Installation Not Found (100)",
            vr::EVRInitError::Init_InterfaceNotFound => c"Interface Not Found (105)",
            vr::EVRInitError::Init_HmdNotFound => c"Hmd Not Found (108)",
            vr::EVRInitError::Init_NotInitialized => c"Not Initialized (109)",
            vr::EVRInitError::Init_NoServerForBackgroundApp => {
                c"Not starting vrserver for background app (121)"
            }
            vr::EVRInitError::Init_InvalidApplicationType => c"Invalid Application Type (130)",
            _ => return self.GetIDForVRInitError(error),
        };
        text.as_ptr()
    }
    fn BIsHmdPresent(&self) -> bool {
        if self.openxr.read().unwrap().is_some() {
            return true;
        }
        // For runtimes the probe can't see yet, or to keep flat launches from starting one.
        if let Some(present) = crate::quirks::env_override("XRIZER_HMD_PRESENT") {
            return present;
        }

        // Held across the probe so concurrent callers don't each create an instance.
        let mut cached = self.hmd_present.lock().unwrap();
        if let Some((time, present)) = *cached
            && time.elapsed() < HMD_PRESENT_CACHE_TIME
        {
            return present;
        }
        let present = probe_hmd();
        *cached = Some((Instant::now(), present));
        present
    }
    fn GetGenericInterface(
        &self,
        name_and_version: *const c_char,
        error: *mut vr::EVRInitError,
    ) -> *mut c_void {
        let interface = unsafe { CStr::from_ptr(name_and_version) };
        debug!("requested interface {interface:?}");

        if !error.is_null() {
            unsafe { *error = vr::EVRInitError::None };
        }

        let openxr = self.openxr.read().unwrap();
        let openxr = openxr.as_ref().unwrap();

        self.try_interface(interface, |injector| System::new(openxr.clone(), injector))
            .or_else(|| {
                self.try_interface(interface, |injector| {
                    Compositor::new(openxr.clone(), injector)
                })
            })
            .or_else(|| self.try_interface(interface, |_| Input::new(openxr.clone())))
            .or_else(|| self.try_interface(interface, |_| RenderModels::default()))
            .or_else(|| {
                self.try_interface(interface, |injector| {
                    OverlayMan::new(openxr.clone(), injector)
                })
            })
            .or_else(|| self.try_interface(interface, |_| Chaperone::new(openxr.clone())))
            .or_else(|| self.try_interface(interface, |_| Applications::default()))
            .or_else(|| self.try_interface(interface, |_| OverlayView::default()))
            .or_else(|| self.try_interface(interface, |_| Screenshots::default()))
            .or_else(|| self.try_interface(interface, Settings::new))
            .or_else(|| self.try_interface(interface, |_| Debug::default()))
            .or_else(|| self.try_interface(interface, |_| ChaperoneSetup::new(openxr.clone())))
            .or_else(|| self.try_interface(interface, |_| ExtendedDisplay::new(openxr.clone())))
            .or_else(|| self.try_interface(interface, |_| HeadsetView::default()))
            .or_else(|| self.try_interface(interface, |_| UnknownInterfaces::default()))
            .unwrap_or_else(|| {
                warn!("app requested unknown interface {interface:?}");
                // Not found if the interface is unknown.
                // Invalid if the interface is known but the version is wrong.
                // We don't have a way to distinguish those cases yet, so just return NotFound.
                if !error.is_null() {
                    unsafe { *error = vr::EVRInitError::Init_InterfaceNotFound };
                }
                std::ptr::null_mut()
            })
    }
    fn IsInterfaceVersionValid(&self, interface_version: *const c_char) -> vr::EVRInitError {
        // Keep this in sync with GetGenericInterface above.
        static KNOWN_INTERFACES: LazyLock<Box<[&CStr]>> = LazyLock::new(|| {
            [
                System::supported_versions(),
                Compositor::supported_versions(),
                Input::<Compositor>::supported_versions(),
                RenderModels::supported_versions(),
                OverlayMan::supported_versions(),
                Chaperone::supported_versions(),
                Applications::supported_versions(),
                OverlayView::supported_versions(),
                Screenshots::supported_versions(),
                Settings::supported_versions(),
                Debug::supported_versions(),
                ChaperoneSetup::supported_versions(),
                ExtendedDisplay::supported_versions(),
                HeadsetView::supported_versions(),
                UnknownInterfaces::supported_versions(),
            ]
            .concat()
            .into_boxed_slice()
        });

        let interface = unsafe { CStr::from_ptr(interface_version) };
        debug!("app asking about interface: {interface:?}");
        if KNOWN_INTERFACES.contains(&interface) {
            vr::EVRInitError::None
        } else {
            warn!("app asked about unknown interface {interface:?}");
            // Not found if the interface is unknown.
            // Invalid if the interface is known but the version is wrong.
            // We don't have a way to distinguish those cases yet, so just return NotFound.
            vr::EVRInitError::Init_InterfaceNotFound
        }
    }
}

#[derive(Default)]
pub struct Injector {
    store: Arc<Mutex<InterfaceStore>>,
}

impl Injector {
    pub fn inject<T: InterfaceImpl>(&self) -> Injected<T> {
        Injected::<T> {
            item: OnceLock::new(),
            store: self.store.clone(),
            _marker: Default::default(),
        }
    }
}

pub struct Injected<T> {
    item: OnceLock<Weak<ErasedInterface>>,
    store: Arc<Mutex<InterfaceStore>>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: InterfaceImpl> Injected<T> {
    #[cfg(test)]
    pub fn set(&self, item: Weak<T>) {
        self.item.set(item).unwrap();
    }
    pub fn get(&self) -> Option<Arc<T>> {
        self.item
            .get()
            .or_else(|| {
                let item: Arc<ErasedInterface> = self.store.lock().unwrap().get::<T>()?;
                // Another thread may have cached the same item in the meantime.
                let _ = self.item.set(Arc::downgrade(&item));
                self.item.get()
            })
            .and_then(|i| Some(i.upgrade()?.downcast().unwrap()))
    }

    pub fn force(&self, init: impl FnOnce(&Injector) -> T) -> Arc<T> {
        self.item
            .get_or_init(|| {
                let injector = Injector {
                    store: self.store.clone(),
                };
                // Item may get forced by another thread while we're in the process of forcing it,
                // so try getting it before inserting one.
                Arc::downgrade(
                    self.store
                        .lock()
                        .unwrap()
                        .entry::<T>()
                        .or_insert_with(|| Arc::new(init(&injector))),
                )
            })
            .upgrade()
            .unwrap()
            .downcast()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ClientCore {
        fn get_interface<T: InterfaceImpl + 'static>(&self) -> Option<Arc<T>> {
            self.interface_store.lock().unwrap().get::<T>()
        }
    }

    macro_rules! interface {
        ($interface:ident$(($($fields:tt)*))?, $version:literal) => {
            #[derive(Debug)]
            struct $interface$(($($fields)*))?;
            impl InterfaceImpl for $interface {
                fn get_version(
                    version: &CStr,
                ) -> Option<Box<dyn FnOnce(&Arc<Self>) -> *mut c_void>> {
                    if version == $version {
                        Some(Box::new(|this| Arc::as_ptr(this) as _))
                    } else {
                        None
                    }
                }
                fn supported_versions() -> &'static [&'static CStr] {
                    &[]
                }
            }
        };
    }
    interface!(Interface1, c"one");
    interface!(Interface2(Injected<Interface1>), c"two");
    interface!(Interface3(Injected<Interface1>), c"three");
    interface!(Interface4(Injected<Interface1>), c"four");

    impl<T: std::fmt::Debug + InterfaceImpl> std::fmt::Debug for Injected<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_fmt(format_args!(
                "Injected<{}>({:?})",
                std::any::type_name::<T>(),
                self.item.get()
            ))
        }
    }

    #[test]
    fn unknown_enum_values() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType(1234), std::ptr::null()),
            vr::EVRInitError::Init_InvalidApplicationType
        );

        let error = |e| unsafe { CStr::from_ptr(core.GetEnglishStringForHmdError(e)) };
        assert_eq!(error(vr::EVRInitError(12345)), c"Unknown");
        assert_eq!(
            error(vr::EVRInitError::Init_HmdNotFound),
            c"Hmd Not Found (108)"
        );
        let symbol = |e| unsafe { CStr::from_ptr(core.GetIDForVRInitError(e)) };
        assert_eq!(symbol(vr::EVRInitError(12345)), c"Unknown");
    }

    #[test]
    fn restart() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        core.clone()
            .Init(vr::EVRApplicationType::Scene, std::ptr::null());
        core.clone().Cleanup();
        core.clone()
            .Init(vr::EVRApplicationType::Scene, std::ptr::null());
    }

    #[test]
    fn cleanup_with_outstanding_openxr_references() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::Scene, std::ptr::null()),
            vr::EVRInitError::None
        );

        // Something other than ClientCore is still holding on to the OpenXR data.
        let outstanding = core.openxr.read().unwrap().as_ref().unwrap().clone();
        assert_eq!(Arc::strong_count(&outstanding), 2);

        core.clone().Cleanup();

        // We must have let go of our reference, but not destroyed data that is still in use.
        assert!(core.openxr.read().unwrap().is_none());
        assert_eq!(Arc::strong_count(&outstanding), 1);

        // And the app must still be able to initialize again.
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::Scene, std::ptr::null()),
            vr::EVRInitError::None
        );
        let new = core.openxr.read().unwrap().as_ref().unwrap().clone();
        assert!(!Arc::ptr_eq(&new, &outstanding));
    }

    #[test]
    fn init_error_strings() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        let string = |ptr| unsafe { CStr::from_ptr(ptr) };

        let symbol = core.GetIDForVRInitError(vr::EVRInitError::Init_InterfaceNotFound);
        assert_eq!(string(symbol), c"VRInitError_Init_InterfaceNotFound");
        // The pointer has to stay valid, so it's the same one every time.
        assert_eq!(
            core.GetIDForVRInitError(vr::EVRInitError::Init_InterfaceNotFound),
            symbol
        );
        assert_eq!(
            string(core.GetIDForVRInitError(vr::EVRInitError::None)),
            c"VRInitError_None"
        );

        assert_eq!(
            string(core.GetEnglishStringForHmdError(vr::EVRInitError::Init_InterfaceNotFound)),
            c"Interface Not Found (105)"
        );
        // SteamVR has no text for this one and returns the symbol.
        assert_eq!(
            string(core.GetEnglishStringForHmdError(vr::EVRInitError::Init_VRServiceStartupFailed)),
            c"VRInitError_Init_VRServiceStartupFailed"
        );
    }

    #[test]
    fn init_error_codes() {
        use vr::EVRApplicationType as App;
        use vr::EVRInitError as E;
        use xr::sys::Result as XrResult;

        let cases = [
            (
                InitError::LoaderUnavailable("libopenxr_loader.so.1: not found".into()),
                App::Scene,
                E::Init_InstallationNotFound,
            ),
            (
                InitError::LoaderUnavailable("libopenxr_loader.so.1: not found".into()),
                App::Background,
                E::Init_NoServerForBackgroundApp,
            ),
            (
                InitError::EnumeratingExtensionsFailed(XrResult::ERROR_RUNTIME_UNAVAILABLE),
                App::Scene,
                E::Init_InstallationNotFound,
            ),
            (
                InitError::EnumeratingExtensionsFailed(XrResult::ERROR_RUNTIME_UNAVAILABLE),
                App::Background,
                E::Init_NoServerForBackgroundApp,
            ),
            (
                InitError::InstanceCreationFailed(XrResult::ERROR_RUNTIME_UNAVAILABLE),
                App::Background,
                E::Init_NoServerForBackgroundApp,
            ),
            (
                InitError::InstanceCreationFailed(XrResult::ERROR_RUNTIME_UNAVAILABLE),
                App::Scene,
                E::Init_VRServiceStartupFailed,
            ),
            (
                InitError::InstanceCreationFailed(XrResult::ERROR_RUNTIME_FAILURE),
                App::Background,
                E::Init_VRServiceStartupFailed,
            ),
            (
                InitError::SystemCreationFailed(XrResult::ERROR_FORM_FACTOR_UNAVAILABLE),
                App::Scene,
                E::Init_HmdNotFound,
            ),
            (
                InitError::SystemCreationFailed(XrResult::ERROR_RUNTIME_FAILURE),
                App::Scene,
                E::Init_VRServiceStartupFailed,
            ),
            (
                InitError::HeadlessUnsupported(App::Overlay),
                App::Overlay,
                E::Init_InvalidApplicationType,
            ),
        ];
        for (error, app_type, expected) in cases {
            assert_eq!(init_error_code(&error, app_type), expected, "{error:?}");
        }
    }

    #[test]
    fn no_headset() {
        fakexr::set_get_system_result(xr::sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE);
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert!(!core.BIsHmdPresent());
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::Scene, std::ptr::null()),
            vr::EVRInitError::Init_HmdNotFound
        );

        fakexr::set_get_system_result(xr::sys::Result::SUCCESS);
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert!(core.BIsHmdPresent());
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::Scene, std::ptr::null()),
            vr::EVRInitError::None
        );
        // Answered from the live instance, not a new probe.
        fakexr::set_get_system_result(xr::sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE);
        assert!(core.BIsHmdPresent());
        fakexr::set_get_system_result(xr::sys::Result::SUCCESS);
    }

    #[test]
    fn hmd_present_while_an_instance_exists() {
        // What the loader says while Init or a leftover session has an instance.
        fakexr::set_create_instance_result(xr::sys::Result::ERROR_LIMIT_REACHED);
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert!(core.BIsHmdPresent());
        fakexr::set_create_instance_result(xr::sys::Result::SUCCESS);
    }

    #[test]
    fn serves_0_9_14_and_1_0_15_interfaces() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::Scene, std::ptr::null()),
            vr::EVRInitError::None
        );
        for version in [
            c"IVRSystem_010",
            c"IVRCompositor_010",
            c"IVROverlay_008",
            c"IVRInput_003",
        ] {
            assert_eq!(
                core.IsInterfaceVersionValid(version.as_ptr()),
                vr::EVRInitError::None,
                "{version:?}"
            );
            let mut error = vr::EVRInitError::Unknown;
            assert!(
                !core
                    .GetGenericInterface(version.as_ptr(), &mut error)
                    .is_null()
            );
            assert_eq!(error, vr::EVRInitError::None, "{version:?}");
        }
    }

    #[test]
    fn overlay_and_utility_apps_are_headless() {
        for app_type in [
            vr::EVRApplicationType::Overlay,
            vr::EVRApplicationType::Utility,
        ] {
            let core = ClientCore::new(c"IVRClientCore_003").unwrap();
            assert_eq!(
                core.clone().Init(app_type, std::ptr::null()),
                vr::EVRInitError::Init_InvalidApplicationType,
                "{app_type:?} without headless support"
            );
            assert!(core.openxr.read().unwrap().is_none());

            fakexr::set_headless_supported(true);
            assert_eq!(
                core.clone().Init(app_type, std::ptr::null()),
                vr::EVRInitError::None,
                "{app_type:?}"
            );
            fakexr::set_headless_supported(false);
            assert!(core.openxr.read().unwrap().as_ref().unwrap().headless);
            core.clone().Cleanup();
        }
    }

    /// A second Init (e.g. from an overlay mod inside a game) must not fail the first one.
    #[test]
    fn init_again_with_unsupported_type() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::Scene, std::ptr::null()),
            vr::EVRInitError::None
        );
        assert_eq!(
            core.clone()
                .Init(vr::EVRApplicationType::VRMonitor, std::ptr::null()),
            vr::EVRInitError::None
        );
    }

    #[test]
    fn cleanup_without_init() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();
        core.clone().Cleanup();
        assert!(core.openxr.read().unwrap().is_none());
    }

    #[test]
    fn inject() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();

        core.try_interface(c"two", |injector| Interface2(injector.inject()));
        let interface2_before = core
            .get_interface::<Interface2>()
            .expect("Interface2 missing from store");

        assert!(
            interface2_before.0.get().is_none(),
            "Interface1 should not have been injected yet"
        );

        core.try_interface(c"one", |_| Interface1);
        let interface1 = core
            .get_interface::<Interface1>()
            .expect("Interface1 missing from store");

        let interface2 = core
            .get_interface::<Interface2>()
            .expect("Interface2 missing from store");
        assert_eq!(
            Arc::as_ptr(&interface2),
            Arc::as_ptr(&interface2_before),
            "Interface2 changed in store"
        );

        let injected = interface2.0.get().expect("Interface1 not injected");
        assert_eq!(Arc::as_ptr(&injected), Arc::as_ptr(&interface1),);
    }

    #[test]
    fn inject_force() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();

        core.try_interface(c"two", |injector| Interface2(injector.inject()));
        let interface2 = core
            .get_interface::<Interface2>()
            .expect("Interface2 missing from store");
        assert!(
            interface2.0.get().is_none(),
            "Interface1 should not have been injected yet"
        );
        core.try_interface(c"three", |injector| Interface3(injector.inject()));
        let interface3 = core
            .get_interface::<Interface3>()
            .expect("Interface3 missing from store");
        assert!(
            interface3.0.get().is_none(),
            "Interface1 should not have been injected yet"
        );

        interface2.0.force(|_| Interface1);
        let interface1 = core
            .get_interface::<Interface1>()
            .expect("Interface1 not injected");
        let injected2 = interface2
            .0
            .get()
            .expect("Interface1 not injected into Interface2");
        let injected3 = interface3
            .0
            .get()
            .expect("Interface1 not injected into Interface3");
        assert_eq!(Arc::as_ptr(&injected2), Arc::as_ptr(&interface1));
        assert_eq!(Arc::as_ptr(&injected3), Arc::as_ptr(&interface1));

        core.try_interface(c"four", |injector| Interface4(injector.inject()));
        let interface4 = core
            .get_interface::<Interface4>()
            .expect("Interface4 missing from store");
        let injected4 = interface4
            .0
            .get()
            .expect("Interface1 not injected into Interface4");
        assert_eq!(Arc::as_ptr(&injected4), Arc::as_ptr(&interface1));
    }

    #[test]
    fn inject_multithread_force() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();

        core.try_interface(c"two", |injector| Interface2(injector.inject()));
        let interface2 = core
            .get_interface::<Interface2>()
            .expect("Interface2 missing from store");

        core.try_interface(c"three", |injector| Interface3(injector.inject()));
        let interface3 = core
            .get_interface::<Interface3>()
            .expect("Interface3 missing from store");

        let (injected2, injected3) = std::thread::scope(|s| {
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let b2 = barrier.clone();
            let i2 = &interface2.0;
            let ij2 = s.spawn(move || {
                b2.wait();
                i2.force(|_| Interface1)
            });
            let i3 = &interface3.0;
            let ij3 = s.spawn(move || {
                barrier.wait();
                i3.force(|_| Interface1)
            });
            (ij2.join().unwrap(), ij3.join().unwrap())
        });

        core.try_interface(c"one", |_| Interface1);
        let interface1 = core
            .get_interface::<Interface1>()
            .expect("Interface1 missing from store");

        assert_eq!(Arc::as_ptr(&injected2), Arc::as_ptr(&interface1));
        assert_eq!(Arc::as_ptr(&injected3), Arc::as_ptr(&interface1));
    }

    #[test]
    fn inject_get_while_forcing() {
        let core = ClientCore::new(c"IVRClientCore_003").unwrap();

        core.try_interface(c"two", |injector| Interface2(injector.inject()));
        let interface2 = core
            .get_interface::<Interface2>()
            .expect("Interface2 missing from store");

        let barrier = std::sync::Barrier::new(2);
        let (forced, got) = std::thread::scope(|s| {
            let injected = &interface2.0;
            let forced = s.spawn(|| {
                injected.force(|_| {
                    barrier.wait();
                    // Let the other thread find the item unset and wait on the store.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    Interface1
                })
            });
            barrier.wait();
            let got = injected.get();
            (forced.join().unwrap(), got)
        });

        let got = got.expect("Interface1 not injected");
        assert_eq!(Arc::as_ptr(&got), Arc::as_ptr(&forced));
    }
}
