# LVRR TODO

Reconciled against CHANGES.md on 2026-10-30. Items already implemented per CHANGES
are removed. The 2026-10-30 branch work is merged to main (f598d729) and pushed; CI is
building the APK.

## Verify on the Quest (shipped this session, not yet confirmed on device)

- Blocker 1, screens crash: the LVRR DMA-buf copy is now opt-in
  (`LVRR_COPY_CAPTURED_FRAMES=1`), default off = upstream in-place sampling. Confirm
  screens show without the Xid 69 device-lost crash.
- Blocker 2, Steam runtime: `~/.config/environment.d/lvrr.conf` is written and the var is
  active. Confirm a Steam VR game opens in the headset, not on the monitor.
- Hand grab (#18): grasp thresholds raised to press 0.9. Confirm a pointing hand no longer
  grabs and the dashboard does not reopen on its own; tune if still off.
- Cover options gear (#8): now overlaid in the cover's top-right corner.
- Dropdown dim backdrop (#16): removed. Confirm no stray layer behind dropdowns.
- Lobby close button (#25): reworded to "Close the app"; needs the new APK from CI.
- Also: Library sections and no duplicate covers (#6/#7), no browser entry (#9), codec
  above foveation (#11), body-tracker toggles wrap (#22), passthrough default off (#13),
  advanced-setting descriptions (#21).

## Open: from the 2026-10-30 test session

Headset interaction (do with the device):
- Watch wrist alignment (#4): the watch follows the hand AIM pose (a ray from the palm),
  so it floats and rotates with pointing, not the wrist. Bind a GRIP pose
  (`/input/grip/pose`) per hand, add `OpenXrInputProfile.grip` + normalize/suggest
  handling, a `Pointer.grip_pose` located each frame (OpenVR backend sets it = pose), and a
  `Positioning::FollowHandGrip` (or a grip flag on FollowHand) so only the watch uses grip
  while the pointer ray keeps aim. Touches wlx-common/windowing, both input backends,
  window.rs, openxr_actions.json5. Tune WATCH_POS/WATCH_ROT on the Quest.
- Hand gesture pictures in grab-help (#3): the grab-help shows controller joystick/trigger/B
  pictures, which don't map to hands (hands grab+move, no scroll to adjust distance/size).
  Design hand-specific help (fist = grab/move, pinch = click) with hand SVGs, shown when the
  active profile is hand_interaction_ext.
- Choppy pointers/rays (#5): pose is lerped from wayvr's own loop fps; choppiness points at
  a low render/update rate (power governor or capture stalls). Profile the loop fps first.
- Hand poke of panels (#19): WayVR panels take ray + pinch, not a finger poke like Horizon
  OS / the WiVRn lobby. Needs a poke interaction (poke_ext/pose, near-field touch against
  panel planes). Larger feature.

Server and dashboard:
- Auto audio switch (#14): WiVRn makes a PipeWire sink "wivrn.sink" (desc "LVRR",
  Audio/Sink) per session in server/audio/audio_pipewire.cpp (wivrn_session.cpp:106). Switch
  the PC default to it on connect and restore on disconnect via the "default" metadata
  (default.configured.audio.sink) from the audio pipewire loop, saving/restoring the prior
  value. Gate behind a config option (default on). Verify on the device; don't shell out to
  pactl/wpctl in the shipped server.
- Game-exited-without-connecting message (blocker-2 follow-up): detect in the server when a
  launched app exits while never opening an XR session, and send a new lvrr_packets message
  so the headset shows "the game closed without starting in VR; check the Steam launch
  option / re-login". Needs app tracking in server/main.cpp (forked_children exit) crossed
  with whether an XR session opened, plus a headset-side notice.
- Toasts under the dashboard (#10): place server/feedback toasts beneath the dashboard
  while it is shown, instead of their current spot.
- Autostart list is empty (#12): Settings > Apps to run on startup shows nothing. List the
  Apps tab's entries with a toggle (grey/green outline or mono/colored icon).
- Posture dropdown placement (#17): put standing/seated next to the recenter button with no
  "Posture" label. The recenter button is on the watch (wayvr), so this is a watch control
  wired to the server's headset config, a cross-component change.
- See-through slider live (#20): the passthrough-fade slider should apply while dragging,
  not only on release.
- Default skybox Table Mountain 2 (#13): ship it as the default; needs the .dds asset
  bundled and a default path (skymaps are downloaded at runtime today). Passthrough default
  is already off.
- Lobby layout (#23): drop the separator between the computer list and "Add server"; put
  the list under the heading "Pick a PC running the LVRR server to stream from."
- Lobby bottom bar (#24): the areas left and right of where the bevel starts are a lighter
  shade with no border; match the bar.
- Real remote stop (optional, from #25): a headset action that actually stops the PC
  session (StopSession/StopServer over the connection), beyond the current close-the-app.

## Open: project roadmap (only what is not already in CHANGES)

Stability, verify on this build:
- WiVRn #873: CachyOS RTX 4050 hybrid laptop, periodic flashes of an old frame. Recheck
  after today's build; may be resolved by the blocker-1 fix or the #1153 Monado patch.

Features not yet built:
- Charging status and latency breakdown in lvrr-ui (the desktop window). The VR performance
  overlay already shows the latency breakdown; lvrr-ui does not show either yet.
- Starting bitrate from a UDP burst during the handshake.
- Black edges on fast head turns: render a margin past the FOV for the headset's
  reprojection (distinct from the FOV-shrink sliders already shipped).
- Game compatibility database: JSON keyed by Steam app id with launch options, xrizer
  settings and controller profile.
- Snapdragon GSR upscaling on the headset after decoding (BSD licensed).
- Remote play: UPnP with miniupnpc, connect by address on the headset, a doctor check.
- Video player: libmpv into a WayVR overlay, with SBS, 180 and 360 projection.
- Cinema environments and a hand or desk passthrough cutout.
- WiVRn #540: continuous adaptive bitrate, on top of the UDP-burst starting bitrate. The
  maintainer found bandwidth estimates unreliable.

Research first:
- Spacewarp on the headset from depth and pose, or optical flow on decoded frames.
- Oculus SDK games: ReviveXR in the Proton prefix over wineopenxr. Try one game.
- OpenXR 1.1 and quad views: track upstream Monado.
- Other headsets: Pico needs testing; others need hardware or testers.

Take from upstream when convenient:
- WayVR PR #662: saves every panel setting; fixes screens coming back behind you.
- WayVR PRs #553/#555: put the keyboard, screens and apps in front of you at start.
- WayVR PR #699: a rustls update in the lockfile (TLS fixes). Trivial.
- WiVRn PR #1087: a Vulkan AV1 encoder, only tested on AMD so far. Take once merged.

## Done and verified in CHANGES (reference)

Roadmap items now implemented (see CHANGES.md for the entries): WayVR #626 restart-in-place,
WayVR #628 Menu-button crash, WiVRn #1082 passthrough dark pixels, WiVRn PR #1033
supersampling, WiVRn #1153 swapchain-free fault, WiVRn #1119 Blender on hybrid GPU, WiVRn
#492 app pacer, "Starting game" feedback, WiVRn #491 see-through slider, WiVRn #967 stream-
ended reason, graphics presets (Potato to Rampage, Headset page), FOV sliders, per-game
controller type, AMD encoder quality, Library search/sort/recent, add any app, WiVRn #670
game fps in stats, USB auto-connect, wake-on-LAN, hands+controllers together, virtual
monitors. Headset-side CAS sharpening covers the old "server-side sharpening" item. The
Library browser entry was shipped then removed this session.
