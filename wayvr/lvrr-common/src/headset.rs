//! The headset's settings and statistics, as the LVRR server reports them on D-Bus.
//! The format is described in LVRR's `docs/dbus.md`.

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
	#[default]
	Automatic,
	H264,
	H265,
	Av1,
}

impl Codec {
	pub const fn key(self) -> &'static str {
		match self {
			Codec::Automatic => "automatic",
			Codec::H264 => "h264",
			Codec::H265 => "h265",
			Codec::Av1 => "av1",
		}
	}

	/// The name shown as is; None for Automatic, which the dashboard translates
	pub const fn display_name(self) -> Option<&'static str> {
		match self {
			Codec::Automatic => None,
			Codec::H264 => Some("H.264"),
			Codec::H265 => Some("HEVC"),
			Codec::Av1 => Some("AV1"),
		}
	}
}

/// Supersampling levels
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostProcessing {
	#[default]
	Off,
	Normal,
	Quality,
}

impl PostProcessing {
	pub const ALL: [PostProcessing; 3] = [PostProcessing::Off, PostProcessing::Normal, PostProcessing::Quality];

	pub const fn key(self) -> &'static str {
		match self {
			PostProcessing::Off => "off",
			PostProcessing::Normal => "normal",
			PostProcessing::Quality => "quality",
		}
	}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Posture {
	#[default]
	Standing,
	Seated,
}

impl Posture {
	pub const fn key(self) -> &'static str {
		match self {
			Posture::Standing => "standing",
			Posture::Seated => "seated",
		}
	}
}

/// Virtual body trackers, in the order the headset defines them
pub const BODY_PARTS: [&str; 8] = [
	"chest",
	"left_elbow",
	"right_elbow",
	"hip",
	"left_knee",
	"right_knee",
	"left_foot",
	"right_foot",
];

/// Config keys that only apply to a new session, marked "restart" in LVRR's `docs/dbus.md`. The
/// server's `restart_keys` (lvrr_offline.cpp) and the headset's `restart_fields` list the same.
pub const RESTART_KEYS: [&str; 10] = [
	"resolution_scale",
	"foveation",
	"codec",
	"bit_depth",
	"microphone",
	"mic_unprocessed",
	"hand_tracking",
	"eye_gaze",
	"face_tracking",
	"body_tracking",
];

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Capabilities {
	pub android: bool,
	pub refresh_rates: Vec<f32>,
	pub refresh_rate_control: bool,
	pub spacewarp: bool,
	pub eye_size: [u32; 2],
	pub codecs: Vec<Codec>,
	pub codecs_10bit: Vec<Codec>,
	pub max_bitrate_bps: u32,
	pub max_bitrate_extended_bps: u32,
	pub post_processing: bool,
	pub high_power_mode: bool,
	pub passthrough_boundary: bool,
	pub see_through: bool,
	pub simultaneous_hands: bool,
	pub usb_network: bool,
	pub keyboard_mouse: bool,
	pub hand_tracking: bool,
	pub eye_gaze: bool,
	pub face_tracking: bool,
	pub body_tracking: bool,
	pub body_parts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
	pub preferred_refresh_rate: f32,
	pub minimum_refresh_rate: f32,
	pub fps_divider: u32,
	pub resolution_scale: f32,
	pub fov_horizontal: f32,
	pub fov_vertical: f32,
	pub foveation: f32,
	pub codec: Codec,
	pub bit_depth: u8,
	pub bitrate_bps: u32,
	pub foveation_override: bool,
	pub foveation_pitch: f32,
	pub foveation_distance: f32,
	pub super_sampling: PostProcessing,
	pub sharpening: f32,
	pub microphone: bool,
	pub mic_unprocessed: bool,
	pub forward_keyboard: bool,
	pub forward_mouse: bool,
	pub forward_gamepad: bool,
	pub hand_tracking: bool,
	pub eye_gaze: bool,
	pub face_tracking: bool,
	pub body_tracking: bool,
	pub body_parts: Vec<String>,
	pub posture: Posture,
	pub height_offset: f32,
	pub high_power_mode: bool,
	pub passthrough_boundary: bool,
	pub see_through: f32,
	pub simultaneous_hands: bool,
	pub usb_network: bool,
	pub extended_config: bool,
}

impl Config {
	pub fn max_bitrate_bps(&self, caps: &Capabilities) -> u32 {
		if self.extended_config {
			caps.max_bitrate_extended_bps
		} else {
			caps.max_bitrate_bps
		}
	}
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HeadsetSettings {
	pub request_id: u32,
	pub capabilities: Capabilities,
	pub config: Config,
	pub restart_required: bool,
	pub server_input_forwarding: bool,
	pub permission_pending: Vec<String>,
	pub permission_denied: Vec<String>,
	/// The headset isn't connected: the settings it had last time, with `pending` applied
	pub offline: bool,
	/// Changes waiting for the headset, config key to new value, sent when it connects; null when none
	pub pending: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DecoderTiming {
	pub encode_ms: f32,
	pub send_ms: f32,
	pub network_ms: f32,
	pub decode_ms: f32,
	pub display_ms: f32,
	pub total_ms: f32,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct HeadsetStats {
	pub cpu_time_ms: f32,
	pub gpu_time_ms: f32,
	pub rx_bps: f32,
	pub tx_bps: f32,
	pub display_hz: f32,
	pub motion_to_photon_ms: f32,
	/// The headset's, from 0 to 1
	pub battery: Option<f32>,
	pub charging: bool,
	pub decoders: Vec<DecoderTiming>,
	/// Measured on the PC: the main XR application's new frames shown per second
	pub game_fps: Option<f32>,
	/// Compositor frames per second that repeated the game's last frame
	pub game_missed_fps: f32,
	/// Frames per second the encoders were too busy for
	pub encoder_skipped_fps: f32,
	/// What the session streams with, until a new one; None for raw video
	pub codec: Option<Codec>,
	/// 8 or 10
	pub bit_depth: u8,
	/// The recommended render size per eye, after `resolution_scale`
	pub render_size: [u32; 2],
}

impl HeadsetStats {
	/// Latency from encoding to display, of the slowest video stream
	pub fn latency_ms(&self) -> f32 {
		self.decoders.iter().map(|d| d.total_ms).fold(0.0, f32::max)
	}

	/// The slowest video stream's timings
	pub fn slowest_decoder(&self) -> Option<&DecoderTiming> {
		self.decoders.iter().max_by(|a, b| a.total_ms.total_cmp(&b.total_ms))
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use super::*;
	use crate::emulator::EXAMPLE;

	const DOCS: &str = include_str!("../../../docs/dbus.md");

	#[test]
	fn parses_server_example() {
		let example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
		let settings: HeadsetSettings = serde_json::from_value(example["HeadsetSettings"].clone()).unwrap();
		assert_eq!(settings.request_id, 7);
		assert_eq!(settings.capabilities.refresh_rates, [72.0, 90.0, 120.0]);
		assert_eq!(settings.capabilities.codecs, [Codec::H264, Codec::H265, Codec::Av1]);
		assert_eq!(settings.config.codec, Codec::Automatic);
		assert_eq!(settings.config.sharpening, 0.5);
		assert_eq!(settings.config.fov_vertical, 0.9);
		assert_eq!(settings.config.body_parts, ["chest", "hip"]);
		assert_eq!(settings.permission_pending, ["microphone"]);
		assert!(settings.restart_required);
		assert!(!settings.offline);
		assert!(settings.pending.is_null());

		let stats: HeadsetStats = serde_json::from_value(example["HeadsetStats"].clone()).unwrap();
		assert_eq!(stats.decoders.len(), 2);
		assert_eq!(stats.latency_ms(), 22.0);
		assert_eq!(stats.slowest_decoder().unwrap().network_ms, 4.0);
		assert_eq!(stats.battery, Some(0.8));
		assert!(stats.charging);
		assert_eq!(stats.game_fps, Some(88.0));
		assert_eq!(stats.encoder_skipped_fps, 0.5);
		assert_eq!(stats.codec, Some(Codec::H265));
		assert_eq!(stats.bit_depth, 10);
		assert_eq!(stats.render_size, [2016, 2112]);
	}

	// Without a headset, the server sends the last one's settings with the changes waiting for it
	#[test]
	fn parses_offline_settings() {
		let example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
		let mut value = example["HeadsetSettings"].clone();
		value["offline"] = true.into();
		value["pending"] = serde_json::json!({"bitrate_bps": 30_000_000});
		value["config"]["bitrate_bps"] = 30_000_000.into();
		let settings: HeadsetSettings = serde_json::from_value(value).unwrap();
		assert!(settings.offline);
		assert_eq!(settings.pending["bitrate_bps"], 30_000_000);
		assert_eq!(settings.config.bitrate_bps, 30_000_000);
	}

	// The D-Bus documentation lists the config keys the server reports, and marks RESTART_KEYS "restart"
	#[test]
	fn config_keys_match_docs() {
		let (_, table) = DOCS
			.split_once("| Key | Type | Applies |")
			.expect("no config table in docs/dbus.md");
		let rows = table
			.lines()
			.skip_while(|l| !l.starts_with("| `"))
			.take_while(|l| l.starts_with('|'));
		let mut documented = BTreeSet::new();
		let mut restart = BTreeSet::new();
		for row in rows {
			let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
			// the first cell names one or more `keys`, the last says when they apply
			let keys = cells[0].split('`').skip(1).step_by(2);
			if cells.last() == Some(&"restart") {
				restart.extend(keys.clone());
			}
			documented.extend(keys);
		}

		let example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
		let reported: BTreeSet<&str> = example["HeadsetSettings"]["config"]
			.as_object()
			.unwrap()
			.keys()
			.map(String::as_str)
			.collect();
		assert_eq!(documented, reported);
		assert_eq!(restart, BTreeSet::from(RESTART_KEYS));
	}
}
