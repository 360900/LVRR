//! The bindings a user chose for an app in SteamVR, so they carry over to xrizer.
//!
//! SteamVR saves the choice in `steamvr.vrsettings` in its config directory, in the section named
//! after the app key, as `<controller type>_250820_CurrentURL_<category>` (older versions leave
//! out `_250820`). The value is a `file://` URL or `vr-input-workshop://<item id>`, a Workshop
//! item of SteamVR's (app 250820), which Steam keeps as a JSON `<n>_legacy.bin` in
//! `<library>/steamapps/workshop/content/250820/<item id>/`. Choosing the default removes the key.

use crate::openxr_data::Hand;
use log::debug;
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// SteamVR's Steam app id, which owns the binding Workshop items.
const STEAMVR_APP_ID: &str = "250820";
/// Where Steam's pressure-vessel container mounts the host system.
const HOST_ROOT: &str = "/run/host";
/// Larger files aren't bindings.
const MAX_BINDINGS_SIZE: u64 = 16 << 20;
/// Workshop item directories hold one file; more means it isn't a binding item.
const MAX_ITEM_FILES: usize = 64;

/// Reads an environment variable.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The input API an app uses. SteamVR keeps a separate choice for each.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Category {
    /// Apps with an action manifest.
    SteamVrInput,
    /// Apps without one, whose input goes through SteamVR's legacy action manifest.
    Legacy,
}

impl Category {
    /// The end of the settings key.
    fn key_suffix(self) -> &'static str {
        match self {
            Self::SteamVrInput => "steamvrinput",
            Self::Legacy => "legacy",
        }
    }

    /// The `category` in a bindings file.
    fn file_category(self) -> &'static str {
        match self {
            Self::SteamVrInput => "steamvr_input",
            Self::Legacy => "legacy",
        }
    }
}

/// The keys SteamVR may know this app by, most specific first: what it identified as with
/// IdentifyApplication, the key SteamVR launched it with, and the ones of its Steam app id.
pub fn app_keys(env: Env, identified: Option<String>) -> Vec<String> {
    let mut keys: Vec<String> = identified.into_iter().collect();
    keys.extend(
        env("STEAMVR_APPKEY")
            .and_then(|key| key.into_string().ok())
            .filter(|key| !key.is_empty()),
    );
    // Steam sets both for its games. Other launches get a 64 bit game id or 0.
    let app_id = ["SteamAppId", "SteamGameId"].into_iter().find_map(|name| {
        env(name)?
            .to_str()?
            .parse::<u32>()
            .ok()
            .filter(|id| *id != 0)
    });
    if let Some(id) = app_id {
        keys.push(format!("steam.app.{id}"));
        keys.push(format!("steam.overlay.{id}"));
    }
    keys
}

/// What SteamVR saved for an app.
pub struct SavedBindings {
    /// The key of the app's settings section.
    app_key: String,
    section: Map<String, Value>,
    /// Steam installs, where SteamVR's Workshop items may be.
    steam_roots: Vec<PathBuf>,
}

impl SavedBindings {
    /// The settings SteamVR has for the first of `app_keys` it knows.
    pub fn load(env: Env, app_keys: &[String]) -> Option<Self> {
        if app_keys.is_empty() {
            return None;
        }
        let home = env("HOME").map(PathBuf::from);
        let mut steam_roots = steam_roots(env, home.as_deref());
        let settings = config_dirs(env, home.as_deref(), &steam_roots)
            .into_iter()
            .map(|dir| host_path(&dir.join("steamvr.vrsettings")))
            .find(|path| path.is_file())?;
        let data = std::fs::read(&settings)
            .inspect_err(|e| debug!("Can't read {}: {e}", settings.display()))
            .ok()?;
        let Ok(Value::Object(mut sections)) = serde_json::from_slice(&data) else {
            debug!("{} isn't a settings file", settings.display());
            return None;
        };
        let (app_key, section) = app_keys.iter().find_map(|key| {
            let name = sections
                .keys()
                .find(|name| *name == key)
                .or_else(|| sections.keys().find(|name| name.eq_ignore_ascii_case(key)))?
                .clone();
            match sections.remove(&name)? {
                Value::Object(section) => Some((name, section)),
                _ => None,
            }
        })?;
        debug!("SteamVR settings for {app_key} in {}", settings.display());

        // The Steam install SteamVR ran from, usually.
        if let Some(root) = settings
            .parent()
            .filter(|dir| dir.file_name().is_some_and(|name| name == "config"))
            .and_then(Path::parent)
        {
            push_dir(&mut steam_roots, root.to_path_buf());
        }
        Some(Self {
            app_key,
            section,
            steam_roots,
        })
    }

    /// The dominant hand chosen for the app, SteamVR's `DominantHand` controller role.
    pub fn dominant_hand(&self) -> Option<Hand> {
        match self.section.get("DominantHand")?.as_i64()? {
            1 => Some(Hand::Left),
            2 => Some(Hand::Right),
            _ => None,
        }
    }

    /// The bindings file chosen for `controller_type`, if the user chose one that can be loaded.
    /// SteamVR falls back to the default too when the chosen one fails to load.
    pub fn file_for(&self, controller_type: &str, category: Category) -> Option<PathBuf> {
        let url = self.choice(controller_type, category)?;
        let file = match parse_url(url) {
            Some(SavedUrl::File(paths)) => paths
                .into_iter()
                .map(|path| host_path(&path))
                .find(|path| self.check(path, controller_type, category, false).is_some()),
            Some(SavedUrl::Workshop(item)) => self.workshop_file(&item, controller_type, category),
            None => None,
        };
        match &file {
            Some(path) => crate::log_unique!(
                log::Level::Info,
                "Using the {controller_type} bindings chosen in SteamVR for {}: {}",
                self.app_key,
                path.display()
            ),
            None => crate::log_unique!(
                log::Level::Warn,
                "Can't load the {controller_type} bindings chosen in SteamVR for {} ({url}), using the default",
                self.app_key
            ),
        }
        file
    }

    /// The URL saved for `controller_type`. Current SteamVR puts its app id in the key.
    fn choice(&self, controller_type: &str, category: Category) -> Option<&str> {
        let suffix = format!("_CurrentURL_{}", category.key_suffix());
        let current = format!("_{STEAMVR_APP_ID}");
        self.section
            .iter()
            .filter_map(|(key, value)| {
                let middle = key.strip_prefix(controller_type)?.strip_suffix(&suffix)?;
                let rank = if middle == current {
                    0
                } else if middle.is_empty() {
                    1
                } else if middle
                    .strip_prefix('_')
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
                {
                    2
                } else {
                    // Another controller type, e.g. knuckles_ev1 for knuckles.
                    return None;
                };
                let url = value.as_str().filter(|url| !url.trim().is_empty())?;
                Some((rank, key, url))
            })
            .min()
            .map(|(_, _, url)| url)
    }

    /// The newest file of a Workshop item with bindings for `controller_type`.
    fn workshop_file(
        &self,
        item: &str,
        controller_type: &str,
        category: Category,
    ) -> Option<PathBuf> {
        self.libraries().into_iter().find_map(|library| {
            let dir = host_path(
                &library
                    .join("steamapps/workshop/content")
                    .join(STEAMVR_APP_ID)
                    .join(item),
            );
            std::fs::read_dir(&dir)
                .ok()?
                .filter_map(Result::ok)
                .take(MAX_ITEM_FILES)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension().is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("bin") || ext.eq_ignore_ascii_case("json")
                    })
                })
                .filter_map(|path| {
                    let modified = self.check(&path, controller_type, category, true)?;
                    Some((modified, path))
                })
                .max()
                .map(|(_, path)| path)
        })
    }

    /// When `path` was written, if it holds `controller_type` bindings SteamVR would load for
    /// this app. Workshop files must name their controller type, like SteamVR requires.
    fn check(
        &self,
        path: &Path,
        controller_type: &str,
        category: Category,
        workshop: bool,
    ) -> Option<SystemTime> {
        let metadata = std::fs::metadata(path)
            .ok()
            .filter(|data| data.is_file() && data.len() <= MAX_BINDINGS_SIZE)?;
        let data: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let field = |name| data.get(name).and_then(Value::as_str);
        let matches = data.get("bindings").is_some_and(Value::is_object)
            && match field("controller_type") {
                Some(ty) => ty.eq_ignore_ascii_case(controller_type),
                None => !workshop,
            }
            && field("category").is_none_or(|name| name == category.file_category())
            // Developer bindings use generated app keys (e.g. system.generated.hlvr.exe).
            && (!workshop || field("app_key").is_none_or(|key| key.eq_ignore_ascii_case(&self.app_key)));
        if !matches {
            debug!(
                "{} isn't {controller_type} bindings for {}",
                path.display(),
                self.app_key
            );
            return None;
        }
        Some(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))
    }

    /// The Steam library folders.
    fn libraries(&self) -> Vec<PathBuf> {
        let mut libraries = Vec::new();
        for root in &self.steam_roots {
            push_dir(&mut libraries, root.clone());
            let vdf = root.join("steamapps/libraryfolders.vdf");
            if let Ok(vdf) = std::fs::read_to_string(&vdf) {
                for library in library_folders(&vdf) {
                    push_dir(&mut libraries, library);
                }
            }
        }
        libraries
    }
}

/// The directories SteamVR may keep its settings in, in the order openvr_api looks for them,
/// then the Steam installs' config directories it uses by default.
fn config_dirs(env: Env, home: Option<&Path>, steam_roots: &[PathBuf]) -> Vec<PathBuf> {
    let non_empty = |name| env(name).filter(|value: &OsString| !value.is_empty());
    let mut dirs: Vec<PathBuf> = non_empty("VR_CONFIG_PATH")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    let vrpath = non_empty("VR_PATHREG_OVERRIDE")
        .map(PathBuf::from)
        .or_else(|| {
            non_empty("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| Some(home?.join(".config")))
                .map(|dir| dir.join("openvr/openvrpaths.vrpath"))
        });
    if let Some(vrpath) = vrpath
        && let Ok(data) = std::fs::read(&vrpath)
        && let Ok(paths) = serde_json::from_slice::<Value>(&data)
    {
        dirs.extend(
            paths["config"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(PathBuf::from),
        );
    }
    dirs.extend(steam_roots.iter().map(|root| root.join("config")));
    dirs
}

/// The SteamVR install in the Steam libraries, for the files it ships (render models).
pub(crate) fn steamvr_install(env: Env) -> Option<PathBuf> {
    let home = env("HOME").map(PathBuf::from);
    let mut libraries = Vec::new();
    for root in steam_roots(env, home.as_deref()) {
        if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            for library in library_folders(&vdf) {
                push_dir(&mut libraries, library);
            }
        }
        push_dir(&mut libraries, root);
    }
    libraries
        .into_iter()
        .map(|library| host_path(&library.join("steamapps/common/SteamVR")))
        .find(|dir| dir.is_dir())
}

/// The Steam installs: the one running the game, then the usual places.
fn steam_roots(env: Env, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let candidates = env("STEAM_COMPAT_CLIENT_INSTALL_PATH")
        .map(PathBuf::from)
        .into_iter()
        .chain(home.into_iter().flat_map(|home| {
            [".local/share/Steam", ".steam/steam", ".steam/root"].map(|dir| home.join(dir))
        }));
    for root in candidates {
        push_dir(&mut roots, root);
    }
    roots
}

/// Adds `dir` to `dirs` if it exists and isn't in it yet under another name.
fn push_dir(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if let Ok(dir) = host_path(&dir).canonicalize()
        && dir.is_dir()
        && !dirs.contains(&dir)
    {
        dirs.push(dir);
    }
}

/// `path`, or where the pressure-vessel container has it if only the host does (Steam libraries
/// outside the home directory).
fn host_path(path: &Path) -> PathBuf {
    if !path.exists()
        && let Ok(relative) = path.strip_prefix("/")
    {
        let host = Path::new(HOST_ROOT).join(relative);
        if host.exists() {
            return host;
        }
    }
    path.to_path_buf()
}

/// A saved bindings URL.
#[derive(Debug, PartialEq)]
enum SavedUrl {
    /// Spellings of a local file, most likely first.
    File(Vec<PathBuf>),
    /// A Workshop item id.
    Workshop(String),
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then_some(&s[prefix.len()..])
}

fn parse_url(url: &str) -> Option<SavedUrl> {
    let url = url.trim();
    if let Some(rest) = strip_prefix_ignore_case(url, "file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        let bytes = path.as_bytes();
        // Windows paths (file:///C:/...) are from SteamVR on Windows.
        if !path.starts_with('/') || (bytes.get(2) == Some(&b':') && bytes[1].is_ascii_alphabetic())
        {
            return None;
        }
        // Spaces are usually escaped, but a path may also contain a literal %.
        let mut paths: Vec<PathBuf> = percent_decode(path)
            .map(|path| OsString::from_vec(path).into())
            .into_iter()
            .collect();
        if !paths.iter().any(|decoded| decoded.as_os_str() == path) {
            paths.push(path.into());
        }
        return Some(SavedUrl::File(paths));
    }
    let item = strip_prefix_ignore_case(url, "vr-input-workshop://")?.trim_end_matches('/');
    (!item.is_empty() && item.len() <= 20 && item.bytes().all(|b| b.is_ascii_digit()))
        .then(|| SavedUrl::Workshop(item.to_owned()))
}

/// Decodes %XX escapes, if they all are valid.
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(byte);
        }
    }
    Some(out)
}

/// The library folders a libraryfolders.vdf lists: `"path"` in each entry, or in the old format
/// the value of each numbered key.
fn library_folders(vdf: &str) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let mut depth = 0usize;
    let mut key: Option<String> = None;
    for token in vdf_tokens(vdf) {
        match token {
            VdfToken::Open => {
                depth += 1;
                key = None;
            }
            VdfToken::Close => {
                depth = depth.saturating_sub(1);
                key = None;
            }
            VdfToken::Text(text) => match key.take() {
                None => key = Some(text),
                Some(name) => {
                    let numbered = !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit());
                    if (depth == 2 && name.eq_ignore_ascii_case("path")) || (depth == 1 && numbered)
                    {
                        folders.push(PathBuf::from(text));
                    }
                }
            },
        }
    }
    folders
}

enum VdfToken {
    Open,
    Close,
    Text(String),
}

/// Splits Valve's KeyValues text format into braces and (quoted or bare) strings.
fn vdf_tokens(vdf: &str) -> Vec<VdfToken> {
    let mut tokens = Vec::new();
    let mut chars = vdf.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => tokens.push(VdfToken::Open),
            '}' => tokens.push(VdfToken::Close),
            '"' => {
                let mut text = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => text.push('\n'),
                            Some('t') => text.push('\t'),
                            Some(other) => text.push(other),
                            None => break,
                        },
                        _ => text.push(c),
                    }
                }
                tokens.push(VdfToken::Text(text));
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            c => {
                let mut text = String::from(c);
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || matches!(next, '{' | '}' | '"') {
                        break;
                    }
                    text.push(next);
                    chars.next();
                }
                tokens.push(VdfToken::Text(text));
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.into()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn urls() {
        let file = |paths: &[&str]| Some(SavedUrl::File(paths.iter().map(PathBuf::from).collect()));
        assert_eq!(
            parse_url("file:///home/user/My%20Bindings/knuckles.json"),
            file(&[
                "/home/user/My Bindings/knuckles.json",
                "/home/user/My%20Bindings/knuckles.json"
            ])
        );
        assert_eq!(
            parse_url("file://localhost/home/user/100%.json"),
            file(&["/home/user/100%.json"])
        );
        assert_eq!(parse_url("FILE:///a b.json"), file(&["/a b.json"]));
        assert_eq!(parse_url("file:///C:/Program%20Files/x.json"), None);
        assert_eq!(parse_url("file://relative.json"), None);
        for url in [
            "vr-input-workshop://3822741892",
            "vr-input-workshop://3822741892/",
        ] {
            assert_eq!(
                parse_url(url),
                Some(SavedUrl::Workshop("3822741892".into()))
            );
        }
        for url in [
            "",
            "vr-input-workshop://",
            "vr-input-workshop://12ab",
            "vr-input-workshop://12/../3",
            "https://example.com/bindings.json",
        ] {
            assert_eq!(parse_url(url), None, "{url}");
        }
    }

    #[test]
    fn library_folders_formats() {
        let current = r#""libraryfolders"
{
	"0"
	{
		"path"		"/home/user/.local/share/Steam"
		"label"		""
		"apps"
		{
			"4000"		"7279080945"
		}
	}
	"1"
	{
		"path"		"/mnt/games/Steam \"Library\""
		"apps"
		{
			"250820"		"5636124391"
		}
	}
}"#;
        assert_eq!(
            library_folders(current),
            [
                PathBuf::from("/home/user/.local/share/Steam"),
                PathBuf::from("/mnt/games/Steam \"Library\"")
            ]
        );
        let old = r#""LibraryFolders"
{
	// a comment
	"TimeNextStatsReport"		"1600000000"
	"ContentStatsID"		"-1234"
	"1"		"/mnt/games/SteamLibrary"
}"#;
        assert_eq!(
            library_folders(old),
            [PathBuf::from("/mnt/games/SteamLibrary")]
        );
    }

    #[test]
    fn keys_of_the_app() {
        let env = env_of(&[("SteamAppId", "438100"), ("SteamGameId", "438100")]);
        assert_eq!(
            app_keys(&env, Some("org.example.app".into())),
            [
                "org.example.app",
                "steam.app.438100",
                "steam.overlay.438100"
            ]
        );
        // A non-Steam shortcut.
        let env = env_of(&[
            ("SteamAppId", "0"),
            ("SteamGameId", "13510798882111733760"),
            ("STEAMVR_APPKEY", "system.generated.game.exe"),
        ]);
        assert_eq!(app_keys(&env, None), ["system.generated.game.exe"]);
    }

    /// The key the current SteamVR writes wins over the older one, and other controller types
    /// and categories don't count.
    #[test]
    fn choice_keys() {
        let section = serde_json::json!({
            "knuckles_ev1_CurrentURL_steamvrinput": "file:///ev1.json",
            "knuckles_CurrentURL_steamvrinput": "file:///old.json",
            "knuckles_250820_CurrentURL_legacy": "file:///legacy.json",
            "knuckles_250820_AutosaveURL_steamvrinput": "vr-input-workshop://1",
            "knuckles_250820_CurrentURL_steamvrinput": "file:///current.json",
            "oculus_touch_CurrentURL_steamvrinput": "",
        });
        let saved = SavedBindings {
            app_key: "steam.app.1".into(),
            section: section.as_object().unwrap().clone(),
            steam_roots: Vec::new(),
        };
        let choice = |ty| saved.choice(ty, Category::SteamVrInput);
        assert_eq!(choice("knuckles"), Some("file:///current.json"));
        assert_eq!(choice("knuckles_ev1"), Some("file:///ev1.json"));
        assert_eq!(choice("oculus_touch"), None);
        assert_eq!(choice("vive_controller"), None);
        assert_eq!(
            saved.choice("knuckles", Category::Legacy),
            Some("file:///legacy.json")
        );
    }

    /// SteamVR's config directory named in openvrpaths.vrpath, and a Workshop item with an older
    /// file and one for another app.
    #[test]
    fn workshop_item_from_vrpath_config() {
        let dir =
            std::env::temp_dir().join(format!("xrizer-saved-bindings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let steam = dir.join("steam");
        let config = steam.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(dir.join("xdg/openvr")).unwrap();
        std::fs::write(
            dir.join("xdg/openvr/openvrpaths.vrpath"),
            format!(
                r#"{{"config": ["{}"], "runtime": ["/opt/xrizer"], "version": 1}}"#,
                config.display()
            ),
        )
        .unwrap();
        std::fs::write(
            config.join("steamvr.vrsettings"),
            r#"{"Steam.App.42": {"oculus_touch_250820_CurrentURL_steamvrinput": "vr-input-workshop://99", "DominantHand": 1}}"#,
        )
        .unwrap();
        let item = steam.join("steamapps/workshop/content/250820/99");
        std::fs::create_dir_all(&item).unwrap();
        let file = |name: &str, app_key: &str, secs| {
            let path = item.join(name);
            std::fs::write(
                &path,
                format!(
                    r#"{{"app_key": "{app_key}", "controller_type": "oculus_touch", "category": "steamvr_input", "bindings": {{}}}}"#
                ),
            )
            .unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
            path
        };
        file("1_legacy.bin", "steam.app.42", 1000);
        let newest = file("3_legacy.bin", "steam.app.42", 2000);
        file("2_legacy.bin", "steam.app.7", 3000);
        std::fs::write(item.join("readme.txt"), "").unwrap();

        let env = env_of(&[("XDG_CONFIG_HOME", dir.join("xdg").to_str().unwrap())]);
        let saved = SavedBindings::load(&env, &["steam.app.42".into()]);
        let touch = saved
            .as_ref()
            .and_then(|saved| saved.file_for("oculus_touch", Category::SteamVrInput));
        let knuckles = saved
            .as_ref()
            .and_then(|saved| saved.file_for("knuckles", Category::SteamVrInput));
        let dominant_hand = saved.as_ref().and_then(SavedBindings::dominant_hand);
        let other_app = SavedBindings::load(&env, &["steam.app.43".into()]).is_some();
        let newest = newest.canonicalize().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(touch, Some(newest));
        assert_eq!(knuckles, None);
        assert_eq!(dominant_hand, Some(Hand::Left));
        assert!(!other_app);
    }
}
