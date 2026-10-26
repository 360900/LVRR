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

#include "lvrr_dbus.h"

#include "application.h"
#include "lvrr_offline.h"
#include "utils/load_icon.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <iostream>
#include <limits>
#include <magic_enum.hpp>
#include <map>
#include <nlohmann/json.hpp>
#include <stdexcept>

using json = nlohmann::json;
using namespace wivrn;

namespace lvrr_dbus
{
namespace
{
WivrnServer * dbus_server = nullptr;
hooks session;

// SetHeadsetSettings calls waiting for the headset's answer, by request id
struct pending_request
{
	GDBusMethodInvocation * invocation;
	guint timeout;
};
std::map<uint32_t, pending_request> pending;
uint32_t next_request_id = 1;
const guint request_timeout_ms = 3000;

// Changes made while the headset was away, sent when it connects: their request id until the
// headset answers, and whether they need a session restart
uint32_t offline_request_id = 0;
bool offline_restart = false;

uint32_t new_request_id()
{
	uint32_t id = next_request_id++;
	if (next_request_id == 0)
		next_request_id = 1;
	return id;
}

// Without a headset, the last one's settings and the changes waiting for it
void publish_offline()
{
	auto view = lvrr_offline::offline_view();
	wivrn_server_set_headset_settings(dbus_server, view.is_null() ? "" : view.dump().c_str());
}

// headset_config fields with a plain JSON value; body_parts is a list of names
#define LVRR_CONFIG_FIELDS(X)     \
	X(preferred_refresh_rate) \
	X(minimum_refresh_rate)   \
	X(fps_divider)            \
	X(resolution_scale)       \
	X(foveation)              \
	X(codec)                  \
	X(bit_depth)              \
	X(bitrate_bps)            \
	X(foveation_override)     \
	X(foveation_pitch)        \
	X(foveation_distance)     \
	X(super_sampling)         \
	X(sharpening)             \
	X(microphone)             \
	X(mic_unprocessed)        \
	X(forward_keyboard)       \
	X(forward_mouse)          \
	X(forward_gamepad)        \
	X(hand_tracking)          \
	X(eye_gaze)               \
	X(face_tracking)          \
	X(body_tracking)          \
	X(posture)                \
	X(height_offset)          \
	X(high_power_mode)        \
	X(passthrough_boundary)   \
	X(see_through)            \
	X(usb_network)            \
	X(extended_config)

// feature_bit values, named like the headset_config keys
const std::pair<lvrr::feature_bit, const char *> feature_names[] = {
        {lvrr::mic, "microphone"},
        {lvrr::hand, "hand_tracking"},
        {lvrr::eye, "eye_gaze"},
        {lvrr::face, "face_tracking"},
        {lvrr::body, "body_tracking"},
};

json features_to_json(uint8_t mask)
{
	json out = json::array();
	for (auto [bit, name]: feature_names)
		if (mask & bit)
			out.push_back(name);
	return out;
}

json body_parts_to_json(uint32_t mask)
{
	json out = json::array();
	for (auto [bit, name]: magic_enum::enum_entries<from_headset::body_part_mask>())
		if (mask & std::to_underlying(bit))
			out.push_back(name);
	return out;
}

uint32_t body_parts_from_json(const json & value)
{
	if (not value.is_array())
		throw std::invalid_argument("body_parts: expected an array of body part names");
	uint32_t mask = 0;
	for (const auto & name: value)
	{
		auto bit = name.is_string() ? magic_enum::enum_cast<from_headset::body_part_mask>(name.get<std::string>()) : std::nullopt;
		if (not bit or *bit == from_headset::body_part_mask::max)
			throw std::invalid_argument("body_parts: unknown body part " + name.dump());
		mask |= std::to_underlying(*bit);
	}
	return mask;
}

template <typename T>
json value_to_json(T value)
{
	if constexpr (std::is_enum_v<T>)
		return magic_enum::enum_name(value);
	else
		return value;
}

template <typename T>
T value_from_json(const json & value, const char * key)
{
	auto invalid = [&](const char * expected) {
		return std::invalid_argument(std::string(key) + ": expected " + expected + ", got " + value.dump());
	};
	if constexpr (std::is_same_v<T, bool>)
	{
		if (not value.is_boolean())
			throw invalid("a boolean");
		return value.get<bool>();
	}
	else if constexpr (std::is_floating_point_v<T>)
	{
		if (not value.is_number() or not std::isfinite(value.get<double>()))
			throw invalid("a number");
		return T(value.get<double>());
	}
	else if constexpr (std::is_enum_v<T>)
	{
		auto v = value.is_string() ? magic_enum::enum_cast<T>(value.get<std::string>()) : std::nullopt;
		if (not v)
			throw invalid("one of the documented names");
		return *v;
	}
	else
	{
		if (not value.is_number_unsigned() or value.get<uint64_t>() > std::numeric_limits<T>::max())
			throw invalid("a non-negative integer");
		return T(value.get<uint64_t>());
	}
}

json config_to_json(const lvrr::headset_config & config)
{
	json out = json::object();
#define X(name)          \
	if (config.name) \
		out[#name] = value_to_json(*config.name);
	LVRR_CONFIG_FIELDS(X)
#undef X
	if (config.body_parts)
		out["body_parts"] = body_parts_to_json(*config.body_parts);
	return out;
}

json codecs_to_json(const std::vector<lvrr::codec_choice> & codecs)
{
	json out = json::array();
	for (auto codec: codecs)
		out.push_back(magic_enum::enum_name(codec));
	return out;
}

json capabilities_to_json(const lvrr::headset_capabilities & caps)
{
	return {
	        {"android", caps.android},
	        {"refresh_rates", caps.refresh_rates},
	        {"refresh_rate_control", caps.refresh_rate_control},
	        {"spacewarp", caps.spacewarp},
	        {"eye_size", json::array({caps.eye_width, caps.eye_height})},
	        {"codecs", codecs_to_json(caps.codecs)},
	        {"codecs_10bit", codecs_to_json(caps.codecs_10bit)},
	        {"max_bitrate_bps", caps.max_bitrate_bps},
	        {"max_bitrate_extended_bps", caps.max_bitrate_extended_bps},
	        {"post_processing", caps.post_processing},
	        {"high_power_mode", caps.high_power_mode},
	        {"passthrough_boundary", caps.passthrough_boundary},
	        {"see_through", caps.see_through},
	        {"usb_network", caps.usb_network},
	        {"keyboard_mouse", caps.keyboard_mouse},
	        {"hand_tracking", caps.hand_tracking},
	        {"eye_gaze", caps.eye_gaze},
	        {"face_tracking", caps.face_tracking},
	        {"body_tracking", caps.body_tracking},
	        {"body_parts", body_parts_to_json(caps.body_parts)},
	};
}

void return_error(GDBusMethodInvocation * invocation, const char * name, const std::string & message)
{
	g_dbus_method_invocation_return_dbus_error(invocation, (std::string("io.github.wivrn.Server.") + name).c_str(), message.c_str());
}

// list_applications() scans Steam and desktop files; GetApplicationIcon is called once per app
const std::unordered_map<std::string, application> & applications()
{
	static std::unordered_map<std::string, application> apps;
	static std::chrono::steady_clock::time_point updated;
	auto now = std::chrono::steady_clock::now();
	if (apps.empty() or now - updated > std::chrono::seconds(10))
	{
		apps = list_applications();
		updated = now;
	}
	return apps;
}

gboolean on_set_headset_settings(WivrnServer * skeleton, GDBusMethodInvocation * invocation, const gchar * patch, gpointer)
{
	if (not session.headset_connected() and not lvrr_offline::known())
	{
		return_error(invocation, "NotConnected", "No headset is connected");
		return TRUE;
	}

	json patch_json;
	lvrr::headset_config config;
	try
	{
		patch_json = json::parse(patch);
		config = config_from_json(patch_json);
	}
	catch (std::exception & e)
	{
		return_error(invocation, "InvalidSettings", e.what());
		return TRUE;
	}

	// Kept for the next connection
	if (not session.headset_connected())
	{
		std::string text = lvrr_offline::stage(patch_json).dump();
		wivrn_server_set_headset_settings(dbus_server, text.c_str());
		wivrn_server_complete_set_headset_settings(skeleton, invocation, text.c_str());
		return TRUE;
	}

	uint32_t id = new_request_id();

	guint timeout = g_timeout_add(request_timeout_ms, [](void * data) -> gboolean {
		if (auto it = pending.find(GPOINTER_TO_UINT(data)); it != pending.end())
		{
			return_error(it->second.invocation, "Timeout", "The headset did not answer");
			pending.erase(it);
		}
		return G_SOURCE_REMOVE; }, GUINT_TO_POINTER(id));
	pending[id] = {invocation, timeout};

	session.send_to_session(lvrr::set_headset_settings{.request_id = id, .config = std::move(config)});
	return TRUE;
}

gboolean on_restart_session(WivrnServer * skeleton, GDBusMethodInvocation * invocation, gpointer)
{
	if (session.session_running())
		session.send_to_session(to_monado::restart_session{});
	wivrn_server_complete_restart_session(skeleton, invocation);
	return TRUE;
}

gboolean on_stop_session(WivrnServer * skeleton, GDBusMethodInvocation * invocation, gpointer)
{
	if (session.session_running())
		session.send_to_session(to_monado::stop{});
	wivrn_server_complete_stop_session(skeleton, invocation);
	return TRUE;
}

gboolean on_list_applications(WivrnServer * skeleton, GDBusMethodInvocation * invocation, gpointer)
{
	std::vector<std::tuple<std::string, std::string, std::string>> list;
	for (const auto & [id, app]: applications())
	{
		auto name = app.name.find("");
		list.emplace_back(id, name != app.name.end() ? name->second : id, app.icon_path ? app.icon_path->string() : "");
	}
	std::ranges::sort(list, [](const auto & a, const auto & b) { return std::get<1>(a) < std::get<1>(b); });

	GVariantBuilder builder;
	g_variant_builder_init(&builder, G_VARIANT_TYPE("a(sss)"));
	for (const auto & [id, name, icon]: list)
		g_variant_builder_add(&builder, "(sss)", id.c_str(), name.c_str(), icon.c_str());
	wivrn_server_complete_list_applications(skeleton, invocation, g_variant_builder_end(&builder));
	return TRUE;
}

gboolean on_get_application_icon(WivrnServer * skeleton, GDBusMethodInvocation * invocation, const gchar * id, gpointer)
{
	const auto & apps = applications();
	auto it = apps.find(id);
	if (it == apps.end() or not it->second.icon_path)
	{
		return_error(invocation, "NotFound", std::string("No icon for ") + id);
		return TRUE;
	}
	// Desktop entries may name a theme icon instead, which callers resolve themselves
	if (not it->second.icon_path->is_absolute())
	{
		return_error(invocation, "NotFound", "Icon of " + std::string(id) + " is a theme icon name: " + it->second.icon_path->string());
		return TRUE;
	}

	try
	{
		const auto & icons = load_icon(*it->second.icon_path);
		if (icons.empty())
			throw std::runtime_error("no image in " + it->second.icon_path->string());

		// same choice as the headset's application list
		const auto & largest = std::ranges::max(icons, [](const wivrn::icon & a, const wivrn::icon & b) {
			if (a.bpp != b.bpp)
				return a.bpp < b.bpp;
			return a.width * a.height < b.width * b.height;
		});
		GVariant * png = g_variant_new_fixed_array(G_VARIANT_TYPE_BYTE, largest.png_data.data(), largest.png_data.size(), 1);
		wivrn_server_complete_get_application_icon(skeleton, invocation, png);
	}
	catch (std::exception & e)
	{
		return_error(invocation, "Failed", e.what());
	}
	return TRUE;
}

gboolean on_start_application(WivrnServer * skeleton, GDBusMethodInvocation * invocation, const gchar * id, gpointer)
{
	// Applications are stopped when the session ends, so starting one without a session would be pointless
	if (not session.session_running())
	{
		return_error(invocation, "NoSession", "No session is running");
		return TRUE;
	}
	try
	{
		session.start_application(id);
		wivrn_server_complete_start_application(skeleton, invocation);
	}
	catch (std::exception & e)
	{
		return_error(invocation, "Failed", e.what());
	}
	return TRUE;
}

gboolean on_stop_application(WivrnServer * skeleton, GDBusMethodInvocation * invocation, guint client_id, gpointer)
{
	if (session.session_running())
		session.send_to_session(from_headset::stop_application{.id = client_id});
	wivrn_server_complete_stop_application(skeleton, invocation);
	return TRUE;
}
} // namespace

json to_json(const lvrr::headset_settings & settings)
{
	return {
	        {"request_id", settings.request_id},
	        {"capabilities", capabilities_to_json(settings.capabilities)},
	        {"config", config_to_json(settings.config)},
	        {"restart_required", settings.restart_required},
	        {"server_input_forwarding", settings.server_input_forwarding},
	        {"permission_pending", features_to_json(settings.permission_pending)},
	        {"permission_denied", features_to_json(settings.permission_denied)},
	        {"offline", false},
	        {"pending", nullptr},
	};
}

json to_json(const lvrr::headset_stats & stats)
{
	json decoders = json::array();
	for (const auto & d: stats.decoders)
		decoders.push_back({
		        {"encode_ms", d.encode_ms},
		        {"send_ms", d.send_ms},
		        {"network_ms", d.network_ms},
		        {"decode_ms", d.decode_ms},
		        {"display_ms", d.display_ms},
		        {"total_ms", d.total_ms},
		});
	return {
	        {"cpu_time_ms", stats.cpu_time_ms},
	        {"gpu_time_ms", stats.gpu_time_ms},
	        {"rx_bps", stats.rx_bps},
	        {"tx_bps", stats.tx_bps},
	        {"display_hz", stats.display_hz},
	        {"motion_to_photon_ms", stats.motion_to_photon_ms},
	        {"decoders", decoders},
	};
}

lvrr::headset_config config_from_json(const json & value)
{
	if (not value.is_object())
		throw std::invalid_argument("expected a JSON object");
	lvrr::headset_config config;
	for (const auto & [key, item]: value.items())
	{
#define X(name)                                                                                \
	if (key == #name)                                                                      \
	{                                                                                      \
		config.name = value_from_json<decltype(config.name)::value_type>(item, #name); \
		continue;                                                                      \
	}
		LVRR_CONFIG_FIELDS(X)
#undef X
		if (key == "body_parts")
		{
			config.body_parts = body_parts_from_json(item);
			continue;
		}
		throw std::invalid_argument("unknown setting " + key);
	}
	return config;
}

void setup(WivrnServer * server, hooks h)
{
	dbus_server = server;
	session = std::move(h);

	g_signal_connect(server, "handle-set-headset-settings", G_CALLBACK(on_set_headset_settings), nullptr);
	g_signal_connect(server, "handle-restart-session", G_CALLBACK(on_restart_session), nullptr);
	g_signal_connect(server, "handle-stop-session", G_CALLBACK(on_stop_session), nullptr);
	g_signal_connect(server, "handle-list-applications", G_CALLBACK(on_list_applications), nullptr);
	g_signal_connect(server, "handle-get-application-icon", G_CALLBACK(on_get_application_icon), nullptr);
	g_signal_connect(server, "handle-start-application", G_CALLBACK(on_start_application), nullptr);
	g_signal_connect(server, "handle-stop-application", G_CALLBACK(on_stop_application), nullptr);

	lvrr_offline::load(session.headsets_file);
	publish_offline();
	wivrn_server_set_headset_stats(server, "");
}

void on_headset_settings(const lvrr::headset_settings & settings)
{
	json value = to_json(settings);
	lvrr_offline::remember(value);

	// The changes made while the headset was away were applied
	bool restart = false;
	if (offline_request_id and settings.request_id == offline_request_id)
	{
		offline_request_id = 0;
		lvrr_offline::clear_pending();
		restart = offline_restart and settings.restart_required;
	}

	auto changes = lvrr_offline::pending();
	value["pending"] = changes;
	std::string text = value.dump();
	wivrn_server_set_headset_settings(dbus_server, text.c_str());

	if (auto it = pending.find(settings.request_id); it != pending.end())
	{
		g_source_remove(it->second.timeout);
		wivrn_server_complete_set_headset_settings(dbus_server, it->second.invocation, text.c_str());
		pending.erase(it);
	}

	if (restart)
	{
		std::cerr << "Restarting the session for settings changed while the headset was away" << std::endl;
		session.send_to_session(to_monado::restart_session{});
	}
	else if (not changes.is_null() and not offline_request_id)
	{
		try
		{
			offline_request_id = new_request_id();
			offline_restart = lvrr_offline::needs_restart(changes);
			session.send_to_session(lvrr::set_headset_settings{.request_id = offline_request_id, .config = config_from_json(changes)});
		}
		catch (std::exception & e)
		{
			// saved by an older server
			std::cerr << "Dropping invalid headset settings changes: " << e.what() << std::endl;
			offline_request_id = 0;
			lvrr_offline::clear_pending();
		}
	}
}

void on_headset_stats(const lvrr::headset_stats & stats)
{
	wivrn_server_set_headset_stats(dbus_server, to_json(stats).dump().c_str());
}

void on_server_message(const to_headset::server_message & message)
{
	wivrn_server_emit_headset_message(dbus_server, magic_enum::enum_name(message.kind).data(), message.msg.c_str());
}

void on_headset_disconnected()
{
	for (auto & [id, request]: pending)
	{
		g_source_remove(request.timeout);
		return_error(request.invocation, "NotConnected", "The headset disconnected");
	}
	pending.clear();
	// not answered, sent again next time
	offline_request_id = 0;

	publish_offline();
	wivrn_server_set_headset_stats(dbus_server, "");
}

void on_session_starting(const std::string & public_key)
{
	lvrr_offline::set_headset(public_key);
}

void on_session_ended()
{
	on_headset_disconnected();
}

void on_key_revoked(const std::string & public_key)
{
	lvrr_offline::forget(public_key);
	if (not session.headset_connected())
		publish_offline();
}
} // namespace lvrr_dbus
