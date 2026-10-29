//! VR applications the LVRR server can start: Steam VR games and desktop entries in the
//! X-WiVRn-VR category. They start with the server's environment, so OpenVR games use the
//! bundled xrizer. Other Steam games and desktop apps can be added, see
//! `lvrr_common::added_apps`.

use std::{
	collections::{HashMap, HashSet},
	marker::PhantomData,
	path::Path,
	rc::Rc,
};

use lvrr_common::{
	added_apps,
	library::{self, LibraryState, Sort},
	state::{LvrrRequest, VrApplication},
	xrizer_apps::{self, ControllerType},
};
use wgui::{
	components::editbox::ComponentEditBox,
	i18n::Translation,
	layout::WidgetID,
	parser::ParserState,
	task::Tasks,
	widget::ConstructEssentials,
	windowing::context_menu::{self, Blueprint, ContextMenu, TickResult},
};
use wlx_common::{async_executor::AsyncExecutor, desktop_finder::DesktopEntry};

use crate::{
	frontend::{Frontend, FrontendTask},
	tab::{
		Tab, TabType,
		lvrr::{
			Page,
			ui::{self, Text, Ui},
		},
	},
	util::{
		cached_fetcher::CoverArt,
		popup_manager::PopupHolder,
		steam_utils::{AppManifest, GameSortMethod, SteamUtils},
	},
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
/// Followed by the index in `Sort::ALL`
const SORT_ACTION_PREFIX: &str = "lvrr-sort;";
/// Followed by the app id
const REMOVE_ACTION_PREFIX: &str = "lvrr-remove;";
/// Apps that can be added, shown at most; the search finds the others
const MAX_CANDIDATES: usize = 30;

#[derive(Clone)]
enum Task {
	Request(LvrrRequest),
	Frontend(FrontendTask),
	SetCover(String, Rc<CoverArt>),
	Start { id: String, name: String },
	OpenMenu(context_menu::Position, Vec<context_menu::Cell>),
	SetControllers(u64, ControllerType),
	SetSort(Sort),
	ToggleAdding,
	Add(Source, String),
	Remove(String),
}

/// An app that is not in the Library yet
#[derive(Clone)]
enum Source {
	Steam(u64),
	/// A desktop entry id
	Desktop(Rc<str>),
}

struct Candidate {
	source: Source,
	/// The id in the server's list if it were a VR app, the name and the icon
	app: VrApplication,
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
	toolbar_widgets: Option<ParserState>,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	toolbar: WidgetID,
	list: WidgetID,
	search_box: Option<Rc<ComponentEditBox>>,
	search: String,
	library: LibraryState,
	/// Steam's last played times, by app id
	steam_played: HashMap<String, u64>,
	/// Installed Steam games and desktop apps
	candidates: Vec<Candidate>,
	/// Ids of the apps the user added
	added: HashSet<String>,
	/// Lists the apps that can be added, also without a search
	adding: bool,
	/// The inner None until the server answered
	shown: Option<Option<Vec<VrApplication>>>,
	cover_common: game_cover::ViewCommon,
	/// A game can be shown twice, as recent and in the list
	covers: HashMap<String, Vec<game_cover::View>>,
	starting: Option<Starting>,
	context_menu: ContextMenu,
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
					for view in self.covers.get_mut(&id).into_iter().flatten() {
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
					self.library.mark_started(&id);
					self.save_library();
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
				Task::SetSort(sort) => {
					self.library.sort = sort;
					self.save_library();
					self.render_toolbar(frontend)?;
					redraw = true;
				}
				Task::ToggleAdding => {
					self.adding = !self.adding;
					self.render_toolbar(frontend)?;
					redraw = true;
				}
				Task::Add(source, name) => {
					let added = match &source {
						Source::Steam(app_id) => added_apps::add_steam(*app_id, &name),
						Source::Desktop(id) => added_apps::add_desktop(id).map(|_| ()),
					};
					if let Err(e) = added {
						log::warn!("Could not add {name} to the library: {e:?}");
					}
					self.added = added_apps::added();
					frontend
						.tasks
						.push(FrontendTask::Lvrr(LvrrRequest::RefreshApplications));
				}
				Task::Remove(id) => {
					if let Err(e) = added_apps::remove(&id) {
						log::warn!("Could not remove {id} from the library: {e:?}");
					}
					self.added = added_apps::added();
					frontend
						.tasks
						.push(FrontendTask::Lvrr(LvrrRequest::RefreshApplications));
				}
			}
		}
		if let TickResult::Action(name) = self.context_menu.tick(&mut frontend.layout, &mut self.state)?
			&& let Some(task) = parse_controllers_action(&name)
				.or_else(|| parse_sort_action(&name))
				.or_else(|| parse_remove_action(&name))
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
		if let Some(search_box) = &self.search_box
			&& *search_box.get_text() != self.search
		{
			self.search = search_box.get_text().clone();
			redraw = true;
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
		let entries = frontend.interface.desktop_finder(data).find_entries();
		let steam_games = steam_games();
		let steam_played = steam_games
			.iter()
			.filter_map(|game| Some((game.app_id.clone(), game.last_played.filter(|t| *t > 0)?)))
			.collect();
		let candidates = candidates(steam_games, &entries);
		let globals = frontend.layout.state.globals.clone();
		let mut state = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut state, &globals);
		let group = ui.group(parent, "LVRR.LIBRARY.VR_APPS", "@/dashboard/games.svg")?;
		let toolbar = ui.row(group)?;
		let list = ui.column(group)?;
		let tasks = Tasks::new();
		// the list may have changed since the last visit
		tasks.push(Task::Request(LvrrRequest::RefreshApplications));
		let mut tab = Self {
			state,
			toolbar_widgets: None,
			widgets: None,
			tasks,
			toolbar,
			list,
			search_box: None,
			search: String::new(),
			library: LibraryState::load(),
			steam_played,
			candidates,
			added: added_apps::added(),
			adding: false,
			shown: None,
			cover_common: game_cover::ViewCommon::new(globals),
			covers: HashMap::new(),
			starting: None,
			context_menu: ContextMenu::default(),
			launcher: PopupHolder::default(),
			marker: PhantomData,
		};
		tab.render_toolbar(frontend)?;
		Ok(tab)
	}

	fn save_library(&self) {
		if let Err(e) = self.library.save() {
			log::warn!("Could not save the library state: {e:?}");
		}
	}

	fn last_played(&self, app: &VrApplication) -> Option<u64> {
		let steam = self.steam_played.get(&app.id).copied();
		let ours = self.library.started.get(&app.id).copied();
		steam.max(ours)
	}

	fn render_toolbar(&mut self, frontend: &mut Frontend<T>) -> anyhow::Result<()> {
		frontend.layout.remove_children(self.toolbar);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.toolbar)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);
		let row = self.toolbar;

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

		let cells = Sort::ALL
			.iter()
			.enumerate()
			.map(|(i, sort)| ui::menu_cell(format!("{SORT_ACTION_PREFIX}{i}"), sort_label(*sort)))
			.collect();
		let tasks = self.tasks.clone();
		ui.dropdown(
			row,
			"LVRR.LIBRARY.SORT",
			sort_label(self.library.sort),
			cells,
			move |position, cells| tasks.push(Task::OpenMenu(position, cells)),
		)?;

		ui.text(row, Text::Key("LVRR.LIBRARY.SEARCH"))?;
		self.search_box = Some(ui.editbox(row, &self.search)?);

		let tasks = self.tasks.clone();
		let (label, icon) = if self.adding {
			("LVRR.LIBRARY.ADD_DONE", "@/dashboard/close.svg")
		} else {
			("LVRR.LIBRARY.ADD_APPS", "@/dashboard/add.svg")
		};
		ui.button(row, Text::Key(label), icon, move || tasks.push(Task::ToggleAdding))?;

		self.toolbar_widgets = Some(widgets);
		Ok(())
	}

	fn render(&mut self, frontend: &mut Frontend<T>, applications: Option<&[VrApplication]>) -> anyhow::Result<()> {
		frontend.layout.remove_children(self.list);
		self.covers.clear();
		let globals = frontend.layout.state.globals.clone();
		let executor = frontend.executor.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.list)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);
		let group = self.list;

		if let Some(starting) = &self.starting {
			let row = ui.row(group)?;
			ui.text(row, Text::Key("LVRR.LIBRARY.STARTING"))?;
			ui.bold(row, Text::Raw(&starting.name), 16)?;
		}

		let Some(applications) = applications else {
			ui.muted(group, Text::Key("LVRR.LIBRARY.LOADING"))?;
			self.widgets = Some(widgets);
			return Ok(());
		};
		if applications.is_empty() {
			ui.muted(group, Text::Key("LVRR.LIBRARY.EMPTY"))?;
		}

		let mut shown: Vec<_> = applications
			.iter()
			.filter(|app| library::matches(app, &self.search))
			.collect();
		if !applications.is_empty() && shown.is_empty() {
			ui.muted(group, Text::Key("LVRR.LIBRARY.NO_MATCH"))?;
		}
		library::sort(&mut shown, self.library.sort, |app| self.last_played(app));

		if self.search.trim().is_empty() {
			let recent = library::recent(&shown, |app| self.last_played(app));
			if recent.is_empty() {
				self.mount_apps(&executor, &mut ui, group, &shown)?;
			} else {
				// the recent apps are a subset of `shown`; keep them out of the full
				// list below so they are not shown twice
				let recent_ids: HashSet<&str> = recent.iter().map(|a| a.id.as_str()).collect();
				ui.section_header(group, "LVRR.LIBRARY.RECENT")?;
				self.mount_apps(&executor, &mut ui, group, &recent)?;
				ui.section_header(group, "LVRR.LIBRARY.ALL_APPS")?;
				let rest: Vec<&VrApplication> = shown
					.iter()
					.copied()
					.filter(|a| !recent_ids.contains(a.id.as_str()))
					.collect();
				self.mount_apps(&executor, &mut ui, group, &rest)?;
			}
		} else {
			self.mount_apps(&executor, &mut ui, group, &shown)?;
		}

		if self.adding || !self.search.trim().is_empty() {
			self.mount_candidates(&mut ui, group, applications)?;
		}

		self.widgets = Some(widgets);
		Ok(())
	}

	/// Apps matching the search that are not in the Library, with an Add button
	fn mount_candidates(&self, ui: &mut Ui, group: WidgetID, applications: &[VrApplication]) -> anyhow::Result<()> {
		let listed: HashSet<&str> = applications.iter().map(|a| a.id.as_str()).collect();
		let mut shown: Vec<_> = self
			.candidates
			.iter()
			.filter(|c| {
				let added = match &c.source {
					Source::Steam(_) => false,
					Source::Desktop(id) => listed.contains(format!("lvrr-{id}").as_str()),
				};
				!added && !listed.contains(c.app.id.as_str()) && library::matches(&c.app, &self.search)
			})
			.collect();
		if shown.is_empty() {
			return Ok(());
		}
		shown.sort_by_cached_key(|c| c.app.name.to_lowercase());
		ui.muted(group, Text::Key("LVRR.LIBRARY.ADD_HEADER"))?;
		for candidate in shown.iter().take(MAX_CANDIDATES) {
			let row = ui.row(group)?;
			let (tasks, source, name) = (self.tasks.clone(), candidate.source.clone(), candidate.app.name.clone());
			ui.button(row, Text::Key("LVRR.LIBRARY.ADD"), "@/dashboard/add.svg", move || {
				tasks.push(Task::Add(source.clone(), name.clone()));
			})?;
			if let Some(icon) = icon_path(&candidate.app.icon)
				&& let Err(e) = ui.file_icon(row, icon)
			{
				log::warn!("Could not show the icon of {}: {e:?}", candidate.app.id);
			}
			ui.bold(row, Text::Raw(&candidate.app.name), 16)?;
			if matches!(candidate.source, Source::Steam(_)) {
				ui.muted(row, Text::Key("LVRR.LIBRARY.STEAM"))?;
			}
		}
		if shown.len() > MAX_CANDIDATES {
			ui.muted(group, Text::Key("LVRR.LIBRARY.ADD_MORE"))?;
		}
		Ok(())
	}

	fn mount_apps(
		&mut self,
		executor: &AsyncExecutor,
		ui: &mut Ui,
		group: WidgetID,
		apps: &[&VrApplication],
	) -> anyhow::Result<()> {
		let (steam, other): (Vec<&VrApplication>, Vec<&VrApplication>) =
			apps.iter().copied().partition(|a| a.steam_app_id().is_some());

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
				let wrap = ui.cover_wrap(covers)?;
				let view = game_cover::View::new(game_cover::Params {
					ess: &mut ConstructEssentials {
						layout: ui.layout,
						parent: wrap,
					},
					executor,
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
				self.covers.entry(app.id.clone()).or_default().push(view);
				self.mount_options(ui, wrap, app, true)?;
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
			self.mount_options(ui, row, app, false)?;
		}
		Ok(())
	}

	/// A menu with the controllers the game sees through xrizer, and Remove for added apps.
	/// A game that doesn't see the real controllers says so.
	fn mount_options(
		&self,
		ui: &mut Ui,
		parent: WidgetID,
		app: &VrApplication,
		corner: bool,
	) -> anyhow::Result<()> {
		let mut cells = Vec::new();
		let mut controllers = ControllerType::Auto;
		if let Some(app_id) = app.steam_app_id() {
			controllers = xrizer_apps::controller_type(app_id);
			cells.extend(ControllerType::ALL.iter().enumerate().map(|(i, ty)| {
				let mut cell = ui::menu_cell(
					format!("{CONTROLLER_ACTION_PREFIX}{app_id};{i}"),
					Translation::from_translation_key(controllers_key(*ty)),
				);
				if let context_menu::Cell::Button(button) = &mut cell
					&& *ty == controllers
				{
					button.sprite_src_builtin = Some("@/dashboard/check.svg".into());
				}
				cell
			}));
		}
		if self.added.contains(&app.id) {
			if !cells.is_empty() {
				cells.push(context_menu::Cell::Separator);
			}
			let mut cell = ui::menu_cell(
				format!("{REMOVE_ACTION_PREFIX}{}", app.id),
				Translation::from_translation_key("LVRR.LIBRARY.REMOVE"),
			);
			if let context_menu::Cell::Button(button) = &mut cell {
				button.sprite_src_builtin = Some("@/dashboard/trash.svg".into());
			}
			cells.push(cell);
		}
		if cells.is_empty() {
			return Ok(());
		}
		let container = if corner {
			ui.corner_overlay(parent)?
		} else {
			ui.line(parent)?
		};
		if controllers != ControllerType::Auto {
			ui.badge(container, controllers_key(controllers), "secondary")?;
		}
		let tasks = self.tasks.clone();
		ui.menu_button(
			container,
			"@/dashboard/options.svg",
			"LVRR.LIBRARY.OPTIONS",
			cells,
			move |position, cells| tasks.push(Task::OpenMenu(position, cells)),
		)
	}
}

const fn controllers_key(ty: ControllerType) -> &'static str {
	match ty {
		ControllerType::Auto => "LVRR.LIBRARY.CONTROLLERS_AUTO",
		ControllerType::Touch => "LVRR.LIBRARY.CONTROLLERS_TOUCH",
		ControllerType::Index => "LVRR.LIBRARY.CONTROLLERS_INDEX",
	}
}

fn parse_controllers_action(name: &str) -> Option<Task> {
	let (app_id, index) = name.strip_prefix(CONTROLLER_ACTION_PREFIX)?.split_once(';')?;
	let ty = *ControllerType::ALL.get(index.parse::<usize>().ok()?)?;
	Some(Task::SetControllers(app_id.parse().ok()?, ty))
}

fn parse_remove_action(name: &str) -> Option<Task> {
	Some(Task::Remove(name.strip_prefix(REMOVE_ACTION_PREFIX)?.to_owned()))
}

fn sort_label(sort: Sort) -> Translation {
	Translation::from_translation_key(match sort {
		Sort::Recent => "LVRR.LIBRARY.SORT_RECENT",
		Sort::Name => "LVRR.LIBRARY.SORT_NAME",
	})
}

fn parse_sort_action(name: &str) -> Option<Task> {
	let index = name.strip_prefix(SORT_ACTION_PREFIX)?.parse::<usize>().ok()?;
	Some(Task::SetSort(*Sort::ALL.get(index)?))
}

/// Installed Steam games, with when Steam last started them, also outside LVRR
fn steam_games() -> Vec<AppManifest> {
	SteamUtils::new()
		.and_then(|steam| steam.list_installed_games(GameSortMethod::None))
		.unwrap_or_else(|e| {
			log::debug!("No Steam games: {e:?}");
			Vec::new()
		})
}

/// Everything that could be added to the Library
fn candidates(steam_games: Vec<AppManifest>, entries: &HashMap<String, Rc<DesktopEntry>>) -> Vec<Candidate> {
	let steam = steam_games.into_iter().filter_map(|game| {
		Some(Candidate {
			source: Source::Steam(game.app_id.parse().ok()?),
			app: VrApplication {
				id: game.app_id,
				name: game.name,
				icon: String::new(),
			},
		})
	});
	let desktop = entries.values().map(|entry| Candidate {
		source: Source::Desktop(entry.app_id.clone()),
		app: VrApplication {
			id: entry.app_id.to_string(),
			name: entry.app_name.to_string(),
			icon: entry.icon_path.as_deref().unwrap_or_default().to_owned(),
		},
	});
	steam.chain(desktop).collect()
}

/// VR sessions other than overlays such as the dashboard
fn session_ids<T>(frontend: &mut Frontend<T>, data: &mut T) -> Vec<i64> {
	let clients = frontend.interface.monado_client_list(data, true).unwrap_or_default();
	clients.iter().map(|c| c.id).collect()
}

/// An icon file the dashboard can draw
fn icon_path(icon: &str) -> Option<&str> {
	let path = Path::new(icon);
	let drawable = path
		.extension()
		.is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("svg"));
	(path.is_absolute() && drawable && path.is_file()).then_some(icon)
}
