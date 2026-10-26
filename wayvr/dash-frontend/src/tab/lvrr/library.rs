//! VR applications the LVRR server can start: Steam VR games and desktop entries in the
//! X-WiVRn-VR category. They start with the server's environment, so OpenVR games use the
//! bundled xrizer. In VR, the system's web browser is listed too and opens as a WayVR window.

use std::{collections::HashMap, marker::PhantomData, path::Path, process::Command, rc::Rc};

use lvrr_common::{
	state::{LvrrRequest, VrApplication},
	xrizer_apps::{self, ControllerType},
};
use wgui::{
	i18n::Translation,
	layout::WidgetID,
	parser::ParserState,
	task::Tasks,
	widget::ConstructEssentials,
	windowing::context_menu::{self, Blueprint, ContextMenu, TickResult},
};
use wlx_common::desktop_finder::DesktopEntry;

use crate::{
	frontend::{Frontend, FrontendTask},
	tab::{
		Tab, TabType,
		lvrr::{
			Page,
			ui::{self, Text, Ui},
		},
	},
	util::{cached_fetcher::CoverArt, popup_manager::PopupHolder, steam_utils::AppManifest},
	views::{app_launcher, game_cover},
};

/// The dashboard itself is listed as a VR app when WayVR is installed system-wide
const HIDDEN_APPS: &[&str] = &["wayvr"];

/// How often to look for the started app's session
const POLL_MS: u32 = 500;
/// Steam can take a while to update or start a game, after this the notice goes away
const STARTING_TIMEOUT_MS: u32 = 180_000;

/// Followed by the Steam app id and the index in `ControllerType::ALL`
const CONTROLLER_ACTION_PREFIX: &str = "lvrr-controllers;";

#[derive(Clone)]
enum Task {
	Request(LvrrRequest),
	Frontend(FrontendTask),
	SetCover(String, Rc<CoverArt>),
	Start { id: String, name: String },
	OpenMenu(context_menu::Position, Vec<context_menu::Cell>),
	SetControllers(u64, ControllerType),
}

/// An app that was started and has no session yet
struct Starting {
	name: String,
	/// Sessions that existed before the start
	known: Vec<i64>,
	since_ms: u32,
	polled_ms: u32,
}

pub struct TabLvrrLibrary<T> {
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	parent: WidgetID,
	/// The inner None until the server answered
	shown: Option<Option<Vec<VrApplication>>>,
	cover_common: game_cover::ViewCommon,
	covers: HashMap<String, game_cover::View>,
	starting: Option<Starting>,
	context_menu: ContextMenu,
	/// None in the desktop window, it can't show windows
	browser: Option<Rc<DesktopEntry>>,
	launcher: PopupHolder<app_launcher::View>,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrLibrary<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Library)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		let mut redraw = false;
		for task in self.tasks.drain() {
			match task {
				Task::Request(request) => frontend.tasks.push(FrontendTask::Lvrr(request)),
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::SetCover(id, cover) => {
					if let Some(view) = self.covers.get_mut(&id) {
						view.set_cover_art(&mut self.cover_common, &mut frontend.layout, &cover)?;
					}
				}
				Task::Start { id, name } => {
					let known = session_ids(frontend, data);
					self.starting = Some(Starting {
						name,
						known,
						since_ms: time_ms,
						polled_ms: time_ms,
					});
					frontend
						.tasks
						.push(FrontendTask::Lvrr(LvrrRequest::StartApplication(id)));
					redraw = true;
				}
				Task::OpenMenu(position, cells) => self.context_menu.open(context_menu::OpenParams {
					on_custom_attribs: None,
					position,
					blueprint: Blueprint::Cells(cells),
				}),
				Task::SetControllers(app_id, ty) => {
					if let Err(e) = xrizer_apps::set_controller_type(app_id, ty) {
						log::warn!("Could not save the controllers of {app_id}: {e:?}");
					}
					redraw = true;
				}
			}
		}
		if let TickResult::Action(name) = self.context_menu.tick(&mut frontend.layout, &mut self.state)?
			&& let Some(task) = parse_controllers_action(&name)
		{
			self.tasks.push(task);
		}
		if let Some(starting) = &mut self.starting
			&& time_ms.wrapping_sub(starting.polled_ms) >= POLL_MS
		{
			starting.polled_ms = time_ms;
			let appeared = session_ids(frontend, data)
				.iter()
				.any(|id| !starting.known.contains(id));
			if appeared || time_ms.wrapping_sub(starting.since_ms) >= STARTING_TIMEOUT_MS {
				self.starting = None;
				redraw = true;
			}
		}
		if redraw {
			self.shown = None;
		}
		let applications = frontend.interface.lvrr_state(data).and_then(|s| s.applications.clone());
		let applications = applications.map(|apps| {
			apps
				.into_iter()
				.filter(|a| !HIDDEN_APPS.contains(&a.id.as_str()))
				.collect::<Vec<_>>()
		});
		if self.shown.as_ref() != Some(&applications) {
			self.render(frontend, applications.as_deref())?;
			self.shown = Some(applications);
		}
		self
			.launcher
			.with_view_res(|view| view.update(&mut frontend.interface, data))?;
		Ok(())
	}
}

impl<T: 'static> TabLvrrLibrary<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, data: &mut T) -> anyhow::Result<Self> {
		let browser = if frontend.interface.lvrr_desktop() {
			None
		} else {
			default_browser(&frontend.interface.desktop_finder(data).find_entries())
		};
		let globals = frontend.layout.state.globals.clone();
		let state = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, parent)?;
		let tasks = Tasks::new();
		// the list may have changed since the last visit
		tasks.push(Task::Request(LvrrRequest::RefreshApplications));
		Ok(Self {
			state,
			widgets: None,
			tasks,
			parent,
			shown: None,
			cover_common: game_cover::ViewCommon::new(globals),
			covers: HashMap::new(),
			starting: None,
			context_menu: ContextMenu::default(),
			browser,
			launcher: PopupHolder::default(),
			marker: PhantomData,
		})
	}

	fn render(&mut self, frontend: &mut Frontend<T>, applications: Option<&[VrApplication]>) -> anyhow::Result<()> {
		frontend.layout.remove_children(self.parent);
		self.covers.clear();
		let globals = frontend.layout.state.globals.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);

		let group = ui.group(self.parent, "LVRR.LIBRARY.VR_APPS", "@/dashboard/games.svg")?;
		let row = ui.row(group)?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.LIBRARY.REFRESH"),
			"@/dashboard/refresh.svg",
			move || {
				tasks.push(Task::Request(LvrrRequest::RefreshApplications));
			},
		)?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.LIBRARY.ALL_GAMES"),
			"@/dashboard/games.svg",
			move || {
				tasks.push(Task::Frontend(FrontendTask::SetTab(TabType::Games)));
			},
		)?;

		if let Some(starting) = &self.starting {
			let row = ui.row(group)?;
			ui.text(row, Text::Key("LVRR.LIBRARY.STARTING"))?;
			ui.bold(row, Text::Raw(&starting.name), 16)?;
		}

		if let Some(browser) = &self.browser {
			let row = ui.row(group)?;
			let (frontend_tasks, globals, entry, launcher) = (
				frontend.tasks.clone(),
				globals.clone(),
				browser.clone(),
				self.launcher.clone(),
			);
			ui.button(
				row,
				Text::Key("LVRR.LIBRARY.LAUNCH"),
				"@/dashboard/play.svg",
				move || {
					app_launcher::mount_popup(
						frontend_tasks.clone(),
						globals.clone(),
						entry.clone(),
						launcher.clone(),
						Box::new(|| {}),
						None,
					);
				},
			)?;
			if let Some(icon) = browser.icon_path.as_deref().and_then(icon_path)
				&& let Err(e) = ui.file_icon(row, icon)
			{
				log::warn!("Could not show the icon of {}: {e:?}", browser.app_id);
			}
			ui.bold(row, Text::Key("LVRR.LIBRARY.BROWSER"), 16)?;
		}

		let Some(applications) = applications else {
			ui.muted(group, Text::Key("LVRR.LIBRARY.LOADING"))?;
			self.widgets = Some(widgets);
			return Ok(());
		};
		if applications.is_empty() {
			ui.muted(group, Text::Key("LVRR.LIBRARY.EMPTY"))?;
		}
		let (steam, other): (Vec<_>, Vec<_>) = applications.iter().partition(|a| a.steam_app_id().is_some());

		// Steam games as covers, like the Games tab
		if !steam.is_empty() {
			let covers = ui.row(group)?;
			for app in steam {
				let manifest = AppManifest {
					app_id: app.id.clone(),
					run_game_id: app.id.clone(),
					name: app.name.clone(),
					raw_state_flags: 0,
					last_played: None,
				};
				let on_loaded = {
					let (tasks, id) = (self.tasks.clone(), app.id.clone());
					Box::new(move |cover: CoverArt| tasks.push(Task::SetCover(id, Rc::new(cover))))
				};
				let stack = ui.stack(covers)?;
				let view = game_cover::View::new(game_cover::Params {
					ess: &mut ConstructEssentials {
						layout: ui.layout,
						parent: stack,
					},
					executor: &frontend.executor,
					manifest: &manifest,
					scale: 1.0,
					on_loaded,
				})?;
				let (tasks, id, name) = (self.tasks.clone(), app.id.clone(), app.name.clone());
				view.button.on_click(Rc::new(move |_, _| {
					tasks.push(Task::Start {
						id: id.clone(),
						name: name.clone(),
					});
					Ok(())
				}));
				self.covers.insert(app.id.clone(), view);
				if let Some(app_id) = app.steam_app_id() {
					self.mount_controllers(&mut ui, stack, app_id)?;
				}
			}
		}

		// Desktop entries with their icon
		for app in other {
			let row = ui.row(group)?;
			let (tasks, id, name) = (self.tasks.clone(), app.id.clone(), app.name.clone());
			ui.button(
				row,
				Text::Key("LVRR.LIBRARY.LAUNCH"),
				"@/dashboard/play.svg",
				move || {
					tasks.push(Task::Start {
						id: id.clone(),
						name: name.clone(),
					});
				},
			)?;
			if let Some(icon) = icon_path(&app.icon)
				&& let Err(e) = ui.file_icon(row, icon)
			{
				log::warn!("Could not show the icon of {}: {e:?}", app.id);
			}
			ui.bold(row, Text::Raw(&app.name), 16)?;
		}

		self.widgets = Some(widgets);
		Ok(())
	}

	/// The controllers the game sees, through xrizer
	fn mount_controllers(&self, ui: &mut Ui, parent: WidgetID, app_id: u64) -> anyhow::Result<()> {
		let cells = ControllerType::ALL
			.iter()
			.enumerate()
			.map(|(i, ty)| {
				ui::menu_cell(
					format!("{CONTROLLER_ACTION_PREFIX}{app_id};{i}"),
					controllers_label(*ty),
				)
			})
			.collect();
		let tasks = self.tasks.clone();
		ui.dropdown_sized(
			parent,
			"LVRR.LIBRARY.CONTROLLERS",
			controllers_label(xrizer_apps::controller_type(app_id)),
			cells,
			60,
			move |position, cells| tasks.push(Task::OpenMenu(position, cells)),
		)
	}
}

fn controllers_label(ty: ControllerType) -> Translation {
	Translation::from_translation_key(match ty {
		ControllerType::Auto => "LVRR.LIBRARY.CONTROLLERS_AUTO",
		ControllerType::Touch => "LVRR.LIBRARY.CONTROLLERS_TOUCH",
		ControllerType::Index => "LVRR.LIBRARY.CONTROLLERS_INDEX",
	})
}

fn parse_controllers_action(name: &str) -> Option<Task> {
	let (app_id, index) = name.strip_prefix(CONTROLLER_ACTION_PREFIX)?.split_once(';')?;
	let ty = *ControllerType::ALL.get(index.parse::<usize>().ok()?)?;
	Some(Task::SetControllers(app_id.parse().ok()?, ty))
}

/// VR sessions other than overlays such as the dashboard
fn session_ids<T>(frontend: &mut Frontend<T>, data: &mut T) -> Vec<i64> {
	let clients = frontend.interface.monado_client_list(data, true).unwrap_or_default();
	clients.iter().map(|c| c.id).collect()
}

/// The default handler of https links, else any web browser
fn default_browser(entries: &HashMap<String, Rc<DesktopEntry>>) -> Option<Rc<DesktopEntry>> {
	let default = Command::new("xdg-mime")
		.args(["query", "default", "x-scheme-handler/https"])
		.output()
		.ok()
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.and_then(|s| {
			let id = s.trim().trim_end_matches(".desktop");
			entries.get(id).cloned().or_else(|| hidden_entry(id))
		});
	default.or_else(|| {
		let mut browsers: Vec<_> = entries
			.values()
			.filter(|e| e.categories.iter().any(|c| &**c == "WebBrowser"))
			.collect();
		browsers.sort_by(|a, b| a.app_id.cmp(&b.app_id));
		browsers.first().map(|e| (*e).clone())
	})
}

/// Defaults set by browsers or `xdg-mime` are often `NoDisplay` entries, which the app list skips
fn hidden_entry(id: &str) -> Option<Rc<DesktopEntry>> {
	if id.is_empty() {
		return None;
	}
	let no_locales: &[&str] = &[];
	let path = freedesktop_desktop_entry::default_paths()
		.map(|dir| dir.join(format!("{id}.desktop")))
		.find(|path| path.is_file())?;
	let entry = freedesktop_desktop_entry::DesktopEntry::from_path(path, Some(no_locales)).ok()?;
	let exec_argv: Rc<[Rc<str>]> = entry
		.parse_exec_with_uris(&[], no_locales)
		.ok()?
		.into_iter()
		.map(Rc::from)
		.collect();
	if exec_argv.is_empty() {
		return None;
	}
	Some(Rc::new(DesktopEntry {
		exec_argv,
		app_id: id.into(),
		app_name: entry
			.name(no_locales)
			.map_or_else(|| id.into(), |name| name.as_ref().into()),
		icon_path: entry.icon().filter(|icon| icon.starts_with('/')).map(Rc::from),
		categories: Rc::new([]),
	}))
}

/// An icon file the dashboard can draw
fn icon_path(icon: &str) -> Option<&str> {
	let path = Path::new(icon);
	let drawable = path
		.extension()
		.is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("svg"));
	(path.is_absolute() && drawable && path.is_file()).then_some(icon)
}
