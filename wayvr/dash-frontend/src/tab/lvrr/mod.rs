//! LVRR pages: the streaming, headset and PC features that used to be WiVRn's own UI.

use std::cell::Cell;

use crate::{frontend::Frontend, tab::Tab};
use wgui::layout::WidgetID;

thread_local! {
	/// Section of the open Headset or PC page
	static SECTION: Cell<&'static str> = const { Cell::new("") };
}

pub(crate) fn set_section(name: &'static str) {
	SECTION.set(name);
}

pub(crate) fn section() -> &'static str {
	SECTION.get()
}

pub mod headset;
pub mod home;
pub mod library;
pub mod pc;
pub mod running;
mod ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
	Library,
	Running,
	Headset,
	Pc,
}

impl Page {
	/// Title translation key and icon
	pub const fn title_icon(self) -> (&'static str, &'static str) {
		match self {
			Page::Library => ("LVRR.LIBRARY.TITLE", "dashboard/games.svg"),
			Page::Running => ("LVRR.RUNNING.TITLE", "dashboard/play.svg"),
			Page::Headset => ("LVRR.HEADSET.TITLE", "dashboard/wivrn_head_symbolic.svg"),
			Page::Pc => ("LVRR.PC.TITLE", "dashboard/display.svg"),
		}
	}
}

pub fn create<T: 'static>(
	page: Page,
	frontend: &mut Frontend<T>,
	parent: WidgetID,
	data: &mut T,
) -> anyhow::Result<Box<dyn Tab<T>>> {
	Ok(match page {
		Page::Library => Box::new(library::TabLvrrLibrary::new(frontend, parent, data)?),
		Page::Running => Box::new(running::TabLvrrRunning::new(frontend, parent, data)?),
		Page::Headset => Box::new(headset::TabLvrrHeadset::new(frontend, parent, data)?),
		Page::Pc => Box::new(pc::TabLvrrPc::new(frontend, parent, data)?),
	})
}

#[cfg(test)]
mod tests {
	use std::rc::Rc;

	use lvrr_common::emulator::LvrrEmulator;
	use wgui::{i18n::Locale, theme::WguiTheme};
	use wlx_common::{dash_interface_emulated::DashInterfaceEmulated, locale::WayVRLangProvider};

	use crate::frontend::{Frontend, FrontendUpdateParams, InitParams};

	const PAGES: [&str; 15] = [
		"home",
		"library",
		"running",
		"headset:video",
		"headset:streaming",
		"headset:post_processing",
		"headset:audio",
		"headset:devices",
		"headset:tracking",
		"headset:system",
		"pc:performance",
		"pc:headsets",
		"pc:server",
		"pc:diagnostics",
		"pc:about",
	];

	/// Every page builds, with a headset streaming, with the server stopped and without D-Bus
	#[test]
	fn pages_open() {
		let lang_provider = WayVRLangProvider::default();
		let locale = Locale::from_env(&lang_provider);
		for lvrr in [Some(true), Some(false), None] {
			let interface = DashInterfaceEmulated::new_headless(&locale, lvrr.map(LvrrEmulator::new));
			let mut frontend = Frontend::new(InitParams {
				interface: Box::new(interface),
				show_welcome: false,
				has_monado: true,
				lang_provider: &lang_provider,
				theme: Rc::new(WguiTheme::default()),
				color_palette: "Default",
				executor: wlx_common::async_executor::create_local(),
			})
			.unwrap();
			for page in PAGES {
				frontend.lvrr_open(page);
				for _ in 0..3 {
					let params = FrontendUpdateParams {
						data: &mut (),
						width: 1280.0,
						height: 960.0,
						timestep_alpha: 0.0,
					};
					if let Err(e) = frontend.update(params) {
						panic!("{page} with LVRR emulator {lvrr:?}: {e:?}");
					}
				}
			}
		}
	}

	const EN: &str = include_str!("../../../lvrr-assets/lang/en.json");

	const SOURCES: [(&str, &str); 12] = [
		("home.rs", include_str!("home.rs")),
		("library.rs", include_str!("library.rs")),
		("running.rs", include_str!("running.rs")),
		("headset.rs", include_str!("headset.rs")),
		("pc.rs", include_str!("pc.rs")),
		("ui.rs", include_str!("ui.rs")),
		("lvrr_ext.rs", include_str!("../../frontend/lvrr_ext.rs")),
		("frontend.rs", include_str!("../../frontend.rs")),
		("dashboard.xml", include_str!("../../../assets/gui/dashboard.xml")),
		("headset.xml", include_str!("../../../lvrr-assets/gui/lvrr/headset.xml")),
		("pc.xml", include_str!("../../../lvrr-assets/gui/lvrr/pc.xml")),
		("widgets.xml", include_str!("../../../lvrr-assets/gui/lvrr/widgets.xml")),
	];

	fn exists(en: &serde_json::Value, key: &str) -> bool {
		key
			.split('.')
			.try_fold(en, |v, k| v.get(k))
			.is_some_and(serde_json::Value::is_string)
	}

	/// Every `LVRR.*` translation key the pages use has an English string
	#[test]
	fn translation_keys_exist() {
		let en: serde_json::Value = serde_json::from_str(EN).unwrap();
		let mut keys = 0;
		for (file, source) in SOURCES {
			for (start, _) in source.match_indices("\"LVRR.") {
				let key = &source[start + 1..];
				let key = &key[..key.find('"').unwrap()];
				// built at runtime, checked below
				if key.contains('{') {
					continue;
				}
				assert!(exists(&en, key), "{file}: {key} is missing from en.json");
				keys += 1;
			}
		}
		assert!(keys > 100, "found only {keys} keys");
		for device in lvrr_common::state::BatteryDevice::ALL {
			assert!(exists(&en, device.key()), "{} is missing from en.json", device.key());
		}
		for part in lvrr_common::headset::BODY_PARTS {
			let key = format!("LVRR.HEADSET.BODY.{}", part.to_uppercase());
			assert!(exists(&en, &key), "{key} is missing from en.json");
		}
	}
}
