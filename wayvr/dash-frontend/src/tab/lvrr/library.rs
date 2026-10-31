//! VR applications the LVRR server can start: Steam VR games and desktop entries in the
//! X-WiVRn-VR category. OpenVR games use the runtime the server writes to openvrpaths.vrpath,
//! the bundled xrizer by default. Other Steam games and desktop apps can be added, see
//! `lvrr_common::added_apps`.

use std::{
	collections::{HashMap, HashSet},
	marker::PhantomData,
	path::Path,
	rc::Rc,
	time::Duration,
};

use lvrr_common::{
	added_apps,
	library::{self, LibraryState, Sort},
	state::{Link, LvrrRequest, VrApplication},
	xrizer_apps::{self, ControllerType},
};
use wgui::{
	assets::AssetPathRef,
	components::{Component, editbox::ComponentEditBox},
	globals::WguiGlobals,
	i18n::Translation,
	layout::{Layout, WidgetID},
	parser::ParserState,
	renderer_vk::text::custom_glyph::CustomGlyphData,
	task::Tasks,
	widget::ConstructEssentials,
	windowing::context_menu::{self, Blueprint, ContextMenu, TickResult},
};
use wlx_common::{async_executor::AsyncExecutor, desktop_finder::DesktopEntry};

use crate::{
	frontend::{Frontend, FrontendTask, SoundType},
	tab::{
		Tab, TabType,
		lvrr::{
			Page, started,
			ui::{self, Text, Ui},
		},
	},
	util::{
		cached_fetcher::{self, CoverArt},
		steam_utils::{AppManifest, GameSortMethod, SteamUtils},
	},
	views::game_cover,
};

/// WayVR's entries, wayvr.desktop and wayvr-headless.desktop, listed as VR apps when WayVR is
/// installed system-wide. LVRR's dashboard is WayVR, and a second one exits at once.
const HIDDEN_APPS: &[&str] = &["wayvr", "wayvr-headless"];
/// Steam tools that ship no toolmanifest.vdf: the Steamworks redistributables, Proton's BattlEye
/// and EasyAntiCheat runtimes, and SteamVR, which LVRR replaces
const STEAM_TOOLS: &[&str] = &["228980", "1007", "1161040", "1826330", "250820"];

/// A click on the app being started is taken for a double click within this, and later for
/// trying again a start that failed
const REPEAT_GUARD: Duration = Duration::from_secs(10);

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
	/// Rereads the local apps, play times, controllers and icons, fetches the covers that failed
	/// again, and asks the server for its list again
	Refresh,
	/// A cover loaded from the disk cache or Steam
	SetCover {
		id: String,
		cover: Rc<CoverArt>,
	},
	Start {
		id: String,
		name: String,
	},
	OpenMenu(context_menu::Position, Vec<context_menu::Cell>),
	SetControllers(u64, ControllerType),
	SetSort(Sort),
	/// Empties the search box and focuses it again, update() then reads that like typing
	ClearSearch,
	ToggleAdding,
	Add(Source, String),
	Remove(String),
}

/// An app that is not in the Library yet
#[derive(Clone)]
enum Source {
	Steam(u64),
	/// A desktop entry's id and the file it was read from
	Desktop {
		id: Rc<str>,
		path: Rc<str>,
	},
}

struct Candidate {
	source: Source,
	/// The id in the server's list if it were a VR app, the name and the icon
	app: VrApplication,
}

/// A Steam game's cover, kept for the tab's life
enum Cover {
	/// Requested, a view shows the spinner until it arrives
	Loading,
	/// Arrived while no view showed it, decoded once one does
	Loaded(Rc<CoverArt>),
	Glyph(CustomGlyphData),
	/// Steam has none or it could not be loaded or decoded
	Placeholder,
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
	/// LVRR's start times the list is sorted by, read with the local apps, so a start doesn't
	/// move the apps under the pointer
	sort_started: HashMap<String, u64>,
	/// Steam's last played times, by app id
	steam_played: HashMap<String, u64>,
	/// Installed Steam games and desktop apps
	candidates: Vec<Candidate>,
	/// Ids of the apps the user added
	added: HashSet<String>,
	/// Lists the apps that can be added in place of the library, also without a search
	adding: bool,
	/// The list, None until the server sent one, and true when it couldn't
	shown: Option<(Option<Vec<VrApplication>>, bool)>,
	/// LvrrState generation last compared with `shown`
	compared: Option<u64>,
	/// The link at the last update: a list asked for while it wasn't up was refused
	link: Link,
	cover_common: game_cover::ViewCommon,
	/// By app id; a game is listed in one section only
	covers: HashMap<String, game_cover::View>,
	/// By app id, so a render neither loads nor decodes a cover again. A held glyph also keeps
	/// wgui's caches of it, which only hold weak references, so it isn't uploaded again either.
	cover_cache: HashMap<String, Cover>,
	/// Desktop icons by file, read and decoded once like the covers; None when one can't be drawn
	icons: HashMap<String, Option<CustomGlyphData>>,
	/// By Steam app id, read once so a render reads no xrizer files
	controllers: HashMap<u64, ControllerType>,
	/// A start was pending at the last render, which put its badge on the app
	badged: bool,
	context_menu: ContextMenu,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrLibrary<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Library)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		// before the clicks below, so a start the server refused can be tried again at once
		started::note_failed(
			frontend
				.interface
				.lvrr_state(data)
				.and_then(|s| s.failed_start.as_ref()),
		);
		let mut redraw = false;
		for task in self.tasks.drain() {
			match task {
				Task::Request(request) => frontend.tasks.push(FrontendTask::Lvrr(request)),
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::Refresh => {
					self.load_local(frontend, data);
					// the controllers and icons may have been changed outside this tab
					self.controllers.clear();
					self.icons.clear();
					// covers that failed, as while offline, are fetched again by the render below; the
					// cache would keep each failure for an hour
					self.cover_cache.retain(|id, cover| {
						let failed = matches!(cover, Cover::Placeholder);
						if failed {
							cached_fetcher::forget_empty_cover_art(id);
						}
						!failed
					});
					frontend
						.tasks
						.push(FrontendTask::Lvrr(LvrrRequest::RefreshApplications));
					// the local apps can change while the server's list stays the same
					redraw = true;
				}
				Task::SetCover { id, cover } => {
					let state = match self.covers.get_mut(&id) {
						Some(view) => view
							.set_cover_art(&mut self.cover_common, &mut frontend.layout, &cover)?
							.map_or(Cover::Placeholder, Cover::Glyph),
						// decoded once a render shows it
						None => Cover::Loaded(cover),
					};
					self.cover_cache.insert(id, state);
				}
				Task::Start { id, name } => {
					// a double click; a start that failed can be tried again soon after
					let repeated = started::starting().is_some_and(|(starting, age)| starting == id && age < REPEAT_GUARD);
					if !repeated {
						self.start(frontend, data, id, &name);
						// shows the badge
						redraw = true;
					}
				}
				Task::OpenMenu(position, cells) => self.context_menu.open(context_menu::OpenParams {
					on_custom_attribs: None,
					position,
					blueprint: Blueprint::Cells(cells),
				}),
				Task::SetControllers(app_id, ty) => {
					if let Err(e) = xrizer_apps::set_controller_type(app_id, ty) {
						log::warn!("Could not save the controllers of {app_id}: {e:?}");
						let name = self.app_name(&app_id.to_string());
						toast_error(frontend, "LVRR.LIBRARY.CONTROLLERS_FAILED", &name, &e);
						// the next render reads what the file says now
						self.controllers.remove(&app_id);
					} else {
						self.controllers.insert(app_id, ty);
					}
					redraw = true;
				}
				Task::SetSort(sort) => {
					self.library.sort = sort;
					self.save_library();
					self.render_toolbar(frontend)?;
					redraw = true;
				}
				Task::ClearSearch => {
					if let Some(search_box) = &self.search_box {
						search_box.set_text(&mut frontend.layout.common(), "");
						// the button's press took the focus, so the next query can be typed right away
						frontend.layout.set_focus(Some(&Component(search_box.clone())))?;
					}
				}
				Task::ToggleAdding => {
					self.adding = !self.adding;
					self.render_toolbar(frontend)?;
					redraw = true;
				}
				Task::Add(source, name) => {
					let added = match &source {
						Source::Steam(app_id) => added_apps::add_steam(*app_id, &name),
						Source::Desktop { id, path } => added_apps::add_desktop(id, Path::new(&**path)).map(|_| ()),
					};
					if let Err(e) = added {
						log::warn!("Could not add {name} to the library: {e:?}");
						toast_error(frontend, "LVRR.LIBRARY.ADD_FAILED", &name, &e);
					}
					self.added = added_apps::added();
					frontend
						.tasks
						.push(FrontendTask::Lvrr(LvrrRequest::RefreshApplications));
				}
				Task::Remove(id) => {
					if let Err(e) = added_apps::remove(&id) {
						log::warn!("Could not remove {id} from the library: {e:?}");
						toast_error(frontend, "LVRR.LIBRARY.REMOVE_FAILED", &self.app_name(&id), &e);
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
		// WayVR's tick ends the start once its session comes or it is given up, and a failure above
		// ends it too; the badge then goes
		if started::pending() != self.badged {
			redraw = true;
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
		let state = frontend.interface.lvrr_state(data);
		// The link refuses the list new() asks for until it is up, the server never got that one.
		// After a failure the page says so instead, and Refresh asks again.
		let link = state.map_or(Link::NoBus, |s| s.link);
		let unanswered = state.is_some_and(|s| s.applications.is_none() && !s.applications_failed);
		if link == Link::Online && self.link != Link::Online && unanswered {
			frontend
				.tasks
				.push(FrontendTask::Lvrr(LvrrRequest::RefreshApplications));
		}
		self.link = link;
		// Only a new generation can bring a new list, so it isn't copied on every frame. Other
		// changes bump it too, the comparison keeps them from rebuilding the list.
		let generation = state.map_or(0, |s| s.generation);
		if self.shown.is_none() || self.compared != Some(generation) {
			self.compared = Some(generation);
			let applications = state.and_then(|s| s.applications.clone());
			let applications = applications.map(|apps| {
				apps
					.into_iter()
					.filter(|a| !HIDDEN_APPS.contains(&a.id.as_str()))
					.collect::<Vec<_>>()
			});
			// a failed refresh leaves the last list, the toast says it failed
			let failed = applications.is_none() && state.is_some_and(|s| s.applications_failed);
			let shown = (applications, failed);
			if self.shown.as_ref() != Some(&shown) {
				self.render(frontend, shown.0.as_deref(), failed)?;
				self.shown = Some(shown);
			}
		}
		Ok(())
	}
}

impl<T: 'static> TabLvrrLibrary<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, data: &mut T) -> anyhow::Result<Self> {
		let globals = frontend.layout.state.globals.clone();
		let mut state = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut state, &globals);
		let group = ui.group(parent, "LVRR.LIBRARY.VR_APPS", "@/dashboard/games.svg")?;
		let toolbar = ui.row(group)?;
		let list = ui.list(group)?;
		let tasks = Tasks::new();
		// the list may have changed since the last visit
		tasks.push(Task::Request(LvrrRequest::RefreshApplications));
		// refused while the link isn't up, update() asks again once it is
		let link = frontend.interface.lvrr_state(data).map_or(Link::NoBus, |s| s.link);
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
			sort_started: HashMap::new(),
			steam_played: HashMap::new(),
			candidates: Vec::new(),
			added: HashSet::new(),
			adding: false,
			shown: None,
			compared: None,
			link,
			cover_common: game_cover::ViewCommon::new(globals),
			covers: HashMap::new(),
			cover_cache: HashMap::new(),
			icons: HashMap::new(),
			controllers: HashMap::new(),
			badged: false,
			context_menu: ContextMenu::default(),
			marker: PhantomData,
		};
		tab.load_local(frontend, data);
		tab.render_toolbar(frontend)?;
		Ok(tab)
	}

	/// Reads the installed Steam games with Steam's play times, the desktop entries and the added
	/// apps, and takes LVRR's start times. Desktop entries are only scanned when WayVR starts, as
	/// in the Apps tab.
	fn load_local(&mut self, frontend: &mut Frontend<T>, data: &mut T) {
		let entries = frontend.interface.desktop_finder(data).find_entries();
		let steam_games = steam_games();
		self.steam_played = steam_games
			.iter()
			.filter_map(|game| Some((game.app_id.clone(), game.last_played.filter(|t| *t > 0)?)))
			.collect();
		self.sort_started = self.library.started.clone();
		self.candidates = candidates(steam_games, &entries);
		self.added = added_apps::added();
	}

	fn save_library(&self) {
		if let Err(e) = self.library.save() {
			log::warn!("Could not save the library state: {e:?}");
		}
	}

	/// The name of a listed app, else its id
	fn app_name(&self, id: &str) -> String {
		let listed = self
			.shown
			.iter()
			.flat_map(|(apps, _)| apps.iter().flatten())
			.find(|app| app.id == id);
		listed.map_or_else(|| id.to_owned(), |app| app.name.clone())
	}

	fn last_played(&self, app: &VrApplication) -> Option<u64> {
		let steam = self.steam_played.get(&app.id).copied();
		let ours = self.sort_started.get(&app.id).copied();
		steam.max(ours)
	}

	/// Asks the server to start app `id` and says so, like the Games and Apps tabs. The app shows
	/// a badge until its session comes or the server refuses it.
	fn start(&mut self, frontend: &mut Frontend<T>, data: &mut T, id: String, name: &str) {
		let online = frontend.interface.lvrr_state(data).filter(|s| s.link == Link::Online);
		let Some(generation) = online.map(|s| s.generation) else {
			// the link refuses it with a toast that says why, nothing starts
			frontend
				.tasks
				.push(FrontendTask::Lvrr(LvrrRequest::StartApplication(id)));
			return;
		};
		// every client, so one whose session begins later isn't taken for this app's
		let known = frontend.interface.monado_client_list(data, false);
		let known = known.ok().map(|clients| clients.iter().map(|c| c.id).collect());
		started::note_start(id.clone(), name, known, generation);
		// the list is sorted with it on the next visit
		self.library.mark_started(&id);
		self.save_library();
		frontend
			.tasks
			.push(FrontendTask::Lvrr(LvrrRequest::StartApplication(id)));
		let text = frontend
			.layout
			.state
			.globals
			.i18n()
			.translate_and_replace("LVRR.LIBRARY.STARTING_TOAST", ("{NAME}", name));
		frontend
			.tasks
			.push(FrontendTask::PushToast(Translation::from_raw_text_string(text)));
		frontend.tasks.push(FrontendTask::PlaySound(SoundType::Launch));
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
				tasks.push(Task::Refresh);
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

		// in a row of their own, so they wrap together
		let search_row = ui.row(row)?;
		ui.text(search_row, Text::Key("LVRR.LIBRARY.SEARCH"))?;
		self.search_box = Some(ui.editbox(search_row, &self.search)?);
		let tasks = self.tasks.clone();
		ui.icon_button(
			search_row,
			"@/dashboard/close.svg",
			"LVRR.LIBRARY.CLEAR_SEARCH",
			move || tasks.push(Task::ClearSearch),
		)?;

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

	/// `failed`: the server couldn't send a list, Refresh in the toolbar asks again
	fn render(
		&mut self,
		frontend: &mut Frontend<T>,
		applications: Option<&[VrApplication]>,
		failed: bool,
	) -> anyhow::Result<()> {
		// before the returns below, or update() would render again on every frame
		self.badged = started::pending();
		frontend.layout.remove_children(self.list);
		self.covers.clear();
		let globals = frontend.layout.state.globals.clone();
		let executor = frontend.executor.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.list)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);
		let group = self.list;

		let Some(applications) = applications else {
			let key = if failed {
				"LVRR.LIBRARY.LIST_FAILED"
			} else {
				"LVRR.LIBRARY.LOADING"
			};
			ui.muted(group, Text::Key(key))?;
			self.widgets = Some(widgets);
			return Ok(());
		};
		if self.adding {
			// in place of the library, so they show right under the toolbar where Add apps was pressed
			self.mount_candidates(&mut ui, group, applications)?;
			self.widgets = Some(widgets);
			return Ok(());
		}
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
				// keep the recent apps out of the full list below, so a game is not
				// listed in both sections
				let recent_ids: HashSet<&str> = recent.iter().map(|a| a.id.as_str()).collect();
				let rest: Vec<&VrApplication> = shown
					.iter()
					.copied()
					.filter(|a| !recent_ids.contains(a.id.as_str()))
					.collect();
				ui.section_header(group, "LVRR.LIBRARY.RECENT")?;
				self.mount_apps(&executor, &mut ui, group, &recent)?;
				if !rest.is_empty() {
					ui.section_header(group, "LVRR.LIBRARY.ALL_APPS")?;
					self.mount_apps(&executor, &mut ui, group, &rest)?;
				}
			}
		} else {
			self.mount_apps(&executor, &mut ui, group, &shown)?;
		}

		if !self.search.trim().is_empty() {
			self.mount_candidates(&mut ui, group, applications)?;
		}

		self.widgets = Some(widgets);
		Ok(())
	}

	/// Apps matching the search that are not in the Library, with an Add button. While adding they
	/// replace the library, so an empty list says so, or that the search found none.
	fn mount_candidates(&mut self, ui: &mut Ui, group: WidgetID, applications: &[VrApplication]) -> anyhow::Result<()> {
		let listed: HashSet<&str> = applications.iter().map(|a| a.id.as_str()).collect();
		let mut shown: Vec<_> = self
			.candidates
			.iter()
			.filter(|c| {
				let added = match &c.source {
					Source::Steam(_) => false,
					Source::Desktop { id, .. } => listed.contains(format!("lvrr-{id}").as_str()),
				};
				!added && !listed.contains(c.app.id.as_str()) && library::matches(&c.app, &self.search)
			})
			.collect();
		if shown.is_empty() && !self.adding {
			return Ok(());
		}
		shown.sort_by_cached_key(|c| c.app.name.to_lowercase());
		ui.muted(group, Text::Key("LVRR.LIBRARY.ADD_HEADER"))?;
		if shown.is_empty() {
			let key = if self.search.trim().is_empty() {
				"LVRR.LIBRARY.ADD_NONE"
			} else {
				"LVRR.LIBRARY.ADD_NO_MATCH"
			};
			ui.muted(group, Text::Key(key))?;
		}
		for candidate in shown.iter().take(MAX_CANDIDATES) {
			let row = ui.row(group)?;
			let (tasks, source, name) = (self.tasks.clone(), candidate.source.clone(), candidate.app.name.clone());
			ui.button(row, Text::Key("LVRR.LIBRARY.ADD"), "@/dashboard/add.svg", move || {
				tasks.push(Task::Add(source.clone(), name.clone()));
			})?;
			if let Some(glyph) = cached_icon(&mut self.icons, &ui.layout.state.globals, &candidate.app.icon) {
				ui.file_icon(row, glyph)?;
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
		// marked where it was clicked, a notice atop the list would be scrolled out of view
		let starting_id = started::starting().map(|(id, _)| id);
		let starting = |app: &VrApplication| starting_id.as_deref() == Some(app.id.as_str());

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
					tool: false,
				};
				let wrap = ui.cover_wrap(covers)?;
				let mut view = game_cover::View::mount(
					&mut ConstructEssentials {
						layout: ui.layout,
						parent: wrap,
					},
					&manifest,
					1.0,
				)?;
				self.mount_cover(executor, ui.layout, &mut view, &app.id)?;
				let (tasks, id, name) = (self.tasks.clone(), app.id.clone(), app.name.clone());
				view.button.on_click(Rc::new(move |_, _| {
					tasks.push(Task::Start {
						id: id.clone(),
						name: name.clone(),
					});
					Ok(())
				}));
				self.covers.insert(app.id.clone(), view);
				self.mount_options(ui, wrap, app, true, starting(app))?;
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
			if let Some(glyph) = cached_icon(&mut self.icons, &ui.layout.state.globals, &app.icon) {
				ui.file_icon(row, glyph)?;
			}
			ui.bold(row, Text::Raw(&app.name), 16)?;
			self.mount_options(ui, row, app, false, starting(app))?;
		}
		Ok(())
	}

	/// Shows a Steam game's cover, loading it once per visit
	fn mount_cover(
		&mut self,
		executor: &AsyncExecutor,
		layout: &mut Layout,
		view: &mut game_cover::View,
		id: &str,
	) -> anyhow::Result<()> {
		let cover = match self.cover_cache.get(id) {
			// the view shows the spinner until the cover arrives
			Some(Cover::Loading) => return Ok(()),
			Some(Cover::Glyph(glyph)) => return view.set_glyph(layout, glyph),
			Some(Cover::Loaded(cover)) => cover.clone(),
			Some(Cover::Placeholder) => Rc::new(CoverArt {
				compressed_image_data: Vec::new(),
			}),
			None => {
				self.cover_cache.insert(id.to_owned(), Cover::Loading);
				let (tasks, id) = (self.tasks.clone(), id.to_owned());
				executor
					.spawn(async move {
						let cover = match cached_fetcher::request_cover_art(id.clone()).await {
							Ok(cover) => cover,
							Err(e) => {
								log::warn!("Could not load the cover of {id}: {e:?}");
								CoverArt {
									compressed_image_data: Vec::new(),
								}
							}
						};
						tasks.push(Task::SetCover {
							id,
							cover: Rc::new(cover),
						});
					})
					.detach();
				return Ok(());
			}
		};
		let glyph = view.set_cover_art(&mut self.cover_common, layout, &cover)?;
		let state = glyph.map_or(Cover::Placeholder, Cover::Glyph);
		self.cover_cache.insert(id.to_owned(), state);
		Ok(())
	}

	/// A menu with the controllers the game sees through xrizer, and Remove for added apps.
	/// A game that doesn't see the real controllers says so, an app being started says that
	/// instead.
	fn mount_options(
		&mut self,
		ui: &mut Ui,
		parent: WidgetID,
		app: &VrApplication,
		corner: bool,
		starting: bool,
	) -> anyhow::Result<()> {
		let mut cells = Vec::new();
		let mut controllers = ControllerType::Auto;
		if let Some(app_id) = app.steam_app_id() {
			controllers = *self
				.controllers
				.entry(app_id)
				.or_insert_with(|| xrizer_apps::controller_type(app_id));
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
		if cells.is_empty() && !starting {
			return Ok(());
		}
		let container = if corner {
			ui.corner_overlay(parent)?
		} else {
			ui.row_end(parent)?
		};
		if starting {
			ui.icon_badge(container, "LVRR.LIBRARY.STARTING", "@/dashboard/play.svg", "primary")?;
		} else if let Some(key) = controllers_badge_key(controllers) {
			ui.icon_badge(container, key, "@/dashboard/controller.svg", "secondary")?;
		}
		if cells.is_empty() {
			return Ok(());
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

/// The badge of a game not on Auto, short so it fits on the cover next to the gear
const fn controllers_badge_key(ty: ControllerType) -> Option<&'static str> {
	match ty {
		ControllerType::Auto => None,
		ControllerType::Touch => Some("LVRR.LIBRARY.BADGE_TOUCH"),
		ControllerType::Index => Some("LVRR.LIBRARY.BADGE_INDEX"),
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

/// Everything that could be added to the Library, except Steam tools, Steam's shortcuts to
/// installed games, WayVR and LVRR
fn candidates(steam_games: Vec<AppManifest>, entries: &HashMap<String, Rc<DesktopEntry>>) -> Vec<Candidate> {
	// the tools too, so a shortcut to SteamVR is skipped as well
	let installed: HashSet<String> = steam_games.iter().map(|game| game.app_id.clone()).collect();
	let steam = steam_games
		.into_iter()
		.filter(|game| !game.tool && !STEAM_TOOLS.contains(&game.app_id.as_str()))
		.filter_map(|game| {
			Some(Candidate {
				source: Source::Steam(game.app_id.parse().ok()?),
				app: VrApplication {
					id: game.app_id,
					name: game.name,
					icon: String::new(),
				},
			})
		});
	let desktop = entries
		.values()
		.filter(|entry| {
			let id: &str = &entry.app_id;
			// WayVR and LVRR's window; the library hides WayVR, so it would be offered here
			let own = HIDDEN_APPS.contains(&id) || id.starts_with("io.github.lvrr");
			!own && !is_steam_shortcut(entry, &installed)
		})
		.map(|entry| Candidate {
			source: Source::Desktop {
				id: entry.app_id.clone(),
				path: entry.path.clone(),
			},
			app: VrApplication {
				id: entry.app_id.to_string(),
				name: entry.app_name.to_string(),
				icon: entry.icon_path.as_deref().unwrap_or_default().to_owned(),
			},
		});
	steam.chain(desktop).collect()
}

/// Steam's own shortcut to an installed game, which the Library lists or offers already. A copy
/// would list it twice and start a VR game without its VR launch option. Shortcuts to non-Steam
/// games have 64-bit ids and stay.
fn is_steam_shortcut(entry: &DesktopEntry, installed: &HashSet<String>) -> bool {
	entry.exec_argv.iter().any(|arg| {
		arg
			.strip_prefix("steam://rungameid/")
			.is_some_and(|id| installed.contains(id))
	})
}

/// Shows the text of `key` with the app's name and the error
fn toast_error<T>(frontend: &Frontend<T>, key: &str, name: &str, e: &anyhow::Error) {
	let text = frontend.layout.state.globals.i18n().translate(key);
	let text = text.replace("{NAME}", name).replace("{ERROR}", &format!("{e:#}"));
	frontend
		.tasks
		.push(FrontendTask::PushToast(Translation::from_raw_text_string(text)));
}

/// An icon file the dashboard can draw
fn icon_path(icon: &str) -> Option<&str> {
	let path = Path::new(icon);
	let drawable = path
		.extension()
		.is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("svg"));
	(path.is_absolute() && drawable && path.is_file()).then_some(icon)
}

/// An app's icon, read and decoded the first time `icons` is asked for it. wgui keeps a glyph only
/// while a sprite holds it, and a render removes the last one's sprites first.
fn cached_icon(
	icons: &mut HashMap<String, Option<CustomGlyphData>>,
	globals: &WguiGlobals,
	icon: &str,
) -> Option<CustomGlyphData> {
	if let Some(glyph) = icons.get(icon) {
		return glyph.clone();
	}
	let glyph = icon_path(icon).and_then(|path| {
		CustomGlyphData::from_assets(globals, AssetPathRef::File(path))
			.inspect_err(|e| log::warn!("Could not load the icon {path}: {e:?}"))
			.ok()
	});
	icons.insert(icon.to_owned(), glyph.clone());
	glyph
}

#[cfg(test)]
mod tests {
	use super::*;

	fn game(app_id: &str, tool: bool) -> AppManifest {
		AppManifest {
			app_id: app_id.into(),
			run_game_id: app_id.into(),
			name: app_id.into(),
			raw_state_flags: 0,
			last_played: None,
			tool,
		}
	}

	fn entry(app_id: &str, exec: &str) -> (String, Rc<DesktopEntry>) {
		let entry = DesktopEntry {
			exec_argv: exec.split(' ').map(Rc::from).collect(),
			app_id: app_id.into(),
			app_name: app_id.into(),
			icon_path: None,
			categories: Rc::new([]),
			path: "".into(),
		};
		(app_id.into(), Rc::new(entry))
	}

	#[test]
	fn candidates_skip_tools_and_shortcuts() {
		let steam = vec![
			game("620980", false),
			game("1493710", true),
			game("228980", false),
			game("250820", false),
		];
		let entries = HashMap::from([
			entry("Beat Saber", "steam steam://rungameid/620980"),
			entry("SteamVR", "steam steam://rungameid/250820"),
			entry("Uninstalled", "steam steam://rungameid/1234"),
			entry("Non-Steam", "steam steam://rungameid/12884901888033554432"),
			entry("wayvr", "wayvr"),
			entry("wayvr-headless", "wayvr --headless"),
			entry("io.github.lvrr", "lvrr"),
			entry("io.github.lvrr-dev", "lvrr"),
			entry("org.gnome.Calculator", "gnome-calculator"),
		]);
		let mut ids: Vec<_> = candidates(steam, &entries).into_iter().map(|c| c.app.id).collect();
		ids.sort();
		assert_eq!(ids, ["620980", "Non-Steam", "Uninstalled", "org.gnome.Calculator"]);
	}
}
