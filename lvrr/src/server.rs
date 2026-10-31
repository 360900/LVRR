//! Starting and stopping LVRR's server.
//!
//! `start` runs it as a systemd user unit, which logs to the journal. Without
//! a user systemd it runs in the background and logs to a file. Either way
//! the server process holds a lock on a file, so there is only one: a second
//! server would unlink the first one's IPC socket.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use dbus::blocking::Connection;
use serde_json::Value;

use crate::bus::{self, Server};
use crate::doctor::{self, Check, Status};
use crate::{checks, exec, install_root, report};

pub const UNIT: &str = "lvrr-server.service";
/// The transient units the server starts apps in, the dashboard included, see
/// wivrn/server/start_systemd_unit.cpp. Without systemd the apps are its
/// children and write to its log.
const APP_UNITS: &str = "wivrn-application-*";
/// The slice the server starts those units in, also set in start_systemd_unit.cpp
const APP_SLICE: &str = "app-lvrr.slice";
/// How long the server gets to come up on the bus
const START_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);

/// Passed on to the unit, which gets the user manager's environment otherwise.
/// The XDG directories decide which config the server uses, the displays are
/// for its debug window. Not PATH: the server finds programs such as adb on the
/// session's PATH whether a terminal or the app menu started it, and the doctor
/// looks there too.
const PASS_ENV: [&str; 8] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "LVRR_ROOT",
];
/// Variables people set to pick a GPU or debug the runtime
const PASS_PREFIXES: [&str; 10] = [
    "WIVRN_",
    "XRT_",
    "XR_",
    "VK_",
    "MESA_",
    "RADV_",
    "DRI_PRIME",
    "__NV_",
    "__GLX_",
    "PRESSURE_VESSEL_",
];

pub enum State {
    /// This install's server owns the D-Bus name
    Ours,
    Foreign {
        pid: u32,
        exe: String,
    },
    /// The server process runs but isn't on the bus (yet)
    Starting,
    Stopped,
}

pub fn state(conn: &Connection, root: &Path) -> State {
    if let Some(pid) = bus::owner_pid(conn, bus::NAME) {
        let ours = fs::canonicalize(root.join("bin/wivrn-server")).ok();
        return match process_exe(pid) {
            Some(exe) if Some(&exe) == ours.as_ref() => State::Ours,
            Some(exe) => State::Foreign {
                pid,
                exe: exe.display().to_string(),
            },
            // A server with file capabilities hides its executable from its
            // own user too, but holds the lock `lvrr` took for it before exec
            None if lock_holder() == Some(pid) => State::Ours,
            None => State::Foreign {
                pid,
                exe: process_name(pid),
            },
        };
    }
    if lock_holder().is_some() || unit_active() {
        State::Starting
    } else {
        State::Stopped
    }
}

/// The executable of a process; None for other users' processes and for
/// undumpable ones, such as a program with file capabilities
pub fn process_exe(pid: u32) -> Option<PathBuf> {
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    // Reinstalling replaces the file under a running server
    Some(
        match exe.to_str().and_then(|e| e.strip_suffix(" (deleted)")) {
            Some(path) => PathBuf::from(path),
            None => exe,
        },
    )
}

/// The name of a process whose executable can't be read; the kernel shows
/// any process's name
fn process_name(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm")).map_or_else(
        |_| "an unknown program".to_string(),
        |name| name.trim_end().to_string(),
    )
}

fn foreign(pid: u32, exe: &str) -> String {
    format!("another WiVRn server is running: {exe} (pid {pid}). Quit it to use LVRR.")
}

/// `lvrr start`: starts the server unless it runs, then tells its state.
/// Kept apart from anything else `lvrr` may open later.
pub fn start() -> Result<(), String> {
    let root = install_root()?;
    let conn = bus::session()?;
    let _start = start_lock();
    wait_unit_gone();
    let summary = match state(&conn, &root) {
        State::Ours => "LVRR is running",
        State::Foreign { pid, exe } => return Err(foreign(pid, &exe)),
        State::Starting => {
            wait_started(&conn, &root, None)?;
            "LVRR started"
        }
        State::Stopped => {
            let child = launch(&root)?;
            wait_started(&conn, &root, child)?;
            // The server shows the PIN of a first pairing itself
            let pairing = Server::new(&conn)
                .get::<bool>("PairingEnabled")
                .unwrap_or(false);
            if pairing && !crate::interactive() {
                return Ok(());
            }
            "LVRR started"
        }
    };
    report(summary, &status_lines(&conn).join("\n"));
    Ok(())
}

/// A connection to this install's server, started first if needed
pub fn ensure_running() -> Result<Connection, String> {
    let root = install_root()?;
    let conn = bus::session()?;
    let _start = start_lock();
    wait_unit_gone();
    match state(&conn, &root) {
        State::Ours => (),
        State::Foreign { pid, exe } => return Err(foreign(pid, &exe)),
        State::Starting => wait_started(&conn, &root, None)?,
        State::Stopped => {
            let child = launch(&root)?;
            wait_started(&conn, &root, child)?;
        }
    }
    Ok(conn)
}

/// A connection to this install's server, which must be running or starting
pub fn require_running() -> Result<Connection, String> {
    let root = install_root()?;
    let conn = bus::session()?;
    match state(&conn, &root) {
        State::Ours => (),
        State::Foreign { pid, exe } => return Err(foreign(pid, &exe)),
        State::Starting => wait_started(&conn, &root, None)?,
        State::Stopped => {
            return Err("LVRR isn't running, start it with `lvrr start`".to_string());
        }
    }
    Ok(conn)
}

/// Starts the server in the background. Returns the process when it isn't a
/// unit, to notice it ending before the lock is taken.
fn launch(root: &Path) -> Result<Option<Child>, String> {
    let path = crate::server_config()?;
    let config = crate::read_config();
    let kept = path
        .parent()
        .map_or_else(Vec::new, |dir| doctor::first_start(dir, &config));
    report_problems(root, path.parent(), &config, kept);
    warn_foreign(root);
    let lvrr = crate::lvrr_exe()?;
    if systemd_available() {
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--quiet", "--collect", "--unit", UNIT])
            .arg("--description=LVRR server");
        for (name, value) in env::vars_os() {
            let name_str = name.to_string_lossy();
            if PASS_ENV.contains(&name_str.as_ref())
                || PASS_PREFIXES.iter().any(|p| name_str.starts_with(p))
            {
                let mut arg = OsString::from("--setenv=");
                arg.push(&name);
                arg.push("=");
                arg.push(&value);
                cmd.arg(arg);
            }
        }
        cmd.arg("--").arg(&lvrr).arg("run-server");
        let status = cmd
            .status()
            .map_err(|e| format!("running systemd-run: {e}"))?;
        // A start that couldn't take the start lock may have made the unit
        // first, and the caller waits for its server then
        return if status.success() || unit_active() || lock_holder().is_some() {
            Ok(None)
        } else {
            Err(format!("systemd-run could not start {UNIT}: {status}"))
        };
    }
    let log = log_path()?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    // Kept for `lvrr logs`, as it may tell why the previous server stopped
    let _ = fs::rename(&log, old_log_path(&log));
    let out = File::create(&log).map_err(|e| format!("creating {}: {e}", log.display()))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("opening {}: {e}", log.display()))?;
    Command::new(&lvrr)
        .arg("run-server")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        // Out of the terminal's foreground group, so closing it doesn't stop the server
        .process_group(0)
        .spawn()
        .map(Some)
        .map_err(|e| format!("starting the server: {e}"))
}

/// Written in the config's directory once a start told that Steam VR games can't reach
/// the runtime
const STEAM_RUNTIME_REPORTED: &str = "steam-runtime-reported";

/// Tells the quick checks that failed, at every start: a config the server ignores, what
/// keeps the server from starting or the headset from connecting, and once, Steam VR games
/// that can't reach the runtime. Checks the first start ran are in `kept`, and don't run
/// again. The full report is `lvrr doctor`, or PC > Diagnostics in the window.
fn report_problems(root: &Path, config_dir: Option<&Path>, config: &Value, mut kept: Vec<Check>) {
    let mut take = |id: &str| {
        let i = kept.iter().position(|c| c.id == id)?;
        Some(kept.remove(i))
    };
    let mut quick = vec![
        checks::config(),
        take("firewall").unwrap_or_else(|| doctor::firewall(config)),
        take("avahi").unwrap_or_else(|| checks::avahi(config)),
        checks::port(root, config),
        checks::openxr(root, false),
    ];
    // Told once: it doesn't keep the server from starting, and a game's launch options,
    // which the check can't see, may set the variable
    if let Some(reported) = config_dir.map(|dir| dir.join(STEAM_RUNTIME_REPORTED))
        && !reported.exists()
    {
        let check = checks::pressure_vessel(checks::steam_flatpak().as_ref());
        if check.status == Status::Fail {
            if let Err(e) = fs::write(&reported, "") {
                eprintln!("lvrr: writing {}: {e}", reported.display());
            }
            quick.push(check);
        }
    }
    let failed: Vec<String> = quick
        .into_iter()
        .filter(|c| c.status == Status::Fail)
        .map(|c| format!("{}: {}", c.title, c.message))
        .collect();
    if failed.is_empty() {
        return;
    }
    let details = if doctor::in_window() {
        "Run the checks in PC > Diagnostics for details and fixes.".to_string()
    } else {
        format!(
            "Run `lvrr doctor` for details, `{}` repairs what it can.",
            doctor::FIX
        )
    };
    report(
        "LVRR found problems",
        &format!("{}\n{details}", failed.join("\n")),
    );
}

fn wait_started(conn: &Connection, root: &Path, mut child: Option<Child>) -> Result<(), String> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let launched = child
            .as_mut()
            .is_some_and(|c| c.try_wait().is_ok_and(|status| status.is_none()));
        match state(conn, root) {
            // The server owns its name before it exports its object and turns
            // on a first start's pairing, which happen in one callback
            State::Ours if Server::new(conn).get::<bool>("HeadsetConnected").is_ok() => {
                return Ok(());
            }
            State::Foreign { pid, exe } => return Err(foreign(pid, &exe)),
            State::Stopped if !launched => {
                return Err("LVRR's server stopped right away, see `lvrr logs`".to_string());
            }
            _ if Instant::now() > deadline => {
                return Err(format!(
                    "LVRR's server didn't start within {}s, see `lvrr logs`",
                    START_TIMEOUT.as_secs()
                ));
            }
            _ => sleep(POLL),
        }
    }
}

/// Lines about the headset and pairing of this install's running server
fn status_lines(conn: &Connection) -> Vec<String> {
    let server = Server::new(conn);
    let mut lines = Vec::new();
    let connected = server.get::<bool>("HeadsetConnected").unwrap_or(false);
    if connected {
        let name = server.get::<String>("SystemName").unwrap_or_default();
        lines.push(format!("Headset: {name}"));
    } else if server.get::<bool>("SessionRunning").unwrap_or(false) {
        lines.push("Headset: disconnected, its session still runs".to_string());
    } else {
        lines.push("Headset: none, open LVRR on the headset to connect".to_string());
    }
    if server.get::<bool>("PairingEnabled").unwrap_or(false) {
        let pin = server.get::<String>("Pin").unwrap_or_default();
        lines.push(format!("Pairing: on, PIN {pin}"));
    }
    lines
}

pub fn status() -> Result<ExitCode, String> {
    let root = install_root()?;
    let conn = bus::session()?;
    match state(&conn, &root) {
        State::Ours => {
            let how = if unit_active() {
                format!(" as {UNIT}")
            } else {
                bus::owner_pid(&conn, bus::NAME).map_or(String::new(), |p| format!(" (pid {p})"))
            };
            println!("LVRR is running{how}");
            for line in status_lines(&conn) {
                println!("{line}");
            }
            let known: Vec<(String, String, i64)> =
                Server::new(&conn).get("KnownKeys").unwrap_or_default();
            let names: Vec<&str> = known.iter().map(|(name, _, _)| name.as_str()).collect();
            println!(
                "Paired: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
            Ok(ExitCode::SUCCESS)
        }
        State::Foreign { pid, exe } => {
            println!("LVRR is stopped, another WiVRn server is running: {exe} (pid {pid})");
            Ok(ExitCode::FAILURE)
        }
        State::Starting => {
            println!("LVRR is starting");
            Ok(ExitCode::SUCCESS)
        }
        State::Stopped => {
            println!("LVRR is stopped");
            // like `systemctl status` for an inactive unit
            Ok(ExitCode::from(3))
        }
    }
}

pub fn stop() -> Result<(), String> {
    let root = install_root()?;
    let conn = bus::session()?;
    let asked = match state(&conn, &root) {
        State::Foreign { pid, exe } => {
            return Err(format!(
                "the running server isn't LVRR's: {exe} (pid {pid})"
            ));
        }
        State::Stopped => {
            bus::quit_ui(&conn);
            report("LVRR isn't running", "");
            return Ok(());
        }
        State::Ours => {
            // The server may exit before it replies
            let _ = Server::new(&conn).call("Quit");
            true
        }
        // Not on the bus to be asked
        State::Starting => false,
    };
    if !(asked && wait_stopped(&conn, &root)) {
        if unit_active() || unit_state() == "deactivating" {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", UNIT])
                .status();
        } else if let Some(pid) = lock_holder()
            .and_then(|pid| i32::try_from(pid).ok())
            // kill(0) would signal this process's own group
            .filter(|pid| *pid > 0)
        {
            // SAFETY: sends a signal, no memory is involved
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        if !wait_stopped(&conn, &root) {
            return Err("LVRR's server didn't stop".to_string());
        }
    }
    // the window and tray go with it
    bus::quit_ui(&conn);
    report("LVRR stopped", "");
    Ok(())
}

fn wait_stopped(conn: &Connection, root: &Path) -> bool {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        // A start can't make the unit again while systemd stops it
        if matches!(state(conn, root), State::Stopped) && unit_state() != "deactivating" {
            return true;
        }
        sleep(POLL);
    }
    false
}

/// `lvrr server`: the server in the foreground, after the first start's checks.
/// Help and version skip those and the lock, so they also show while LVRR runs.
pub fn foreground(args: &[OsString]) -> String {
    // wivrn-server answers these without serving
    if args
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help" | "--version")))
    {
        let root = match install_root() {
            Ok(root) => root,
            Err(e) => return e,
        };
        let mut cmd = Command::new(root.join("bin/wivrn-server"));
        cmd.args(args);
        return exec(cmd);
    }
    match server_command(true) {
        Ok(mut cmd) => {
            cmd.args(args);
            exec(cmd)
        }
        Err(e) => e,
    }
}

/// `lvrr run-server`: what `start` runs in the background
pub fn background(args: &[OsString]) -> String {
    match server_command(false) {
        Ok(mut cmd) => {
            cmd.arg("--no-instructions").args(args);
            exec(cmd)
        }
        Err(e) => e,
    }
}

/// The server's command line, with the lock taken for it
fn server_command(first_start: bool) -> Result<Command, String> {
    let root = install_root()?;
    let bin = root.join("bin");
    let config = crate::server_config()?;
    if first_start && let Some(dir) = config.parent() {
        doctor::first_start(dir, &crate::read_config());
    }
    let lock = take_lock()?;
    if let Ok(conn) = bus::session()
        && let Some(pid) = bus::owner_pid(&conn, bus::NAME)
    {
        let exe = process_exe(pid).map_or_else(|| process_name(pid), |e| e.display().to_string());
        return Err(format!(
            "a WiVRn server is already running: {exe} (pid {pid})"
        ));
    }
    if first_start {
        warn_foreign(&root);
    }
    // The server keeps the lock, which survives exec
    let fd = lock.into_raw_fd();
    // SAFETY: fd is open and owned by nothing else now
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    let mut cmd = Command::new(bin.join("wivrn-server"));
    cmd.arg("-f")
        .arg(config)
        .env("PATH", crate::path_with(&bin)?);
    // Without a user systemd the server forks wayvr and games, which inherit its
    // environment: the variable would take them to another runtime than the one
    // the server activates
    if let Some(set) = env::var_os("XR_RUNTIME_JSON")
        && !checks::same_file(
            Path::new(&set),
            &root.join("share/openxr/1/openxr_wivrn.json"),
        )
    {
        cmd.env_remove("XR_RUNTIME_JSON");
    }
    Ok(cmd)
}

/// A file only a running server holds a write lock on. A POSIX lock, kept
/// across exec, and its holder can be read without taking it.
const SERVER_LOCK: &str = "lvrr-server";
/// A file each start holds a lock on, from looking for the server until it is up
const START_LOCK: &str = "lvrr-start";

fn lock_path(name: &str) -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir.join(format!("{name}.lock")),
        // SAFETY: getuid has no preconditions
        _ => env::temp_dir().join(format!("{name}-{}.lock", unsafe { libc::getuid() })),
    }
}

/// `create` only for taking the lock, so looking doesn't leave a file behind
fn open_lock(name: &str, create: bool) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path(name))
}

/// Makes a second start wait for the first one's server instead of starting
/// another. Starts go on without it when it can't be taken. std opens files
/// close-on-exec, so the server and other programs a start runs don't keep it.
fn start_lock() -> Option<File> {
    let file = open_lock(START_LOCK, true).ok()?;
    // SAFETY: fd is open
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0).then_some(file)
}

fn whole_file(kind: libc::c_int) -> libc::flock {
    // SAFETY: flock is plain data
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock
}

fn take_lock() -> Result<File, String> {
    let file = open_lock(SERVER_LOCK, true)
        .map_err(|e| format!("opening {}: {e}", lock_path(SERVER_LOCK).display()))?;
    let lock = whole_file(libc::F_WRLCK);
    // SAFETY: fd is open, lock is a valid flock
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0 {
        return Ok(file);
    }
    Err(match lock_holder().filter(|pid| *pid > 0) {
        Some(pid) => format!("LVRR's server is already running (pid {pid})"),
        None => "LVRR's server is already running".to_string(),
    })
}

/// The pid of the process that holds the lock; 0 when it isn't known, such as
/// for a holder in another PID namespace
pub fn lock_holder() -> Option<u32> {
    let file = open_lock(SERVER_LOCK, false).ok()?;
    let mut lock = whole_file(libc::F_WRLCK);
    // SAFETY: fd is open, lock is a valid flock
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) } != 0 {
        return None;
    }
    (lock.l_type != libc::F_UNLCK as libc::c_short).then(|| u32::try_from(lock.l_pid).unwrap_or(0))
}

/// Whether there is a systemd user manager, which `start` runs the server under
pub fn systemd_available() -> bool {
    env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| Path::new(&dir).join("systemd/private").exists())
        && crate::which("systemd-run").is_some()
}

/// The unit's state as `systemctl is-active` prints it, such as "active" or
/// "deactivating"; "" without systemd
fn unit_state() -> String {
    if !systemd_available() {
        return String::new();
    }
    Command::new("systemctl")
        .args(["--user", "is-active", UNIT])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether the unit runs or starts
fn unit_active() -> bool {
    matches!(
        unit_state().as_str(),
        "active" | "reloading" | "refreshing" | "activating"
    )
}

/// Waits while systemd stops the unit: systemd-run can't make it again until
/// it is gone
fn wait_unit_gone() {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while unit_state() == "deactivating" && Instant::now() < deadline {
        sleep(POLL);
    }
}

/// The log of a server started without systemd
fn log_path() -> Result<PathBuf, String> {
    Ok(crate::xdg_dir("XDG_STATE_HOME", ".local/state")?.join("lvrr/server.log"))
}

/// Where a start moves the previous server's log
fn old_log_path(log: &Path) -> PathBuf {
    log.with_file_name("server.log.old")
}

/// `lvrr logs`: the journal of the unit and the apps' units, or without systemd
/// the log file after the previous server's. journalctl resolves the units'
/// glob when it starts, so `-f` follows apps started later by their slice:
/// their output, without systemd's lines about them.
pub fn logs(args: &[OsString]) -> String {
    let cmd = if systemd_available() {
        let mut cmd = Command::new("journalctl");
        // Entries of any; the glob adds systemd's lines about the apps, such as
        // how they exited
        cmd.args(["--user", "--boot", "--unit", UNIT])
            .args(["--unit", APP_SLICE, "--unit", APP_UNITS])
            .args(args);
        cmd
    } else {
        let path = match log_path() {
            Ok(path) => path,
            Err(e) => return e,
        };
        let old = old_log_path(&path);
        let mut cmd = Command::new("tail");
        // ARGS before the files, where every tail takes options, not only GNU's.
        // With two files, tail heads each with its name.
        cmd.args(["-n", "200"]).args(args);
        if old.exists() {
            cmd.arg(old);
        }
        cmd.arg(path);
        cmd
    };
    exec(cmd)
}

/// Warns about a WiVRn server or wayvr from another install. Callers have
/// refused a server on the bus already; one off the bus would share the
/// server's socket, and a second wayvr exits right away.
fn warn_foreign(root: &Path) {
    let Ok(procs) = fs::read_dir("/proc") else {
        return;
    };
    // /proc shows real paths, LVRR_ROOT may not be one
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut found = Vec::new();
    let mut wayvr = false;
    let pids = procs
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok());
    for pid in pids {
        let Some(exe) = process_exe(pid) else {
            continue;
        };
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if matches!(name, "wivrn-server" | "wayvr") && !exe.starts_with(&root) {
            wayvr |= name == "wayvr";
            found.push(format!("{} (pid {pid})", exe.display()));
        }
    }
    if found.is_empty() {
        return;
    }
    let mut body = format!(
        "{}\nQuit {} to use LVRR.",
        found.join("\n"),
        if found.len() == 1 { "it" } else { "them" }
    );
    if wayvr {
        body.push_str(
            " A second wayvr exits at once, so LVRR's dashboard wouldn't open in the headset.",
        );
    }
    report("Another WiVRn server or wayvr is running", &body);
}
