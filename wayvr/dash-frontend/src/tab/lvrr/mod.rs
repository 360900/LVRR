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

	use lvrr_common::{
		emulator::LvrrEmulator,
		state::{LvrrRequest, LvrrState},
	};
	use wayvr_ipc::{
		packet_client::WvrProcessLaunchParams,
		packet_server::{WvrProcess, WvrProcessHandle, WvrWindow, WvrWindowHandle},
	};
	use wgui::{i18n::Locale, theme::WguiTheme};
	use wlx_common::{
		config::GeneralConfig,
		dash_interface::{
			BoxDashInterface, ConfigChangeKind, DashBackendTask, DashInterface, DashPlayspaceTask, HmdStats, InterfaceFeats,
			MonadoClient, MonadoDumpSessionFrame,
		},
		dash_interface_emulated::DashInterfaceEmulated,
		desktop_finder::DesktopFinder,
		locale::WayVRLangProvider,
	};

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

	/// The desktop window has no Library or Running page
	const DESKTOP_PAGES: [&str; 8] = [
		"home",
		"headset:video",
		"headset:tracking",
		"pc:performance",
		"pc:headsets",
		"pc:server",
		"pc:diagnostics",
		"pc:about",
	];

	/// A headset streaming, a server with an offline headset, a stopped server, and no D-Bus
	fn emulators() -> [Option<LvrrEmulator>; 4] {
		[
			Some(LvrrEmulator::new(true)),
			Some(LvrrEmulator::offline()),
			Some(LvrrEmulator::new(false)),
			None,
		]
	}

	fn frontend(interface: BoxDashInterface<()>, lang_provider: &WayVRLangProvider) -> Frontend<()> {
		Frontend::new(InitParams {
			interface,
			show_welcome: false,
			has_monado: true,
			lang_provider,
			theme: Rc::new(WguiTheme::default()),
			color_palette: "Default",
			executor: wlx_common::async_executor::create_local(),
		})
		.unwrap()
	}

	fn update(frontend: &mut Frontend<()>, what: &str) {
		for _ in 0..3 {
			let params = FrontendUpdateParams {
				data: &mut (),
				width: 1280.0,
				height: 960.0,
				timestep_alpha: 0.0,
			};
			if let Err(e) = frontend.update(params) {
				panic!("{what}: {e:?}");
			}
		}
	}

	/// Every page builds in VR, for each state of the server
	#[test]
	fn pages_open() {
		let lang_provider = WayVRLangProvider::default();
		let locale = Locale::from_env(&lang_provider);
		for (n, lvrr) in emulators().into_iter().enumerate() {
			let interface = DashInterfaceEmulated::new_headless(&locale, lvrr);
			let mut frontend = frontend(Box::new(interface), &lang_provider);
			for page in PAGES {
				frontend.lvrr_open(page);
				update(&mut frontend, &format!("{page} with LVRR emulator {n}"));
			}
		}
	}

	/// The same in the desktop window, and pairing from its tray
	#[test]
	fn desktop_pages_open() {
		let lang_provider = WayVRLangProvider::default();
		let locale = Locale::from_env(&lang_provider);
		for (n, lvrr) in emulators().into_iter().enumerate() {
			let interface = Desktop(DashInterfaceEmulated::new_headless(&locale, lvrr));
			let mut frontend = frontend(Box::new(interface), &lang_provider);
			for page in DESKTOP_PAGES {
				frontend.lvrr_open(page);
				update(&mut frontend, &format!("desktop {page} with LVRR emulator {n}"));
			}
			frontend.lvrr_start_pairing();
			update(&mut frontend, &format!("desktop pairing with LVRR emulator {n}"));
		}
	}

	/// The emulator as the desktop window sees it
	struct Desktop(DashInterfaceEmulated);

	impl DashInterface<()> for Desktop {
		fn lvrr_desktop(&self) -> bool {
			true
		}
		fn lvrr_pair(&mut self, data: &mut ()) -> anyhow::Result<()> {
			self
				.0
				.lvrr_request(data, LvrrRequest::EnablePairing { timeout_secs: 120 })
		}
		fn lvrr_state<'a>(&'a mut self, data: &'a mut ()) -> Option<&'a LvrrState> {
			self.0.lvrr_state(data)
		}
		fn lvrr_request(&mut self, data: &mut (), request: LvrrRequest) -> anyhow::Result<()> {
			self.0.lvrr_request(data, request)
		}
		fn window_list(&mut self, data: &mut ()) -> anyhow::Result<Vec<WvrWindow>> {
			self.0.window_list(data)
		}
		fn window_set_visible(&mut self, data: &mut (), handle: WvrWindowHandle, visible: bool) -> anyhow::Result<()> {
			self.0.window_set_visible(data, handle, visible)
		}
		fn window_request_close(&mut self, data: &mut (), handle: WvrWindowHandle) -> anyhow::Result<()> {
			self.0.window_request_close(data, handle)
		}
		fn process_get(&mut self, data: &mut (), handle: WvrProcessHandle) -> Option<WvrProcess> {
			self.0.process_get(data, handle)
		}
		fn process_launch(
			&mut self,
			data: &mut (),
			auto_start: bool,
			params: WvrProcessLaunchParams,
		) -> anyhow::Result<WvrProcessHandle> {
			self.0.process_launch(data, auto_start, params)
		}
		fn process_list(&mut self, data: &mut ()) -> anyhow::Result<Vec<WvrProcess>> {
			self.0.process_list(data)
		}
		fn process_terminate(&mut self, data: &mut (), handle: WvrProcessHandle) -> anyhow::Result<()> {
			self.0.process_terminate(data, handle)
		}
		fn monado_client_list(&mut self, _: &mut (), _: bool) -> anyhow::Result<Vec<MonadoClient>> {
			Ok(Vec::new())
		}
		fn monado_client_focus(&mut self, data: &mut (), client_id: i64) -> anyhow::Result<()> {
			self.0.monado_client_focus(data, client_id)
		}
		fn monado_brightness_get(&mut self, data: &mut ()) -> Option<f32> {
			self.0.monado_brightness_get(data)
		}
		fn monado_brightness_set(&mut self, data: &mut (), brightness: f32) -> Option<()> {
			self.0.monado_brightness_set(data, brightness)
		}
		fn monado_metrics_set_enabled(&mut self, data: &mut (), enabled: bool) -> bool {
			self.0.monado_metrics_set_enabled(data, enabled)
		}
		fn monado_metrics_dump_session_frames(&mut self, data: &mut ()) -> Vec<MonadoDumpSessionFrame> {
			self.0.monado_metrics_dump_session_frames(data)
		}
		fn playspace_task(&mut self, data: &mut (), task: DashPlayspaceTask) -> anyhow::Result<()> {
			self.0.playspace_task(data, task)
		}
		fn backend_task(&mut self, data: &mut (), task: DashBackendTask) -> anyhow::Result<()> {
			self.0.backend_task(data, task)
		}
		fn desktop_finder<'a>(&'a mut self, data: &'a mut ()) -> &'a mut DesktopFinder {
			self.0.desktop_finder(data)
		}
		fn general_config<'a>(&'a mut self, data: &'a mut ()) -> &'a mut GeneralConfig {
			self.0.general_config(data)
		}
		fn config_changed(&mut self, data: &mut (), kind: ConfigChangeKind) {
			self.0.config_changed(data, kind);
		}
		fn restart(&mut self, data: &mut ()) {
			self.0.restart(data);
		}
		fn close(&mut self, data: &mut ()) {
			self.0.close(data);
		}
		fn toggle_dashboard(&mut self, data: &mut ()) {
			self.0.toggle_dashboard(data);
		}
		fn get_feats(&mut self, _: &mut ()) -> InterfaceFeats {
			InterfaceFeats::default_for_backend(wlx_common::XrBackend::OpenXR)
		}
		fn hmd_stats(&mut self, _: &mut ()) -> HmdStats {
			HmdStats::default()
		}
		fn enumerate_vulkan_devices(&mut self, data: &mut ()) -> anyhow::Result<Vec<String>> {
			self.0.enumerate_vulkan_devices(data)
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
