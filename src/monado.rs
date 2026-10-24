use libmonado::{Device, DeviceRole, Monado};
use log::{debug, info};
use std::path::{Path, PathBuf};

use openvr as vr;

/// Where Steam's pressure-vessel container mounts the host system.
const HOST_ROOT: &str = "/run/host";

pub struct SafeMonado(pub Monado);

unsafe impl Send for SafeMonado {}
unsafe impl Sync for SafeMonado {}

impl SafeMonado {
    pub fn safe_connect() -> Option<SafeMonado> {
        let monado = match Monado::auto_connect().or_else(connect_from_container) {
            Ok(m) => m,
            Err(e) => {
                // Tried again for every session.
                crate::log_unique!(log::Level::Warn, "Failed to connect to Monado: {e}");
                return None;
            }
        };

        info!("Connected to Monado! Version: {}", monado.get_api_version());
        Some(SafeMonado(monado))
    }

    pub fn get_device_from_vr_index(&self, device_index: u32) -> Option<Device<'_>> {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => self.0.device_from_role(DeviceRole::Head).ok(),
            // Controllers are pre-created at the fixed OpenVR indices 1 (left)
            // and 2 (right); see TrackedDeviceList::new.
            1 => self.0.device_from_role(DeviceRole::Left).ok(),
            2 => self.0.device_from_role(DeviceRole::Right).ok(),
            _ => None,
        }
    }
}

/// pressure-vessel gives games a copy of the runtime JSON without `MND_libmonado_path`, and the
/// host's own JSON is usually a link into the host's /usr, which the container has under
/// [HOST_ROOT]. So libmonado is looked up through the host's JSON there.
fn connect_from_container(error: String) -> Result<Monado, String> {
    let host = Path::new(HOST_ROOT);
    if !host.is_dir() || std::env::var_os("LIBMONADO_PATH").is_some() {
        return Err(error);
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")));
    let Some(path) = host_libmonado_path(host, config_home.as_deref()) else {
        return Err(error);
    };
    debug!("Using the host's libmonado at {}", path.display());
    Monado::create(&path).map_err(|e| format!("{error}, and {} failed: {e:?}", path.display()))
}

/// The libmonado named by the host's active runtime JSON, as a path under `host`. The user's
/// config directory is shared with the container.
fn host_libmonado_path(host: &Path, config_home: Option<&Path>) -> Option<PathBuf> {
    const JSON: &str = "openxr/1/active_runtime.json";
    let candidates = config_home
        .map(|dir| dir.join(JSON))
        .into_iter()
        .chain(["etc/xdg", "etc"].map(|dir| host.join(dir).join(JSON)));
    candidates.into_iter().find_map(|json| {
        let json = resolve_host_links(host, &json)?;
        let contents: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).ok()?).ok()?;
        let lib = Path::new(contents["runtime"]["MND_libmonado_path"].as_str()?);
        let path = if lib.is_absolute() {
            in_host(host, lib)
        } else if lib.components().count() > 1 {
            json.parent()?.join(lib)
        } else {
            // A bare name is found through the host's library search path.
            ["usr/lib", "usr/lib64", "usr/lib/x86_64-linux-gnu"]
                .into_iter()
                .map(|dir| host.join(dir).join(lib))
                .find(|path| path.exists())?
        };
        path.canonicalize().ok().filter(|path| path.is_file())
    })
}

/// Follows the symlinks of `path`, whose absolute targets are host paths.
fn resolve_host_links(host: &Path, path: &Path) -> Option<PathBuf> {
    let mut path = path.to_path_buf();
    // Like the kernel's limit on nested links.
    for _ in 0..40 {
        let Ok(target) = path.read_link() else {
            return path.exists().then_some(path);
        };
        path = if target.is_absolute() {
            in_host(host, &target)
        } else {
            path.parent()?.join(target)
        };
    }
    None
}

/// The host's `path` under `host`, unless the container has it too (e.g. the shared home).
fn in_host(host: &Path, path: &Path) -> PathBuf {
    let mapped = host.join(path.strip_prefix("/").unwrap_or(path));
    if mapped.exists() || !path.exists() {
        mapped
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::host_libmonado_path;
    use std::os::unix::fs::symlink;

    /// The user's active runtime JSON links to the runtime's JSON in the host's /usr, which names
    /// libmonado relative to itself, like WiVRn's.
    #[test]
    fn libmonado_through_host_json() {
        let dir =
            std::env::temp_dir().join(format!("xrizer-libmonado-test-{}", std::process::id()));
        let host = dir.join("host");
        let config = dir.join("config");
        let share = host.join("usr/share/openxr/1");
        let lib = host.join("usr/lib/wivrn/libmonado_wivrn.so");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::create_dir_all(config.join("openxr/1")).unwrap();
        std::fs::write(&lib, "").unwrap();
        std::fs::write(
            share.join("openxr_wivrn.json"),
            r#"{"runtime": {"library_path": "x", "MND_libmonado_path": "../../../lib/wivrn/libmonado_wivrn.so"}}"#,
        )
        .unwrap();
        symlink(
            "/usr/share/openxr/1/openxr_wivrn.json",
            config.join("openxr/1/active_runtime.json"),
        )
        .unwrap();

        let lib = lib.canonicalize().unwrap();
        let found = host_libmonado_path(&host, Some(&config));
        // pressure-vessel's copy, which wins over the user's JSON in the container, has no path.
        let without_user_json = host_libmonado_path(&host, None);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Some(lib));
        assert_eq!(without_user_json, None);
    }
}
