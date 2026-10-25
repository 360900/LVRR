//! Prints what the dashboard would see from a running LVRR server, for debugging the link.
//! cargo run -p lvrr-common --features dbus --example lvrr_probe [-- bitrate_bps]

use std::time::{Duration, Instant};

use lvrr_common::{
	link::LvrrLink,
	state::{LvrrBackend, LvrrRequest},
};

fn main() -> anyhow::Result<()> {
	let mut link = LvrrLink::start()?;
	let bitrate: Option<u32> = std::env::args().nth(1).and_then(|a| a.parse().ok());
	let start = Instant::now();
	let mut requested = false;
	while start.elapsed() < Duration::from_secs(4) {
		for notice in link.poll() {
			println!("notice: {notice:?}");
		}
		if !requested && link.state().headset.is_some() {
			requested = true;
			link.request(LvrrRequest::RefreshApplications)?;
			if let Some(bitrate) = bitrate {
				link.request(LvrrRequest::SetHeadsetSettings(
					serde_json::json!({ "bitrate_bps": bitrate }),
				))?;
			}
		}
		std::thread::sleep(Duration::from_millis(50));
	}
	let s = link.state();
	println!(
		"link: {:?}, connected: {}, session: {}, system: {:?}",
		s.link, s.headset_connected, s.session_running, s.system_name
	);
	println!(
		"bitrate: {}, pairing: {}, encryption: {}, keys: {:?}",
		s.bitrate_bps, s.pairing_enabled, s.encryption_enabled, s.known_keys
	);
	println!("server config: {}", s.server_config);
	println!("applications: {:?}", s.applications);
	if let Some(h) = &s.headset {
		println!(
			"headset: rates {:?}, codecs {:?}, bitrate {}, restart_required {}",
			h.capabilities.refresh_rates, h.capabilities.codecs, h.config.bitrate_bps, h.restart_required
		);
	}
	println!("stats samples: {}", s.stats.len());
	Ok(())
}
