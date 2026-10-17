# PICKS

Tracking of upstream PRs integrated into xrizer-edge (personal fork of Supreeeme/xrizer).

Baseline: upstream `main` at `63fc7eb` (2026-10-14). Merge commits preserve original authors.

Status: `pending` | `merged` | `superseded-by-<n>` | `blocked` | `dropped` | `revived`

## Phase 1a: quick wins

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 451 | clippy: Swap chunks_exact call for as_chunks | merged | blocks clippy on new toolchains |
| 297 | improve error message when app is using wrong GPU | merged | plain text fix |
| 230 | fire VREvent_Quit when session STOPPING | merged | also handles Quit for OpenVR 0.9.12 |
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
| 430 | HMD identity from controller interaction profile | merged | #365; probe session at init; review by hand (vibe-coded) |
| 429 | input: implement action binding and origin queries | merged | fixes #162 |
| 418 | GetActionOrigins, GetActionBindingInfo, InputProfilePath | superseded-by-429 | follow-up: port InputProfilePath_String tracked property (per-profile input_profile_path exists already) |

## Phase 1c: quirks and poses

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 397 | hl2-vr: multiple fixes | merged | adds src/quirks.rs; prerequisite of 400 |
| 400 | superhot-vr: fix grab, menu button | merged | on top of 397; probably fixes #385 |
| 382 | controller pose transformations | merged | #303; supersedes 234; time-keyed raw pose cache + per-call transform |

## Phase 1d: fix CI first

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 338 | XR_EXT_hand_tracking_data_source skeletal level | merged | CI fixed (xr::sys::Handle import); unblocks 261 |
| 388 | overlays, skybox, color conversion, canting | merged | CI fixed (unused PI import); high value (#79, racing) |
| 440 | Steamframe profile | merged | adapted to HmdProperties/input_profile_path/pose_transformation; pose transforms TODO |
| 41 | action set priorities and device restriction | merged | needs VRChat validation on edge build |

## Phase 2: rebase conflicted

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 261 | hand tracking with no interaction profile and/or grip pose | merged | ported synthesised grip pose + pre-created hand devices + VRLinkHand fallback; closes #165 |
| 401 | log to per-process files, crash reports | merged | per-process log files, crash collection, 32-bit loader hint (#450); partial #73 |
| 130 | WMR controller support | merged | WMR profiles adapted to static trait; registered for HolographicController/HPMotionController; fakexr exposes HP + Samsung extensions |
| 330 | Background and Utility application types | merged | headless sessions (XR_MND_headless + XR_KHR_convert_timespec_time), headless graphics backend, OpenXrData::new(injector, application_type) |
| 193 | New interface versions | merged | overlay name/raw texture/thumbnail additions; newer 1.23.8 header; fixes #177 UEVR |
| 229 | libmonado battery status | pending | closes #26 |
| 69 | Skeletal Summary | pending | likely superseded by merged 313 |
| 168 | Hitman | pending | by Supreeeme, empty body |
| 87 | IVRDebug stub and IVROverlay014 stub | pending | old |
| 234 | Support all the BoundPoseTypes | superseded-by-382 | draft |
| 337 | input profile path support | superseded-by-429 | draft |

## Phase 3: fork hardening

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 442 | build: inherit workspace rust-version | pending | closed by AI policy upstream (#433) |
| 443 | skip bindings with mismatched action type | pending | panic -> skip; #334? |
| 444 | clientcore: don't panic in Cleanup | pending | #423, #439 |
| 445 | compositor: replace todo!() with warnings | pending | #391? |
| 446 | compositor: implement GetFrameTimings | pending | |
