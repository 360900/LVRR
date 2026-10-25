/*
 * LVRR additions to the WiVRn protocol
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

#include <cstdint>
#include <optional>
#include <vector>

// Packets used by the LVRR dashboard (WayVR) to show and change headset settings, and to
// read headset statistics. Only LVRR's server and headset app know them, which is fine: the
// protocol hash changes with them, so LVRR and stock WiVRn refuse to talk to each other.
namespace wivrn::lvrr
{
enum class codec_choice : uint8_t
{
	automatic,
	h264,
	h265,
	av1,
};

// XR_FB_composition_layer_settings levels
enum class pp_level : uint8_t
{
	off,
	normal,
	quality,
};

enum class posture_kind : uint8_t
{
	standing,
	seated,
};

enum feature_bit : uint8_t
{
	mic = 1 << 0,
	hand = 1 << 1,
	eye = 1 << 2,
	face = 1 << 3,
	body = 1 << 4,
};

// Headset settings the dashboard can change. In set_headset_settings, absent fields are left
// unchanged; in headset_settings, every field is present.
// "restart" fields only apply to a new session, see headset_settings::restart_required.
struct headset_config
{
	std::optional<float> preferred_refresh_rate; // 0: automatic
	std::optional<float> minimum_refresh_rate;   // 0: none, for automatic
	std::optional<uint32_t> fps_divider;         // 2: application SpaceWarp
	std::optional<float> resolution_scale;       // restart
	std::optional<float> foveation;              // 1 - stream scale, restart
	std::optional<codec_choice> codec;           // restart
	std::optional<uint8_t> bit_depth;            // 8 or 10, restart
	std::optional<uint32_t> bitrate_bps;
	std::optional<bool> foveation_override;
	std::optional<float> foveation_pitch;    // radians
	std::optional<float> foveation_distance; // meters
	std::optional<pp_level> super_sampling;
	std::optional<pp_level> sharpening;
	std::optional<bool> microphone;      // restart
	std::optional<bool> mic_unprocessed; // restart
	std::optional<bool> forward_keyboard;
	std::optional<bool> forward_mouse;
	std::optional<bool> forward_gamepad;
	std::optional<bool> hand_tracking;  // restart
	std::optional<bool> eye_gaze;       // restart
	std::optional<bool> face_tracking;  // restart
	std::optional<bool> body_tracking;  // restart
	std::optional<uint32_t> body_parts; // from_headset::body_part_mask bits
	std::optional<posture_kind> posture;
	std::optional<float> height_offset; // meters, for the current posture
	std::optional<bool> high_power_mode;
	std::optional<bool> passthrough_boundary;
	std::optional<bool> usb_network;
	std::optional<bool> extended_config;
};

// What the headset supports, to show only the settings that apply.
struct headset_capabilities
{
	bool android;
	std::vector<float> refresh_rates;
	bool refresh_rate_control; // false: the device's own settings pick the refresh rate
	bool spacewarp;
	uint16_t eye_width; // recommended render size, before resolution_scale
	uint16_t eye_height;
	std::vector<codec_choice> codecs;
	std::vector<codec_choice> codecs_10bit;
	uint32_t max_bitrate_bps;
	uint32_t max_bitrate_extended_bps;
	bool post_processing;
	bool high_power_mode;
	bool passthrough_boundary;
	bool usb_network;
	bool keyboard_mouse;
	bool hand_tracking;
	bool eye_gaze;
	bool face_tracking;
	bool body_tracking;
	uint32_t body_parts; // body parts the body tracker provides
};

// Sent by the headset when a stream starts, after each set_headset_settings and when
// something changes on its own, such as a permission result.
struct headset_settings
{
	uint32_t request_id; // of the set_headset_settings this answers, 0 otherwise
	headset_capabilities capabilities;
	headset_config config;
	bool restart_required;        // restart fields differ from the running session
	bool server_input_forwarding; // the server accepts forwarded keyboard, mouse and gamepad
	uint8_t permission_pending;   // feature_bit mask
	uint8_t permission_denied;    // feature_bit mask
};

struct set_headset_settings
{
	uint32_t request_id;
	headset_config config;
};

// The server is about to close the connection on purpose; the headset reconnects within
// timeout_ms without showing an error.
struct expect_reconnect
{
	uint32_t timeout_ms;
};

// Sent by the headset about once per second while streaming.
struct headset_stats
{
	float cpu_time_ms;
	float gpu_time_ms;
	float rx_bps;
	float tx_bps;
	float display_hz;
	float motion_to_photon_ms;
	struct decoder_timing
	{
		float encode_ms;
		float send_ms;
		float network_ms;
		float decode_ms;
		float display_ms;
		float total_ms;
	};
	std::vector<decoder_timing> decoders;
};
} // namespace wivrn::lvrr
