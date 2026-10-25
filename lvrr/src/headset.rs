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
/// How long the pairing question waits for an answer without a terminal
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

/// A yes/no question on the terminal, or as a notification with a button
fn ask(question: &str, button: &str) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() {
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

/// `headsets`, `rename` and `unpair` are wivrnctl's
pub fn wivrnctl(command: &str, args: &[OsString]) -> String {
    let root = match install_root() {
        Ok(root) => root,
        Err(e) => return e,
    };
    let mut cmd = Command::new(root.join("bin/wivrnctl"));
    cmd.arg(if command == "headsets" {
        "list-paired"
    } else {
        command
    })
    .args(args);
    exec(cmd)
}

/// Connects the headset on USB to the server through an adb tunnel, like the
/// dashboard's "Connect by USB"
pub fn usb(args: &[String]) -> Result<(), String> {
    let adb = adb()?;
    let serial = match args {
        [] => only_device(&adb)?,
        [serial] => serial.clone(),
        _ => return Err("usb: expected at most one device serial".to_string()),
    };
    let package = app_package(&adb, &serial)?
        .ok_or("LVRR isn't installed on the headset, run `lvrr install`")?;
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

/// Installs the app on the headset plugged in by USB. Without an argument,
/// the APK this install ships, if it does.
pub fn install(args: &[OsString]) -> Result<(), String> {
    let apk = match args {
        [] => bundled_apk()?,
        [apk] => PathBuf::from(apk),
        _ => return Err("install: expected at most one APK".to_string()),
    };
    if !apk.is_file() {
        return Err(format!("{} not found", apk.display()));
    }
    let adb = adb()?;
    let serial = only_device(&adb)?;
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

fn bundled_apk() -> Result<PathBuf, String> {
    let path = install_root()?.join("share/lvrr/lvrr.apk");
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "this install has no APK at {}, pass one, such as a CI build",
            path.display()
        ))
    }
}

fn adb() -> Result<PathBuf, String> {
    crate::which("adb").ok_or_else(|| "adb isn't installed, install android-tools".to_string())
}

/// The serial of the one device on USB that allows debugging
fn only_device(adb: &Path) -> Result<String, String> {
    let output = Command::new(adb)
        .arg("devices")
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("running adb: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let devices: Vec<(&str, &str)> = text
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once('\t'))
        .collect();
    let ready: Vec<&str> = devices
        .iter()
        .filter(|(_, state)| *state == "device")
        .map(|(serial, _)| *serial)
        .collect();
    match ready[..] {
        [serial] => Ok(serial.to_string()),
        [] if devices.iter().any(|(_, state)| *state == "unauthorized") => {
            Err("allow USB debugging in the headset first".to_string())
        }
        [] => {
            Err("no headset on USB, plug it in and allow USB debugging in the headset".to_string())
        }
        _ => Err(format!(
            "several devices on USB, pass one of: {}",
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
/// computed here
pub fn steam() -> Result<(), String> {
    let root = install_root()?;
    let running = bus::session()
        .ok()
        .filter(|conn| matches!(server::state(conn, &root), State::Ours));
    let command = match running {
        Some(conn) => Server::new(&conn).get::<String>("SteamCommand")?,
        None => steam_command(&root),
    };
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
