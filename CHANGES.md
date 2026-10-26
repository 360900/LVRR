# Changes

Upstreams merged into LVRR, and LVRR's own changes to them.

## Upstreams

Merged as git subtrees without `--squash`, which keeps upstream history and authors. `.github/scripts/upstream-watch.py` reports unmerged upstream commits every day.

| Name | Repo | Branch | Prefix | Local edits |
|------|------|--------|--------|-------------|
| xrizer | 360900/xrizer-edge | main | xrizer | no |
| wivrn | WiVRn/WiVRn | master | wivrn | yes |
| wayvr | wayvr-org/wayvr | main | wayvr | yes |

`xrizer/` takes no local edits. Changes go to xrizer-edge first, which tracks Supreeeme/xrizer.

Merging an upstream:

```sh
git fetch --no-tags https://github.com/WiVRn/WiVRn.git master
git subtree merge --prefix=wivrn <sha> -m "wivrn: merge WiVRn/WiVRn master at <sha>"
cargo xtask rebrand
```

`cargo xtask rebrand` re-applies the LVRR name and logo. On a merge conflict in a rebranded line, take upstream's side and run it again; `cargo xtask rebrand --check` (also run by CI) reports rules that no longer match.

## LVRR changes

| Component | Change | Files |
|-----------|--------|-------|
| wivrn | app id `io.github.lvrr`, detected by the dashboard over adb | `build.gradle`, `dashboard/adb.cpp` |
| wivrn, wayvr | LVRR name in user-visible strings and translations, LVRR logo | applied by `cargo xtask rebrand`, see `xtask/src/rebrand.rs` |
| wayvr | LVRR assets (`lvrr-assets/`) looked up before upstream ones; language files rebranded on load | `Cargo.toml`, `dash-frontend/src/assets.rs`, `wayvr/src/gui/asset.rs`, new `lvrr-common` crate |
| wivrn | headset settings, statistics and session restarts for the LVRR dashboard: new packets and D-Bus API ([docs/dbus.md](docs/dbus.md)) | `common/wivrn_packets.h`, `dbus/io.github.wivrn.Server.xml`, `server/{CMakeLists.txt,main.cpp,wivrn_ipc.h,accept_connection.cpp}`, `server/driver/{wivrn_connection.cpp,wivrn_session.h}`, `client/{configuration.h,configuration.cpp}`, `client/scenes/{stream.h,stream.cpp,stream_network.cpp,lobby.cpp}`; new `common/lvrr_packets.h`, `server/lvrr_*`, `server/driver/wivrn_session_lvrr.cpp`, `client/scenes/stream_lvrr.cpp` |
| wivrn | a headset the running session refuses gets a new session | `server/driver/wivrn_session.cpp` |
| wivrn | the server listens again only once the old session's applications stopped, and restarts the dashboard when a headset reconnects | `server/main.cpp` |
| wivrn | Monado accepts apps with a no-error EGL context, such as SDL3 apps (Minecraft 26.x with Sodium) | new `patches/monado/0012-c-egl-share-no-error-contexts.patch` |
| wayvr | link to the LVRR server: state, requests, server messages as toasts | `wayvr/{Cargo.toml,src/main.rs,src/state.rs,src/overlays/dashboard.rs}`, `wlx-common/{Cargo.toml,src/dash_interface.rs}`; new `wayvr/src/lvrr/` |
| wayvr | LVRR pages (Home, Library, Running, Headset, PC) replace WiVRn's menus; the side bar and power menu lead to them, Games and Monado open from those pages | `dash-frontend/assets/gui/dashboard.xml`, `dash-frontend/src/{frontend.rs,tab/mod.rs}`; new `dash-frontend/src/{frontend/lvrr_ext.rs,tab/lvrr/}`, `dash-frontend/lvrr-assets/{gui,lang}` |
| wayvr | uidev emulates the LVRR server (`LVRR_EMU`) and opens a page (`LVRR_PAGE`); the emulator runs without a GPU in tests | `wlx-common/src/dash_interface_emulated.rs`, `uidev/src/testbed/testbed_dashboard.rs` |
| wayvr | captured DMA-buf frames are copied once the compositor finished drawing them, before the buffer goes back; fixes mixed frames during motion, mostly when the compositor draws on another GPU | `wgui/src/gfx/{dmabuf.rs,raw.rs}`, `wayvr/src/graphics/dmabuf.rs`, `wayvr/src/overlays/screen/capture.rs`; new `wayvr/src/overlays/screen/capture/lvrr_dmabuf.rs` |
| wayvr | head-locked performance overlay, toggled from PC > Performance | `wayvr/src/overlays/dashboard.rs`; new `wayvr/src/lvrr/perf_overlay.rs`, `wayvr/lvrr-assets/{gui,lang}` |
| wivrn | headset app without WiVRn's settings, environments, wizard and in-stream window: a lobby to find a PC, a "Starting LVRR" screen, a silent reconnect when the server restarts the session, passthrough in the lobby, autoconnect to the first paired PC | `client/scenes/{lobby.h,lobby.cpp,lobby_gui.cpp,stream.cpp,stream_network.cpp}`; new `client/lvrr.h`, `client/scenes/lobby_lvrr.cpp` |
| wivrn | the server refuses a runtime directory too long for its IPC socket instead of overflowing the socket address | `server/main.cpp` |
| wivrn, wayvr | "Restart desktop" on the headset replaces a running dashboard, also a frozen one; `wayvr --replace` kills an instance that ignores SIGTERM for 3 s, and its own Restart replaces the process in place, so it keeps running in the server's systemd unit | `wivrn/server/main.cpp`, `wayvr/wayvr/src/main.rs` |
| wayvr | playspace drag and rotate move nothing until the hand moved 3 cm or turned 5 degrees, and a drag multiplier of 0 moves nothing; each move tells every app that its reference spaces changed, which crashed games when their Menu button was pressed | `wayvr/src/backend/openxr/playspace.rs` |
| wivrn | the server keeps each headset's last settings and the changes made while it is away, sends them when it connects and restarts the session once if they need it ([docs/dbus.md](docs/dbus.md)) | `server/{CMakeLists.txt,main.cpp}`, `server/driver/{wivrn_connection.h,wivrn_connection.cpp}`; new `server/lvrr_offline.{h,cpp}` |
| wivrn | a paired headset's last connection time is read with the offset it was saved with; it shifted by an hour or more with each save | `server/driver/configuration.cpp` |
| wayvr | LVRR desktop window (`lvrr-ui`) with a tray icon: the dashboard's status, pairing, headset and server pages | `Cargo.toml`, `Cargo.lock`, `dash-frontend/assets/gui/dashboard.xml`, `dash-frontend/src/frontend.rs`, `wlx-common/src/dash_interface.rs`; new `lvrr-ui/`, LVRR pages in `dash-frontend/src/{frontend/lvrr_ext.rs,tab/lvrr/}` |
| wivrn, wayvr | the server compositor samples WayVR's panels 4 times per pixel, which stops their text from shimmering; WayVR asks for it with `XR_FB_composition_layer_settings`. WiVRn PR #1033 by caszuu, rebased on LVRR's Monado revision | `wivrn/server/{CMakeLists.txt,compositor/layer_squasher.cpp}`; new `wivrn/patches/monado/{0013-a-shaders-target-spirv-1.3-Vulkan-1.1.patch,0014-a-shaders-add-per-layer-supersampling.patch}`; `wayvr/wayvr/src/backend/openxr/{helpers.rs,overlay.rs}` |
| wivrn | the headset counts nearly opaque pixels as opaque: 8-bit video (H.264, 8-bit HEVC) lands them a little under 1, and the passthrough showed through dark content (WiVRn #1082) | `client/shaders/reprojection.glsl` |
| wivrn | Monado waits for the app's queue before freeing a destroyed swapchain's images: Unity games recreate swapchains on a render scale change with frames in flight, which faulted the GPU (NVIDIA Xid 31) and lost the session (WiVRn #1153) | new `patches/monado/0015-c-vk_client-wait-for-the-queue-before-freeing-swapch.patch` |
| wivrn | an app that can't make the next display slot runs without waiting, however many slots it misses; sleeping until the slot it could make made apps near the frame time swing between half and full rate (WiVRn #492) | `server/driver/app_pacer.cpp` |
| wivrn | native OpenXR OpenGL apps on another GPU than the runtime, such as Blender on a hybrid laptop's integrated GPU, draw into plain textures that Monado copies through host memory into the runtime's images instead of crashing in Mesa's import (WiVRn #1119); the same copy xrizer makes for OpenVR games | new `patches/monado/0016-c-gl_client-copy-frames-through-host-memory-for-apps.patch` |
| wayvr | a dragged slider on the Headset page is neither sent nor rebuilt under the pointer, it glitched between the old and the new value; the "After restart" badge sits next to a slider's label instead of floating beside the bar | `wgui/src/components/slider.rs`, `dash-frontend/src/tab/lvrr/{ui.rs,headset.rs,pc.rs}`, `dash-frontend/lvrr-assets/gui/lvrr/widgets.xml` |
| wayvr | the watch can't be turned off, it is the way to the dashboard; the option is gone from Look & Feel and a config that turned it off is ignored | `dash-frontend/src/tab/settings/tab_look_and_feel.rs`, `wayvr/src/config.rs` |
| wivrn | the headset streams its microphone by default, like Virtual Desktop; the permission is asked once in the lobby and turning it off later sticks | `client/scenes/lobby_lvrr.cpp` |
| wivrn | the headset starts at the low clock level and raises the CPU or the GPU only on missed frames, long frames of its own or a runtime notification, dropping back after a hold; "High power mode" pins both high and is off by default; the lobby runs at the lowest refresh rate, draws its panels again only when they changed and skips eye passes with nothing in them; the tracking pattern log is debug level | new `client/power_governor.{h,cpp}`, `client/{application.cpp,application.h,configuration.h,scene.cpp,scene.h}`, `client/render/imgui_impl.{cpp,h}`, `client/scenes/{lobby.cpp,stream.cpp,stream.h,stream_lvrr.cpp,stream_tracking.cpp}`, `client/xr/instance.h` |
| wivrn | lobby hands are translucent with a bright rim and drawn over the panels like the rays, instead of grey solids | new `client/shaders/hand.frag.glsl`, `client/scenes/{hand_model.cpp,lobby.cpp}` |
| wivrn | hand tracking is on by default; turning it off later sticks | `client/scenes/lobby_lvrr.cpp` |
| wayvr | hands work in the dashboard: point with the hand, pinch to click, make a fist to grab, a double fist with the left hand toggles the dashboard and with the right hand shows or hides the screens | `wlx-common/assets/openxr_actions.json5`, `wlx-common/src/openxr_actions.rs` |
| wivrn | the lobby looks like the dashboard: its palette and Quicksand font, the pill of round buttons, the title pill with a close button, the gradient panel, a bar with the battery and the clock, group boxes with a titled rule, gradient buttons; the connect page lives in the shell too | new `assets/Quicksand-{Regular,Bold}.ttf`, `assets/licenses/Quicksand`, `client/render/{ui_theme.cpp,ui_theme.h,ui_widgets.cpp,ui_widgets.h,imgui_impl.cpp,imgui_impl.h}`, `client/scenes/{lobby.h,lobby_gui.cpp,lobby_gui_theme.cpp,lobby_lvrr.cpp}`, `client/configuration.h` |
| wivrn | the clock ladder has a power savings step: menus run there and step up only on missed frames, a stream tries it after two quiet minutes and, when that fails, again after twice the quiet time, up to an hour; while the runtime does not ask for frames (system menu, headset off) both domains sit at power savings; discovery, which holds the Wi-Fi multicast lock, only runs while a PC is looked for; a GUI animation nobody points at redraws every other frame | `client/power_governor.{h,cpp}`, `client/render/imgui_impl.{cpp,h}`, `client/scenes/{lobby.cpp,lobby.h,stream.cpp}` |
| wivrn | the lobby shows a meadow instead of passthrough: a sky dome around a tone-mapped Poly Haven HDRI (CC0), one textured draw per frame where passthrough keeps the cameras and their pipeline running | new `assets-source/meadow.glb`, `assets/licenses/Meadow`, `client/CMakeLists.txt`, `client/configuration.h`, `client/scenes/{lobby_lvrr.cpp,lobby_gui.cpp}` |
| wivrn | quitting the headset app ends the PC session: the headset sends the exiting state on a deliberate exit and the server stops the session on it, where it waited for a reconnect because the dashboard always keeps an OpenXR client; a headset that just vanished gets five minutes to come back | `client/scenes/stream.cpp`, `server/driver/{wivrn_session.cpp,wivrn_session.h}` |
| wivrn | the headset microphone is only stopped when the PipeWire source stays paused for three seconds, consumers such as EasyEffects pause and resume it every second; the state log is debug level | `server/audio/audio_pipewire.cpp` |
| wayvr | on KDE, screens are captured through KWin's own screencast protocol (`zkde_screencast_unstable_v1`), which hands over a PipeWire stream of the output without the portal's picker dialog; the portal stays the fallback for other compositors | new `wlx-capture/src/kde_screencast.rs`, `wlx-capture/{Cargo.toml,src/lib.rs,src/wayland.rs}`, `wayvr/src/overlays/screen/wl.rs`, `Cargo.lock` |
| wayvr | on wlroots 0.19+, Hyprland, niri and COSMIC, screens are captured through ext-image-copy-capture-v1, the standard successor of wlr-screencopy: a session per output, client buffers (dma-buf, shared memory when refused), copies as the screen changes | new `wlx-capture/src/ext_image_copy_capture.rs`, `wlx-capture/src/{lib.rs,wayland.rs,wlr_screencopy.rs}`, `wayvr/src/overlays/screen/wl.rs` |
| wayvr | on GNOME, screens are captured through Mutter's own screencast D-Bus API (`org.gnome.Mutter.ScreenCast`, record the monitor by connector), which hands over a PipeWire stream without the portal's picker dialog; the order per screen is wlr-screencopy, image copy capture, KDE, Mutter, then the portal | new `wlx-capture/src/mutter_screencast.rs`, `wlx-capture/src/lib.rs`, `wayvr/src/overlays/screen/wl.rs` |
| wivrn | see-through slider: the headset fades the stream over passthrough, up to 80% so the dashboard stays visible (WiVRn #491) | `client/{configuration.cpp,configuration.h}`, `client/scenes/{stream.cpp,stream_lvrr.cpp}`, `common/lvrr_packets.h`, `server/{lvrr_dbus.cpp,lvrr_test.cpp}` |
| wivrn | the headset says why the stream ended: stopped on the PC, disconnected by the PC, or lost, with a hint to run `lvrr logs` (WiVRn #967); fixes an int overflow in the reconnect deadline | `client/scenes/{stream.h,stream_lvrr.cpp,stream_network.cpp,stream_tracking.cpp}`, `common/{lvrr_packets.h,wivrn_packets.h}`, `server/driver/{wivrn_session.cpp,wivrn_session.h,wivrn_session_lvrr.cpp}` |
| wivrn | the headset sharpens the decoded stream with contrast adaptive sharpening (after AMD FidelityFX CAS), 0 to 100% on the Headset page; it replaces the Quest runtime's sharpening toggle in LVRR | `client/shaders/reprojection.glsl`, `client/scenes/{stream.cpp,stream_defoveator.cpp,stream_defoveator.h,stream_lvrr.cpp}`, `client/{configuration.h,configuration.cpp}`, `common/lvrr_packets.h` |
| wivrn | FOV sliders: the headset scales the horizontal and vertical FOV tangents it reports, 70 to 100%, so games render a smaller FOV and the edges stay black | `client/{configuration.cpp,configuration.h}`, `client/scenes/{stream.h,stream_lvrr.cpp,stream_tracking.cpp}`, `common/lvrr_packets.h`, `server/{lvrr_dbus.cpp,lvrr_test.cpp}` |
| wivrn | AMD encoder quality: VAAPI asks Mesa for the balanced preset with pre-encode and VBAQ instead of the speed preset; Vulkan encode asks RADV for quality level 1 (balanced); `compression_level` in an encoder's `options` still overrides it | `server/encoder/ffmpeg/video_encoder_va.cpp`, `server/encoder/{video_encoder_vulkan.cpp,video_encoder_vulkan.h}` |
