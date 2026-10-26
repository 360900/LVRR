/*
 * LVRR: CPU and GPU clock levels that follow the load
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

#include <array>
#include <openxr/openxr.h>

namespace xr
{
class session;
}

// The headset runs at the sustained low clock level and only raises a domain when it
// shows signs of not keeping up: missed frames, a long frame on our side, or the runtime
// reporting its compositor or our rendering as late. A raised domain drops back after a
// hold, which doubles when the drop was followed by another raise.
class power_governor
{
public:
	enum class domain
	{
		cpu,
		gpu,
	};

	power_governor() = default;
	explicit power_governor(xr::session & session);

	// Both domains pinned at sustained high, the "High power mode" setting
	void set_pinned_high(bool pinned);

	// A menu scene: both domains at sustained low, no history
	void idle();

	// Once per rendered frame. real_period is the time since the previous frame, which
	// exceeds display_period when a frame was missed. cpu_time and gpu_time are the
	// client's own times for the previous frame, in seconds.
	void frame(XrTime now, XrDuration display_period, XrDuration real_period, float cpu_time, float gpu_time);

	void on_event(const XrEventDataPerfSettingsEXT & event, XrTime now);

private:
	struct state
	{
		XrPerfSettingsLevelEXT level = XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT;
		XrTime hold_until = 0;   // not before this does a raised domain try low again
		XrTime last_probe = 0;   // when it last went back to low
		XrDuration hold = 0;     // current hold, grows with every early re-raise
		int over_budget = 0;     // consecutive frames over the time budget
	};

	xr::session * session = nullptr;
	bool pinned = false;
	std::array<state, 2> domains;

	// missed frames in the current window
	XrTime miss_window_start = 0;
	int misses = 0;
	XrTime last_miss = 0;

	void apply(domain d, XrPerfSettingsLevelEXT level);
	void raise(domain d, XrTime now, const char * why);
	void relax(domain d, XrTime now);
};
