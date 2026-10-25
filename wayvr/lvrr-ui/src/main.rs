//! LVRR's desktop window: the dashboard's pages that work without a headset (status, pairing,
//! headset and server settings, diagnostics) and a tray icon. Closing the window keeps the tray,
//! and the server runs on either way.

mod app;
mod instance;
mod interface;
mod tray;

use std::process::ExitCode;

use instance::Request;
use tracing_subscriber::{EnvFilter, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};
use winit::event_loop::EventLoop;

const USAGE: &str = "usage: lvrr-ui [--pair]

Opens LVRR's window, or the running one. --pair starts pairing a new headset.";

pub enum UserEvent {
	/// From the tray, another launch or `lvrr pair`, with the launch's activation token
	Open(Request, String),
	/// The window closes, the tray stays
	Close,
	/// Stops LVRR's server, then quits
	Stop,
	Quit,
}

fn init_logging() {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
		.with(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy()
				.add_directive("cosmic_text=warn".parse().unwrap()),
		)
		.init();
}

/// The token a launcher passed for raising the window
fn activation_token() -> String {
	std::env::var("XDG_ACTIVATION_TOKEN")
		.or_else(|_| std::env::var("DESKTOP_STARTUP_ID"))
		.unwrap_or_default()
}

fn main() -> ExitCode {
	let request = match std::env::args().nth(1).as_deref() {
		None => Request::Open,
		Some("--pair") => Request::Pair,
		Some("--help" | "-h") => {
			println!("{USAGE}");
			return ExitCode::SUCCESS;
		}
		Some(arg) => {
			eprintln!("lvrr-ui: unknown argument {arg}\n\n{USAGE}");
			return ExitCode::FAILURE;
		}
	};
	init_logging();

	let conn = match instance::claim() {
		Ok(Some(conn)) => Some(conn),
		Ok(None) => {
			return match instance::forward(request, &activation_token()) {
				Ok(()) => ExitCode::SUCCESS,
				Err(e) => {
					log::error!("The LVRR window is open but doesn't answer: {e}");
					ExitCode::FAILURE
				}
			};
		}
		Err(e) => {
			log::warn!("No session bus, so a second window may open: {e}");
			None
		}
	};

	match run(request, conn) {
		Ok(()) => ExitCode::SUCCESS,
		Err(e) => {
			log::error!("{e:?}");
			ExitCode::FAILURE
		}
	}
}

fn run(request: Request, conn: Option<dbus::blocking::Connection>) -> anyhow::Result<()> {
	let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
	let proxy = event_loop.create_proxy();
	if let Some(conn) = conn {
		instance::serve(conn, proxy.clone());
	}
	let mut app = app::App::new(proxy, request)?;
	event_loop.run_app(&mut app)?;
	Ok(())
}
