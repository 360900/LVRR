/*
 * LVRR: connects headsets plugged in by USB
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

#include "lvrr_usb.h"

#include <chrono>
#include <gio/gio.h>
#include <iostream>
#include <map>
#include <memory>
#include <optional>
#include <ranges>
#include <set>
#include <sstream>
#include <vector>

namespace
{
using namespace std::chrono_literals;

constexpr auto poll_interval = 2s;
// A device that shows up during a session is connected if the session ends within this time,
// so a dropped adb connection comes back but a headset plugged in to charge isn't taken over
constexpr auto pending_timeout = 60s;
constexpr const char * packages[] = {"io.github.lvrr", "io.github.lvrr.debug"};

struct device
{
	std::chrono::steady_clock::time_point seen;
	bool done = false;
	// the adb tunnel is set up, which an adb drop takes down with it
	bool reversed = false;
};

std::optional<lvrr_usb::server> config;
std::string adb_path;
guint timer;
bool polling;
// Devices adb lists as ready, by serial
std::map<std::string, device> devices;

using callback = std::function<void(std::optional<std::string> output)>;

// Runs adb without blocking the main loop, the output is empty if it fails
void adb(const std::vector<std::string> & args, callback done)
{
	std::vector<const char *> argv{adb_path.c_str()};
	for (const auto & arg: args)
		argv.push_back(arg.c_str());
	argv.push_back(nullptr);

	GError * error = nullptr;
	GSubprocess * process = g_subprocess_newv(argv.data(), GSubprocessFlags(G_SUBPROCESS_FLAGS_STDOUT_PIPE | G_SUBPROCESS_FLAGS_STDERR_SILENCE), &error);
	if (not process)
	{
		std::cerr << "USB: cannot run adb: " << error->message << std::endl;
		g_error_free(error);
		done(std::nullopt);
		return;
	}

	g_subprocess_communicate_utf8_async(
	        process,
	        nullptr,
	        nullptr,
	        [](GObject * source, GAsyncResult * result, gpointer data) {
		        std::unique_ptr<callback> done(static_cast<callback *>(data));
		        auto process = G_SUBPROCESS(source);
		        char * out = nullptr;
		        GError * error = nullptr;
		        std::optional<std::string> output;
		        if (g_subprocess_communicate_utf8_finish(process, result, &out, nullptr, &error) and g_subprocess_get_successful(process))
			        output = out ? out : "";
		        g_free(out);
		        if (error)
			        g_error_free(error);
		        g_object_unref(process);
		        (*done)(std::move(output));
	        },
	        new callback(std::move(done)));
}

std::string uri()
{
	auto port = std::to_string(config->port);
	auto pin = config->pin();
	if (pin.empty())
		return "wivrn+tcp://127.0.0.1:" + port;
	return "wivrn+tcp://:" + pin + "@127.0.0.1:" + port;
}

// Sets up the adb tunnel, and with start_app also starts the headset app pointed at it
void connect(const std::string & serial, bool start_app)
{
	adb({"-s", serial, "shell", "pm", "list", "packages", "io.github.lvrr"}, [serial, start_app](auto output) {
		if (not output or not config)
			return;

		std::set<std::string> installed;
		std::istringstream lines(*output);
		for (std::string line; std::getline(lines, line);)
		{
			if (line.ends_with('\r'))
				line.pop_back();
			if (line.starts_with("package:"))
				installed.insert(line.substr(8));
		}
		auto package = std::ranges::find_if(packages, [&](const char * p) { return installed.contains(p); });
		if (package == std::end(packages))
			return;

		std::cerr << (start_app ? "USB: connecting the headset " : "USB: restoring the adb tunnel for ") << serial << std::endl;
		auto tcp = "tcp:" + std::to_string(config->port);
		adb({"-s", serial, "reverse", tcp, tcp}, [serial, start_app, package = std::string(*package)](auto output) {
			if (not output or not config)
			{
				std::cerr << "USB: adb reverse failed for " << serial << std::endl;
				return;
			}
			if (not start_app)
				return;
			adb({"-s", serial, "shell", "am", "start", "-a", "android.intent.action.VIEW", "-d", uri(), package}, [serial](auto output) {
				if (not output)
					std::cerr << "USB: starting the headset app failed on " << serial << std::endl;
			});
		});
	});
}

void on_devices(std::optional<std::string> output)
{
	polling = false;
	if (not output or not config)
		return;

	std::set<std::string> ready;
	std::istringstream lines(*output);
	std::string line;
	std::getline(lines, line); // "List of devices attached"
	while (std::getline(lines, line))
	{
		auto tab = line.find('\t');
		if (tab != std::string::npos and line.substr(tab + 1).starts_with("device"))
			ready.insert(line.substr(0, tab));
	}

	auto now = std::chrono::steady_clock::now();
	std::erase_if(devices, [&](const auto & d) { return not ready.contains(d.first); });
	for (const auto & serial: ready)
		devices.try_emplace(serial, device{.seen = now});

	bool idle = config->idle();
	for (auto & [serial, d]: devices)
	{
		if (d.done)
			continue;
		if (idle)
		{
			d.done = true;
			connect(serial, true);
			continue;
		}
		// during a session only the tunnel, so a headset whose adb dropped can reconnect to it
		if (not d.reversed)
		{
			d.reversed = true;
			connect(serial, false);
		}
		if (now - d.seen > pending_timeout)
			d.done = true;
	}
}

gboolean poll(void *)
{
	if (not polling)
	{
		polling = true;
		adb({"devices"}, on_devices);
	}
	return G_SOURCE_CONTINUE;
}
} // namespace

void lvrr_usb::configure(bool enabled, server s)
{
	// Keeps which devices were connected already when other settings change
	if (enabled and config and config->port == s.port)
		return;

	if (timer)
		g_source_remove(timer);
	timer = 0;
	config.reset();
	devices.clear();

	if (not enabled)
		return;

	char * path = g_find_program_in_path("adb");
	if (not path)
	{
		std::cerr << "USB: usb-auto-connect is on but adb isn't installed" << std::endl;
		return;
	}
	adb_path = path;
	g_free(path);

	config = std::move(s);
	timer = g_timeout_add_seconds(std::chrono::seconds(poll_interval).count(), poll, nullptr);
	poll(nullptr);
}
