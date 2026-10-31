//! A stand-in for the LVRR server, for working on the dashboard without one (uidev).
//! `LVRR_EMU=absent` emulates a server that isn't running, `LVRR_EMU=offline` one whose
//! headset is away, with a change waiting for it.

use std::time::Instant;

use crate::{
	headset::{HeadsetSettings, HeadsetStats, RESTART_KEYS},
	state::{
		Failure, KnownKey, Link, LvrrBackend, LvrrRequest, LvrrState, Message, MessageKind, Notice, Reason, VrApplication,
	},
};

/// The server's `HeadsetSettings` and `HeadsetStats`, written by its `lvrr-test --example`
pub const EXAMPLE: &str = include_str!("../../../docs/dbus-example.json");

pub struct LvrrEmulator {
	state: LvrrState,
	/// The headset settings as JSON, so patches merge like on the headset
	settings: serde_json::Value,
	stats: HeadsetStats,
	started: Instant,
	last_stats: Instant,
	notices: Vec<Notice>,
	/// The headset is away; settings changes wait for it
	offline: bool,
	/// The config the emulated session started with, for `restart_required`. None until a
	/// RestartSession: the example's session started with other restart settings, which it doesn't show.
	session: Option<serde_json::Value>,
}

impl LvrrEmulator {
	pub fn new(running: bool) -> Self {
		let example: serde_json::Value = serde_json::from_str(EXAMPLE).expect("invalid docs/dbus-example.json");
		let settings = example["HeadsetSettings"].clone();
		let stats: HeadsetStats = serde_json::from_value(example["HeadsetStats"].clone()).unwrap_or_default();

		let mut state = LvrrState {
			link: if running { Link::Online } else { Link::ServerAbsent },
			..Default::default()
		};
		if running {
			state.headset_connected = true;
			state.session_running = true;
			// as the link counts the session its first GetAll finds
			state.sessions = 1;
			state.system_name = "Meta Quest 3 on LVRR".into();
			state.encryption_enabled = true;
			state.known_keys = vec![
				KnownKey {
					name: "Quest 3".into(),
					public_key: "emulated-key-1".into(),
					last_connection: Some(1_761_350_000),
				},
				KnownKey {
					name: "Pico 4".into(),
					public_key: "emulated-key-2".into(),
					last_connection: None,
				},
			];
			state.steam_command = "PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1 %command%".into();
			state.server_config = serde_json::json!({"application": ["wayvr", "--openxr", "--wait"]});
			state.headset = serde_json::from_value(settings.clone()).ok();
			state.bitrate_bps = state.headset.as_ref().map_or(0, |h| h.config.bitrate_bps);
		}

		Self {
			state,
			settings,
			stats,
			started: Instant::now(),
			last_stats: Instant::now(),
			notices: Vec::new(),
			offline: false,
			session: None,
		}
	}

	/// A running server without a headset, which knows the last one's settings
	pub fn offline() -> Self {
		let mut emulator = Self::new(true);
		emulator.headset_left();
		emulator.state.session_running = false;
		emulator.state.bitrate_bps = 0;
		emulator.set_settings(&serde_json::json!({"bitrate_bps": 80_000_000}));
		emulator
	}

	/// The headset disconnected, as the server sees it: its settings stay, marked offline, and
	/// the session waits for it to come back
	fn headset_left(&mut self) {
		self.offline = true;
		self.state.headset_connected = false;
		self.settings["offline"] = true.into();
		self.settings["restart_required"] = false.into();
		self.settings["pending"] = serde_json::Value::Null;
		self.state.headset = serde_json::from_value(self.settings.clone()).ok();
		// the server's HeadsetStats is "" then
		self.state.clear_stats();
		self.state.changed();
	}

	fn message(&mut self, text: &str) {
		let message = Message {
			kind: MessageKind::Info,
			text: text.into(),
		};
		self.state.push_message(message.clone());
		self.notices.push(Notice::Message(message));
	}

	fn set_settings(&mut self, patch: &serde_json::Value) {
		let (Some(config), Some(patch)) = (self.settings["config"].as_object_mut(), patch.as_object()) else {
			return;
		};
		for (key, value) in patch {
			config.insert(key.clone(), value.clone());
		}
		if self.offline {
			// kept for the next connection, like the server does
			if !self.settings["pending"].is_object() {
				self.settings["pending"] = serde_json::json!({});
			}
			if let Some(pending) = self.settings["pending"].as_object_mut() {
				pending.extend(patch.clone());
			}
			self.state.headset = serde_json::from_value(self.settings.clone()).ok();
			self.state.changed();
			return;
		}
		// as on the headset: due while restart settings differ from the session's. Before a
		// RestartSession they are unknown, and the example's flag stays.
		if let Some(session) = &self.session {
			let restart_required = RESTART_KEYS
				.iter()
				.any(|k| !same_value(&self.settings["config"][*k], &session[*k]));
			self.settings["restart_required"] = restart_required.into();
		}
		let settings: HeadsetSettings = serde_json::from_value(self.settings.clone()).unwrap_or_default();
		self.state.bitrate_bps = settings.config.bitrate_bps;
		self.state.headset = Some(settings);
		self.state.changed();
	}
}

/// Whether the headset stores both as the same value. Its numbers are f32 or integers, so the
/// 1.2000000476837158 it reports is 1.2, and 10.0 is 10.
fn same_value(a: &serde_json::Value, b: &serde_json::Value) -> bool {
	match (a.as_f64(), b.as_f64()) {
		(Some(a), Some(b)) => (a - b).abs() < 1e-4,
		_ => a == b,
	}
}

impl LvrrBackend for LvrrEmulator {
	fn poll(&mut self) -> Vec<Notice> {
		if self.state.link == Link::Online && self.state.headset_connected && self.last_stats.elapsed().as_secs_f32() >= 1.0
		{
			self.last_stats = Instant::now();
			let t = self.started.elapsed().as_secs_f32();
			let wave = |base: f32, amplitude: f32, period: f32| base + amplitude * (t * std::f32::consts::TAU / period).sin();
			let mut stats = self.stats.clone();
			stats.cpu_time_ms = wave(4.0, 1.5, 7.0);
			stats.gpu_time_ms = wave(1.6, 0.5, 5.0);
			stats.rx_bps = wave(self.state.bitrate_bps as f32 * 0.9, 4e6, 11.0);
			stats.motion_to_photon_ms = wave(38.0, 6.0, 9.0);
			for d in &mut stats.decoders {
				d.total_ms = wave(22.0, 4.0, 6.0);
			}
			self.state.push_stats(stats);
		}
		std::mem::take(&mut self.notices)
	}

	fn state(&self) -> &LvrrState {
		&self.state
	}

	fn request(&mut self, request: LvrrRequest) -> anyhow::Result<()> {
		if self.state.link != Link::Online {
			anyhow::bail!("The LVRR server is not running");
		}
		match request {
			LvrrRequest::SetHeadsetSettings(patch) => self.set_settings(&patch),
			LvrrRequest::RestartSession => {
				// the headset comes back to a new session, which starts with the current settings
				if !self.offline {
					self.session = Some(self.settings["config"].clone());
					self.settings["restart_required"] = false.into();
					self.state.headset = serde_json::from_value(self.settings.clone()).ok();
					// a new session as the link counts it, though session_running doesn't drop here
					self.state.sessions += 1;
					self.state.changed();
				}
				self.message("Restarting the session");
			}
			LvrrRequest::StopSession => {
				if self.state.headset_connected {
					self.headset_left();
				}
				self.state.session_running = false;
				self.state.changed();
			}
			LvrrRequest::Disconnect => {
				if self.state.headset_connected {
					self.headset_left();
				}
			}
			LvrrRequest::Quit => self.state.set_link(Link::ServerAbsent),
			// what the link reports once the session didn't end
			request @ LvrrRequest::EnablePairing { .. } if self.state.session_running || self.state.headset_connected => {
				if let Some(title) = request.failure_key() {
					self.notices.push(Notice::RequestFailed {
						title,
						failure: Failure::Known(Reason::Busy),
					});
				}
			}
			LvrrRequest::EnablePairing { .. } => {
				self.state.pairing_enabled = true;
				self.state.pin = "482193".into();
				self.state.changed();
			}
			LvrrRequest::DisablePairing => {
				self.state.pairing_enabled = false;
				self.state.pin.clear();
				self.state.changed();
			}
			LvrrRequest::RevokeKey(key) => {
				self.state.known_keys.retain(|k| k.public_key != key);
				self.state.changed();
			}
			LvrrRequest::RenameKey { key, name } => {
				if let Some(k) = self.state.known_keys.iter_mut().find(|k| k.public_key == key) {
					k.name = name;
				}
				self.state.changed();
			}
			LvrrRequest::SetServerConfig(config) => {
				self.state.server_config = config;
				self.state.changed();
			}
			LvrrRequest::RefreshApplications => {
				self.state.applications = Some(vec![
					VrApplication {
						id: "546560".into(),
						name: "Half-Life: Alyx".into(),
						icon: String::new(),
					},
					VrApplication {
						id: "620980".into(),
						name: "Beat Saber".into(),
						icon: String::new(),
					},
					VrApplication {
						id: "org.example.vrapp".into(),
						name: "Example VR app".into(),
						icon: "applications-games".into(),
					},
				]);
				self.state.changed();
			}
			LvrrRequest::StartApplication(id) => self.message(&format!("Starting {id}")),
			LvrrRequest::StopApplication(id) => self.message(&format!("Stopping client {id}")),
			LvrrRequest::SetDashboardVisible(_) => {}
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn offline_keeps_changes() {
		let mut emulator = LvrrEmulator::offline();
		let headset = emulator.state().headset.clone().unwrap();
		assert!(headset.offline && !headset.restart_required);
		assert!(!emulator.state().headset_connected);
		assert_eq!(headset.pending, serde_json::json!({"bitrate_bps": 80_000_000}));
		assert_eq!(headset.config.bitrate_bps, 80_000_000);

		emulator
			.request(LvrrRequest::SetHeadsetSettings(serde_json::json!({"codec": "h265"})))
			.unwrap();
		let headset = emulator.state().headset.clone().unwrap();
		assert!(headset.offline && !headset.restart_required);
		assert_eq!(
			headset.pending,
			serde_json::json!({"bitrate_bps": 80_000_000, "codec": "h265"})
		);
		assert!(emulator.poll().is_empty());
		assert!(emulator.state().stats.is_empty());
	}

	// Like the server: it keeps running, the session waits for the headset, changes wait too
	#[test]
	fn disconnect_keeps_the_server() {
		let mut emulator = LvrrEmulator::new(true);
		emulator.state.push_stats(HeadsetStats::default());
		let stats_generation = emulator.state.stats_generation;
		emulator.request(LvrrRequest::Disconnect).unwrap();
		let state = emulator.state();
		assert_eq!(state.link, Link::Online);
		assert!(!state.headset_connected && state.session_running);
		assert!(state.headset.as_ref().is_some_and(|h| h.offline && !h.restart_required));
		assert!(state.stats.is_empty());
		// a step for clearing them, like the link's, so PC > Performance sees the gap
		assert_eq!(state.stats_generation, stats_generation + 1);

		emulator
			.request(LvrrRequest::SetHeadsetSettings(serde_json::json!({"codec": "h265"})))
			.unwrap();
		let headset = emulator.state().headset.clone().unwrap();
		assert_eq!(headset.pending, serde_json::json!({"codec": "h265"}));
	}

	// The server refuses to pair during a session, lvrr-ui ends it first
	#[test]
	fn pairing_ends_the_session_first() {
		let mut emulator = LvrrEmulator::new(true);
		let pair = || LvrrRequest::EnablePairing { timeout_secs: 120 };
		emulator.request(pair()).unwrap();
		assert!(matches!(
			&emulator.poll()[..],
			[Notice::RequestFailed {
				title: "LVRR.FAILED.PAIR",
				failure: Failure::Known(Reason::Busy),
			}]
		));
		assert!(!emulator.state().pairing_enabled && emulator.state().pin.is_empty());

		emulator.request(LvrrRequest::StopSession).unwrap();
		let state = emulator.state();
		assert!(!state.session_running && !state.headset_connected);
		assert!(state.headset.as_ref().is_some_and(|h| h.offline));
		emulator.request(pair()).unwrap();
		assert!(emulator.state().pairing_enabled && !emulator.state().pin.is_empty());
		assert!(emulator.poll().is_empty());
	}

	#[test]
	fn quit_leaves_no_server_state() {
		let mut emulator = LvrrEmulator::new(true);
		emulator.request(LvrrRequest::StopSession).unwrap();
		emulator
			.request(LvrrRequest::EnablePairing { timeout_secs: 120 })
			.unwrap();
		assert!(emulator.state().pairing_enabled);
		emulator.request(LvrrRequest::Quit).unwrap();
		let state = emulator.state();
		assert_eq!(state.link, Link::ServerAbsent);
		assert!(!state.headset_connected && !state.session_running && state.headset.is_none());
		assert!(!state.pairing_enabled && state.pin.is_empty());
		assert!(emulator.request(LvrrRequest::DisablePairing).is_err());
	}

	// Like the headset: a restart is due while restart settings differ from the session's
	#[test]
	fn restart_follows_the_session() {
		let mut emulator = LvrrEmulator::new(true);
		let restart_required = |e: &LvrrEmulator| e.state().headset.as_ref().is_some_and(|h| h.restart_required);
		// the example's session started with other restart settings
		assert!(restart_required(&emulator));
		assert_eq!(emulator.state().sessions, 1);
		emulator.request(LvrrRequest::RestartSession).unwrap();
		assert!(!restart_required(&emulator));
		assert_eq!(emulator.state().sessions, 2);

		for (patch, restart) in [
			(serde_json::json!({"hand_tracking": false}), true),
			(serde_json::json!({"hand_tracking": true}), false),
			(serde_json::json!({"bitrate_bps": 30_000_000}), false),
			// the f32 the headset reports, sent as an f64
			(serde_json::json!({"resolution_scale": 1.2}), false),
		] {
			emulator
				.request(LvrrRequest::SetHeadsetSettings(patch.clone()))
				.unwrap();
			assert_eq!(restart_required(&emulator), restart, "after {patch}");
		}
	}
}
