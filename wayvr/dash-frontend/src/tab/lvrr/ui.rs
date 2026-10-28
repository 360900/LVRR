//! Building blocks for the LVRR pages, from `lvrr-assets/gui/lvrr/widgets.xml`.

use std::rc::Rc;

use wgui::{
	assets::AssetPathRef,
	color::WguiColorName,
	components::{
		bar_graph::ComponentBarGraph, button::ComponentButton, checkbox::ComponentCheckbox, editbox::ComponentEditBox,
		slider::ComponentSlider,
	},
	globals::WguiGlobals,
	i18n::Translation,
	layout::{Layout, WidgetID},
	parser::{Fetchable, ParseDocumentParams, ParserState, TemplateParams},
	widget::label::WidgetLabel,
	windowing::context_menu,
};

use crate::util::wgui_simple;

pub const WIDGETS: &str = "gui/lvrr/widgets.xml";

pub fn doc_params(globals: &WguiGlobals) -> ParseDocumentParams<'_> {
	ParseDocumentParams {
		globals: globals.clone(),
		path: AssetPathRef::BuiltIn(WIDGETS),
		extra: Default::default(),
	}
}

/// Text that is either a translation key or shown as is
#[derive(Clone, Copy)]
pub enum Text<'a> {
	Key(&'a str),
	Raw(&'a str),
}

impl Text<'_> {
	fn insert(self, params: &mut TemplateParams) {
		match self {
			Text::Key(key) => params.insert("translation", key),
			Text::Raw(text) => params.insert("text", text),
		};
	}
}

pub struct Ui<'a> {
	pub layout: &'a mut Layout,
	pub state: &'a mut ParserState,
	doc: ParseDocumentParams<'a>,
	next_id: usize,
}

impl<'a> Ui<'a> {
	pub fn new(layout: &'a mut Layout, state: &'a mut ParserState, globals: &'a WguiGlobals) -> Self {
		Self {
			layout,
			state,
			doc: doc_params(globals),
			next_id: 0,
		}
	}

	fn id(&mut self) -> String {
		self.next_id += 1;
		format!("lvrr_{}", self.next_id)
	}

	fn instantiate(&mut self, template: &str, parent: WidgetID, params: TemplateParams) -> anyhow::Result<()> {
		self
			.state
			.instantiate_template(&self.doc, template, self.layout, parent, params)
	}

	fn container(
		&mut self,
		template: &str,
		parent: WidgetID,
		extra: impl FnOnce(&mut TemplateParams),
	) -> anyhow::Result<WidgetID> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		extra(&mut params);
		self.instantiate(template, parent, params)?;
		self.state.get_widget_id(&id)
	}

	/// A titled box
	pub fn group(&mut self, parent: WidgetID, translation: &str, icon: &str) -> anyhow::Result<WidgetID> {
		self.container("Group", parent, |p| {
			p.insert("translation", translation);
			p.insert("icon", icon);
		})
	}

	pub fn row(&mut self, parent: WidgetID) -> anyhow::Result<WidgetID> {
		self.container("Row", parent, |_| {})
	}

	/// A row that never wraps, for a column of details followed by buttons
	pub fn line(&mut self, parent: WidgetID) -> anyhow::Result<WidgetID> {
		self.container("Line", parent, |_| {})
	}

	pub fn column(&mut self, parent: WidgetID) -> anyhow::Result<WidgetID> {
		self.container("Column", parent, |_| {})
	}

	/// A column as wide as its content, such as a game cover with its settings below
	pub fn stack(&mut self, parent: WidgetID) -> anyhow::Result<WidgetID> {
		self.container("Stack", parent, |_| {})
	}

	fn label(&mut self, template: &str, parent: WidgetID, text: Text, size: u32, color: &str) -> anyhow::Result<()> {
		let mut params = TemplateParams::new();
		text.insert(&mut params);
		params.insert_rc("size", size.to_string().into());
		params.insert("color", color);
		self.instantiate(template, parent, params)
	}

	pub fn text(&mut self, parent: WidgetID, text: Text) -> anyhow::Result<()> {
		self.label("Text", parent, text, 14, "on_background")
	}

	pub fn muted(&mut self, parent: WidgetID, text: Text) -> anyhow::Result<()> {
		self.label("Text", parent, text, 12, "secondary")
	}

	pub fn bold(&mut self, parent: WidgetID, text: Text, size: u32) -> anyhow::Result<()> {
		self.label("Bold", parent, text, size, "on_background")
	}

	/// An icon from a file, such as an application's icon
	pub fn file_icon(&mut self, parent: WidgetID, path: &str) -> anyhow::Result<()> {
		let mut params = TemplateParams::new();
		params.insert("src", path);
		self.instantiate("FileIcon", parent, params)
	}

	/// A small colored tag, color is a theme color such as `primary` or `danger`
	pub fn badge(&mut self, parent: WidgetID, translation: &str, color: &str) -> anyhow::Result<()> {
		let mut params = TemplateParams::new();
		params.insert("translation", translation);
		params.insert("color", color);
		self.instantiate("Badge", parent, params)
	}

	/// A full width notice, returns the row to put buttons in
	pub fn banner(&mut self, parent: WidgetID, translation: &str, color: &str) -> anyhow::Result<WidgetID> {
		self.container("Banner", parent, |p| {
			p.insert("translation", translation);
			p.insert("color", color);
		})
	}

	pub fn toggle(
		&mut self,
		parent: WidgetID,
		text: Text,
		checked: bool,
		on_toggle: impl Fn(bool) + 'static,
	) -> anyhow::Result<Rc<ComponentCheckbox>> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		text.insert(&mut params);
		params.insert("checked", if checked { "1" } else { "0" });
		self.instantiate("ToggleText", parent, params)?;
		let checkbox = self.state.fetch_component_as::<ComponentCheckbox>(&id)?;
		checkbox.on_toggle(Box::new(move |_common, e| {
			on_toggle(e.checked);
			Ok(())
		}));
		Ok(checkbox)
	}

	/// A slider under its label; `badge` goes next to the label, see [Self::badge]
	pub fn slider(
		&mut self,
		parent: WidgetID,
		translation: &str,
		badge: Option<(&str, &str)>,
		range: (f32, f32, f32),
		value: f32,
		on_change: impl Fn(f32) + 'static,
	) -> anyhow::Result<Rc<ComponentSlider>> {
		let column = self.column(parent)?;
		let header = self.line(column)?;
		self.text(header, Text::Key(translation))?;
		if let Some((translation, color)) = badge {
			self.badge(header, translation, color)?;
		}

		let id = self.id();
		let (min, max, step) = range;
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		params.insert_rc("min", min.to_string().into());
		params.insert_rc("max", max.to_string().into());
		params.insert_rc("step", step.to_string().into());
		params.insert_rc("value", value.clamp(min, max).to_string().into());
		self.instantiate("Slider", column, params)?;
		let slider = self.state.fetch_component_as::<ComponentSlider>(&id)?;
		slider.on_value_changed(Box::new(move |_common, e| on_change(e.value)));
		Ok(slider)
	}

	pub fn button(
		&mut self,
		parent: WidgetID,
		text: Text,
		icon: &str,
		on_click: impl Fn() + 'static,
	) -> anyhow::Result<Rc<ComponentButton>> {
		self.button_colored(parent, text, icon, "background_contrast", on_click)
	}

	pub fn danger_button(
		&mut self,
		parent: WidgetID,
		text: Text,
		icon: &str,
		on_click: impl Fn() + 'static,
	) -> anyhow::Result<Rc<ComponentButton>> {
		self.button_colored(parent, text, icon, "danger", on_click)
	}

	fn button_colored(
		&mut self,
		parent: WidgetID,
		text: Text,
		icon: &str,
		color: &str,
		on_click: impl Fn() + 'static,
	) -> anyhow::Result<Rc<ComponentButton>> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		text.insert(&mut params);
		params.insert("icon", icon);
		params.insert("color", color);
		self.instantiate("Button", parent, params)?;
		let button = self.state.fetch_component_as::<ComponentButton>(&id)?;
		button.on_click(Rc::new(move |_common, _e| {
			on_click();
			Ok(())
		}));
		Ok(button)
	}

	/// A button with only an icon that opens a menu where it was clicked
	pub fn menu_button(
		&mut self,
		parent: WidgetID,
		icon: &str,
		tooltip: &str,
		cells: Vec<context_menu::Cell>,
		on_open: impl Fn(context_menu::Position, Vec<context_menu::Cell>) + 'static,
	) -> anyhow::Result<()> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		params.insert("icon", icon);
		params.insert("tooltip", tooltip);
		self.instantiate("IconButton", parent, params)?;
		let button = self.state.fetch_component_as::<ComponentButton>(&id)?;
		button.on_click(Rc::new(move |_common, e| {
			on_open(e.mouse_pos_absolute.into(), cells.clone());
			Ok(())
		}));
		Ok(())
	}

	/// A text field; only the desktop window has a keyboard for it
	pub fn editbox(&mut self, parent: WidgetID, text: &str) -> anyhow::Result<Rc<ComponentEditBox>> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		params.insert("text", text);
		self.instantiate("EditBox", parent, params)?;
		self.state.fetch_component_as::<ComponentEditBox>(&id)
	}

	/// A large tile button, for the home page
	pub fn action(
		&mut self,
		parent: WidgetID,
		translation: &str,
		icon: &str,
		on_click: impl Fn() + 'static,
	) -> anyhow::Result<Rc<ComponentButton>> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		params.insert("translation", translation);
		params.insert("icon", icon);
		self.instantiate("ActionButton", parent, params)?;
		let button = self.state.fetch_component_as::<ComponentButton>(&id)?;
		button.on_click(Rc::new(move |_common, _e| {
			on_click();
			Ok(())
		}));
		Ok(button)
	}

	/// A labelled value, returns the value label to update later
	pub fn stat(&mut self, parent: WidgetID, translation: &str) -> anyhow::Result<WidgetID> {
		let value_id = self.id();
		let mut params = TemplateParams::new();
		params.insert("translation", translation);
		params.insert("value_id", &value_id);
		self.instantiate("Stat", parent, params)?;
		self.state.get_widget_id(&value_id)
	}

	pub fn graph(
		&mut self,
		parent: WidgetID,
		translation: &str,
		unit: &str,
		limit_max: f32,
	) -> anyhow::Result<Rc<ComponentBarGraph>> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("id", &id);
		params.insert("name", translation);
		params.insert("unit", unit);
		params.insert_rc("limit_max", limit_max.to_string().into());
		self.instantiate("Graph", parent, params)?;
		self.state.fetch_component_as::<ComponentBarGraph>(&id)
	}

	/// A button that opens a list to pick from; cells are context menu buttons whose
	/// action names come back from the page's context menu
	pub fn dropdown(
		&mut self,
		parent: WidgetID,
		translation: &str,
		current: Translation,
		cells: Vec<context_menu::Cell>,
		on_open: impl Fn(context_menu::Position, Vec<context_menu::Cell>) + 'static,
	) -> anyhow::Result<()> {
		self.dropdown_sized(parent, translation, current, cells, 120, on_open)
	}

	/// A dropdown whose value takes at least `min_width`
	pub fn dropdown_sized(
		&mut self,
		parent: WidgetID,
		translation: &str,
		current: Translation,
		cells: Vec<context_menu::Cell>,
		min_width: u32,
		on_open: impl Fn(context_menu::Position, Vec<context_menu::Cell>) + 'static,
	) -> anyhow::Result<()> {
		let id = self.id();
		let mut params = TemplateParams::new();
		params.insert("translation", translation);
		params.insert_rc("min_width", min_width.to_string().into());
		wgui_simple::mount_dropdown(wgui_simple::MountDropdownParams {
			layout: self.layout,
			parser_state: self.state,
			doc_params: &self.doc,
			parent,
			id,
			params,
			current_text: current,
			cells,
			on_open: Box::new(on_open),
		})
	}
}

/// A context menu entry; `action` is returned when it is picked
pub fn menu_cell(action: String, title: Translation) -> context_menu::Cell {
	context_menu::Cell::Button(context_menu::Button {
		action_name: Some(action.into()),
		title,
		..Default::default()
	})
}

pub fn set_label(layout: &mut Layout, id: WidgetID, text: Translation) {
	let mut common = layout.common();
	if let Ok(mut label) = common.state.widgets.cast_as::<WidgetLabel>(id) {
		label.set_text(&mut common, text);
	}
}

pub fn set_label_color(layout: &mut Layout, id: WidgetID, color: WguiColorName) {
	let mut common = layout.common();
	if let Ok(mut label) = common.state.widgets.cast_as::<WidgetLabel>(id) {
		label.set_color(&mut common, color.into(), true);
	}
}
