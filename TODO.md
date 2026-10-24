# TODO

What SteamVR/OpenVR provides that xrizer-edge doesn't, ranked by importance. Issue numbers are on Supreeeme/xrizer.

Effort: S under a day, M a few days, L a week or more. "WiVRn" notes describe what WiVRn exposes as of 2026-10.

## 1. Breaks games now

1. Binding modes and options (S-M each)
   - radial mode (entries are dropped)
   - `touchy_click` (#449)
   - Index force sensor curve (#208)
   - dpad `deadzone_pct`, `overlap_pct`, `sticky` (parsed, ignored; fixed 90° zones)
   - `sticky_click`, force sensor `haptic_amplitude`
   - `scalar_constant` values other than 1.0
   - vector3 actions
2. Controller profiles (M)
   - Pico 4 / 4 Ultra, `XR_BD_controller_interaction` (#97, #120, #403)
   - Touch Pro, `XR_FB_touch_controller_pro`
   - Vive Cosmos, `XR_HTC_vive_cosmos_controller_interaction`
   - Xbox gamepad (core profile), PSVR2 Sense
   - `vive_cosmos_controller`, `gamepad`, `vive_tracker_*` default bindings are ignored and can't be conversion sources
   - WiVRn: Pico works through its Touch fallback; a pico4 profile fixes controller type and names
3. Binding sources (M)
   - read SteamVR's saved per-app binding choice and Workshop bindings (app 250820)
   - binding variants: only the first file per controller type loads (Alyx `_left`, `_single`)
   - remappable legacy input (Skyrim VR, Fallout 4 VR use a fixed mapping)
4. Frame timing (M)
   - `GetFrameTiming`/`GetFrameTimings` report constant GPU/CPU times, zero phase timestamps, reprojection flags and mispresents
   - `ShouldAppRenderWithLowResources` always false
   - games that scale resolution from these never adapt
   - options: time `xrWaitFrame`/`xrEndFrame`, Vulkan timestamps on our copies, `XR_META_performance_metrics`
   - WiVRn: no timing extension
5. Haptics (S)
   - legacy `TriggerHapticPulse` always plays amplitude 1.0; games encode strength as duration
   - `start_seconds_from_now` ignored
   - weak or missing vibration in VRChat (#371, #420)
6. Origin names (S)
   - `GetOriginLocalizedName` returns OpenXR paths instead of names like "Trigger" (#162, #331)
   - `GetActionBindingInfo` guesses the mode from the component; never reports dpad, scroll, grab
7. Color space (S-M)
   - Linear color space unimplemented in Vulkan backend
   - UNORM textures relabelled as sRGB (#441)

## 2. Fake success and crash insurance

8. Calls that succeed without doing anything (S, `SetOverlayRaw` M)
   - `GetBindingVariant`, `GetComponentStateForBinding` leave output buffers unwritten
   - `ShowMessageOverlay` returns `ButtonPress_0` without showing anything; `CouldntFindSystemOverlay` is the honest answer
   - `SetOverlayRaw` returns success but draws nothing
   - `HookScreenshot` succeeds but `VREvent_RequestScreenshot` is never sent
   - `SetOverlayInputMethod(Mouse)` succeeds with no effect
9. Interfaces not served at all, null for apps that don't check (S)
   - IVRTrackedCamera, IVRResources, IVRNotifications, IVRDriverManager, IVRSpatialAnchors, IVRIOBuffer, IVRIPCResourceManagerClient, IVRControlPanel_001
   - serve stubs that report nothing available
10. Device properties (S)
    - `SecondsFromVsyncToPhotons_Float`
    - `DisplaySupportsRuntimeFramerateChange_Bool`, `DisplayAvailableFrameRates_Float_Array`
    - `CurrentUniverseId_Uint64`, `DashboardScale_Float`
    - HMD `RenderModelName`, `ResourceRoot`, `InputProfilePath`, `RegisteredDeviceType`
    - `Has*Component_Bool`
    - all Matrix34 properties (`CameraToHeadTransform`)
    - role specific tracker `ControllerType`
    - wrong-type getters should return `WrongDataType`, not `UnknownProperty`
11. Device presence (S)
    - pre-created hand slots report `Controller` and are returned by `GetTrackedDeviceIndexForControllerRole` while disconnected; SteamVR answers Invalid
    - trackers: `GetControllerRoleForTrackedDeviceIndex` and `ControllerRoleHint` disagree

## 3. Overlay apps and interactive overlays

12. Overlay apps can't draw (L)
    - Overlay app type gets a headless session, `SetOverlayTexture` returns `RequestFailed`
    - XSOverlay, OVR Toolkit, Desktop+, fpsVR, OVR Advanced Settings, VRCX (#99, #322)
    - needs `XR_EXTX_overlay` and our own frame loop
    - WiVRn: `XR_EXTX_overlay` supported, overlay sessions composited above the app by z-order
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
    - side-by-side stereo overlays show both halves to each eye
    - panorama overlays ignored; cubemap skybox shows only the front face (`XR_KHR_composition_layer_cube`, not on WiVRn)
    - `SetOverlayColor`, texel aspect, pre-curve pitch, auto-curve range ignored
    - OverlayRelative, Cursor, Projection, TrackedComponent transforms
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
    - upstream PR #453 (`XR_EXT_view_configuration_views_change`)
    - `xrRequestDisplayRefreshRateFB` never called
    - WiVRn: no `views_change`; `xrRequestDisplayRefreshRateFB` works and disables its automatic rate
20. Events (S-M)
    - `TrackedDeviceUpdated`, `PropertyChanged`, `TrackedDeviceRoleChanged`, `IpdChanged`
    - `EnterStandbyMode`/`LeaveStandbyMode`
    - `SceneApplicationChanged`, `ProcessQuit`
    - `Input_BindingLoadSuccessful`/`Failed`, `ActionBindingReloaded`, `Input_ActionManifestReloaded`
    - `Compositor_*`, `ShowRenderModels`/`HideRenderModels`, `WirelessDisconnect`/`Reconnect`
    - `eventAgeSeconds` always 0; `PollNextEventWithPose` gives no pose
21. Trackers (M)
    - only through `XR_MNDX_xdev_space`; add `XR_HTCX_vive_tracker_interaction`
    - no tracker roles in IVRInput (`/user/foot/*`, `/user/waist`, `/user/chest`)
    - no hot-plug: `XrEventDataXDevListChangedMNDX` unhandled
    - no `TrackedDeviceDeactivated` for removed trackers; no tracker battery
    - body tracking extensions unused
    - WiVRn: body tracking parts are xdevs named `WiVRn generic tracker (<part>)` (Chest, Hip, Left/Right Elbow, Knee, Foot); they pass our filter but get no role, map them to `vive_tracker_<role>`; device list fixed at connect, no hot-plug
22. Skeletal (S-M)
    - compressed bone data calls unimplemented
    - tracked bones ignore motion range (`XR_EXT_hand_joints_motion_range`)
    - finger splay constant, summary type ignored
23. Legacy input (S)
    - only six buttons; no DPad or System
    - only the main axis on controllers with a stick and a trackpad (Index, WMR)
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
    - calibration state always OK; unknown play area becomes 1x1 m
    - bounds visibility and color calls do nothing; ChaperoneSetup writes ignored
29. Settings (S)
    - nothing persists, `Sync` does nothing
    - `motionSmoothing` true while `IsMotionSmoothingEnabled` false; `preferredRefreshRate` fixed at 90
30. IVRApplications (M)
    - no installed app list; vrmanifest fields other than `app_key` dropped
    - launch, MIME, auto launch and transition calls unimplemented
31. Screenshots (M)
    - no capture, files or `Screenshot*` events
32. Input paths and identity (S-M)
    - `/user/head` pose bindings, gamepad, treadmill, stylus dropped
    - no way to report another controller type to games that gate on it

## Out of scope

- SteamVR driver API (SlimeVR driver, Driver4VR, SpaceCalibrator driver); runtime drivers cover this
- D3D, DXGI, Metal, IOSurface texture types and adapter queries; Proton translates D3D to Vulkan
- runtime features: reprojection, motion smoothing, bounds drawing, room setup, system menu
- SteamVR internals: IVRMailbox, IVRControlPanel, `DriverDebugRequest`, firmware updates, `PerformanceTest*`, `CompositorDumpImages`, `ForceReconnectProcess`, benchmarks
- TrackingReference and DisplayRedirect device classes, extended display mode
