//! Entry point of the LVRR desktop app.
//!
//! Runs this install's WiVRn server in the background with LVRR's own server
//! config, so settings stay apart from a system WiVRn. The config starts wayvr
//! when a headset connects, which is where LVRR's interface lives, and WiVRn
//! finds this install's xrizer first on its OpenVR compat search path.
//!
//! The desktop entry runs `lvrr`, `lvrr pair` and `lvrr stop` without a
//! terminal, so results and errors also go to desktop notifications then.

mod bus;
mod checks;
mod doctor;
mod headset;
mod server;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: lvrr [COMMAND] [ARGS...]

  start           start LVRR's server in the background, or show its status (default)
  stop            stop the server
  status          show the server, headset and pairing state
  logs [ARGS]     show the server log; ARGS such as -f go to journalctl, or tail without systemd
  server [ARGS]   run the server in the foreground; ARGS go to wivrn-server
  pair            let a new headset connect and show its PIN
  headsets        list paired headsets with their ids
  rename ID NAME  rename a paired headset
  unpair ID       remove a paired headset
  usb [SERIAL]    connect the headset plugged in by USB
  install [APK]   install the headset app over USB
  steam           show the Steam launch options for VR games
  desktop [ARGS]  start wayvr on the running server
  doctor          check that the headset can reach this PC; --fix repairs what it can,
                  --json prints the report as JSON
  setup-firewall  open the server port and mDNS in ufw or firewalld; needs root,
                  `doctor --fix` runs it with pkexec
  --version       print the version";

/// Started by the server when a headset connects. A bare name, found on the
/// PATH that `server` sets, so the config keeps working if LVRR moves.
const DEFAULT_CONFIG: &str = "{\n\t\"application\": [\"wayvr\", \"--openxr\", \"--wait\"]\n}\n";

/// The server's port when the config doesn't set one
pub const DEFAULT_PORT: u16 = 9757;

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.to_string_lossy().into_owned(), rest),
        None => ("start".to_string(), &[][..]),
    };
    let result = match command.as_str() {
        "start" => no_args(&command, rest).and_then(|()| server::start()),
        "stop" => no_args(&command, rest).and_then(|()| server::stop()),
        "status" => {
            return match no_args(&command, rest).and_then(|()| server::status()) {
                Ok(code) => code,
                Err(e) => fail(&e),
            };
        }
        "logs" => Err(server::logs(rest)),
        "server" => Err(server::foreground(rest)),
        "run-server" => Err(server::background(rest)),
        "pair" => no_args(&command, rest).and_then(|()| headset::pair()),
        "headsets" | "rename" | "unpair" => Err(headset::wivrnctl(&command, rest)),
        "usb" => headset::usb(&strings(rest)),
        "install" => headset::install(rest),
        "steam" => no_args(&command, rest).and_then(|()| headset::steam()),
        "desktop" => Err(desktop(rest)),
        "doctor" => return doctor::main(&strings(rest)),
        "setup-firewall" => doctor::setup_firewall(),
        "--version" => {
            println!("lvrr {}", option_env!("LVRR_VERSION").unwrap_or("unknown"));
            Ok(())
        }
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("unknown command {command}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(error: &str) -> ExitCode {
    eprintln!("lvrr: {error}");
    if !interactive() {
        bus::notify("LVRR", error);
    }
    ExitCode::FAILURE
}

fn no_args(command: &str, args: &[OsString]) -> Result<(), String> {
    match args.first() {
        Some(arg) => Err(format!(
            "{command}: unexpected argument {}",
            arg.to_string_lossy()
        )),
        None => Ok(()),
    }
}

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// Whether someone reads stderr. From the desktop entry nobody does, so
/// results go to notifications too.
pub fn interactive() -> bool {
    std::io::stderr().is_terminal()
}

/// Prints a result, and shows it as a notification when nobody reads stderr
pub fn report(summary: &str, body: &str) {
    if body.is_empty() {
        eprintln!("{summary}");
    } else {
        eprintln!("{summary}\n{body}");
    }
    if !interactive() {
        bus::notify(summary, body);
    }
}

fn desktop(args: &[OsString]) -> String {
    let root = match install_root() {
        Ok(root) => root,
        Err(e) => return e,
    };
    let mut cmd = Command::new(root.join("bin/wayvr"));
    cmd.arg("--openxr").args(args);
    exec(cmd)
}

/// Runs `cmd` in place of this process; returns only on error
pub fn exec(mut cmd: Command) -> String {
    let err = cmd.exec();
    format!("running {}: {err}", cmd.get_program().to_string_lossy())
}

/// The install prefix: `LVRR_ROOT`, or two levels above the real executable,
/// so a `/usr/bin/lvrr` symlink still finds `/opt/lvrr`.
pub fn install_root() -> Result<PathBuf, String> {
    if let Some(root) = env::var_os("LVRR_ROOT") {
        return Ok(root.into());
    }
    let exe = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("locating the lvrr executable: {e}"))?;
    exe.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{} is not in <prefix>/bin", exe.display()))
}

/// PATH with `bin` first, so programs the server starts by name are this
/// install's.
pub fn path_with(bin: &Path) -> Result<OsString, String> {
    let rest = env::var_os("PATH").unwrap_or_default();
    env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&rest)))
        .map_err(|e| format!("building PATH: {e}"))
}

/// Finds `program` on PATH
pub fn which(program: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// LVRR's WiVRn server config, which the server saves settings to as well
pub fn config_path() -> Result<PathBuf, String> {
    Ok(config_home()?.join("lvrr/wivrn-server.json"))
}

/// The server config, created with LVRR's defaults on first use
pub fn server_config() -> Result<PathBuf, String> {
    let path = config_path()?;
    if path.exists() {
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => f
            .write_all(DEFAULT_CONFIG.as_bytes())
            .map_err(|e| format!("writing {}: {e}", path.display()))?,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => (),
        Err(e) => return Err(format!("creating {}: {e}", path.display())),
    }
    Ok(path)
}

/// The server config's contents; null when it doesn't exist or can't be read
pub fn read_config() -> serde_json::Value {
    config_path()
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_default()
}

pub fn server_port(config: &serde_json::Value) -> u16 {
    config
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .unwrap_or(DEFAULT_PORT)
}

fn config_home() -> Result<PathBuf, String> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// An XDG base directory, or its default under HOME
pub fn xdg_dir(var: &str, default: &str) -> Result<PathBuf, String> {
    if let Some(dir) = env::var_os(var).map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir);
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(default))
        .ok_or_else(|| format!("neither {var} nor HOME is set"))
}
