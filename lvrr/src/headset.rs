//! Headset commands: pairing, USB, installing the app and Steam's launch
//! options.

use std::env;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::bus::{self, Server};
use crate::server::{self, State};
use crate::{checks, exec, install_root, report};

/// How long a PIN works, like the WiVRn dashboard's
const PAIRING_SECS: i32 = 120;
/// How long the old session gets to end before pairing gives up
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the pairing question waits for an answer as a notification
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// The app's package names, the release build first
const PACKAGES: [&str; 2] = ["io.github.lvrr", "io.github.lvrr.debug"];

pub fn pair() -> Result<(), String> {
    // from the app menu, the window asks and shows the PIN
    if !crate::interactive() && crate::window_available() {
        return crate::open(&["--pair"]);
    }
    let conn = server::ensure_running()?;
    let server = Server::new(&conn);
    if server.get::<bool>("HeadsetConnected")? || server.get::<bool>("SessionRunning")? {
        let question = "Pairing a new headset ends the current session and closes its apps.";
        if !ask(question, "Disconnect and pair")? {
            report("Pairing canceled", "");
            return Ok(());
        }
        server.call("StopSession")?;
    }
    // The server refuses to pair while a session runs or a headset connects
    let deadline = Instant::now() + SESSION_END_TIMEOUT;
    let pin = loop {
        if !server.get::<bool>("SessionRunning")? {
            let pin = server.enable_pairing(PAIRING_SECS)?;
            if !pin.is_empty() {
                break pin;
            }
        }
        if Instant::now() > deadline {
            return Err("the headset session didn't end, so pairing couldn't start. If the headset reconnected, quit LVRR on it and try again.".to_string());
        }
        sleep(Duration::from_millis(250));
    };
    // Without a terminal, the server's own PIN notification shows it
    println!("PIN: {pin}");
    eprintln!(
        "Valid for {} minutes. Open LVRR on the new headset and pick this PC.",
        PAIRING_SECS / 60
    );
    Ok(())
}

/// A yes/no question on stderr, answered with a line on stdin, which also
/// works over ssh without a terminal; no answer is a no. Without any terminal
/// but with a display, as from the desktop entry when the window is missing,
/// it asks with a notification button instead.
fn ask(question: &str, button: &str) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() && !crate::interactive() && crate::display_available() {
        return bus::confirm("Pair a new headset?", question, button, CONFIRM_TIMEOUT);
    }
    eprint!("{question} {button}? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| format!("reading the answer: {e}"))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `headsets`, `rename` and `unpair` run wivrnctl on LVRR's server, started
/// first if needed, as for pair. Not on another WiVRn server: unpairing there
/// would keep the headset's LVRR settings in lvrr-headsets.json.
pub fn wivrnctl(command: &str, args: &[OsString]) -> String {
    let args = match wivrnctl_args(command, args) {
        Ok(args) => args,
        Err(e) => return e,
    };
    let root = match install_root() {
        Ok(root) => root,
        Err(e) => return e,
    };
    if let Err(e) = server::ensure_running() {
        return e;
    }
    let mut cmd = Command::new(root.join("bin/wivrnctl"));
    cmd.args(args);
    exec(cmd)
}

/// wivrnctl's arguments for `command`. Checked here, as wivrnctl's help would
/// name its own commands.
fn wivrnctl_args(command: &str, args: &[OsString]) -> Result<Vec<OsString>, String> {
    // wivrnctl numbers headsets from 1, as `lvrr headsets` lists them
    let id = |arg: &OsString| {
        arg.to_str()
            .and_then(|id| id.parse::<u32>().ok())
            .filter(|id| *id >= 1)
            .map(|id| OsString::from(id.to_string()))
    };
    // `--` keeps a name that starts with '-' from being taken for an option
    let argv = match (command, args) {
        ("headsets", []) => Some(vec!["list-paired".into()]),
        ("headsets", [keys]) if keys == "-k" || keys == "--keys" => {
            Some(vec!["list-paired".into(), "--keys".into()])
        }
        ("rename", [headset, name]) => {
            id(headset).map(|id| vec!["rename".into(), "--".into(), id, name.clone()])
        }
        ("unpair", [headset]) => id(headset).map(|id| vec!["unpair".into(), "--".into(), id]),
        _ => None,
    };
    argv.ok_or_else(|| {
        match command {
            "headsets" => "headsets: expected no argument or --keys",
            "rename" => "rename: expected ID NAME, with an ID from `lvrr headsets`",
            _ => "unpair: expected an ID from `lvrr headsets`",
        }
        .to_string()
    })
}

/// Connects the headset on USB to the server through an adb tunnel, like the
/// dashboard's "Connect by USB"
pub fn usb(args: &[String]) -> Result<(), String> {
    let adb = adb()?;
    let wanted = match args {
        [] => None,
        [serial] => Some(serial.as_str()),
        _ => return Err("usb: expected at most one device serial".to_string()),
    };
    let serial = only_device(&adb, wanted, true)?;
    let package = app_package(&adb, &serial)?.ok_or_else(|| {
        let arg = wanted
            .map(|serial| format!(" {serial}"))
            .unwrap_or_default();
        format!("LVRR isn't installed on the headset, install it with `lvrr install APK{arg}`")
    })?;
    let conn = server::ensure_running()?;
    let server = Server::new(&conn);
    let port = crate::server_port(&crate::read_config());
    let tcp = format!("tcp:{port}");
    adb_run(&adb, &serial, &["reverse", &tcp, &tcp])?;
    let pin = if server.get::<bool>("PairingEnabled").unwrap_or(false) {
        server.get::<String>("Pin").unwrap_or_default()
    } else {
        String::new()
    };
    let uri = if pin.is_empty() {
        format!("wivrn+tcp://127.0.0.1:{port}")
    } else {
        format!("wivrn+tcp://:{pin}@127.0.0.1:{port}")
    };
    adb_run(
        &adb,
        &serial,
        &[
            "shell",
            "am",
            "start",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            &uri,
            package,
        ],
    )?;
    report("Connecting the headset by USB", "");
    Ok(())
}

/// `install APK [SERIAL]`: installs the app on the headset plugged in by USB.
/// No install ships the APK, CI builds it.
pub fn install(args: &[OsString]) -> Result<(), String> {
    let (apk, wanted) = match args {
        [apk] => (PathBuf::from(apk), None),
        [apk, serial] => (PathBuf::from(apk), Some(serial.to_string_lossy())),
        [] => {
            return Err(
                "install: pass the headset APK, such as the lvrr-apk-release build from CI"
                    .to_string(),
            );
        }
        _ => return Err("install: expected an APK and at most one device serial".to_string()),
    };
    if !apk.is_file() {
        return Err(format!("{} not found", apk.display()));
    }
    let adb = adb()?;
    // adb installs over Wi-Fi too
    let serial = only_device(&adb, wanted.as_deref(), false)?;
    let status = Command::new(&adb)
        .args(["-s", &serial, "install", "-r"])
        .arg(&apk)
        .status()
        .map_err(|e| format!("running adb: {e}"))?;
    if !status.success() {
        return Err(format!("adb install {}: {status}", apk.display()));
    }
    report("LVRR installed on the headset", "");
    Ok(())
}

fn adb() -> Result<PathBuf, String> {
    crate::which("adb").ok_or_else(|| "adb isn't installed, install android-tools".to_string())
}

/// The serial of the device to use: `wanted`, else ANDROID_SERIAL, which adb
/// ignores when given -s, else the one device on USB that allows debugging.
/// The devices adb reaches over the network count only when nothing is on
/// USB, and never with `usb_only`.
fn only_device(adb: &Path, wanted: Option<&str>, usb_only: bool) -> Result<String, String> {
    let from_env = env::var("ANDROID_SERIAL")
        .ok()
        .filter(|serial| !serial.is_empty());
    let output = Command::new(adb)
        .arg("devices")
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("running adb: {e}"))?;
    pick_device(
        &String::from_utf8_lossy(&output.stdout),
        wanted,
        from_env.as_deref(),
        usb_only,
    )
}

/// adb connect and tcpip list a device as host:port, wireless debugging by an
/// mDNS name. A tunnel through those goes over Wi-Fi rather than the cable, so
/// the server's USB connection skips them too (lvrr_usb.cpp on_devices).
fn network_serial(serial: &str) -> bool {
    serial.contains(':') || serial.contains("._adb")
}

/// Picks the device from `adb devices` output, or says why none is usable.
/// `env`, ANDROID_SERIAL, counts when no serial is `wanted`; an error that
/// names it says where it came from, as the user didn't type it.
fn pick_device(
    list: &str,
    wanted: Option<&str>,
    env: Option<&str>,
    usb_only: bool,
) -> Result<String, String> {
    let serial = wanted.or(env);
    // with a serial, only its rows are left
    let (on_network, on_usb): (Vec<(&str, &str)>, _) = list
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once('\t'))
        .filter(|(row, _)| serial.is_none_or(|serial| *row == serial))
        .partition(|(serial, _)| network_serial(serial));
    if let Some(serial) = serial.filter(|_| usb_only && !on_network.is_empty()) {
        let from = if wanted.is_none() {
            " (from ANDROID_SERIAL)"
        } else {
            ""
        };
        return Err(format!(
            "{serial}{from} is connected over the network, plug the headset in by USB and pass its serial from `adb devices`"
        ));
    }
    // install goes over the network only when nothing is on USB, so a headset
    // listed both ways takes the cable
    let (devices, place) = if !usb_only && on_usb.is_empty() {
        (&on_network, "over the network")
    } else {
        (&on_usb, "on USB")
    };
    // what usb leaves out, for its error
    let network_ready: Vec<&str> = on_network
        .iter()
        .filter(|(_, state)| *state == "device")
        .map(|(serial, _)| *serial)
        .collect();
    let ready: Vec<&str> = devices
        .iter()
        .filter(|(_, state)| *state == "device")
        .map(|(serial, _)| *serial)
        .collect();
    let any = |test: fn(&str) -> bool| devices.iter().any(|(_, state)| test(state));
    match ready[..] {
        [serial] => Ok(serial.to_string()),
        [] if any(|state| state == "unauthorized" || state == "authorizing") => {
            Err("allow USB debugging in the headset first".to_string())
        }
        [] if any(|state| state.starts_with("no permissions")) => Err(
            "adb has no permission to open the headset, install your distribution's udev rules for adb (such as android-udev) and plug it in again"
                .to_string(),
        ),
        [] if any(|state| state == "offline") => {
            Err("the headset's adb connection is offline, plug it in again".to_string())
        }
        [] => Err(match (devices.first(), wanted, env) {
            (Some((_, state)), _, _) => format!("adb can't use the device {place}: {state}"),
            (None, Some(serial), _) => format!("no device {serial} on USB, see `adb devices`"),
            (None, None, Some(serial)) => format!(
                "no device {serial} (from ANDROID_SERIAL) on USB, unset it or pass the headset's serial from `adb devices`"
            ),
            (None, None, None) if !network_ready.is_empty() => format!(
                "adb reaches the headset only over the network ({}), plug it in by USB",
                network_ready.join(", ")
            ),
            (None, None, None) => {
                "no headset on USB, plug it in and allow USB debugging in the headset".to_string()
            }
        }),
        _ => Err(format!(
            "several devices {place}, pass one of: {}",
            ready.join(", ")
        )),
    }
}

fn app_package(adb: &Path, serial: &str) -> Result<Option<&'static str>, String> {
    let output = Command::new(adb)
        .args([
            "-s",
            serial,
            "shell",
            "pm",
            "list",
            "packages",
            "io.github.lvrr",
        ])
        .output()
        .map_err(|e| format!("running adb: {e}"))?;
    // A failed check isn't a missing app, whose hint would send the user to install it
    if !output.status.success() {
        return Err(format!(
            "checking for LVRR on {serial}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let installed: Vec<&str> = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .collect();
    Ok(PACKAGES.into_iter().find(|p| installed.contains(p)))
}

fn adb_run(adb: &Path, serial: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(adb)
        .args(["-s", serial])
        .args(args)
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("running adb: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("adb {}: {status}", args.join(" ")))
    }
}

/// The launch options Steam VR games need: the running server's, or the same
/// computed here, with the OpenXR import when the running Steam lacks it
pub fn steam() -> Result<(), String> {
    let root = install_root()?;
    let running = bus::session()
        .ok()
        .filter(|conn| matches!(server::state(conn, &root), State::Ours));
    let command = match running {
        Some(conn) => Server::new(&conn).get::<String>("SteamCommand")?,
        None => steam_command(&root),
    };
    let command = with_openxr_import(command, checks::steam_client_env());
    if command.is_empty() {
        eprintln!("Steam games need no launch options");
    } else {
        println!("{command}");
    }
    Ok(())
}

/// What the server's `steam_command()` computes
fn steam_command(root: &Path) -> String {
    let mut command = String::new();
    if env::var("PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES").as_deref() != Ok("1") {
        command.push_str("PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1 ");
    }
    let home = env::var("HOME").unwrap_or_default();
    if let Some(path) = checks::openvr_compat_path(root, &crate::read_config()) {
        let text = path.to_string_lossy();
        // /usr can't be shared into Steam's container
        if text.starts_with("/usr") {
            command.push_str(&format!("VR_OVERRIDE=/run/host{text} "));
        } else if text.starts_with("/var") && (home.is_empty() || !text.starts_with(&home)) {
            let dir = path
                .ancestors()
                .find(|dir| {
                    dir.file_name()
                        .is_some_and(|n| n == "io.github.wivrn.wivrn")
                })
                .unwrap_or(&path);
            command.push_str(&format!(
                "PRESSURE_VESSEL_FILESYSTEMS_RW={} ",
                dir.display()
            ));
        }
    }
    if !command.is_empty() {
        command.push_str("%command%");
    }
    command
}

/// `command` with `PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1` first when the
/// running Steam lacks it (`steam`, see checks::steam_client_env). Its games get
/// its environment, whatever the server's or this process's has. The dashboard's
/// PC > Server applies the same rule (lvrr-common's steam_env).
fn with_openxr_import(command: String, steam: Option<bool>) -> String {
    let set = format!("{}=1", checks::OPENXR_IMPORT_VAR);
    if steam != Some(false) || command.contains(&set) {
        command
    } else if command.is_empty() {
        format!("{set} %command%")
    } else {
        format!("{set} {command}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_device() {
        let list = |rows: &str| format!("List of devices attached\n{rows}\n");
        let mixed = list("1WMHH\tdevice\nR58M\tunauthorized\n");
        assert_eq!(
            pick_device(&mixed, None, None, true),
            Ok("1WMHH".to_string())
        );
        assert_eq!(
            pick_device(&mixed, Some("R58M"), None, true),
            Err("allow USB debugging in the headset first".to_string())
        );
        assert_eq!(
            pick_device(&mixed, Some("R58"), None, true),
            Err("no device R58 on USB, see `adb devices`".to_string())
        );
        // the headset on the cable and over Wi-Fi: usb and install take the
        // cable, install also takes the network serial when named
        let two = list("1WMHH\tdevice\n192.168.1.5:5555\tdevice\n");
        assert_eq!(pick_device(&two, None, None, true), Ok("1WMHH".to_string()));
        assert_eq!(
            pick_device(&two, None, None, false),
            Ok("1WMHH".to_string())
        );
        assert_eq!(
            pick_device(&two, Some("1WMHH"), None, false),
            Ok("1WMHH".to_string())
        );
        assert_eq!(
            pick_device(&two, Some("192.168.1.5:5555"), None, false),
            Ok("192.168.1.5:5555".to_string())
        );
        let network = "192.168.1.5:5555 is connected over the network";
        assert!(
            pick_device(&two, Some("192.168.1.5:5555"), None, true)
                .is_err_and(|e| e.starts_with(network))
        );
        assert_eq!(
            pick_device(&two, Some("192.168.1.5:5556"), None, true),
            Err("no device 192.168.1.5:5556 on USB, see `adb devices`".to_string())
        );
        let plugged = list("1WMHH\tdevice\nR58M\tdevice\n");
        assert_eq!(
            pick_device(&plugged, None, None, true),
            Err("several devices on USB, pass one of: 1WMHH, R58M".to_string())
        );
        // ANDROID_SERIAL picks the device unless a serial is passed; an error
        // that names it says where it came from
        let phone = Some("R58M");
        assert_eq!(
            pick_device(&plugged, None, phone, true),
            Ok("R58M".to_string())
        );
        let headset = list("1WMHH\tdevice\n");
        assert_eq!(
            pick_device(&headset, Some("1WMHH"), phone, true),
            Ok("1WMHH".to_string())
        );
        let unset = "no device R58M (from ANDROID_SERIAL) on USB, unset it or pass the headset's serial from `adb devices`";
        assert_eq!(
            pick_device(&headset, None, phone, true),
            Err(unset.to_string())
        );
        let from_env = "192.168.1.5:5555 (from ANDROID_SERIAL) is connected over the network";
        assert!(
            pick_device(&two, None, Some("192.168.1.5:5555"), true)
                .is_err_and(|e| e.starts_with(from_env))
        );
        // adb over Wi-Fi only, also wireless debugging's mDNS name: usb's error
        // names them, install uses them
        let mdns = "adb-1WMHH-AbCdEf._adb-tls-connect._tcp";
        let wifi = list(&format!("192.168.1.5:5555\tdevice\n{mdns}\tdevice\n"));
        let only = "adb reaches the headset only over the network";
        assert_eq!(
            pick_device(&wifi, None, None, true),
            Err(format!(
                "{only} (192.168.1.5:5555, {mdns}), plug it in by USB"
            ))
        );
        assert_eq!(
            pick_device(&wifi, None, None, false),
            Err(format!(
                "several devices over the network, pass one of: 192.168.1.5:5555, {mdns}"
            ))
        );
        assert_eq!(
            pick_device(&wifi, Some(mdns), None, false),
            Ok(mdns.to_string())
        );
        let one = list("192.168.1.5:5555\tdevice\n");
        assert_eq!(
            pick_device(&one, None, None, false),
            Ok("192.168.1.5:5555".to_string())
        );
        // an offline adb connect entry, maybe stale, isn't the headset on Wi-Fi
        let old = list("192.168.1.5:5555\toffline\n");
        assert!(
            pick_device(&old, None, None, true).is_err_and(|e| e.starts_with("no headset on USB"))
        );
        let error =
            |state: &str| pick_device(&list(&format!("1WMHH\t{state}\n")), None, None, true);
        let no_permissions = "no permissions (missing udev rules? user is in the plugdev group); see [http://developer.android.com/tools/device.html]";
        assert!(error(no_permissions).is_err_and(|e| e.starts_with("adb has no permission")));
        assert!(error("offline").is_err_and(|e| e.contains("offline, plug it in again")));
        assert_eq!(
            error("authorizing"),
            Err("allow USB debugging in the headset first".to_string())
        );
        assert_eq!(
            error("recovery"),
            Err("adb can't use the device on USB: recovery".to_string())
        );
        assert!(
            pick_device(&list(""), None, None, true)
                .is_err_and(|e| e.starts_with("no headset on USB"))
        );
    }

    #[test]
    fn steam_openxr_import() {
        let set = "PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1";
        let with = |command: &str, steam| with_openxr_import(command.to_string(), steam);
        // the running Steam lacks it
        assert_eq!(with("", Some(false)), format!("{set} %command%"));
        let compat = "VR_OVERRIDE=/run/host/usr/lib/xrizer %command%";
        assert_eq!(with(compat, Some(false)), format!("{set} {compat}"));
        let server = format!("{set} {compat}");
        assert_eq!(with(&server, Some(false)), server);
        // it has it, or none runs
        assert_eq!(with("", Some(true)), "");
        assert_eq!(with(compat, None), compat);
    }

    #[test]
    fn checks_wivrnctl_args() {
        let run = |command: &str, args: &[&str]| -> Result<Vec<String>, String> {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let argv = wivrnctl_args(command, &args)?;
            Ok(argv
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect())
        };
        let ok = |argv: &[&str]| -> Result<Vec<String>, String> {
            Ok(argv.iter().map(|a| a.to_string()).collect())
        };
        assert_eq!(run("headsets", &[]), ok(&["list-paired"]));
        assert_eq!(run("headsets", &["-k"]), ok(&["list-paired", "--keys"]));
        assert_eq!(
            run("rename", &["2", "-Quest"]),
            ok(&["rename", "--", "2", "-Quest"])
        );
        assert_eq!(run("unpair", &["+1"]), ok(&["unpair", "--", "1"]));
        let wrong: [(&str, &[&str]); 7] = [
            ("headsets", &["--keys", "x"]),
            ("rename", &["1"]),
            ("rename", &["0", "Quest"]),
            ("rename", &["one", "Quest"]),
            ("unpair", &[]),
            ("unpair", &["-1"]),
            ("unpair", &["1", "2"]),
        ];
        for (command, args) in wrong {
            assert!(
                run(command, args).is_err_and(|e| e.starts_with(&format!("{command}: expected"))),
                "{command} {args:?}"
            );
        }
    }
}
