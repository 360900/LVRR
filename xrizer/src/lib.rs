#![deny(clippy::all)]

mod applications;
mod chaperone;
mod chaperone_setup;
mod clientcore;
mod compositor;
mod debug;
mod extended_display;
mod graphics_backends;
mod headset_view;
mod input;
mod misc_unknown;
mod openxr_data;
mod overlay;
mod overlayview;
mod quirks;
mod quit_watchdog;
mod rendermodels;
mod screenshots;
mod settings;
mod stubs;
mod system;

#[cfg(not(test))]
mod error_dialog;

#[cfg(feature = "monado")]
mod monado;

use clientcore::ClientCore;
use openvr as vr;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{
    Arc, LazyLock, Mutex, OnceLock,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

macro_rules! warn_unimplemented {
    ($function:literal) => {
        crate::warn_once!("{} unimplemented ({}:{})", $function, file!(), line!());
    };
}
use warn_unimplemented;
macro_rules! warn_once {
    ($literal:literal $(,$($tt:tt)*)?) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::warn!(concat!("[ONCE] ", $literal) $(,$($tt)*)?);
        });
    }}
}
use warn_once;

/// Logs at `level` the first time this exact message comes up, and at debug level after that,
/// for messages about data that is loaded again, e.g. on every session restart.
macro_rules! log_unique {
    ($level:expr, $($arg:tt)+) => {{
        let message = format!($($arg)+);
        let level = if crate::first_occurrence(&message) {
            $level
        } else {
            log::Level::Debug
        };
        log::log!(level, "{message}");
    }};
}
use log_unique;

/// Whether `message` comes up for the first time in this process.
fn first_occurrence(message: &str) -> bool {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);
    SEEN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_default()
        .insert(message.to_owned())
}

#[cfg(feature = "tracing")]
macro_rules! tracy_span {
    ($($tt:tt)*) => {
        let _span = tracy_client::span!($($tt)*);
    }
}

#[cfg(not(feature = "tracing"))]
macro_rules! tracy_span {
    ($($tt:tt)*) => {};
}
use tracy_span;

#[cfg(feature = "tracing")]
tracy_client::register_demangler!();

macro_rules! atomic_float {
    ($name:ident, $float:ty, $atomic:ty) => {
        #[derive(Default)]
        struct $name($atomic);

        impl $name {
            fn new(value: $float) -> Self {
                Self(value.to_bits().into())
            }

            #[allow(dead_code)]
            #[inline]
            fn load(&self) -> $float {
                <$float>::from_bits(self.0.load(Ordering::Relaxed))
            }

            #[allow(dead_code)]
            #[inline]
            fn store(&self, value: $float) {
                self.0.store(value.to_bits(), Ordering::Relaxed)
            }

            #[allow(dead_code)]
            #[inline]
            fn swap(&self, value: $float) -> $float {
                <$float>::from_bits(self.0.swap(value.to_bits(), Ordering::Relaxed))
            }
        }

        impl From<$float> for $name {
            fn from(value: $float) -> Self {
                Self::new(value)
            }
        }
    };
}

atomic_float!(AtomicF32, f32, AtomicU32);
atomic_float!(AtomicF64, f64, AtomicU64);

/// The header name of an OpenVR enum value, for the `*NameFromEnum` functions: the bindgen
/// variant names are the header names minus `prefix`. The returned string is never freed.
fn enum_name(prefix: &str, value: impl std::fmt::Debug) -> *const c_char {
    static NAMES: LazyLock<Mutex<HashMap<String, Box<CStr>>>> = LazyLock::new(Default::default);

    let name = format!("{value:?}");
    // A value the headers don't name prints as its number. Apps can pass any of those, so
    // don't keep a string for each.
    if name.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
        return c"Unknown".as_ptr();
    }
    let name = format!("{prefix}{name}");
    NAMES
        .lock()
        .unwrap()
        .entry(name)
        // Boxed, so the pointer stays valid when the map grows.
        .or_insert_with_key(|name| CString::new(name.as_str()).unwrap().into())
        .as_ptr()
}

/// Per-process log files older than this get removed on startup.
#[cfg(not(test))]
const LOG_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(48 * 60 * 60);

/// The directory xrizer writes its logs to, if a state directory is known.
#[cfg(not(test))]
fn log_dir() -> Option<std::path::PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("xrizer"))
}

/// Log file name for this process. The timestamp makes logs sort
/// chronologically (pids are reused across reboots) and the pid keeps
/// concurrently running processes out of each other's file.
#[cfg(not(test))]
fn log_file_name() -> String {
    use time::macros::format_description;

    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let timestamp = now
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]"
        ))
        .unwrap_or_default();

    format!("xrizer-{timestamp}-{}.txt", std::process::id())
}

/// The log file this process is writing to, once logging has been set up.
#[cfg(not(test))]
pub(crate) fn log_file_path() -> Option<&'static std::path::Path> {
    LOG_FILE.get().map(std::path::PathBuf::as_path)
}

#[cfg(not(test))]
static LOG_FILE: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Remove old per-process log files so the log directory doesn't accumulate
/// one file per xrizer process forever. Files newer than the cutoff are kept,
/// as they may belong to a concurrently running process.
fn prune_old_logs(dir: &std::path::Path, keep_newer_than: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("xrizer-") || !name.ends_with(".txt") {
            continue;
        }
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < keep_newer_than);
        if too_old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn init_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| {
        let mut builder = env_logger::Builder::new();
        #[allow(unused_mut)]
        let mut startup_err: Option<String> = None;

        builder.filter_level(log::LevelFilter::Info);
        builder.parse_default_env();

        #[cfg(not(test))]
        {
            struct ComboWriter(std::fs::File, std::io::Stderr);

            impl std::io::Write for ComboWriter {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    let _ = self.0.write(buf)?;
                    self.1.write(buf)
                }

                fn flush(&mut self) -> std::io::Result<()> {
                    self.0.flush()?;
                    self.1.flush()
                }
            }

            let dir = log_dir();

            if let Some(dir) = &dir {
                let mut setup = || {
                    // Multiple processes (e.g. a launcher's VR probe and the game itself)
                    // can run concurrently; a shared truncating log file lets one clobber
                    // the other's output, so give each process its own file.
                    let path = dir.join(log_file_name());
                    match std::fs::File::create(&path) {
                        Ok(file) => {
                            let _ = LOG_FILE.set(path);
                            let writer = ComboWriter(file, std::io::stderr());
                            builder.target(env_logger::Target::Pipe(Box::new(writer)));
                        }
                        Err(e) => startup_err = Some(format!("Failed to create log file: {e:?}")),
                    }
                };

                match std::fs::create_dir_all(dir) {
                    Ok(_) => {
                        prune_old_logs(dir, std::time::SystemTime::now() - LOG_MAX_AGE);
                        setup()
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => setup(),
                    err => {
                        startup_err =
                            Some(format!("Failed to create log directory ({dir:?}): {err:?}"))
                    }
                }
            }

            // Some launch setups don't deliver RUST_LOG to the game process
            // (e.g. launchers that sanitize the environment), so fall back to
            // a filter file next to the logs.
            if std::env::var_os("RUST_LOG").is_none()
                && let Some(dir) = &dir
                && let Ok(filter) = std::fs::read_to_string(dir.join("log_filter"))
                && !filter.trim().is_empty()
            {
                builder.parse_filters(filter.trim());
            }

            std::panic::set_hook(Box::new(|info| {
                log::error!("{info}");
                let backtrace = std::backtrace::Backtrace::force_capture();
                log::error!("Backtrace: \n{backtrace}");
                error_dialog::dialog(format!("{info}"), backtrace);
                std::process::abort();
            }));
        }

        builder
            .is_test(cfg!(test))
            .format(|buf, record| {
                use std::io::Write;
                use time::macros::format_description;

                let style = buf.default_level_style(record.level());
                let now = time::OffsetDateTime::now_local()
                    .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
                let now = now
                    .format(format_description!(
                        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]"
                    ))
                    .unwrap();

                write!(buf, "[{now} {style}{:5}{style:#}", record.level())?;
                if let Some(path) = record.module_path() {
                    write!(buf, " {path}")?;
                }
                writeln!(buf, " {:?}] {}", std::thread::current().id(), record.args())
            })
            .init();

        let version = reported_version(
            option_env!("XRIZER_VERSION"),
            option_env!("VERGEN_GIT_DESCRIBE"),
        );
        log::info!("Initializing XRizer version {version}");
        if let Some(err) = startup_err {
            log::warn!("{err}");
        }
    });
}

/// Empty values count as unset, like in build.rs: rustc inherits XRIZER_VERSION
/// from cargo's environment even when build.rs ignores it.
fn reported_version(
    xrizer_version: Option<&'static str>,
    git_describe: Option<&'static str>,
) -> &'static str {
    xrizer_version
        .filter(|v| !v.is_empty())
        .or_else(|| git_describe.filter(|v| !v.is_empty() && *v != "VERGEN_IDEMPOTENT_OUTPUT"))
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// # Safety
///
/// interface_name must be valid
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn VRClientCoreFactory(
    interface_name: *const c_char,
    return_code: *mut i32,
) -> *mut c_void {
    let interface = unsafe { CStr::from_ptr(interface_name) };

    struct ClientCorePtr(*mut c_void);
    // SAFETY: Vtables are fine to send across threads.
    unsafe impl Send for ClientCorePtr {}
    unsafe impl Sync for ClientCorePtr {}

    static C: OnceLock<ClientCorePtr> = OnceLock::new();
    if C.get().is_none() {
        let ret = ClientCore::new(interface).map(|c| {
            if let Some(ret) = unsafe { return_code.as_mut() } {
                *ret = 0;
            }
            let vtable = match c.base.get().unwrap() {
                clientcore::Vtable::V2(v) => v as *const _ as *const vr::IVRClientCore002 as _,
                clientcore::Vtable::V3(v) => v as *const _ as *const vr::IVRClientCore003 as _,
            };
            // Leak it!
            let _ = Arc::into_raw(c);
            vtable
        });

        // A racing call may have set it first; ours is leaked like any other.
        if let Some(c) = ret {
            let _ = C.set(ClientCorePtr(c));
        }
    }

    C.get().map(|c| c.0).unwrap_or(std::ptr::null_mut())
}

/// Needed for Proton, but seems unused.
///
/// # Safety
///
/// `return_code` must be a valid pointer or null.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn HmdSystemFactory(
    _interface_name: *const c_char,
    return_code: *mut i32,
) -> *mut c_void {
    crate::warn_unimplemented!("HmdSystemFactory");
    if let Some(ret) = unsafe { return_code.as_mut() } {
        *ret = 1;
    }
    std::ptr::null_mut()
}

#[cfg(test)]
mod log_prune_tests {
    use super::prune_old_logs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn prune_only_removes_old_xrizer_logs() {
        let dir = std::env::temp_dir().join(format!("xrizer-prune-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("xrizer-20260101T120000-123.txt"), "a").unwrap();
        // logs from before the timestamped naming should be cleaned up too
        std::fs::write(dir.join("xrizer-456.txt"), "b").unwrap();
        std::fs::write(dir.join("unrelated.txt"), "c").unwrap();

        // cutoff in the past: nothing is old enough to remove
        prune_old_logs(&dir, SystemTime::now() - Duration::from_secs(60));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);

        // cutoff in the future: all xrizer logs count as old
        prune_old_logs(&dir, SystemTime::now() + Duration::from_secs(60));
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(remaining, vec!["unrelated.txt".to_string()]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod version_tests {
    use super::reported_version;

    #[test]
    fn empty_version_env_falls_through() {
        const PKG: &str = env!("CARGO_PKG_VERSION");
        assert_eq!(reported_version(Some("v2"), Some("v1-1-g0")), "v2");
        assert_eq!(reported_version(Some(""), Some("v1-1-g0")), "v1-1-g0");
        assert_eq!(reported_version(Some(""), None), PKG);
        assert_eq!(reported_version(None, Some("")), PKG);
        assert_eq!(
            reported_version(None, Some("VERGEN_IDEMPOTENT_OUTPUT")),
            PKG
        );
    }
}
