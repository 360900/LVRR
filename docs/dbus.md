# LVRR server D-Bus API

LVRR's server (`wivrn-server`) keeps WiVRn's session bus interface: name `io.github.wivrn.Server`, object `/io/github/wivrn/Server`, interface `io.github.wivrn.Server` (see `wivrn/dbus/io.github.wivrn.Server.xml`). The LVRR dashboard uses these additions. It treats a server without `HeadsetSettings` as another WiVRn server and leaves it alone.

## Headset settings

- `HeadsetSettings` (property, `s`): JSON shown below. Updated when a stream starts, after each change, and when the headset changes something on its own (for example a permission answer). Without a headset, the last headset's settings marked offline (see [Offline](#offline)), or `""` when no headset reported any yet or the last one was unpaired.
- `SetHeadsetSettings(s patch) -> s settings`: `patch` is a JSON object with any `config` keys. It returns the updated `HeadsetSettings` once the headset applied and saved them. Without a headset, or while one connects and hasn't reported its settings yet, it keeps the patch for the next connection and returns the offline settings right away. A change still waiting for the headset when it disconnects is kept the same way.

  Errors:
  - `io.github.wivrn.Server.NotConnected`: no headset is connected, and none is known;
  - `io.github.wivrn.Server.InvalidSettings`: not a JSON object, an unknown key, a wrong type or a name not listed below;
  - `io.github.wivrn.Server.Timeout`: no answer within 3 s. The headset may still apply the change, which `HeadsetSettings` then shows.

  The headset clamps values to the ranges below and ignores settings and values it doesn't support (see `capabilities`). `extended_config` widens the ranges of `resolution_scale`, `foveation` and `bitrate_bps` and applies before the other keys of its patch; turning it off brings their values back into the safe ranges.

`config` keys (all present in `HeadsetSettings`):

| Key | Type | Applies |
|-----|------|---------|
| `preferred_refresh_rate` | one of `capabilities.refresh_rates`, or 0 for automatic | now |
| `minimum_refresh_rate` | number, 0 for none | now |
| `fps_divider` | 1, or 2 for SpaceWarp, which needs `capabilities.spacewarp` and a `preferred_refresh_rate` other than 0 | now |
| `resolution_scale` | number, 0.5 to 1.5, up to 3.5 with `extended_config` | restart |
| `fov_horizontal`, `fov_vertical` | 0.7 to 1, scales the FOV tangents games render | now |
| `foveation` | number, 0.3 to 0.8, 0 to 0.8 with `extended_config` | restart |
| `codec` | `automatic`, or `h264`, `h265`, `av1` from `capabilities.codecs` | restart |
| `bit_depth` | 8 or 10, used only with a `codec` from `capabilities.codecs_10bit` | restart |
| `bitrate_bps` | integer, 5000000 to `capabilities.max_bitrate_bps`, up to `max_bitrate_extended_bps` with `extended_config` | now |
| `foveation_override` | boolean, fixes the foveation center at `foveation_pitch` and `foveation_distance` | now |
| `foveation_pitch` | radians, -π/2 to π/2, negative is down | now |
| `foveation_distance` | meters, 0.2 to 20 | now |
| `super_sampling` | `off`, `normal`, `quality` | now |
| `sharpening` | 0 (off) to 1, sharpens the decoded stream on the headset | now |
| `microphone`, `mic_unprocessed` | boolean | restart |
| `forward_keyboard`, `forward_mouse`, `forward_gamepad` | boolean | now |
| `hand_tracking`, `eye_gaze`, `face_tracking`, `body_tracking` | boolean | restart |
| `body_parts` | array of `chest`, `left_elbow`, `right_elbow`, `hip`, `left_knee`, `right_knee`, `left_foot`, `right_foot` | now |
| `posture` | `standing`, `seated` | now |
| `height_offset` | meters, -0.3 to 0.7, for the current posture | now |
| `high_power_mode`, `passthrough_boundary`, `simultaneous_hands`, `usb_network`, `extended_config` | boolean | now |
| `see_through` | 0 (opaque stream) to 0.8, fades the stream over passthrough | now |

"restart" settings are saved on the headset and apply to the next session: `restart_required` is then true. Call `RestartSession` to apply them. `restart_required` is also true after the headset came back to a running session that started with other "restart" settings. If `codec`, `bit_depth` or `body_tracking` differ, the server restarts the session instead, as `RestartSession` does.

Other `HeadsetSettings` fields:
- `request_id`: the server's id of the change the headset answers, 0 when it reported on its own. Clients can ignore it.
- `capabilities`: what the headset supports. `post_processing` is for `super_sampling`, `keyboard_mouse` for `forward_keyboard` and `forward_mouse`. `refresh_rate_control` is false when the device's own settings pick the refresh rate. `eye_size` is the recommended render size per eye, before `resolution_scale`.
- `server_input_forwarding`: false when the server refuses forwarded keyboard, mouse and gamepad.
- `permission_pending`, `permission_denied`: features, named like the `config` keys, waiting for or refused an Android permission. A feature still waiting when the headset disconnects is kept as a pending change, and the headset asks again when it connects.
- `offline`, `pending`: see below.

### Offline

The server keeps each headset's last settings, by public key, in `lvrr-headsets.json` next to its config file. Unpairing a headset forgets them.

Without a headset, `HeadsetSettings` is the last headset's, with:
- `offline`: true;
- `config`: the last values with the pending changes applied;
- `pending`: the changes waiting for it, as `config` keys with their new values, or null;
- `restart_required`: false.

When that headset connects, the server sends the pending changes. Once the headset applied them, `pending` is null. If they change "restart" settings and the session just started for that headset, the server restarts it once, as `RestartSession` does; a headset that comes back to a running session reports `restart_required` instead, so a running game isn't closed. Another headset connecting leaves them pending.

## Statistics

`HeadsetStats` (property, `s`): JSON updated about once per second while streaming, `""` otherwise or once 3 s pass without an update (the headset sleeps or doesn't render).
- Times are in milliseconds, network rates in bit/s.
- `display_hz` is the headset's display refresh rate.
- `decoders` has one entry per video stream, averaged over the frames shown since the last update: `encode_ms` is encoding on the PC; `send_ms` the PC handing the frame's parts to the network; `network_ms` from the last part sent until the headset has the whole frame; `decode_ms` decoding on the headset; `display_ms` from decoded to shown; `total_ms` from the start of encoding to shown, which also counts the wait between encoding and sending. An encoder that sends slices while it encodes (x264) overlaps `send_ms` with `encode_ms`.
- `battery` is the headset's, from 0 to 1, or null when unknown; `charging` is a boolean.
- Measured on the PC, per second: `game_fps` is the main XR application's new frames shown, null without one; `game_missed_fps` is the compositor frames that repeated its last frame; `encoder_skipped_fps` is the frames the encoders were still too busy for.
- What the session streams with, which changes only with a new session: `codec` is `h264`, `h265` or `av1`, null for the `raw` codec; `bit_depth` is 8 or 10; `render_size` is `[width, height]`, the recommended render size per eye, after `resolution_scale`.

## Session

- `RestartSession()`: ends the session, so restart settings apply. All XR applications close, including the dashboard. The headset reconnects without showing an error, and the server starts the dashboard again. Without a session it does nothing.
- `StopSession()`: ends the session; the headset says the PC disconnected it and returns to its server list. Use it before `EnablePairing`, which returns an empty PIN while a session runs or a headset connects, so wait for `SessionRunning` to turn false.
- `SetDashboardVisible(b visible)`: the dashboard opened or closed. The headset draws the hands over the stream while no game runs, or while the dashboard is open.

## Applications

- `ListApplications() -> a(sss)`: VR applications as (id, name, icon). These are Steam VR apps, and desktop entries with the `X-WiVRn-VR` category. The icon is a file path, or `""` when none was found. Fails with `io.github.wivrn.Server.Failed` when the applications can't be listed.
- `StartApplication(s id)`: starts an application. Under a systemd user session it runs in a transient unit with the user manager's environment and the server's `PATH`, otherwise as a child of the server with its environment. An empty id starts the configured `application`, which is the dashboard. Fails with `io.github.wivrn.Server.NoSession` without a session, and with `io.github.wivrn.Server.Failed` for an unknown id or when the server can't launch it.
- `StopApplication(u client_id)`: asks an OpenXR application to exit. `client_id` is the Monado client id, as libmonado reports it.

## Messages

`HeadsetMessage(s kind, s message)` (signal): server messages to the headset, so the dashboard can show them. `kind` is `toast`, `toast_urgent` or `error`.

## Example

`dbus-example.json` is generated by `target/wivrn/server/lvrr-test --example` (`target/wivrn-dev/server/lvrr-test` after a build without `--release`).
