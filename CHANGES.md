# Changes

Upstream PRs integrated into xrizer-edge (personal fork of Supreeeme/xrizer) and the fork's own changes.

Baseline: upstream `main` at `0519ded` (2026-10-20). Merge commits preserve original authors.

Status: `pending` | `merged` | `superseded-by-<n>` | `blocked` | `dropped` | `revived`

## Phase 1a: quick wins

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 451 | clippy: Swap chunks_exact call for as_chunks | merged | blocks clippy on new toolchains |
| 297 | improve error message when app is using wrong GPU | merged | plain text fix |
| 230 | fire VREvent_Quit when session STOPPING | merged | also handles Quit for OpenVR 0.9.12; false Quit on xrizer-initiated restart fixed in `dc6ed93` |
| 398 | treat swapchain sample count 0 as 1 | merged | fixes #369 |
| 245 | poses functions with "seconds from now" | merged | The Lab; conflict resolved: xr_time threaded through new input refactors |
| 306 | Add OpenVR v0.9.19 for BoneWorks | merged | closes #305 |
| 404 | rendermodels: standard pose components | merged | BigWalk #403 |
| 416 | input: report release edge of custom bindings | merged | Alyx throw/grab |

## Phase 1b: input infra (sequential, same files)

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 419 | System button | merged | prerequisite of 440; merged with IVRClientCore003, GetEnglishStringForHmdError, SimpleController fixes |
| 422 | input: support Vector2 binding inversion | merged | |
| 406 | action set name invalid chars + string/number json | merged | Halo CEVR #390 |
| 394 | system: HMD tracking system/model/manufacturer strings | merged | Serious Sam 3, #232 |
| 430 | HMD identity from controller interaction profile | merged | #365; probe session at init; review by hand (vibe-coded); skip for Background/Utility, `XRIZER_PROFILE_PROBE=0`, error path fixed in `5353596` |
| 429 | input: implement action binding and origin queries | merged | fixes #162 |
| 418 | GetActionOrigins, GetActionBindingInfo, InputProfilePath | superseded-by-429 | follow-up: port InputProfilePath_String tracked property (per-profile input_profile_path exists already) |

## Phase 1c: quirks and poses

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 397 | hl2-vr: multiple fixes | merged | adds src/quirks.rs; prerequisite of 400 |
| 400 | superhot-vr: fix grab, menu button | merged | on top of 397; probably fixes #385 |
| 382 | controller pose transformations | merged | #303; supersedes 234; raw pose cache keyed by time and origin (bounded, LRU) + per-call transform |

## Phase 1d: fix CI first

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 338 | XR_EXT_hand_tracking_data_source skeletal level | merged | CI fixed (xr::sys::Handle import); unblocks 261; later `ffc4e53` ported by hand, Frame included, also covers summary and level |
| 452 | hand tracking data source support (frame fingers) | superseded-by-338 | closed upstream in favor of 338; Frame finger sensing allowed for via `SENSES_FINGERS` |
| 388 | overlays, skybox, color conversion, canting | merged | CI fixed (unused PI import); high value (#79, racing) |
| 440 | Steamframe profile | merged | adapted to HmdProperties/input_profile_path/pose_transformation; pose transforms TODO |
| 41 | action set priorities and device restriction | merged | needs VRChat validation on edge build; priority dedup, offset overflow, sync failures fixed in `a0e4ea0`; XR_EXT_active_action_set_priority stays off (it would block xrizer's own sets), priorities are emulated per input instead |

## Phase 2: rebase conflicted

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 261 | hand tracking with no interaction profile and/or grip pose | merged | ported synthesised grip pose + pre-created hand devices + VRLinkHand fallback; closes #165 |
| 401 | log to per-process files, crash reports | merged | per-process log files, crash collection, 32-bit loader hint (#450); partial #73 |
| 130 | WMR controller support | merged | WMR profiles adapted to static trait; registered for HolographicController/HPMotionController; fakexr exposes HP + Samsung extensions |
| 330 | Background and Utility application types | merged | headless sessions (XR_MND_headless + XR_KHR_convert_timespec_time), headless graphics backend, OpenXrData::new(injector, application_type) |
| 193 | New interface versions | merged | overlay name/raw texture/thumbnail additions; newer 1.23.8 header; fixes #177 UEVR |
| 229 | libmonado battery status | merged | closes #26; monado feature now enables libmonado; battery percentage/provides/charging properties; builds fine without system libmonado |
| 69 | Skeletal Summary | dropped | superseded by merged 313 + 261 skeletal summary work |
| 168 | Hitman | merged | C-unwind ABI on exported fns/vtables; GetPosesForFrame delegates to GetLastPoses; GetLastPosePredictionIDs; partially fixes #448 |
| 87 | IVRDebug stub and IVROverlay014 stub | merged | adapted; IVRDebug + IVRChaperoneSetup added, overlay 014 already covered by 193 |
| 234 | Support all the BoundPoseTypes | superseded-by-382 | draft |
| 337 | input profile path support | superseded-by-429 | draft |

## Phase 3: fork hardening

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 442 | build: inherit workspace rust-version | merged | closed by AI policy upstream (#433); workspace MSRV bumped 1.85 -> 1.88 (as_chunks, PR 451) |
| 443 | skip bindings with mismatched action type | merged | panic -> warn+skip, fixes #384; adapted add_vector2_binding call site |
| 444 | clientcore: don't panic in Cleanup | merged | #423, #439; graceful cleanup with outstanding refs |
| 445 | compositor: replace todo!() with warnings | merged | fixes #283; GetLastFrameRenderer conflict resolved to std::process::id(); GetFrameTimings implemented by 446, SubmitWithArrayIndex fixed in sweep |
| 446 | compositor: implement GetFrameTimings | merged | real timing from last presented frame |
| 453 | openxr_data: enable XR_EXT_view_configuration_views_change | merged | eye size is queried per call, so apps that query again get the new size; change logged; not on WiVRn |

## Fork fixes (not upstream PRs)

| Commit | Change |
|--------|--------|
| `cf9881b` | overlay: 53 `todo!()` stubs replaced with one-time warnings and safe defaults |
| `a523d24` | system + applications: stubs warn and return safe defaults; `ApplyTransform` implemented; non-Quit queued events no longer panic |
| `4a0c756` | rendermodels, overlayview, vulkan Linear colorspace, vive_tracker bindings, `HmdSystemFactory` stubs replaced; no `todo!()`/`unimplemented!()` left in `src/` |
| `faab880` | compositor: `SubmitWithArrayIndex` warns and returns `IncompatibleVersion` |
| `bf6be63` | workspace MSRV 1.85 -> 1.88 (for `as_chunks`) |
| `d9f387b` | CI: cancel superseded runs, embed `git describe` version; release: fork container image |
| `dc6ed93` | no Quit on xrizer-initiated session exit; ignore stale session events; bounded end_session waits |
| `5353596` | probe: skip for Background/Utility, `XRIZER_PROFILE_PROBE=0` opt-out, keep results on teardown failure |
| `a0e4ea0` | input: dedup set priorities, i64 priority offset, unrestricted sync for sets without OpenXR actions, no panic on sync failure |
| `3348188` | refresh rate fallback returns Hz instead of seconds |
| `3950c0c` | gl: apps rendering on another GPU than the runtime (hybrid laptops) get frames copied through host memory |
| `e193c84` | session the runtime stops (WiVRn's stop button) ended right away instead of after WiVRn's 10 s loss |
| `2b3e8e4` `5cb67b7` | refresh rate cached and updated from runtime events, which apps get as a display frequency change; no queries once the session is lost |
| `a7c8ef3` | gaps in converted bindings logged at debug, in the game's own once at info; quieter repeated manifest loads and routine events |
| `31c311f` | libmonado found through the host's runtime file inside Steam's container (Proton battery, trackers) |
| `23ad894` | Index `/input/pinch` bound to the trigger, like SteamVR |
| `ef5dbfb` | gl: fbconfig of contexts created without one taken from the drawable |
| `176f51f` | applications: added manifests and the app's identity kept for the process |
| `9714aab` | apps that don't quit 5 s after the runtime asks (30 s if acknowledged) are killed, like SteamVR; `XRIZER_KILL_ON_QUIT=0` opts out |
| `d9d3e55` | games can be shown Index or Touch controllers whatever the real ones are (`XRIZER_CONTROLLER_TYPE`, or `controller_type` in `~/.config/xrizer/apps/<app key>.json`): bindings converted, that controller's identity and poses, the headset keeps its own |
| `704a419` | without a chosen controller type, controllers a game has no bindings for show as the Index or Touch controllers their bindings are converted from |

## Audit fixes (2026-10-22, 335 commits `9a6d924`..`3323c56`)

From a full review of the code against the OpenVR headers, Monado, WiVRn and Proton. Verified with unit tests, an end-to-end test against Monado, Miri and 32-bit builds. Commits listed are representative; `git log 8532410..3323c56` has all of them.

| Area | Change | Commits |
|------|--------|---------|
| session loss | session and instance loss, and the runtime stopping the session (also the temporary one), send `VREvent_Quit` instead of aborting; WaitGetPoses and Submit then return `DoNotHaveFocus`, IVRInput `IPCError`; other runtime errors fail the call or fall back to defaults instead of panicking, repeats logged once; a rejected real session returns `InvalidTexture` and keeps the temporary one; swapchain creation failures return `RequestFailed`; overlays over the runtime's layer limit are dropped | `5e313fd` `9ae755e` `4e09502` `c2b7f25` `434586f` `32ea882` |
| locking | recursive session reads; session lock taken before the overlay and skybox locks, device list before the bone cache; no write lock for an unchanged tracking space; `PollNextEvent` and Input creation no longer wait for the frame thread; overlay swapchains stay locked until the frame has ended; the real session is set up once when Submit and overlay calls race; concurrent handle creation no longer duplicates handles; fixes deadlocks between app threads and the frame loop | `21837bd` `2ab6d01` `f9e9708` `5dcd434` `c1056f6` `270f2ae` |
| interaction profiles | `hand_interaction_ext` profile only suggested with `XR_EXT_hand_interaction`, which stays disabled, so tracked hands bind to `simple_controller`, get manifest bindings and are reported as hands; profiles the runtime rejects are skipped by the probe, legacy input and manifest loading instead of panicking; Steam Frame profile enabled (extension name matched with its nul, `dpad_*` paths); fixes legacy-input games crashing on runtimes with hand tracking (Monado, WiVRn) | `c563de9` `9a79130` `993b864` `28177bd` `471ae20` `06adc91` |
| ABI | enums the app fills are newtypes, so unknown values are no longer UB; unexpected struct sizes and null action data or names return `InvalidParam`; `InputSkeletalActionData_t` without `boneCount` for IVRInput_005+; IVRInput_007 `InputBindingInfo_t` and IVRInput_003 `VRActiveActionSet_t` accepted; `vrshared_*` types 8-byte aligned on i686; struct sizes checked at compile time on x86_64 and i686 | `d5aeb2f` `7bd972d` `d192508` `ef5a572` `8c04f3d` `d1dd7f4` |
| interfaces | IVRSystem_010, IVRCompositor_010, IVROverlay_008 and IVRInput_003 served; all events reach IVRSystem_009 and IVROverlay_007/008; flag numbers of IVROverlay_020 and older converted to masks; IVRInput_004 `GetOriginLocalizedName` returns the name; `GetOutputDevice` writes 0 instead of crashing on IVRSystem_016's null instance or leaving garbage for non-Vulkan requests; header names for enum values and SteamVR's text from `GetEnglishStringForHmdError` | `36ab212` `137e471` `80c0650` `b5713c9` `7058eb8` `dce912e` |
| init | Init reports a missing loader, runtime or headset with SteamVR's codes instead of aborting or `Init_VRServiceStartupFailed`; background apps get `Init_NoServerForBackgroundApp`, which Proton's helper treats as no VR; `BIsHmdPresent` probes the runtime instead of always answering true, `XRIZER_HMD_PRESENT` skips the probe; Overlay and Utility apps get a headless session; a second Init with another app type succeeds; concurrent factory and first interface lookups no longer abort | `ba81549` `f53c217` `e988305` `d9ac47e` `601241b` `b019a07` |
| headless apps | apps without a frame loop get events, poses and input from `PollNextEvent`, `GetDeviceToAbsoluteTrackingPose` and `UpdateActionState`; Background, Overlay and Utility apps get `IsNotSceneApplication` from Submit and `RequestFailed` for overlay textures instead of a panic or a graphics session; only scene apps probe the interaction profile | `468137d` `937de95` `2469913` `aafb4ce` `088e2a4` |
| legacy input | SteamVR's Index, Frame and Focus 3 legacy mappings; button events every frame, activations first; touch state for trigger, menu, A and grip; Index and Frame trigger and Frame grip pressed at SteamVR's analog thresholds; packet number published with its state; null pose accepted in `GetControllerStateWithPose`; no panic on a rejected haptic or when legacy setup races a manifest load | `0417a46` `981738f` `ddef254` `e14507c` `b77be5a` `899abdd` |
| bindings | chord, timed (long, held, single) and scroll bindings; stick and trackpad deadzones; `scalar_constant` as a button press; integer and boolean parameters; Index finger curl paths; grab mode without a force sensor; trigger click thresholds on controllers without a click; dpad releases reported; invalid entries and mismatched action types skipped instead of whole files or aborts; an action per bound input; bindings converted for controllers the game ships none for | `45a2ea9` `874489b` `f93a845` `b9ade26` `4341b27` `80f8ff6` |
| action state | action set priorities emulated; `fUpdateTime` reports the age of a change; active origin of unrestricted reads; controller role hint and device class properties; Vector1 state computed once per sync; pose actions of absent hands inactive; `MismatchedActionManifest` for a second manifest; action names past OpenXR's limits deduplicated and cut instead of aborting; one handle per name under concurrent lookups | `e4e22ac` `fc813bf` `9e01142` `1203c41` `2b1c0f3` `6493e4a` |
| skeletal | `GetBoneName` and `GetBoneHierarchy`; bone arrays longer than 31 no longer panic; Partial level for the Index, else from the hand tracking data source and kept while untracked; aux bones from the finished finger chain; motion range honoured for estimated bones; skeletal actions inactive without their hand or set; summary curls from tracked joints without a bound profile; joints located once per frame; hand trackers only when supported, requesting data sources so joint queries work on Monado | `31679e1` `7e57364` `c548e00` `fe5e1d0` `76e2844` `f27742a` |
| poses | pose cache keyed by time and origin with LRU eviction; untracked devices report connected and out of range at their last valid pose, forgotten on profile changes and zero pose resets; synthesized velocity history per origin, reset after pauses and zero pose resets; game and render pose arrays filled independently, every slot filled; Reverb G2 raw controller pose no longer pitched 70 degrees off; negative and non-finite pose time offsets no longer panic and are kept within a second | `34b47ec` `0a9b82c` `06bca4a` `4a087e8` `927beeb` `e3e959e` `8148144` |
| spaces and events | runtime recenters followed, `XRIZER_RECENTER_STANDING` recenters standing too; standing floor kept on zero pose resets; real seated and raw zero poses; play area rect in the standing universe; `IVRChaperoneSetup` backed by runtime bounds; user presence; focus loss reported like an open dashboard; `/user/head/proximity`; `GetSortedTrackedDeviceIndicesOfClass`; queued events carry their data and device index | `d474641` `fca49bf` `c90bc55` `9ec2cb3` `c01dca4` `e508840` |
| overlay | premultiplied and alpha flags, curvature, color space (sRGB decoded only for gamma textures), multisampled textures, device-relative overlays, scaled transforms, `KeyInUse`, `KeyboardClosed`; lat-long skyboxes as equirect layers, shown only while the app shows no frame; invalid layers skipped; swapchains freed with destroyed overlays, kept on clears, recreated with the next texture after a session restart; dashboard visible while the runtime holds the input focus; no panics on foreign, empty or unsupported textures, missing graphics sessions or swapchain failures | `9f07df0` `b625fe3` `2f56d1b` `3bb0636` `49ce964` `b463212` |
| vulkan | copy submissions fenced and waited for before reuse, swapchain destruction and teardown, waits bounded to 1 s; eye copy barrier, blit region and stale content on shrinking copies fixed; multisampled eyes resolved in their own format, swapchains always single sampled; overlay render area, texture layout, resource lifetimes and sRGB decode fixed; multisampled overlays drawn; the app's device and instance never destroyed; Init fails naming missing device extensions instead of aborting; lost device logged instead of aborting | `7cdce12` `cea59ab` `c049f4d` `849d1a6` `3eadc8c` `0b7c7ad` |
| gl | EGL contexts through `XR_MNDX_egl_enable`; GLX found in `libGL.so.1` without glvnd; `Submit_GlRenderBuffer` and `Submit_GlArrayTexture` honored; unsized and 3-channel formats mapped to sized ones, gamma data in sRGB swapchains, gamma RGBA8 copied without a second encode; app GL state and bindings restored; overlays copied into 2D images, flipped overlay bounds mirrored; eye layers cleared when copies shrink; manifest restarts and overlay swapchain destruction deferred to the Submit thread when the GL context isn't current; session leaked at shutdown once the context is gone (Vivecraft crashed on exit) | `eeee603` `db0e7d3` `aecb526` `108abb8` `82b046d` `ec1de04` |
| compositor | full frame timing and cumulative stats; `FadeToColor`; `Submit_TextureWithPose`; `SubmitWithArrayIndex`; explicit timing: frames the app hands off are left to it, IVRCompositor_021's explicit timing maps to runtime hand-off; texture bounds clamped; null, unsupported and foreign textures rejected; failed session or swapchain creation returns an error instead of aborting; WaitGetPoses paced without a frame | `99a1256` `3726a65` `e35a106` `2743c56` `f72fd4a` `434586f` |
| system | canted views get a widened FOV and a matching hidden area mesh; line loop mesh counts and per-call leaks fixed; invalid runtime FOVs replaced; failed view location falls back to the last views instead of panicking; views located into zeroed buffers; velocities rotated in `ApplyTransform` | `99b07f1` `583af82` `ddf5a73` `e023894` `3e61aa6` |
| rendermodels | WMR and Steam Frame names resolve to the generic and Index meshes, moved to the reporting profile's raw pose; tracked hands get pose components without a mesh; `GetComponentState` returns the profile's transforms for `tip`, `base`, `handgrip` and `grip`, identity on failure; needed size returned for short name buffers; handed out buffers stay valid | `1409ad0` `150d64d` `46a6416` `0cb50b3` `81c1e75` `7d9ac76` |
| settings | SteamVR's defaults for the keys the headers name, `worldScale` and `steam.app.*/resolutionScale`; other unset keys report `UnsetSettingHasNoDefault`; IVRSettings_001 getters return the caller's default; app-set values kept across `VR_Shutdown`; `ipd` and `LastKnown` HMD model and manufacturer read from the headset | `440aa69` `b23d54e` |
| build and CI | version embedded and checked in `vrclient.so`; MSRV `cargo check` of all targets; tests of every workspace member; edition 2024 resolver; releases built from the tag with CI's tests and lints, write token only in the draft job; `:latest` image only pushed from main, other refs build in a per-commit image; xbuild replaces a stale `vrclient.so` link | `b852906` `09f4596` `6129a27` `3b5770e` `4df4190` `a437336` |
| tests | end-to-end test against a real runtime (`XRIZER_E2E=1`); fakexr gained events, moved spaces, headless sessions, session and instance loss, Vulkan extension checks and hand joints; under Miri, fakexr's pointer provenance and the factory's C-unwind ABI fixed, timing, lock contention and dlopen tests skipped | `9a6d924` `b657e1e` `80561cc` `fcd9302` `28c764c` `d742b99` |
| docs | pressure-vessel runtime import and socket limits; build requirements, 32-bit build, install prefix, static loader config path; custom bindings for controllers the game has none for; only scene apps probe; `XRIZER_HMD_PRESENT`; `XRIZER_RECENTER_STANDING`; canting opt-in skips the widened FOV | `064f94b` `b522695` `5981f7a` `baad68f` `0fcc34e` `b026477` |

## TODO section 1 (2026-10-25, `dde4ab2`..)

Gaps that broke games, checked against SteamVR's shipped files (binding UI defaults, profiles, localization, vrclient strings), Proton, Unity's plugin, WiVRn and Monado.

| Area | Change | Commits |
|------|--------|---------|
| binding modes | `touchy_click` and `sticky_click`; dpad `deadzone_pct`, `overlap_pct`, `sticky` with SteamVR's defaults (25%, 50%); grab `force_hold_threshold`/`force_release_threshold`, so a light touch on the Index force sensor no longer grabs (#208, partly); Index trackpad dpad haptic restored; scalar_constant `on/*` and `off/*` on every axis; vector3 actions; radial menus skipped with one warning (format unknown, see second pass) | `d11463e` `e2a0d8f` `4e7a324` `64b6acd` `65a560a` `f310342` `f0be19c` `14fa2dd` |
| controller profiles | Pico 4 and Neo 3 (`XR_BD_controller_interaction`), Quest Pro (`XR_FB_touch_controller_pro`), Vive Cosmos (`XR_HTC_vive_cosmos_controller_interaction`); `pico_controller` and `vive_cosmos_controller` bindings load and convert; converted Pico and Cosmos bindings report the controller they were made for, like SteamVR | `73d97fb` `cb2233c` `1f4c35a` `0ff0de8` |
| binding sources | bindings chosen in SteamVR (`steamvr.vrsettings`, Workshop items) used after custom ones, `XRIZER_STEAMVR_BINDINGS=0` opts out; `GetBindingVariant` reports `options.variant` (Alyx single controller) | `dde4ab2` `9ae9259` |
| frame timing | measured from the frame loop: GPU time from Vulkan fences and GL timestamps, CPU phases, presents, reprojection and mispresents from predicted display times; cumulative stats and `GetFrameTimeRemaining` from the same records; `ShouldAppRenderWithLowResources` while the runtime has the input focus | `c288f34` `aaaff85` `dd60dd1` `cf3c8f1` `7655329` `fe7ad43` |
| haptics | legacy pulse length scales the amplitude (full strength since the second pass); zero, NaN and sub-10 ms vibrations made playable, frequencies clamped like SteamVR's drivers; `start_seconds_from_now` honoured; SteamVR's threshold click haptics on button, toggle, trigger and grab bindings (`haptic_*`, `off_haptic_*`, `haptic_delay`; VRChat menu feedback, #371, #420) | `e40ea9e` `02cd032` `78fa1a7` `19507a8` |
| origin names | `GetOriginLocalizedName` returns SteamVR's names ("Left Hand Oculus Touch Trigger", #162, #331); action origins are OpenVR inputs; `GetActionBindingInfo` reports the binding's mode, slot and input type; render model component names from SteamVR's models | `351011b` `a647acd` `ef24d18` |
| color space | Linear textures shown as linear; gamma float textures and runtimes missing a swapchain format keep the right brightness; Auto 8-bit UNORM stays sRGB per the OpenVR header (#441), `XRIZER_LINEAR_UNORM=1` opts out | `f9061f1` `6287b37` `3392fd9` |
| gl | GL sessions the runtime rejects fall back to the Vulkan bridge instead of a black screen, e.g. no-error EGL contexts (Sodium on Minecraft 26.x under Wayland) on Monado based runtimes with NVIDIA; on the runtime's GPU the bridge reads into memory shared with it and waits on the GPU, not the CPU | `ed169f5` `45191a7` |

## TODO section 1, second pass (2026-10-25, `e8a7789`..)

What was left of section 1, plus WiVRn reports whose cause is on our side.

| Area | Change | Commits |
|------|--------|---------|
| binding modes | `radial` and `radial_menu` bind as dpads with `num_buttons` zones (output names guessed; SteamVR ignores `radial`); Index grabs drop when the squeeze is let go while moving (`drop_speed`, `downgrade_speed`, read from SteamVR's tooltips) | `32200bd` `4bebb46` `9498293` |
| controller profiles | Xbox gamepad (`/user/gamepad`): `gamepad` bindings drive actions, haptics and priorities, no tracked device, origins only while one is connected; Cosmos reports `vive_eyes` and `vive_cosmos`; SteamVR's left `/input/system` binds the menu button on Touch, Focus 3 and Pico | `8748b24` `2693912` `5ce6287` `0d48890` `7ebc2c2` `7befc07` `6cd7ff8` |
| binding sources | per-app custom bindings in `~/.config/xrizer/bindings/<app key>/` (WiVRn #1191); bindings chosen again when an app identifies after loading them; `IdentifyApplication` pid 0 is the caller; legacy games (Skyrim VR, Fallout 4 VR) take custom or SteamVR-chosen legacy bindings through SteamVR's legacy actions, including mirrored mode and `simulated_controller_type` | `fd8aa05` `b80271b` `55cbc88` `5f628c9` `1732e7c` `a3a91d2` |
| controller presence | short interaction profile losses (Quest system menu, WiVRn #1100) keep controllers connected for 0.5 s and while the runtime has the input focus; a controller replaced by another is reported as reconnected; custom bindings follow the profile of the current sync; hands without a profile (WiVRn #1134) get activation events; Monado trackers keep their index | `b248aaa` `5d332d5` `c3afa1d` `b75bd5a` `b735fe1` |
| frame timing | `VSyncsReadyForUse` and `VSyncsToFirstView`; mispresents count frames handed over after their display time; `TransferLatencyMs` and `WaitForPresentCpuMs` stay 0 like SteamVR without Steam Link | `e8a7789` |
| haptics | trigger mode clicks on analog inputs bump like button mode; legacy pulses at full strength like SteamVR | `05e4ae5` `de42240` |
| origin names | action data's `activeOrigin` is the input driving it, the pose source for poses; `GetActionOrigins` lists every bound input; `GetOriginTrackedDeviceInfo` reports the device as `devicePath`; HP, Cosmos and Focus 3 input names pinned to SteamVR's | `58c0f7c` `bc23f30` `a1b142d` `f650111` `7e445b2` |
| color space | Outer Wilds' Auto UNORM textures shown as linear (#441), `XRIZER_LINEAR_UNORM=0` opts out; `A2R10G10B10_UINT`, `A2B10G10R10_UINT` and GL `RGB10_A2UI` shown as UNORM | `ed83bc4` `cf14941` `6860b0a` |
| tests | Alyx's Index weapon wheel reads trackpad force at 0.38 (#208); right stick turn and axis2d bindings on Touch (#321, not reproduced); hand joints derived from controllers (WiVRn #1178) estimated on Touch, tracked joints give SteamVR's bones; fakexr routes inputs and reports actions per hand like Monado | `f73db24` `fbfb3fc` `c967d17` `9cbcc2d` `8591bdf` `daee804` `e9bfd85` |

## TODO section 2 (2026-10-26, `2572062`..)

Calls that faked success, interfaces apps crashed without, and device properties and presence, checked against SteamVR's shipped files and headers.

| Area | Change | Commits |
|------|--------|---------|
| faked success | `ShowMessageOverlay` answers `CouldntFindSystemOverlay`; `SetOverlayInputMethod` fails for mouse input and `GetOverlayInputMethod` reports `None`; `HookScreenshot` fails, as nothing requests screenshots; `GetComponentStateForBinding` fills the render model's component state; `SetOverlayRaw` draws the pixels (RGBA8 or RGB8, through the session's Vulkan, OpenGL or GL bridge backend, in the overlay's colour space) and sends `ImageLoaded`/`ImageFailed`; pixels sent before the first frame or without the app's GL context wait for it | `2572062` `7cda0ad` `561996c` `a3193f9` |
| interfaces | IVRTrackedCamera (001, 003 to 006), IVRResources, IVRNotifications, IVRDriverManager, IVRSpatialAnchors, IVRIOBuffer (001, 002), IVRIPCResourceManagerClient (002, 003) and IVRControlPanel_001 served, every call reporting nothing available with its out parameters written; bindings generated for them, with field renames and 0.9.12's `CameraVideoStreamFrame_t` layout handled | `62d0c2c` `43c8eee` |
| device properties | `WrongDataType` for properties of another type, `UnknownProperty` only for ones the device lacks; HMD `Has*Component_Bool`, `SecondsFromVsyncToPhotons` (0, see TODO), `DashboardScale` (1), `CurrentUniverseId` (1), identity camera, status display and IMU transforms; `DisplaySupportsRuntimeFramerateChange`, `DisplaySupportsMultipleFramerates` and `DisplayAvailableFrameRates_Float_Array` (`GetArrayTrackedDeviceProperty`) from `XR_FB_display_refresh_rate`; HMD `RenderModelName` (`generic_hmd`), `RegisteredDeviceType`, `InputProfilePath` and `ResourceRoot` from the identifying controller profile, `ResourceRoot` for controllers; trackers named after a body part (WiVRn) identify as `vive_tracker_<role>` with that role's input profile | `c571e3d` |
| device presence | controller slots report `Invalid` until a controller was there; `GetTrackedDeviceIndexForControllerRole` answers only for connected controllers; trackers' role is `OptOut` like their hint; a hand tracked after its controller lost its profile replaces it (Quest on WiVRn), and the controller replaces the hand when its profile returns | `b16cccb` |
| tests | fakexr lists refresh rates (`xrEnumerateDisplayRefreshRatesFB`) and takes named trackers (`add_tracker`); the fake Vulkan device gained buffers, mapped memory, flushes and buffer to image copies | `c571e3d` `7cda0ad` |

## TODO section 3 (2026-10-26, `5518260`..)

Overlay apps and overlay rendering.

| Area | Change | Commits |
|------|--------|---------|
| overlay apps | Overlay apps get an `XR_EXTX_overlay` session (create info chained by wrapping `xrCreateSession`) instead of a headless one when the runtime has it; a frame thread draws their overlays; their GL textures go through the GL bridge; scene `Submit` still rejected | `5518260` |
| overlay rendering | `SetOverlayColor` tints layers (`XR_KHR_composition_layer_color_scale_bias`); `SetOverlayTexelAspect` applied; side-by-side overlays show one half to each eye (parallel and crossed); overlay relative transforms (old interfaces) follow their parent | `5b655e9` `66d3097` `46abd17` `f61f308` |
| tests | fakexr accepts and records the overlay session create info | `5518260` |

## TODO section 4 (2026-10-28, `305fd4a`..)

Accuracy and polish.

| Area | Change | Commits |
|------|--------|---------|
| render models | with a SteamVR install in a Steam library, its models load with their textures (PNG and TGA) by every name spelling, drivers' models included; `GetRenderModelOriginalPath` and `GetRenderModelThumbnailURL` answer for them; the bundled meshes stay the fallback | `2ed64a0` |
| render model components | SteamVR's models have the components their JSON lists: `GetComponentCount`/`Name`, each part's mesh as `<model>/<part>`, `GetComponentButtonMask`; `GetComponentState` moves triggers, grips, buttons, sticks, trackpads and the Frame's dpad with the controller's raw input (an action set of our own, synced with the app's), shows the trackpad touch marker and the scroll wheel per SteamVR's visibility rules, reports `IsTouched`/`IsPressed`/`IsScrolled`; pose components report an identity render model transform and their frame as the local one, as the Unity plugin composes them | `c400f44` |
| poses | the profiles' pose components (`tip`, `base`, `grip` and the rest) apply SteamVR's `rotate_xyz` as Z, Y, X, as its files do; with two nonzero angles they were off, the Index tip by about a degree | `b309919` |
| dominant hand | apps with `supports_dominant_hand_setting` get their bindings mirrored for a left-handed user, from the `DominantHand` setting in xrizer's settings file or the one chosen in SteamVR; `SetDominantHand` saves it and loads the bindings again; `GetDominantHand` is `PermissionDenied` for other apps, as the header says | `57e5fec` |
| applications | the app list holds the manifests added in this process, those registered with xrizer (`~/.config/xrizer/appconfig.json`, where manifests added for good go) and with SteamVR, and Steam's installed VR games; string and bool properties from the manifests, relative paths made absolute; `LaunchApplication`, `LaunchDashboardOverlay` and `LaunchApplicationFromMimeType` start binaries with their arguments or open URLs, a scene app launched from a scene app quits it like SteamVR; MIME defaults and auto launch flags kept in the settings file, SteamVR's `vrappconfig` read for the latter; launched processes and registered subprocesses answer `GetApplicationProcessId`/`GetApplicationKeyByProcessId`; `ApplicationListUpdated` and `ApplicationMimeTypeLoad` events | `48f66a5` |
| resolution | `supersampleScale` and the app's `resolutionScale` scale the render size like SteamVR's resolution settings, from xrizer's settings file | `762fa3f` |
| compositor | `FadeGrid` fades over its time, which `GetCurrentGridAlpha` reports | `762fa3f` |
| skeletal | `GetSkeletalBoneDataCompressed`/`DecompressSkeletalBoneData` and IVRInput_004's variants, in a format of our own holding both transform spaces | `d26b910` |
| legacy input | SteamVR's legacy bindings for the Pico, Focus 3 and Frame controllers simulate Touch controllers, so legacy apps see those | `6de9a8d` |
| chaperone | `ForceBoundsVisible`/`AreBoundsVisible` recorded; `GetBoundsColor` from the `collisionBounds` settings; `SetSceneColor` and `ReloadInfo` quiet | `d26b910` |
| settings | values apps set persist in `~/.config/xrizer/settings.json`, `Sync` reports changes; `preferredRefreshRate` is the runtime's rate; `IsMotionSmoothingEnabled` follows `motionSmoothing` | `d26b910` |
| events | `eventAgeSeconds` is the event's age; `PollNextEventWithPose` gives the event's device pose; `TrackedDeviceRoleChanged` with controller activations; `IpdChanged` when the headset's IPD moves by 0.5 mm; `HideRenderModels`/`ShowRenderModels` with the input focus; `EnterStandbyMode`/`LeaveStandbyMode` after 5 s without the user, activity level `Standby`; `Input_BindingLoadSuccessful`/`Failed`, `ActionBindingReloaded` and `Input_ActionManifestReloaded` on manifest loads; `SceneApplicationChanged` and `SceneApplicationStateChanged` for scene apps, whose `GetSceneApplicationState` is Starting, Running or Quitting | `305fd4a` `5e13f8f` |
