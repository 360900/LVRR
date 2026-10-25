/*
 * LVRR: D-Bus API for the LVRR dashboard
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

#pragma once

#include "wivrn_ipc.h"
#include "wivrn_packets.h"
#include "wivrn_server_dbus.h"

#include <functional>
#include <nlohmann/json_fwd.hpp>
#include <string>

// The LVRR parts of the io.github.wivrn.Server interface: headset settings and statistics,
// session restarts and the VR application list. The JSON formats are in docs/dbus.md.
namespace lvrr_dbus
{
struct hooks
{
	std::function<bool()> headset_connected;
	std::function<bool()> session_running;
	std::function<void(to_monado::packets)> send_to_session;
	// Starts an application from the list, or the configured one for an empty id; throws on failure
	std::function<void(const std::string & app_id)> start_application;
};

void setup(WivrnServer *, hooks);

void on_headset_settings(const wivrn::lvrr::headset_settings &);
void on_headset_stats(const wivrn::lvrr::headset_stats &);
void on_server_message(const wivrn::to_headset::server_message &);
void on_headset_disconnected();

// Exposed for tests
nlohmann::json to_json(const wivrn::lvrr::headset_settings &);
nlohmann::json to_json(const wivrn::lvrr::headset_stats &);
// Throws std::invalid_argument on unknown keys or wrong types
wivrn::lvrr::headset_config config_from_json(const nlohmann::json &);
} // namespace lvrr_dbus
