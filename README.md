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

`XRIZER_CUSTOM_BINDINGS_DIR` - Directory with controller binding overrides. A file named `<controller_type>.json` (e.g. `knuckles.json`) replaces the game's own bindings and the ones chosen in SteamVR for that controller, or provides them if the game has none. Same format as SteamVR binding files. Overrides are looked for in this directory, then in `~/.config/xrizer/bindings/<app key>/` (under `$XDG_CONFIG_HOME` if set), then, if the variable isn't set, in `xrizer` in the game's working directory. The app key is `steam.app.<id>` for Steam games, or what the app identifies as (e.g. `org.jrbudda.vivecraft.steamvrinput`), lowercase, with characters other than letters, digits, `.`, `_` and `-` replaced by `_`. For games without an action manifest (Skyrim VR, Fallout 4 VR) the file holds bindings for SteamVR's `/actions/legacy` actions; controllers without one keep the built-in mapping. Without an override, a controller the game ships no bindings for gets them converted from the closest controller it does, like SteamVR.

`XRIZER_STEAMVR_BINDINGS` - `0` ignores the bindings chosen for the game in SteamVR, including Workshop bindings, which are used by default. They're read from `steamvr.vrsettings` in `VR_CONFIG_PATH`, the `config` paths in `openvrpaths.vrpath`, or Steam's `config` directory. Includes the legacy bindings chosen for games without an action manifest.

`XRIZER_ADDITIONAL_BINDINGS` - `;` separated list of controller types to load bindings for in addition to the ones the game lists, e.g. `knuckles;oculus_touch`. Each is read from the custom bindings directory, or `bindings_<controller_type>.json` next to the game's action manifest.

Controller models come from SteamVR's own files, with their textures, when SteamVR is installed in a Steam library. Without it, bundled meshes with a plain grey texture are used.

Settings games write through `IVRSettings` are kept in `~/.config/xrizer/settings.json` (under `$XDG_CONFIG_HOME` if set), like SteamVR keeps them in `steamvr.vrsettings`. `supersampleScale` in its `steamvr` section (an area factor, `1.5` is 150 %) and `resolutionScale` (a percentage) in a game's `steam.app.<id>` section scale the render size like SteamVR's resolution settings.

`XRIZER_CONTROLLER_TYPE` - `knuckles` or `oculus_touch` shows the controllers to the game as Index or Touch controllers: they report that controller's identity and poses, and get the game's bindings for it converted, if it has any. The headset keeps its identity. Without the variable, `controller_type` in `~/.config/xrizer/apps/<app key>.json` (same key as above) chooses it per game. `auto` or no choice shows the real controllers. Games without an action manifest always see the real controllers.

`XRIZER_LINEAR_UNORM` - `1` shows 8-bit UNORM textures submitted with `ColorSpace_Auto` as linear instead of sRGB encoded, `0` forces sRGB. Try `1` if a game looks much too dark. Enabled automatically for games known to need it.

`XRIZER_PRESERVE_VIEW_CANTING` - `1` keeps per-eye view rotations from canted-display headsets (e.g. Pimax, Bigscreen Beyond) instead of flattening them to parallel views with a widened FOV. Saves rendering the wider FOV, may confuse games that assume parallel views.

`XRIZER_MAX_TRACKERS` - Max number of generic trackers to expose. Requires a runtime with `XR_MNDX_xdev_space` (Monado, WiVRn). Trackers appear as Vive Trackers, which can confuse some games' controller detection. `0` hides them. Set automatically for games known to need it.

`XRIZER_TRACKER_SERIALS` - `;` separated list of device serials to expose as generic trackers, e.g. to use controllers as FBT trackers.

`XRIZER_PROFILE_PROBE` - `0` skips the startup interaction profile probe, a short throwaway session used to learn the controller type before the game's first frame. Only scene apps probe.

`XRIZER_HMD_PRESENT` - `1` or `0` answers games asking whether a headset is connected before starting VR, instead of briefly connecting to the runtime to find out.

`XRIZER_RECENTER_STANDING` - `1` makes the runtime's recenter (e.g. holding the Quest system button) recenter standing games too, not just seated ones. Off by default, since on a room-scale setup the standing origin is the calibrated play area.

`XRIZER_KILL_ON_QUIT` - `0` keeps a game running when it doesn't quit after the runtime asks it to (e.g. WiVRn's stop button). By default, like SteamVR, it's killed 5 seconds later, or 30 if it acknowledges the quit.

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
