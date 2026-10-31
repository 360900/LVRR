//! Per game xrizer settings, in `~/.config/xrizer/apps/steam.app.<id>.json`. Keys the
//! dashboard doesn't know are kept as they are.

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use serde_json::{Map, Value};

/// The controllers a game sees
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerType {
	/// The real ones
	Auto,
	Touch,
	Index,
}

impl ControllerType {
	pub const ALL: [Self; 3] = [Self::Auto, Self::Touch, Self::Index];

	/// As named in xrizer's file
	fn name(self) -> Option<&'static str> {
		match self {
			Self::Auto => None,
			Self::Touch => Some("oculus_touch"),
			Self::Index => Some("knuckles"),
		}
	}

	fn from_name(name: &str) -> Self {
		Self::ALL
			.into_iter()
			.find(|t| t.name() == Some(name))
			.unwrap_or(Self::Auto)
	}
}

fn apps_dir() -> Option<PathBuf> {
	crate::xdg_dir("XDG_CONFIG_HOME", ".config").map(|config| config.join("xrizer/apps"))
}

fn file(dir: &Path, steam_app_id: u64) -> PathBuf {
	dir.join(format!("steam.app.{steam_app_id}.json"))
}

fn read(path: &Path) -> Map<String, Value> {
	fs::read_to_string(path)
		.ok()
		.and_then(|s| serde_json::from_str(&s).ok())
		.unwrap_or_default()
}

pub fn controller_type(steam_app_id: u64) -> ControllerType {
	apps_dir().map_or(ControllerType::Auto, |dir| controller_type_in(&dir, steam_app_id))
}

pub fn set_controller_type(steam_app_id: u64, ty: ControllerType) -> anyhow::Result<()> {
	let dir = apps_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
	set_controller_type_in(&dir, steam_app_id, ty)
}

fn controller_type_in(dir: &Path, steam_app_id: u64) -> ControllerType {
	read(&file(dir, steam_app_id))
		.get("controller_type")
		.and_then(Value::as_str)
		.map_or(ControllerType::Auto, ControllerType::from_name)
}

fn set_controller_type_in(dir: &Path, steam_app_id: u64, ty: ControllerType) -> anyhow::Result<()> {
	let path = file(dir, steam_app_id);
	let mut map = read(&path);
	match ty.name() {
		Some(name) => map.insert("controller_type".into(), name.into()),
		None => map.remove("controller_type"),
	};
	if map.is_empty() {
		match fs::remove_file(&path) {
			Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
			_ => return Ok(()),
		}
	}
	fs::create_dir_all(dir)?;
	fs::write(&path, serde_json::to_string_pretty(&Value::Object(map))?)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn controller_type_round_trip() {
		let dir = std::env::temp_dir().join(format!("lvrr-xrizer-apps-{}", std::process::id()));
		let path = file(&dir, 42);
		assert_eq!(controller_type_in(&dir, 42), ControllerType::Auto);

		set_controller_type_in(&dir, 42, ControllerType::Index).unwrap();
		assert_eq!(controller_type_in(&dir, 42), ControllerType::Index);
		assert!(fs::read_to_string(&path).unwrap().contains("\"knuckles\""));

		// other keys stay, the file goes once nothing is left
		fs::write(&path, r#"{"controller_type": "oculus_touch", "other": 1}"#).unwrap();
		assert_eq!(controller_type_in(&dir, 42), ControllerType::Touch);
		set_controller_type_in(&dir, 42, ControllerType::Auto).unwrap();
		assert_eq!(
			read(&path),
			serde_json::from_str::<Map<_, _>>(r#"{"other": 1}"#).unwrap()
		);
		fs::write(&path, r#"{"controller_type": "vive_controller"}"#).unwrap();
		assert_eq!(controller_type_in(&dir, 42), ControllerType::Auto);
		set_controller_type_in(&dir, 42, ControllerType::Auto).unwrap();
		assert!(!path.exists());

		fs::remove_dir(&dir).unwrap();
	}
}
