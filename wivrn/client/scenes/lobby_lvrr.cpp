/*
 * LVRR: the lobby without WiVRn's settings, environments and wizard
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

#include "lobby.h"

#include "application.h"
#include "configuration.h"
#include "imgui.h"
#include "imgui_internal.h"
#include "lvrr.h"
#include "render/ui_theme.h"
#include "render/ui_widgets.h"
#include "scenes/stream.h"
#include "utils/i18n.h"
#include "version.h"
#include <IconsFontAwesome7.h>
#include <chrono>
#include <cmath>
#include <filesystem>
#include <fstream>
#include <imspinner.h>
#include <spdlog/fmt/fmt.h>
#include <spdlog/spdlog.h>
#include <string>

// The lobby only finds and connects to a PC. Once connected, the WayVR dashboard on the PC
// shows every setting, so the lobby waits for its video. When the server restarts the session
// to apply settings, it sends expect_reconnect first and the lobby reconnects without asking.

using namespace std::chrono_literals;

namespace
{
// How long a restarted session may take to accept the headset again
constexpr auto reconnect_window = 60s;
constexpr auto retry_delay = 1s;

// Written once autoconnect was turned on for the first paired PC, so it is only done once
const char * const autoconnect_stamp = "lvrr-autoconnect";
} // namespace

void scenes::lobby::lvrr_focused()
{
	if constexpr (lvrr::stock_ui)
		return;

	auto & config = application::get_config();
	// there is no environment page, the lobby shows the room when it can
	config.passthrough_enabled = system.passthrough_supported() != xr::passthrough_type::none;

	if (config.first_run)
	{
		// no wizard: hand tracking only, the dashboard asks for the rest when it is enabled
		config.first_run = false;
		if (system.hand_tracking_supported())
			config.set_feature(feature::hand_tracking, true);
		config.save();
	}
}

void scenes::lobby::lvrr_connected()
{
	if constexpr (lvrr::stock_ui)
		return;

	lvrr_reconnect_until = {};

	// like Virtual Desktop, connect to the first paired PC automatically from then on
	const auto stamp = application::get_config_path() / autoconnect_stamp;
	if (not lvrr_server or std::filesystem::exists(stamp))
		return;

	auto cookie = lvrr_server->service.txt.find("cookie");
	if (cookie == lvrr_server->service.txt.end())
		return;

	auto & config = application::get_config();
	auto server = config.servers.find(cookie->second);
	if (server == config.servers.end() or server->second.manual)
		return;

	server->second.autoconnect = true;
	config.save();
	std::ofstream{stamp};
	spdlog::info("Autoconnect turned on for {}", server->second.service.name);
}

bool scenes::lobby::lvrr_connect_failed()
{
	if constexpr (lvrr::stock_ui)
		return false;

	if (lvrr_reconnect_until == std::chrono::steady_clock::time_point{})
		return false;

	auto now = std::chrono::steady_clock::now();
	if (now >= lvrr_reconnect_until)
	{
		lvrr_reconnect_until = {};
		return false;
	}

	lvrr_retry_at = now + retry_delay;
	return true;
}

bool scenes::lobby::lvrr_stream_ended()
{
	if constexpr (lvrr::stock_ui)
		return false;

	if (not lvrr_server or not next_scene->reconnect_expected())
		return false;

	// the session restarts on purpose, its errors are expected
	while (next_scene->pop_stream_error())
		;
	next_scene.reset();

	auto now = std::chrono::steady_clock::now();
	lvrr_reconnect_until = now + reconnect_window;
	lvrr_retry_at = now;
	current_tab = tab::connected;
	spdlog::info("Reconnecting to {}", lvrr_server->service.hostname);
	return true;
}

void scenes::lobby::lvrr_tick()
{
	if constexpr (lvrr::stock_ui)
		return;

	if (lvrr_reconnect_until == std::chrono::steady_clock::time_point{} or not lvrr_server or async_session.valid() or next_scene)
		return;

	if (std::chrono::steady_clock::now() >= lvrr_retry_at)
		connect(*lvrr_server);
}

void scenes::lobby::lvrr_disconnect()
{
	lvrr_server.reset();
	lvrr_reconnect_until = {};
	async_error.reset();
	autoconnect_enabled = false;

	if (async_session.valid())
	{
		async_session.cancel();
		auto request = pin_request.lock();
		request->pin_cancelled = true;
		request.notify_one();
	}

	if (next_scene)
		next_scene->exit();

	current_tab = tab::server_list;
}

void scenes::lobby::lvrr_gui_waiting()
{
	namespace ui = wivrn::ui;

	// the PC asks to pair again: the server list shows the PIN keypad
	if (async_session.valid() and pin_request.lock()->pin_requested)
	{
		current_tab = tab::server_list;
		return;
	}

	if (not next_scene and not async_session.valid() and not async_error and lvrr_reconnect_until == std::chrono::steady_clock::time_point{})
	{
		current_tab = tab::server_list;
		return;
	}

	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {34, 34});
	ImGui::BeginChild("waiting", {0, 0}, ImGuiChildFlags_AlwaysUseWindowPadding);

	const float header_top = ImGui::GetCursorPosY();
	// connections from an intent (USB) have no server name
	std::string title;
	if (async_error)
		title = server_name.empty() ? _("Disconnected") : fmt::format(_F("Disconnected from {}"), server_name);
	else
		title = server_name.empty() ? _("Starting LVRR") : fmt::format(_F("Starting LVRR on {}"), server_name);

	if (async_error)
		ui::page_header(title, *async_error);
	else if (next_scene)
		ui::page_header(title, _("The desktop appears here in a moment. If it does not, restart it."));
	else
		ui::page_header(title, _("Reconnecting to the PC."));

	const std::string close_label = async_error ? _("Close") : _("Disconnect");
	const std::string restart_label = _("Restart desktop");
	const float gap = ImGui::GetStyle().ItemSpacing.x;
	const float close_w = ui::button_width(close_label);
	const float restart_w = ui::button_width(ICON_FA_ROTATE_RIGHT, restart_label);
	const float right = ImGui::GetContentRegionAvail().x + ImGui::GetCursorScreenPos().x - ImGui::GetWindowPos().x;

	ImGui::SetCursorPos({right - close_w, header_top});
	if (ui::button(close_label, async_error ? ui::button_style::primary : ui::button_style::danger))
		lvrr_disconnect();

	if (not async_error)
	{
		ImGui::SetCursorPos({right - close_w - gap - restart_w, header_top});
		ImGui::BeginDisabled(not next_scene);
		// starts the configured application on the PC when it is not running
		if (ui::button(ICON_FA_ROTATE_RIGHT, restart_label, ui::button_style::secondary) and next_scene)
			next_scene->start_application("");
		ImGui::EndDisabled();
	}

	ImGui::EndChild();
	ImGui::PopStyleVar();

	if (not async_error)
	{
		const float r = ui::metrics::app_spinner_radius;
		ImGui::SetCursorPos(ImGui::GetWindowSize() / 2 - ImVec2{r, r});
		ImSpinner::SpinnerAng("waiting", r, ui::metrics::app_spinner_thickness, ImColor{ui::current().accent}, ImColor{ui::current().card}, 6, 0.75f * 2 * M_PI);
	}
}

void scenes::lobby::lvrr_gui_sidebar()
{
	if (wivrn::ui::nav_item(ICON_FA_COMPUTER, _cS("tab label", "Computers"), current_tab == tab::server_list))
		current_tab = tab::server_list;

	// pinned to the bottom
	wivrn::ui::sidebar_footer();
	if (wivrn::ui::nav_item(ICON_FA_SCALE_BALANCED, _cS("tab label", "Licenses"), current_tab == tab::licenses))
		current_tab = tab::licenses;
	if (wivrn::ui::nav_item(ICON_FA_DOOR_OPEN, _cS("exit button in the lobby", "Exit"), current_tab == tab::exit))
		current_tab = tab::exit;
}

void scenes::lobby::lvrr_gui_version()
{
	if constexpr (lvrr::stock_ui)
		return;

	ImGui::PushStyleColor(ImGuiCol_Text, wivrn::ui::current().text_muted);
	ImGui::TextUnformatted((std::string("LVRR ") + wivrn::display_version()).c_str());
	ImGui::PopStyleColor();
}
