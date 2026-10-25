//! Running VR applications: bring one to the front or stop it.

use std::marker::PhantomData;

use lvrr_common::state::LvrrRequest;
use wgui::{layout::WidgetID, parser::ParserState, task::Tasks};
use wlx_common::dash_interface::MonadoClient;

use crate::{
	frontend::{Frontend, FrontendTask},
	tab::{
		Tab, TabType,
		lvrr::{
			Page,
			ui::{self, Text, Ui},
		},
	},
};

/// Ticks between refreshes of the application list
const REFRESH_TICKS: u32 = 120;

#[derive(Clone)]
enum Task {
	Focus(i64),
	Stop(i64),
	Frontend(FrontendTask),
	Refresh,
}

pub struct TabLvrrRunning<T> {
	#[allow(dead_code)]
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	parent: WidgetID,
	shown: Option<Vec<(i64, String, bool)>>,
	ticks: u32,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrRunning<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Running)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		let mut refresh = self.ticks.is_multiple_of(REFRESH_TICKS);
		self.ticks += 1;
		for task in self.tasks.drain() {
			match task {
				Task::Focus(id) => {
					if let Err(e) = frontend.interface.monado_client_focus(data, id) {
						log::warn!("Could not focus client {id}: {e:?}");
					}
					refresh = true;
				}
				Task::Stop(id) => {
					if let Ok(id) = u32::try_from(id) {
						frontend.lvrr_confirm("LVRR.RUNNING.STOP_CONFIRM", LvrrRequest::StopApplication(id));
					}
				}
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::Refresh => refresh = true,
			}
		}
		if refresh {
			let clients = frontend.interface.monado_client_list(data, true).unwrap_or_default();
			self.render(frontend, &clients)?;
		}
		Ok(())
	}
}

impl<T: 'static> TabLvrrRunning<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, _data: &mut T) -> anyhow::Result<Self> {
		let globals = frontend.layout.state.globals.clone();
		let state = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, parent)?;
		Ok(Self {
			state,
			widgets: None,
			tasks: Tasks::new(),
			parent,
			shown: None,
			ticks: 0,
			marker: PhantomData,
		})
	}

	fn render(&mut self, frontend: &mut Frontend<T>, clients: &[MonadoClient]) -> anyhow::Result<()> {
		// The dashboard is an overlay and not listed; redraw only when something changed
		let shown: Vec<_> = clients.iter().map(|c| (c.id, c.name.clone(), c.is_primary)).collect();
		if self.shown.as_ref() == Some(&shown) {
			return Ok(());
		}
		self.shown = Some(shown);

		frontend.layout.remove_children(self.parent);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);

		let group = ui.group(self.parent, "LVRR.RUNNING.APPS", "@/dashboard/play.svg")?;
		if clients.is_empty() {
			ui.muted(group, Text::Key("LVRR.RUNNING.EMPTY"))?;
		}
		for client in clients {
			let row = ui.row(group)?;
			ui.bold(row, Text::Raw(&client.name), 16)?;
			if client.is_primary {
				ui.badge(row, "LVRR.RUNNING.FOCUSED", "success")?;
			} else {
				let (tasks, id) = (self.tasks.clone(), client.id);
				ui.button(
					row,
					Text::Key("LVRR.RUNNING.FOCUS"),
					"@/dashboard/play.svg",
					move || {
						tasks.push(Task::Focus(id));
					},
				)?;
			}
			let (tasks, id) = (self.tasks.clone(), client.id);
			ui.danger_button(
				row,
				Text::Key("LVRR.RUNNING.STOP"),
				"@/dashboard/close.svg",
				move || {
					tasks.push(Task::Stop(id));
					tasks.push(Task::Refresh);
				},
			)?;
		}

		let row = ui.row(self.parent)?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.RUNNING.DETAILS"),
			"@/dashboard/monado.svg",
			move || {
				tasks.push(Task::Frontend(FrontendTask::SetTab(TabType::Monado)));
			},
		)?;

		self.widgets = Some(widgets);
		Ok(())
	}
}
