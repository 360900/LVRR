/*
 * LVRR: headset settings and statistics for the LVRR dashboard
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

#include "stream.h"

#include "application.h"
#include "battery.h"
#include "configuration.h"
#include "decoder/decoder.h"
#include "hand_model.h"
#include "utils/i18n.h"
#include <algorithm>
#include <cmath>
#include <ranges>
#include <spdlog/spdlog.h>
#include <utility>
#include <openxr/openxr.h>

#ifdef __ANDROID__
#include "android/permissions.h"
#endif

// The LVRR dashboard (WayVR, on the PC) replaces the in-stream settings: it sends
// set_headset_settings, which are applied here like the settings window did, and gets
// headset_settings and headset_stats back.

namespace
{
constexpr uint32_t mbit = 1'000'000;
constexpr uint32_t min_bitrate = 5 * mbit;
// Safe ranges; extended_config raises the resolution limit and lets foveation go down to 0
constexpr float min_resolution_scale = 0.5f;
constexpr float max_resolution_scale = 1.5f;
constexpr float max_resolution_scale_extended = 3.5f;
constexpr float min_foveation = 0.3f;
constexpr float max_foveation = 0.8f;

lvrr::codec_choice to_choice(video_codec codec)
{
	switch (codec)
	{
		case h264:
			return lvrr::codec_choice::h264;
		case h265:
			return lvrr::codec_choice::h265;
		case av1:
			return lvrr::codec_choice::av1;
		case raw:
			break;
	}
	return lvrr::codec_choice::automatic;
}

lvrr::pp_level to_level(XrCompositionLayerSettingsFlagsFB flags)
{
	if (flags & (XR_COMPOSITION_LAYER_SETTINGS_QUALITY_SUPER_SAMPLING_BIT_FB | XR_COMPOSITION_LAYER_SETTINGS_QUALITY_SHARPENING_BIT_FB))
		return lvrr::pp_level::quality;
	if (flags & (XR_COMPOSITION_LAYER_SETTINGS_NORMAL_SUPER_SAMPLING_BIT_FB | XR_COMPOSITION_LAYER_SETTINGS_NORMAL_SHARPENING_BIT_FB))
		return lvrr::pp_level::normal;
	return lvrr::pp_level::off;
}

XrCompositionLayerSettingsFlagsFB to_flags(lvrr::pp_level level, XrCompositionLayerSettingsFlagsFB normal, XrCompositionLayerSettingsFlagsFB quality)
{
	switch (level)
	{
		case lvrr::pp_level::normal:
			return normal;
		case lvrr::pp_level::quality:
			return quality;
		case lvrr::pp_level::off:
			break;
	}
	return 0;
}

const std::pair<feature, lvrr::feature_bit> features[] = {
        {feature::microphone, lvrr::mic},
        {feature::hand_tracking, lvrr::hand},
        {feature::eye_gaze, lvrr::eye},
        {feature::face_tracking, lvrr::face},
        {feature::body_tracking, lvrr::body},
};

lvrr::feature_bit bit_of(feature f)
{
	return std::ranges::find(features, f, &std::pair<feature, lvrr::feature_bit>::first)->second;
}

// Whether turning the feature on shows a permission prompt
bool prompts(feature f)
{
#ifdef __ANDROID__
	return not check_permission(application::get_hmd_traits().permission_name(f));
#else
	return false;
#endif
}

// The connection-time fields, which only apply to a new session
lvrr::headset_config restart_fields(const lvrr::headset_config & c)
{
	return {
	        .resolution_scale = c.resolution_scale,
	        .foveation = c.foveation,
	        .codec = c.codec,
	        .bit_depth = c.bit_depth,
	        .microphone = c.microphone,
	        .mic_unprocessed = c.mic_unprocessed,
	        .hand_tracking = c.hand_tracking,
	        .eye_gaze = c.eye_gaze,
	        .face_tracking = c.face_tracking,
	        .body_tracking = c.body_tracking,
	};
}

template <typename T>
bool contains(const std::vector<T> & range, const T & value)
{
	return std::ranges::find(range, value) != range.end();
}

bool same(const lvrr::headset_config & a, const lvrr::headset_config & b)
{
	return a.resolution_scale == b.resolution_scale and a.foveation == b.foveation and a.codec == b.codec and
	       a.bit_depth == b.bit_depth and a.microphone == b.microphone and a.mic_unprocessed == b.mic_unprocessed and
	       a.hand_tracking == b.hand_tracking and a.eye_gaze == b.eye_gaze and a.face_tracking == b.face_tracking and
	       a.body_tracking == b.body_tracking;
}
} // namespace

void scenes::stream::lvrr_start()
{
	// fixed for the session; before lvrr_report(), which sends it in the capabilities
	lvrr_see_through_supported = composition_layer_color_scale_bias_supported and system.passthrough_supported() != xr::passthrough_type::none;
	lvrr_session_config = restart_fields(lvrr_config());
	const auto & config = application::get_config();
	lvrr_fov_horizontal = std::clamp(config.fov_horizontal, lvrr::min_fov_tangent, 1.f);
	lvrr_fov_vertical = std::clamp(config.fov_vertical, lvrr::min_fov_tangent, 1.f);
	lvrr_simultaneous_hands = config.simultaneous_hands;
	lvrr_report(0);
}

lvrr::headset_capabilities scenes::stream::lvrr_capabilities()
{
	const auto & config = application::get_config();
	lvrr::headset_capabilities caps{
#ifdef __ANDROID__
	        .android = true,
	        .keyboard_mouse = true,
#endif
	};

	if (instance.has_extension(XR_FB_DISPLAY_REFRESH_RATE_EXTENSION_NAME))
		caps.refresh_rates = session.get_refresh_rates();
	caps.refresh_rate_control = caps.refresh_rates.size() > 1;
	caps.spacewarp = caps.refresh_rate_control;

	auto view = application::get_hmd_traits().override_view(system.view_configuration_views(viewconfig)[0]);
	caps.eye_width = view.recommendedImageRectWidth;
	caps.eye_height = view.recommendedImageRectHeight;

	for (const auto & capability: decoder::supported_codecs())
	{
		if (capability.codec == raw)
			continue;
		caps.codecs.push_back(to_choice(capability.codec));
		if (decoder::supports_10bit(capability.codec))
			caps.codecs_10bit.push_back(to_choice(capability.codec));
	}

	caps.max_bitrate_bps = config.max_bitrate(false);
	caps.max_bitrate_extended_bps = config.max_bitrate(true);
	caps.post_processing = application::get_openxr_post_processing_supported();
	caps.high_power_mode = instance.has_extension(XR_EXT_PERFORMANCE_SETTINGS_EXTENSION_NAME);
	caps.passthrough_boundary = session.boundary_visibility_supported();
	caps.see_through = lvrr_see_through_supported;
	caps.simultaneous_hands = session.simultaneous_hands_and_controllers_supported();
#ifdef __ANDROID__
	caps.usb_network = application::get_hmd_traits().usb_net;
#endif
	caps.hand_tracking = system.hand_tracking_supported();
	caps.eye_gaze = application::get_eye_gaze_supported();
	caps.face_tracking = system.face_tracker_supported() != xr::face_tracker_type::none;

	using mask = from_headset::body_part_mask;
	switch (system.body_tracker_supported())
	{
		case xr::body_tracker_type::none:
			break;
		case xr::body_tracker_type::fb:
			// the FB body tracker has no legs
			caps.body_tracking = true;
			caps.body_parts = std::to_underlying(mask::chest) | std::to_underlying(mask::left_elbow) |
			                  std::to_underlying(mask::right_elbow) | std::to_underlying(mask::hip);
			break;
		default:
			caps.body_tracking = true;
			caps.body_parts = (std::to_underlying(mask::right_foot) << 1) - 1;
			break;
	}
	return caps;
}

lvrr::headset_config scenes::stream::lvrr_config()
{
	const auto & config = application::get_config();
	return {
	        .preferred_refresh_rate = config.preferred_refresh_rate,
	        .minimum_refresh_rate = config.minimum_refresh_rate.value_or(0),
	        .fps_divider = config.fps_divider,
	        .resolution_scale = config.resolution_scale,
	        .fov_horizontal = config.fov_horizontal,
	        .fov_vertical = config.fov_vertical,
	        .foveation = 1 - config.get_stream_scale(),
	        .codec = config.codec ? to_choice(*config.codec) : lvrr::codec_choice::automatic,
	        .bit_depth = config.bit_depth,
	        .bitrate_bps = config.bitrate_bps,
	        .foveation_override = config.override_foveation_enable,
	        .foveation_pitch = config.override_foveation_pitch,
	        .foveation_distance = config.override_foveation_distance,
	        .super_sampling = to_level(config.openxr_post_processing.super_sampling),
	        .sharpening = config.sharpness,
	        .microphone = config.check_feature(feature::microphone),
	        .mic_unprocessed = config.mic_unprocessed_audio,
	        .forward_keyboard = config.forward_keyboard,
	        .forward_mouse = config.forward_mouse,
	        .forward_gamepad = config.forward_gamepad,
	        .hand_tracking = config.check_feature(feature::hand_tracking),
	        .eye_gaze = config.check_feature(feature::eye_gaze),
	        .face_tracking = config.check_feature(feature::face_tracking),
	        .body_tracking = config.check_feature(feature::body_tracking),
	        .body_parts = config.body_part_mask,
	        .posture = config.get_posture() == posture::seated ? lvrr::posture_kind::seated : lvrr::posture_kind::standing,
	        .height_offset = config.get_height_offset(),
	        .high_power_mode = config.high_power_mode,
	        .passthrough_boundary = config.passthrough_boundary_enabled,
	        .see_through = config.see_through,
	        .simultaneous_hands = config.simultaneous_hands,
	        .usb_network = config.usb_network,
	        .extended_config = config.extended_config,
	};
}

void scenes::stream::lvrr_report(uint32_t request_id)
{
	auto config = lvrr_config();
	bool restart_required = not same(restart_fields(config), lvrr_session_config);
	try
	{
		network_session->send_control(lvrr::headset_settings{
		        .request_id = request_id,
		        .capabilities = lvrr_capabilities(),
		        .config = std::move(config),
		        .restart_required = restart_required,
		        .server_input_forwarding = hid_forwarding,
		        .permission_pending = lvrr_permission_pending,
		        .permission_denied = lvrr_permission_denied,
		});
	}
	catch (std::exception & e)
	{
		spdlog::warn("Failed to send headset settings: {}", e.what());
	}
}

void scenes::stream::lvrr_send_settings_changed()
{
	const auto & config = application::get_config();
	try
	{
		network_session->send_control(from_headset::settings_changed{
		        .preferred_refresh_rate = config.preferred_refresh_rate,
		        .minimum_refresh_rate = config.minimum_refresh_rate.value_or(0),
		        .fps_divider = config.fps_divider,
		        .bitrate_bps = config.bitrate_bps,
		        .mirror_gamepad = config.forward_gamepad,
		        .enabled_body_parts = config.body_part_mask,
		});
	}
	catch (std::exception & e)
	{
		spdlog::warn("Failed to send settings changes: {}", e.what());
	}
}

bool scenes::stream::lvrr_apply(const lvrr::headset_config & patch)
{
	auto & config = application::get_config();
	const auto caps = lvrr_capabilities();
	bool live_changed = false; // settings the server must know about

	// first, it changes the allowed ranges below
	if (patch.extended_config)
	{
		config.extended_config = *patch.extended_config;
		// turned off: values past the safe limits would stay in use, hidden by the dashboard's narrower sliders
		if (not config.extended_config)
		{
			config.resolution_scale = std::min(config.resolution_scale, max_resolution_scale);
			// an unset stream scale defaults within the range and stays unset
			if (1 - config.get_stream_scale() < min_foveation)
				config.set_stream_scale(1 - min_foveation);
			if (config.bitrate_bps > config.max_bitrate())
			{
				config.bitrate_bps = config.max_bitrate();
				live_changed = true;
			}
		}
	}

	if (patch.preferred_refresh_rate)
	{
		float rate = *patch.preferred_refresh_rate;
		if (rate == 0)
		{
			config.preferred_refresh_rate = 0;
			config.fps_divider = 1;
			live_changed = true;
		}
		else if (caps.refresh_rate_control and contains(caps.refresh_rates, rate))
		{
			session.set_refresh_rate(rate);
			// video_stream_description keeps the stream's first rate: a new swapchain and
			// on_focused() ask for this one
			lvrr_refresh_rate = config.preferred_refresh_rate = rate;
			live_changed = true;
		}
	}
	if (patch.minimum_refresh_rate)
	{
		float rate = *patch.minimum_refresh_rate;
		config.minimum_refresh_rate = rate == 0 ? std::nullopt : std::optional(rate);
		live_changed = true;
	}
	if (patch.fps_divider and caps.spacewarp and config.preferred_refresh_rate != 0 and (*patch.fps_divider == 1 or *patch.fps_divider == 2))
	{
		config.fps_divider = *patch.fps_divider;
		live_changed = true;
	}

	if (patch.resolution_scale)
		config.resolution_scale = std::clamp(*patch.resolution_scale, min_resolution_scale, config.extended_config ? max_resolution_scale_extended : max_resolution_scale);
	if (patch.fov_horizontal)
		lvrr_fov_horizontal = config.fov_horizontal = std::clamp(*patch.fov_horizontal, lvrr::min_fov_tangent, 1.f);
	if (patch.fov_vertical)
		lvrr_fov_vertical = config.fov_vertical = std::clamp(*patch.fov_vertical, lvrr::min_fov_tangent, 1.f);
	if (patch.foveation)
		config.set_stream_scale(1 - std::clamp(*patch.foveation, config.extended_config ? 0.f : min_foveation, max_foveation));
	if (patch.codec)
	{
		if (*patch.codec == lvrr::codec_choice::automatic)
			config.codec = std::nullopt;
		else if (contains(caps.codecs, *patch.codec))
		{
			for (const auto & capability: decoder::supported_codecs())
				if (capability.codec != raw and to_choice(capability.codec) == *patch.codec)
					config.codec = capability.codec;
		}
	}
	if (patch.bit_depth and (*patch.bit_depth == 8 or *patch.bit_depth == 10))
		config.bit_depth = *patch.bit_depth;
	if (patch.bitrate_bps)
	{
		config.bitrate_bps = std::clamp(*patch.bitrate_bps, min_bitrate, config.max_bitrate());
		live_changed = true;
	}

	if (patch.foveation_override or patch.foveation_pitch or patch.foveation_distance)
	{
		if (patch.foveation_override)
			config.override_foveation_enable = *patch.foveation_override;
		if (patch.foveation_pitch)
			config.override_foveation_pitch = std::clamp(*patch.foveation_pitch, float(-M_PI_2), float(M_PI_2));
		if (patch.foveation_distance)
			config.override_foveation_distance = std::clamp(*patch.foveation_distance, 0.2f, 20.f);
		foveation_center.set_manual_mode({
		        .enabled = config.override_foveation_enable,
		        .pitch = config.override_foveation_pitch,
		        .distance = config.override_foveation_distance,
		});
	}

	if (patch.super_sampling and caps.post_processing)
		config.openxr_post_processing.super_sampling = to_flags(*patch.super_sampling, XR_COMPOSITION_LAYER_SETTINGS_NORMAL_SUPER_SAMPLING_BIT_FB, XR_COMPOSITION_LAYER_SETTINGS_QUALITY_SUPER_SAMPLING_BIT_FB);
	if (patch.sharpening)
	{
		config.sharpness = std::clamp(*patch.sharpening, 0.f, 1.f);
		// replaced by our own sharpening, don't stack the runtime's on top
		config.openxr_post_processing.sharpening = 0;
	}

	if (patch.mic_unprocessed)
		config.mic_unprocessed_audio = *patch.mic_unprocessed;
	if (patch.forward_keyboard and caps.keyboard_mouse)
		config.forward_keyboard = *patch.forward_keyboard;
	if (patch.forward_mouse and caps.keyboard_mouse)
		config.forward_mouse = *patch.forward_mouse;
	if (patch.forward_gamepad)
	{
		config.forward_gamepad = *patch.forward_gamepad;
		live_changed = true;
	}
	if (patch.body_parts)
	{
		config.body_part_mask = *patch.body_parts & caps.body_parts;
		live_changed = true;
	}

	if (patch.posture)
		config.set_posture(*patch.posture == lvrr::posture_kind::seated ? posture::seated : posture::standing);
	if (patch.height_offset)
		config.set_height_offset(std::clamp(*patch.height_offset, -0.3f, 0.7f));

	if (patch.high_power_mode and caps.high_power_mode)
	{
		config.high_power_mode = *patch.high_power_mode;
		// a stream not shown yet leaves the lobby's clocks to idle(); on_focused() applies
		// the saved setting when it shows
		if (focused)
			application::get_power().set_pinned_high(config.high_power_mode);
	}
	if (patch.passthrough_boundary and caps.passthrough_boundary)
	{
		config.passthrough_boundary_enabled = *patch.passthrough_boundary;
		session.set_passthrough_boundary_enabled(config.passthrough_boundary_enabled);
	}
	if (patch.see_through and caps.see_through)
		config.see_through = std::clamp(*patch.see_through, 0.f, lvrr::max_see_through);
	if (patch.simultaneous_hands and caps.simultaneous_hands)
		lvrr_simultaneous_hands = config.simultaneous_hands = *patch.simultaneous_hands;
#ifdef __ANDROID__
	if (patch.usb_network and caps.usb_network)
	{
		config.usb_network = *patch.usb_network;
		application::instance().set_usb_networking(config.usb_network);
	}
#endif

	// Features may need a permission, asked on the headset; the answer comes back later
	auto features_patch = std::array{
	        std::pair{feature::microphone, patch.microphone},
	        std::pair{feature::hand_tracking, caps.hand_tracking ? patch.hand_tracking : std::nullopt},
	        std::pair{feature::eye_gaze, caps.eye_gaze ? patch.eye_gaze : std::nullopt},
	        std::pair{feature::face_tracking, caps.face_tracking ? patch.face_tracking : std::nullopt},
	        std::pair{feature::body_tracking, caps.body_tracking ? patch.body_tracking : std::nullopt},
	};
	for (auto [f, value]: features_patch)
	{
		if (not value)
			continue;
		uint8_t bit = bit_of(f);
		// this value replaces a prompt still waiting for the session
		if (lvrr_prompts_waiting & bit)
		{
			lvrr_prompts_waiting &= uint8_t(~bit);
			lvrr_permission_pending &= uint8_t(~bit);
		}
		if (*value == config.check_feature(f))
			continue;
		lvrr_permission_denied &= uint8_t(~bit);
		if (*value)
			lvrr_permission_pending |= bit;
		// Android may cancel the prompt of an app it doesn't show, which reads as refused: it
		// waits for the session, see lvrr_update()
		if (*value and not application::is_session_running() and prompts(f))
			lvrr_prompts_waiting |= bit;
		else
			lvrr_set_feature(f, *value);
	}

	config.save();
	return live_changed;
}

void scenes::stream::lvrr_set_feature(feature f, bool value)
{
	uint8_t bit = bit_of(f);
	// The stream outlives the request: answers after the stream ended are dropped with it
	std::weak_ptr<stream> weak = weak_from_this();
	application::get_config().set_feature(f, value, [weak, bit, wanted = value](bool enabled) {
		if (auto self = weak.lock())
		{
			self->lvrr_permission_pending &= uint8_t(~bit);
			if (wanted and not enabled)
				self->lvrr_permission_denied |= bit;
			self->lvrr_report_needed = true;
		}
	});
}

void scenes::stream::lvrr_update()
{
	// An ended stream drops its queue: the server fails these calls (Timeout), or keeps their
	// changes when the headset leaves, and sends its offline changes again next time. Applying
	// them would change the lobby's refresh rate or ask for permissions in it. The held prompts
	// drop too, the server keeps their features from permission_pending the same way
	if (state_ == state::shutdown)
	{
		lvrr_requests.lock()->clear();
		lvrr_report_needed = false;
		return;
	}

	// the prompts held while the session was stopped, now that the headset shows the app
	if (lvrr_prompts_waiting and application::is_session_running())
	{
		uint8_t waiting = std::exchange(lvrr_prompts_waiting, 0);
		try
		{
			for (auto [f, bit]: features)
				if (waiting & bit)
					lvrr_set_feature(f, true);
		}
		catch (std::exception & e)
		{
			spdlog::warn("Failed to ask for permissions: {}", e.what());
		}
	}

	std::vector<lvrr::set_headset_settings> requests;
	std::swap(requests, *lvrr_requests.lock());

	for (const auto & request: requests)
	{
		bool live_changed = false;
		try
		{
			live_changed = lvrr_apply(request.config);
		}
		catch (std::exception & e)
		{
			spdlog::warn("Failed to apply headset settings: {}", e.what());
		}
		// Answer first: the server may take a while to reconfigure its encoders
		lvrr_report(request.request_id);
		if (live_changed)
			lvrr_send_settings_changed();
	}

	if (lvrr_report_needed.exchange(false) and requests.empty())
		lvrr_report(0);
}

// Answers the dashboard while the headset is off the head or another app is in front: applying
// works without a running session, and refresh rate and clocks are set again by on_focused()
void scenes::stream::lvrr_idle()
{
	lvrr_update();
}

void scenes::stream::lvrr_stats(XrTime now)
{
	if (now - lvrr_last_stats < 1'000'000'000)
		return;
	lvrr_last_stats = now;

	const auto battery = get_battery_status();
	lvrr::headset_stats stats{
	        .cpu_time_ms = compact_cpu_time * 1000,
	        .gpu_time_ms = compact_gpu_time * 1000,
	        .rx_bps = compact_bandwidth_rx * 8,
	        .tx_bps = compact_bandwidth_tx * 8,
	        // the runtime's period: the last frame interval doubles after a missed frame
	        .display_hz = display_time_period ? 1e9f / float(display_time_period) : 0.f,
	        .motion_to_photon_ms = float(tracking_control.lock()->motions_to_photons) / 1e6f,
	        .battery = battery.charge,
	        .charging = battery.charging,
	};

	// Average the frames measured since the last report
	const int size = global_metrics.size();
	const int count = (metrics_offset - lvrr_stats_offset + size) % size;
	for (const auto & metrics: decoder_metrics)
	{
		lvrr::headset_stats::decoder_timing sum{};
		int n = 0;
		for (int i = 0; i < count and std::cmp_less(i, metrics.size()); ++i)
		{
			const auto & m = metrics[(lvrr_stats_offset + i) % size];
			if (m.displayed <= 0)
				continue;
			sum.encode_ms += (m.encode_end - m.encode_begin) * 1000;
			sum.send_ms += (m.send_end - m.send_begin) * 1000;
			// after send_ms, not overlapping it; send_end comes from the PC's clock through the
			// offset estimate, which can put it after the arrival
			sum.network_ms += std::max(0.f, m.received_last_packet - m.send_end) * 1000;
			sum.decode_ms += (m.received_from_decoder - m.sent_to_decoder) * 1000;
			sum.display_ms += (m.displayed - m.received_from_decoder) * 1000;
			sum.total_ms += (m.displayed - m.encode_begin) * 1000;
			++n;
		}
		if (n)
		{
			for (float * v: {&sum.encode_ms, &sum.send_ms, &sum.network_ms, &sum.decode_ms, &sum.display_ms, &sum.total_ms})
				*v /= n;
		}
		stats.decoders.push_back(sum);
	}
	lvrr_stats_offset = metrics_offset;

	try
	{
		network_session->send_control(std::move(stats));
	}
	catch (std::exception & e)
	{
		spdlog::warn("Failed to send headset statistics: {}", e.what());
	}
}

void scenes::stream::operator()(lvrr::set_headset_settings && request)
{
	// configuration is not thread safe, apply on the render thread
	lvrr_requests.lock()->push_back(std::move(request));
}

void scenes::stream::operator()(lvrr::show_hands && packet)
{
	lvrr_hands_requested = packet.show;
}

// The same hands as the lobby's
void scenes::stream::lvrr_setup_hands()
{
	if (not system.hand_tracking_supported() or not application::get_config().check_feature(feature::hand_tracking))
		return;

	lvrr_left_hand = session.create_hand_tracker(XR_HAND_LEFT_EXT);
	lvrr_right_hand = session.create_hand_tracker(XR_HAND_RIGHT_EXT);

	if (system.hand_mesh_fb_supported())
	{
		const auto * left_mesh = lvrr_left_hand->mesh();
		const auto * right_mesh = lvrr_right_hand->mesh();
		if (left_mesh and right_mesh)
		{
			hand_model::add_hand(*this, XR_HAND_LEFT_EXT, *left_mesh, layer_hands);
			hand_model::add_hand(*this, XR_HAND_RIGHT_EXT, *right_mesh, layer_hands);
			return;
		}
	}
	hand_model::add_hand(*this, XR_HAND_LEFT_EXT, "assets://left-hand.glb", layer_hands);
	hand_model::add_hand(*this, XR_HAND_RIGHT_EXT, "assets://right-hand.glb", layer_hands);
}

// Places the hands; true when one should be drawn
bool scenes::stream::lvrr_locate_hands(XrTime predicted_display_time)
{
	if (not lvrr_left_hand or not lvrr_right_hand)
		return false;

	std::optional<xr::hand_tracker::joint_array> left;
	std::optional<xr::hand_tracker::joint_array> right;
	if (lvrr_hands_requested)
	{
		XrSpace world_space = application::space(xr::spaces::world);
		left = lvrr_left_hand->locate(world_space, predicted_display_time).joints;
		right = lvrr_right_hand->locate(world_space, predicted_display_time).joints;
	}
	hand_model::apply(world, left ? &*left : nullptr, right ? &*right : nullptr);
	return left or right;
}

void scenes::stream::lvrr_render_hands(XrTime predicted_display_time)
{
	XrSpace world_space = application::space(xr::spaces::world);
	auto views = session.locate_views(viewconfig, predicted_display_time, world_space).second;
	render_world(XR_COMPOSITION_LAYER_BLEND_TEXTURE_SOURCE_ALPHA_BIT, world_space, views, width, height, false, layer_hands, {});
}

// Head-locked, full-FOV premultiplied black that dims the room to 1 - opacity, also where the
// stream doesn't reach: the edges a reduced FOV leaves, and those reprojection shows on a fast
// turn. Layer mask 0 draws nothing, render_world only clears, so any size does
bool scenes::stream::lvrr_render_see_through(XrTime predicted_display_time, float opacity)
{
	XrSpace view_space = application::space(xr::spaces::view);
	auto [flags, views] = session.locate_views(viewconfig, predicted_display_time, view_space);
	// like the foveation layer: an invalid pose would make xrEndFrame drop the whole frame
	constexpr XrViewStateFlags valid = XR_VIEW_STATE_POSITION_VALID_BIT | XR_VIEW_STATE_ORIENTATION_VALID_BIT;
	if ((flags & valid) != valid)
		return false;
	render_world(XR_COMPOSITION_LAYER_BLEND_TEXTURE_SOURCE_ALPHA_BIT, view_space, views, 64, 64, false, 0, {0, 0, 0, opacity});
	return true;
}

void scenes::stream::operator()(lvrr::expect_reconnect && packet)
{
	// first: the lobby reads the time once it sees the flag
	lvrr_reconnect_timeout_ms = packet.timeout_ms;
	reconnect_expected_ = true;
}

void scenes::stream::operator()(lvrr::server_stopping && packet)
{
	if (lvrr_end_reported.exchange(true))
		return;

	switch (packet.reason)
	{
		case lvrr::server_stopping::reason_t::stopped:
			stream_error_queue.lock()->emplace(_("LVRR was stopped on the PC."));
			break;
		case lvrr::server_stopping::reason_t::disconnected:
			stream_error_queue.lock()->emplace(_("The PC disconnected the headset."));
			break;
	}
}

// Called by the network and tracking threads, the server sent no reason
void scenes::stream::lvrr_connection_lost(const std::exception & e)
{
	spdlog::info("Connection to the server lost: {}", e.what());
	lvrr_end_stream(_("The connection to the PC ended unexpectedly. If LVRR crashed, run \"lvrr logs\" on the PC for details."));
}

// Called by the network thread after each poll. A link that died without a reset (the PC hung, the
// path dropped) errors only after TCP retransmits for about 15 minutes, or never: with nothing in
// flight, or through adbd. The server sends a timesync query at least every 100 ms all session, also
// over TCP in USB mode. steady_clock stops while Android is suspended, so a headset waking is not cut
void scenes::stream::lvrr_check_alive()
{
	constexpr auto timeout = std::chrono::seconds(15);
	auto now = std::chrono::steady_clock::now();
	if (uint64_t bytes = network_session->bytes_received(); bytes != lvrr_bytes_seen)
	{
		lvrr_bytes_seen = bytes;
		lvrr_bytes_time = now;
		return;
	}

	// armed by the first packet: until then the server may still be starting the session
	if (lvrr_bytes_time == std::chrono::steady_clock::time_point{} or now - lvrr_bytes_time < timeout)
		return;

	lvrr_connection_lost(std::runtime_error(fmt::format("nothing received from the server for {} s", timeout.count())));
}

// Called by the network and tracking threads when the headset itself fails, e.g. its decoder
void scenes::stream::lvrr_headset_error(const std::exception & e)
{
	spdlog::error("Error on the headset, ending the stream: {}", e.what());
	lvrr_end_stream(fmt::format(_F("The stream stopped because of an error on the headset: {}"), e.what()));
}

// Not exit(): an error is not a quit. Shutting the sockets down tells the server, which pauses the
// session and waits five minutes for a reconnect, instead of streaming to a headset that no longer
// reads; threads blocked sending into full buffers (tracking, feedback, the mic) get EPIPE. The
// end is reported and state_ set first, so the errors this causes on other threads find it reported
void scenes::stream::lvrr_end_stream(std::string message)
{
	if (state_ != state::shutdown and not reconnect_expected_ and not lvrr_end_reported.exchange(true))
		stream_error_queue.lock()->emplace(std::move(message));
	state_ = state::shutdown;
	if (network_session)
		network_session->shutdown();
}
