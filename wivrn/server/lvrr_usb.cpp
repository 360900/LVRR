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

#include <algorithm>
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
using std::chrono::steady_clock;

constexpr auto poll_interval = 2s;
// A device that shows up while a headset streams is connected if the session ends or loses its
// headset within this time, so a dropped adb connection comes back but a headset plugged in to
// charge isn't taken over
constexpr auto pending_timeout = 60s;
// An adb server restart takes the tunnel down while the device stays listed with the same id
constexpr auto refresh_interval = 10s;
// The wait before trying a device again, doubled after each failure and reset once an attempt
// ends well, so an am start that keeps failing backs off too
constexpr steady_clock::duration first_backoff = 10s;
constexpr steady_clock::duration max_backoff = 60s;
constexpr const char * packages[] = {"io.github.lvrr", "io.github.lvrr.debug"};

struct device
{
	// "transport_id:N", another one means adb lost the device and found it again
	std::string transport;
	// tells callbacks of an earlier appearance of the serial apart
	unsigned gen;
	steady_clock::time_point seen;
	bool done = false;
	// the adb tunnel is set up, which an adb drop takes down with it
	bool reversed = false;
	// adb runs for this device
	bool busy = false;
	steady_clock::time_point retry_at;
	steady_clock::duration backoff = first_backoff;
	steady_clock::time_point refresh_at;
	// failures logged already, so retries don't repeat them
	std::set<std::string> logged;
};

std::optional<lvrr_usb::server> config;
// tells a start-server that ends after configure() ran again apart
unsigned configured;
std::string adb_path;
guint timer;
bool polling;
// the last poll failed, so a lasting failure is logged once
bool poll_failed;
// Devices on USB adb lists as ready, by serial
std::map<std::string, device> devices;
unsigned last_gen;
// The state of the devices adb can't use, by serial, as the last poll listed them
std::map<std::string, std::string> unusable;
// When each of adb's lines about its server was last logged, so one that comes back every poll
// is logged once a minute
std::map<std::string, steady_clock::time_point> server_lines;

// The output is empty if the program fails, error is what it printed on stderr
using callback = std::function<void(std::optional<std::string> output, std::string error)>;

// stderr without adb's lines about its server, which are logged: it started one, or replaced one
// of another version
std::string error_text(const char * err)
{
	std::string text;
	std::istringstream lines(err ? err : "");
	for (std::string line; std::getline(lines, line);)
	{
		if (line.ends_with('\r'))
			line.pop_back();
		if (line.starts_with("* daemon") or line.find("doesn't match this client") != std::string::npos)
		{
			auto now = steady_clock::now();
			auto [it, added] = server_lines.try_emplace(line, now);
			if (added or now - it->second >= 1min)
			{
				it->second = now;
				std::cerr << "USB: adb: " << line << std::endl;
			}
		}
		else if (not line.empty())
			text += (text.empty() ? "" : " ") + line;
	}
	return text;
}

// Runs a program without blocking the main loop
void run(const std::vector<std::string> & args, callback done)
{
	std::vector<const char *> argv;
	for (const auto & arg: args)
		argv.push_back(arg.c_str());
	argv.push_back(nullptr);

	GError * error = nullptr;
	GSubprocess * process = g_subprocess_newv(argv.data(), GSubprocessFlags(G_SUBPROCESS_FLAGS_STDOUT_PIPE | G_SUBPROCESS_FLAGS_STDERR_PIPE), &error);
	if (not process)
	{
		std::cerr << "USB: cannot run " << args.front() << ": " << error->message << std::endl;
		g_error_free(error);
		done(std::nullopt, "");
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
		        char * err = nullptr;
		        GError * error = nullptr;
		        std::optional<std::string> output;
		        if (g_subprocess_communicate_utf8_finish(process, result, &out, &err, &error) and g_subprocess_get_successful(process))
			        output = out ? out : "";
		        auto text = error ? std::string(error->message) : error_text(err);
		        g_free(out);
		        g_free(err);
		        if (error)
			        g_error_free(error);
		        g_object_unref(process);
		        (*done)(std::move(output), std::move(text));
	        },
	        new callback(std::move(done)));
}

void adb(const std::vector<std::string> & args, callback done)
{
	std::vector<std::string> argv{adb_path};
	argv.insert(argv.end(), args.begin(), args.end());
	run(argv, std::move(done));
}

std::string uri()
{
	auto port = std::to_string(config->port);
	auto pin = config->pin();
	if (pin.empty())
		return "wivrn+tcp://127.0.0.1:" + port;
	return "wivrn+tcp://:" + pin + "@127.0.0.1:" + port;
}

// The device a callback runs for, null when it went away or showed up again since
device * current(const std::string & serial, unsigned gen)
{
	auto it = devices.find(serial);
	if (not config or it == devices.end() or it->second.gen != gen)
		return nullptr;
	return &it->second;
}

// Logs each failure once per appearance
void report(device & d, const std::string & message, const std::string & error)
{
	auto line = error.empty() ? message : message + ": " + error;
	if (d.logged.insert(line).second)
		std::cerr << "USB: " << line << std::endl;
}

// Tries the device again later, waiting longer after each failure
void failed(device & d, const std::string & message, const std::string & error = "")
{
	report(d, message, error);
	d.busy = false;
	d.retry_at = steady_clock::now() + d.backoff;
	d.backoff = std::min(d.backoff * 2, max_backoff);
}

void start_app(const std::string & serial, unsigned gen, const std::string & package)
{
	adb({"-s", serial, "shell", "am", "start", "-a", "android.intent.action.VIEW", "-d", uri(), package}, [serial, gen](auto output, auto error) {
		auto d = current(serial, gen);
		if (not d)
			return;
		if (not output)
		{
			failed(*d, "starting the headset app failed on " + serial, error);
			return;
		}
		d->busy = false;
		d->done = true;
	});
}

void reverse(const std::string & serial, unsigned gen, bool start, const std::string & package)
{
	auto tcp = "tcp:" + std::to_string(config->port);
	adb({"-s", serial, "reverse", tcp, tcp}, [serial, gen, start, package](auto output, auto error) {
		auto d = current(serial, gen);
		if (not d)
			return;
		if (not output)
		{
			failed(*d, "adb reverse failed for " + serial, error);
			return;
		}
		d->reversed = true;
		d->refresh_at = steady_clock::now() + refresh_interval;
		if (not start)
		{
			d->busy = false;
			d->backoff = first_backoff;
			return;
		}
		// The headset app would act on the intent once a session that started meanwhile ends.
		// The device stays pending, like one that showed up during a session.
		if (not config->waiting())
		{
			std::cerr << "USB: a headset connected meanwhile, not starting the app on " << serial << std::endl;
			d->busy = false;
			d->backoff = first_backoff;
			return;
		}
		start_app(serial, gen, package);
	});
}

void find_app(const std::string & serial, unsigned gen, bool start)
{
	adb({"-s", serial, "shell", "pm", "list", "packages", "io.github.lvrr"}, [serial, gen, start](auto output, auto error) {
		auto d = current(serial, gen);
		if (not d)
			return;
		if (not output)
		{
			failed(*d, "checking for LVRR on " + serial + " failed", error);
			return;
		}

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
		{
			failed(*d, "LVRR isn't installed on " + serial + ", install it with `lvrr install APK`");
			return;
		}

		std::cerr << (start ? "USB: connecting the headset " : "USB: restoring the adb tunnel for ") << serial << std::endl;
		reverse(serial, gen, start, *package);
	});
}

// Sets up the adb tunnel, and with start also starts the headset app pointed at it. The device
// is done once the app started, a failed attempt is tried again later.
void connect(const std::string & serial, device & d, bool start)
{
	d.busy = true;
	adb({"-s", serial, "shell", "getprop", "sys.boot_completed"}, [serial, gen = d.gen, start](auto output, auto error) {
		auto d = current(serial, gen);
		if (not d)
			return;
		if (not output)
		{
			failed(*d, "adb shell failed on " + serial, error);
			return;
		}
		// adb lists a booting headset before its package manager runs, try again next poll
		std::string booted;
		std::istringstream(*output) >> booted;
		if (booted != "1")
		{
			d->busy = false;
			return;
		}
		find_app(serial, gen, start);
	});
}

// Sets up the tunnel again, which an adb server restart took down if the device stays listed
// with the same transport id
void refresh(const std::string & serial, device & d)
{
	d.busy = true;
	auto tcp = "tcp:" + std::to_string(config->port);
	adb({"-s", serial, "reverse", tcp, tcp}, [serial, gen = d.gen](auto output, auto error) {
		auto d = current(serial, gen);
		if (not d)
			return;
		d->busy = false;
		d->refresh_at = steady_clock::now() + refresh_interval;
		if (not output)
			report(*d, "adb reverse failed for " + serial, error);
	});
}

void on_devices(std::optional<std::string> output, std::string error)
{
	polling = false;
	if (not config)
		return;
	if (not output)
	{
		if (not poll_failed)
			std::cerr << "USB: adb devices failed" << (error.empty() ? "" : ": " + error) << std::endl;
		poll_failed = true;
		return;
	}
	poll_failed = false;

	// The transport id of the devices that allow debugging, and the state of the others, by serial
	std::map<std::string, std::string> ready;
	std::map<std::string, std::string> others;
	std::istringstream lines(*output);
	std::string line;
	std::getline(lines, line); // "List of devices attached"
	while (std::getline(lines, line))
	{
		// The serial is padded with spaces, details follow the state and transport_id:N is last
		std::istringstream fields(line);
		std::string serial, state, transport;
		fields >> serial >> state;
		for (std::string field; fields >> field;)
			if (field.starts_with("transport_id:"))
				transport = field;
		// adb connect and tcpip list host:port, wireless debugging an mDNS name: the stream would
		// go through adb over Wi-Fi instead of straight to the headset
		if (serial.find(':') != std::string::npos or serial.find("._adb") != std::string::npos)
			continue;
		if (state == "device")
			ready.emplace(serial, transport);
		else if (not state.empty())
			others.emplace(serial, state);
	}

	for (const auto & [serial, state]: others)
	{
		if (auto it = unusable.find(serial); it != unusable.end() and it->second == state)
			continue;
		if (state == "unauthorized")
			std::cerr << "USB: allow USB debugging in the headset " << serial << std::endl;
		// "no permissions (...); see [...]"
		else if (state == "no")
			std::cerr << "USB: adb has no permission to open " << serial << ", install your distribution's udev rules for adb (such as android-udev) and plug it in again" << std::endl;
	}
	unusable = std::move(others);

	auto now = steady_clock::now();
	// Another transport id means the device dropped and came back between two polls
	std::erase_if(devices, [&](const auto & d) {
		auto it = ready.find(d.first);
		return it == ready.end() or it->second != d.second.transport;
	});
	for (const auto & [serial, transport]: ready)
		if (not devices.contains(serial))
			devices.emplace(serial, device{.transport = transport, .gen = ++last_gen, .seen = now});

	bool waiting = config->waiting();
	for (auto & [serial, d]: devices)
	{
		if (d.busy)
			continue;
		if (not d.done and now >= d.retry_at)
		{
			// while a headset streams only the tunnel, the app is started if the session ends or
			// loses its headset within pending_timeout
			if (waiting or not d.reversed)
			{
				connect(serial, d, waiting);
				continue;
			}
			if (now - d.seen > pending_timeout)
				d.done = true;
		}
		if (d.reversed and now >= d.refresh_at)
			refresh(serial, d);
	}
}

gboolean poll(void *)
{
	if (not polling)
	{
		polling = true;
		adb({"devices", "-l"}, on_devices);
	}
	return G_SOURCE_CONTINUE;
}

void start_polling()
{
	timer = g_timeout_add_seconds(std::chrono::seconds(poll_interval).count(), poll, nullptr);
	poll(nullptr);
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
	unusable.clear();
	poll_failed = false;
	++configured;

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

	// adb starts its server on first use, which in LVRR's service would stop with it, cutting off
	// everything else using adb
	char * systemd_run = g_find_program_in_path("systemd-run");
	if (not systemd_run)
	{
		start_polling();
		return;
	}
	std::vector<std::string> argv{systemd_run, "--user", "--scope", "--quiet", "--collect", "--", adb_path, "start-server"};
	g_free(systemd_run);
	run(argv, [generation = configured](auto output, auto error) {
		if (generation != configured or not config)
			return;
		// the first poll starts adb's server then, in LVRR's service
		if (not output)
			std::cerr << "USB: cannot start adb's server in its own scope" << (error.empty() ? "" : ": " + error) << std::endl;
		start_polling();
	});
}
