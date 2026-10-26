/*
 * LVRR: headset settings and statistics for the dashboard
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

#include "wivrn_session.h"

#include "util/u_logging.h"

namespace wivrn
{
// The session only relays these between the headset and the main process, which serves
// them on D-Bus (server/lvrr_dbus.cpp).

void wivrn_session::operator()(lvrr::headset_settings && settings)
{
	send_to_main(std::move(settings));
}

void wivrn_session::operator()(lvrr::headset_stats && stats)
{
	send_to_main(std::move(stats));
}

// Lets the headset tell a deliberate stop from a crash.
void wivrn_session::send_stopping(lvrr::server_stopping::reason_t reason)
{
	try
	{
		if (connection)
			connection->send_control(lvrr::server_stopping{.reason = reason});
	}
	catch (std::exception & e)
	{
		U_LOG_D("Could not tell the headset the server stops: %s", e.what());
	}
}

void wivrn_session::operator()(lvrr::set_headset_settings && request)
{
	send_control(std::move(request));
}

void wivrn_session::operator()(to_monado::restart_session &&)
{
	U_LOG_I("Restarting the session as requested by main loop");
	send_control(lvrr::expect_reconnect{.timeout_ms = 30'000});
	request_stop();
}
} // namespace wivrn
