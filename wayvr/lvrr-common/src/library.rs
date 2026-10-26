//! The Library's own state in `$XDG_STATE_HOME/lvrr/library.json`: how the list is sorted
//! and when LVRR last started each app, so apps outside Steam count as recent too.

use std::{
	collections::HashMap,
	fs,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::state::VrApplication;

/// How many apps the Library shows as recently played
pub const RECENT_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
	/// Last played first
	#[default]
	Recent,
	Name,
}

impl Sort {
	pub const ALL: [Self; 2] = [Self::Recent, Self::Name];
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryState {
	pub sort: Sort,
	/// App id to seconds since the epoch
	pub started: HashMap<String, u64>,
}

fn state_file() -> Option<PathBuf> {
	let non_empty = |name| std::env::var_os(name).filter(|v| !v.is_empty());
	let state = non_empty("XDG_STATE_HOME")
		.map(PathBuf::from)
		.filter(|d| d.is_absolute())
		.or_else(|| Some(Path::new(&non_empty("HOME")?).join(".local").join("state")))?;
	Some(state.join("lvrr").join("library.json"))
}

impl LibraryState {
	pub fn load() -> Self {
		state_file().map(|path| Self::load_from(&path)).unwrap_or_default()
	}

	pub fn save(&self) -> anyhow::Result<()> {
		let path = state_file().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
		self.save_to(&path)
	}

	fn load_from(path: &Path) -> Self {
		fs::read_to_string(path)
			.ok()
			.and_then(|s| serde_json::from_str(&s).ok())
			.unwrap_or_default()
	}

	fn save_to(&self, path: &Path) -> anyhow::Result<()> {
		if let Some(dir) = path.parent() {
			fs::create_dir_all(dir)?;
		}
		fs::write(path, serde_json::to_string_pretty(self)?)?;
		Ok(())
	}

	pub fn mark_started(&mut self, id: &str) {
		let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
		self.started.insert(id.to_owned(), now);
	}
}

/// Whether the name contains the query, ignoring case
pub fn matches(app: &VrApplication, query: &str) -> bool {
	app.name.to_lowercase().contains(&query.trim().to_lowercase())
}

/// Sorts apps; by name for ties and for apps that were never played
pub fn sort(apps: &mut [&VrApplication], sort: Sort, last_played: impl Fn(&VrApplication) -> Option<u64>) {
	apps.sort_by_cached_key(|app| {
		let played = match sort {
			Sort::Recent => last_played(app),
			Sort::Name => None,
		};
		(std::cmp::Reverse(played), app.name.to_lowercase())
	});
}

/// The last played apps, most recent first
pub fn recent<'a>(
	apps: &[&'a VrApplication],
	last_played: impl Fn(&VrApplication) -> Option<u64>,
) -> Vec<&'a VrApplication> {
	let mut recent: Vec<_> = apps.iter().copied().filter(|app| last_played(app).is_some()).collect();
	sort(&mut recent, Sort::Recent, last_played);
	recent.truncate(RECENT_COUNT);
	recent
}

#[cfg(test)]
mod tests {
	use super::*;

	fn app(id: &str, name: &str) -> VrApplication {
		VrApplication {
			id: id.into(),
			name: name.into(),
			icon: String::new(),
		}
	}

	#[test]
	fn sort_and_search() {
		let apps = [
			app("1", "beat Saber"),
			app("2", "Alyx"),
			app("3", "Contractors"),
			app("x", "Blender"),
		];
		let played = |a: &VrApplication| match a.id.as_str() {
			"3" => Some(20),
			"x" => Some(10),
			_ => None,
		};
		let mut list: Vec<_> = apps.iter().collect();
		let names = |list: &[&VrApplication]| list.iter().map(|a| a.name.clone()).collect::<Vec<_>>();

		sort(&mut list, Sort::Recent, played);
		assert_eq!(names(&list), ["Contractors", "Blender", "Alyx", "beat Saber"]);
		sort(&mut list, Sort::Name, played);
		assert_eq!(names(&list), ["Alyx", "beat Saber", "Blender", "Contractors"]);
		assert_eq!(names(&recent(&list, played)), ["Contractors", "Blender"]);

		assert!(matches(&apps[0], " BEAT "));
		assert!(!matches(&apps[1], "beat"));
		assert!(matches(&apps[1], ""));
	}

	#[test]
	fn state_round_trip() {
		let path = std::env::temp_dir()
			.join(format!("lvrr-library-{}", std::process::id()))
			.join("library.json");
		assert_eq!(LibraryState::load_from(&path), LibraryState::default());

		let mut state = LibraryState {
			sort: Sort::Name,
			..Default::default()
		};
		state.mark_started("620980");
		state.save_to(&path).unwrap();
		let loaded = LibraryState::load_from(&path);
		assert_eq!(loaded, state);
		assert!(loaded.started["620980"] > 0);
		assert!(fs::read_to_string(&path).unwrap().contains("\"name\""));

		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}
}
