/*
 * WiVRn VR streaming
 * Copyright (C) 2026  Guillaume Meunier <guillaume.meunier@centraliens.net>
 * Copyright (C) 2026  Patrick Nicolas <patricknicolas@laposte.net>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

#include "ui_widgets.h"

#include "imgui.h"
#include "imgui_internal.h"
#include "ui_theme.h"

#include "IconsFontAwesome7.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <functional>
#include <unordered_map>
#include <vector>

// drag-to-scroll helper, defined in imgui_impl.cpp
void ScrollWhenDragging();

namespace wivrn::ui
{

namespace
{
std::function<void()> hover_haptic_hook;
std::function<void(const char *)> tooltip_hook;

// height reserved for the pinned sidebar footer, set by begin_sidebar
float sidebar_footer_height = 0;
} // namespace

void set_hover_haptic(std::function<void()> hook)
{
	hover_haptic_hook = std::move(hook);
}

void set_tooltip_hook(std::function<void(const char *)> hook)
{
	tooltip_hook = std::move(hook);
}

// fire the hover haptic for the last submitted item, hook checks if it is hovered
static void hover_haptic()
{
	if (hover_haptic_hook)
		hover_haptic_hook();
}

// show a tooltip for the last item, caller ensures it is hovered
static void show_tooltip(const std::string & text)
{
	if (not text.empty() and tooltip_hook)
		tooltip_hook(text.c_str());
}

void page_header(const std::string & title, const std::string & subtitle)
{
	const theme & t = current();

	ImGui::PushFont(nullptr, ImGui::GetStyle().FontSizeBase * metrics::font_title);
	ImGui::TextUnformatted(title.c_str());
	ImGui::PopFont();

	if (not subtitle.empty())
	{
		ImGui::PushStyleColor(ImGuiCol_Text, t.text_muted);
		ImGui::TextUnformatted(subtitle.c_str());
		ImGui::PopStyleColor();
	}
	ImGui::Dummy({0, ImGui::GetStyle().ItemSpacing.y});
}

// a rounded panel, not a child window, so the page stays one scroll surface, drawn behind
// the content using last frame's height, pair with end_card
namespace
{
struct card_frame
{
	ImGuiID id;
	ImVec2 origin;
	float width;
	float content_max_x_backup;
	float work_max_x_backup;
	ImVec2 padding;
};
std::vector<card_frame> card_stack;
std::unordered_map<ImGuiID, float> card_heights;

bool begin_card_impl(const char * id, ImVec2 padding)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	const theme & t = current();
	const ImGuiID gid = window->GetID(id);

	card_stack.push_back({gid,
	                      window->DC.CursorPos,
	                      ImGui::GetContentRegionAvail().x,
	                      window->ContentRegionRect.Max.x,
	                      window->WorkRect.Max.x,
	                      padding});
	const card_frame & cf = card_stack.back();

	if (const float h = card_heights[gid]; h > 0)
	{
		const ImVec2 bb_max = {cf.origin.x + cf.width, cf.origin.y + h};
		ImVec4 card = t.card;
		card.w *= background_alpha(); // card fill follows the panel opacity
		window->DrawList->AddRectFilled(cf.origin, bb_max, t.col(card), t.card_rounding);
		if (t.border_size > 0)
			window->DrawList->AddRect(cf.origin, bb_max, t.col(t.border), t.card_rounding, t.border_size);
	}

	ImGui::PushID(id);
	window->ContentRegionRect.Max.x -= padding.x;
	window->WorkRect.Max.x -= padding.x;
	ImGui::Indent(padding.x);
	ImGui::SetCursorScreenPos({ImGui::GetCursorScreenPos().x, cf.origin.y + padding.y});
	ImGui::BeginGroup();
	return true;
}
} // namespace

bool begin_card(const char * id, const ImVec2 &)
{
	return begin_card_impl(id, metrics::card_padding);
}

bool begin_list_card(const char * id)
{
	return begin_card_impl(id, metrics::list_card_padding);
}

void end_card()
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	const card_frame cf = card_stack.back();
	card_stack.pop_back();

	ImGui::EndGroup();
	ImGui::Unindent(cf.padding.x);
	window->ContentRegionRect.Max.x = cf.content_max_x_backup;
	window->WorkRect.Max.x = cf.work_max_x_backup;
	ImGui::PopID();

	const float height = ImGui::GetItemRectMax().y + cf.padding.y - cf.origin.y;
	card_heights[cf.id] = height;

	// reserve the full card extent so layout and scrolling account for it
	ImGui::SetCursorScreenPos(cf.origin);
	ImGui::Dummy({cf.width, height});
}

void row_separator()
{
	const theme & t = current();
	ImVec4 edge = t.border;
	edge.w *= background_alpha(); // match the panel/card opacity
	ImGui::PushStyleColor(ImGuiCol_Separator, edge);
	ImGui::Separator();
	ImGui::PopStyleColor();
}

float setting_label(const std::string & title, const std::string & description, float control_width)
{
	const theme & t = current();
	const ImGuiStyle & style = ImGui::GetStyle();

	const ImVec2 start = ImGui::GetCursorPos();
	const float avail = ImGui::GetContentRegionAvail().x;
	float text_w = avail - control_width - style.ItemSpacing.x;
	if (text_w < 1)
		text_w = avail;

	ImGui::PushStyleVar(ImGuiStyleVar_ItemSpacing, {style.ItemSpacing.x, metrics::label_line_gap});
	ImGui::PushTextWrapPos(start.x + text_w);
	ImGui::TextUnformatted(title.c_str());
	ImGui::PopTextWrapPos();
	if (not description.empty())
	{
		ImGui::PushFont(nullptr, style.FontSizeBase * metrics::font_description);
		ImGui::PushStyleColor(ImGuiCol_Text, t.text_muted);
		// never wider than half the row, wrap to as many lines as needed
		const float desc_w = ImMin(text_w, avail * 0.5f);
		ImGui::PushTextWrapPos(start.x + desc_w);
		ImGui::TextUnformatted(description.c_str());
		ImGui::PopTextWrapPos();
		ImGui::PopStyleColor();
		ImGui::PopFont();
	}
	ImGui::PopStyleVar();

	// right-aligned control slot, vertically centered against the label block
	const float label_h = ImGui::GetCursorPos().y - start.y;
	const float ctrl_h = ImGui::GetFrameHeight() * metrics::control_height;
	const float ctrl_y = start.y + ImMax(0.f, (label_h - ctrl_h) * 0.5f);
	ImGui::SetCursorPos({start.x + avail - control_width, ctrl_y});
	return start.y + label_h;
}

namespace
{
struct button_colors
{
	ImVec4 bg, bg_hovered, bg_active, text;
};

button_colors colors_for(button_style s, const theme & t)
{
	switch (s)
	{
		case button_style::primary:
			return {t.accent, t.accent_hovered, t.accent_active, t.on_accent};
		case button_style::secondary:
			return {t.control, t.control_hovered, t.control_active, t.text};
		case button_style::danger:
			return {t.danger, t.danger_hovered, t.danger, t.on_accent};
		case button_style::ghost:
			return {{0, 0, 0, 0}, t.control, t.control_active, t.text};
	}
	return {};
}

// LVRR: the dashboard's button fill, darker at the bottom, with a lighter border
void button_frame(ImDrawList * draw, const ImRect & bb, const ImVec4 & bg, float rounding)
{
	const theme & t = current();
	if (bg.w <= 0)
		return;
	const float k = 1 - metrics::button_gradient;
	gradient_rect(draw, bb.Min, bb.Max, rounding, bg, {bg.x * k, bg.y * k, bg.z * k, bg.w});
	if (t.border_size > 0)
	{
		const ImVec4 edge{std::min(bg.x * 1.3f + 0.04f, 1.f), std::min(bg.y * 1.3f + 0.04f, 1.f), std::min(bg.z * 1.3f + 0.04f, 1.f), bg.w};
		draw->AddRect(bb.Min, bb.Max, t.col(edge), rounding, 0, t.border_size);
	}
}
} // namespace

std::string icon_label(const char * icon, const std::string & label)
{
	return std::string(icon) + "  " + label;
}

float button_width(const std::string & label)
{
	return ImGui::CalcTextSize(label.c_str()).x + metrics::button_padding.x * 2;
}

float button_width(const char * icon, const std::string & label)
{
	ImGui::PushFont(nullptr, ImGui::GetFontSize() * metrics::button_label_glyph);
	const float icon_w = ImGui::CalcTextSize(icon).x;
	ImGui::PopFont();
	const float gap = ImGui::GetFontSize() * 0.4f;
	return icon_w + gap + ImGui::CalcTextSize(label.c_str()).x + metrics::button_padding.x * 2;
}

ImVec2 chip_size(const std::string & label)
{
	const ImVec2 ts = ImGui::CalcTextSize(label.c_str());
	return {ts.x + metrics::chip_padding.x * 2, ts.y + metrics::chip_padding.y * 2};
}

float chip_width(const std::string & label, bool dot, float height)
{
	// mirrors the footprint computed in chip()
	const ImVec2 ts = ImGui::CalcTextSize(label.c_str());
	const float pad_x = height > 0 ? metrics::chip_pill_padding_x : metrics::chip_padding.x;
	const float dot_r = ts.y * 0.26f;
	const float dot_gap = dot ? dot_r * 2 + pad_x * 0.55f : 0;
	return ts.x + pad_x * 2 + dot_gap;
}

bool button(const std::string & label, button_style s, const ImVec2 & size)
{
	const theme & t = current();

	// control height by default, width auto-fits the label
	const float h = size.y > 0 ? size.y : ImGui::GetFrameHeight() * metrics::control_height;

	const button_colors c = colors_for(s, t);

	// drawn by hand so the fill can carry the dashboard's gradient and border
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;
	const ImGuiStyle & style = ImGui::GetStyle();
	const ImVec2 label_size = ImGui::CalcTextSize(label.c_str(), nullptr, true);
	const float w = size.x > 0 ? size.x : label_size.x + metrics::button_padding.x * 2;
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + ImVec2{w, h});
	ImGui::ItemSize(bb, style.FramePadding.y);
	const ImGuiID id = window->GetID(label.c_str());
	if (not ImGui::ItemAdd(bb, id))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, id, &hovered, &held);
	hover_haptic();

	ImVec4 bg = c.bg;
	if (held)
		bg = c.bg_active;
	else if (hovered)
		bg = c.bg_hovered;
	button_frame(window->DrawList, bb, bg, t.rounding);

	ImGui::PushStyleColor(ImGuiCol_Text, c.text);
	ImGui::RenderTextClipped(bb.Min + metrics::button_padding, bb.Max - metrics::button_padding, label.c_str(), nullptr, &label_size, {0.5f, 0.5f}, &bb);
	ImGui::PopStyleColor();
	return pressed;
}

bool button(const char * icon, const std::string & label, button_style s, const ImVec2 & size)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	ImGuiContext & g = *GImGui;
	const ImGuiStyle & style = g.Style;
	const theme & t = current();

	const float h = size.y > 0 ? size.y : ImGui::GetFrameHeight() * metrics::control_height;
	const float glyph_size = ImGui::GetFontSize() * metrics::button_label_glyph;
	const float gap = ImGui::GetFontSize() * 0.4f;

	ImGui::PushFont(nullptr, glyph_size);
	const float icon_w = ImGui::CalcTextSize(icon).x;
	ImGui::PopFont();
	const ImVec2 label_size = ImGui::CalcTextSize(label.c_str());
	const float content_w = icon_w + gap + label_size.x;
	const float w = size.x > 0 ? size.x : content_w + metrics::button_padding.x * 2;

	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + ImVec2{w, h});
	ImGui::ItemSize(bb, style.FramePadding.y);
	const ImGuiID id = window->GetID(label.c_str());
	if (not ImGui::ItemAdd(bb, id))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, id, &hovered, &held);
	hover_haptic();

	const button_colors c = colors_for(s, t);
	ImVec4 bg = c.bg;
	if (held)
		bg = c.bg_active;
	else if (hovered)
		bg = c.bg_hovered;

	ImDrawList * draw = window->DrawList;
	button_frame(draw, bb, bg, t.rounding);

	const ImU32 col = t.col(c.text);
	const ImVec2 center = bb.GetCenter();
	float x = center.x - content_w * 0.5f;

	// icon centred vertically on its ink box, like icon_button
	ImGui::PushFont(nullptr, glyph_size);
	unsigned int codepoint = 0;
	ImTextCharFromUtf8(&codepoint, icon, nullptr);
	const ImFontGlyph * glyph = ImGui::GetFontBaked()->FindGlyph(ImWchar(codepoint));
	if (glyph)
		draw->AddText({x, center.y - (glyph->Y0 + glyph->Y1) * 0.5f}, col, icon);
	else
		draw->AddText({x, center.y - glyph_size * 0.5f}, col, icon);
	ImGui::PopFont();

	x += icon_w + gap;
	draw->AddText({x, center.y - label_size.y * 0.5f}, col, label.c_str());

	return pressed;
}

bool icon_button(const char * icon, const ImVec2 & size_arg, bool active, const std::string & tooltip)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	ImGuiContext & g = *GImGui;
	const ImGuiStyle & style = g.Style;
	const theme & t = current();

	// square touch target, same height as the sliders and other controls
	const float side = ImGui::GetFrameHeight() * metrics::control_height;
	const ImVec2 size = size_arg.x > 0 ? size_arg : ImVec2{side, side};

	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + size);
	ImGui::ItemSize(bb, style.FramePadding.y);
	const ImGuiID gid = window->GetID(icon);
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, gid, &hovered, &held);
	hover_haptic();
	if (hovered)
		show_tooltip(tooltip);

	ImVec4 bg = active ? t.accent : t.control;
	if (held)
		bg = active ? t.accent_active : t.control_active;
	else if (hovered)
		bg = active ? t.accent_hovered : t.control_hovered;

	ImDrawList * draw = window->DrawList;
	button_frame(draw, bb, bg, t.rounding);

	// centre the glyph on its ink box, not CalcTextSize
	const float glyph_size = size.y * metrics::icon_button_glyph;
	ImGui::PushFont(nullptr, glyph_size);
	const ImU32 col = t.col(active ? t.on_accent : t.text);
	const ImVec2 center = bb.GetCenter();

	unsigned int codepoint = 0;
	ImTextCharFromUtf8(&codepoint, icon, nullptr);
	const ImFontGlyph * glyph = ImGui::GetFontBaked()->FindGlyph(ImWchar(codepoint));
	if (glyph)
		draw->AddText({center.x - (glyph->X0 + glyph->X1) * 0.5f, center.y - (glyph->Y0 + glyph->Y1) * 0.5f}, col, icon);
	else
	{
		const ImVec2 ts = ImGui::CalcTextSize(icon);
		draw->AddText({center.x - ts.x * 0.5f, center.y - ts.y * 0.5f}, col, icon);
	}
	ImGui::PopFont();

	return pressed;
}

bool cancel_progress_button(const char * id, float fraction, const ImVec2 & size_arg, const std::string & tooltip)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	ImGuiContext & g = *GImGui;
	const theme & t = current();

	const float side = ImGui::GetFrameHeight() * metrics::control_height;
	const ImVec2 size = size_arg.x > 0 ? size_arg : ImVec2{side, side};

	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + size);
	ImGui::ItemSize(bb, g.Style.FramePadding.y);
	const ImGuiID gid = window->GetID(id);
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, gid, &hovered, &held);
	hover_haptic();
	if (hovered)
		show_tooltip(tooltip);

	ImDrawList * draw = window->DrawList;
	const ImVec2 center = bb.GetCenter();
	const float thickness = ImMax(2.f, size.y * 0.09f);
	const float radius = ImMin(size.x, size.y) * 0.5f - thickness;

	// faint full ring as the track
	draw->PathArcTo(center, radius, 0, 2 * IM_PI, 48);
	draw->PathStroke(t.col(t.control), thickness);

	const ImU32 accent = t.col(t.accent);
	if (fraction >= 0)
	{
		// determinate arc, clockwise from the top
		const float a0 = -IM_PI * 0.5f;
		draw->PathArcTo(center, radius, a0, a0 + ImClamp(fraction, 0.f, 1.f) * 2 * IM_PI, 48);
		draw->PathStroke(accent, thickness);
	}
	else
	{
		// indeterminate: a rotating quarter arc
		const float a0 = float(g.Time) * 3.5f;
		draw->PathArcTo(center, radius, a0, a0 + IM_PI * 0.5f, 24);
		draw->PathStroke(accent, thickness);
	}

	// stop glyph in the centre, brightening on hover to read as a cancel target
	const float glyph_size = size.y * metrics::icon_button_glyph * 0.7f;
	ImGui::PushFont(nullptr, glyph_size);
	const ImU32 col = t.col(hovered or held ? t.text : t.text_muted);
	unsigned int codepoint = 0;
	ImTextCharFromUtf8(&codepoint, ICON_FA_STOP, nullptr);
	const ImFontGlyph * glyph = ImGui::GetFontBaked()->FindGlyph(ImWchar(codepoint));
	if (glyph)
		draw->AddText({center.x - (glyph->X0 + glyph->X1) * 0.5f, center.y - (glyph->Y0 + glyph->Y1) * 0.5f}, col, ICON_FA_STOP);
	ImGui::PopFont();

	return pressed;
}

// Horizontal space a trailing reset slot consumes, gap included
float reset_slot_width()
{
	return ImGui::GetFrameHeight() * metrics::control_height + ImGui::GetStyle().ItemSpacing.x;
}

// trailing reset button on a value control's row, empty slot when not shown so the right edge stays put
static bool reset_slot(const char * id, bool show)
{
	const float side = ImGui::GetFrameHeight() * metrics::control_height;
	ImGui::SameLine(0, ImGui::GetStyle().ItemSpacing.x);
	if (not show)
	{
		ImGui::Dummy({side, side});
		return false;
	}
	ImGui::PushID(id);
	const bool clicked = icon_button(ICON_FA_ARROW_ROTATE_LEFT, {side, side}, false, "Reset to default");
	ImGui::PopID();
	return clicked;
}

bool toggle(const char * id, bool * v, const bool * default_value)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	const theme & t = current();
	const ImGuiStyle & style = ImGui::GetStyle();

	// match the height of the other controls
	const float height = ImGui::GetFrameHeight() * metrics::control_height;
	const float width = height * metrics::toggle_aspect;
	const float radius = height * 0.5f;

	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + ImVec2(width, height));
	ImGui::ItemSize(bb, style.FramePadding.y);
	const ImGuiID gid = window->GetID(id);
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	bool hovered, held;
	bool changed = ImGui::ButtonBehavior(bb, gid, &hovered, &held);
	hover_haptic();
	if (changed)
		*v = not *v;

	ImVec4 track = *v ? t.accent : t.control;
	if (hovered)
		track = *v ? t.accent_hovered : t.control_hovered;

	ImDrawList * draw = window->DrawList;
	draw->AddRectFilled(bb.Min, bb.Max, t.col(track), radius);
	const float cx = *v ? (bb.Max.x - radius) : (bb.Min.x + radius);
	draw->AddCircleFilled({cx, bb.Min.y + radius}, radius - metrics::toggle_knob_inset, t.col(t.on_accent));

	if (default_value and reset_slot(id, *v != *default_value))
	{
		*v = *default_value;
		changed = true;
	}

	return changed;
}

bool slider_int(const char * id, int * v, int v_min, int v_max, const char * format, const ImVec2 & size_arg, const int * default_value)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	ImGuiContext & g = *GImGui;
	const ImGuiStyle & style = g.Style;
	const theme & t = current();
	const ImGuiID gid = window->GetID(id);

	ImVec2 size = ImGui::CalcItemSize(size_arg, ImGui::CalcItemWidth(), ImGui::GetFrameHeight() * metrics::control_height);
	if (default_value)
		size.x -= reset_slot_width();
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + size);
	ImGui::ItemSize(bb, style.FramePadding.y);
	if (not ImGui::ItemAdd(bb, gid, &bb))
		return false;

	// activation, mirrors ImGui::SliderScalar
	const bool hovered = ImGui::ItemHoverable(bb, gid, g.LastItemData.ItemFlags);
	hover_haptic();
	const bool clicked = hovered and ImGui::IsMouseClicked(ImGuiMouseButton_Left, ImGuiInputFlags_None, gid);
	if (clicked or g.NavActivateId == gid)
	{
		if (clicked)
			ImGui::SetKeyOwner(ImGuiKey_MouseLeft, gid);
		ImGui::SetActiveID(gid, window);
		ImGui::SetFocusID(gid, window);
		ImGui::FocusWindow(window);
		g.ActiveIdUsingNavDirMask |= (1 << ImGuiDir_Left) | (1 << ImGuiDir_Right);
	}

	ImRect grab;
	const bool changed = ImGui::SliderBehavior(bb, gid, ImGuiDataType_S32, v, &v_min, &v_max, format, ImGuiSliderFlags_None, &grab);
	if (changed)
		ImGui::MarkItemEdited(gid);

	const float fraction = v_max > v_min ? float(*v - v_min) / float(v_max - v_min) : 0;
	const float fill_x = bb.Min.x + (bb.Max.x - bb.Min.x) * fraction;

	ImDrawList * draw = window->DrawList;
	draw->AddRectFilled(bb.Min, bb.Max, t.col(t.control), t.rounding);
	if (fraction > 0)
	{
		// round the right corners too once the fill reaches the box edge
		const ImDrawFlags fill_flags = fill_x >= bb.Max.x - t.rounding ? ImDrawFlags_RoundCornersAll : ImDrawFlags_RoundCornersLeft;
		draw->AddRectFilled(bb.Min, {fill_x, bb.Max.y}, t.col(t.accent_active), t.rounding, fill_flags);
	}
	// bright grab at the fill edge
	const float grab_w = metrics::slider_grab_width;
	const float gx = ImClamp(fill_x, bb.Min.x + grab_w * 0.5f, bb.Max.x - grab_w * 0.5f);
	draw->AddRectFilled({gx - grab_w * 0.5f, bb.Min.y}, {gx + grab_w * 0.5f, bb.Max.y}, t.col(t.accent), t.rounding);

	char buf[64];
	std::snprintf(buf, sizeof(buf), format, *v);
	const ImVec2 ts = ImGui::CalcTextSize(buf);
	draw->AddText({bb.Min.x + (size.x - ts.x) * 0.5f, bb.Min.y + (size.y - ts.y) * 0.5f}, t.col(t.text), buf);

	bool reset = false;
	if (default_value and reset_slot(id, *v != *default_value))
	{
		*v = *default_value;
		reset = true;
	}

	return changed or reset;
}

bool segmented(const char * id, const std::vector<std::string> & options, int * selected, const ImVec2 & size_arg, const int * default_value)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems or options.empty())
		return false;

	ImGuiContext & g = *GImGui;
	const ImGuiStyle & style = g.Style;
	const theme & t = current();

	ImVec2 size = ImGui::CalcItemSize(size_arg, ImGui::CalcItemWidth(), ImGui::GetFrameHeight() * metrics::control_height);
	if (default_value)
		size.x -= reset_slot_width();
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + size);
	ImGui::ItemSize(bb, style.FramePadding.y);
	const ImGuiID gid = window->GetID(id);
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	const bool hovered = ImGui::ItemHoverable(bb, gid, g.LastItemData.ItemFlags);
	hover_haptic();
	// claim the active id on press so a drag here scrolls nothing (see ScrollWhenDragging)
	if (hovered and ImGui::IsMouseClicked(ImGuiMouseButton_Left))
	{
		ImGui::SetActiveID(gid, window);
		ImGui::FocusWindow(window);
	}
	if (g.ActiveId == gid and not g.IO.MouseDown[ImGuiMouseButton_Left])
		ImGui::ClearActiveID();

	ImDrawList * draw = window->DrawList;
	draw->AddRectFilled(bb.Min, bb.Max, t.col(t.control), t.rounding);

	const int n = int(options.size());
	const float seg_w = size.x / n;
	const float pad = metrics::segmented_inset;
	bool changed = false;

	for (int i = 0; i < n; ++i)
	{
		const ImRect seg({bb.Min.x + seg_w * i, bb.Min.y}, {bb.Min.x + seg_w * (i + 1), bb.Max.y});
		const bool seg_hovered = hovered and seg.Contains(g.IO.MousePos);
		if (seg_hovered and ImGui::IsMouseClicked(ImGuiMouseButton_Left))
		{
			*selected = i;
			changed = true;
		}

		const bool active = *selected == i;
		if (active)
			draw->AddRectFilled(seg.Min + ImVec2(pad, pad), seg.Max - ImVec2(pad, pad), t.col(t.accent), t.rounding);
		else if (seg_hovered)
			draw->AddRectFilled(seg.Min + ImVec2(pad, pad), seg.Max - ImVec2(pad, pad), t.col(t.control_hovered), t.rounding);

		const char * label = options[i].c_str();
		const ImVec2 ts = ImGui::CalcTextSize(label);
		draw->AddText({seg.Min.x + (seg_w - ts.x) * 0.5f, seg.Min.y + (size.y - ts.y) * 0.5f}, t.col(active ? t.on_accent : t.text), label);
	}

	if (default_value and reset_slot(id, *selected != *default_value))
	{
		*selected = *default_value;
		changed = true;
	}

	return changed;
}

namespace
{
// where combo modals open, in imgui display coords, set each frame by the lobby
ImVec2 popup_center = {0, 0};
// height of the popup layer, caps tall combo lists so they scroll, 0 = unbounded
float popup_available_height = 0;

// one selectable option inside the combo modal, tall full-width touch target with an optional description
bool combo_row(const combo_item & item, bool selected, bool checked)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	const theme & t = current();

	const float row_h = ImGui::GetFrameHeight() * metrics::combo_row_height;
	const float row_w = ImGui::GetContentRegionAvail().x;
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + ImVec2(row_w, row_h));
	ImGui::ItemSize(bb, 0);
	const ImGuiID gid = window->GetID(item.name);
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, gid, &hovered, &held);
	hover_haptic();

	ImVec4 bg = selected ? t.control_active : t.card;
	if (held)
		bg = t.control_active;
	else if (hovered)
		bg = t.control_hovered;

	ImDrawList * draw = window->DrawList;
	draw->AddRectFilled(bb.Min, bb.Max, t.col(bg), t.rounding);
	if (selected)
		draw->AddRectFilled(bb.Min, {bb.Min.x + metrics::combo_padding.x * 0.25f, bb.Max.y}, t.col(t.accent), t.rounding, ImDrawFlags_RoundCornersLeft);

	if (checked)
	{
		const ImVec2 cs = ImGui::CalcTextSize(ICON_FA_CHECK);
		draw->AddText({bb.Max.x - metrics::combo_padding.x - cs.x, bb.Min.y + (row_h - cs.y) * 0.5f}, t.col(t.accent), ICON_FA_CHECK);
	}

	const float tx = bb.Min.x + metrics::combo_padding.x;
	const ImU32 name_col = t.col(t.text);
	const ImVec2 nts = ImGui::CalcTextSize(item.name);

	if (item.description and item.description[0])
	{
		const float desc_size = ImGui::GetStyle().FontSizeBase * metrics::font_description;
		ImGui::PushFont(nullptr, desc_size);
		const ImVec2 dts = ImGui::CalcTextSize(item.description);
		const float total = nts.y + metrics::label_line_gap + dts.y;
		const float y = bb.Min.y + (row_h - total) * 0.5f;
		draw->AddText({tx, y + nts.y + metrics::label_line_gap}, t.col(t.text_muted), item.description);
		ImGui::PopFont();
		draw->AddText({tx, y}, name_col, item.name);
	}
	else
	{
		draw->AddText({tx, bb.Min.y + (row_h - nts.y) * 0.5f}, name_col, item.name);
	}

	return pressed;
}

bool combo_impl(const char * id, const std::string & title, const char * preview, const std::vector<combo_item> & items, int * selected, std::vector<char> * selected_multi, float width, const int * default_value, const std::vector<char> * default_value_multi, bool close_on_click)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems or items.empty())
		return false;

	const ImGuiStyle & style = ImGui::GetStyle();
	const theme & t = current();
	const ImGuiID gid = window->GetID(id);

	// closed box, same height as the slider so setting rows line up
	ImVec2 size = ImGui::CalcItemSize({width, 0}, ImGui::CalcItemWidth(), ImGui::GetFrameHeight() * metrics::control_height);
	if (default_value or default_value_multi)
		size.x -= reset_slot_width();
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + size);
	ImGui::ItemSize(bb, style.FramePadding.y);
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	bool hovered, held;
	const bool clicked = ImGui::ButtonBehavior(bb, gid, &hovered, &held);
	hover_haptic();

	ImVec4 box = t.control;
	if (held)
		box = t.control_active;
	else if (hovered)
		box = t.control_hovered;

	ImDrawList * draw = window->DrawList;
	draw->AddRectFilled(bb.Min, bb.Max, t.col(box), t.rounding);
	if (t.border_size > 0)
		draw->AddRect(bb.Min, bb.Max, t.col(t.border), t.rounding, t.border_size);

	const int sel = selected ? ImClamp(*selected, 0, int(items.size()) - 1) : -1;

	// preview, left aligned and clipped to the box
	const ImVec2 ts = ImGui::CalcTextSize(preview);
	const float pad_x = metrics::combo_padding.x;
	draw->PushClipRect(bb.Min, bb.Max, true);
	draw->AddText({bb.Min.x + pad_x, bb.Min.y + (size.y - ts.y) * 0.5f}, t.col(t.text), preview);
	draw->PopClipRect();

	// chevron on the right
	const float cx = bb.Max.x - pad_x - metrics::combo_chevron * 0.5f;
	const float cy = bb.Min.y + size.y * 0.5f;
	const float r = metrics::combo_chevron * 0.5f;
	draw->AddTriangleFilled({cx - r, cy - r * 0.4f}, {cx + r, cy - r * 0.4f}, {cx, cy + r * 0.6f}, t.col(t.text_muted));

	char popup_id[64];
	std::snprintf(popup_id, sizeof(popup_id), "##combo_%08X", gid);
	if (clicked)
		ImGui::OpenPopup(popup_id);

	ImGui::SetNextWindowPos(popup_center, ImGuiCond_Always, {0.5f, 0.5f});
	ImGui::SetNextWindowSize({ImMax(size.x, metrics::combo_modal_min_width), 0});

	ImGui::PushStyleColor(ImGuiCol_PopupBg, t.background_contrast);
	ImGui::PushStyleColor(ImGuiCol_Border, t.border);
	ImGui::PushStyleColor(ImGuiCol_Text, t.text);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowRounding, t.card_rounding);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, metrics::card_padding);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowBorderSize, t.border_size > 0 ? t.border_size : 1);

	bool changed = false;
	if (ImGui::BeginPopupModal(popup_id, nullptr, ImGuiWindowFlags_NoTitleBar | ImGuiWindowFlags_NoResize | ImGuiWindowFlags_NoMove | ImGuiWindowFlags_AlwaysAutoResize))
	{
		if (not title.empty())
		{
			ImGui::PushStyleColor(ImGuiCol_Text, t.text_muted);
			ImGui::TextUnformatted(title.c_str());
			ImGui::PopStyleColor();
			ImGui::Dummy({0, style.ItemSpacing.y});
		}

		// cap the list height so a long list scrolls inside the modal instead of clipping
		const float row_h = ImGui::GetFrameHeight() * metrics::combo_row_height;
		// include the inter-row gaps or a scrollbar shows even when it all fits
		float list_h = int(items.size()) * row_h + ImMax(0, int(items.size()) - 1) * style.ItemSpacing.y;
		if (popup_available_height > 0)
		{
			const float title_h = title.empty() ? 0 : ImGui::GetTextLineHeight() + style.ItemSpacing.y;
			const float max_list = popup_available_height * 0.9f - 2 * metrics::card_padding.y - title_h;
			list_h = ImMin(list_h, ImMax(max_list, row_h));
		}

		ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {0, 0});
		ImGui::BeginChild("##list", {0, list_h});
		for (int i = 0; i < int(items.size()); ++i)
		{
			const bool is_selected = i == sel;
			const bool is_selected_multi = selected_multi and i < selected_multi->size() and (*selected_multi)[i];
			if (combo_row(items[i], is_selected, is_selected_multi))
			{
				if (selected)
					*selected = i;

				if (selected_multi)
				{
					if (selected_multi->size() <= i)
						selected_multi->resize(i + 1);
					(*selected_multi)[i] = not(*selected_multi)[i];
				}

				changed = true;
				if (close_on_click)
					ImGui::CloseCurrentPopup();
			}
			if (is_selected and ImGui::IsWindowAppearing())
				ImGui::SetScrollHereY(0.5f); // reveal the current value when opening
		}
		ImGui::EndChild();
		ImGui::PopStyleVar();

		// click outside dismisses, AllowWhenBlockedByActiveItem so pressing a row still counts as inside
		if (ImGui::IsMouseClicked(ImGuiMouseButton_Left) and not ImGui::IsWindowHovered(ImGuiHoveredFlags_RootAndChildWindows | ImGuiHoveredFlags_AllowWhenBlockedByActiveItem))
			ImGui::CloseCurrentPopup();

		ImGui::EndPopup();
	}

	ImGui::PopStyleVar(3);
	ImGui::PopStyleColor(3);

	bool show_reset_slot = (selected and default_value and *selected != *default_value) or
	                       (selected_multi and default_value_multi and *selected_multi != *default_value_multi);

	if (reset_slot(id, show_reset_slot))
	{
		if (selected and default_value)
			*selected = *default_value;

		if (selected_multi and default_value_multi)
			*selected_multi = *default_value_multi;

		changed = true;
	}

	return changed;
}
} // namespace

void set_popup_center(const ImVec2 & center, float available_height)
{
	popup_center = center;
	popup_available_height = available_height;
}

bool combo(const char * id, const std::string & title, const std::vector<combo_item> & items, int * selected, float width, const int * default_value)
{
	const int sel = ImClamp(*selected, 0, int(items.size()) - 1);
	const char * preview = items[sel].name;

	return combo_impl(id, title, preview, items, selected, nullptr, width, default_value, nullptr, true);
}

bool combo_multi(const char * id, const std::string & title, const std::string & preview, const std::vector<combo_item> & items, std::vector<char> * selected, float width, const std::vector<char> * default_value)
{
	return combo_impl(id, title, preview.c_str(), items, nullptr, selected, width, nullptr, default_value, false);
}

void chip(const std::string & label, chip_style style, bool dot, float height)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return;

	const theme & t = current();

	// foreground (text/dot/icon) and the pill fill
	ImVec4 fg = t.text;
	ImVec4 bg = t.control;
	switch (style)
	{
		case chip_style::neutral:
			break;
		case chip_style::muted:
			fg = t.text_muted;
			break;
		case chip_style::accent:
			fg = t.accent;
			bg = {t.accent.x, t.accent.y, t.accent.z, 0.16f};
			break;
		case chip_style::success:
			fg = t.success;
			bg = {t.success.x, t.success.y, t.success.z, 0.16f};
			break;
		case chip_style::warning:
			fg = t.warning;
			bg = {t.warning.x, t.warning.y, t.warning.z, 0.16f};
			break;
		case chip_style::danger:
			fg = t.danger;
			bg = {t.danger.x, t.danger.y, t.danger.z, 0.16f};
			break;
	}

	const ImVec2 ts = ImGui::CalcTextSize(label.c_str());
	const ImVec2 pad = metrics::chip_padding;
	// tall pill chips (height override) get roomier sides
	const float pad_x = height > 0 ? metrics::chip_pill_padding_x : pad.x;
	const float dot_r = ts.y * 0.26f;
	const float dot_gap = dot ? dot_r * 2 + pad_x * 0.55f : 0;
	const ImVec2 size = {ts.x + pad_x * 2 + dot_gap, height > 0 ? height : ts.y + pad.y * 2};

	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + size);
	ImGui::ItemSize(bb);
	if (not ImGui::ItemAdd(bb, 0))
		return;

	ImDrawList * draw = window->DrawList;
	draw->AddRectFilled(bb.Min, bb.Max, t.col(bg), size.y * 0.5f);

	float tx = bb.Min.x + pad_x;
	if (dot)
	{
		draw->AddCircleFilled({tx + dot_r, bb.Min.y + size.y * 0.5f}, dot_r, t.col(fg));
		tx += dot_r * 2 + pad_x * 0.55f;
	}
	draw->AddText({tx, bb.Min.y + (size.y - ts.y) * 0.5f}, t.col(fg), label.c_str());
}

void nav_section(const std::string & label)
{
	const theme & t = current();
	const ImGuiStyle & style = ImGui::GetStyle();

	ImGui::Dummy({0, metrics::nav_section_gap}); // gap above the section
	ImGui::Indent(metrics::combo_padding.x);     // align with the nav_item icons
	ImGui::PushFont(nullptr, style.FontSizeBase * metrics::font_description);
	ImGui::PushStyleColor(ImGuiCol_Text, t.text_muted);
	ImGui::TextUnformatted(label.c_str());
	ImGui::PopStyleColor();
	ImGui::PopFont();
	ImGui::Unindent(metrics::combo_padding.x);
	ImGui::Dummy({0, 4});
}

bool nav_item(const char * icon, const std::string & label, bool selected)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	const theme & t = current();
	const ImGuiStyle & style = ImGui::GetStyle();

	const float h = ImGui::GetFrameHeight() * metrics::control_height;
	const float w = ImGui::GetContentRegionAvail().x;
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + ImVec2(w, h));
	ImGui::ItemSize(bb, style.FramePadding.y);
	const ImGuiID gid = window->GetID(label.c_str());
	if (not ImGui::ItemAdd(bb, gid))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, gid, &hovered, &held);
	hover_haptic();

	ImDrawList * draw = window->DrawList;
	const float lum = t.background.x * 0.299f + t.background.y * 0.587f + t.background.z * 0.114f;
	if (selected)
	{
		// accent fill for the active tab
		draw->AddRectFilled(bb.Min, bb.Max, t.col(t.accent), t.rounding);
	}
	else if (hovered)
	{
		// lighten on dark themes, darken on light ones, the framebuffer blends in linear
		// light, so solve for the alpha that yields a constant perceived (gamma-space) step
		const float step = lum < 0.5f ? 0.10f : 0.06f; // perceptual lightness delta, stronger on dark
		auto to_linear = [](float c) { return c <= 0.04045f ? c / 12.92f : std::pow((c + 0.055f) / 1.055f, 2.4f); };
		const float overlay = lum < 0.5f ? 1.f : 0.f; // lighten vs darken
		const float ys = to_linear(lum);
		const float yt = to_linear(std::clamp(lum + (overlay - lum) / std::abs(overlay - lum) * step, 0.f, 1.f));
		const float a = (yt - ys) / (overlay - ys);
		draw->AddRectFilled(bb.Min, bb.Max, t.col({overlay, overlay, overlay, a}), t.rounding);
	}

	const float pad = metrics::combo_padding.x;
	const float icon_w = ImGui::GetFontSize() * 1.5f; // icon column

	const ImVec2 its = ImGui::CalcTextSize(icon);
	draw->AddText({bb.Min.x + pad + (icon_w - its.x) * 0.5f, bb.Min.y + (h - its.y) * 0.5f}, t.col(selected ? t.on_accent : t.text_muted), icon);

	const ImVec2 ls = ImGui::CalcTextSize(label.c_str());
	draw->AddText({bb.Min.x + pad + icon_w + pad * 0.5f, bb.Min.y + (h - ls.y) * 0.5f}, t.col(selected ? t.on_accent : t.text), label.c_str());

	return pressed;
}

static int input_text_resize(ImGuiInputTextCallbackData * data)
{
	if (data->EventFlag == ImGuiInputTextFlags_CallbackResize)
	{
		std::string & s = *static_cast<std::string *>(data->UserData);
		s.resize(data->BufTextLen);
		data->Buf = s.data();
	}
	return 0;
}

// common frame styling for the input widgets, paired with pop_input_style
static void push_input_style(float height)
{
	const theme & t = current();
	ImGui::PushStyleColor(ImGuiCol_FrameBg, t.control);
	ImGui::PushStyleColor(ImGuiCol_FrameBgHovered, t.control_hovered);
	ImGui::PushStyleColor(ImGuiCol_FrameBgActive, t.control_active);
	ImGui::PushStyleColor(ImGuiCol_Text, t.text);
	ImGui::PushStyleColor(ImGuiCol_TextDisabled, t.text_muted);
	ImGui::PushStyleColor(ImGuiCol_Border, t.border);
	ImGui::PushStyleVar(ImGuiStyleVar_FrameRounding, t.rounding);
	ImGui::PushStyleVar(ImGuiStyleVar_FrameBorderSize, t.border_size);
	ImGui::PushStyleVar(ImGuiStyleVar_FramePadding, {metrics::input_padding_x, (height - ImGui::GetFontSize()) * 0.5f});
}

static void pop_input_style()
{
	ImGui::PopStyleVar(3);
	ImGui::PopStyleColor(6);
}

bool input_text(const char * id, std::string & text, const char * hint, float width)
{
	push_input_style(ImGui::GetFrameHeight() * metrics::control_height);
	const bool changed = ImGui::InputTextEx(id, hint, text.data(), int(text.size()) + 1, {width, 0}, ImGuiInputTextFlags_CallbackResize, input_text_resize, &text);
	pop_input_style();
	hover_haptic();
	return changed;
}

bool input_int(const char * id, int * v, int step, int v_min, int v_max, float width)
{
	const ImGuiStyle & style = ImGui::GetStyle();
	const float side = ImGui::GetFrameHeight() * metrics::control_height;
	const float gap = style.ItemSpacing.x;
	const float total = width > 0 ? width : ImGui::CalcItemWidth();
	const float field_w = total - 2 * (side + gap);

	ImGui::PushID(id);

	push_input_style(side);
	ImGui::SetNextItemWidth(field_w);
	bool changed = ImGui::InputInt("##field", v, 0, 0, ImGuiInputTextFlags_CharsDecimal); // step 0 hides native buttons
	pop_input_style();
	hover_haptic();

	ImGui::SameLine(0, gap);
	if (icon_button(ICON_FA_MINUS, {side, side}))
	{
		*v -= step;
		changed = true;
	}
	ImGui::SameLine(0, gap);
	if (icon_button(ICON_FA_PLUS, {side, side}))
	{
		*v += step;
		changed = true;
	}

	if (v_min != v_max)
		*v = ImClamp(*v, v_min, v_max);

	ImGui::PopID();
	return changed;
}

bool begin_modal(const char * id, const std::string & title, float width)
{
	const theme & t = current();
	const ImGuiStyle & style = ImGui::GetStyle();

	ImGui::SetNextWindowPos(popup_center, ImGuiCond_Always, {0.5f, 0.5f});
	if (width > 0)
		ImGui::SetNextWindowSize({width, 0});

	ImGui::PushStyleColor(ImGuiCol_PopupBg, t.background_contrast); // LVRR: card is translucent, the panel behind would show through
	ImGui::PushStyleColor(ImGuiCol_Border, t.border);
	ImGui::PushStyleColor(ImGuiCol_Text, t.text);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowRounding, t.card_rounding);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, metrics::card_padding);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowBorderSize, t.border_size > 0 ? t.border_size : 1);

	ImGuiWindowFlags flags = ImGuiWindowFlags_NoTitleBar | ImGuiWindowFlags_NoResize | ImGuiWindowFlags_NoMove;
	if (width <= 0)
		flags |= ImGuiWindowFlags_AlwaysAutoResize;

	const bool open = ImGui::BeginPopupModal(id, nullptr, flags);
	if (open)
	{
		if (not title.empty())
		{
			ImGui::PushFont(nullptr, style.FontSizeBase * metrics::font_modal_title);
			ImGui::TextUnformatted(title.c_str());
			ImGui::PopFont();
			ImGui::Dummy({0, style.ItemSpacing.y});
		}
	}
	else
	{
		ImGui::PopStyleVar(3);
		ImGui::PopStyleColor(3);
	}
	return open;
}

void end_modal()
{
	ImGui::EndPopup();
	ImGui::PopStyleVar(3);
	ImGui::PopStyleColor(3);
}

int confirm_modal(const char * id, const std::string & title, const std::string & message, const std::string & confirm_label, const std::string & cancel_label, bool danger)
{
	int result = 0;
	if (begin_modal(id, title, 520))
	{
		const theme & t = current();
		ImGui::PushStyleColor(ImGuiCol_Text, t.text_muted);
		ImGui::TextWrapped("%s", message.c_str());
		ImGui::PopStyleColor();
		ImGui::Dummy({0, 12});

		const float gap = ImGui::GetStyle().ItemSpacing.x;
		const float cancel_w = ImGui::CalcTextSize(cancel_label.c_str()).x + metrics::button_padding.x * 2;
		const float confirm_w = ImGui::CalcTextSize(confirm_label.c_str()).x + metrics::button_padding.x * 2;
		ImGui::SetCursorPosX(ImGui::GetCursorPosX() + ImGui::GetContentRegionAvail().x - cancel_w - confirm_w - gap);
		if (button(cancel_label, button_style::secondary, {cancel_w, 0}))
		{
			result = -1;
			ImGui::CloseCurrentPopup();
		}
		ImGui::SameLine();
		if (button(confirm_label, danger ? button_style::danger : button_style::primary, {confirm_w, 0}))
		{
			result = 1;
			ImGui::CloseCurrentPopup();
		}
		end_modal();
	}
	return result;
}

namespace
{
std::vector<ImVec2> list_row_after; // cursor to restore at end_list_row
}

list_row_result begin_list_row(const char * id, const char * icon, ImTextureID image, const std::string & title, const std::string & subtitle, bool selected, float trailing_width, float height, bool large_thumb, bool interactive)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	const theme & t = current();
	const ImGuiStyle & style = ImGui::GetStyle();

	// measure title + (possibly multi-line) subtitle so the row can auto-size
	const float pad = metrics::list_row_pad;
	const float box = large_thumb ? metrics::list_row_box_large : metrics::list_row_box;
	const ImVec2 ts = ImGui::CalcTextSize(title.c_str());
	ImVec2 sub_sz{0, 0};
	if (not subtitle.empty())
	{
		ImGui::PushFont(nullptr, style.FontSizeBase * metrics::font_description);
		sub_sz = ImGui::CalcTextSize(subtitle.c_str());
		ImGui::PopFont();
	}
	const float text_h = ts.y + (subtitle.empty() ? 0 : metrics::label_line_gap + sub_sz.y);
	const float row_h = height > 0 ? height : ImMax(box, text_h) + 2 * pad;
	const ImVec2 p0 = window->DC.CursorPos;
	const float avail = ImGui::GetContentRegionAvail().x;

	// non-interactive rows just reserve layout space, no click area and no hover highlight
	bool clicked = false;
	if (interactive)
	{
		// the click area excludes the trailing controls so they receive their own clicks
		ImGui::SetNextItemAllowOverlap();
		clicked = ImGui::InvisibleButton(id, {ImMax(avail - trailing_width, 1.f), row_h});
		hover_haptic();
	}
	else
		ImGui::Dummy({avail, row_h});
	const bool hovered = interactive and ImGui::IsItemHovered();
	list_row_after.push_back(window->DC.CursorPos);

	ImDrawList * draw = window->DrawList;
	const ImRect bb(p0, p0 + ImVec2(avail, row_h));
	// no tinted background for the selected row, callers mark the active item with a chip
	if (hovered)
		draw->AddRectFilled(bb.Min, bb.Max, t.col(t.control), t.card_rounding);

	// leading thumbnail or icon box, large_thumb spans the full row height
	const float thumb = large_thumb ? row_h - 2 * pad : box;
	const ImVec2 box_min = {p0.x + pad, p0.y + (row_h - thumb) * 0.5f};
	const ImVec2 box_max = box_min + ImVec2(thumb, thumb);
	if (image)
		draw->AddImageRounded(image, box_min, box_max, {0, 0}, {1, 1}, IM_COL32_WHITE, t.rounding);
	else
	{
		draw->AddRectFilled(box_min, box_max, t.col(selected ? t.accent : t.control), t.rounding);
		if (icon and icon[0])
		{
			const ImVec2 is = ImGui::CalcTextSize(icon);
			draw->AddText(box_min + (ImVec2(thumb, thumb) - is) * 0.5f, t.col(selected ? t.on_accent : t.text_muted), icon);
		}
	}

	// title + subtitle
	const float tx = box_max.x + pad;
	const float total = subtitle.empty() ? ts.y : ts.y + metrics::label_line_gap + sub_sz.y;
	const float ty = p0.y + (row_h - total) * 0.5f;
	draw->AddText({tx, ty}, t.col(t.text), title.c_str());
	if (not subtitle.empty())
	{
		ImGui::PushFont(nullptr, style.FontSizeBase * metrics::font_description);
		draw->AddText({tx, ty + ts.y + metrics::label_line_gap}, t.col(t.text_muted), subtitle.c_str());
		ImGui::PopFont();
	}

	// trailing controls right-align against the inner padding
	return {bb.Min, {bb.Max.x - pad, bb.Max.y}, clicked};
}

void end_list_row()
{
	ImGui::GetCurrentWindow()->DC.CursorPos = list_row_after.back();
	list_row_after.pop_back();
}

int action_menu(const char * id, const char * icon, const std::vector<action_item> & items)
{
	const theme & t = current();

	if (icon_button(icon, {}, false))
		ImGui::OpenPopup(id);

	int chosen = -1;

	const float pad = metrics::combo_padding.x;
	const float icon_w = ImGui::GetFontSize() * 1.5f;
	float content_w = 0;
	for (const auto & it: items)
		content_w = ImMax(content_w, icon_w + pad + ImGui::CalcTextSize(it.label).x + pad * 2 + ImGui::GetFontSize());

	// centred modal on the popup layer, like combo()
	ImGui::SetNextWindowPos(popup_center, ImGuiCond_Always, {0.5f, 0.5f});
	ImGui::SetNextWindowSize({ImMax(content_w, metrics::combo_modal_min_width), 0});

	ImGui::PushStyleColor(ImGuiCol_PopupBg, t.background_contrast); // LVRR: card is translucent, the panel behind would show through
	ImGui::PushStyleColor(ImGuiCol_Border, t.border);
	ImGui::PushStyleColor(ImGuiCol_Text, t.text);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowRounding, t.card_rounding);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, metrics::card_padding);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowBorderSize, t.border_size > 0 ? t.border_size : 1);

	if (ImGui::BeginPopupModal(id, nullptr, ImGuiWindowFlags_NoTitleBar | ImGuiWindowFlags_NoResize | ImGuiWindowFlags_NoMove | ImGuiWindowFlags_AlwaysAutoResize))
	{
		ImGuiWindow * window = ImGui::GetCurrentWindow();
		const float row_h = ImGui::GetFrameHeight() * metrics::control_height;

		for (int i = 0; i < int(items.size()); ++i)
		{
			const action_item & it = items[i];
			ImGui::PushID(i);
			const float row_w = ImGui::GetContentRegionAvail().x;
			const ImVec2 pos = window->DC.CursorPos;
			const ImRect bb(pos, pos + ImVec2(row_w, row_h));
			ImGui::ItemSize(bb, 0);
			const ImGuiID gid = window->GetID("##row");
			if (ImGui::ItemAdd(bb, gid))
			{
				bool hovered, held;
				if (ImGui::ButtonBehavior(bb, gid, &hovered, &held))
				{
					chosen = i;
					ImGui::CloseCurrentPopup();
				}
				hover_haptic();

				ImDrawList * draw = window->DrawList;
				if (hovered)
					draw->AddRectFilled(bb.Min, bb.Max, t.col(t.control_hovered), t.rounding);

				const ImU32 fg = t.col(it.danger ? t.danger : t.text);
				const ImVec2 its = ImGui::CalcTextSize(it.icon);
				draw->AddText({bb.Min.x + pad + (icon_w - its.x) * 0.5f, bb.Min.y + (row_h - its.y) * 0.5f}, fg, it.icon);
				const ImVec2 ls = ImGui::CalcTextSize(it.label);
				draw->AddText({bb.Min.x + pad + icon_w, bb.Min.y + (row_h - ls.y) * 0.5f}, fg, it.label);
				if (it.checked)
				{
					const ImVec2 cs = ImGui::CalcTextSize(ICON_FA_CHECK);
					draw->AddText({bb.Max.x - pad - cs.x, bb.Min.y + (row_h - cs.y) * 0.5f}, t.col(t.accent), ICON_FA_CHECK);
				}
			}
			ImGui::PopID();
		}

		// click outside dismisses, AllowWhenBlockedByActiveItem so pressing a row still counts as inside
		if (ImGui::IsMouseClicked(ImGuiMouseButton_Left) and not ImGui::IsWindowHovered(ImGuiHoveredFlags_RootAndChildWindows | ImGuiHoveredFlags_AllowWhenBlockedByActiveItem))
			ImGui::CloseCurrentPopup();

		ImGui::EndPopup();
	}

	ImGui::PopStyleVar(3);
	ImGui::PopStyleColor(3);
	return chosen;
}

void top_bar(float height, ImTextureID logo, const std::vector<top_bar_item> & right_items)
{
	const float side = ImGui::GetFrameHeight() * metrics::control_height;
	const float gap = 8;
	const float margin = 24;

	// transparent child bg so the alpha-blended window background shows through
	ImGui::SetCursorPos({0, 0});
	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::BeginChild("TopBar", {ImGui::GetWindowSize().x, height}, 0, ImGuiWindowFlags_NoScrollbar);
	{
		// logo: WiVRn mascot followed by the wordmark
		const float logo_size = 44;
		ImGui::SetCursorPos({margin, (height - logo_size) * 0.5f});
		if (logo)
			ImGui::Image(logo, {logo_size, logo_size});
		ImGui::SameLine(0, 12);
		ImGui::PushFont(nullptr, ImGui::GetStyle().FontSizeBase * 1.3f);
		ImGui::SetCursorPosY((height - ImGui::GetFontSize()) * 0.5f);
		ImGui::TextUnformatted("LVRR");
		ImGui::PopFont();

		// right-aligned cluster
		float cluster_w = 0;
		for (const auto & it: right_items)
			cluster_w += it.width;
		if (not right_items.empty())
			cluster_w += gap * (right_items.size() - 1);

		ImGui::SetCursorPos({ImGui::GetWindowSize().x - margin - cluster_w, (height - side) * 0.5f});
		for (size_t i = 0; i < right_items.size(); ++i)
		{
			ImGui::SetCursorPosY((height - side) * 0.5f);
			right_items[i].draw();
			if (i + 1 < right_items.size())
				ImGui::SameLine(0, gap);
		}
	}
	ImGui::EndChild();
	ImGui::PopStyleColor();
}

void begin_sidebar(float top_bar_h, float tab_width, int footer_items)
{
	// shares the transparent window background, sections separated by shell_dividers
	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::SetCursorPos({0, top_bar_h});
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {16, 16});
	ImGui::BeginChild("Tabs", {tab_width, ImGui::GetWindowSize().y - top_bar_h}, ImGuiChildFlags_AlwaysUseWindowPadding);
	ImGui::PushStyleVar(ImGuiStyleVar_ItemSpacing, {0, 4});

	// nav list scrolls in a child that fills the rest, footer stays pinned in footer_items rows
	const float row_h = ImGui::GetFrameHeight() * metrics::control_height + ImGui::GetStyle().ItemSpacing.y;
	sidebar_footer_height = footer_items > 0 ? footer_items * row_h : 0;
	ImGui::BeginChild("TabsScroll", {0, ImGui::GetContentRegionAvail().y - sidebar_footer_height});
}

void sidebar_footer()
{
	::ScrollWhenDragging(); // drag to scroll the nav list, like the rest of the GUI
	ImGui::EndChild();      // TabsScroll
	// pin the footer to the bottom of the sidebar
	ImGui::SetCursorPosY(ImGui::GetContentRegionAvail().y + ImGui::GetCursorScreenPos().y - ImGui::GetWindowPos().y - sidebar_footer_height);
}

void end_sidebar()
{
	ImGui::PopStyleVar();   // ImGuiStyleVar_ItemSpacing
	ImGui::EndChild();      // Tabs
	ImGui::PopStyleVar();   // ImGuiStyleVar_WindowPadding
	ImGui::PopStyleColor(); // ImGuiCol_ChildBg
}

void shell_dividers(float top_bar_h, float tab_width)
{
	ImVec4 divider_col = current().border;
	divider_col.w *= background_alpha(); // follow the panel opacity
	const ImU32 divider = ImGui::GetColorU32(divider_col);
	const ImVec2 win_pos = ImGui::GetWindowPos();
	const ImVec2 win_size = ImGui::GetWindowSize();
	ImDrawList * dl = ImGui::GetWindowDrawList();
	dl->AddLine({win_pos.x, win_pos.y + top_bar_h}, {win_pos.x + win_size.x, win_pos.y + top_bar_h}, divider);
	dl->AddLine({win_pos.x + tab_width, win_pos.y + top_bar_h}, {win_pos.x + tab_width, win_pos.y + win_size.y}, divider);
}

// LVRR: the dashboard's shell

void gradient_rect(ImDrawList * draw, const ImVec2 & min, const ImVec2 & max, float rounding, const ImVec4 & from, const ImVec4 & to, bool horizontal, ImDrawFlags corners)
{
	const theme & t = current();
	const int first = draw->VtxBuffer.Size;
	// AddConvexPolyFilled draws nothing for alpha 0, so fill with the more opaque end
	const ImU32 fill = t.col(from.w >= to.w ? from : to);
	draw->PathRect(min, max, rounding, corners);
	draw->PathFillConvex(fill);

	// the fan's vertices sit on the outline, recolor them along the gradient and keep the AA fringe alpha
	const float len = std::max(horizontal ? max.x - min.x : max.y - min.y, 1.f);
	for (int i = first; i < draw->VtxBuffer.Size; ++i)
	{
		ImDrawVert & v = draw->VtxBuffer[i];
		const float f = std::clamp((horizontal ? v.pos.x - min.x : v.pos.y - min.y) / len, 0.f, 1.f);
		const ImVec4 c = {std::lerp(from.x, to.x, f), std::lerp(from.y, to.y, f), std::lerp(from.z, to.z, f), std::lerp(from.w, to.w, f)};
		const ImU32 packed = t.col(c);
		// fringe vertices were written with alpha 0, the others with the fill's alpha
		const ImU32 fill_a = (fill >> IM_COL32_A_SHIFT) & 0xff;
		const ImU32 fringe = fill_a ? ((v.col >> IM_COL32_A_SHIFT) & 0xff) * 255 / fill_a : 0;
		const ImU32 a = fringe * ((packed >> IM_COL32_A_SHIFT) & 0xff) / 255;
		v.col = (packed & ~IM_COL32_A_MASK) | (std::min<ImU32>(a, 255) << IM_COL32_A_SHIFT);
	}
}

namespace
{
// a soft ellipse of light inscribed in the rect, the dashboard's radial gradient
void glow_ellipse(ImDrawList * draw, const ImVec2 & min, const ImVec2 & max, const ImVec4 & color)
{
	const theme & t = current();
	const ImVec2 center = (min + max) * 0.5f;
	const ImVec2 radius = (max - min) * 0.5f;
	const int n = 64;
	const ImU32 col_center = t.col(color);
	const ImU32 col_edge = col_center & ~IM_COL32_A_MASK;
	const ImVec2 uv = draw->_Data->TexUvWhitePixel;

	draw->PrimReserve(n * 3, n + 1);
	const unsigned int base = draw->_VtxCurrentIdx;
	draw->PrimWriteVtx(center, uv, col_center);
	for (int i = 0; i < n; ++i)
	{
		const float a = i * 2 * float(M_PI) / n;
		draw->PrimWriteVtx({center.x + radius.x * std::cos(a), center.y + radius.y * std::sin(a)}, uv, col_edge);
	}
	for (int i = 0; i < n; ++i)
	{
		draw->PrimWriteIdx(ImDrawIdx(base));
		draw->PrimWriteIdx(ImDrawIdx(base + 1 + i));
		draw->PrimWriteIdx(ImDrawIdx(base + 1 + (i + 1) % n));
	}
}

// glyph centred on its ink box
void glyph_centered(ImDrawList * draw, const char * icon, const ImVec2 & center, float size, ImU32 col)
{
	ImGui::PushFont(nullptr, size);
	unsigned int codepoint = 0;
	ImTextCharFromUtf8(&codepoint, icon, nullptr);
	const ImFontGlyph * glyph = ImGui::GetFontBaked()->FindGlyph(ImWchar(codepoint));
	if (glyph)
		draw->AddText({center.x - (glyph->X0 + glyph->X1) * 0.5f, center.y - (glyph->Y0 + glyph->Y1) * 0.5f}, col, icon);
	else
	{
		const ImVec2 ts = ImGui::CalcTextSize(icon);
		draw->AddText(center - ts * 0.5f, col, icon);
	}
	ImGui::PopFont();
}

float side_items_height = 0;
} // namespace

void shell_side_begin(float height, int buttons, int separators)
{
	const theme & t = current();
	const float w = metrics::shell_side_width;
	const float gap = 6;

	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {0, 0});
	ImGui::PushStyleVar(ImGuiStyleVar_ItemSpacing, {0, gap});
	ImGui::BeginChild("Side", {w, height}, 0, ImGuiWindowFlags_NoScrollbar | ImGuiWindowFlags_NoScrollWithMouse);

	// the pill wraps the buttons, in the middle of the column
	side_items_height = buttons * w + separators * (metrics::card_title_rule + gap * 2) + (buttons + separators - 1) * gap;
	const float top = std::max(0.f, (height - side_items_height) * 0.5f);

	ImGuiWindow * window = ImGui::GetCurrentWindow();
	const ImVec2 p = window->Pos + ImVec2{0, top};
	window->DrawList->AddRectFilled(p, p + ImVec2{w, side_items_height}, t.col(t.background_contrast), w * 0.5f);

	ImGui::SetCursorPosY(top);
}

bool shell_side_button(const char * icon, const std::string & tooltip, bool selected)
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	if (window->SkipItems)
		return false;

	const theme & t = current();
	const float w = metrics::shell_side_width;
	const ImVec2 pos = window->DC.CursorPos;
	const ImRect bb(pos, pos + ImVec2{w, w});
	ImGui::ItemSize(bb);
	const ImGuiID id = window->GetID(icon);
	if (not ImGui::ItemAdd(bb, id))
		return false;

	bool hovered, held;
	const bool pressed = ImGui::ButtonBehavior(bb, id, &hovered, &held);
	hover_haptic();
	if (hovered)
		show_tooltip(tooltip);

	ImDrawList * draw = window->DrawList;
	const ImVec2 center = bb.GetCenter();
	if (selected or hovered or held)
	{
		ImVec4 bg = selected ? t.control : t.control_hovered;
		if (held)
			bg = t.control_active;
		else if (selected and hovered)
			bg = t.control_hovered;
		draw->AddCircleFilled(center, w * 0.5f - 2, t.col(bg));
	}
	glyph_centered(draw, icon, center, metrics::shell_side_glyph, t.col(selected or hovered ? t.text : t.text_muted));
	return pressed;
}

void shell_side_separator()
{
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	const theme & t = current();
	const float gap = 6;
	const float w = metrics::shell_side_glyph;
	const ImVec2 pos = window->DC.CursorPos;
	ImGui::Dummy({metrics::shell_side_width, metrics::card_title_rule + gap * 2});
	const float x = pos.x + (metrics::shell_side_width - w) * 0.5f;
	window->DrawList->AddRectFilled({x, pos.y + gap}, {x + w, pos.y + gap + metrics::card_title_rule}, t.col(t.border));
}

void shell_side_end()
{
	ImGui::EndChild();
	ImGui::PopStyleVar(2);
	ImGui::PopStyleColor();
}

bool shell_top(float width, const char * icon, const std::string & title, const std::string & close_tooltip)
{
	const theme & t = current();
	const float h = metrics::shell_top_height;

	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {0, 0});
	ImGui::BeginChild("Top", {width, h}, 0, ImGuiWindowFlags_NoScrollbar | ImGuiWindowFlags_NoScrollWithMouse);
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	ImDrawList * draw = window->DrawList;
	const ImVec2 p = window->Pos;

	// title pill in the middle, a gap clear of the close button; a longer title ends in an ellipsis
	ImFont * bold = bold_font();
	ImGui::PushFont(bold, ImGui::GetStyle().FontSizeBase * 1.1f);
	const ImVec2 ts = ImGui::CalcTextSize(title.c_str());
	const float glyph = metrics::shell_title_glyph;
	const float gap = 12;
	const float max_w = width - 2 * (h + gap);
	const float pill_w = std::clamp(glyph + gap + ts.x + 48, std::min(metrics::shell_title_min_width, max_w), max_w);
	const float text_w = std::max(0.f, std::min(ts.x, max_w - 48 - glyph - gap));
	const ImVec2 pill_min = {p.x + (width - pill_w) * 0.5f, p.y};
	const ImVec2 pill_max = pill_min + ImVec2{pill_w, h};
	draw->AddRectFilled(pill_min, pill_max, t.col(t.background_contrast), h * 0.5f);
	const float content_w = glyph + gap + text_w;
	float x = pill_min.x + (pill_w - content_w) * 0.5f;
	glyph_centered(draw, icon, {x + glyph * 0.5f, p.y + h * 0.5f}, glyph, t.col(t.text));
	x += glyph + gap;
	const ImVec2 text_pos = {x, p.y + (h - ts.y) * 0.5f};
	if (text_w < ts.x)
	{
		// draws in the current font and ImGuiCol_Text; with no end it would stop at "##"
		ImGui::PushStyleColor(ImGuiCol_Text, t.text);
		ImGui::RenderTextEllipsis(draw, text_pos, {x + text_w, p.y + h}, x + text_w, title.c_str(), title.c_str() + title.size(), &ts);
		ImGui::PopStyleColor();
	}
	else
		draw->AddText(text_pos, t.col(t.text), title.c_str());
	ImGui::PopFont();

	// close button at the right edge
	const ImVec2 pos = {p.x + width - h, p.y};
	ImGui::SetCursorScreenPos(pos);
	const ImRect bb(pos, pos + ImVec2{h, h});
	ImGui::ItemSize(bb);
	const ImGuiID id = window->GetID("close");
	bool pressed = false;
	if (ImGui::ItemAdd(bb, id))
	{
		bool hovered, held;
		pressed = ImGui::ButtonBehavior(bb, id, &hovered, &held);
		hover_haptic();
		if (hovered)
			show_tooltip(close_tooltip);
		const ImVec4 bg = held ? t.danger_hovered : hovered ? t.danger
		                                                    : t.background_contrast;
		draw->AddCircleFilled(bb.GetCenter(), h * 0.5f, t.col(bg));
		glyph_centered(draw, ICON_FA_XMARK, bb.GetCenter(), glyph, t.col(hovered or held ? t.on_accent : t.text));
	}

	ImGui::EndChild();
	ImGui::PopStyleVar();
	ImGui::PopStyleColor();
	return pressed;
}

void shell_content_begin(const ImVec2 & size, const ImVec2 & padding)
{
	const theme & t = current();
	const float r = metrics::shell_panel_rounding;

	// the panel's fill is drawn by the parent, behind the scrolling child
	ImDrawList * draw = ImGui::GetWindowDrawList();
	const ImVec2 min = ImGui::GetCursorScreenPos();
	const ImVec2 max = min + size;
	gradient_rect(draw, min, max, r, t.background, t.background_contrast);
	glow_ellipse(draw, min, max, {t.control.x, t.control.y, t.control.z, 0.5f});

	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::PushStyleVar(ImGuiStyleVar_ChildRounding, r);
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, padding);
	ImGui::BeginChild("Content", size, ImGuiChildFlags_AlwaysUseWindowPadding);
}

void shell_content_end()
{
	::ScrollWhenDragging();
	ImGui::EndChild();
	ImGui::PopStyleVar(2);
	ImGui::PopStyleColor();
}

void shell_bottom(float width, const std::function<void()> & left, const std::function<void()> & right)
{
	const theme & t = current();
	const float h = metrics::shell_bottom_height;
	const float r = metrics::shell_panel_rounding;

	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {0, 0});
	ImGui::BeginChild("Bottom", {width, h}, 0, ImGuiWindowFlags_NoScrollbar | ImGuiWindowFlags_NoScrollWithMouse);
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	ImDrawList * draw = window->DrawList;
	const ImVec2 p = window->Pos;
	const ImVec2 q = p + ImVec2{width, h};
	draw->AddRectFilled(p, q, t.col(t.background), r);
	// shadows at both ends, rounded like the bar so its ends get them too, and a shine along the
	// top over 99% of the width, like the dashboard's bar
	gradient_rect(draw, p, {p.x + width * 0.4f, q.y}, r, {0, 0, 0, 0.35f}, {0, 0, 0, 0}, true, ImDrawFlags_RoundCornersLeft);
	gradient_rect(draw, {q.x - width * 0.4f, p.y}, q, r, {0, 0, 0, 0}, {0, 0, 0, 0.35f}, true, ImDrawFlags_RoundCornersRight);
	const float inset = width * 0.005f;
	draw->AddRectFilled({p.x + inset, p.y}, {q.x - inset, p.y + 2}, t.col({1, 1, 1, 0.2f}));

	const float side = ImGui::GetFrameHeight() * metrics::control_height;
	ImGui::SetCursorPos({12, (h - side) * 0.5f});
	ImGui::BeginGroup();
	left();
	ImGui::EndGroup();

	// right-aligned cluster, measured from last frame's width
	static float right_w = 0;
	ImGui::SetCursorPos({width - 20 - right_w, (h - side) * 0.5f});
	ImGui::BeginGroup();
	right();
	ImGui::EndGroup();
	right_w = ImGui::GetItemRectSize().x;

	ImGui::EndChild();
	ImGui::PopStyleVar();
	ImGui::PopStyleColor();
}

void card_title(const char * icon, const std::string & title)
{
	const theme & t = current();
	ImGuiWindow * window = ImGui::GetCurrentWindow();
	ImDrawList * draw = window->DrawList;

	ImGui::PushFont(bold_font(), ImGui::GetStyle().FontSizeBase * 1.05f);
	const ImVec2 ts = ImGui::CalcTextSize(title.c_str());
	const float glyph = ts.y * 0.95f;
	const float gap = 10;
	const ImVec2 pos = window->DC.CursorPos;
	ImGui::Dummy({glyph + gap + ts.x, ts.y});
	glyph_centered(draw, icon, {pos.x + glyph * 0.5f, pos.y + ts.y * 0.5f}, glyph, t.col(t.heading));
	draw->AddText({pos.x + glyph + gap, pos.y}, t.col(t.heading), title.c_str());
	ImGui::PopFont();

	// rule under the title, full card width
	const ImVec2 rp = window->DC.CursorPos;
	const float w = ImGui::GetContentRegionAvail().x;
	ImGui::Dummy({w, metrics::card_title_rule});
	draw->AddRectFilled(rp, {rp.x + w, rp.y + metrics::card_title_rule}, t.col(t.border));
	ImGui::Dummy({0, 2});
}

} // namespace wivrn::ui
