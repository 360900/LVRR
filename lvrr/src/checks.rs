//! The doctor's system checks besides the firewall: the WiVRn dashboard's
//! warnings, plus what it showed elsewhere or LVRR needs without it.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use ash::vk;
use dbus::blocking::Connection;
use serde_json::Value;

use crate::bus;
use crate::doctor::{Check, FIX, Status};

/// WiVRn's OVR_COMPAT_SEARCH_PATH from wivrn/CMakeLists.txt, which xtask
/// puts after this install's xrizer
const OVR_COMPAT_SEARCH_PATH: [&str; 7] = [
    "/opt/xrizer",
    "/usr/local/lib/OpenComposite",
    "/usr/lib/OpenComposite",
    "/opt/OpenComposite",
    "/opt/opencomposite",
    "/opt/VapoR",
    "/usr/local/lib/VapoR",
];

/// NVIDIA's driver version 565.77 as Vulkan encodes it, the oldest WiVRn supports
const NVIDIA_MIN_VERSION: u32 = (565 << 22) | (77 << 14);

const STEAM_FLATPAK: &str = "com.valvesoftware.Steam";

fn check(id: &'static str, title: &'static str, status: Status, message: String) -> Check {
    Check {
        id,
        title,
        status,
        message,
        fix: None,
    }
}

/// The server ignores a config it can't read and runs on its defaults, which have
/// no "application" entry to start LVRR's VR dashboard and publish through Avahi
pub fn config() -> Check {
    let (status, message) = match (crate::config_path(), crate::config_error()) {
        (Err(e), _) => (Status::Fail, e),
        (Ok(path), Some(e)) => (
            Status::Fail,
            format!(
                "LVRR's server can't read {} ({e}) and runs on its defaults, without the VR dashboard. Fix or delete the file, then stop and start LVRR.",
                path.display()
            ),
        ),
        (Ok(path), None) if path.exists() => (
            Status::Ok,
            format!("LVRR's server reads its settings from {}.", path.display()),
        ),
        (Ok(path), None) => (
            Status::Ok,
            format!(
                "LVRR writes {} with its defaults when it starts.",
                path.display()
            ),
        ),
    };
    check("config", "Server config", status, message)
}

/// Whether the server publishes itself for the headset to find. It takes both
/// null, which WiVRn's docs give, and "none" as off.
pub fn publishing(config: &Value) -> bool {
    match config.get("publish-service") {
        Some(Value::Null) => false,
        Some(Value::String(service)) => service != "none",
        _ => true,
    }
}

/// The server publishes itself through Avahi and stops when it can't
pub fn avahi(config: &Value) -> Check {
    let (status, message) = if !publishing(config) {
        (
            Status::Ok,
            "Publishing is off in the server config, add this PC by its address in the headset."
                .to_string(),
        )
    } else if Connection::new_system().is_ok_and(|c| bus::has_owner(&c, "org.freedesktop.Avahi")) {
        (
            Status::Ok,
            "Running, so the headset finds this PC by itself.".to_string(),
        )
    } else if installed("avahi-daemon") {
        let start = if systemd_booted() {
            ""
        } else {
            " Start the avahi-daemon service with your init system."
        };
        (
            Status::Fail,
            format!(
                "Avahi is installed but not running, so LVRR's server can't start and the headset can't find this PC.{start}"
            ),
        )
    } else {
        let config_file = crate::config_path().map_or_else(
            |_| "LVRR's server config".to_string(),
            |path| path.display().to_string(),
        );
        (
            Status::Fail,
            format!(
                "Avahi isn't installed, so LVRR's server can't start. Install avahi, or set \"publish-service\" to null in {config_file} and add this PC by its address in the headset."
            ),
        )
    };
    let mut c = check("avahi", "Avahi", status, message);
    // setup-system starts it with systemctl
    if status != Status::Ok && installed("avahi-daemon") && systemd_booted() {
        c.fix = Some(FIX);
    }
    c
}

/// Whether systemd runs this system, as sd_booted() tells; Void, Artix or Gentoo may use
/// another init. server::systemd_available() tells about the user manager.
pub fn systemd_booted() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

fn installed(program: &str) -> bool {
    crate::which(program).is_some()
        || ["/usr/bin", "/usr/sbin", "/sbin"]
            .iter()
            .any(|dir| Path::new(dir).join(program).exists())
}

struct Gpu {
    name: String,
    kind: vk::PhysicalDeviceType,
    driver: vk::DriverId,
    version: u32,
    /// Such as "NVIDIA 580.95.05" or "radv Mesa 26.1.6"
    driver_text: String,
}

/// The GPU the dashboard looked at: the first discrete one, else the first
pub fn vulkan() -> Check {
    let (status, message) = match gpu() {
        Err(e) => (Status::Fail, e),
        Ok(None) => (Status::Fail, "No Vulkan device found.".to_string()),
        Ok(Some(gpu)) if gpu.kind == vk::PhysicalDeviceType::CPU => (
            Status::Fail,
            format!("No Vulkan GPU driver found, only {}.", gpu.name),
        ),
        Ok(Some(gpu))
            if gpu.driver == vk::DriverId::NVIDIA_PROPRIETARY
                && gpu.version < NVIDIA_MIN_VERSION =>
        {
            (
                Status::Fail,
                format!(
                    "NVIDIA driver {}.{} is too old, LVRR needs 565.77 or newer.",
                    gpu.version >> 22,
                    (gpu.version >> 14) & 0xff
                ),
            )
        }
        Ok(Some(gpu))
            if matches!(
                gpu.driver,
                vk::DriverId::AMD_PROPRIETARY | vk::DriverId::AMD_OPEN_SOURCE
            ) =>
        {
            (
                Status::Warn,
                format!(
                    "{} uses AMDVLK or AMDGPU-PRO, which hardware encoding doesn't work with. Use Mesa's RADV driver.",
                    gpu.name
                ),
            )
        }
        Ok(Some(gpu)) => (Status::Ok, format!("{}, {}.", gpu.name, gpu.driver_text)),
    };
    check("vulkan", "GPU", status, message)
}

fn gpu() -> Result<Option<Gpu>, String> {
    // SAFETY: loads the system's Vulkan loader, which has no preconditions
    let entry =
        unsafe { ash::Entry::load() }.map_err(|e| format!("Vulkan isn't installed: {e}"))?;
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
    let info = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: a valid create info without layers or extensions
    let instance = unsafe { entry.create_instance(&info, None) }
        .map_err(|e| format!("creating a Vulkan instance: {e}"))?;
    let gpus = describe(&instance);
    // SAFETY: nothing created from the instance outlives it
    unsafe { instance.destroy_instance(None) };
    let gpus = gpus?;
    let discrete = gpus
        .iter()
        .position(|g| g.kind == vk::PhysicalDeviceType::DISCRETE_GPU);
    Ok(gpus.into_iter().nth(discrete.unwrap_or(0)))
}

fn describe(instance: &ash::Instance) -> Result<Vec<Gpu>, String> {
    // SAFETY: the instance is valid
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("listing Vulkan devices: {e}"))?;
    let text = |s: Result<&std::ffi::CStr, _>| s.map(|s| s.to_string_lossy().into_owned());
    Ok(devices
        .into_iter()
        .map(|device| {
            // SAFETY: device comes from this instance
            let props = unsafe { instance.get_physical_device_properties(device) };
            let mut driver = vk::PhysicalDeviceDriverProperties::default();
            // The driver properties are core since 1.2
            if props.api_version >= vk::API_VERSION_1_2 {
                let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
                // SAFETY: device comes from this instance, which is 1.2
                unsafe { instance.get_physical_device_properties2(device, &mut props2) };
            }
            Gpu {
                name: text(props.device_name_as_c_str()).unwrap_or_default(),
                kind: props.device_type,
                driver: driver.driver_id,
                version: props.driver_version,
                driver_text: [
                    text(driver.driver_name_as_c_str()),
                    text(driver.driver_info_as_c_str()),
                ]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            }
        })
        .collect())
}

/// What the Steam flatpak must be allowed to read: the server's socket, the
/// active runtimes and this install
fn steam_filesystems(root: &Path) -> Vec<String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    vec![
        "xdg-run/wivrn".to_string(),
        "xdg-config/openxr".to_string(),
        "xdg-config/openvr".to_string(),
        root.to_string_lossy().into_owned(),
    ]
}

/// Whether Steam other than the flatpak is installed, or umu, which runs Lutris's
/// and Heroic's games in the same pressure-vessel sandbox. Generous, as an install
/// it misses would go without the pressure-vessel fix.
fn steam_installed() -> bool {
    let mut paths = vec![
        PathBuf::from("/usr/games/steam"),
        PathBuf::from("/snap/bin/steam"),
    ];
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".steam"));
        paths.push(home.join(".local/share/Steam"));
    }
    if let Ok(data) = crate::xdg_dir("XDG_DATA_HOME", ".local/share") {
        paths.push(data.join("Steam"));
        paths.push(data.join("umu"));
    }
    installed("steam") || paths.iter().any(|path| path.exists())
}

/// `flatpak`: the Steam flatpak's permissions, see steam_flatpak()
pub fn steam(root: &Path, flatpak: Option<&Result<String, String>>) -> Check {
    let (status, message) = if Path::new("/snap/bin/steam").exists() {
        (
            Status::Warn,
            "Steam is installed as a snap, which can't start games in VR with LVRR. Use your distribution's Steam or the Flathub one.".to_string(),
        )
    } else if let Some(permissions) = flatpak {
        match permissions.as_deref().map(|p| missing_filesystems(p, root)) {
            Ok(missing) if missing.is_empty() => {
                (Status::Ok, "The Steam flatpak can reach LVRR.".to_string())
            }
            Ok(missing) => (
                Status::Warn,
                format!("The Steam flatpak can't read {}.", missing.join(", ")),
            ),
            Err(e) => (Status::Warn, e.to_string()),
        }
    } else if !steam_installed() {
        (Status::Ok, "Steam isn't installed.".to_string())
    } else {
        (Status::Ok, "Steam needs no changes.".to_string())
    };
    let mut c = check("steam", "Steam", status, message);
    if status != Status::Ok && flatpak.is_some() {
        c.fix = Some(FIX);
    }
    c
}

/// Gives the Steam flatpak what it needs, like the dashboard's "Fix it". A user
/// override, which flatpak applies to a system install too, so it needs no admin
/// password.
pub fn fix_steam(root: &Path) -> Result<(), String> {
    if flatpak_scope().is_none() {
        return Ok(());
    }
    let mut args = vec!["--user".to_string(), "override".to_string()];
    args.extend(
        steam_filesystems(root)
            .into_iter()
            .map(|path| format!("--filesystem={path}:ro")),
    );
    args.push(STEAM_FLATPAK.to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("flatpak", &args)
}

/// The variable pressure-vessel needs to pass LVRR's OpenXR runtime into Steam's
/// sandbox; without it xrizer in a game can't reach the runtime socket
pub const OPENXR_IMPORT_VAR: &str = "PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES";
/// Dropped in the user's config so every login sets the variable for Steam
const ENV_CONF: &str = "environment.d/lvrr.conf";
const ENV_CONF_TEXT: &str = "# Written by LVRR: let Steam's pressure-vessel sandbox reach LVRR's OpenXR runtime.\nPRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1\n";

/// `~/.config/environment.d/lvrr.conf`
fn env_conf_path() -> Option<PathBuf> {
    crate::xdg_dir("XDG_CONFIG_HOME", ".config")
        .ok()
        .map(|dir| dir.join(ENV_CONF))
}

/// The variable is in this process's environment. Not necessarily Steam's: a
/// terminal has the shell's, VR the user manager's.
fn openxr_import_in_env() -> bool {
    env::var(OPENXR_IMPORT_VAR).as_deref() == Ok("1")
}

/// Whether the running Steam client has the variable, which its games inherit;
/// None when none runs or its environment can't be read. Every install runs
/// the client as ubuntu12_32/steam, and process_exe sees only this user's.
/// The dashboard's PC > Server reads it the same way (lvrr-common's steam_env).
pub fn steam_client_env() -> Option<bool> {
    let pid = fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .find(|pid| {
            crate::server::process_exe(*pid).is_some_and(|exe| exe.ends_with("ubuntu12_32/steam"))
        })?;
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let set = format!("{OPENXR_IMPORT_VAR}=1");
    Some(environ.split(|b| *b == 0).any(|var| var == set.as_bytes()))
}

/// When our environment.d file, which applies after the next login, was written;
/// None when it doesn't set the variable
fn env_conf_written() -> Option<SystemTime> {
    let path = env_conf_path()?;
    let text = fs::read_to_string(&path).ok()?;
    let set = format!("{OPENXR_IMPORT_VAR}=1");
    if !text.lines().any(|l| l.trim() == set) {
        return None;
    }
    fs::metadata(&path).and_then(|m| m.modified()).ok()
}

/// When the systemd user manager started; None without one, or when systemctl
/// can't tell, as before systemd 251. A reload, such as the one Arch's pacman
/// hook runs when a package with user units changes, keeps it.
fn manager_started() -> Option<SystemTime> {
    if !crate::server::systemd_available() {
        return None;
    }
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=UserspaceTimestamp",
            "--timestamp=unix",
            "--value",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    // such as "@1761900000"
    let text = String::from_utf8_lossy(&output.stdout);
    let secs = text.trim().strip_prefix('@')?.parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Looks for `PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1` in the [Environment]
/// group of a flatpak's permissions
fn has_openxr_env(permissions: &str) -> bool {
    let mut environment = false;
    for line in permissions.lines().map(str::trim) {
        if line.starts_with('[') {
            environment = line == "[Environment]";
        } else if environment
            && let Some(value) = line.strip_prefix(&format!("{OPENXR_IMPORT_VAR}="))
        {
            return value.trim() == "1";
        }
    }
    false
}

/// Steam sandboxes VR games with pressure-vessel, which hides LVRR's OpenXR
/// runtime unless `PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1` is set. Without
/// it a game's xrizer fails with `ERROR_RUNTIME_UNAVAILABLE`. `flatpak`: the
/// Steam flatpak's permissions, see steam_flatpak().
pub fn pressure_vessel(flatpak: Option<&Result<String, String>>) -> Check {
    let (status, message, fixable) = match flatpak {
        Some(Ok(permissions)) if has_openxr_env(permissions) => (
            Status::Ok,
            "The Steam flatpak passes LVRR's runtime into VR games.".to_string(),
            false,
        ),
        Some(Ok(_)) => (
            Status::Fail,
            "Steam VR games can't reach LVRR's runtime. Fixing lets the Steam flatpak import it; restart Steam afterwards.".to_string(),
            true,
        ),
        Some(Err(e)) => (Status::Warn, e.clone(), false),
        None if !steam_installed() => (Status::Ok, "Steam isn't installed.".to_string(), false),
        None => {
            let written = env_conf_written();
            native_import(
                steam_client_env(),
                openxr_import_in_env(),
                manager_env()
                    .map(|env| env_value(&env, OPENXR_IMPORT_VAR).as_deref() == Some("1")),
                written.is_some(),
                written.is_some_and(|w| manager_started().is_some_and(|s| w > s)),
            )
        }
    };
    let mut c = check("pressure-vessel", "Steam game sandbox", status, message);
    if fixable {
        c.fix = Some(FIX);
    }
    c
}

/// The Steam game sandbox check without the flatpak, by the environment Steam
/// gets rather than this process's. `steam`: whether the running Steam has the
/// variable, None when none runs. `here`: whether this process has it.
/// `manager`: whether the systemd user manager's units have it, LVRR's VR
/// dashboard among them and so a Steam it starts; None without a manager.
/// `conf`: whether lvrr.conf sets it. `reloaded`: lvrr.conf is newer than the
/// manager, which then has the variable from a reload, and a desktop that
/// started before it doesn't. Also returns whether writing lvrr.conf helps.
fn native_import(
    steam: Option<bool>,
    here: bool,
    manager: Option<bool>,
    conf: bool,
    reloaded: bool,
) -> (Status, String, bool) {
    let set = format!("{OPENXR_IMPORT_VAR}=1");
    match (steam, manager) {
        (Some(true), _) => (
            Status::Ok,
            format!("Steam runs with {set}, so its VR games reach LVRR's runtime."),
            false,
        ),
        (None, _) if here && manager != Some(false) => (
            Status::Ok,
            "Steam VR games inherit LVRR's runtime.".to_string(),
            false,
        ),
        // The manager has it but the desktop doesn't, so the desktop doesn't take
        // the manager's environment, which environment.d feeds; after a reload the
        // desktop may just predate lvrr.conf. A Steam the VR dashboard starts has
        // it, so only a running Steam without it fails.
        (_, Some(true)) if !reloaded => (
            if steam == Some(false) {
                Status::Fail
            } else {
                Status::Warn
            },
            format!(
                "Your desktop doesn't apply ~/.config/environment.d, so a Steam started from it runs VR games on the monitor. Set {set} in your compositor's or session's environment and restart Steam, or add \"{set} %command%\" to each VR game's launch options."
            ),
            false,
        ),
        // Only the systemd user manager reads environment.d
        (_, None) => (
            Status::Fail,
            format!(
                "Steam VR games can't reach LVRR's runtime, so they start on the monitor. Set {set} in your session's environment and restart Steam, or add \"{set} %command%\" to each VR game's launch options."
            ),
            false,
        ),
        _ if conf => (
            Status::Warn,
            "Steam VR games reach LVRR's runtime after the next login. Log out and back in to apply it.".to_string(),
            false,
        ),
        _ => (
            Status::Fail,
            format!(
                "Steam VR games can't reach LVRR's runtime, so they start on the monitor instead of the headset. Fixing sets {OPENXR_IMPORT_VAR} for your session; log out and back in to apply it."
            ),
            true,
        ),
    }
}

/// Sets the pressure-vessel OpenXR import for Steam: for the Steam flatpak a user
/// override, which flatpak applies to a system install too, so it needs no admin
/// password; else `~/.config/environment.d/lvrr.conf`, which the systemd user
/// manager reads
pub fn fix_pressure_vessel() -> Result<(), String> {
    if flatpak_scope().is_some() {
        let env = format!("--env={OPENXR_IMPORT_VAR}=1");
        return run("flatpak", &["override", "--user", &env, STEAM_FLATPAK]);
    }
    if !crate::server::systemd_available() {
        return Err(format!(
            "without a systemd user manager ~/.config/environment.d isn't read, set {OPENXR_IMPORT_VAR}=1 in your session's environment"
        ));
    }
    let path = env_conf_path().ok_or("could not find the config directory")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    fs::write(&path, ENV_CONF_TEXT).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn flatpak_scope() -> Option<&'static str> {
    crate::which("flatpak")?;
    ["--user", "--system"].into_iter().find(|scope| {
        Command::new("flatpak")
            .args([scope, "info", STEAM_FLATPAK])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// The Steam flatpak's permissions as `flatpak info --show-permissions` prints
/// them: its metadata with every override that applies, system and user,
/// global and its own. `override --show` prints one of those files. None
/// without the Steam flatpak.
pub fn steam_flatpak() -> Option<Result<String, String>> {
    let scope = flatpak_scope()?;
    Some(
        Command::new("flatpak")
            .args([scope, "info", "--show-permissions", STEAM_FLATPAK])
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("running flatpak: {e}"))
            .and_then(|output| {
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
                    .ok_or_else(|| "Could not read the Steam flatpak's permissions.".to_string())
            }),
    )
}

/// What of steam_filesystems() the flatpak's `permissions` don't let it read
fn missing_filesystems(permissions: &str, root: &Path) -> Vec<String> {
    // steam_filesystems() has the install's real path
    let home =
        env::var_os("HOME").map(|home| fs::canonicalize(&home).unwrap_or_else(|_| home.into()));
    let have = filesystems(permissions);
    steam_filesystems(root)
        .into_iter()
        .filter(|need| !covered(&have, need, home.as_deref()))
        .collect()
}

/// The top folders flatpak's host grant leaves out besides /lib*, see
/// flatpak-metadata(5)
const HOST_RESERVED: [&str; 14] = [
    "app", "bin", "boot", "dev", "efi", "etc", "proc", "root", "run", "sbin", "sys", "tmp", "usr",
    "var",
];

/// Whether the filesystem grants `have` let the flatpak read `need`, one of
/// steam_filesystems(): its own grant or one for a folder above it.
/// xdg-run/wivrn needs its own: flatpak has no grant for all of xdg-run, and
/// home and host leave out /run.
fn covered(have: &[String], need: &str, home: Option<&Path>) -> bool {
    have.iter().any(|grant| {
        if grant == need {
            return true;
        }
        if need.starts_with("xdg-config/") {
            return matches!(grant.as_str(), "xdg-config" | "home" | "host");
        }
        if !need.starts_with('/') {
            return false;
        }
        let need = Path::new(need);
        match grant.as_str() {
            "home" => home.is_some_and(|home| need.starts_with(home)),
            "host" => {
                let top = need
                    .iter()
                    .nth(1)
                    .and_then(|top| top.to_str())
                    .unwrap_or_default();
                !(top.starts_with("lib") || HOST_RESERVED.contains(&top))
            }
            grant => match grant.strip_prefix("~/") {
                Some(rest) => home.is_some_and(|home| need.starts_with(home.join(rest))),
                None => grant.starts_with('/') && need.starts_with(grant),
            },
        }
    })
}

/// The `filesystems` of the [Context] group of a flatpak's permissions, without
/// `:ro`/`:rw` suffixes and negations
fn filesystems(permissions: &str) -> Vec<String> {
    let mut context = false;
    for line in permissions.lines().map(str::trim) {
        if line.starts_with('[') {
            context = line == "[Context]";
        } else if context && let Some(value) = line.strip_prefix("filesystems=") {
            return value
                .split(';')
                .filter(|item| !item.is_empty() && !item.starts_with('!'))
                .map(|item| match item.rsplit_once(':') {
                    Some((path, "ro" | "rw" | "create")) => path.to_string(),
                    _ => item.to_string(),
                })
                .collect();
        }
    }
    Vec::new()
}

pub fn uinput(config: &Value) -> Check {
    let forwarding = config.get("hid-forwarding").and_then(Value::as_bool) == Some(true);
    // First: off, no device reaches this PC, whatever /dev/uinput allows
    let (status, message) = if !forwarding {
        (
            Status::Ok,
            "Accepting keyboard, mouse and gamepad from the headset is off, so /dev/uinput isn't needed.",
        )
    } else {
        match fs::OpenOptions::new().write(true).open("/dev/uinput") {
            Ok(_) => (
                Status::Ok,
                "The headset's keyboard, mouse and gamepad can reach this PC.",
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => (
                Status::Warn,
                "/dev/uinput is missing, so the headset's keyboard, mouse and gamepad don't reach this PC. Load the uinput kernel module.",
            ),
            Err(_) => (
                Status::Warn,
                "/dev/uinput isn't writable, so the headset's keyboard, mouse and gamepad don't reach this PC. Installing steam-devices, or adding your user to the input group, fixes it.",
            ),
        }
    };
    check("uinput", "Input forwarding", status, message.to_string())
}

pub fn usb(root: &Path, config: &Value) -> Check {
    let auto = config.get("usb-auto-connect").and_then(Value::as_bool) == Some(true);
    let (status, message) = if !auto {
        (Status::Ok, "Connecting by USB when plugged in is off.")
    } else if crate::which_in("adb", &server_path(root)).is_some() {
        (
            Status::Ok,
            "The headset connects when it's plugged in, if USB debugging is allowed.",
        )
    } else if crate::which("adb").is_some() {
        (
            Status::Warn,
            "adb is on the PATH here but not on your session's, where LVRR's server looks for it, so the headset doesn't connect when plugged in. Install android-tools, or add adb's folder to PATH in ~/.config/environment.d/ and log in again.",
        )
    } else {
        (
            Status::Warn,
            "Connecting by USB when plugged in is on but adb isn't installed, install android-tools.",
        )
    };
    check("usb", "USB", status, message.to_string())
}

/// The PATH LVRR's server finds programs such as adb on: a unit gets the user
/// manager's, else the server has the PATH of what started it, likely this
/// process's. `server_command` puts this install's bin first.
fn server_path(root: &Path) -> OsString {
    let path = manager_env()
        .and_then(|env| env_value(&env, "PATH"))
        .map_or_else(|| env::var_os("PATH").unwrap_or_default(), OsString::from);
    env::join_paths(std::iter::once(root.join("bin")).chain(env::split_paths(&path)))
        .unwrap_or(path)
}

/// The environment the systemd user manager gives its units, LVRR's server
/// among them, as `systemctl --user show-environment` prints it; None without
/// a user manager
fn manager_env() -> Option<String> {
    if !crate::server::systemd_available() {
        return None;
    }
    let output = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A variable's value in `systemctl show-environment` output, which writes a
/// value with spaces or shell characters as $'...', with \ before \ and '
fn env_value(env: &str, name: &str) -> Option<String> {
    let value = env
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))?;
    let Some(quoted) = value.strip_prefix("$'").and_then(|v| v.strip_suffix('\'')) else {
        return Some(value.to_string());
    };
    let mut unquoted = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        unquoted.push(if c == '\\' { chars.next()? } else { c });
    }
    Some(unquoted)
}

pub fn pipewire() -> Check {
    let dir = env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| env::var_os("XDG_RUNTIME_DIR"));
    let running =
        dir.is_some_and(|dir| UnixStream::connect(Path::new(&dir).join("pipewire-0")).is_ok());
    let (status, message) = if running {
        (
            Status::Ok,
            "Running, so the headset has sound and a microphone.",
        )
    } else {
        (
            Status::Warn,
            "PipeWire isn't running, so the headset has no sound or microphone.",
        )
    };
    check("pipewire", "Audio", status, message.to_string())
}

pub fn port(root: &Path, config: &Value) -> Check {
    let port = crate::server_port(config);
    let ours = fs::canonicalize(root.join("bin/wivrn-server")).ok();
    let (status, message) = match listener(port) {
        None => (Status::Ok, format!("Port {port} is free.")),
        Some(Listener::Visible { exe, .. }) if Some(&exe) == ours.as_ref() => (
            Status::Ok,
            format!("LVRR's server is listening on port {port}."),
        ),
        Some(Listener::Visible { pid, exe }) => (
            Status::Fail,
            format!("Port {port} is in use by {} (pid {pid}).", exe.display()),
        ),
        // A server with file capabilities hides its descriptors from its own
        // user too: the listener is LVRR's when the lock holder hides as well
        Some(Listener::Hidden { mine: true })
            if crate::server::lock_holder()
                .is_some_and(|pid| crate::server::process_exe(pid).is_none()) =>
        {
            (
                Status::Ok,
                format!("LVRR's server is listening on port {port}."),
            )
        }
        Some(Listener::Hidden { mine: true }) => (
            Status::Fail,
            format!("Port {port} is in use by another of your programs."),
        ),
        Some(Listener::Hidden { mine: false }) => (
            Status::Fail,
            format!("Port {port} is in use by a program of another user."),
        ),
    };
    check("port", "Server port", status, message)
}

/// A process listening on a TCP port
enum Listener {
    /// One whose descriptors this user can read
    Visible { pid: u32, exe: PathBuf },
    /// One that hides them: another user's, or an undumpable one of this
    /// user's
    Hidden { mine: bool },
}

/// Who listens on TCP `port`, None when nobody
fn listener(port: u16) -> Option<Listener> {
    let sockets: Vec<(String, u32)> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|table| fs::read_to_string(table).ok())
        .flat_map(|table| listening_inodes(&table, port))
        .collect();
    if sockets.is_empty() {
        return None;
    }
    Some(match socket_owner(&sockets) {
        Some((pid, exe)) => Listener::Visible { pid, exe },
        None => {
            // SAFETY: getuid has no preconditions
            let uid = unsafe { libc::getuid() };
            Listener::Hidden {
                mine: sockets.iter().any(|&(_, owner)| owner == uid),
            }
        }
    })
}

/// Socket inodes of a /proc/net/tcp table that listen on `port`, with the
/// uids that own them
fn listening_inodes(table: &str, port: u16) -> Vec<(String, u32)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (_, local_port) = fields.get(1)?.rsplit_once(':')?;
            let listening = fields.get(3) == Some(&"0A");
            (listening && u16::from_str_radix(local_port, 16) == Ok(port))
                .then(|| {
                    let uid = fields.get(7)?.parse().ok()?;
                    Some((fields.get(9)?.to_string(), uid))
                })
                .flatten()
        })
        .collect()
}

fn socket_owner(sockets: &[(String, u32)]) -> Option<(u32, PathBuf)> {
    let targets: Vec<String> = sockets
        .iter()
        .map(|(inode, _)| format!("socket:[{inode}]"))
        .collect();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|p| p.parse().ok()) else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let owns = fds.flatten().any(|fd| {
            fs::read_link(fd.path())
                .is_ok_and(|link| targets.iter().any(|t| link.as_os_str() == t.as_str()))
        });
        if owns {
            return Some((pid, crate::server::process_exe(pid)?));
        }
    }
    None
}

/// `session_running`: a headset session of this install's server runs, so
/// the active runtime should be ours
pub fn openxr(root: &Path, session_running: bool) -> Check {
    let manifest = root.join("share/openxr/1/openxr_wivrn.json");
    let (status, message) = match runtime_library(&manifest) {
        Err(e) => (Status::Fail, e),
        Ok(()) => match env::var_os("XR_RUNTIME_JSON") {
            Some(set) if !same_file(Path::new(&set), &manifest) => (
                Status::Warn,
                format!(
                    "XR_RUNTIME_JSON is set to {}, so apps started with it don't use LVRR.",
                    set.to_string_lossy()
                ),
            ),
            _ => {
                let active = crate::xdg_dir("XDG_CONFIG_HOME", ".config")
                    .map(|dir| dir.join("openxr/1/active_runtime.json"));
                match active {
                    Ok(active) if session_running && !same_file(&active, &manifest) => (
                        Status::Warn,
                        format!(
                            "A headset is connected, but {} doesn't point to LVRR's runtime.",
                            active.display()
                        ),
                    ),
                    _ => (
                        Status::Ok,
                        "Apps use LVRR's runtime while a headset is connected.".to_string(),
                    ),
                }
            }
        },
    };
    check("openxr", "OpenXR runtime", status, message)
}

fn runtime_library(manifest: &Path) -> Result<(), String> {
    let library = fs::read(manifest)
        .ok()
        .and_then(|text| serde_json::from_slice::<Value>(&text).ok())
        .and_then(|json| json["runtime"]["library_path"].as_str().map(PathBuf::from))
        .ok_or_else(|| {
            format!(
                "This install's OpenXR runtime manifest {} is missing or broken.",
                manifest.display()
            )
        })?;
    let library = manifest.parent().unwrap_or(manifest).join(library);
    if library.exists() {
        Ok(())
    } else {
        Err(format!(
            "The OpenXR runtime library {} is missing.",
            library.display()
        ))
    }
}

pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// The OpenVR compatibility library the server uses, picked like its
/// `active_runtime::openvr_compat_path()`
pub fn openvr_compat_path(root: &Path, config: &Value) -> Option<PathBuf> {
    match config.get("openvr-compat-path") {
        Some(Value::String(path)) => Some(PathBuf::from(path)),
        Some(Value::Null) => None,
        _ => std::iter::once(root.join("lib/xrizer"))
            .chain(OVR_COMPAT_SEARCH_PATH.iter().map(PathBuf::from))
            .find(|path| path.exists()),
    }
}

pub fn openvr(root: &Path, config: &Value) -> Check {
    let (status, message) = match openvr_compat_path(root, config) {
        None if config.get("openvr-compat-path") == Some(&Value::Null) => unmanaged_openvr(),
        None => (
            Status::Warn,
            "No OpenVR compatibility library, so Steam games can't start in VR.".to_string(),
        ),
        Some(path) if path.join("bin/linux64/vrclient.so").exists() => {
            (Status::Ok, format!("Steam games use {}.", path.display()))
        }
        Some(path) => (
            Status::Warn,
            format!(
                "{} has no bin/linux64/vrclient.so, so Steam games can't start in VR.",
                path.display()
            ),
        ),
    };
    check("openvr", "OpenVR compatibility", status, message)
}

/// PC > Server's "Leave OpenVR settings alone": the server doesn't touch
/// openvrpaths.vrpath, so Steam games use the runtime it names
fn unmanaged_openvr() -> (Status, String) {
    let vrpath = match crate::xdg_dir("XDG_CONFIG_HOME", ".config") {
        Ok(dir) => dir.join("openvr/openvrpaths.vrpath"),
        Err(e) => return (Status::Warn, e),
    };
    let runtime = fs::read_to_string(&vrpath)
        .ok()
        .and_then(|text| vrpath_runtime(&text));
    match runtime {
        Some(runtime) => (
            Status::Ok,
            format!("LVRR leaves OpenVR settings alone, Steam games use {runtime}."),
        ),
        None => (
            Status::Warn,
            format!(
                "LVRR leaves OpenVR settings alone and {} names no OpenVR runtime, so Steam games can't start in VR.",
                vrpath.display()
            ),
        ),
    }
}

/// The runtime an openvrpaths.vrpath names: the first of its `runtime` list,
/// the one OpenVR's loader uses
fn vrpath_runtime(text: &str) -> Option<String> {
    let json: Value = serde_json::from_str(text).ok()?;
    json["runtime"][0]
        .as_str()
        .filter(|runtime| !runtime.is_empty())
        .map(str::to_string)
}

pub fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        // ufw and firewall-cmd print to stdout, which carries `lvrr doctor --json`'s report
        .stdout(std::io::stderr())
        .status()
        .map_err(|e| format!("running {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {}: {status}", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_listeners() {
        // 9757 listening on IPv4 and IPv6, an established connection from port 9757
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:261D 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41231 1 0000000000000000 100 0 0 10 0
   1: 0100007F:261D 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 41299 1 0000000000000000 20 4 30 10 -1
   2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1234 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(listening_inodes(table, 9757), [("41231".to_string(), 1000)]);
        // root's sshd
        assert_eq!(listening_inodes(table, 22), [("1234".to_string(), 0)]);
        assert!(listening_inodes(table, 9758).is_empty());
    }

    #[test]
    fn flatpak_filesystems() {
        let overrides = "[Context]\nfilesystems=xdg-run/wivrn:ro;xdg-config/openxr;/opt/lvrr:ro;!home;\n\n[Environment]\nfilesystems=nope\n";
        assert_eq!(
            filesystems(overrides),
            ["xdg-run/wivrn", "xdg-config/openxr", "/opt/lvrr"]
        );
        assert!(filesystems("[Environment]\nFOO=1\n").is_empty());
    }

    #[test]
    fn flatpak_grants() {
        // `flatpak info --show-permissions` of a system Steam given home and host
        // by a user override, as Flatseal writes it
        let permissions = "[Context]\nshared=network;ipc;\nsockets=x11;wayland;pulseaudio;\ndevices=all;\nfilesystems=xdg-music:ro;xdg-pictures:ro;xdg-run/speech-dispatcher:ro;home;host;\npersistent=.;\n\n[Session Bus Policy]\norg.freedesktop.Notifications=talk\n\n[Environment]\nPRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1\n";
        let have = filesystems(permissions);
        let home = Some(Path::new("/home/u"));
        assert!(covered(&have, "xdg-config/openxr", home));
        assert!(covered(&have, "/home/u/.local/lvrr", home));
        assert!(covered(&have, "/opt/lvrr", home));
        assert!(!covered(&have, "xdg-run/wivrn", home));
        assert!(has_openxr_env(permissions));
        // host leaves out /usr, /lib* and /var
        let host = ["host".to_string()];
        assert!(!covered(&host, "/usr/local/lvrr", home));
        assert!(!covered(&host, "/lib64/lvrr", home));
        assert!(!covered(&host, "/var/lib/lvrr", home));
        // home alone doesn't reach /opt, but a home under /var
        let home_only = ["home".to_string()];
        assert!(!covered(&home_only, "/opt/lvrr", home));
        assert!(covered(&home_only, "xdg-config/openvr", home));
        let var_home = Some(Path::new("/var/home/u"));
        assert!(covered(&home_only, "/var/home/u/lvrr", var_home));
        // parents by path, by ~/ and by xdg-config; a child or a sibling doesn't count
        let paths = ["/opt", "~/apps", "xdg-config", "xdg-run/wivrn-old"].map(String::from);
        assert!(covered(&paths, "/opt/lvrr", home));
        assert!(covered(&paths, "/home/u/apps/lvrr", home));
        assert!(covered(&paths, "xdg-config/openxr", home));
        assert!(!covered(&paths, "/home/u/app", home));
        assert!(!covered(&paths, "xdg-run/wivrn", home));
        let child = ["/opt/lvrr/bin".to_string()];
        assert!(!covered(&child, "/opt/lvrr", home));
        // what fix_steam() writes
        let fixed = "[Context]\nfilesystems=xdg-run/wivrn:ro;xdg-config/openxr:ro;xdg-config/openvr:ro;/nonexistent/lvrr:ro;\n";
        let root = Path::new("/nonexistent/lvrr");
        assert!(missing_filesystems(fixed, root).is_empty());
        let partial = "[Context]\nfilesystems=xdg-config/openxr;\n";
        assert_eq!(
            missing_filesystems(partial, root),
            ["xdg-run/wivrn", "xdg-config/openvr", "/nonexistent/lvrr"]
        );
    }

    #[test]
    fn native_steam_import() {
        let status = |steam, here, manager, conf, reloaded| {
            let (status, _, fixable) = native_import(steam, here, manager, conf, reloaded);
            (status, fixable)
        };
        // the running Steam decides
        assert_eq!(
            status(Some(true), false, Some(false), false, false),
            (Status::Ok, false)
        );
        // none runs: this process's environment, unless the manager's lacks it
        assert_eq!(
            status(None, true, Some(true), false, false),
            (Status::Ok, false)
        );
        assert_eq!(status(None, true, None, false, false), (Status::Ok, false));
        assert_eq!(
            status(None, true, Some(false), false, false),
            (Status::Fail, true)
        );
        // the manager has it, the desktop doesn't: no fix helps
        assert_eq!(
            status(Some(false), true, Some(true), true, false),
            (Status::Fail, false)
        );
        assert_eq!(
            status(None, false, Some(true), true, false),
            (Status::Warn, false)
        );
        let (_, message, _) = native_import(Some(false), false, Some(true), true, false);
        assert!(message.starts_with("Your desktop doesn't apply ~/.config/environment.d"));
        // a reload gave the manager lvrr.conf, the desktop started before it
        assert_eq!(
            status(Some(false), false, Some(true), true, true),
            (Status::Warn, false)
        );
        assert_eq!(
            status(None, false, Some(true), true, true),
            (Status::Warn, false)
        );
        let (_, message, _) = native_import(Some(false), false, Some(true), true, true);
        assert!(message.contains("after the next login"));
        // without a user manager environment.d does nothing
        assert_eq!(
            status(Some(false), false, None, true, false),
            (Status::Fail, false)
        );
        assert_eq!(
            status(None, false, None, false, false),
            (Status::Fail, false)
        );
        // written, not yet applied
        assert_eq!(
            status(Some(false), false, Some(false), true, false),
            (Status::Warn, false)
        );
        assert_eq!(
            status(None, false, Some(false), false, false),
            (Status::Fail, true)
        );
    }

    #[test]
    fn openxr_env() {
        let on = "[Context]\nfilesystems=xdg-run/wivrn\n\n[Environment]\nPRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1\n";
        assert!(has_openxr_env(on));
        // the same key outside [Environment] does not count
        let ctx = "[Context]\nPRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1\n";
        assert!(!has_openxr_env(ctx));
        assert!(!has_openxr_env(
            "[Environment]\nPRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=0\n"
        ));
        assert!(!has_openxr_env(""));
    }

    #[test]
    fn compat_path() {
        let root = Path::new("/nonexistent/lvrr");
        let config: Value = serde_json::from_str(r#"{"openvr-compat-path": "/opt/x"}"#).unwrap();
        assert_eq!(
            openvr_compat_path(root, &config),
            Some(PathBuf::from("/opt/x"))
        );
        let off: Value = serde_json::from_str(r#"{"openvr-compat-path": null}"#).unwrap();
        assert_eq!(openvr_compat_path(root, &off), None);
    }

    #[test]
    fn vrpath_runtimes() {
        let vrpath = r#"{"jsonid": "vrpathreg", "runtime": ["/opt/OpenComposite", "/opt/xrizer"], "version": 1}"#;
        assert_eq!(
            vrpath_runtime(vrpath).as_deref(),
            Some("/opt/OpenComposite")
        );
        assert_eq!(vrpath_runtime(r#"{"runtime": [], "version": 1}"#), None);
        // broken JSON
        assert_eq!(vrpath_runtime(r#"{"runtime": ["/opt/xrizer""#), None);
    }

    #[test]
    fn manager_env_values() {
        let env = "HOME=/home/u\nPATH=/usr/local/bin:/usr/bin\nPATHEXT=x\n";
        assert_eq!(
            env_value(env, "PATH").as_deref(),
            Some("/usr/local/bin:/usr/bin")
        );
        assert_eq!(env_value("PATHEXT=x\n", "PATH"), None);
        // quoted for the shell: a space, a backslash and a quote
        let quoted = r"PATH=$'/home/u/my tools:/opt/a\\b:/opt/it\'s:/usr/bin'";
        assert_eq!(
            env_value(quoted, "PATH").as_deref(),
            Some(r"/home/u/my tools:/opt/a\b:/opt/it's:/usr/bin")
        );
    }

    #[test]
    fn service_publishing() {
        assert!(!publishing(&serde_json::json!({"publish-service": null})));
        assert!(!publishing(&serde_json::json!({"publish-service": "none"})));
        assert!(publishing(&serde_json::json!({"publish-service": "avahi"})));
        assert!(publishing(&serde_json::json!({})));
        // no config file
        assert!(publishing(&Value::Null));
    }
}
