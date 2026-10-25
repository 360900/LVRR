//! The dashboard's view of the system in the desktop window: the LVRR server and nothing else.
//! VR apps, overlays and the playspace only exist inside a headset session.

use std::{
	cell::RefCell,
	io::Write,
	path::PathBuf,
	process::{Command, Stdio},
	rc::Rc,
	sync::mpsc::{self, Receiver, Sender},
	thread::sleep,
	time::{Duration, Instant},
};

use dbus::blocking::{Connection, stdintf::org_freedesktop_dbus::Properties};
use lvrr_common::state::{LvrrBackend, LvrrRequest, LvrrState, Notice};
use wayvr_ipc::{
	packet_client::WvrProcessLaunchParams,
	packet_server::{WvrProcess, WvrProcessHandle, WvrWindow, WvrWindowHandle},
};
use wgui::i18n::Locale;
use winit::event_loop::EventLoopProxy;
use wlx_common::{
	config::GeneralConfig,
	dash_interface::{
		ConfigChangeKind, DashBackendTask, DashInterface, DashPlayspaceTask, HmdStats, InterfaceFeats, MonadoClient,
		MonadoDumpSessionFrame,
	},
	desktop_finder::DesktopFinder,
};

use crate::UserEvent;

const SERVER: &str = "io.github.wivrn.Server";
const SERVER_PATH: &str = "/io/github/wivrn/Server";
/// How long a PIN works, like `lvrr pair`'s
const PAIRING_SECS: i32 = 120;
/// How long the old session gets to end before pairing gives up
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(15);

pub struct UiInterface {
	lvrr: Box<dyn LvrrBackend>,
	desktop_finder: DesktopFinder,
	general_config: GeneralConfig,
	/// Messages for the window to show as toasts
	toasts: Rc<RefCell<Vec<String>>>,
	pairing: Option<Receiver<anyhow::Result<()>>>,
	proxy: EventLoopProxy<UserEvent>,
}

impl UiInterface {
	pub fn new(
		lvrr: Box<dyn LvrrBackend>,
		locale: &Locale,
		toasts: Rc<RefCell<Vec<String>>>,
		proxy: EventLoopProxy<UserEvent>,
	) -> Self {
		// serde defaults, not WayVR's config: nothing here is saved. The panel is see-through
		// like in VR.
		let general_config: GeneralConfig = serde_json::from_str("{}").unwrap();
		Self {
			lvrr,
			desktop_finder: DesktopFinder::new(locale),
			general_config,
			toasts,
			pairing: None,
			proxy,
		}
	}
}

/// This install's `lvrr`, next to `lvrr-ui`, or the one on PATH
pub fn lvrr_command() -> PathBuf {
	std::env::current_exe()
		.ok()
		.and_then(|exe| Some(exe.canonicalize().ok()?.parent()?.join("lvrr")))
		.filter(|path| path.is_file())
		.unwrap_or_else(|| "lvrr".into())
}

/// What `lvrr pair` does: ends a session, which refuses pairing, then shows a PIN
fn pair() -> anyhow::Result<()> {
	let conn = Connection::new_session()?;
	let server = conn.with_proxy(SERVER, SERVER_PATH, Duration::from_secs(5));
	let busy = |property| server.get::<bool>(SERVER, property);
	if busy("HeadsetConnected")? || busy("SessionRunning")? {
		server.method_call::<(), _, _, _>(SERVER, "StopSession", ())?;
	}
	let deadline = Instant::now() + SESSION_END_TIMEOUT;
	loop {
		if !busy("SessionRunning")? {
			let (pin,): (String,) = server.method_call(SERVER, "EnablePairing", (PAIRING_SECS,))?;
			if !pin.is_empty() {
				return Ok(());
			}
		}
		if Instant::now() > deadline {
			anyhow::bail!(
				"The headset session didn't end, so pairing couldn't start. Quit LVRR on the headset and try again."
			);
		}
		sleep(Duration::from_millis(250));
	}
}

impl DashInterface<()> for UiInterface {
	fn lvrr_state<'a>(&'a mut self, _: &'a mut ()) -> Option<&'a LvrrState> {
		let mut toasts = self.toasts.borrow_mut();
		for notice in self.lvrr.poll() {
			match notice {
				Notice::Message(message) => toasts.push(message.text),
				Notice::RequestFailed { request, error } => toasts.push(format!("{request} failed: {error}")),
			}
		}
		if let Some(result) = self.pairing.as_ref().and_then(|r| r.try_recv().ok()) {
			self.pairing = None;
			if let Err(e) = result {
				toasts.push(e.to_string());
			}
		}
		Some(self.lvrr.state())
	}

	fn lvrr_request(&mut self, _: &mut (), request: LvrrRequest) -> anyhow::Result<()> {
		self.lvrr.request(request)
	}

	fn lvrr_desktop(&self) -> bool {
		true
	}

	fn lvrr_start_server(&mut self, _: &mut ()) -> anyhow::Result<()> {
		// it reports failures as notifications, so it can run on its own
		Command::new(lvrr_command()).arg("start").stdin(Stdio::null()).spawn()?;
		Ok(())
	}

	fn lvrr_pair(&mut self, _: &mut ()) -> anyhow::Result<()> {
		if self.pairing.is_some() {
			return Ok(());
		}
		let state = self.lvrr.state();
		if !state.session_running && !state.headset_connected {
			return self.lvrr.request(LvrrRequest::EnablePairing {
				timeout_secs: PAIRING_SECS,
			});
		}
		// the link has no request for ending a session first
		let (tx, rx): (Sender<_>, _) = mpsc::channel();
		std::thread::spawn(move || {
			let _ = tx.send(pair());
		});
		self.pairing = Some(rx);
		Ok(())
	}

	fn clipboard_set_text(&mut self, _: &mut (), text: &str) -> anyhow::Result<()> {
		for (program, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
			let Ok(mut child) = Command::new(program).args(args).stdin(Stdio::piped()).spawn() else {
				continue;
			};
			if let Some(mut stdin) = child.stdin.take() {
				stdin.write_all(text.as_bytes())?;
			}
			child.wait()?;
			return Ok(());
		}
		anyhow::bail!("Install wl-clipboard or xclip to copy text")
	}

	fn toggle_dashboard(&mut self, _: &mut ()) {
		let _ = self.proxy.send_event(UserEvent::Close);
	}

	fn general_config<'a>(&'a mut self, _: &'a mut ()) -> &'a mut GeneralConfig {
		&mut self.general_config
	}

	fn desktop_finder<'a>(&'a mut self, _: &'a mut ()) -> &'a mut DesktopFinder {
		&mut self.desktop_finder
	}

	fn get_feats(&mut self, _: &mut ()) -> InterfaceFeats {
		InterfaceFeats::default_for_backend(wlx_common::XrBackend::OpenXR)
	}

	// The rest belongs to a headset session, which the desktop window is not part of

	fn window_list(&mut self, _: &mut ()) -> anyhow::Result<Vec<WvrWindow>> {
		Ok(Vec::new())
	}

	fn window_set_visible(&mut self, _: &mut (), _: WvrWindowHandle, _: bool) -> anyhow::Result<()> {
		anyhow::bail!("No desktop windows here")
	}

	fn window_request_close(&mut self, _: &mut (), _: WvrWindowHandle) -> anyhow::Result<()> {
		anyhow::bail!("No desktop windows here")
	}

	fn process_get(&mut self, _: &mut (), _: WvrProcessHandle) -> Option<WvrProcess> {
		None
	}

	fn process_launch(&mut self, _: &mut (), _: bool, _: WvrProcessLaunchParams) -> anyhow::Result<WvrProcessHandle> {
		anyhow::bail!("Apps start in VR")
	}

	fn process_list(&mut self, _: &mut ()) -> anyhow::Result<Vec<WvrProcess>> {
		Ok(Vec::new())
	}

	fn process_terminate(&mut self, _: &mut (), _: WvrProcessHandle) -> anyhow::Result<()> {
		anyhow::bail!("Apps start in VR")
	}

	fn monado_client_list(&mut self, _: &mut (), _: bool) -> anyhow::Result<Vec<MonadoClient>> {
		Ok(Vec::new())
	}

	fn monado_client_focus(&mut self, _: &mut (), _: i64) -> anyhow::Result<()> {
		anyhow::bail!("VR apps are managed in VR")
	}

	fn monado_brightness_get(&mut self, _: &mut ()) -> Option<f32> {
		None
	}

	fn monado_brightness_set(&mut self, _: &mut (), _: f32) -> Option<()> {
		None
	}

	fn monado_metrics_set_enabled(&mut self, _: &mut (), _: bool) -> bool {
		false
	}

	fn monado_metrics_dump_session_frames(&mut self, _: &mut ()) -> Vec<MonadoDumpSessionFrame> {
		Vec::new()
	}

	fn playspace_task(&mut self, _: &mut (), _: DashPlayspaceTask) -> anyhow::Result<()> {
		anyhow::bail!("The playspace is set in VR")
	}

	fn backend_task(&mut self, _: &mut (), _: DashBackendTask) -> anyhow::Result<()> {
		anyhow::bail!("Bindings are set in VR")
	}

	fn config_changed(&mut self, _: &mut (), _: ConfigChangeKind) {}

	fn restart(&mut self, _: &mut ()) {}

	fn close(&mut self, _: &mut ()) {}

	fn hmd_stats(&mut self, _: &mut ()) -> HmdStats {
		HmdStats::default()
	}

	fn enumerate_vulkan_devices(&mut self, _: &mut ()) -> anyhow::Result<Vec<String>> {
		Ok(Vec::new())
	}
}
