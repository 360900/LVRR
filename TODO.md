# TODO

What SteamVR/OpenVR provides that xrizer-edge doesn't, ranked by importance. Issue numbers are on Supreeeme/xrizer.

Effort: S under a day, M a few days, L a week or more. "WiVRn" notes describe what WiVRn exposes as of 2026-10.

## 1. Breaks games now

1. Binding modes and options (S-M each)
   - radial: output names (`button_<n>`), layout and defaults guessed; needs vrclient disassembly
   - grab `drop_speed`/`downgrade_speed` read from SteamVR's tooltips, needs an Index test; pinch ignores `capsense_enable_threshold` and the trackpad force SteamVR adds
   - Index trackpad force (#208): Alyx's binding reads trackpad force at 0.38 (tested); if the wheel still opens on a light touch, the runtime reports force on touch, needs Monado's "Trackpad force" debug output
   - right stick dead on Quest 2 + WiVRn (#321, WiVRn #874): not reproduced with Alyx and TWD bindings, needs a headset capture (xrizer input trace, `OXR_DEBUG_BINDINGS=1`, wivrn-server `XRT_LOG=debug`)
2. Controller profiles (M)
   - Pico 4 tilt (#97): WiVRn reports the 4 and 4 Ultra as `pico4_controller` and SteamVR has no grip offset for the Ultra; needs the Ultra's OpenXR system name on WiVRn and a measured offset
   - PSVR2 Sense: `XR_MNDX_psvr2_interaction` is a preview in Monado main only, WiVRn builds without it; works as Touch today
   - Cosmos registered device type, serials and render model names unverified; Focus 3 tracking system may be `vive_eyes` like the Cosmos; `vive_tracker_*` default bindings still ignored (trackers: 21)
   - gamepad has no tracked device or binding variant; untested on a headset
3. Frame timing (S)
   - `XR_META_performance_metrics` unused (no Linux runtime has it)
4. Haptics (S)
   - force_sensor mode haptic not played (Alyx zeroes it, so SteamVR likely has one)
   - dpad ticks only on Index trackpads; unverified whether SteamVR ticks on joystick dpads
5. Origin names (S)
   - Focus 3 and Cosmos controller names are guesses (drivers not public)
   - several native bindings on one hand report the first, as OpenXR merges them; skeleton actions report the hand
6. Color space (S)
   - Outer Wilds (NomaiVR) quirk rests on one report (#441), needs a headset check
   - D3D11 `R16G16B16A16_TYPELESS` arrives through Proton as UNORM, wrong if rendered as float

## 2. Fake success and crash insurance

10. Device properties (S)
    - HMD `RegisteredDeviceType` and `InputProfilePath` unknown for WMR, Steam Link, Pico, Cosmos, Focus 3 and Frame headsets (reported as unknown and as the tracking system name)
    - `SecondsFromVsyncToPhotons` is 0, as `GetTimeSinceLastVsync` counts from the display time; SteamVR's drivers report ~11 ms before an earlier vsync
    - tracker roles only from WiVRn's body tracking names; `vive_tracker_<role>` bindings still ignored (trackers: 21)
11. Device presence (S)
    - `GetControllerRoleForTrackedDeviceIndex` keeps a disconnected controller's hand; whether SteamVR does is unverified

## 3. Overlay apps and interactive overlays

12. Overlay apps (M)
    - drawn in an `XR_EXTX_overlay` session with our own frame loop; untested on a headset (XSOverlay, OVR Toolkit, Desktop+, fpsVR, OVR Advanced Settings, VRCX, #99, #322)
    - without `XR_EXTX_overlay` they stay headless and `SetOverlayTexture` returns `RequestFailed`
    - Vulkan apps' queue is used by our frame thread's `xrEndFrame` without the app's locking
13. Overlay input (L)
    - no laser, no hit testing, no mouse, scroll or focus events
    - `ComputeOverlayIntersection`, `IsHoverTargetOverlay`, `HandleControllerOverlayInteractionAsMouse` return false
    - The Lab (#122); prerequisite for 14 and 15
14. Dashboard (M-L)
    - dashboard overlays are plain hidden overlays with no tab; `ShowDashboard` does nothing
    - focus loss sends `InputFocusCaptured`, never `DashboardActivated`/`Deactivated`
    - option: follow WayVR's dashboard over WayVR IPC
15. Virtual keyboard (M after 13)
    - `KeyboardDone`/`KeyboardClosed` sent at once, text unchanged
    - no `KeyboardCharInput`, no `*_Global` events
    - option: WayVR's keyboard over WayVR IPC
16. Overlay rendering (S-M each)
    - panorama overlays ignored; cubemap skybox shows only the front face (`XR_KHR_composition_layer_cube`, not on WiVRn)
    - pre-curve pitch, auto-curve range ignored
    - Cursor (needs 13), Projection, TrackedComponent transforms
    - subviews, render-model overlays
    - `SetOverlayFromFile`, `GetOverlayTexture`, `GetOverlayImageData`, IVROverlayView
    - overlays past the runtime's layer limit are dropped (SteamVR allows 128)
17. Notifications and message boxes are never drawn (M)
    - option: WayVR toasts over WayVR IPC

## 4. Accuracy and polish

18. Render models (M)
    - textures are a 1x1 grey placeholder
    - components never animate
    - no HMD, base station, Pico, Quest Pro or native WMR meshes
    - one fake component per model; button masks 0
    - `XR_EXT_render_model`, `XR_EXT_interaction_render_model`, `XR_FB_render_model`, `XR_MSFT_controller_model`
    - WiVRn: none of these; bundled assets needed there
19. Resolution and refresh (S-M)
    - `supersampleScale` and per-app `resolutionScale` don't affect the render size
    - `XR_EXT_view_configuration_views_change` enabled (upstream PR #453), but no event tells apps to query the size again; SteamVR has none either
    - `xrRequestDisplayRefreshRateFB` never called
    - WiVRn: no `views_change`; `xrRequestDisplayRefreshRateFB` works and disables its automatic rate
20. Events (S)
    - `PropertyChanged` only for refresh rate changes; SteamVR sends one per driver property write (battery level, IPD)
    - `TrackedDeviceUpdated` is the old driver API's "properties changed", current drivers use `PropertyChanged`
    - `ProcessQuit`, `Compositor_ApplicationNotResponding`/`Resumed`, `Compositor_DisplayDisconnected`/`Reconnected`, `WirelessDisconnect`/`Reconnect` are about other processes or SteamVR's display handling; a lost headset is `Quit` here
    - `Compositor_ChaperoneBoundsShown`/`Hidden`: runtimes don't report when they draw their bounds
    - standby follows `XR_EXT_user_presence` only, not the headset lying still
21. Trackers (M)
    - only through `XR_MNDX_xdev_space`; add `XR_HTCX_vive_tracker_interaction`
    - no tracker roles in IVRInput (`/user/foot/*`, `/user/waist`, `/user/chest`)
    - no hot-plug: `XrEventDataXDevListChangedMNDX` unhandled
    - no `TrackedDeviceDeactivated` for removed trackers; no tracker battery
    - body tracking extensions unused
    - WiVRn: body tracking parts are xdevs named `WiVRn generic tracker (<part>)` (Chest, Hip, Left/Right Elbow, Knee, Foot); device list fixed at connect, no hot-plug
22. Skeletal (S-M)
    - compressed bone data is our own format, not SteamVR's (undocumented; apps only hand it back)
    - tracked bones ignore motion range (`XR_EXT_hand_joints_motion_range`)
    - finger splay constant, summary type ignored
    - Steam Frame thumb curl from joints unverified
    - without `XR_EXT_hand_tracking_data_source`, joints derived from a controller count as optical
23. Legacy input without chosen bindings (S)
    - fixed mappings match SteamVR's per-controller legacy files (no DPad there, the system button is the runtime's); Focus 3 bumper (`Axis3`) isn't in its OpenXR profile
    - WMR legacy mapping unverified: SteamVR's WMR driver isn't shipped on Linux
    - `vive_tracker_handed` legacy bindings (trackers: 21)
24. Dominant hand (S-M)
    - stored, bindings never mirrored; `supports_dominant_hand_setting` not parsed
25. Compositor (S-M)
    - `CanRenderScene` always true, `GetCurrentSceneFocusProcess` always our pid
    - `FadeGrid` ignores its fade time, `GetCurrentGridAlpha` always 0
    - partial `GetCumulativeStats`
    - depth submissions dropped (`XR_KHR_composition_layer_depth`, not on WiVRn)
    - `TextureWithMotion`, `LensDistortionAlreadyApplied` ignored
    - no skybox, grid or overlays while the app stalls without `WaitGetPoses`
26. Eye tracking and foveation (M)
    - IVRInput eye tracking and `GetEyeTrackedFoveationCenter*` unimplemented (`XR_EXT_eye_gaze_interaction`)
    - WiVRn: advertised, active on headsets with eye tracking
27. Mirror window (M-L)
    - IVRHeadsetView only stores values; size defaults to 0x0
    - `GetMirrorTextureGL`, `GetMirrorTextureD3D11` unimplemented
28. Chaperone (S)
    - calibration state always OK; unknown play area becomes 1x1 m (an error state would stop games on runtimes without bounds)
    - `AreBoundsVisible` only knows what the app forced; `GetBoundsColor` ignores the fade distance; ChaperoneSetup writes ignored, the runtime owns its bounds
29. Settings (S)
    - the user's `steamvr.vrsettings` values (e.g. `supersampleScale`) aren't read, only bindings choices are
30. IVRApplications (M)
    - no installed app list; vrmanifest fields other than `app_key` dropped
    - launch, MIME, auto launch and transition calls unimplemented
31. Screenshots (M)
    - no capture, files or `Screenshot*` events
32. Input paths and identity (S-M)
    - `/user/head` pose bindings, gamepad, treadmill, stylus dropped
    - games without an action manifest can't be shown another controller type

## Out of scope

- SteamVR driver API (SlimeVR driver, Driver4VR, SpaceCalibrator driver); runtime drivers cover this
- D3D, DXGI, Metal, IOSurface texture types and adapter queries; Proton translates D3D to Vulkan
- runtime features: reprojection, motion smoothing, bounds drawing, room setup, system menu
- SteamVR internals: IVRMailbox, IVRControlPanel, `DriverDebugRequest`, firmware updates, `PerformanceTest*`, `CompositorDumpImages`, `ForceReconnectProcess`, benchmarks
- TrackingReference and DisplayRedirect device classes, extended display mode
