# Changes

Upstreams merged into LVRR, and LVRR's own changes to them.

## Upstreams

Merged as git subtrees without `--squash`, which keeps upstream history and authors. `.github/scripts/upstream-watch.py` reports unmerged upstream commits every Monday.

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
| wivrn | the headset counts nearly opaque pixels as opaque: 8-bit video (H.264, 8-bit HEVC) lands them a little under 1, and the passthrough showed through dark content (WiVRn #1082) | `client/shaders/reprojection.glsl` |
