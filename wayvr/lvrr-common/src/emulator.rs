//! A stand-in for the LVRR server, for working on the dashboard without one (uidev).
//! `LVRR_EMU=absent` emulates a server that isn't running.

use std::time::Instant;

use crate::{
	headset::{HeadsetSettings, HeadsetStats},
	state::{KnownKey, Link, LvrrBackend, LvrrRequest, LvrrState, Message, MessageKind, Notice, VrApplication},
};

// Written by the server's `lvrr-test --example`
const EXAMPLE: &str = include_str!("../../../docs/dbus-example.json");

pub struct LvrrEmulator {
	state: LvrrState,
	/// The headset settings as JSON, so patches merge like on the headset
	settings: serde_json::Value,
	stats: HeadsetStats,
	started: Instant,
	last_stats: Instant,
	notices: Vec<Notice>,
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
		}
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
		let mut settings: HeadsetSettings = serde_json::from_value(self.settings.clone()).unwrap_or_default();
		settings.restart_required = ["resolution_scale", "foveation", "codec", "bit_depth", "microphone"]
			.iter()
			.any(|k| patch.contains_key(*k))
			|| self.state.headset.as_ref().is_some_and(|h| h.restart_required);
		self.state.bitrate_bps = settings.config.bitrate_bps;
		self.state.headset = Some(settings);
		self.state.changed();
	}
}

impl LvrrBackend for LvrrEmulator {
	fn poll(&mut self) -> Vec<Notice> {
		if self.state.link == Link::Online && self.last_stats.elapsed().as_secs_f32() >= 1.0 {
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
			LvrrRequest::RestartSession => self.message("Restarting the session"),
			LvrrRequest::Disconnect | LvrrRequest::Quit => {
				self.state.link = Link::ServerAbsent;
				self.state.headset = None;
				self.state.changed();
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
		}
		Ok(())
	}
}
