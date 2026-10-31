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

#include "power_governor.h"

#include "xr/session.h"
#include <algorithm>
#include <spdlog/spdlog.h>

namespace
{
constexpr XrDuration second = 1'000'000'000;

constexpr XrDuration hold_min = 20 * second;
constexpr XrDuration hold_max = 5 * 60 * second;
// a raise this soon after a probe (a domain going down) means the probe failed: the hold
// doubles, or for a power savings try, the wait before the next one
constexpr XrDuration probe_failed_within = 60 * second;
// a domain only relaxes once this long passed without a missed frame
constexpr XrDuration quiet_before_relax = 5 * second;

constexpr XrDuration miss_window = 1 * second;
constexpr int misses_to_raise = 2;

// fraction of the display period our own frame may take before a domain is raised
constexpr float budget = 0.6f;
constexpr int over_budget_to_raise = 3;

// a stream tries power savings after this long at sustained low without any raise; a
// failed try costs a couple of missed frames, so the wait doubles each time, up to an hour
constexpr XrDuration savings_probe_min = 2 * 60 * second;
constexpr XrDuration savings_probe_max = 60 * 60 * second;

const char * name(power_governor::domain d)
{
	return d == power_governor::domain::cpu ? "CPU" : "GPU";
}

const char * name(XrPerfSettingsLevelEXT level)
{
	switch (level)
	{
		case XR_PERF_SETTINGS_LEVEL_POWER_SAVINGS_EXT:
			return "power savings";
		case XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT:
			return "sustained low";
		case XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT:
			return "sustained high";
		case XR_PERF_SETTINGS_LEVEL_BOOST_EXT:
			return "boost";
		default:
			return "?";
	}
}

XrPerfSettingsDomainEXT xr_domain(power_governor::domain d)
{
	return d == power_governor::domain::cpu ? XR_PERF_SETTINGS_DOMAIN_CPU_EXT : XR_PERF_SETTINGS_DOMAIN_GPU_EXT;
}

XrPerfSettingsLevelEXT one_up(XrPerfSettingsLevelEXT level)
{
	switch (level)
	{
		case XR_PERF_SETTINGS_LEVEL_POWER_SAVINGS_EXT:
			return XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT;
		default:
			return XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT;
	}
}
} // namespace

power_governor::power_governor(xr::session & session) :
        session(session.performance_settings_supported() ? &session : nullptr)
{
}

void power_governor::apply(domain d, XrPerfSettingsLevelEXT level)
{
	domains[int(d)].level = level;
	push(d);
}

// the runtime gets power savings while hidden, s.level is kept for when it shows again
void power_governor::push(domain d)
{
	const XrPerfSettingsLevelEXT level = visible ? domains[int(d)].level : XR_PERF_SETTINGS_LEVEL_POWER_SAVINGS_EXT;
	if (not session or sent[int(d)] == level)
		return;
	sent[int(d)] = level;
	spdlog::info("{} clock level: {}{}", name(d), name(level), visible ? "" : " (not visible)");
	session->set_performance_level(xr_domain(d), level);
}

void power_governor::reset(bool pinned_, bool menu_)
{
	pinned = pinned_;
	menu = menu_;
	domains = {};
	// a reset always reaches the runtime, whatever it was told before
	sent.fill(XR_PERF_SETTINGS_LEVEL_MAX_ENUM_EXT);
	misses = 0;
	miss_window_start = 0;
	last_miss = 0;
	quiet_since = 0;
	for (auto d: {domain::cpu, domain::gpu})
	{
		state & s = domains[int(d)];
		s.floor = menu ? XR_PERF_SETTINGS_LEVEL_POWER_SAVINGS_EXT : XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT;
		apply(d, pinned ? XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT : s.floor);
	}
}

void power_governor::set_pinned_high(bool pinned_)
{
	reset(pinned_, false);
}

void power_governor::idle()
{
	reset(false, true);
}

void power_governor::set_visible(bool visible_)
{
	if (visible == visible_)
		return;
	visible = visible_;
	for (auto d: {domain::cpu, domain::gpu})
		push(d);
}

void power_governor::raise(domain d, XrTime now, const char * why)
{
	state & s = domains[int(d)];
	quiet_since = now;

	// a stream raised from power savings gets sustained low back as its floor. A raise soon
	// after the try means the try failed: sustained low held through the quiet time before
	// it, so go back there and wait twice as long before the next try. A later raise means
	// the load went up: a usual raise, and the wait stays the same
	if (s.level == XR_PERF_SETTINGS_LEVEL_POWER_SAVINGS_EXT and not menu)
	{
		s.floor = XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT;
		if (s.last_probe and now - s.last_probe < probe_failed_within)
		{
			s.savings_backoff = std::clamp(s.savings_backoff * 2, savings_probe_min, savings_probe_max);
			s.over_budget = 0;
			spdlog::info("{} clocks: power savings failed ({}), back to sustained low, next try after {}s", name(d), why, s.savings_backoff / second);
			apply(d, XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT);
			return;
		}
	}

	// a menu only needs the next step, a stream goes straight to high
	const XrPerfSettingsLevelEXT target = menu ? one_up(s.level) : XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT;
	if (s.level == target)
	{
		// already there, the hold starts over
		s.hold_until = now + s.hold;
		return;
	}

	// at least hold_min: a domain only lowered by a power savings try was never given a
	// hold, and twice its 0 is no hold at all
	if (s.last_probe and now - s.last_probe < probe_failed_within)
		s.hold = std::clamp(s.hold * 2, hold_min, hold_max);
	else
		s.hold = hold_min;
	s.hold_until = now + s.hold;
	s.over_budget = 0;

	spdlog::info("{} clocks raised: {}, for at least {}s", name(d), why, s.hold / second);
	apply(d, target);
}

void power_governor::relax(domain d, XrTime now)
{
	state & s = domains[int(d)];
	s.last_probe = now;
	s.over_budget = 0;
	apply(d, s.floor);
}

void power_governor::frame(XrTime now, XrDuration display_period, XrDuration real_period, float cpu_time, float gpu_time)
{
	if (not session or pinned or not visible or display_period <= 0)
		return;
	if (not quiet_since)
		quiet_since = now;

	// missed frames raise both domains, we cannot tell which one was late
	if (real_period > display_period + display_period / 2)
	{
		if (now - miss_window_start > miss_window)
		{
			miss_window_start = now;
			misses = 0;
		}
		last_miss = now;
		if (++misses >= misses_to_raise)
		{
			misses = 0;
			raise(domain::cpu, now, "missed frames");
			raise(domain::gpu, now, "missed frames");
		}
	}

	// our own frame close to the period is an early warning for one domain
	const float period = display_period * 1e-9f;
	for (auto [d, t]: {std::pair{domain::cpu, cpu_time}, std::pair{domain::gpu, gpu_time}})
	{
		state & s = domains[int(d)];
		if (t > budget * period)
		{
			if (++s.over_budget >= over_budget_to_raise)
				raise(d, now, d == domain::cpu ? "long frames on the CPU" : "long frames on the GPU");
		}
		else
			s.over_budget = 0;

		if (s.level != s.floor and now > s.hold_until and now - last_miss > quiet_before_relax)
			relax(d, now);

		// a stream that ran a while at sustained low without trouble tries power savings; the
		// wait counts from when it came down, not from the raise before a long hold
		if (s.savings_backoff == 0)
			s.savings_backoff = savings_probe_min;
		if (not menu and s.level == XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT and s.floor == XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT and now - std::max(quiet_since, s.last_probe) > s.savings_backoff)
		{
			s.floor = XR_PERF_SETTINGS_LEVEL_POWER_SAVINGS_EXT;
			spdlog::info("{} clocks: trying power savings after {}s quiet", name(d), s.savings_backoff / second);
			relax(d, now);
		}
	}
}

void power_governor::on_event(const XrEventDataPerfSettingsEXT & event, XrTime now)
{
	if (not session)
		return;

	spdlog::info("Performance notification: domain {} sub-domain {} level {} -> {}",
	             int(event.domain),
	             int(event.subDomain),
	             int(event.fromLevel),
	             int(event.toLevel));

	if (pinned or not visible or event.toLevel == XR_PERF_SETTINGS_NOTIF_LEVEL_NORMAL_EXT)
		return;

	// thermal warnings are the runtime's to handle, it throttles on its own
	if (event.subDomain != XR_PERF_SETTINGS_SUB_DOMAIN_COMPOSITING_EXT and event.subDomain != XR_PERF_SETTINGS_SUB_DOMAIN_RENDERING_EXT)
		return;

	switch (event.domain)
	{
		case XR_PERF_SETTINGS_DOMAIN_CPU_EXT:
			raise(domain::cpu, now, "runtime notification");
			break;
		case XR_PERF_SETTINGS_DOMAIN_GPU_EXT:
			raise(domain::gpu, now, "runtime notification");
			break;
		default:
			break;
	}
}
