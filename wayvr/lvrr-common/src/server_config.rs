//! The LVRR server's configuration file (`JsonConfiguration` on D-Bus), with the settings
//! the dashboard shows. Keys it doesn't know are kept as they are.

use std::path::PathBuf;

use serde_json::{Map, Value};

/// Encoders, as named in the configuration
pub const ENCODERS: [&str; 4] = ["nvenc", "vaapi", "vulkan", "x264"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Encoder {
	Automatic,
	Named(String),
	/// Per stream or per codec settings, edited by hand
	Custom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenVrCompat {
	/// The bundled xrizer, or what the server finds
	Automatic,
	Path(String),
	/// The server leaves the OpenVR configuration alone
	Unmanaged,
}

/// The file lvrr starts the server with, as lvrr's `config_path()` finds it
pub fn path() -> Option<PathBuf> {
	crate::xdg_dir("XDG_CONFIG_HOME", ".config").map(|config| config.join("lvrr/wivrn-server.json"))
}

#[derive(Clone, Debug)]
pub struct ServerConfig(pub Value);

/// No settings, so the server's defaults
impl Default for ServerConfig {
	fn default() -> Self {
		Self(Value::Object(Map::new()))
	}
}

impl ServerConfig {
	fn map(&self) -> Option<&Map<String, Value>> {
		self.0.as_object()
	}

	/// None for a file the server couldn't parse, which it publishes as null. A setting changed
	/// then would become the whole file once sent.
	fn map_mut(&mut self) -> Option<&mut Map<String, Value>> {
		self.0.as_object_mut()
	}

	fn get(&self, key: &str) -> Option<&Value> {
		self.map()?.get(key)
	}

	pub fn bool(&self, key: &str) -> bool {
		self.get(key).and_then(Value::as_bool).unwrap_or(false)
	}

	pub fn number(&self, key: &str) -> Option<f64> {
		self.get(key).and_then(Value::as_f64)
	}

	/// Does nothing unless the configuration is an object, see [Self::map_mut]
	pub fn set(&mut self, key: &str, value: Option<Value>) {
		let Some(map) = self.map_mut() else {
			return;
		};
		match value {
			Some(v) => {
				map.insert(key.into(), v);
			}
			None => {
				map.remove(key);
			}
		}
	}

	pub fn encoder(&self) -> Encoder {
		match self.get("encoder") {
			None => Encoder::Automatic,
			Some(Value::String(name)) if name == "auto" => Encoder::Automatic,
			Some(Value::String(name)) => Encoder::Named(name.clone()),
			Some(Value::Object(object)) if object.len() == 1 => match object.get("encoder") {
				Some(Value::String(name)) => Encoder::Named(name.clone()),
				_ => Encoder::Custom,
			},
			Some(_) => Encoder::Custom,
		}
	}

	pub fn set_encoder(&mut self, encoder: &Encoder) {
		match encoder {
			Encoder::Automatic => self.set("encoder", None),
			Encoder::Named(name) => self.set("encoder", Some(Value::String(name.clone()))),
			Encoder::Custom => {}
		}
	}

	pub fn openvr_compat(&self) -> OpenVrCompat {
		match self.get("openvr-compat-path") {
			None => OpenVrCompat::Automatic,
			Some(Value::Null) => OpenVrCompat::Unmanaged,
			Some(Value::String(path)) => OpenVrCompat::Path(path.clone()),
			Some(_) => OpenVrCompat::Automatic,
		}
	}

	pub fn set_openvr_compat(&mut self, compat: &OpenVrCompat) {
		match compat {
			OpenVrCompat::Automatic => self.set("openvr-compat-path", None),
			OpenVrCompat::Path(path) => self.set("openvr-compat-path", Some(Value::String(path.clone()))),
			OpenVrCompat::Unmanaged => self.set("openvr-compat-path", Some(Value::Null)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keeps_unknown_keys() {
		let mut config = ServerConfig(serde_json::json!({"application": ["wayvr", "--openxr"], "port": 9757}));
		config.set_encoder(&Encoder::Named("x264".into()));
		config.set("hid-forwarding", Some(Value::Bool(true)));
		assert_eq!(config.0["application"], serde_json::json!(["wayvr", "--openxr"]));
		assert_eq!(config.0["port"], 9757);
		assert_eq!(config.encoder(), Encoder::Named("x264".into()));
		assert!(config.bool("hid-forwarding"));
	}

	#[test]
	fn reads_encoder_forms() {
		let enc = |v: Value| ServerConfig(serde_json::json!({ "encoder": v })).encoder();
		assert_eq!(ServerConfig::default().encoder(), Encoder::Automatic);
		assert_eq!(enc("nvenc".into()), Encoder::Named("nvenc".into()));
		assert_eq!(
			enc(serde_json::json!({"encoder": "vaapi"})),
			Encoder::Named("vaapi".into())
		);
		assert_eq!(
			enc(serde_json::json!({"encoder": "vaapi", "codec": "h265"})),
			Encoder::Custom
		);
		assert_eq!(enc(serde_json::json!(["nvenc", "nvenc", "x264"])), Encoder::Custom);
	}

	#[test]
	fn openvr_compat_forms() {
		let mut config = ServerConfig::default();
		assert_eq!(config.openvr_compat(), OpenVrCompat::Automatic);
		config.set_openvr_compat(&OpenVrCompat::Unmanaged);
		assert_eq!(config.0["openvr-compat-path"], Value::Null);
		assert_eq!(config.openvr_compat(), OpenVrCompat::Unmanaged);
		config.set_openvr_compat(&OpenVrCompat::Automatic);
		assert!(config.0.get("openvr-compat-path").is_none());
	}

	#[test]
	fn leaves_an_unread_file_alone() {
		let mut config = ServerConfig(Value::Null);
		config.set("usb-auto-connect", Some(Value::Bool(true)));
		config.set("hid-forwarding", None);
		config.set_encoder(&Encoder::Named("nvenc".into()));
		config.set_openvr_compat(&OpenVrCompat::Unmanaged);
		assert_eq!(config.0, Value::Null);
	}
}
