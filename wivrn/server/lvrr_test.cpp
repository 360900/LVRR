/*
 * LVRR: tests for the LVRR packets and their D-Bus JSON
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
#include "lvrr_offline.h"
#include "wivrn_serialization.h"

#include <cstring>
#include <filesystem>
#include <iostream>
#include <unistd.h>
#include <nlohmann/json.hpp>

// Run without arguments to test; with --example, prints example D-Bus JSON (see docs/dbus.md).

using json = nlohmann::json;
using namespace wivrn;

namespace
{
int failures = 0;

void check(bool ok, const char * what)
{
	if (not ok)
	{
		std::cerr << "FAIL: " << what << std::endl;
		++failures;
	}
}

template <typename T>
T round_trip(const T & value)
{
	serialization_packet packet;
	packet.serialize(value);
	std::vector<uint8_t> bytes;
	for (auto span: static_cast<std::vector<std::span<uint8_t>> &>(packet))
		bytes.insert(bytes.end(), span.begin(), span.end());

	std::shared_ptr<uint8_t[]> memory(new uint8_t[bytes.size()]);
	std::memcpy(memory.get(), bytes.data(), bytes.size());
	deserialization_packet in(memory, {memory.get(), bytes.size()});
	T out = in.deserialize<T>();
	check(in.empty(), "deserialization reads the whole packet");
	return out;
}

lvrr::headset_settings example_settings()
{
	return {
	        .request_id = 7,
	        .capabilities = {
	                .android = true,
	                .refresh_rates = {72, 90, 120},
	                .refresh_rate_control = true,
	                .spacewarp = true,
	                .eye_width = 1680,
	                .eye_height = 1760,
	                .codecs = {lvrr::codec_choice::h264, lvrr::codec_choice::h265, lvrr::codec_choice::av1},
	                .codecs_10bit = {lvrr::codec_choice::h265, lvrr::codec_choice::av1},
	                .max_bitrate_bps = 200'000'000,
	                .max_bitrate_extended_bps = 800'000'000,
	                .post_processing = true,
	                .high_power_mode = true,
	                .passthrough_boundary = true,
	                .see_through = true,
	                .usb_network = false,
	                .keyboard_mouse = true,
	                .hand_tracking = true,
	                .eye_gaze = false,
	                .face_tracking = false,
	                .body_tracking = true,
	                .body_parts = 0b1111,
	        },
	        .config = {
	                .preferred_refresh_rate = 90,
	                .minimum_refresh_rate = 0,
	                .fps_divider = 1,
	                .resolution_scale = 1.2f,
	                .foveation = 0.5f,
	                .codec = lvrr::codec_choice::automatic,
	                .bit_depth = 10,
	                .bitrate_bps = 50'000'000,
	                .foveation_override = false,
	                .foveation_pitch = -0.17f,
	                .foveation_distance = 3,
	                .super_sampling = lvrr::pp_level::off,
	                .sharpening = lvrr::pp_level::normal,
	                .microphone = false,
	                .mic_unprocessed = false,
	                .forward_keyboard = false,
	                .forward_mouse = false,
	                .forward_gamepad = true,
	                .hand_tracking = true,
	                .eye_gaze = false,
	                .face_tracking = false,
	                .body_tracking = true,
	                .body_parts = 0b1001,
	                .posture = lvrr::posture_kind::standing,
	                .height_offset = 0,
	                .high_power_mode = true,
	                .passthrough_boundary = false,
	                .see_through = 0,
	                .usb_network = false,
	                .extended_config = false,
	        },
	        .restart_required = true,
	        .server_input_forwarding = true,
	        .permission_pending = lvrr::mic,
	        .permission_denied = lvrr::eye,
	};
}

lvrr::headset_stats example_stats()
{
	return {
	        .cpu_time_ms = 4.2f,
	        .gpu_time_ms = 1.5f,
	        .rx_bps = 48e6f,
	        .tx_bps = 0.4e6f,
	        .display_hz = 90,
	        .motion_to_photon_ms = 38,
	        .decoders = {
	                {.encode_ms = 3, .send_ms = 1, .network_ms = 4, .decode_ms = 5, .display_ms = 9, .total_ms = 22},
	                {.encode_ms = 3, .send_ms = 1, .network_ms = 4, .decode_ms = 5, .display_ms = 9, .total_ms = 22},
	        },
	};
}

bool rejects(const char * patch)
{
	try
	{
		lvrr_dbus::config_from_json(json::parse(patch));
		return false;
	}
	catch (std::invalid_argument &)
	{
		return true;
	}
}
} // namespace

int main(int argc, char ** argv)
{
	if (argc > 1 and std::string(argv[1]) == "--example")
	{
		std::cout << json{
		                     {"HeadsetSettings", lvrr_dbus::to_json(example_settings())},
		                     {"HeadsetStats", lvrr_dbus::to_json(example_stats())},
		             }
		                     .dump(2)
		          << std::endl;
		return 0;
	}

	// Packets survive the wire
	auto settings = example_settings();
	check(lvrr_dbus::to_json(round_trip(settings)) == lvrr_dbus::to_json(settings), "headset_settings round trip");
	auto stats = example_stats();
	check(lvrr_dbus::to_json(round_trip(stats)) == lvrr_dbus::to_json(stats), "headset_stats round trip");
	auto request = round_trip(lvrr::set_headset_settings{.request_id = 3, .config = {.bitrate_bps = 30'000'000}});
	check(request.request_id == 3 and request.config.bitrate_bps == 30'000'000u and not request.config.codec, "set_headset_settings round trip");

	// A full config read back from its JSON is the same
	auto config_json = lvrr_dbus::to_json(settings)["config"];
	settings.config = lvrr_dbus::config_from_json(config_json);
	check(lvrr_dbus::to_json(settings)["config"] == config_json, "config JSON round trip");

	// Patches set only what they name
	auto patch = lvrr_dbus::config_from_json(json::parse(R"({"bitrate_bps": 30000000, "codec": "h265", "body_parts": ["chest", "hip"], "posture": "seated"})"));
	check(patch.bitrate_bps == 30'000'000u, "patch bitrate");
	check(patch.codec == lvrr::codec_choice::h265, "patch codec");
	check(patch.body_parts == 0b1001u, "patch body parts");
	check(patch.posture == lvrr::posture_kind::seated, "patch posture");
	check(not patch.resolution_scale and not patch.microphone, "patch leaves other settings unset");

	// Invalid patches are refused
	check(rejects(R"({"nope": 1})"), "unknown key");
	check(rejects(R"({"bitrate_bps": "fast"})"), "bitrate as string");
	check(rejects(R"({"bitrate_bps": -5})"), "negative bitrate");
	check(rejects(R"({"bit_depth": 300})"), "bit depth out of range");
	check(rejects(R"({"codec": "mpeg2"})"), "unknown codec");
	check(rejects(R"({"hand_tracking": 1})"), "number for a boolean");
	check(rejects(R"({"body_parts": ["tail"]})"), "unknown body part");
	check(rejects(R"([1, 2])"), "not an object");

	// Settings kept while the headset is away
	auto store = std::filesystem::temp_directory_path() / ("lvrr-test-" + std::to_string(getpid()) + ".json");
	lvrr_offline::load(store);
	check(not lvrr_offline::known() and lvrr_offline::offline_view().is_null(), "no headset known at first");
	lvrr_offline::set_headset("key-a");
	lvrr_offline::remember(lvrr_dbus::to_json(example_settings()));
	auto changes = json::parse(R"({"bitrate_bps": 30000000, "codec": "h265"})");
	auto view = lvrr_offline::stage(changes);
	check(view["offline"] == true and view["pending"] == changes, "offline view has the changes");
	check(view["config"]["bitrate_bps"] == 30000000 and view["config"]["codec"] == "h265" and view["config"]["preferred_refresh_rate"] == 90, "offline config is the last one with the changes");
	check(view["restart_required"] == false, "offline view never asks for a restart");
	lvrr_offline::load(store);
	check(lvrr_offline::offline_view() == view, "offline settings survive a restart");
	check(lvrr_offline::pending().is_null(), "no changes before the session knows its headset");
	lvrr_offline::set_headset("key-b");
	check(lvrr_offline::pending().is_null(), "another headset has no changes");
	lvrr_offline::set_headset("key-a");
	check(lvrr_offline::pending() == changes, "the headset gets its changes");
	check(lvrr_offline::needs_restart(changes) and not lvrr_offline::needs_restart(json::parse(R"({"bitrate_bps": 1})")), "restart settings are told apart");
	lvrr_offline::clear_pending();
	check(lvrr_offline::pending().is_null() and lvrr_offline::offline_view()["pending"].is_null(), "applied changes are cleared");
	lvrr_offline::forget("key-a");
	check(not lvrr_offline::known(), "an unpaired headset is forgotten");
	std::filesystem::remove(store);
	check(lvrr_dbus::to_json(settings)["offline"] == false and lvrr_dbus::to_json(settings)["pending"].is_null(), "live settings are not offline");

	if (failures)
		return 1;
	std::cout << "lvrr-test: all passed" << std::endl;
	return 0;
}
