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
// a raise this soon after a probe means the probe failed: hold longer next time
constexpr XrDuration probe_failed_within = 60 * second;
// a domain only relaxes once this long passed without a missed frame
constexpr XrDuration quiet_before_relax = 5 * second;

constexpr XrDuration miss_window = 1 * second;
constexpr int misses_to_raise = 2;

// fraction of the display period our own frame may take before a domain is raised
constexpr float budget = 0.6f;
constexpr int over_budget_to_raise = 3;

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
} // namespace

power_governor::power_governor(xr::session & session) :
        session(&session)
{
}

void power_governor::apply(domain d, XrPerfSettingsLevelEXT level)
{
	state & s = domains[int(d)];
	if (s.level == level or not session)
		return;
	s.level = level;
	spdlog::info("{} clock level: {}", name(d), name(level));
	session->set_performance_level(xr_domain(d), level);
}

void power_governor::set_pinned_high(bool pinned_)
{
	pinned = pinned_;
	for (auto d: {domain::cpu, domain::gpu})
	{
		domains[int(d)] = {};
		apply(d, pinned ? XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT : XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT);
	}
	misses = 0;
	miss_window_start = 0;
	last_miss = 0;
}

void power_governor::idle()
{
	set_pinned_high(false);
}

void power_governor::raise(domain d, XrTime now, const char * why)
{
	state & s = domains[int(d)];
	if (s.level == XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT)
	{
		// already high, the hold starts over
		s.hold_until = now + s.hold;
		return;
	}

	if (s.last_probe and now - s.last_probe < probe_failed_within)
		s.hold = std::min(s.hold * 2, hold_max);
	else
		s.hold = hold_min;
	s.hold_until = now + s.hold;
	s.over_budget = 0;

	spdlog::info("{} clocks raised: {}, for at least {}s", name(d), why, s.hold / second);
	apply(d, XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT);
}

void power_governor::relax(domain d, XrTime now)
{
	state & s = domains[int(d)];
	s.last_probe = now;
	s.over_budget = 0;
	apply(d, XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT);
}

void power_governor::frame(XrTime now, XrDuration display_period, XrDuration real_period, float cpu_time, float gpu_time)
{
	if (pinned or display_period <= 0)
		return;

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

		if (s.level == XR_PERF_SETTINGS_LEVEL_SUSTAINED_HIGH_EXT and now > s.hold_until and now - last_miss > quiet_before_relax)
			relax(d, now);
	}
}

void power_governor::on_event(const XrEventDataPerfSettingsEXT & event, XrTime now)
{
	spdlog::info("Performance notification: domain {} sub-domain {} level {} -> {}",
	             int(event.domain),
	             int(event.subDomain),
	             int(event.fromLevel),
	             int(event.toLevel));

	if (pinned or event.toLevel == XR_PERF_SETTINGS_NOTIF_LEVEL_NORMAL_EXT)
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
