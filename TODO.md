# LVRR TODO

Reconciled against CHANGES.md on 2026-10-31. Items already implemented per CHANGES
are removed. The 2026-10-30 branch work is merged to main (f598d729) and pushed; CI is
building the APK.

## Verify on the Quest (shipped this session, not yet confirmed on device)

- Blocker 1, screens crash: the LVRR DMA-buf copy is now opt-in
  (`copy_captured_frames: true` in `~/.config/wayvr/lvrr.json5`, only on GPUs with a
  second graphics queue), default off = upstream in-place sampling. Confirm screens show
  without the Xid 69 device-lost crash.
- Blocker 2, Steam runtime: `~/.config/environment.d/lvrr.conf` is written and the var is
  active. Confirm a Steam VR game opens in the headset, not on the monitor, and that
  `lvrr doctor`'s Steam game sandbox row, `lvrr steam` and PC > Server's Steam launch
  options agree with it (all three now read the running Steam's environment).
- Hand grab (#18): grasp thresholds raised to press 0.9. Confirm a pointing hand no longer
  grabs and the dashboard does not reopen on its own; tune if still off. A double fist now
  counts only while the hand points at nothing and holds nothing (a quick regrab toggled
  the dashboard): confirm regrabbing a screen toggles nothing and both double fists work.
- Cover options gear (#8): now overlaid in the cover's top-right corner; games not on Auto
  show a short Touch/Index badge with an icon beside it. Confirm both stay inside the cover
  and the badge reads on bright covers.
- Dropdown dim backdrop (#16): off in VR, and the click catcher behind menus is now
  transparent (it was opaque black). Confirm nothing darkens the panels behind dropdowns
  and menus.
- Lobby close button (#25): reworded to "Close the app"; needs the new APK from CI.
- Lobby bottom bar (#24): its rounded ends now get the bar's shadows and shine instead of
  a lighter plain fill; needs the new APK.
- Library launch: it toasts, plays the launch sound and badges the app; a second click
  within 10 s is ignored; a start the server refuses toasts why, drops the badge and can
  be clicked again at once.
- Game titles: Now playing, Running and Stop's confirm show a Library-started OpenVR game's
  title, not hlvr, also when the dashboard was closed right after the click and reopened
  over 3 minutes later. A game launched from All Steam games while a Library start is
  pending shows its own name.
- VR keyboard: clicking the Library search shows it, and after the clear button it keeps
  typing into the box, not the PC's window. Hidden from the watch, it stays hidden on a
  press elsewhere (between covers, on a label); a press on the box shows it.
- PC > Diagnostics > Fix in VR (wayvr runs under lvrr-server.service): the admin password
  prompt shows on the PC's screen and the row updates after it; cancelling the prompt
  toasts "Couldn't apply the fix: the admin password prompt was closed".
- Home's Codec and Resolution tiles show what the session streams (e.g. HEVC 10-bit with
  the codec on Automatic) and keep the running values after a restart-only change until
  Restart session.
- Pairing from the desktop window during a session: one confirm, Getting a PIN..., then the
  PIN. With LVRR stopped, the tray's Pair headset starts it first; Pair headset again while
  a PIN shows keeps that PIN.
- Stats pause: stream with the desktop window on PC > Performance, then open the headset's
  system menu or take it off for 5 s. The graphs stay and go on when you return; the stage
  values, Home's tiles and the VR overlay show "-" meanwhile. Disconnecting still shows
  "Statistics appear while a headset streams."
- Also: Library sections and no duplicate covers (#6/#7), no browser entry (#9), codec
  above foveation (#11), body-tracker toggles wrap (#22, shown only with Body tracking on),
  passthrough default off (#13), advanced-setting descriptions (#21).

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
  a low render/update rate (power governor or capture stalls). Two headset causes are fixed:
  streams ran at the lobby's power savings clocks, and after a stall a stream kept the
  lobby's lowest refresh rate. Recheck, then profile the loop fps if still choppy.
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
- Real remote stop (optional, from #25): a headset action that actually stops the PC
  session (StopSession/StopServer over the connection), beyond the current close-the-app.

## Open: project roadmap (only what is not already in CHANGES)

Stability, verify on this build:
- WiVRn #873: CachyOS RTX 4050 hybrid laptop, periodic flashes of an old frame. Recheck
  after today's build; may be resolved by the blocker-1 fix or the #1153 Monado patch.

Known bugs and limits:
- Monado lvrr-0004 at session end: for apps without a timeline semaphore
  (XR_KHR_vulkan_enable, so Proton games), swapchains destroyed after the last frame are
  freed at once, and work queued after the last xrEndFrame is not waited for. A fence per
  swapchain on each release barrier would cover both.
- A headset the running session refuses on reconnect (codec, bit depth, audio or headset
  features changed) gets a new session, and the game closes with only a server log line.
  The server could send the reason as a HeadsetMessage once the new dashboard is linked.
- WayVR (upstream): a click released while the pointer is untracked or grabbing never
  reaches the clicked panel, as interact_hand (backend/input.rs) returns early. A screen
  keeps the desktop mouse button down until the next click, and a VR keyboard key swallows
  its next press. Sending the release to `interaction.clicked_id` there would fix it.
- WayVR (upstream): a screen whose saved settings (stereo, mouse transform, size) all go
  back to their defaults comes back with the old ones after a restart: persist_layout
  (windowing/manager.rs) skips it and never clears its entry. Clearing must skip
  placeholder backends (pw.rs), which answer None.
- BGR888 screen capture: wlroots with the GLES2 renderer on NVIDIA (seen on 0.20 with an
  RTX 5080) offers only BGR888 shared memory, which neither wlr_screencopy.rs nor
  ext_image_copy_capture.rs takes, so screens and virtual monitors stay blank on
  "ScreenCopy CPU", and on GPU capture once it falls back to shared memory. Needs
  Bgr888/Rgb888 in both (image copy wants a stride that is a multiple of 3) and each row
  expanded to Xbgr8888/Xrgb8888 before upload, as fourcc_to_vk has no 24-bit format.
- KWin before 6.8 offers its screencast to wayvr only if io.github.lvrr.wayvr.desktop is
  found. When it isn't, only VR says so; `lvrr doctor` (and so PC > Diagnostics) could
  check the entry and its Exec, and fix it by linking it into ~/.local/share/applications.
- Virtual monitors: Home lists one still being restored like a working one; only the toast
  at start says it is retried. After removing a lower-numbered one, the rest are renamed at
  the next start ([LVRR-1, LVRR-3] come back as LVRR-1 and LVRR-2), and the renamed one
  loses its placement and settings. Saving the names in lvrr.json5 would fix it.
  Compositor detection still trusts another login's Hyprland: a launcher can hand Hyprland
  its Wayland socket, so a pid check can't rule it out.
- A Library page restored after a restart can toast "The LVRR server is not running." when
  its first request beats the link's first answer. new() could skip the request while the
  link is down, as update() asks once it is up.
- CI debug APKs get a new signing key each run, so one can't update another, until the repo
  secret APK_DEBUG_KEYSTORE_BASE64 holds a debug keystore in base64. With the local one
  (`base64 -w0 ~/.android/debug.keystore`), local and CI debug builds update each other.
  After setting it, uninstall io.github.lvrr.debug once.

Features not yet built:
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
game fps in stats, headset charging and latency stages in the desktop window, USB
auto-connect, wake-on-LAN, hands+controllers together, virtual monitors. Headset-side CAS
sharpening covers the old "server-side sharpening" item. The Library browser entry was
shipped then removed this session.
