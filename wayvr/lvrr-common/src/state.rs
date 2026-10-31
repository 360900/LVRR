//! What the dashboard knows about the LVRR server, and what it can ask of it.

use std::collections::VecDeque;

use crate::headset::{HeadsetSettings, HeadsetStats};

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
	/// Another WiVRn server owns the bus name; the dashboard leaves it alone
	Foreign,
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
	/// A file path, or empty when the server found none
	pub icon: String,
}

impl VrApplication {
	/// Steam games are listed under their app id, which is 32-bit. Non-Steam games added to Steam
	/// come under a 64-bit game id, which has no store art and which xrizer never uses as a key.
	pub fn steam_app_id(&self) -> Option<u64> {
		self.id.parse::<u32>().ok().filter(|id| *id != 0).map(u64::from)
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

pub const MAX_VIRTUAL_MONITORS: usize = 3;

/// A monitor that exists only in VR, added to the desktop by WayVR
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualMonitor {
	pub name: String,
	pub width: u32,
	pub height: u32,
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
	/// Changes with each statistics sample, and when they are cleared
	pub stats_generation: u64,
	pub link: Link,
	pub headset_connected: bool,
	pub session_running: bool,
	/// How many sessions the link saw start, to tell a session from the next one
	pub sessions: u64,
	pub system_name: String,
	pub bitrate_bps: u32,
	pub pairing_enabled: bool,
	pub encryption_enabled: bool,
	pub pin: String,
	/// An EnablePairing has no PIN yet; the link asks for it until the session ended
	pub pairing_pending: bool,
	pub known_keys: Vec<KnownKey>,
	pub steam_command: String,
	/// The server's configuration file
	pub server_config: serde_json::Value,
	pub headset: Option<HeadsetSettings>,
	/// SetHeadsetSettings calls not answered yet
	pub headset_requests: u32,
	/// The stream's samples, kept until the headset disconnects: it sends none while it sleeps,
	/// shows its system menu or waits out a stall
	pub stats: VecDeque<HeadsetStats>,
	/// The newest sample is current, false during such a pause. Changes without `stats_generation`.
	pub stats_live: bool,
	pub applications: Option<Vec<VrApplication>>,
	/// The last ListApplications failed; `applications` keeps the list from before
	pub applications_failed: bool,
	/// The `generation` the last StartApplication that failed bumped, and its app id, so the
	/// Library ends a start that never comes
	pub failed_start: Option<(u64, String)>,
	pub messages: VecDeque<Message>,
}

impl LvrrState {
	/// The current sample, None while the headset sends none
	pub fn latest_stats(&self) -> Option<&HeadsetStats> {
		self.stats.back().filter(|_| self.stats_live)
	}

	pub(crate) fn push_stats(&mut self, stats: HeadsetStats) {
		if self.stats.len() == STATS_HISTORY {
			self.stats.pop_front();
		}
		self.stats.push_back(stats);
		self.stats_live = true;
		self.stats_generation += 1;
	}

	/// The stream ended: its statistics go, the next one starts a new history
	pub(crate) fn clear_stats(&mut self) {
		self.stats_live = false;
		if !self.stats.is_empty() {
			self.stats.clear();
			self.stats_generation += 1;
		}
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

	/// Sets the link. Without one, what the server reported goes too: a server sends nothing when it
	/// quits or crashes, a new one has a new PIN, and GetAll fills it in again. Paired headsets,
	/// configuration and messages stay as the last known values; a message may say why it stopped.
	pub(crate) fn set_link(&mut self, link: Link) {
		let mut changed = self.link != link;
		self.link = link;
		if link != Link::Online {
			changed |= self.headset_connected
				|| self.session_running
				|| self.pairing_enabled
				|| self.encryption_enabled
				|| !self.pin.is_empty()
				|| self.pairing_pending
				|| self.headset.is_some()
				|| !self.stats.is_empty();
			self.headset_connected = false;
			self.session_running = false;
			self.pairing_enabled = false;
			self.encryption_enabled = false;
			self.pin.clear();
			// the link stops waiting for this server's PIN
			self.pairing_pending = false;
			self.headset = None;
			self.clear_stats();
			// queued calls may wait long for a new link, the Headset page shouldn't
			self.headset_requests = 0;
		}
		if changed {
			self.changed();
		}
	}
}

#[derive(Clone, Debug)]
pub enum LvrrRequest {
	/// A JSON object with headset config keys
	SetHeadsetSettings(serde_json::Value),
	/// Applies connection-time settings; closes all XR applications including the dashboard
	RestartSession,
	/// Ends the session; the headset goes back to its server list
	StopSession,
	Disconnect,
	Quit,
	/// Shows a PIN for a new headset. The server refuses it while a session runs or a headset
	/// connects; the link then asks again until the session ended, see `pairing_pending`.
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
	/// The headset draws the hands over games while the dashboard is open
	SetDashboardVisible(bool),
}

impl LvrrRequest {
	/// The D-Bus method it calls, also its name in logs; SetServerConfig sets the JsonConfiguration
	/// property instead
	pub const fn name(&self) -> &'static str {
		match self {
			LvrrRequest::SetHeadsetSettings(_) => "SetHeadsetSettings",
			LvrrRequest::RestartSession => "RestartSession",
			LvrrRequest::StopSession => "StopSession",
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
			LvrrRequest::SetDashboardVisible(_) => "SetDashboardVisible",
		}
	}

	/// Translation key of what failed, in the dashboard's words. None for the dashboard's own
	/// report, which the user didn't ask for.
	pub const fn failure_key(&self) -> Option<&'static str> {
		match self {
			LvrrRequest::SetHeadsetSettings(_) => Some("LVRR.FAILED.HEADSET_SETTINGS"),
			LvrrRequest::RestartSession => Some("LVRR.FAILED.RESTART_SESSION"),
			LvrrRequest::StopSession => Some("LVRR.FAILED.STOP_SESSION"),
			LvrrRequest::Disconnect => Some("LVRR.FAILED.DISCONNECT"),
			LvrrRequest::Quit => Some("LVRR.FAILED.QUIT"),
			LvrrRequest::EnablePairing { .. } => Some("LVRR.FAILED.PAIR"),
			LvrrRequest::DisablePairing => Some("LVRR.FAILED.STOP_PAIRING"),
			LvrrRequest::RevokeKey(_) => Some("LVRR.FAILED.FORGET"),
			LvrrRequest::RenameKey { .. } => Some("LVRR.FAILED.RENAME"),
			LvrrRequest::SetServerConfig(_) => Some("LVRR.FAILED.SERVER_SETTINGS"),
			LvrrRequest::RefreshApplications => Some("LVRR.FAILED.LIST_APPS"),
			LvrrRequest::StartApplication(_) => Some("LVRR.FAILED.START_APP"),
			LvrrRequest::StopApplication(_) => Some("LVRR.FAILED.STOP_APP"),
			LvrrRequest::SetDashboardVisible(_) => None,
		}
	}
}

/// Why a request failed
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
	/// One the dashboard explains in its own words
	Known(Reason),
	/// Anything else, in the server's words
	Other(String),
}

/// Request failures the dashboard explains, see LVRR's `docs/dbus.md`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
	/// SetHeadsetSettings had no answer within 3 s. The headset may still apply the change,
	/// HeadsetSettings then shows it, so the dashboards don't title it as a failure.
	HeadsetSlow,
	/// No headset is connected, and none is known
	NoHeadset,
	/// No headset session is running
	NoSession,
	/// EnablePairing stayed refused while the link waited for the session to end: it didn't end,
	/// or another one started
	Busy,
	/// The server didn't answer, or quit
	ServerGone,
}

impl Reason {
	/// Translation key of the explanation
	pub const fn key(self) -> &'static str {
		match self {
			Self::HeadsetSlow => "LVRR.REASON.HEADSET_SLOW",
			Self::NoHeadset => "LVRR.REASON.NO_HEADSET",
			Self::NoSession => "LVRR.REASON.NO_SESSION",
			Self::Busy => "LVRR.REASON.BUSY",
			Self::ServerGone => "LVRR.REASON.SERVER_GONE",
		}
	}
}

/// Things the dashboard should tell the user about
#[derive(Clone, Debug)]
pub enum Notice {
	Message(Message),
	/// `title` is the translation key of what failed, from [`LvrrRequest::failure_key`]
	RequestFailed {
		title: &'static str,
		failure: Failure,
	},
}

/// The LVRR server, or a stand-in for it
pub trait LvrrBackend {
	/// Applies what happened since the last call and returns what to tell the user
	fn poll(&mut self) -> Vec<Notice>;
	fn state(&self) -> &LvrrState;
	fn request(&mut self, request: LvrrRequest) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
	use super::*;

	// A server that stopped while pairing leaves no PIN behind, and keeps its headsets listed
	#[test]
	fn link_loss_clears_the_server_state() {
		let mut state = LvrrState {
			link: Link::Online,
			headset_connected: true,
			session_running: true,
			pairing_enabled: true,
			encryption_enabled: true,
			pin: "482193".into(),
			pairing_pending: true,
			known_keys: vec![KnownKey {
				name: "Quest 3".into(),
				public_key: "key".into(),
				last_connection: None,
			}],
			headset: Some(HeadsetSettings::default()),
			..Default::default()
		};
		state.push_stats(HeadsetStats::default());
		state.set_link(Link::ServerAbsent);
		assert!(!state.pairing_enabled && state.pin.is_empty() && !state.pairing_pending);
		assert!(!state.headset_connected && !state.session_running);
		assert!(state.headset.is_none() && state.stats.is_empty());
		// a step for the sample and one for clearing it, so PC > Performance sees the gap
		assert_eq!(state.stats_generation, 2);
		assert_eq!(state.known_keys.len(), 1);
		assert_eq!(state.generation, 1);

		// the link worker repeats a missing link, which shouldn't rebuild the pages each time
		state.set_link(Link::ServerAbsent);
		assert_eq!(state.generation, 1);
		state.set_link(Link::Online);
		assert_eq!(state.generation, 2);
	}

	// Only Steam app ids get a cover and the controllers menu
	#[test]
	fn steam_app_ids_are_32_bit() {
		let app = |id: &str| VrApplication {
			id: id.into(),
			name: String::new(),
			icon: String::new(),
		};
		assert_eq!(app("620980").steam_app_id(), Some(620980));
		// a non-Steam game added to Steam, as steam_info.cpp lists it
		let shortcut = ((3_145_728_000u64 << 32) | 0x0200_0000).to_string();
		assert_eq!(app(&shortcut).steam_app_id(), None);
		assert_eq!(app("0").steam_app_id(), None);
		assert_eq!(app("org.example.vrapp").steam_app_id(), None);
	}
}
