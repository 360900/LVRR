#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(
    clippy::cargo_common_metadata,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::match_wildcard_for_single_variants,
    clippy::multiple_crate_versions,
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::struct_excessive_bools,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::use_self
)]
mod app_misc;
mod backend;
mod config;
mod graphics;
mod gui;
mod ipc;
mod lvrr;
mod overlays;
mod shaders;
mod state;
mod subsystem;
mod test;
mod windowing;

use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use clap::Parser;
use libc::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;
use sysinfo::Pid;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use wgui::{font_config::WguiFontConfig, globals::WguiGlobals};

use crate::{config::load_general_config, subsystem::dbus::DbusConnector};
use wlx_common::{
    XrBackend,
    config::GeneralConfig,
    config_io::{self, get_config_file_path},
    locale::WayVRLangProvider,
    palette::load_palette,
};

pub static FRAME_COUNTER: AtomicUsize = AtomicUsize::new(0);
pub static RUNNING: AtomicBool = AtomicBool::new(true);
pub static RESTART: AtomicBool = AtomicBool::new(false);
pub static KEYMAP_CHANGE: AtomicBool = AtomicBool::new(false);

/// The lightweight desktop overlay for OpenVR and OpenXR
#[derive(Default, Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[cfg(feature = "openvr")]
    /// Force OpenVR backend
    #[arg(long)]
    openvr: bool,

    #[cfg(feature = "openxr")]
    /// Force OpenXR backend
    #[arg(long)]
    openxr: bool,

    /// Show the working set of overlay on startup. Also skips tutorial.
    #[arg(long)]
    show: bool,

    /// Wait for a runtime to be available instead of exiting
    #[arg(long)]
    wait: bool,

    /// Uninstall SteamVR manifest and exit
    #[arg(long)]
    uninstall: bool,

    /// Install SteamVR manifest (not recommended; LVRR may have issues when auto-started by SteamVR!)
    #[arg(long)]
    install: bool,

    /// Replace running LVRR desktop instance
    #[arg(long)]
    replace: bool,

    /// Allow multiple running instances of the LVRR desktop (things may break!)
    #[arg(long)]
    multi: bool,

    /// Disable desktop access altogether.
    #[arg(long)]
    headless: bool,

    /// Do not auto-start registered apps
    #[arg(long)]
    no_autostart: bool,

    /// Path to write logs to
    #[arg(short, long, value_name = "FILE_PATH")]
    log_to: Option<String>,

    /// Disable audio output
    #[arg(long)]
    no_audio: bool,
}

fn load_config() -> GeneralConfig {
    let config_root_path = config_io::ConfigRoot::Generic.ensure_dir();
    log::info!("Config root path: {}", config_root_path.display());
    load_general_config()
}

fn init_run_params(no_audio: bool) -> backend::RunParams {
    let assets = Box::new(gui::asset::GuiAsset {});
    let config = load_config();
    let lang_provider = WayVRLangProvider::from_config(&config);
    let theme_path = config.theme_path.clone();

    let wgui_globals = WguiGlobals::new(
        assets,
        &lang_provider,
        &WguiFontConfig::default(),
        get_config_file_path(&theme_path),
        load_palette(&config.color_palette),
    )
    .expect("Failed to init Globals, can't proceed");
    // LVRR: no dim behind menus, its square corners show past the rounded panels
    wgui_globals.get().popup_backdrop_dim = false;

    backend::RunParams {
        wgui_globals,
        config,
        no_audio,
    }
}

#[allow(clippy::unnecessary_wraps)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = if std::env::args().skip(1).any(|a| !a.is_empty()) {
        Args::parse()
    } else {
        Args::default()
    };

    if !args.multi && !ensure_single_instance(args.replace) {
        // LVRR: a --replace that gave up printed why
        if !args.replace {
            println!("Looks like the LVRR desktop is already running.");
            println!("Use --replace and I will terminate it for you.");
        }
        return Ok(());
    }

    logging_init(&mut args);

    log::info!(
        "Welcome to {} version {}!{}",
        env!("CARGO_PKG_NAME"),
        env!("WLX_BUILD"),
        if std::env::var("APPDIR").is_ok() {
            " (AppImage)"
        } else {
            ""
        }
    );
    log::info!("It is {}.", chrono::Local::now().format("%c"));

    #[cfg(feature = "openvr")]
    if args.uninstall {
        crate::backend::openvr::openvr_uninstall();
        return Ok(());
    }

    setup_signal_hooks()?;

    let mut used_backend = None;
    auto_run(args, &mut used_backend);

    if RESTART.load(Ordering::Relaxed) {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        log::warn!("Restarting...");
        let mut exe = std::env::current_exe()?;
        // LVRR: once an update replaced the binary, /proc/self/exe ends in " (deleted)"
        if let Some(path) = exe.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
            exe = PathBuf::from(OsStr::from_bytes(path));
        }
        let mut args = vec!["--replace", "--show"];

        match used_backend {
            Some(XrBackend::OpenXR) => args.push("--openxr"),
            Some(XrBackend::OpenVR) => args.push("--openvr"),
            _ => {}
        }

        // LVRR: in place, so the systemd unit the server started this in keeps running
        let err = std::os::unix::process::CommandExt::exec(Command::new(exe).args(args));
        log::error!("Could not restart: {err}");
    }

    Ok(())
}

#[allow(unused_mut, clippy::similar_names)]
fn auto_run(args: Args, used_backend: &mut Option<XrBackend>) {
    let mut tried_xr = false;
    let mut tried_vr = false;

    loop {
        #[cfg(feature = "openxr")]
        if !args_get_openvr(&args) {
            use crate::backend::{BackendError, openxr::openxr_run};
            tried_xr = true;
            match openxr_run(&args, init_run_params(args.no_audio)) {
                Ok(()) => {
                    used_backend.replace(XrBackend::OpenXR);
                    return;
                }
                Err(BackendError::NotSupported) => (),
                Err(e) => {
                    used_backend.replace(XrBackend::OpenXR);
                    log::error!("{e:?}");
                    return;
                }
            }
        }

        #[cfg(feature = "openvr")]
        if !args_get_openxr(&args) {
            use crate::backend::{BackendError, openvr::openvr_run};
            tried_vr = true;
            match openvr_run(&args, init_run_params(args.no_audio)) {
                Ok(()) => {
                    used_backend.replace(XrBackend::OpenVR);
                    return;
                }
                Err(BackendError::NotSupported) => (),
                Err(e) => {
                    used_backend.replace(XrBackend::OpenVR);
                    log::error!("{e:?}");
                    return;
                }
            }
        }

        if args.wait {
            // wait 5s unless we get signaled
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if !RUNNING.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else {
            break;
        }
    }

    log::error!("No more backends to try");

    let instructions_key = match (tried_xr, tried_vr) {
        // LVRR: LVRR's server becomes the runtime when a headset connects
        (true, _) | (_, true) => "LVRR.NOT_RUNNING",
        _ => "NOTIFICATION.INVALID_ARGS",
    };

    let run_params = init_run_params(args.no_audio);

    let instructions_str = run_params.wgui_globals.i18n().translate(instructions_key);

    let instructions_title = run_params
        .wgui_globals
        .i18n()
        .translate("NOTIFICATION.COUD_NOT_CONNECT_TO_VR_RUNTIME");

    let instructions = format!("{instructions_title}\n{instructions_str}");

    let _ = DbusConnector::notify_send("LVRR", &instructions, 1, 30000, 0, false);

    #[cfg(not(any(feature = "openvr", feature = "openxr")))]
    compile_error!("No VR support! Enable either openvr or openxr features!");
}

#[allow(dead_code, unused_variables)]
const fn args_get_openvr(args: &Args) -> bool {
    #[cfg(feature = "openvr")]
    let ret = args.openvr;

    #[cfg(not(feature = "openvr"))]
    let ret = false;

    ret
}

#[allow(dead_code, unused_variables)]
const fn args_get_openxr(args: &Args) -> bool {
    #[cfg(feature = "openxr")]
    let ret = args.openxr;

    #[cfg(not(feature = "openxr"))]
    let ret = false;

    ret
}

#[allow(clippy::single_match_else)]
fn setup_signal_hooks() -> anyhow::Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGUSR1])?;

    std::thread::spawn(move || {
        for signal in signals.forever() {
            match signal {
                SIGUSR1 => {
                    log::info!("SIGUSR1 received (keymap changed)");
                    KEYMAP_CHANGE.store(true, Ordering::Relaxed);
                }
                _ => {
                    RUNNING.store(false, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
    Ok(())
}

fn logging_init(args: &mut Args) {
    let log_file_path = args
        .log_to
        .take()
        .or_else(|| std::env::var("WAYVR_LOGFILE").ok())
        .unwrap_or_else(|| String::from("/tmp/wayvr.log"));

    let file_writer = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_file_path)
    {
        Ok(file) => {
            println!("Logging to {log_file_path}");
            Some(file)
        }
        Err(e) => {
            println!("Failed to open log file (path: {e:?}): {log_file_path}");
            None
        }
    };

    let registry = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .pretty()
                .with_writer(std::io::stderr),
        )
        .with(
            /* read RUST_LOG env var */
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy()
                .add_directive("symphonia_core::probe=warn".parse().unwrap())
                .add_directive("symphonia_bundle_mp3=warn".parse().unwrap())
                .add_directive("zbus=warn".parse().unwrap())
                .add_directive("usvg=error".parse().unwrap())
                .add_directive("resvg=error".parse().unwrap())
                .add_directive("cosmic_text=warn".parse().unwrap())
                .add_directive("wlx_capture::wayland=info".parse().unwrap())
                .add_directive("smithay=debug".parse().unwrap()), /* GLES render spam */
        );

    if let Some(writer) = file_writer {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_file(true)
                    .with_line_number(true)
                    .with_writer(writer)
                    .with_ansi(false),
            )
            .init();
    } else {
        registry.init();
    }

    log_panics::init();
}

/// LVRR: a frozen instance ignores SIGTERM, so it gets SIGKILL after a while
fn lvrr_wait_or_kill(system: &mut sysinfo::System, pid: Pid) {
    let alive = |system: &mut sysinfo::System| {
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        system
            .process(pid)
            .is_some_and(|p| p.status() != sysinfo::ProcessStatus::Zombie)
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(system) {
        if Instant::now() > deadline {
            println!("The previous instance didn't quit, killing it.");
            if let Some(proc) = system.process(pid) {
                proc.kill();
                proc.wait();
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// LVRR: the process if it's a wayvr other than this one. A pid file left by a crash can name
/// another program by now, so the process name has to match ours. Not the binary's path: an
/// update replaces the binary under a running instance.
fn lvrr_find_wayvr(system: &mut sysinfo::System, pid: u32) -> Option<&sysinfo::Process> {
    let (pid, own) = (Pid::from_u32(pid), Pid::from_u32(std::process::id()));
    // a restart in place keeps the pid
    if pid == own {
        return None;
    }
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid, own]), true);
    let name = system
        .process(own)
        .map_or_else(|| std::ffi::OsStr::new("wayvr"), sysinfo::Process::name);
    system
        .process(pid)
        .filter(|p| p.name() == name && p.status() != sysinfo::ProcessStatus::Zombie)
}

/// LVRR: the pid in the file
fn lvrr_read_pid(mut file: &std::fs::File) -> Option<u32> {
    use std::io::Seek;
    file.rewind().ok()?;
    std::io::read_to_string(file).ok()?.trim().parse().ok()
}

/// LVRR: --replace waits for the instance holding the lock to quit: SIGTERM, and SIGKILL 3 s later
/// for a frozen one. False when it's still there 3 s after that, or when another --replace took
/// the lock first, so pressing Restart desktop twice starts one desktop.
fn lvrr_replace_locked(file: &std::fs::File) -> bool {
    let mut system = sysinfo::System::new();
    let mut target = None;
    let mut killed = false;
    let mut since = Instant::now();
    while matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
        // the holder writes its pid right after taking the lock
        if let Some(pid) = lvrr_read_pid(file)
            && let Some(proc) = lvrr_find_wayvr(&mut system, pid)
        {
            match target {
                None => {
                    proc.kill_with(sysinfo::Signal::Term);
                    target = Some(pid);
                    since = Instant::now();
                }
                Some(old) if old != pid => {
                    println!("The previous instance was replaced by another one.");
                    return false;
                }
                Some(_) if !killed && since.elapsed() > Duration::from_secs(3) => {
                    println!("The previous instance didn't quit, killing it.");
                    proc.kill();
                    killed = true;
                }
                Some(_) => {}
            }
        }
        if since.elapsed() > Duration::from_secs(6) {
            println!("The previous instance couldn't be stopped.");
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// LVRR: never closed, so the pid file stays locked as long as the process runs
static PID_LOCK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

fn ensure_single_instance(replace: bool) -> bool {
    use std::{fs::TryLockError, os::unix::fs::FileExt};

    let mut path =
        std::env::var("XDG_RUNTIME_DIR").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from);
    path.push("wayvr.pid");

    // LVRR: the running instance holds a lock on the pid file, which a crash or the in-place
    // restart's exec drops, so a pid left in the file doesn't count. The file is never removed:
    // a waiter could lock the removed one while a new start creates another.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::Error(e)) => println!("Failed to lock {}: {e}", path.display()),
        Err(TryLockError::WouldBlock) if !replace => return false,
        Err(TryLockError::WouldBlock) => {
            if !lvrr_replace_locked(&file) {
                return false;
            }
        }
    }

    // LVRR: a wayvr that doesn't lock, upstream's or LVRR's from before the lock
    let mut system = sysinfo::System::new();
    if let Some(pid) = lvrr_read_pid(&file)
        && let Some(proc) = lvrr_find_wayvr(&mut system, pid)
    {
        if replace {
            proc.kill_with(sysinfo::Signal::Term);
            lvrr_wait_or_kill(&mut system, Pid::from_u32(pid));
        } else {
            return false;
        }
    }

    let pid = std::process::id().to_string();
    file.set_len(0).unwrap();
    file.write_all_at(pid.as_bytes(), 0).unwrap();
    let _ = PID_LOCK.set(file);

    true
}
