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
#include "gui_common.h"
#include "imgui.h"
#include "imgui_internal.h"
#include "lvrr.h"
#include "render/ui_theme.h"
#include "render/ui_widgets.h"
#include "scenes/stream.h"
#include "utils/i18n.h"
#include "version.h"
#include <IconsFontAwesome7.h>
#include <algorithm>
#include <arpa/inet.h>
#include <chrono>
#include <cmath>
#include <cstring>
#include <ctime>
#include <filesystem>
#include <fstream>
#include <ifaddrs.h>
#include <imspinner.h>
#include <net/if.h>
#include <spdlog/fmt/fmt.h>
#include <spdlog/spdlog.h>
#include <string>
#include <sys/socket.h>
#include <unistd.h>

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
// Written once the microphone was turned on, so turning it off later sticks
const char * const microphone_stamp = "lvrr-microphone";
// Written once hand tracking was turned on, so turning it off later sticks
const char * const hand_tracking_stamp = "lvrr-hand-tracking";
} // namespace

void scenes::lobby::lvrr_focused()
{
	if constexpr (lvrr::stock_ui)
		return;

	auto & config = application::get_config();
	// there is no environment page: the lobby shows the meadow, a sky dome that costs one
	// textured draw, where passthrough keeps the cameras and their pipeline running
	config.passthrough_enabled = false;
	config.environment_model = application::get_default_config().environment_model;

	if (config.first_run)
	{
		// no wizard: hand tracking only, the dashboard asks for the rest when it is enabled
		config.first_run = false;
		if (system.hand_tracking_supported())
			config.set_feature(feature::hand_tracking, true);
		config.save();
	}

	// The microphone streams by default, like Virtual Desktop; this asks for the permission once
	const auto mic_stamp = application::get_config_path() / microphone_stamp;
	if (not std::filesystem::exists(mic_stamp))
	{
		std::ofstream{mic_stamp};
		config.set_feature(feature::microphone, true);
	}

	// Hands work out of the box too, in the lobby and in the dashboard
	const auto hand_stamp = application::get_config_path() / hand_tracking_stamp;
	if (system.hand_tracking_supported() and not std::filesystem::exists(hand_stamp))
	{
		std::ofstream{hand_stamp};
		config.set_feature(feature::hand_tracking, true);
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

void scenes::lobby::lvrr_remember_mac(const std::optional<std::array<uint8_t, 6>> & mac)
{
	if (not mac or not lvrr_server)
		return;

	auto cookie = lvrr_server->service.txt.find("cookie");
	if (cookie == lvrr_server->service.txt.end())
		return;

	auto & config = application::get_config();
	auto server = config.servers.find(cookie->second);
	if (server == config.servers.end() or server->second.mac == mac)
		return;

	server->second.mac = mac;
	config.save();
}

void scenes::lobby::lvrr_wake(const configuration::server_data & server)
{
	if (not server.mac)
		return;

	// magic packet: 6 times 0xff, then the MAC 16 times
	std::array<uint8_t, 102> packet;
	std::fill_n(packet.begin(), 6, 0xff);
	for (int i = 0; i < 16; i++)
		std::ranges::copy(*server.mac, packet.begin() + 6 + i * 6);

	const int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	if (fd < 0)
	{
		spdlog::warn("Cannot wake {}: {}", server.service.name, strerror(errno));
		return;
	}
	const int yes = 1;
	setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &yes, sizeof(yes));

	// the limited broadcast only leaves through the default route, also send to each network's own
	std::vector<in_addr> targets{{htonl(INADDR_BROADCAST)}};
	if (ifaddrs * addrs; getifaddrs(&addrs) == 0)
	{
		for (auto i = addrs; i; i = i->ifa_next)
			if (i->ifa_broadaddr and i->ifa_addr and i->ifa_addr->sa_family == AF_INET and i->ifa_flags & IFF_BROADCAST and not(i->ifa_flags & IFF_LOOPBACK))
				targets.push_back(((sockaddr_in *)i->ifa_broadaddr)->sin_addr);
		freeifaddrs(addrs);
	}

	int sent = 0;
	for (const auto & target: targets)
	{
		sockaddr_in to{.sin_family = AF_INET, .sin_port = htons(9), .sin_addr = target};
		if (sendto(fd, packet.data(), packet.size(), 0, (sockaddr *)&to, sizeof(to)) == ssize_t(packet.size()))
			sent++;
	}
	close(fd);
	spdlog::info("Sent {} wake-on-LAN packets to {}", sent, server.service.name);
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

std::string scenes::lobby::lvrr_waiting_title() const
{
	// connections from an intent (USB) have no server name
	if (async_error)
		return server_name.empty() ? _("Disconnected") : fmt::format(_F("Disconnected from {}"), server_name);
	return server_name.empty() ? _("Starting LVRR") : fmt::format(_F("Starting LVRR on {}"), server_name);
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

	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {10, 10});
	ImGui::PushStyleColor(ImGuiCol_ChildBg, ImVec4{0, 0, 0, 0});
	ImGui::BeginChild("waiting", {0, 0}, ImGuiChildFlags_AlwaysUseWindowPadding);

	const float header_top = ImGui::GetCursorPosY();
	// the title is in the shell's title pill, only the status goes here
	ImGui::PushStyleColor(ImGuiCol_Text, ui::current().text_muted);
	ImGui::PushTextWrapPos(ImGui::GetContentRegionAvail().x * 0.6f);
	if (async_error)
		ImGui::TextUnformatted(async_error->c_str());
	else if (next_scene)
		ImGui::TextUnformatted(_S("The desktop appears here in a moment. If it does not, restart it."));
	else
		ImGui::TextUnformatted(_S("Reconnecting to the PC."));
	ImGui::PopTextWrapPos();
	ImGui::PopStyleColor();

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

	if (not async_error)
	{
		const float r = ui::metrics::app_spinner_radius;
		ImGui::SetCursorPos(ImGui::GetWindowSize() / 2 - ImVec2{r, r});
		ImSpinner::SpinnerAng("waiting", r, ui::metrics::app_spinner_thickness, ImColor{ui::current().accent}, ImColor{ui::current().control}, 6, 0.75f * 2 * M_PI);
	}

	ImGui::EndChild();
	ImGui::PopStyleColor();
	ImGui::PopStyleVar();
}

// The dashboard's layout (wayvr dashboard.xml): a pill of round buttons on the left, the
// page title in a pill on top with a close button, the page in a panel, a bar below
void scenes::lobby::lvrr_gui_shell(XrTime predicted_display_time)
{
	namespace ui = wivrn::ui;
	using namespace ui::metrics;

	const ImVec2 win{1400, 900};
	ImGui::PushStyleVar(ImGuiStyleVar_WindowPadding, {0, 0});
	ImGui::SetNextWindowSize(win);
	ImGui::Begin("WiVRn", nullptr, ImGuiWindowFlags_NoTitleBar | ImGuiWindowFlags_NoResize | ImGuiWindowFlags_NoMove | ImGuiWindowFlags_NoScrollbar | ImGuiWindowFlags_NoBackground);

	const float col_x = shell_pad + shell_side_width + shell_gap;
	const float col_w = win.x - col_x - shell_pad;
	const float content_y = shell_pad + shell_top_height + shell_gap;
	const float content_h = win.y - shell_pad - shell_bottom_height - shell_gap - content_y;

	// which page: the waiting page is the Computers one while a PC starts
	const bool on_computers = current_tab == tab::server_list or current_tab == tab::connected;

	ImGui::SetCursorPos({shell_pad, shell_pad});
	ui::shell_side_begin(win.y - 2 * shell_pad, 2, 1);
	if (ui::shell_side_button(ICON_FA_COMPUTER, _cS("tab label", "Computers"), on_computers) and not on_computers)
		current_tab = tab::server_list;
	ui::shell_side_separator();
	if (ui::shell_side_button(ICON_FA_SCALE_BALANCED, _cS("tab label", "Licenses"), current_tab == tab::licenses))
		current_tab = tab::licenses;
	ui::shell_side_end();

	const char * icon = ICON_FA_COMPUTER;
	std::string title = _cS("tab label", "Computers");
	switch (current_tab)
	{
		case tab::connected:
			icon = ICON_FA_PLAY;
			title = lvrr_waiting_title();
			break;
		case tab::licenses:
			icon = ICON_FA_SCALE_BALANCED;
			title = _cS("tab label", "Licenses");
			break;
		default:
			break;
	}

	ImGui::SetCursorPos({col_x, shell_pad});
	bool close = ui::shell_top(col_w, icon, title, _cS("tooltip of the close button in the lobby", "Stop LVRR"));

	ImGui::SetCursorPos({col_x, content_y});
	ui::shell_content_begin({col_w, content_h}, shell_content_padding);
	switch (current_tab)
	{
		case tab::connected:
			gui_connected(predicted_display_time);
			break;
		case tab::licenses:
			gui_licenses();
			break;
		case tab::server_list:
			gui_server_list();
			break;
		default:
			current_tab = tab::server_list;
			break;
	}
	if (current_tab != last_current_tab)
	{
		last_current_tab = current_tab;
		ImGui::SetScrollY(0);
	}
	ui::shell_content_end();

	ImGui::SetCursorPos({col_x, win.y - shell_pad - shell_bottom_height});
	const float side = ImGui::GetFrameHeight() * control_height;
	ui::shell_bottom(
	        col_w,
	        [&] {
		        if (auto bat = wivrn::gui::battery_status_indicator(instance.now()))
			        ui::chip(bat->label, bat->style, false, side);
		        else
			        ImGui::Dummy({0, side});
	        },
	        [&] {
		        // the clock, like the dashboard's bar
		        std::time_t now = std::time(nullptr);
		        std::tm tm{};
		        localtime_r(&now, &tm);
		        char clock[16];
		        std::strftime(clock, sizeof(clock), "%H:%M", &tm);
		        ImGui::PushFont(ui::bold_font(), ImGui::GetStyle().FontSizeBase * 1.1f);
		        ImGui::SetCursorPosY(ImGui::GetCursorPosY() + (side - ImGui::GetFontSize()) * 0.5f);
		        ImGui::TextUnformatted(clock);
		        ImGui::PopFont();
	        });

	if (close)
		ImGui::OpenPopup("stop lvrr");
	if (ui::confirm_modal("stop lvrr", _cS("confirmation messagebox title", "Stop LVRR"), _S("Close the headset app? The PC keeps running."), _cS("button", "Stop"), _("Cancel"), true) == 1)
		application::pop_scene();

	ImGui::End();
	ImGui::PopStyleVar(); // ImGuiStyleVar_WindowPadding
}

void scenes::lobby::lvrr_gui_version()
{
	if constexpr (lvrr::stock_ui)
		return;

	ImGui::PushStyleColor(ImGuiCol_Text, wivrn::ui::current().text_muted);
	ImGui::TextUnformatted((std::string("LVRR ") + wivrn::display_version()).c_str());
	ImGui::PopStyleColor();
}
