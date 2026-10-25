# LVRR

Linux VR Runtime. Stream VR from a Linux PC to a standalone headset, run SteamVR games without SteamVR and use your desktop in VR, with one app and one headset APK.

LVRR combines:

- [WiVRn](https://github.com/WiVRn/WiVRn) (`wivrn/`): streaming server and headset client
- [xrizer-edge](https://github.com/360900/xrizer-edge) (`xrizer/`): OpenVR on top of OpenXR
- [wayvr](https://github.com/wayvr-org/wayvr) (`wayvr/`): desktop overlay

# Install

Arch Linux:

```sh
cd packaging/arch && makepkg -si
```

This installs to `/opt/lvrr` and adds `lvrr` to the app menu. It doesn't conflict with system WiVRn or wayvr packages.

Other distributions: install the build dependencies of WiVRn, xrizer and wayvr, then:

```sh
cargo xtask build --release --i686 --prefix $HOME/.local/opt/lvrr
cargo xtask install
```

The prefix is fixed at build time. Run `$HOME/.local/opt/lvrr/bin/lvrr`.

The headset APK is built by CI.

# Usage

Run `lvrr` (or LVRR from the app menu) to start the server in the background and open LVRR's window, with pairing and the headset and server settings; closing it leaves the server and a tray icon. Open LVRR on the headset and pick this PC; the first time, enter the PIN the PC shows. Once connected, the settings are in the dashboard in VR too. OpenVR games use the bundled xrizer.

LVRR keeps its server settings in `~/.config/lvrr/wivrn-server.json`. Pairing is shared with WiVRn.

- `lvrr stop`, `lvrr status`, `lvrr logs`
- `lvrr pair`: pair another headset
- `lvrr headsets`, `lvrr rename`, `lvrr unpair`: manage paired headsets
- `lvrr usb`: connect the headset plugged in by USB; `lvrr install APK` installs the app
- `lvrr steam`: launch options for Steam VR games, if they need any
- `lvrr doctor`: check that the headset can reach this PC; `--fix` repairs what it can, the first start offers it too
- `lvrr server`: the server in the foreground

# Development

```sh
cargo xtask run
```

This builds into `target/stage`, installs there and starts its `lvrr`. Build options such as `--i686` go first, arguments after `--` go to `lvrr`.

`cargo xtask apk --debug` builds an installable headset APK and needs the Android SDK and NDK. Upstream changes are merged as described in [CHANGES.md](CHANGES.md), followed by `cargo xtask rebrand`.

# License

GPL-3.0. Each component keeps its own license files.
