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

use crate::bus::{self, Server};
use crate::doctor::{self, Status};
use crate::{checks, exec, install_root, report};

pub const UNIT: &str = "lvrr-server.service";
/// How long the server gets to come up on the bus
const START_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);

/// Passed on to the unit, which gets the user manager's environment otherwise.
/// The XDG directories decide which config the server uses, the displays are
/// for its debug window.
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
            exe => State::Foreign {
                pid,
                exe: exe.map_or("an unknown program".to_string(), |e| {
                    e.display().to_string()
                }),
            },
        };
    }
    if lock_holder().is_some() || unit_active() {
        State::Starting
    } else {
        State::Stopped
    }
}

/// The executable of a process; None for other users' processes
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

fn foreign(pid: u32, exe: &str) -> String {
    format!("another WiVRn server is running: {exe} (pid {pid}). Quit it to use LVRR.")
}

/// `lvrr start`: starts the server unless it runs, then tells its state.
/// Kept apart from anything else `lvrr` may open later.
pub fn start() -> Result<(), String> {
    let root = install_root()?;
    let conn = bus::session()?;
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

/// Starts the server in the background. Returns the process when it isn't a
/// unit, to notice it ending before the lock is taken.
fn launch(root: &Path) -> Result<Option<Child>, String> {
    let config = crate::server_config()?;
    if let Some(dir) = config.parent() {
        doctor::first_start(dir);
    }
    report_problems(root);
    warn_foreign(root);
    let lvrr = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("locating the lvrr executable: {e}"))?;
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
        return if status.success() {
            Ok(None)
        } else {
            Err(format!("systemd-run could not start {UNIT}: {status}"))
        };
    }
    let log = log_path()?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
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

/// Tells the first start's failed quick checks. The full report is `lvrr doctor`.
fn report_problems(root: &Path) {
    let config = crate::read_config();
    let failed: Vec<String> = [
        doctor::firewall(),
        checks::avahi(&config),
        checks::port(root, &config),
        checks::openxr(root, false),
    ]
    .into_iter()
    .filter(|c| c.status == Status::Fail)
    .map(|c| format!("{}: {}", c.title, c.message))
    .collect();
    if !failed.is_empty() {
        report(
            "LVRR found problems",
            &format!("{}\nRun `lvrr doctor` for details.", failed.join("\n")),
        );
    }
}

fn wait_started(conn: &Connection, root: &Path, mut child: Option<Child>) -> Result<(), String> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let launched = child
            .as_mut()
            .is_some_and(|c| c.try_wait().is_ok_and(|status| status.is_none()));
        match state(conn, root) {
            State::Ours => return Ok(()),
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
        if unit_active() {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", UNIT])
                .status();
        } else if let Some(pid) = lock_holder().and_then(|pid| i32::try_from(pid).ok()) {
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
        if matches!(state(conn, root), State::Stopped) {
            return true;
        }
        sleep(POLL);
    }
    false
}

/// `lvrr server`: the server in the foreground, after the first start's checks
pub fn foreground(args: &[OsString]) -> String {
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
    if first_start {
        if let Some(dir) = config.parent() {
            doctor::first_start(dir);
        }
        warn_foreign(&root);
    }
    let lock = take_lock()?;
    if let Ok(conn) = bus::session()
        && let Some(pid) = bus::owner_pid(&conn, bus::NAME)
    {
        let exe = process_exe(pid).map_or("an unknown program".to_string(), |e| {
            e.display().to_string()
        });
        return Err(format!(
            "a WiVRn server is already running: {exe} (pid {pid})"
        ));
    }
    // The server keeps the lock, which survives exec
    let fd = lock.into_raw_fd();
    // SAFETY: fd is open and owned by nothing else now
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    let mut cmd = Command::new(bin.join("wivrn-server"));
    cmd.arg("-f")
        .arg(config)
        .env("PATH", crate::path_with(&bin)?);
    Ok(cmd)
}

/// A file only a running server holds a write lock on. A POSIX lock, kept
/// across exec, and its holder can be read without taking it.
fn lock_path() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir.join("lvrr-server.lock"),
        // SAFETY: getuid has no preconditions
        _ => env::temp_dir().join(format!("lvrr-server-{}.lock", unsafe { libc::getuid() })),
    }
}

/// `create` only for taking the lock, so looking doesn't leave a file behind
fn open_lock(create: bool) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path())
}

fn whole_file(kind: libc::c_int) -> libc::flock {
    // SAFETY: flock is plain data
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock
}

fn take_lock() -> Result<File, String> {
    let file = open_lock(true).map_err(|e| format!("opening {}: {e}", lock_path().display()))?;
    let lock = whole_file(libc::F_WRLCK);
    // SAFETY: fd is open, lock is a valid flock
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == 0 {
        return Ok(file);
    }
    Err(match lock_holder() {
        Some(pid) => format!("LVRR's server is already running (pid {pid})"),
        None => "LVRR's server is already running".to_string(),
    })
}

/// The pid of the process that holds the lock
fn lock_holder() -> Option<u32> {
    let file = open_lock(false).ok()?;
    let mut lock = whole_file(libc::F_WRLCK);
    // SAFETY: fd is open, lock is a valid flock
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) } != 0 {
        return None;
    }
    (lock.l_type != libc::F_UNLCK as libc::c_short).then(|| u32::try_from(lock.l_pid).unwrap_or(0))
}

fn systemd_available() -> bool {
    env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| Path::new(&dir).join("systemd/private").exists())
        && crate::which("systemd-run").is_some()
}

fn unit_active() -> bool {
    systemd_available()
        && Command::new("systemctl")
            .args(["--user", "--quiet", "is-active", UNIT])
            .status()
            .is_ok_and(|s| s.success())
}

/// The log of a server started without systemd
fn log_path() -> Result<PathBuf, String> {
    Ok(crate::xdg_dir("XDG_STATE_HOME", ".local/state")?.join("lvrr/server.log"))
}

/// `lvrr logs`: the journal of the unit, or the log file without systemd
pub fn logs(args: &[OsString]) -> String {
    let mut cmd = if systemd_available() {
        let mut cmd = Command::new("journalctl");
        cmd.args(["--user", "--boot", "--unit", UNIT]);
        cmd
    } else {
        let path = match log_path() {
            Ok(path) => path,
            Err(e) => return e,
        };
        let mut cmd = Command::new("tail");
        cmd.args(["-n", "200"]).arg(path);
        cmd
    };
    cmd.args(args);
    exec(cmd)
}

/// Warns about a WiVRn server or wayvr that is not from this install and not
/// on the bus: it would share the server's socket, and a second wayvr exits
/// right away.
fn warn_foreign(root: &Path) {
    let Ok(procs) = fs::read_dir("/proc") else {
        return;
    };
    for entry in procs.flatten() {
        let Ok(exe) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if matches!(name, "wivrn-server" | "wayvr") && !exe.starts_with(root) {
            eprintln!(
                "lvrr: warning: {} is running (pid {}), stop it to use LVRR's",
                exe.display(),
                entry.file_name().to_string_lossy()
            );
        }
    }
}
