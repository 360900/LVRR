//! The doctor's system checks besides the firewall: the WiVRn dashboard's
//! warnings, plus what it showed elsewhere or LVRR needs without it.

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

pub fn avahi(config: &Value) -> Check {
    let (status, message) = if config.get("publish-service").and_then(Value::as_str) == Some("none")
    {
        (
            Status::Ok,
            "Publishing is off in the server config, add this PC by its address in the headset.",
        )
    } else if Connection::new_system().is_ok_and(|c| bus::has_owner(&c, "org.freedesktop.Avahi")) {
        (
            Status::Ok,
            "Running, so the headset finds this PC by itself.",
        )
    } else if installed("avahi-daemon") {
        (
            Status::Fail,
            "Avahi is installed but not running, so the headset can't find this PC.",
        )
    } else {
        (
            Status::Fail,
            "Avahi isn't installed, so the headset can't find this PC. Install avahi, or add this PC by its address in the headset.",
        )
    };
    let mut c = check("avahi", "Avahi", status, message.to_string());
    if status != Status::Ok && installed("avahi-daemon") {
        c.fix = Some(FIX);
    }
    c
}

pub fn fix_avahi() -> Result<(), String> {
    run(
        "pkexec",
        &["systemctl", "enable", "--now", "avahi-daemon.service"],
    )
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

pub fn steam(root: &Path) -> Check {
    let (status, message) = if Path::new("/snap/bin/steam").exists() {
        (
            Status::Warn,
            "Steam is installed as a snap, which can't start games in VR with LVRR. Use your distribution's Steam or the Flathub one.".to_string(),
        )
    } else if let Some(scope) = flatpak_scope() {
        match missing_filesystems(scope, root) {
            Ok(missing) if missing.is_empty() => {
                (Status::Ok, "The Steam flatpak can reach LVRR.".to_string())
            }
            Ok(missing) => (
                Status::Warn,
                format!("The Steam flatpak can't read {}.", missing.join(", ")),
            ),
            Err(e) => (Status::Warn, e),
        }
    } else {
        (Status::Ok, "Steam needs no changes.".to_string())
    };
    let mut c = check("steam", "Steam", status, message);
    if status != Status::Ok && flatpak_scope().is_some() {
        c.fix = Some(FIX);
    }
    c
}

/// Gives the Steam flatpak what it needs, like the dashboard's "Fix it"
pub fn fix_steam(root: &Path) -> Result<(), String> {
    let Some(scope) = flatpak_scope() else {
        return Ok(());
    };
    let mut args = vec![scope.to_string(), "override".to_string()];
    args.extend(
        steam_filesystems(root)
            .into_iter()
            .map(|path| format!("--filesystem={path}:ro")),
    );
    args.push(STEAM_FLATPAK.to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if scope == "--system" {
        let mut with_flatpak = vec!["flatpak"];
        with_flatpak.extend(args);
        run("pkexec", &with_flatpak)
    } else {
        run("flatpak", &args)
    }
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

fn missing_filesystems(scope: &str, root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("flatpak")
        .args([scope, "override", "--show", STEAM_FLATPAK])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("running flatpak: {e}"))?;
    if !output.status.success() {
        return Err("Could not read the Steam flatpak's permissions.".to_string());
    }
    let have = filesystems(&String::from_utf8_lossy(&output.stdout));
    Ok(steam_filesystems(root)
        .into_iter()
        .filter(|path| !have.contains(path))
        .collect())
}

/// The `filesystems` of a flatpak override's [Context] group, without
/// `:ro`/`:rw` suffixes
fn filesystems(overrides: &str) -> Vec<String> {
    let mut context = false;
    for line in overrides.lines().map(str::trim) {
        if line.starts_with('[') {
            context = line == "[Context]";
        } else if context && let Some(value) = line.strip_prefix("filesystems=") {
            return value
                .split(';')
                .filter(|item| !item.is_empty())
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
    let (status, message) = match fs::OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => (
            Status::Ok,
            "The headset's keyboard and mouse can reach this PC.",
        ),
        Err(_) if !forwarding => (
            Status::Ok,
            "Keyboard and mouse forwarding is off, so /dev/uinput isn't needed.",
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => (
            Status::Warn,
            "/dev/uinput is missing, so the headset's keyboard and mouse don't reach this PC. Load the uinput kernel module.",
        ),
        Err(_) => (
            Status::Warn,
            "/dev/uinput isn't writable, so the headset's keyboard and mouse don't reach this PC. Installing steam-devices, or adding your user to the input group, fixes it.",
        ),
    };
    check("uinput", "Input forwarding", status, message.to_string())
}

pub fn usb(config: &Value) -> Check {
    let auto = config.get("usb-auto-connect").and_then(Value::as_bool) == Some(true);
    let (status, message) = match (auto, crate::which("adb").is_some()) {
        (false, _) => (Status::Ok, "Connecting by USB when plugged in is off."),
        (true, true) => (
            Status::Ok,
            "The headset connects when it's plugged in, if USB debugging is allowed.",
        ),
        (true, false) => (
            Status::Warn,
            "Connecting by USB when plugged in is on but adb isn't installed, install android-tools.",
        ),
    };
    check("usb", "USB", status, message.to_string())
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
        Some(Some((_, exe))) if Some(&exe) == ours.as_ref() => (
            Status::Ok,
            format!("LVRR's server is listening on port {port}."),
        ),
        Some(Some((pid, exe))) => (
            Status::Fail,
            format!("Port {port} is in use by {} (pid {pid}).", exe.display()),
        ),
        Some(None) => (
            Status::Fail,
            format!("Port {port} is in use by a program of another user."),
        ),
    };
    check("port", "Server port", status, message)
}

/// Who listens on TCP `port`: None when nobody, Some(None) for a process this
/// user can't see
fn listener(port: u16) -> Option<Option<(u32, PathBuf)>> {
    let inodes: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|table| fs::read_to_string(table).ok())
        .flat_map(|table| listening_inodes(&table, port))
        .collect();
    if inodes.is_empty() {
        return None;
    }
    Some(socket_owner(&inodes))
}

/// Socket inodes of a /proc/net/tcp table that listen on `port`
fn listening_inodes(table: &str, port: u16) -> Vec<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (_, local_port) = fields.get(1)?.rsplit_once(':')?;
            let listening = fields.get(3) == Some(&"0A");
            (listening && u16::from_str_radix(local_port, 16) == Ok(port))
                .then(|| fields.get(9).map(|inode| inode.to_string()))
                .flatten()
        })
        .collect()
}

fn socket_owner(inodes: &[String]) -> Option<(u32, PathBuf)> {
    let targets: Vec<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();
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

fn same_file(a: &Path, b: &Path) -> bool {
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

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
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
        assert_eq!(listening_inodes(table, 9757), ["41231"]);
        assert!(listening_inodes(table, 9758).is_empty());
    }

    #[test]
    fn flatpak_filesystems() {
        let overrides = "[Context]\nfilesystems=xdg-run/wivrn:ro;xdg-config/openxr;/opt/lvrr:ro;\n\n[Environment]\nfilesystems=nope\n";
        assert_eq!(
            filesystems(overrides),
            ["xdg-run/wivrn", "xdg-config/openxr", "/opt/lvrr"]
        );
        assert!(filesystems("[Environment]\nFOO=1\n").is_empty());
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
}
