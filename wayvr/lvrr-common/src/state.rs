//! What the dashboard knows about the LVRR server, and what it can ask of it.

use std::collections::VecDeque;

use crate::headset::{HeadsetSettings, HeadsetStats};

/// Headset settings the dashboard may change, see LVRR's `docs/dbus.md`
pub const HEADSET_KEYS: &[&str] = &[
	"preferred_refresh_rate",
	"minimum_refresh_rate",
	"fps_divider",
	"resolution_scale",
	"fov_horizontal",
	"fov_vertical",
	"foveation",
	"codec",
	"bit_depth",
	"bitrate_bps",
	"foveation_override",
	"foveation_pitch",
	"foveation_distance",
	"super_sampling",
	"sharpening",
	"microphone",
	"mic_unprocessed",
	"forward_keyboard",
	"forward_mouse",
	"forward_gamepad",
	"hand_tracking",
	"eye_gaze",
	"face_tracking",
	"body_tracking",
	"body_parts",
	"posture",
	"height_offset",
	"high_power_mode",
	"passthrough_boundary",
	"see_through",
	"simultaneous_hands",
	"usb_network",
	"extended_config",
];

/// Statistics kept for graphs, at about one sample per second
pub const STATS_HISTORY: usize = 120;
const MESSAGES_KEPT: usize = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
	/// No session bus
	#[default]
	NoBus,
	/// The LVRR server is not running
	ServerAbsent,
	Online,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownKey {
	pub name: String,
	pub public_key: String,
	/// Last connection, in seconds since the epoch
	pub last_connection: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VrApplication {
	pub id: String,
	pub name: String,
	/// A file path, a theme icon name, or empty
	pub icon: String,
}

impl VrApplication {
	/// Steam applications have numeric ids
	pub fn steam_app_id(&self) -> Option<u64> {
		self.id.parse().ok()
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
	Info,
	Urgent,
	Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
	pub kind: MessageKind,
	pub text: String,
}

/// A tracked device with a battery
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryDevice {
	Headset,
	LeftController,
	RightController,
	Tracker,
}

impl BatteryDevice {
	pub const ALL: [Self; 4] = [
		Self::Headset,
		Self::LeftController,
		Self::RightController,
		Self::Tracker,
	];

	/// Translation key of the device's name
	pub fn key(self) -> &'static str {
		match self {
			Self::Headset => "LVRR.BATTERY.HEADSET",
			Self::LeftController => "LVRR.BATTERY.LEFT",
			Self::RightController => "LVRR.BATTERY.RIGHT",
			Self::Tracker => "LVRR.BATTERY.TRACKER",
		}
	}
}

/// A battery reported by the OpenXR runtime
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
	pub device: BatteryDevice,
	/// From 0 to 1
	pub level: f32,
	pub charging: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LvrrState {
	/// Changes whenever anything but the statistics changes
	pub generation: u64,
	/// Changes with each statistics sample
	pub stats_generation: u64,
	pub link: Link,
	pub headset_connected: bool,
	pub session_running: bool,
	pub system_name: String,
	pub bitrate_bps: u32,
	pub pairing_enabled: bool,
	pub encryption_enabled: bool,
	pub pin: String,
	pub known_keys: Vec<KnownKey>,
	pub steam_command: String,
	/// The server's configuration file
	pub server_config: serde_json::Value,
	pub headset: Option<HeadsetSettings>,
	pub stats: VecDeque<HeadsetStats>,
	pub applications: Option<Vec<VrApplication>>,
	pub messages: VecDeque<Message>,
}

impl LvrrState {
	pub fn latest_stats(&self) -> Option<&HeadsetStats> {
		self.stats.back()
	}

	pub(crate) fn push_stats(&mut self, stats: HeadsetStats) {
		if self.stats.len() == STATS_HISTORY {
			self.stats.pop_front();
		}
		self.stats.push_back(stats);
		self.stats_generation += 1;
	}

	pub(crate) fn push_message(&mut self, message: Message) {
		if self.messages.len() == MESSAGES_KEPT {
			self.messages.pop_front();
		}
		self.messages.push_back(message);
		self.generation += 1;
	}

	pub(crate) fn changed(&mut self) {
		self.generation += 1;
	}
}

#[derive(Clone, Debug)]
pub enum LvrrRequest {
	/// A JSON object with headset config keys
	SetHeadsetSettings(serde_json::Value),
	/// Applies connection-time settings; closes all XR applications including the dashboard
	RestartSession,
	Disconnect,
	Quit,
	EnablePairing {
		timeout_secs: i32,
	},
	DisablePairing,
	RevokeKey(String),
	RenameKey {
		key: String,
		name: String,
	},
	/// The full server configuration
	SetServerConfig(serde_json::Value),
	RefreshApplications,
	StartApplication(String),
	/// A Monado client id
	StopApplication(u32),
}

impl LvrrRequest {
	pub const fn name(&self) -> &'static str {
		match self {
			LvrrRequest::SetHeadsetSettings(_) => "SetHeadsetSettings",
			LvrrRequest::RestartSession => "RestartSession",
			LvrrRequest::Disconnect => "Disconnect",
			LvrrRequest::Quit => "Quit",
			LvrrRequest::EnablePairing { .. } => "EnablePairing",
			LvrrRequest::DisablePairing => "DisablePairing",
			LvrrRequest::RevokeKey(_) => "RevokeKey",
			LvrrRequest::RenameKey { .. } => "RenameKey",
			LvrrRequest::SetServerConfig(_) => "SetServerConfig",
			LvrrRequest::RefreshApplications => "ListApplications",
			LvrrRequest::StartApplication(_) => "StartApplication",
			LvrrRequest::StopApplication(_) => "StopApplication",
		}
	}
}

/// Things the dashboard should tell the user about
#[derive(Clone, Debug)]
pub enum Notice {
	Message(Message),
	RequestFailed { request: &'static str, error: String },
}

/// The LVRR server, or a stand-in for it
pub trait LvrrBackend {
	/// Applies what happened since the last call and returns what to tell the user
	fn poll(&mut self) -> Vec<Notice>;
	fn state(&self) -> &LvrrState;
	fn request(&mut self, request: LvrrRequest) -> anyhow::Result<()>;
}
