//! The window around the dashboard, and the tray that outlives it.

use std::{
	cell::RefCell,
	process::{Command, Stdio},
	rc::Rc,
	sync::Arc,
	time::{Duration, Instant},
};

use dash_frontend::frontend::{Frontend, FrontendTask, FrontendUpdateParams, InitParams};
use glam::{Vec2, vec2};
use ksni::blocking::Handle;
use lvrr_common::{
	emulator::LvrrEmulator,
	link::LvrrLink,
	lvrr_command,
	state::{Failure, Link, LvrrBackend, Notice, Reason},
};
use wgui::{
	drawing::Boundary,
	event::{
		DeviceBitmask, MouseButtonEvent, MouseButtonIndex, MouseLeaveEvent, MouseMotionEvent, MouseWheelEvent,
		TextInputEvent,
	},
	gfx::{
		CommandBufferUsage, DevicePreference, PresentStatus, Swapchain, SwapchainError, WGfx, WGfxClearMode, WGfxCreateInfo,
	},
	i18n::{Locale, Translation},
	renderer_vk::context::{Context, SharedContext},
	theme::WguiTheme,
	widget::EventResult,
};
use winit::{
	application::ApplicationHandler,
	dpi::{LogicalSize, PhysicalSize},
	event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
	event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
	platform::{
		startup_notify::WindowAttributesExtStartupNotify,
		wayland::{ActiveEventLoopExtWayland, WindowAttributesExtWayland},
	},
	window::{ActivationToken, CursorIcon, ResizeDirection, UserAttentionType, Window, WindowId},
};
use wlx_common::{
	async_executor::AsyncExecutor,
	audio::{AudioProvider, DummyAudioProvider},
	locale::WayVRLangProvider,
	timestep::Timestep,
};

use crate::{
	LaunchToken, UserEvent, activate,
	instance::Request,
	interface::UiInterface,
	notify,
	tray::{self, Phase, Tray},
};

/// The Wayland app id and X11 class, which match the desktop file
const APP_ID: &str = "io.github.lvrr";
const FRAME: Duration = Duration::from_micros(16_667);
/// The frame time while nobody uses the window, 15 Hz. Timestep runs up to 5 ticks a frame, so
/// animations and toast timers keep time at 4.
const IDLE_FRAME: Duration = Duration::from_micros(66_667);
/// How long input keeps the full frame rate
const ACTIVE_FOR: Duration = Duration::from_secs(2);
/// How long a drawn frame keeps it, so animations and scrolling stay smooth
const DRAWN_FOR: Duration = Duration::from_millis(100);
/// The wait before drawing again after a GPU error, doubled while it keeps failing
const GPU_RETRY: Duration = Duration::from_secs(1);
const GPU_RETRY_MAX: Duration = Duration::from_secs(30);
/// How often the tray follows what the link doesn't wake the loop for: the window's changes to the
/// emulator, and a start of LVRR, which ends when it fails or the link reaches LVRR
const TRAY_INTERVAL: Duration = Duration::from_millis(500);
/// Around the dashboard the desktop shows, like the world in VR
const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
/// How close to the window's edge the pointer resizes it, in logical pixels: past the 4 px
/// see-through rim, 2 px into the panel. The panel's left and top edges sit further in, past the
/// side and title pills, and resize it too. There's no title bar or frame: the dashboard draws
/// everything, and a press where nothing reacts moves the window.
const RESIZE_EDGE: f32 = 6.0;
/// How far a wheel line scrolls a page, in logical pixels: `step_pixels` in wgui's
/// `WidgetState::process_wheel`
const WHEEL_LINE: f32 = 64.0;

/// What draws into the window, dropped after a GPU error and created again
struct Gpu {
	// dropped in this order: the renderer before the swapchain and device
	render: Context,
	shared: SharedContext,
	swapchain: Arc<Swapchain>,
	gfx: Arc<WGfx>,
}

/// The window and what draws into it, dropped when it closes
struct Surface {
	// dropped before the window
	gpu: Option<Gpu>,
	window: Window,
	/// The size winit last reported: on X11, asking for it waits on the X server
	size: PhysicalSize<u32>,
	scale: f32,
	mouse: Vec2,
	/// The pointer moved since the layout last saw it, see `App::flush_motion`
	motion: bool,
	/// The cursor shown: winit would send the same one to the compositor again
	cursor: CursorIcon,
	/// The layout took the left button's press, the cursor stays until the release
	held: bool,
	recreate: bool,
	/// When to create the lost `gpu` again
	retry_at: Instant,
	/// How long the next GPU error waits before that
	backoff: Duration,
}

pub struct App {
	frontend: Frontend<()>,
	executor: AsyncExecutor,
	audio: Box<dyn AudioProvider>,
	toasts: Rc<RefCell<Vec<Notice>>>,
	surface: Option<Surface>,
	/// The size the window opens at, the last one it closed at unmaximized; logical, so a scale
	/// change keeps it
	size: LogicalSize<f64>,
	/// The window closed maximized and opens so again
	maximized: bool,
	tray: Option<Handle<Tray>>,
	/// What the tray shows, see `App::update_tray`
	tray_status: String,
	tray_link: Link,
	tray_phase: Phase,
	/// For the thread that runs `lvrr stop`
	proxy: EventLoopProxy<UserEvent>,
	/// `lvrr stop` runs, see `App::stop`
	stopping: bool,
	/// What the launch asked for and its activation token, handled once the event loop runs
	first: Option<(Request, LaunchToken)>,
	/// Why the window couldn't open when nothing could open it again, for `run` to return
	failed: Option<anyhow::Error>,
	timestep: Timestep,
	next_frame: Instant,
	next_status: Instant,
	/// Until then the window runs at the full frame rate, after that at the idle one
	active_until: Instant,
	/// The error logged last, not logged again before a frame is drawn
	last_error: Option<String>,
}

/// The LVRR server, or with `LVRR_EMU` a stand-in for trying the window without one
fn backend(proxy: EventLoopProxy<UserEvent>) -> anyhow::Result<Box<dyn LvrrBackend>> {
	Ok(match std::env::var("LVRR_EMU").as_deref() {
		Ok("connected") => Box::new(LvrrEmulator::new(true)),
		Ok("absent") => Box::new(LvrrEmulator::new(false)),
		Ok("offline") => Box::new(LvrrEmulator::offline()),
		_ => Box::new(LvrrLink::start_with_waker(move || {
			let _ = proxy.send_event(UserEvent::LinkChanged);
		})?),
	})
}

impl App {
	pub fn new(proxy: EventLoopProxy<UserEvent>, request: Request, token: LaunchToken) -> anyhow::Result<Self> {
		let lang_provider = WayVRLangProvider::default();
		let locale = Locale::from_env(&lang_provider);
		let toasts = Rc::default();
		let interface = UiInterface::new(backend(proxy.clone())?, &locale, Rc::clone(&toasts), proxy.clone());
		let executor = wlx_common::async_executor::create_local();
		let mut frontend = Frontend::new(InitParams {
			interface: Box::new(interface),
			show_welcome: false,
			has_monado: false,
			lang_provider: &lang_provider,
			theme: Rc::new(WguiTheme::default()),
			color_palette: "Default",
			executor: executor.clone(),
		})?;
		// no dim behind menus, as in VR: it would darken the see-through margins into a square box
		frontend.globals.get().popup_backdrop_dim = false;
		// like uidev: open a page directly, e.g. LVRR_PAGE=pc:headsets
		if let Ok(page) = std::env::var("LVRR_PAGE") {
			frontend.lvrr_open(&page);
		}

		let labels = {
			let mut i18n = frontend.globals.i18n();
			tray::Labels {
				open: i18n.translate("LVRR.TRAY.OPEN").to_string(),
				pair: i18n.translate("LVRR.PC.PAIR").to_string(),
				start: i18n.translate("LVRR.DESKTOP.START").to_string(),
				stop: i18n.translate("LVRR.QUIT").to_string(),
				quit: i18n.translate("LVRR.TRAY.QUIT").to_string(),
			}
		};
		let tray = tray::spawn(labels, proxy.clone());

		let now = Instant::now();
		Ok(Self {
			frontend,
			executor,
			audio: Box::new(DummyAudioProvider::new()),
			toasts,
			surface: None,
			size: LogicalSize::new(1180.0, 820.0),
			maximized: false,
			tray,
			tray_status: String::new(),
			tray_link: Link::default(),
			tray_phase: Phase::default(),
			proxy,
			stopping: false,
			first: Some((request, token)),
			failed: None,
			timestep: Timestep::new(60.0),
			next_frame: now,
			next_status: now,
			active_until: now,
			last_error: None,
		})
	}

	/// Why the window couldn't open, when the app quit for it
	pub fn take_failure(&mut self) -> Option<anyhow::Error> {
		self.failed.take()
	}

	fn open(&mut self, event_loop: &ActiveEventLoop, request: Request, token: Option<ActivationToken>) {
		if let Some(surface) = &self.surface {
			raise(&surface.window, event_loop, token);
		} else if let Err(e) = self.create_surface(event_loop, token) {
			if self.tray.is_none() {
				// nothing could open it again: main() logs it, shows it and fails
				self.failed = Some(e);
				event_loop.exit();
				return;
			}
			log::error!("Could not open the window: {e:?}");
			// nobody reads the log when the desktop started it. The tray stays, each Open tries again.
			notify(&format!("LVRR's window couldn't open: {e:#}"));
			return;
		}
		self.wake_for_input();
		if matches!(request, Request::Pair) {
			self.frontend.lvrr_start_pairing();
		}
	}

	fn create_surface(&mut self, event_loop: &ActiveEventLoop, token: Option<ActivationToken>) -> anyhow::Result<()> {
		let mut attributes = Window::default_attributes()
			.with_title("LVRR")
			.with_decorations(false)
			.with_transparent(true)
			.with_inner_size(self.size)
			.with_maximized(self.maximized)
			.with_min_inner_size(LogicalSize::new(860.0, 600.0))
			// sets the X11 class too
			.with_name(APP_ID, "lvrr-ui");
		if let Some(token) = token {
			attributes = attributes.with_activation_token(token);
		}
		let window = event_loop.create_window(attributes)?;
		let size = window.inner_size();
		let scale = window.scale_factor() as f32;
		let gpu = create_gpu(&window, scale)?;
		self.frontend.layout.mark_redraw();
		self.surface = Some(Surface {
			gpu: Some(gpu),
			window,
			size,
			scale,
			mouse: Vec2::ZERO,
			motion: false,
			cursor: CursorIcon::Default,
			held: false,
			recreate: false,
			retry_at: Instant::now(),
			backoff: GPU_RETRY,
		});
		Ok(())
	}

	fn close(&mut self, event_loop: &ActiveEventLoop) {
		// The window opens again with nothing hovered, wherever the pointer is then, and nothing
		// held: the release of a press held while it closes never comes. The leave goes first, so
		// the release clicks nothing, then the cancel, so a held checkbox doesn't flip. A held
		// slider keeps its value and the page sends it, as after any release.
		if self.surface.is_some() {
			let device = DeviceBitmask(0);
			self.push_event(&wgui::event::Event::MouseLeave(MouseLeaveEvent { device }));
			self.push_event(&wgui::event::Event::MouseCancel);
			self.push_event(&wgui::event::Event::MouseUp(MouseButtonEvent {
				pos: vec2(-1.0, -1.0),
				index: MouseButtonIndex::Left,
				device,
			}));
		}
		if let Some(surface) = self.surface.take() {
			// it opens again as large as it is now, or maximized
			self.maximized = surface.window.is_maximized();
			if !self.maximized {
				self.size = surface.window.inner_size().to_logical(surface.window.scale_factor());
			}
			if let Some(gpu) = &surface.gpu {
				let _ = gpu.gfx.wait_idle();
			}
		}
		// the tray takes what the window changed last, from here on only the link's news updates it
		self.update_tray();
		// nothing could open it again
		if self.tray.is_none() {
			event_loop.exit();
		}
	}

	/// Starts LVRR's server for the tray. Not with a `FrontendTask`: the frontend runs those only while
	/// the window is open.
	fn start(&mut self) {
		// the interface logs it. Once it runs, `lvrr start` shows its failures itself.
		if let Err(e) = self.frontend.interface.lvrr_start_server(&mut ()) {
			notify(&e.to_string());
		}
		// Starting LVRR..., without Start
		self.update_tray();
	}

	/// Closes the window and stops LVRR's server. `lvrr stop` can wait 10 s and more for the server,
	/// so it runs on a thread: the closed window goes at once, and a launch meanwhile opens it again.
	/// `UserEvent::Stopped` quits after it.
	fn stop(&mut self, event_loop: &ActiveEventLoop) {
		self.close(event_loop);
		let proxy = self.proxy.clone();
		let stop = move || {
			// `lvrr stop` falls back on systemd and signals when the server doesn't answer
			match Command::new(lvrr_command()).arg("stop").stdin(Stdio::null()).status() {
				Ok(status) if !status.success() => log::error!("lvrr stop failed: {status}"),
				Ok(_) => {}
				Err(e) => log::error!("Could not run lvrr stop: {e}"),
			}
			let _ = proxy.send_event(UserEvent::Stopped);
		};
		match std::thread::Builder::new().name("lvrr-stop".into()).spawn(stop) {
			Ok(_) => {
				self.stopping = true;
				// Stopping LVRR..., without the items `user_event` ignores meanwhile
				self.update_tray();
			}
			Err(e) => {
				log::error!("Could not run lvrr stop: {e}");
				event_loop.exit();
			}
		}
	}

	/// The tray's status and menu, kept up to date with the server, also while it starts or stops
	fn update_tray(&mut self) {
		let Some(tray) = &self.tray else {
			return;
		};
		let mut data = ();
		let Some(state) = self.frontend.interface.lvrr_state(&mut data) else {
			return;
		};
		let link = state.link;
		let key = if link == Link::Foreign {
			"LVRR.TRAY.FOREIGN".to_string()
		} else if link != Link::Online {
			"LVRR.TRAY.NOT_RUNNING".to_string()
		} else if state.pairing_enabled && !state.pin.is_empty() {
			format!("LVRR.TRAY.PAIRING;{}", state.pin)
		} else if state.headset_connected {
			format!("LVRR.TRAY.CONNECTED;{}", state.system_name)
		} else {
			"LVRR.TRAY.WAITING".to_string()
		};
		// after the poll above, which ends a start once the link reaches LVRR or the start failed
		let starting = link != Link::Online && self.frontend.interface.lvrr_starting();
		let phase = if self.stopping {
			Phase::Stopping
		} else if starting {
			Phase::Starting
		} else {
			Phase::Idle
		};
		// in the order of the pages' banners, see `Ui::not_running`
		let key = match phase {
			Phase::Stopping => "LVRR.TRAY.STOPPING".to_string(),
			Phase::Starting if link != Link::Foreign => "LVRR.DESKTOP.STARTING".to_string(),
			_ => key,
		};
		let status = self.frontend.globals.i18n().translate(&key).to_string();
		if status != self.tray_status || link != self.tray_link || phase != self.tray_phase {
			self.tray_status.clone_from(&status);
			self.tray_link = link;
			self.tray_phase = phase;
			tray.update(|tray| {
				tray.status = status;
				tray.link = link;
				tray.phase = phase;
			});
		}
		// with the window closed, nobody would see them
		if self.surface.is_none() {
			self.toasts.borrow_mut().clear();
		}
	}

	fn push_event(&mut self, event: &wgui::event::Event) -> EventResult {
		self
			.frontend
			.layout
			.push_event(event, &mut (), &mut ())
			.unwrap_or_else(|e| {
				log::warn!("{e:?}");
				EventResult::Consumed
			})
	}

	/// Logs an error, but not again while it repeats: a broken frame fails the same way every frame
	fn report(&mut self, error: &anyhow::Error) {
		let text = format!("{error:?}");
		if self.last_error.as_ref() != Some(&text) {
			log::error!("{text}");
			self.last_error = Some(text);
		}
	}

	/// Drops what draws into the window after an error, to create it again after a delay
	fn lose_gpu(&mut self, error: &anyhow::Error) {
		self.report(error);
		let Some(surface) = &mut self.surface else {
			return;
		};
		if let Some(gpu) = surface.gpu.take() {
			// fails after a device loss, the device goes either way
			let _ = gpu.gfx.wait_idle();
		}
		surface.retry_at = Instant::now() + surface.backoff;
		surface.backoff = (surface.backoff * 2).min(GPU_RETRY_MAX);
	}

	/// Input keeps the window at the full frame rate for a while, and an idle one draws at once
	fn wake_for_input(&mut self) {
		let now = Instant::now();
		if now >= self.active_until {
			self.next_frame = now;
		}
		self.active_until = now + ACTIVE_FOR;
	}

	/// Pushes the pointer's last position: the layout sees motion once a frame, not at the mouse's rate
	fn flush_motion(&mut self) {
		let Some(surface) = &mut self.surface else {
			return;
		};
		if !surface.motion {
			return;
		}
		surface.motion = false;
		let pos = surface.mouse / surface.scale;
		let device = DeviceBitmask(0);
		self.push_event(&wgui::event::Event::MouseMotion(MouseMotionEvent { pos, device }));
	}

	/// Creates what draws into the window again: all of it once the wait after an error is over,
	/// or the swapchain after a resize
	fn restore_gpu(&mut self) -> anyhow::Result<()> {
		let Some(surface) = &mut self.surface else {
			return Ok(());
		};
		let due = if surface.gpu.is_some() {
			surface.recreate
		} else {
			Instant::now() >= surface.retry_at
		};
		if !due {
			return Ok(());
		}
		let size = surface.window.inner_size();
		if size.width == 0 || size.height == 0 {
			return Ok(());
		}
		if let Some(gpu) = &mut surface.gpu {
			// even a failure retires the old swapchain, so an error drops the whole GPU state
			gpu.swapchain = gpu.swapchain.recreate([size.width, size.height])?;
			let [width, height] = gpu.swapchain.extent();
			// a resize recreates it nearly every frame
			log::debug!("swapchain size: {width}x{height}, scale {}", surface.scale);
			gpu
				.render
				.update_viewport(&mut gpu.shared, gpu.swapchain.extent(), surface.scale)?;
		} else {
			surface.gpu = Some(create_gpu(&surface.window, surface.scale)?);
		}
		surface.recreate = false;
		self.frontend.layout.mark_redraw();
		Ok(())
	}

	/// Runs the pages, which keep working while the GPU is lost
	fn update(&mut self) -> anyhow::Result<()> {
		self.flush_motion();
		let Some(surface) = &self.surface else {
			return Ok(());
		};
		let [width, height] = match &surface.gpu {
			Some(gpu) => gpu.swapchain.extent(),
			None => [surface.size.width, surface.size.height],
		};
		let scale = surface.scale;
		while self.timestep.on_tick() {
			self.frontend.layout.tick()?;
		}
		for notice in self.toasts.borrow_mut().drain(..) {
			let text = match notice {
				Notice::Message(message) => message.text,
				// the headset may still apply the change, so it isn't titled as a failure, as in VR
				Notice::RequestFailed {
					failure: Failure::Known(Reason::HeadsetSlow),
					..
				} => self
					.frontend
					.globals
					.i18n()
					.translate(Reason::HeadsetSlow.key())
					.to_string(),
				// what failed and why, the title and text of the toast in VR
				Notice::RequestFailed { title, failure } => {
					let mut i18n = self.frontend.globals.i18n();
					let reason = match failure {
						Failure::Known(reason) => i18n.translate(reason.key()).to_string(),
						Failure::Other(text) => text,
					};
					format!("{}: {reason}", i18n.translate(title))
				}
			};
			self
				.frontend
				.tasks
				.push(FrontendTask::PushToast(Translation::from_raw_text_string(text)));
		}
		let result = self.frontend.update(FrontendUpdateParams {
			data: &mut (),
			width: width as f32 / scale,
			height: height as f32 / scale,
			timestep_alpha: self.timestep.alpha,
		})?;
		self.frontend.process_update(result, &mut self.audio)?;
		while self.executor.try_tick() {}
		Ok(())
	}

	/// Draws the pages if anything changed
	fn draw(&mut self) -> anyhow::Result<()> {
		let Some(surface) = &mut self.surface else {
			return Ok(());
		};
		// still pending, the window has no size to draw at
		if surface.recreate {
			return Ok(());
		}
		let Some(gpu) = &mut surface.gpu else {
			return Ok(());
		};
		if !gpu.render.dirty && !self.frontend.layout.check_toggle_needs_redraw() {
			return Ok(());
		}

		let frame = match gpu.swapchain.acquire() {
			Ok(frame) => frame,
			Err(SwapchainError::OutOfDate) => {
				surface.recreate = true;
				return Ok(());
			}
			Err(e) => anyhow::bail!("failed to acquire the next image: {e}"),
		};
		let mut cmd = gpu.gfx.create_gfx_command_buffer(CommandBufferUsage::OneTimeSubmit)?;
		cmd.begin_rendering(frame.image_view().clone(), WGfxClearMode::Clear(CLEAR))?;
		let layout = &mut self.frontend.layout;
		let globals = layout.state.globals.clone();
		let mut globals = globals.get();
		let primitives = wgui::drawing::draw(&mut wgui::drawing::DrawParams {
			globals: &mut globals,
			layout,
			debug_draw: false,
			timestep_alpha: self.timestep.alpha,
		})?;
		gpu
			.render
			.draw(&globals.font_system, &mut gpu.shared, &mut cmd, &primitives)?;
		cmd.end_rendering()?;
		let cmd = cmd.build()?;
		match frame.present(&cmd) {
			Ok(PresentStatus::Optimal) => {}
			Ok(PresentStatus::Suboptimal) | Err(SwapchainError::OutOfDate) => surface.recreate = true,
			Err(e) => anyhow::bail!("failed to present: {e}"),
		}
		// the GPU works: the next error logs again and waits the shortest delay
		surface.backoff = GPU_RETRY;
		self.last_error = None;
		// animations and scrolling draw every frame, which keeps the full frame rate
		let now = Instant::now();
		self.active_until = self.active_until.max(now + DRAWN_FOR);
		self.next_frame = self.next_frame.min(now + FRAME);
		Ok(())
	}
}

impl ApplicationHandler<UserEvent> for App {
	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		if let Some((request, token)) = self.first.take() {
			// the variable winit would read on this display server
			let token = if event_loop.is_wayland() {
				token.wayland
			} else {
				token.x11
			};
			self.open(event_loop, request, token.map(ActivationToken::from_raw));
		}
	}

	fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
		match event {
			UserEvent::Open(request, token) => {
				let token = (!token.is_empty()).then(|| ActivationToken::from_raw(token));
				self.open(event_loop, request, token);
			}
			UserEvent::Close => self.close(event_loop),
			// one `lvrr stop` at a time, and it ends with a Quit that would close a window opened
			// meanwhile. A start would race it.
			UserEvent::Start | UserEvent::Stop | UserEvent::Quit if self.stopping => {}
			UserEvent::Start => self.start(),
			UserEvent::Stop => self.stop(event_loop),
			UserEvent::Stopped => {
				self.stopping = false;
				// a window opened meanwhile stays, and shows the server stopped
				if self.surface.is_none() {
					event_loop.exit();
				}
			}
			UserEvent::Quit => event_loop.exit(),
			UserEvent::LinkChanged => self.update_tray(),
		}
	}

	fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
		if matches!(
			event,
			WindowEvent::CursorMoved { .. }
				| WindowEvent::CursorEntered { .. }
				| WindowEvent::MouseInput { .. }
				| WindowEvent::MouseWheel { .. }
				| WindowEvent::KeyboardInput { .. }
				| WindowEvent::Resized(_)
				| WindowEvent::ScaleFactorChanged { .. }
				| WindowEvent::Focused(true)
		) {
			self.wake_for_input();
		}
		// the panel, whose left and top edges resize the window too
		let content = self.frontend.lvrr_content_rect();
		let Some(surface) = &mut self.surface else {
			return;
		};
		let scale = surface.scale;
		let device = DeviceBitmask(0);
		match event {
			WindowEvent::CloseRequested => self.close(event_loop),
			// also after a scale change
			WindowEvent::Resized(size) => {
				surface.size = size;
				surface.recreate = true;
			}
			WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
				surface.scale = scale_factor as f32;
				surface.recreate = true;
			}
			// the release may go elsewhere
			WindowEvent::Focused(false) => surface.held = false,
			WindowEvent::CursorMoved { position, .. } => {
				surface.mouse = vec2(position.x as f32, position.y as f32);
				surface.motion = true;
				// a drag the layout has keeps its cursor, also past the window's edge
				if !surface.held {
					let icon = resize_direction(surface, content).map_or(CursorIcon::Default, CursorIcon::from);
					if icon != surface.cursor {
						surface.window.set_cursor(icon);
						surface.cursor = icon;
					}
				}
			}
			// the pointer is gone: hover and tooltips end, and a pending motion would bring them back
			WindowEvent::CursorLeft { .. } => {
				surface.motion = false;
				self.push_event(&wgui::event::Event::MouseLeave(MouseLeaveEvent { device }));
			}
			WindowEvent::MouseInput {
				state,
				button: MouseButton::Left,
				..
			} => {
				if state == ElementState::Pressed
					&& let Some(direction) = resize_direction(surface, content)
				{
					let _ = surface.window.drag_resize_window(direction);
					return;
				}
				let event = MouseButtonEvent {
					pos: surface.mouse / scale,
					index: MouseButtonIndex::Left,
					device,
				};
				// the press or release lands on what the pointer hovers now
				self.flush_motion();
				let pressed = state == ElementState::Pressed;
				let result = self.push_event(&if pressed {
					wgui::event::Event::MouseDown(event)
				} else {
					wgui::event::Event::MouseUp(event)
				});
				let Some(surface) = &mut self.surface else {
					return;
				};
				// the compositor keeps the release of a window move
				surface.held = pressed && result != EventResult::NoHit;
				if pressed && result == EventResult::NoHit && surface.window.drag_window().is_ok() {
					// so the press ends here, or a page that scrolls would follow the bare pointer.
					// Nothing took the press, so nothing clicks.
					self.push_event(&wgui::event::Event::MouseUp(event));
				}
			}
			WindowEvent::MouseWheel { delta, .. } => {
				let delta = match delta {
					MouseScrollDelta::LineDelta(x, y) => Vec2::new(x, y),
					// touchpads, in physical pixels: the page moves as far as the fingers
					MouseScrollDelta::PixelDelta(pos) => Vec2::new(pos.x as f32, pos.y as f32) / scale / WHEEL_LINE,
				};
				let pos = surface.mouse / scale;
				self.flush_motion();
				self.push_event(&wgui::event::Event::MouseWheel(MouseWheelEvent { delta, pos, device }));
			}
			WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
				let text = event.text.map(|text| Rc::from(text.as_str()));
				self.push_event(&wgui::event::Event::TextInput(TextInputEvent { text }));
			}
			WindowEvent::RedrawRequested => {
				// before the update, which lays the pages out at the swapchain's size
				if let Err(e) = self.restore_gpu() {
					self.lose_gpu(&e);
				}
				if let Err(e) = self.update() {
					self.report(&e);
				} else if let Err(e) = self.draw() {
					self.lose_gpu(&e);
				}
			}
			_ => {}
		}
	}

	fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
		let now = Instant::now();
		// With the window closed, the loop sleeps until an event such as the link's LinkChanged,
		// unless LVRR starts: the interface notices the start failing, or giving up at its
		// deadline, only when polled
		let follow = self.surface.is_some() || self.frontend.interface.lvrr_starting();
		if follow && now >= self.next_status {
			self.update_tray();
			self.next_status = now + TRAY_INTERVAL;
		}
		let wake = if let Some(surface) = &self.surface {
			if now >= self.next_frame {
				surface.window.request_redraw();
				// the full rate while the window is in use or animating
				self.next_frame = now + if now < self.active_until { FRAME } else { IDLE_FRAME };
			}
			Some(self.next_frame.min(self.next_status))
		} else {
			follow.then_some(self.next_status)
		};
		event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
	}
}

/// Brings the open window to the front, with the activation token of the click or launch that
/// asked for it
fn raise(window: &Window, event_loop: &ActiveEventLoop, token: Option<ActivationToken>) {
	if !event_loop.is_wayland() {
		window.focus_window();
		return;
	}
	// focus_window() does nothing on Wayland, where only a token from the user's click raises a
	// window
	if let Some(token) = token {
		match activate::activate(window, &token.into_raw()) {
			Ok(()) => return,
			Err(e) => log::debug!("Could not activate the window: {e:?}"),
		}
	}
	// the compositor flags it instead, in the task bar or with a notification
	window.request_user_attention(Some(UserAttentionType::Informational));
}

/// The Vulkan device, swapchain and renderer for the window
fn create_gpu(window: &Window, scale: f32) -> anyhow::Result<Gpu> {
	let gfx = WGfx::new_for_window(
		window,
		&WGfxCreateInfo {
			application_name: "lvrr-ui".to_owned(),
			engine_name: "wgui".to_owned(),
			device_preference: device_preference(),
			..Default::default()
		},
	)?;
	log::info!("Using vkPhysicalDevice: {}", gfx.device_info().name);
	let size = window.inner_size();
	let swapchain = gfx.create_swapchain([size.width, size.height])?;
	let mut shared = SharedContext::new(gfx.clone())?;
	let mut render = Context::new(&mut shared, scale)?;
	render.update_viewport(&mut shared, swapchain.extent(), scale)?;
	let [width, height] = swapchain.extent();
	log::info!("swapchain size: {width}x{height}, scale {scale}");
	Ok(Gpu {
		render,
		shared,
		swapchain,
		gfx,
	})
}

/// On a hybrid laptop the integrated GPU, which drives its panel, so the window doesn't keep the
/// discrete one awake. A desktop keeps wgui's default, discrete first: its monitors are on the
/// discrete GPU, also with the CPU's integrated one enabled. `DRI_PRIME` and the like pick a GPU,
/// nearly always the discrete one, which the default ranks first; empty or 0 picks none.
fn device_preference() -> DevicePreference {
	let picked = ["DRI_PRIME", "__NV_PRIME_RENDER_OFFLOAD", "MESA_VK_DEVICE_SELECT"]
		.into_iter()
		.any(|var| std::env::var(var).is_ok_and(|value| !value.is_empty() && value != "0"));
	if picked || !has_battery() {
		DevicePreference::Default
	} else {
		DevicePreference::LowPower
	}
}

/// The machine has a battery of its own, like a laptop. Wireless mice, controllers and headsets
/// list theirs too, with the scope Device.
fn has_battery() -> bool {
	let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else {
		return false;
	};
	supplies.flatten().any(|supply| {
		let read = |name: &str| std::fs::read_to_string(supply.path().join(name)).unwrap_or_default();
		read("type").trim() == "Battery" && read("scope").trim() != "Device"
	})
}

/// The edge or corner under the pointer, if it is close enough to resize the window, to its edges
/// or to the left and top ones of `content`, the panel
fn resize_direction(surface: &Surface, content: Option<Boundary>) -> Option<ResizeDirection> {
	let size = surface.size;
	let scale = surface.scale;
	let edge = RESIZE_EDGE * scale;
	let (x, y) = (surface.mouse.x, surface.mouse.y);
	let mut left = x < edge;
	let right = x > size.width as f32 - edge;
	let mut top = y < edge;
	let bottom = y > size.height as f32 - edge;
	if let Some(content) = content {
		// from RESIZE_EDGE before the edge to 2 px into the panel, as on the right and bottom, along
		// the edge and that far past its ends, so the corners combine like the window's
		let inside = 2.0 * scale;
		let (l, t) = (content.left() * scale, content.top() * scale);
		let (r, b) = (content.right() * scale, content.bottom() * scale);
		left |= (l - edge..l + inside).contains(&x) && (t - edge..=b + edge).contains(&y);
		top |= (t - edge..t + inside).contains(&y) && (l - edge..=r + edge).contains(&x);
	}
	Some(match (left, right, top, bottom) {
		(true, _, true, _) => ResizeDirection::NorthWest,
		(_, true, true, _) => ResizeDirection::NorthEast,
		(true, _, _, true) => ResizeDirection::SouthWest,
		(_, true, _, true) => ResizeDirection::SouthEast,
		(true, ..) => ResizeDirection::West,
		(_, true, ..) => ResizeDirection::East,
		(_, _, true, _) => ResizeDirection::North,
		(.., true) => ResizeDirection::South,
		_ => return None,
	})
}
