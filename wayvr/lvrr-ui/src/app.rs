//! The window around the dashboard, and the tray that outlives it.

use std::{
	cell::RefCell,
	process::Command,
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
	state::{Link, LvrrBackend},
};
use wgui::{
	event::{DeviceBitmask, MouseButtonEvent, MouseButtonIndex, MouseMotionEvent, MouseWheelEvent, TextInputEvent},
	gfx::{CommandBufferUsage, PresentStatus, Swapchain, SwapchainError, WGfx, WGfxClearMode, WGfxCreateInfo},
	i18n::{Locale, Translation},
	renderer_vk::context::{Context, SharedContext},
	theme::WguiTheme,
	widget::EventResult,
};
use winit::{
	application::ApplicationHandler,
	dpi::LogicalSize,
	event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
	event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
	platform::{
		startup_notify::{EventLoopExtStartupNotify, WindowAttributesExtStartupNotify},
		wayland::WindowAttributesExtWayland,
	},
	window::{ActivationToken, CursorIcon, ResizeDirection, Window, WindowId},
};
use wlx_common::{
	async_executor::AsyncExecutor,
	audio::{AudioProvider, DummyAudioProvider},
	locale::WayVRLangProvider,
	timestep::Timestep,
};

use crate::{
	UserEvent,
	instance::Request,
	interface::{UiInterface, lvrr_command},
	tray::{self, Tray},
};

/// The Wayland app id and X11 class, which match the desktop file
const APP_ID: &str = "io.github.lvrr";
const FRAME: Duration = Duration::from_micros(16_667);
/// How often the tray follows the server while the window is closed
const TRAY_INTERVAL: Duration = Duration::from_millis(500);
/// Around the dashboard the desktop shows, like the world in VR
const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
/// How close to the window's edge the pointer resizes it, in logical pixels. There's no title
/// bar or frame: the dashboard draws everything, and a press where nothing reacts moves the window.
const RESIZE_EDGE: f32 = 6.0;

/// The window and what draws into it, dropped when it closes
struct Surface {
	// dropped in this order: the renderer before the swapchain and device, the window last
	render: Context,
	shared: SharedContext,
	swapchain: Arc<Swapchain>,
	gfx: Arc<WGfx>,
	window: Window,
	scale: f32,
	mouse: Vec2,
	recreate: bool,
}

pub struct App {
	frontend: Frontend<()>,
	executor: AsyncExecutor,
	audio: Box<dyn AudioProvider>,
	toasts: Rc<RefCell<Vec<String>>>,
	surface: Option<Surface>,
	tray: Option<Handle<Tray>>,
	tray_status: String,
	/// What the launch asked for, handled once the event loop runs
	first: Option<Request>,
	timestep: Timestep,
	next_frame: Instant,
	next_status: Instant,
}

/// The LVRR server, or with `LVRR_EMU` a stand-in for trying the window without one
fn backend() -> anyhow::Result<Box<dyn LvrrBackend>> {
	Ok(match std::env::var("LVRR_EMU").as_deref() {
		Ok("connected") => Box::new(LvrrEmulator::new(true)),
		Ok("absent") => Box::new(LvrrEmulator::new(false)),
		Ok("offline") => Box::new(LvrrEmulator::offline()),
		_ => Box::new(LvrrLink::start()?),
	})
}

impl App {
	pub fn new(proxy: EventLoopProxy<UserEvent>, request: Request) -> anyhow::Result<Self> {
		let lang_provider = WayVRLangProvider::default();
		let locale = Locale::from_env(&lang_provider);
		let toasts = Rc::default();
		let interface = UiInterface::new(backend()?, &locale, Rc::clone(&toasts), proxy.clone());
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
		// like uidev: open a page directly, e.g. LVRR_PAGE=pc:headsets
		if let Ok(page) = std::env::var("LVRR_PAGE") {
			frontend.lvrr_open(&page);
		}

		let labels = {
			let mut i18n = frontend.globals.i18n();
			tray::Labels {
				open: i18n.translate("LVRR.TRAY.OPEN").to_string(),
				pair: i18n.translate("LVRR.PC.PAIR").to_string(),
				stop: i18n.translate("LVRR.QUIT").to_string(),
			}
		};
		let tray = tray::spawn(labels, proxy);

		let now = Instant::now();
		Ok(Self {
			frontend,
			executor,
			audio: Box::new(DummyAudioProvider::new()),
			toasts,
			surface: None,
			tray,
			tray_status: String::new(),
			first: Some(request),
			timestep: Timestep::new(60.0),
			next_frame: now,
			next_status: now,
		})
	}

	fn open(&mut self, event_loop: &ActiveEventLoop, request: Request, token: Option<ActivationToken>) {
		if let Some(surface) = &self.surface {
			surface.window.focus_window();
		} else if let Err(e) = self.create_surface(event_loop, token) {
			log::error!("Could not open the window: {e:?}");
			if self.tray.is_none() {
				event_loop.exit();
			}
			return;
		}
		if matches!(request, Request::Pair) {
			self.frontend.lvrr_start_pairing();
		}
	}

	fn create_surface(&mut self, event_loop: &ActiveEventLoop, token: Option<ActivationToken>) -> anyhow::Result<()> {
		let mut attributes = Window::default_attributes()
			.with_title("LVRR")
			.with_decorations(false)
			.with_transparent(true)
			.with_inner_size(LogicalSize::new(1180.0, 820.0))
			.with_min_inner_size(LogicalSize::new(860.0, 600.0))
			// sets the X11 class too
			.with_name(APP_ID, "lvrr-ui");
		if let Some(token) = token {
			attributes = attributes.with_activation_token(token);
		}
		let window = event_loop.create_window(attributes)?;
		let gfx = WGfx::new_for_window(
			&window,
			&WGfxCreateInfo {
				application_name: "lvrr-ui".to_owned(),
				engine_name: "wgui".to_owned(),
				..Default::default()
			},
		)?;
		let size = window.inner_size();
		let swapchain = gfx.create_swapchain([size.width, size.height])?;
		let scale = window.scale_factor() as f32;
		let mut shared = SharedContext::new(gfx.clone())?;
		let mut render = Context::new(&mut shared, scale)?;
		render.update_viewport(&mut shared, swapchain.extent(), scale)?;
		let [width, height] = swapchain.extent();
		log::info!("swapchain size: {width}x{height}, scale {scale}");
		self.frontend.layout.mark_redraw();
		self.surface = Some(Surface {
			render,
			shared,
			swapchain,
			gfx,
			window,
			scale,
			mouse: Vec2::ZERO,
			recreate: false,
		});
		Ok(())
	}

	fn close(&mut self, event_loop: &ActiveEventLoop) {
		if let Some(surface) = self.surface.take() {
			let _ = surface.gfx.wait_idle();
		}
		// nothing could open it again
		if self.tray.is_none() {
			event_loop.exit();
		}
	}

	fn stop(&mut self, event_loop: &ActiveEventLoop) {
		self.close(event_loop);
		// `lvrr stop` falls back on systemd and signals when the server doesn't answer
		if let Err(e) = Command::new(lvrr_command()).arg("stop").status() {
			log::error!("Could not run lvrr stop: {e}");
		}
		event_loop.exit();
	}

	/// The tray's tooltip, kept up to date with the server
	fn update_tray(&mut self) {
		let Some(tray) = &self.tray else {
			return;
		};
		let mut data = ();
		let Some(state) = self.frontend.interface.lvrr_state(&mut data) else {
			return;
		};
		let key = if state.link != Link::Online {
			"LVRR.TRAY.NOT_RUNNING".to_string()
		} else if state.pairing_enabled && !state.pin.is_empty() {
			format!("LVRR.TRAY.PAIRING;{}", state.pin)
		} else if state.headset_connected {
			format!("LVRR.TRAY.CONNECTED;{}", state.system_name)
		} else {
			"LVRR.TRAY.WAITING".to_string()
		};
		let status = self.frontend.globals.i18n().translate(&key).to_string();
		if status != self.tray_status {
			self.tray_status.clone_from(&status);
			tray.update(|tray| tray.status = status);
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

	fn redraw(&mut self) -> anyhow::Result<()> {
		let Some(surface) = &mut self.surface else {
			return Ok(());
		};
		if surface.recreate {
			let size = surface.window.inner_size();
			if size.width == 0 || size.height == 0 {
				return Ok(());
			}
			surface.swapchain = surface.swapchain.recreate([size.width, size.height])?;
			let [width, height] = surface.swapchain.extent();
			log::info!("swapchain size: {width}x{height}, scale {}", surface.scale);
			surface
				.render
				.update_viewport(&mut surface.shared, surface.swapchain.extent(), surface.scale)?;
			surface.recreate = false;
			self.frontend.layout.mark_redraw();
		}

		while self.timestep.on_tick() {
			self.frontend.layout.tick()?;
		}
		for text in self.toasts.borrow_mut().drain(..) {
			self
				.frontend
				.tasks
				.push(FrontendTask::PushToast(Translation::from_raw_text_string(text)));
		}
		let [width, height] = surface.swapchain.extent();
		let result = self.frontend.update(FrontendUpdateParams {
			data: &mut (),
			width: width as f32 / surface.scale,
			height: height as f32 / surface.scale,
			timestep_alpha: self.timestep.alpha,
		})?;
		self.frontend.process_update(result, &mut self.audio)?;
		while self.executor.try_tick() {}

		if !surface.render.dirty && !self.frontend.layout.check_toggle_needs_redraw() {
			return Ok(());
		}

		let frame = match surface.swapchain.acquire() {
			Ok(frame) => frame,
			Err(SwapchainError::OutOfDate) => {
				surface.recreate = true;
				return Ok(());
			}
			Err(e) => anyhow::bail!("failed to acquire the next image: {e}"),
		};
		let mut cmd = surface
			.gfx
			.create_gfx_command_buffer(CommandBufferUsage::OneTimeSubmit)?;
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
		surface
			.render
			.draw(&globals.font_system, &mut surface.shared, &mut cmd, &primitives)?;
		cmd.end_rendering()?;
		let cmd = cmd.build()?;
		match frame.present(&cmd) {
			Ok(PresentStatus::Optimal) => {}
			Ok(PresentStatus::Suboptimal) | Err(SwapchainError::OutOfDate) => surface.recreate = true,
			Err(e) => anyhow::bail!("failed to present: {e}"),
		}
		Ok(())
	}
}

impl ApplicationHandler<UserEvent> for App {
	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		if let Some(request) = self.first.take() {
			let token = event_loop.read_token_from_env();
			// apps started from the window shouldn't use it
			winit::platform::startup_notify::reset_activation_token_env();
			self.open(event_loop, request, token);
		}
	}

	fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
		match event {
			UserEvent::Open(request, token) => {
				let token = (!token.is_empty()).then(|| ActivationToken::from_raw(token));
				self.open(event_loop, request, token);
			}
			UserEvent::Close => self.close(event_loop),
			UserEvent::Stop => self.stop(event_loop),
			UserEvent::Quit => event_loop.exit(),
		}
	}

	fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
		let Some(surface) = &mut self.surface else {
			return;
		};
		let scale = surface.scale;
		let device = DeviceBitmask(0);
		match event {
			WindowEvent::CloseRequested => self.close(event_loop),
			WindowEvent::Resized(_) => surface.recreate = true,
			WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
				surface.scale = scale_factor as f32;
				surface.recreate = true;
			}
			WindowEvent::CursorMoved { position, .. } => {
				surface.mouse = vec2(position.x as f32, position.y as f32);
				let pos = surface.mouse / scale;
				let icon = resize_direction(surface).map_or(CursorIcon::Default, CursorIcon::from);
				surface.window.set_cursor(icon);
				self.push_event(&wgui::event::Event::MouseMotion(MouseMotionEvent { pos, device }));
			}
			WindowEvent::MouseInput {
				state,
				button: MouseButton::Left,
				..
			} => {
				if state == ElementState::Pressed
					&& let Some(direction) = resize_direction(surface)
				{
					let _ = surface.window.drag_resize_window(direction);
					return;
				}
				let event = MouseButtonEvent {
					pos: surface.mouse / scale,
					index: MouseButtonIndex::Left,
					device,
				};
				let pressed = state == ElementState::Pressed;
				let result = self.push_event(&if pressed {
					wgui::event::Event::MouseDown(event)
				} else {
					wgui::event::Event::MouseUp(event)
				});
				if pressed
					&& matches!(result, EventResult::NoHit)
					&& let Some(surface) = &self.surface
				{
					let _ = surface.window.drag_window();
				}
			}
			WindowEvent::MouseWheel { delta, .. } => {
				let delta = match delta {
					MouseScrollDelta::LineDelta(x, y) => Vec2::new(x, y),
					MouseScrollDelta::PixelDelta(pos) => Vec2::new(pos.x as f32, pos.y as f32) / 5.0,
				};
				let pos = surface.mouse / scale;
				self.push_event(&wgui::event::Event::MouseWheel(MouseWheelEvent { delta, pos, device }));
			}
			WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
				let text = event.text.map(|text| Rc::from(text.as_str()));
				self.push_event(&wgui::event::Event::TextInput(TextInputEvent { text }));
			}
			WindowEvent::RedrawRequested => {
				if let Err(e) = self.redraw() {
					log::error!("{e:?}");
				}
			}
			_ => {}
		}
	}

	fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
		let now = Instant::now();
		if now >= self.next_status {
			self.update_tray();
			self.next_status = now + TRAY_INTERVAL;
			// with the window closed, nobody would see them
			if self.surface.is_none() {
				self.toasts.borrow_mut().clear();
			}
		}
		let wake = if let Some(surface) = &self.surface {
			if now >= self.next_frame {
				surface.window.request_redraw();
				self.next_frame = now + FRAME;
			}
			self.next_frame.min(self.next_status)
		} else {
			self.next_status
		};
		event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
	}
}

/// The edge or corner under the pointer, if it is close enough to resize the window
fn resize_direction(surface: &Surface) -> Option<ResizeDirection> {
	let size = surface.window.inner_size();
	let edge = RESIZE_EDGE * surface.scale;
	let (x, y) = (surface.mouse.x, surface.mouse.y);
	let left = x < edge;
	let right = x > size.width as f32 - edge;
	let top = y < edge;
	let bottom = y > size.height as f32 - edge;
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
