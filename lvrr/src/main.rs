//! Entry point of the LVRR desktop app.
//!
//! Runs the WiVRn dashboard or server from this install with LVRR's own
//! server config, so settings stay apart from a system WiVRn. The config
//! starts wayvr when a headset connects, and WiVRn finds this install's
//! xrizer first on its OpenVR compat search path.

use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: lvrr [dashboard] [ARGS...]
       lvrr server [ARGS...]
       lvrr desktop [ARGS...]
       lvrr --version

  dashboard  open the dashboard, which starts the server (default)
  server     run the streaming server only
  desktop    start the wayvr desktop overlay on the running server";

/// Started by the server when a headset connects. A bare name, found on the
/// PATH that `run` sets, so the config keeps working if LVRR moves.
const DEFAULT_CONFIG: &str = "{\n\t\"application\": [\"wayvr\", \"--openxr\", \"--wait\"]\n}\n";

fn main() -> ExitCode {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    let command = match args.first().and_then(|a| a.to_str()) {
        Some(c @ ("dashboard" | "server" | "desktop")) => {
            let c = c.to_string();
            args.remove(0);
            c
        }
        Some("--version") => {
            println!("lvrr {}", option_env!("LVRR_VERSION").unwrap_or("unknown"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => "dashboard".to_string(),
    };

    match run(&command, args) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("lvrr: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: &str, args: Vec<OsString>) -> Result<Infallible, String> {
    let root = install_root()?;
    let bin = root.join("bin");
    let mut cmd = match command {
        "desktop" => {
            let mut cmd = Command::new(bin.join("wayvr"));
            cmd.arg("--openxr");
            cmd
        }
        _ => {
            let config = server_config()?;
            warn_foreign(&root);
            let exe = if command == "server" {
                "wivrn-server"
            } else {
                "wivrn-dashboard"
            };
            // The dashboard passes its arguments on to the server it starts.
            let mut cmd = Command::new(bin.join(exe));
            cmd.arg("-f").arg(config).env("PATH", path_with(&bin)?);
            cmd
        }
    };
    cmd.args(args);
    let err = cmd.exec();
    Err(format!(
        "running {}: {err}",
        cmd.get_program().to_string_lossy()
    ))
}

/// The install prefix: `LVRR_ROOT`, or two levels above the real executable,
/// so a `/usr/bin/lvrr` symlink still finds `/opt/lvrr`.
fn install_root() -> Result<PathBuf, String> {
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
fn path_with(bin: &Path) -> Result<OsString, String> {
    let rest = env::var_os("PATH").unwrap_or_default();
    env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&rest)))
        .map_err(|e| format!("building PATH: {e}"))
}

/// LVRR's WiVRn server config, created with LVRR's defaults on first use.
/// The server saves settings to the same file.
fn server_config() -> Result<PathBuf, String> {
    let dir = config_home()?.join("lvrr");
    let path = dir.join("wivrn-server.json");
    if path.exists() {
        return Ok(path);
    }
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
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

fn config_home() -> Result<PathBuf, String> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir);
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config"))
        .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_string())
}

/// Warns about a WiVRn server or wayvr that is not from this install. The
/// dashboard would control that server instead of starting its own, and a
/// second wayvr exits right away.
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
