# LVRR server D-Bus API

LVRR's server (`wivrn-server`) keeps WiVRn's session bus interface: name `io.github.wivrn.Server`, object `/io/github/wivrn/Server`, interface `io.github.wivrn.Server` (see `wivrn/dbus/io.github.wivrn.Server.xml`). The LVRR dashboard uses these additions.

## Headset settings

- `HeadsetSettings` (property, `s`): JSON shown below. Updated when a stream starts, after each change, and when the headset changes something on its own (for example a permission answer). Without a headset, the last headset's settings marked offline (see [Offline](#offline)), or `""` when no headset reported any yet.
- `SetHeadsetSettings(s patch) -> s settings`: `patch` is a JSON object with any `config` keys. It returns the updated `HeadsetSettings` once the headset applied and saved them. Without a headset, it keeps the patch for the next connection and returns the offline settings right away.

  Errors:
  - `io.github.wivrn.Server.NotConnected`: no headset is connected, and none is known;
  - `io.github.wivrn.Server.InvalidSettings`: unknown key or wrong type;
  - `io.github.wivrn.Server.Timeout`: no answer within 3 s.

  The headset clamps values to what it supports and ignores settings it doesn't have.

`config` keys (all present in `HeadsetSettings`):

| Key | Type | Applies |
|-----|------|---------|
| `preferred_refresh_rate` | number, 0 for automatic | now |
| `minimum_refresh_rate` | number, 0 for none | now |
| `fps_divider` | 1, or 2 for SpaceWarp | now |
| `resolution_scale` | number | restart |
| `foveation` | number, 0 to 0.8 | restart |
| `codec` | `automatic`, `h264`, `h265`, `av1` | restart |
| `bit_depth` | 8 or 10 | restart |
| `bitrate_bps` | integer | now |
| `foveation_override` | boolean | now |
| `foveation_pitch` | radians | now |
| `foveation_distance` | meters | now |
| `super_sampling`, `sharpening` | `off`, `normal`, `quality` | now |
| `microphone`, `mic_unprocessed` | boolean | restart |
| `forward_keyboard`, `forward_mouse`, `forward_gamepad` | boolean | now |
| `hand_tracking`, `eye_gaze`, `face_tracking`, `body_tracking` | boolean | restart |
| `body_parts` | array of `chest`, `left_elbow`, `right_elbow`, `hip`, `left_knee`, `right_knee`, `left_foot`, `right_foot` | now |
| `posture` | `standing`, `seated` | now |
| `height_offset` | meters, for the current posture | now |
| `high_power_mode`, `passthrough_boundary`, `usb_network`, `extended_config` | boolean | now |
| `see_through` | 0 (opaque stream) to 0.8, fades the stream over passthrough | now |

"restart" settings are saved on the headset and apply to the next session: `restart_required` is then true. Call `RestartSession` to apply them.

Other `HeadsetSettings` fields:
- `capabilities`: what the headset supports. `refresh_rate_control` is false when the device's own settings pick the refresh rate. `eye_size` is the recommended render size per eye, before `resolution_scale`.
- `server_input_forwarding`: false when the server refuses forwarded keyboard, mouse and gamepad.
- `permission_pending`, `permission_denied`: features, named like the `config` keys, waiting for or refused an Android permission.
- `offline`, `pending`: see below.

### Offline

The server keeps each headset's last settings, by public key, in `lvrr-headsets.json` next to its config file. Unpairing a headset forgets them.

Without a headset, `HeadsetSettings` is the last headset's, with:
- `offline`: true;
- `config`: the last values with the pending changes applied;
- `pending`: the `config` keys changed since, or null;
- `restart_required`: false.

When that headset connects, the server sends the pending changes. Once the headset applied them, `pending` is null. If they include "restart" settings, the server restarts the session once, as `RestartSession` does. Another headset connecting leaves them pending.

## Statistics

`HeadsetStats` (property, `s`): JSON updated about once per second while streaming, `""` otherwise.
- Times are in milliseconds, network rates in bit/s.
- `decoders` has one entry per video stream.

## Session

- `RestartSession()`: ends the session, so restart settings apply. All XR applications close, including the dashboard. The headset reconnects without showing an error, and the server starts the dashboard again.
- `StopSession()`: ends the session and the headset returns to its server list. Use it before `EnablePairing`, which is refused while a session runs.

## Applications

- `ListApplications() -> a(sss)`: VR applications as (id, name, icon). These are Steam VR apps, and desktop entries with the `X-WiVRn-VR` category. The icon is a file path, a theme icon name, or `""`.
- `GetApplicationIcon(s id) -> ay`: PNG data of an application's icon file. Theme icon names return `io.github.wivrn.Server.NotFound`.
- `StartApplication(s id)`: starts an application with the server's environment. An empty id starts the configured `application`, which is the dashboard. Fails with `io.github.wivrn.Server.NoSession` without a session.
- `StopApplication(u client_id)`: asks an OpenXR application to exit. `client_id` is the Monado client id, as libmonado reports it.

## Messages

`HeadsetMessage(s kind, s message)` (signal): server messages to the headset, so the dashboard can show them. `kind` is `toast`, `toast_urgent` or `error`.

## Example

`dbus-example.json` is generated by `target/wivrn/server/lvrr-test --example`.
