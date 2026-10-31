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
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{ErrorKind, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: lvrr [COMMAND] [ARGS...]

  (none)          start the server and open LVRR's window, like the app menu does;
                  without a display, like start
  start           start LVRR's server in the background, or show its status
  stop            stop the server
  status          show the server, headset and pairing state
  logs [ARGS]     show the log of the server and the apps it starts; ARGS such as -f
                  go to journalctl, or tail without systemd
  server [ARGS]   run the server in the foreground; ARGS go to wivrn-server
  pair            let a new headset connect and show its PIN
  headsets        list paired headsets with their ids
  rename ID NAME  rename a paired headset
  unpair ID       remove a paired headset
  usb [SERIAL]    connect the headset plugged in by USB
  install APK [SERIAL]
                  install the headset app over USB
  steam           show the Steam launch options for VR games
  desktop [ARGS]  start wayvr on the running server
  doctor          check the PC for VR: network, GPU, runtimes, Steam, input and audio;
                  --fix repairs what it can, --fix ID only that check, --json prints
                  the report as JSON
  setup-system [--firewall [--port N] [--tcp-only] [--no-mdns]] [--avahi]
                  as root: open the server port (9757 by default) and mDNS in ufw or
                  firewalld with --firewall, start Avahi with --avahi; `doctor --fix`
                  runs it with pkexec
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
        None => ("open".to_string(), &[][..]),
    };
    // logs, server and desktop pass their arguments on, so journalctl,
    // wivrn-server and wayvr answer --help; wivrnctl's help for headsets,
    // rename and unpair would name its own commands
    let own = matches!(
        command.as_str(),
        "start"
            | "stop"
            | "status"
            | "pair"
            | "headsets"
            | "rename"
            | "unpair"
            | "usb"
            | "install"
            | "steam"
            | "doctor"
            | "setup-firewall"
            | "setup-system"
    );
    if own && rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let result = match command.as_str() {
        "open" if args.is_empty() => open(&[]),
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
        "usb" => no_options(&command, rest).and_then(|()| headset::usb(&strings(rest))),
        "install" => no_options(&command, rest).and_then(|()| headset::install(rest)),
        "steam" => no_args(&command, rest).and_then(|()| headset::steam()),
        "desktop" => Err(desktop(rest)),
        "doctor" => return doctor::main(&strings(rest)),
        "setup-system" => return setup_system(&strings(rest)),
        // its name before it started Avahi too
        "setup-firewall" => {
            return setup_system(&[vec!["--firewall".to_string()], strings(rest)].concat());
        }
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
    report_error(error);
    ExitCode::FAILURE
}

/// `lvrr setup-system`, whose exit code tells `doctor --fix` which part failed
fn setup_system(args: &[String]) -> ExitCode {
    match doctor::setup_system(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err((error, code)) => {
            report_error(&error);
            ExitCode::from(code)
        }
    }
}

/// Prints an error, and shows it as a notification when nobody reads stderr
fn report_error(error: &str) {
    eprintln!("lvrr: {error}");
    if !interactive() {
        bus::notify("LVRR", error);
    }
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

/// For commands that take a serial or a file, so a mistyped option isn't
/// used as one
fn no_options(command: &str, args: &[OsString]) -> Result<(), String> {
    match args
        .iter()
        .find(|arg| arg.to_string_lossy().starts_with('-'))
    {
        Some(arg) => Err(format!(
            "{command}: unknown option {}",
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

/// Whether this runs in a graphical session
pub fn display_available() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|var| env::var_os(var).is_some_and(|v| !v.is_empty()))
}

/// The desktop window needs a display, and an install that has it
pub fn window_available() -> bool {
    display_available() && install_root().is_ok_and(|root| root.join("bin/lvrr-ui").is_file())
}

/// `lvrr` without arguments: the server, then the window on top of it
pub fn open(ui_args: &[&str]) -> Result<(), String> {
    if !window_available() {
        return server::start();
    }
    let mut cmd = Command::new(install_root()?.join("bin/lvrr-ui"));
    match server::ensure_running() {
        Ok(_) => {
            cmd.args(ui_args);
        }
        // The window needs the session bus
        Err(e) if bus::session().is_err() => return Err(e),
        // The window opens on "LVRR isn't running", with Start and Diagnostics,
        // without --pair: pairing needs the server. Its link tells another WiVRn
        // server by the missing HeadsetSettings, says so, offers no Start and
        // sends that server no requests.
        Err(e) => report_error(&e),
    }
    Err(exec(cmd))
}

/// `lvrr desktop`: wayvr on the headset's session, with the caller's environment
fn desktop(args: &[OsString]) -> String {
    let root = match install_root() {
        Ok(root) => root,
        Err(e) => return e,
    };
    // wayvr answers these without a runtime
    let info = args
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help" | "-V" | "--version")));
    if !info {
        // The server makes LVRR the active OpenXR runtime only while a session runs
        let session = match server::require_running() {
            Ok(conn) => bus::Server::new(&conn)
                .get::<bool>("SessionRunning")
                .unwrap_or(false),
            Err(e) => return e,
        };
        if !session {
            return "no headset is connected, LVRR starts the desktop when one connects"
                .to_string();
        }
    }
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
    let exe = lvrr_exe()?;
    exe.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{} is not in <prefix>/bin", exe.display()))
}

/// The real path of this executable. Reinstalling replaces the file under a
/// running lvrr, and the kernel then names it "<path> (deleted)".
pub fn lvrr_exe() -> Result<PathBuf, String> {
    env::current_exe()
        .and_then(
            |exe| match exe.to_str().and_then(|e| e.strip_suffix(" (deleted)")) {
                Some(path) => fs::canonicalize(path),
                None => fs::canonicalize(exe),
            },
        )
        .map_err(|e| format!("locating the lvrr executable: {e}"))
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
    which_in(program, &env::var_os("PATH")?)
}

/// Finds `program` in `path`, a list of directories like PATH
pub fn which_in(program: &str, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path)
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

/// The server config's contents; null when it doesn't exist, or when the server
/// can't read it and runs on its defaults, see config_error()
pub fn read_config() -> serde_json::Value {
    config_path()
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|text| parse_config(&text).ok())
        .unwrap_or_default()
}

/// Why the server can't read its config; None when it can, or when there is none
/// and the next start writes LVRR's defaults
pub fn config_error() -> Option<String> {
    let path = config_path().ok()?;
    match fs::read(&path) {
        Ok(text) => parse_config(&text).err(),
        // Unless it is a dangling link, which server_config() keeps
        Err(e) if e.kind() == ErrorKind::NotFound && !path.is_symlink() => None,
        Err(e) => Some(e.to_string()),
    }
}

/// Parses the config like the server, whose JSON parser skips a UTF-8 BOM. The
/// error tells where the JSON breaks.
fn parse_config(text: &[u8]) -> Result<serde_json::Value, String> {
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
    let config: serde_json::Value = serde_json::from_slice(text).map_err(|e| e.to_string())?;
    if config.is_object() {
        Ok(config)
    } else {
        Err("not a JSON object".to_string())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parsing() {
        let port = |text: &[u8]| parse_config(text).map(|config| server_port(&config));
        assert_eq!(port(b"{\"port\": 9800}"), Ok(9800));
        // the server's parser skips a BOM
        assert_eq!(port(b"\xEF\xBB\xBF{\"port\": 9800}"), Ok(9800));
        // the server ignores these and runs on its defaults
        let trailing = parse_config(b"{\"port\": 9800,\n}").unwrap_err();
        assert!(trailing.contains("line 2 column 1"), "{trailing}");
        assert_eq!(port(b"[]"), Err("not a JSON object".to_string()));
        assert!(port(b"").is_err());
    }
}
