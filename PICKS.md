# PICKS

Tracking of upstream PRs integrated into xrizer-edge (personal fork of Supreeeme/xrizer).

Baseline: upstream `main` at `63fc7eb` (2026-10-14). Merge commits preserve original authors.

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
| 382 | controller pose transformations | merged | #303; supersedes 234; time-keyed raw pose cache + per-call transform |

## Phase 1d: fix CI first

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 338 | XR_EXT_hand_tracking_data_source skeletal level | merged | CI fixed (xr::sys::Handle import); unblocks 261 |
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
