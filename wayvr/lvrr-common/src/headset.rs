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

	pub const fn display_name(self) -> &'static str {
		match self {
			Codec::Automatic => "Automatic",
			Codec::H264 => "H.264",
			Codec::H265 => "HEVC",
			Codec::Av1 => "AV1",
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

#[derive(Clone, Debug, Default, Deserialize)]
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
	pub usb_network: bool,
	pub keyboard_mouse: bool,
	pub hand_tracking: bool,
	pub eye_gaze: bool,
	pub face_tracking: bool,
	pub body_tracking: bool,
	pub body_parts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
	pub preferred_refresh_rate: f32,
	pub minimum_refresh_rate: f32,
	pub fps_divider: u32,
	pub resolution_scale: f32,
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

#[derive(Clone, Debug, Default, Deserialize)]
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
	/// Config keys changed while offline, applied when the headset connects; null when none
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
	pub decoders: Vec<DecoderTiming>,
}

impl HeadsetStats {
	/// Latency from encoding to display, of the slowest video stream
	pub fn latency_ms(&self) -> f32 {
		self.decoders.iter().map(|d| d.total_ms).fold(0.0, f32::max)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// Written by the server's `lvrr-test --example`
	const EXAMPLE: &str = include_str!("../../../docs/dbus-example.json");

	#[test]
	fn parses_server_example() {
		let example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
		let settings: HeadsetSettings = serde_json::from_value(example["HeadsetSettings"].clone()).unwrap();
		assert_eq!(settings.request_id, 7);
		assert_eq!(settings.capabilities.refresh_rates, [72.0, 90.0, 120.0]);
		assert_eq!(settings.capabilities.codecs, [Codec::H264, Codec::H265, Codec::Av1]);
		assert_eq!(settings.config.codec, Codec::Automatic);
		assert_eq!(settings.config.sharpening, 0.5);
		assert_eq!(settings.config.body_parts, ["chest", "hip"]);
		assert_eq!(settings.permission_pending, ["microphone"]);
		assert!(settings.restart_required);
		assert!(!settings.offline);
		assert!(settings.pending.is_null());

		let stats: HeadsetStats = serde_json::from_value(example["HeadsetStats"].clone()).unwrap();
		assert_eq!(stats.decoders.len(), 2);
		assert_eq!(stats.latency_ms(), 22.0);
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

	// Every config key the dashboard may send is one the server reports
	#[test]
	fn config_keys_match_server() {
		let example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
		let keys: Vec<&String> = example["HeadsetSettings"]["config"]
			.as_object()
			.unwrap()
			.keys()
			.collect();
		for key in crate::state::HEADSET_KEYS {
			assert!(keys.iter().any(|k| k == key), "server does not report {key}");
		}
	}
}
