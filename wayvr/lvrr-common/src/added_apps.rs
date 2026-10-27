//! Apps the user added to the Library. WiVRn lists desktop entries in the `X-WiVRn-VR` category,
//! so adding one writes a hidden entry in that category to `$XDG_DATA_HOME/applications`.
//! Steam games get `<app id>.desktop`, which keeps the Steam app id as their id.

use std::{
	collections::HashSet,
	fs,
	path::{Path, PathBuf},
};

const CATEGORY: &str = "X-WiVRn-VR";
/// Only entries with this line are removed
const MARKER: &str = "X-LVRR-Added=true";
/// Prefix of the copied desktop entries
const PREFIX: &str = "lvrr-";

fn non_empty(name: &str) -> Option<std::ffi::OsString> {
	std::env::var_os(name).filter(|v| !v.is_empty())
}

fn data_home() -> Option<PathBuf> {
	non_empty("XDG_DATA_HOME")
		.map(PathBuf::from)
		.filter(|d| d.is_absolute())
		.or_else(|| Some(Path::new(&non_empty("HOME")?).join(".local").join("share")))
}

fn data_dirs() -> Vec<PathBuf> {
	let dirs = non_empty("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
	data_home()
		.into_iter()
		.chain(std::env::split_paths(&dirs).filter(|d| d.is_absolute()))
		.collect()
}

fn applications_dir() -> anyhow::Result<PathBuf> {
	Ok(
		data_home()
			.ok_or_else(|| anyhow::anyhow!("no home directory"))?
			.join("applications"),
	)
}

/// Ids of the apps the user added
pub fn added() -> HashSet<String> {
	applications_dir().map(|dir| added_in(&dir)).unwrap_or_default()
}

fn added_in(dir: &Path) -> HashSet<String> {
	let Ok(entries) = fs::read_dir(dir) else {
		return HashSet::new();
	};
	entries
		.flatten()
		.filter_map(|entry| {
			let path = entry.path();
			let id = path.file_stem()?.to_str()?.to_owned();
			let ours = path.extension().is_some_and(|e| e == "desktop")
				&& fs::read_to_string(&path).is_ok_and(|s| s.lines().any(|l| l.trim() == MARKER));
			ours.then_some(id)
		})
		.collect()
}

/// Adds an installed Steam game
pub fn add_steam(app_id: u64, name: &str) -> anyhow::Result<()> {
	write(&applications_dir()?, &app_id.to_string(), &steam_entry(app_id, name))
}

/// Adds a copy of a desktop entry, returns the copy's id
pub fn add_desktop(id: &str) -> anyhow::Result<String> {
	let original = find_entry(id).ok_or_else(|| anyhow::anyhow!("no desktop entry {id}"))?;
	let new_id = format!("{PREFIX}{id}");
	write(&applications_dir()?, &new_id, &vr_entry(&fs::read_to_string(original)?))?;
	Ok(new_id)
}

/// Removes an app the user added
pub fn remove(id: &str) -> anyhow::Result<()> {
	remove_in(&applications_dir()?, id)
}

fn remove_in(dir: &Path, id: &str) -> anyhow::Result<()> {
	anyhow::ensure!(added_in(dir).contains(id), "{id} was not added by LVRR");
	fs::remove_file(dir.join(format!("{id}.desktop")))?;
	Ok(())
}

fn write(dir: &Path, id: &str, content: &str) -> anyhow::Result<()> {
	let path = dir.join(format!("{id}.desktop"));
	anyhow::ensure!(!path.exists(), "{} already exists", path.display());
	fs::create_dir_all(dir)?;
	fs::write(path, content)?;
	Ok(())
}

/// The desktop file with this id, see the desktop entry spec's file naming
fn find_entry(id: &str) -> Option<PathBuf> {
	fn walk(root: &Path, dir: &Path, id: &str) -> Option<PathBuf> {
		for entry in fs::read_dir(dir).ok()?.flatten() {
			let path = entry.path();
			if path.is_dir() {
				if let Some(found) = walk(root, &path, id) {
					return Some(found);
				}
			} else if path.extension().is_some_and(|e| e == "desktop")
				&& let Ok(relative) = path.with_extension("").strip_prefix(root)
				&& relative.to_string_lossy().replace('/', "-") == id
			{
				return Some(path);
			}
		}
		None
	}
	data_dirs().iter().find_map(|dir| {
		let dir = dir.join("applications");
		walk(&dir, &dir, id)
	})
}

fn escape(value: &str) -> String {
	value.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r")
}

fn steam_entry(app_id: u64, name: &str) -> String {
	format!(
		"[Desktop Entry]\nType=Application\nName={}\nExec=xdg-open steam://rungameid/{app_id}\nIcon=steam_icon_{app_id}\nCategories=Game;{CATEGORY};\nNoDisplay=true\n{MARKER}\n",
		escape(name)
	)
}

/// The server passes Exec's arguments as they are, so `%U` would reach the app
fn strip_field_codes(exec: &str) -> String {
	let mut out = String::new();
	let mut chars = exec.chars().peekable();
	while let Some(c) = chars.next() {
		match (c, chars.peek()) {
			('%', Some('%')) => {
				out.push('%');
				chars.next();
			}
			('%', Some(_)) => {
				chars.next();
			}
			_ => out.push(c),
		}
	}
	out.trim_end().to_owned()
}

/// The main section of a desktop entry, in the VR category and hidden from menus
fn vr_entry(original: &str) -> String {
	const DROPPED: &[&str] = &[
		"NoDisplay",
		"Hidden",
		"OnlyShowIn",
		"NotShowIn",
		"Actions",
		"DBusActivatable",
	];
	let mut out = String::new();
	let mut main = false;
	let mut categories = false;
	for line in original.lines() {
		let trimmed = line.trim();
		if trimmed.starts_with('[') {
			if main {
				break;
			}
			main = trimmed == "[Desktop Entry]";
			if main {
				out.push_str("[Desktop Entry]\n");
			}
			continue;
		}
		if !main || trimmed == MARKER {
			continue;
		}
		let key = trimmed.split(['=', '[']).next().unwrap_or_default().trim();
		if DROPPED.contains(&key) {
			continue;
		}
		if key == "Categories" && !trimmed.contains('[') {
			let value = trimmed.split_once('=').map_or("", |(_, v)| v.trim());
			let value = if value.is_empty() || value.ends_with(';') {
				value.to_owned()
			} else {
				format!("{value};")
			};
			out.push_str(&format!("Categories={value}{CATEGORY};\n"));
			categories = true;
			continue;
		}
		if key == "Exec" {
			out.push_str(&strip_field_codes(line));
		} else {
			out.push_str(line);
		}
		out.push('\n');
	}
	if !categories {
		out.push_str(&format!("Categories={CATEGORY};\n"));
	}
	out.push_str(&format!("NoDisplay=true\n{MARKER}\n"));
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn copies_main_section_into_vr_category() {
		let original = "# comment\n[Desktop Entry]\nName=Foo\nName[de]=Föö\nExec=foo %U\nCategories=Game\nNoDisplay=false\nActions=new;\n\n[Desktop Action new]\nExec=foo --new\n";
		assert_eq!(
			vr_entry(original),
			"[Desktop Entry]\nName=Foo\nName[de]=Föö\nExec=foo\nCategories=Game;X-WiVRn-VR;\n\nNoDisplay=true\nX-LVRR-Added=true\n"
		);
		assert_eq!(strip_field_codes("Exec=bar --x=100%% %f"), "Exec=bar --x=100%");
		assert!(vr_entry("[Desktop Entry]\nExec=bar\n").contains("Categories=X-WiVRn-VR;\n"));
	}

	#[test]
	fn adds_and_removes_only_its_own() {
		let dir = std::env::temp_dir().join(format!("lvrr-added-{}", std::process::id()));
		write(&dir, "620", &steam_entry(620, "Portal\n2")).unwrap();
		assert!(
			fs::read_to_string(dir.join("620.desktop"))
				.unwrap()
				.contains("Name=Portal\\n2\n")
		);
		assert!(write(&dir, "620", "").is_err());
		fs::write(dir.join("other.desktop"), "[Desktop Entry]\nName=Other\n").unwrap();

		assert_eq!(added_in(&dir), HashSet::from(["620".to_owned()]));
		assert!(remove_in(&dir, "other").is_err());
		remove_in(&dir, "620").unwrap();
		assert!(added_in(&dir).is_empty());

		fs::remove_dir_all(&dir).unwrap();
	}
}
