//! Bindings that replace an app's own: custom binding files, then the bindings chosen for the app
//! in SteamVR.

use super::action_manifest::{ControllerType, env_var, isolated_env_var};
use super::steamvr_bindings::{self, Category, SavedBindings};
use std::cell::RefCell;
use std::env::current_dir;
use std::path::{Path, PathBuf};

/// Where the bindings replacing an app's own come from, first match wins:
/// 1. `XRIZER_CUSTOM_BINDINGS_DIR`
/// 2. `$XDG_CONFIG_HOME/xrizer/bindings/<app key>`, for each key the app may have
/// 3. `xrizer` in the working directory, unless `XRIZER_CUSTOM_BINDINGS_DIR` is set
/// 4. the bindings chosen for the app in SteamVR
///
/// The per-app directory goes before the working directory: it names the app exactly, while
/// apps can share a working directory, and a tool editing it can't know about a working
/// directory file that would hide its changes.
pub(super) struct BindingSources {
    /// Directories with custom `<controller type>.json` files, first match wins.
    dirs: Vec<PathBuf>,
    saved: Option<SavedBindings>,
    category: Category,
    /// The file chosen for each controller type asked about, see [Self::into_choices].
    chosen: RefCell<Vec<(ControllerType, Option<PathBuf>)>>,
}

impl BindingSources {
    /// The sources for the app that identified as `app_key`, if it did.
    pub fn new(app_key: Option<String>, category: Category) -> Self {
        let keys = steamvr_bindings::app_keys(&isolated_env_var, app_key);
        let custom = env_var("XRIZER_CUSTOM_BINDINGS_DIR").map(PathBuf::from);
        let mut dirs: Vec<PathBuf> = custom.iter().cloned().collect();
        if let Some(config) = config_dir() {
            for name in keys.iter().filter_map(|key| key_dir_name(key)) {
                let dir = config.join(name);
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
        if custom.is_none() {
            dirs.push(current_dir().unwrap_or_default().join("xrizer"));
        }
        Self {
            dirs,
            saved: steamvr_saved_bindings(&keys),
            category,
            chosen: Default::default(),
        }
    }

    /// The file whose bindings replace the app's own for `controller_type`, if there is one.
    pub fn file_for(&self, controller_type: &ControllerType) -> Option<PathBuf> {
        let file = self.custom_file(controller_type).or_else(|| {
            self.saved
                .as_ref()?
                .file_for(&controller_type.to_string(), self.category)
        });
        let mut chosen = self.chosen.borrow_mut();
        if !chosen.iter().any(|(ty, _)| ty == controller_type) {
            chosen.push((controller_type.clone(), file.clone()));
        }
        file
    }

    fn custom_file(&self, controller_type: &ControllerType) -> Option<PathBuf> {
        let name = format!("{controller_type}.json");
        // The custom bindings of an app that loads an action manifest after starting on legacy
        // input aren't legacy bindings.
        let path = self.dirs.iter().map(|dir| dir.join(&name)).find(|path| {
            path.is_file() && (self.category != Category::Legacy || has_legacy_sets(path))
        })?;
        crate::log_unique!(
            log::Level::Info,
            "Using custom {controller_type} bindings: {}",
            path.display()
        );
        Some(path)
    }

    /// The files chosen for the controller types asked about.
    pub fn into_choices(self) -> BindingChoices {
        BindingChoices {
            category: self.category,
            files: self.chosen.into_inner(),
            emulated: None,
        }
    }
}

/// The files that replaced an app's own bindings, to tell whether another app key changes them.
#[derive(Debug)]
pub(super) struct BindingChoices {
    category: Category,
    files: Vec<(ControllerType, Option<PathBuf>)>,
    /// The controller chosen for the game to see, see [super::emulation].
    pub emulated: Option<ControllerType>,
}

impl BindingChoices {
    /// Whether the app would get other bindings as `app_key`.
    pub fn changed_for(&self, app_key: &str) -> bool {
        // Legacy input doesn't emulate controllers.
        if self.category == Category::SteamVrInput
            && super::emulation::emulated_controller(Some(app_key.to_owned())) != self.emulated
        {
            return true;
        }
        let sources = BindingSources::new(Some(app_key.to_owned()), self.category);
        self.files
            .iter()
            .any(|(controller_type, file)| sources.file_for(controller_type) != *file)
    }
}

/// Whether the bindings file at `path` has bindings for SteamVR's legacy action sets.
fn has_legacy_sets(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let Ok(serde_json::Value::Object(file)) = serde_json::from_slice(&data) else {
        return false;
    };
    file.get("bindings")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|sets| {
            sets.keys().any(|set| {
                set.eq_ignore_ascii_case("/actions/legacy")
                    || set.eq_ignore_ascii_case("/actions/legacy_mirrored")
            })
        })
}

/// The bindings the user chose for this app in SteamVR, unless XRIZER_STEAMVR_BINDINGS=0.
fn steamvr_saved_bindings(keys: &[String]) -> Option<SavedBindings> {
    if env_var("XRIZER_STEAMVR_BINDINGS").is_some_and(|value| value == "0") {
        return None;
    }
    SavedBindings::load(&isolated_env_var, keys)
}

/// Where per-app custom bindings are kept.
fn config_dir() -> Option<PathBuf> {
    Some(xrizer_config_dir()?.join("bindings"))
}

/// `$XDG_CONFIG_HOME/xrizer`.
pub(super) fn xrizer_config_dir() -> Option<PathBuf> {
    let non_empty = |name| isolated_env_var(name).filter(|value| !value.is_empty());
    let config = non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(Path::new(&non_empty("HOME")?).join(".config")))?;
    Some(config.join("xrizer"))
}

/// The directory named after an app key: lowercase, as SteamVR matches keys ignoring case, with
/// characters other than letters, digits, `.`, `_` and `-` replaced by `_`.
pub(super) fn key_dir_name(key: &str) -> Option<String> {
    let name: String = key
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '.' | '_' | '-') => c,
            _ => '_',
        })
        .collect();
    name.bytes().any(|b| b != b'.').then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_dir_names() {
        let name = |key| key_dir_name(key);
        assert_eq!(
            name("steam.app.611670").as_deref(),
            Some("steam.app.611670")
        );
        assert_eq!(
            name("Org.JRBudda.Vivecraft.SteamVRInput").as_deref(),
            Some("org.jrbudda.vivecraft.steamvrinput")
        );
        assert_eq!(
            name("system.generated.My Game/../x.exe").as_deref(),
            Some("system.generated.my_game_.._x.exe")
        );
        assert_eq!(name("ünï").as_deref(), Some("_n_"));
        for key in ["", ".", ".."] {
            assert_eq!(name(key), None, "{key:?}");
        }
    }
}
