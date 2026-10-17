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
| 419 | System button | pending | prerequisite of 440 |
| 422 | input: support Vector2 binding inversion | pending | |
| 406 | action set name invalid chars + string/number json | pending | Halo CEVR #390 |
| 394 | system: HMD tracking system/model/manufacturer strings | pending | Serious Sam 3, #232 |
| 430 | HMD identity from controller interaction profile | pending | #365; review by hand (vibe-coded) |
| 429 | input: implement action binding and origin queries | pending | prefer over 418; fixes #162 |
| 418 | GetActionOrigins, GetActionBindingInfo, InputProfilePath | pending | diff vs 429, keep better one as reference |

## Phase 1c: quirks and poses

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 397 | hl2-vr: multiple fixes | pending | adds src/quirks.rs; prerequisite of 400 |
| 400 | superhot-vr: fix grab, menu button | blocked | rebase on 397; probably fixes #385 |
| 382 | controller pose transformations | pending | #303; supersedes 234 |

## Phase 1d: fix CI first

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 338 | XR_EXT_hand_tracking_data_source skeletal level | blocked | CI red; unblocks 261 |
| 388 | overlays, skybox, color conversion, canting | blocked | CI red; high value (#79, racing) |
| 440 | Steamframe profile | blocked | needs 419; draft |
| 41 | action set priorities and device restriction | blocked | draft, large; merge last, test VRChat |

## Phase 2: rebase conflicted

| PR | Title | Status | Notes |
|----|-------|--------|-------|
| 261 | hand tracking with no interaction profile and/or grip pose | blocked | needs 338; closes #165 |
| 401 | log to per-process files, crash reports | blocked | needs 397; partial #73 |
| 130 | WMR controller support | pending | |
| 330 | Background and Utility application types | pending | |
| 193 | New interface versions | pending | fixes #177, UEVR |
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
