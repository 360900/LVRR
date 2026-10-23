# xrizer-edge

An OpenVR implementation on top of OpenXR. Run OpenVR games through any OpenXR runtime (Monado, WiVRn, etc.) without SteamVR.

xrizer-edge started as a fork of [xrizer](https://github.com/Supreeeme/xrizer) and is developed independently. It includes many upstream PRs that weren't merged there (see [CHANGES.md](CHANGES.md)) plus its own fixes and API coverage. Missing OpenVR functions log a warning and return a safe default instead of crashing the game.

Things will still break. Please open issues here, not on upstream xrizer.

# Download

- [Latest nightly](https://nightly.link/360900/xrizer-edge/workflows/ci/main/xrizer-edge-nightly-release.zip)
- [Releases](https://github.com/360900/xrizer-edge/releases)

Unzip it anywhere.

# Usage

OpenVR games need to be pointed at xrizer-edge instead of SteamVR. Either:

1. Add its path to `$XDG_CONFIG_HOME/openvr/openvrpaths.vrpath`, or
2. Set `VR_OVERRIDE` when launching: `VR_OVERRIDE=/path/to/xrizer-edge %command%` in Steam launch options.
   - This still requires a valid `openvrpaths.vrpath` to exist, though xrizer-edge doesn't need to be in it.

With WiVRn, you can instead set the xrizer-edge path as the OpenVR compatibility library in the WiVRn settings.

## openvrpaths.vrpath

A JSON file OpenVR games read to find the runtime. If you've run SteamVR before, add the xrizer-edge path to the `runtime` list. Otherwise, create `$XDG_CONFIG_HOME/openvr/openvrpaths.vrpath`:

```json
{
    "version": 1,
    "runtime": [
        "/path/to/xrizer-edge"
    ]
}
```

## Steam Linux Runtime

Games running in the Steam Linux Runtime (all Proton 6+ games) are in a container, so paths behave differently:
- Paths in your home directory are always available.
- `/usr` is available at `/run/host/usr`.
- Some paths are unavailable.

Let pressure-vessel import the host's active OpenXR runtime and its socket:

```
PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1 VR_OVERRIDE=/path/to/xrizer-edge %command%
```

The active runtime is `active_runtime.json` in `~/.config/openxr/1` or `/etc/xdg/openxr/1`. 32-bit games (e.g. Half-Life 2: VR Mod) need a 32-bit build of the runtime (e.g. `lib32-wivrn-server`) as `active_runtime.i686.json`. WiVRn sets these up while its server runs.

Only the default Monado and WiVRn sockets are imported; expose others with `PRESSURE_VESSEL_FILESYSTEMS_RW`. Don't also set `XR_RUNTIME_JSON`.

Otherwise, set `XR_RUNTIME_JSON` and use `PRESSURE_VESSEL_FILESYSTEMS_RW` to expose the socket. `XR_RUNTIME_JSON` applies to 32-bit games too, so this only works for 64-bit ones. Example for a systemwide Monado install:

```
XR_RUNTIME_JSON=/run/host/usr/share/openxr/1/openxr_monado.json VR_OVERRIDE=/path/to/xrizer-edge PRESSURE_VESSEL_FILESYSTEMS_RW=$XDG_RUNTIME_DIR/monado_comp_ipc %command%
```

See [Valve's Pressure Vessel docs](https://gitlab.steamos.cloud/steamrt/steam-runtime-tools/-/blob/main/pressure-vessel/wrap.1.md).

# Environment variables

`RUST_LOG` - Log filter, see [env_logger](https://docs.rs/env_logger/latest/env_logger/#enabling-logging). Useful extra targets:
- `openvr_calls` - every OpenVR function called
- `tracked_property` - every tracked device property requested, with device index

If a launcher strips `RUST_LOG`, put the same filter string in a file named `log_filter` in the log directory. It's only read when `RUST_LOG` is unset.

`XRIZER_SYNTHESIZED_VELOCITY` - `1` computes controller linear velocity from position deltas, `0` forces the runtime's. Helps with throwing on runtimes that under-report velocity. Enabled automatically for games known to need it.

`XRIZER_CUSTOM_BINDINGS_DIR` - Directory with controller binding overrides, default `xrizer` in the game's working directory. A file named `<controller_type>.json` (e.g. `knuckles.json`) replaces the game's own bindings for that controller, or provides them if the game has none. Same format as SteamVR binding files.

`XRIZER_ADDITIONAL_BINDINGS` - `;` separated list of controller types to load bindings for in addition to the ones the game lists, e.g. `knuckles;oculus_touch`. Each is read from the custom bindings directory, or `bindings_<controller_type>.json` next to the game's action manifest.

`XRIZER_PRESERVE_VIEW_CANTING` - `1` keeps per-eye view rotations from canted-display headsets (e.g. Pimax, Bigscreen Beyond) instead of flattening them to parallel views with a widened FOV. Saves rendering the wider FOV, may confuse games that assume parallel views.

`XRIZER_MAX_TRACKERS` - Max number of generic trackers to expose. Requires a runtime with `XR_MNDX_xdev_space` (Monado, WiVRn). Trackers appear as Vive Trackers, which can confuse some games' controller detection. `0` hides them. Set automatically for games known to need it.

`XRIZER_TRACKER_SERIALS` - `;` separated list of device serials to expose as generic trackers, e.g. to use controllers as FBT trackers.

`XRIZER_PROFILE_PROBE` - `0` skips the startup interaction profile probe, a short throwaway session used to learn the controller type before the game's first frame. Only scene apps probe.

`XRIZER_HMD_PRESENT` - `1` or `0` answers games asking whether a headset is connected before starting VR, instead of briefly connecting to the runtime to find out.

`XRIZER_RECENTER_STANDING` - `1` makes the runtime's recenter (e.g. holding the Quest system button) recenter standing games too, not just seated ones. Off by default, since on a room-scale setup the standing origin is the calibrated play area.

# Reporting bugs

Attach a log. Logs are in `$XDG_STATE_HOME/xrizer` (or `~/.local/state/xrizer`), one file per process named `xrizer-<timestamp>-<pid>.txt`. The newest is usually the one you want; launchers often start a short-lived process first to check for a headset. Logs older than two days are deleted on startup.

# Building

Needs `glslc` (shaderc) and libclang.

```
cargo xbuild            # debug, output in target/debug
cargo xbuild --release  # release, output in target/release
```

The output directory is the runtime directory.

32-bit games need a 32-bit build in `bin/vrclient.so` (needs a multilib C toolchain):

```
rustup target add i686-unknown-linux-gnu
cargo xbuild --release --target i686-unknown-linux-gnu
cp -L target/i686-unknown-linux-gnu/release/bin/vrclient.so target/release/bin/
```

These builds load the system OpenXR loader, so 32-bit games also need a 32-bit one (e.g. `lib32-openxr`). Releases bundle the loader with `--features static-openxr` instead, which also needs cmake, a C++ compiler and, for 32-bit, C++ multilib and 32-bit X11 headers. That loader also reads OpenXR config from its build directory and fails if that exists but isn't readable, so build packages in a path that won't exist on users' machines.

To install elsewhere, copy `target/release/bin` with `cp -rL` (its libraries are symlinks), and build with `XRIZER_INSTALL_PREFIX` set to the destination so the generated `openvrpaths.vrpath` points there.

The reported version comes from `git describe` via the default `git-version` feature. For builds without a git checkout (distro packages, tarballs), use `--no-default-features` (re-adding what you need, e.g. `--features monado`) and set `XRIZER_VERSION` at build time. `XRIZER_VERSION` always takes precedence when set.

# Contributing

PRs welcome. Please add tests where it makes sense; OpenVR is a large API and games are fickle, so tests are what prevent regressions.

# Credits

- [xrizer](https://github.com/Supreeeme/xrizer) and its contributors, which this project is built on. Merged upstream PRs keep their original authors.
- [OpenComposite](https://gitlab.com/znixian/OpenOVR), which parts of xrizer were based on.

# See also

- [OpenVR](https://github.com/ValveSoftware/openvr)
- [Monado](https://gitlab.freedesktop.org/monado/monado) - Open source OpenXR runtime
- [WiVRn](https://github.com/WiVRn/WiVRn) - OpenXR streaming runtime for standalone headsets, based on Monado
