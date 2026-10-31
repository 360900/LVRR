//! Brings the open window to the front on Wayland. winit only uses an activation token for a new
//! window, and its `focus_window` does nothing there.

use anyhow::Context as _;
use wayland_client::{
	Connection, Dispatch, Proxy, QueueHandle,
	backend::{Backend, ObjectId},
	delegate_noop,
	globals::{GlobalListContents, registry_queue_init},
	protocol::{wl_registry::WlRegistry, wl_surface::WlSurface},
};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use winit::{
	raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle},
	window::Window,
};

/// Handles nothing: the globals are read once, and the activation global has no events
struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
	fn event(
		_state: &mut Self,
		_proxy: &WlRegistry,
		_event: <WlRegistry as Proxy>::Event,
		_data: &GlobalListContents,
		_conn: &Connection,
		_qhandle: &QueueHandle<Self>,
	) {
	}
}

delegate_noop!(State: XdgActivationV1);

/// Asks the compositor to activate `window` with `token`, which a launcher or the tray got for the
/// user's click. The compositor may still refuse, and then usually flags the window instead.
pub fn activate(window: &Window, token: &str) -> anyhow::Result<()> {
	let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
		(window.display_handle()?.as_raw(), window.window_handle()?.as_raw())
	else {
		anyhow::bail!("not a Wayland window");
	};
	// SAFETY: both pointers come from the live winit window, which `window` borrows for the whole
	// call. A guest backend neither owns nor closes the display, and it and everything made from it
	// are dropped before this returns, so none of it outlives the display or the surface.
	let (conn, id) = unsafe {
		let conn = Connection::from_backend(Backend::from_foreign_display(display.display.as_ptr().cast()));
		let id = ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast())?;
		(conn, id)
	};
	let surface = WlSurface::from_id(&conn, id)?;
	// its own queue: the roundtrip reading the globals dispatches none of winit's events
	let (globals, queue) = registry_queue_init::<State>(&conn)?;
	let activation: XdgActivationV1 = globals
		.bind(&queue.handle(), 1..=1, ())
		.context("the compositor doesn't support xdg_activation_v1")?;
	activation.activate(token.to_owned(), &surface);
	activation.destroy();
	// frees the registry where the compositor can (wl_fixes), or each call would leave one behind
	globals.destroy();
	conn.flush()?;
	Ok(())
}
