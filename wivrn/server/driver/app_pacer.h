/*
 * WiVRn VR streaming
 * Copyright (C) 2025  Patrick Nicolas <patricknicolas@laposte.net>
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

#include "util/u_pacing.h"

#include <mutex>
#include <vector>

namespace wivrn
{

class wivrn_session;
class app_pacer;

class pacing_app_factory : public u_pacing_app_factory
{
	friend class app_pacer;
	std::mutex mutex;
	std::vector<app_pacer *> app_pacers;
	void remove_app(app_pacer *);

public:
	using base_t = u_pacing_app_factory;

	pacing_app_factory();
	xrt_result_t create(bool is_overlay, struct u_pacing_app ** out_upa);
	void destroy();

	int64_t get_frame_time();

	// LVRR: compositor frames that latched the main application's frames since the last
	// call, new ones and repeats of the last, counted per application rather than by commits
	struct game_frames
	{
		uint32_t shown;
		uint32_t repeated;
	};
	game_frames take_game_frames();
	// LVRR: which kinds of applications have a session
	struct running_apps
	{
		bool game;
		bool overlay;
	};
	running_apps running();
};

} // namespace wivrn
