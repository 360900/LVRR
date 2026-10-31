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

fn applications_dir() -> anyhow::Result<PathBuf> {
	Ok(
		crate::xdg_dir("XDG_DATA_HOME", ".local/share")
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
			// only the names add_steam and add_desktop write, so other entries aren't read
			let named = id.starts_with(PREFIX) || id.bytes().all(|b| b.is_ascii_digit());
			let ours = named && path.extension().is_some_and(|e| e == "desktop") && is_ours(&path);
			ours.then_some(id)
		})
		.collect()
}

fn is_ours(path: &Path) -> bool {
	fs::read_to_string(path).is_ok_and(|s| s.lines().any(|l| l.trim() == MARKER))
}

/// Adds an installed Steam game
pub fn add_steam(app_id: u64, name: &str) -> anyhow::Result<()> {
	write(&applications_dir()?, &app_id.to_string(), &steam_entry(app_id, name))
}

/// Adds a copy of the desktop entry `original`, whose id is `id`; returns the copy's id
pub fn add_desktop(id: &str, original: &Path) -> anyhow::Result<String> {
	let entry = vr_entry(&fs::read_to_string(original)?);
	anyhow::ensure!(
		listable(&entry),
		"its desktop entry has no Name or Exec, or isn't an application"
	);
	let new_id = format!("{PREFIX}{id}");
	write(&applications_dir()?, &new_id, &entry)?;
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
	// our own copy is replaced, so adding again works and fixes an outdated copy
	anyhow::ensure!(!path.exists() || is_ours(&path), "{} already exists", path.display());
	fs::create_dir_all(dir)?;
	fs::write(path, content)?;
	Ok(())
}

/// Whether the server lists this entry. Like do_desktop_entry in wivrn/common/application.cpp,
/// it needs a Name without locale and an Exec, both with a value, and no Type other than Application.
fn listable(entry: &str) -> bool {
	let (mut name, mut exec) = (false, false);
	for line in entry.split('\n') {
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		// as in the server, blank values and indented lines don't count
		let value = value.trim_start_matches(' ');
		if value.is_empty() {
			continue;
		}
		match key.trim_end_matches(' ') {
			"Type" if value != "Application" => return false,
			"Name" => name = true,
			"Exec" => exec = true,
			_ => {}
		}
	}
	name && exec
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
		// NoDisplay entries still handle the files, URLs and D-Bus interfaces they list, and the copy's
		// Exec takes no file or URL; the server only reads Type, Name, Exec, Path, Icon and Categories
		"MimeType",
		"Implements",
		"X-KDE-Protocols",
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
		let original = "# comment\n[Desktop Entry]\nName=Foo\nName[de]=Föö\nExec=foo %U\nCategories=Game\nNoDisplay=false\nActions=new;\nMimeType=video/mp4;x-scheme-handler/foo;\nImplements=org.example.Foo;\nX-KDE-Protocols=foo\n\n[Desktop Action new]\nExec=foo --new\n";
		assert_eq!(
			vr_entry(original),
			"[Desktop Entry]\nName=Foo\nName[de]=Föö\nExec=foo\nCategories=Game;X-WiVRn-VR;\n\nNoDisplay=true\nX-LVRR-Added=true\n"
		);
		assert_eq!(strip_field_codes("Exec=bar --x=100%% %f"), "Exec=bar --x=100%");
		assert!(vr_entry("[Desktop Entry]\nExec=bar\n").contains("Categories=X-WiVRn-VR;\n"));
	}

	#[test]
	fn copies_only_what_the_server_lists() {
		let listed = |original: &str| listable(&vr_entry(original));
		assert!(listed("[Desktop Entry]\nType=Application\nName=Foo\nExec=foo %U\n"));
		// a NoDisplay override, a localized name only, a blank command, a link
		assert!(!listed("[Desktop Entry]\nNoDisplay=true\n"));
		assert!(!listed("[Desktop Entry]\nName=\nName[en]=Foo\nExec=foo\n"));
		assert!(!listed("[Desktop Entry]\nName=Foo\nExec=%U\n"));
		assert!(!listed("[Desktop Entry]\nType=Link\nName=Foo\nExec=foo\n"));
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
		// adding again replaces our copy, never someone else's file
		write(&dir, "620", &steam_entry(620, "Portal 2")).unwrap();
		assert!(
			fs::read_to_string(dir.join("620.desktop"))
				.unwrap()
				.contains("Name=Portal 2\n")
		);
		fs::write(dir.join("other.desktop"), "[Desktop Entry]\nName=Other\n").unwrap();
		assert!(write(&dir, "other", &steam_entry(1, "Other")).is_err());
		write(&dir, "lvrr-foo", &vr_entry("[Desktop Entry]\nName=Foo\nExec=foo\n")).unwrap();
		// a file with the marker under a name LVRR doesn't write isn't read
		fs::write(dir.join("renamed.desktop"), steam_entry(620, "Portal 2")).unwrap();

		assert_eq!(added_in(&dir), HashSet::from(["620".to_owned(), "lvrr-foo".to_owned()]));
		assert!(remove_in(&dir, "other").is_err());
		remove_in(&dir, "620").unwrap();
		remove_in(&dir, "lvrr-foo").unwrap();
		assert!(added_in(&dir).is_empty());

		fs::remove_dir_all(&dir).unwrap();
	}
}
