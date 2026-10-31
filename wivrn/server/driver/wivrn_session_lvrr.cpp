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

#include "os/os_time.h"
#include "util/u_logging.h"

namespace
{
// How long the headset keeps reconnecting after expect_reconnect: a restart takes seconds, this
// leaves room for applications slow to quit
constexpr uint32_t reconnect_timeout_ms = 60'000;

std::optional<wivrn::lvrr::codec_choice> to_choice(wivrn::video_codec codec)
{
	switch (codec)
	{
		case wivrn::h264:
			return wivrn::lvrr::codec_choice::h264;
		case wivrn::h265:
			return wivrn::lvrr::codec_choice::h265;
		case wivrn::av1:
			return wivrn::lvrr::codec_choice::av1;
		case wivrn::raw:
			break;
	}
	return std::nullopt;
}
} // namespace

namespace wivrn
{
// The session only relays these between the headset and the main process, which serves
// them on D-Bus (server/lvrr_dbus.cpp).

void wivrn_session::operator()(lvrr::headset_settings && settings)
{
	// the headset compares with the start of its own stream, a session it resumed may still
	// run older restart settings (see reconnect)
	settings.restart_required = settings.restart_required or lvrr_outdated;
	send_to_main(std::move(settings));
}

// Adds the PC's frame rates over the same period, and what the session streams with.
void wivrn_session::operator()(lvrr::headset_stats && stats)
{
	const int64_t now = os_monotonic_get_ns();
	const bool first = lvrr_stats_ns == 0;
	// after a stall the queued statistics arrive back to back, too close together to measure:
	// those repeat the last rates and leave the frames to the next period
	if (first or now - lvrr_stats_ns >= 500'000'000)
	{
		const auto game = app_pacers.take_game_frames();
		const uint32_t skipped = compositor.take_skipped_frames();
		const float seconds = (now - lvrr_stats_ns) / 1e9f;
		lvrr_stats_ns = now;

		// the session's, which a headset coming back to it doesn't change (see reconnect)
		const auto & encoder = compositor.get_stream_settings();
		lvrr_pc_stats = {
		        .codec = to_choice(encoder.codec),
		        .bit_depth = uint8_t(encoder.bit_depth),
		        .render_width = get_info().render_eye_width,
		        .render_height = get_info().render_eye_height,
		};
		if (not first)
		{
			if (game.shown + game.repeated > 0)
				lvrr_pc_stats.game_fps = game.shown / seconds;
			lvrr_pc_stats.game_missed_fps = game.repeated / seconds;
			lvrr_pc_stats.encoder_skipped_fps = skipped / seconds;
		}
	}
	send_to_main(lvrr::stats{.headset = std::move(stats), .pc = lvrr_pc_stats});

	// games start and stop without telling the session, this catches them within a second
	lvrr_update_hands();
}

void wivrn_session::operator()(to_monado::dashboard_visible && packet)
{
	lvrr_dashboard_open = packet.visible;
	lvrr_update_hands();
}

// The headset draws the hands while no game draws its own, or while the dashboard is open.
// A dashboard that quit with its window open counts as closed. Runs on the network thread
// only: from the stats and dashboard_visible handlers and from reconnect.
void wivrn_session::lvrr_update_hands()
{
	const auto apps = app_pacers.running();
	const bool show = not apps.game or (lvrr_dashboard_open and apps.overlay);
	if (lvrr_hands_shown == show)
		return;

	try
	{
		send_control(lvrr::show_hands{.show = show});
		lvrr_hands_shown = show;
	}
	catch (std::exception & e)
	{
		U_LOG_D("Could not tell the headset to show the hands: %s", e.what());
	}
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

// Lets the headset reconnect to the restarted session without showing an error. A failed send
// must neither keep the caller from stopping the session nor send run_net into reconnect().
void wivrn_session::send_expect_reconnect()
{
	try
	{
		if (connection)
			connection->send_control(lvrr::expect_reconnect{.timeout_ms = reconnect_timeout_ms});
	}
	catch (std::exception & e)
	{
		U_LOG_D("Could not tell the headset to expect a reconnect: %s", e.what());
	}
}

void wivrn_session::operator()(lvrr::set_headset_settings && request)
{
	send_control(std::move(request));
}

void wivrn_session::operator()(to_monado::restart_session &&)
{
	U_LOG_I("Restarting the session as requested by main loop");
	send_expect_reconnect();
	request_stop();
}
} // namespace wivrn
