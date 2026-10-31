//! LVRR: virtual monitors. The compositor adds an output of the chosen size and WayVR
//! shows it as a screen; the output goes away with the screen.
//!
//! KWin and Mutter add the output for a screencast and remove it when the cast ends,
//! also when WayVR dies. Sway and Hyprland add a headless output on request, which
//! outlives WayVR, so the outputs added are listed in the runtime directory and removed
//! at the next start.

use std::{
    io::Write,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::net::UnixStream,
    },
    process::Command,
    sync::{Arc, Mutex, OnceLock, PoisonError, mpsc},
    time::{Duration, Instant},
};

use anyhow::Context;
use glam::{DVec2, vec2};
use idmap::IdMap;
use wgui::i18n::Translation;
use wlx_capture::{
    frame::Transform,
    kde_screencast::{self, KdeScreencastCapture, KdeStream},
    mutter_screencast::{MutterScreencastCapture, MutterSession},
    wayland::{WlxClient, WlxOutput},
};
use wlx_common::overlays::{BackendAttrib, BackendAttribValue, MouseTransform, ToastTopic};

use crate::{
    backend::{
        input::{HoverResult, PointerHit},
        task::{OverlayTask, SpawnPos, TaskType},
    },
    overlays::toast::Toast,
    state::AppState,
    subsystem::hid::WheelDelta,
    windowing::{
        OverlaySelector,
        backend::{FrameMeta, OverlayBackend, OverlayEventData, RenderResources, ShouldRender},
    },
};

use super::{
    backend::{CaptureType, ScreenBackend},
    capture::new_wlx_capture,
    create_screen_from_backend,
};

/// How long the new output may take to show up
const LOCATE_TIMEOUT: Duration = Duration::from_secs(10);
const LOCATE_INTERVAL: Duration = Duration::from_millis(250);
/// Wait before trying a restored monitor again, doubled after each failure
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// A failed try costs KWin and Mutter one request, so the monitor shows soon after the
/// desktop allows it, as once GNOME is unlocked
const RETRY_DELAY_MAX_STREAM: Duration = Duration::from_secs(15);
/// A failed try on Sway and Hyprland can add and unplug an output, which moves workspaces
const RETRY_DELAY_MAX: Duration = Duration::from_secs(60);
/// Headless outputs this install added, as `method<TAB>instance<TAB>output` lines
const ORPHANS_FILE: &str = "lvrr-virtual-outputs";
static ORPHANS_LOCK: Mutex<()> = Mutex::new(());
/// Sway's answer names no output, so ours is the name that is new: one creation at a
/// time. Taken before ORPHANS_LOCK.
static SWAY_CREATE_LOCK: Mutex<()> = Mutex::new(());

/// How the compositor adds outputs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Kde,
    Mutter,
    Sway,
    Hyprland,
}

impl Method {
    /// Asks the compositor. SWAYSOCK and HYPRLAND_INSTANCE_SIGNATURE come from the user
    /// manager, which can lack them or keep them from an earlier session.
    pub fn detect() -> Option<Self> {
        // first, as the Wayland connection reaches the running compositor; Sway and
        // Hyprland offer no zkde_screencast
        if WlxClient::new().is_some_and(|wl| kde_screencast::supports_virtual(&wl)) {
            return Some(Self::Kde);
        }
        if hyprland_answers() {
            return Some(Self::Hyprland);
        }
        if sway_socket().is_some() {
            return Some(Self::Sway);
        }
        if MutterSession::available() {
            return Some(Self::Mutter);
        }
        None
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Kde => "kde",
            Self::Mutter => "mutter",
            Self::Sway => "sway",
            Self::Hyprland => "hyprland",
        }
    }

    /// The running compositor; a headless output belongs to one
    fn instance(self) -> String {
        match self {
            // a gone Sway's lines match none, and go without a swaymsg
            Self::Sway => sway_socket().unwrap_or_default().to_owned(),
            Self::Hyprland => std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap_or_default(),
            Self::Kde | Self::Mutter => String::new(),
        }
    }
}

/// Hyprland answers at the socket HYPRLAND_INSTANCE_SIGNATURE names. Hyprland sets and
/// unsets it in the user manager itself, but a crash leaves it behind.
fn hyprland_answers() -> bool {
    let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") else {
        return false;
    };
    // under /tmp before 0.40
    let answers = std::env::var("XDG_RUNTIME_DIR")
        .into_iter()
        .chain([String::from("/tmp")])
        .any(|dir| hyprland_replies(&format!("{dir}/hypr/{signature}/.socket.sock")));
    if !answers {
        log::info!(
            "Virtual monitors: ignoring HYPRLAND_INSTANCE_SIGNATURE {signature}, Hyprland does \
             not answer"
        );
    }
    answers
}

/// Asks for the version and reads the answer to its end, as hyprctl does. Hyprland
/// answers every connection, and answering one already closed raises SIGPIPE in it.
fn hyprland_replies(socket: &str) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    stream.write_all(b"version").is_ok()
        && std::io::copy(&mut stream, &mut std::io::sink()).is_ok_and(|n| n > 0)
}

/// Sway's IPC socket, looked up once
fn sway_socket() -> Option<&'static str> {
    static SOCKET: OnceLock<Option<String>> = OnceLock::new();
    SOCKET.get_or_init(find_sway_socket).as_deref()
}

/// SWAYSOCK if Sway answers there, else the one socket in the runtime directory it
/// answers at. The user manager can lack SWAYSOCK, or keep it from an earlier Sway.
/// All of a user's logins share the user manager and the runtime directory, so a Sway
/// that isn't the compositor WayVR is connected to is skipped, when both pids are known.
fn find_sway_socket() -> Option<String> {
    let compositor = compositor_pid();
    let other = |socket: &str, stream: &UnixStream| {
        let sway = peer_pid(stream.as_fd());
        if compositor.is_none() || sway.is_none() || sway == compositor {
            return false;
        }
        log::info!("Virtual monitors: ignoring {socket}, WayVR is connected to another compositor");
        true
    };
    if let Ok(socket) = std::env::var("SWAYSOCK") {
        match UnixStream::connect(&socket) {
            Ok(stream) if !other(&socket, &stream) => return Some(socket),
            Ok(_) => {}
            Err(_) => {
                log::info!("Virtual monitors: ignoring SWAYSOCK {socket}, Sway does not answer");
            }
        }
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    // SAFETY: getuid always succeeds
    let prefix = format!("sway-ipc.{}.", unsafe { libc::getuid() });
    let mut live = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        // sway-ipc.<uid>.<pid>.sock
        .filter(|name| name.starts_with(&prefix))
        // spelled like Sway's own SWAYSOCK, so orphan lines match
        .map(|name| format!("{dir}/{name}"))
        .filter(|socket| UnixStream::connect(socket).is_ok_and(|s| !other(socket, &s)));
    let socket = live.next()?;
    if live.next().is_some() {
        log::info!("Virtual monitors: several Sway sockets answer in {dir}, using none");
        return None;
    }
    log::info!("Virtual monitors: Sway answers at {socket}");
    Some(socket)
}

/// The process at WayVR's Wayland socket. Sway makes its own; other compositors can get
/// theirs from a wrapper, as KWin does.
fn compositor_pid() -> Option<libc::pid_t> {
    // connecting would use up WAYLAND_SOCKET, which the screens connect with
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        return None;
    }
    let connection = wayland_client::Connection::connect_to_env().ok()?;
    peer_pid(connection.backend().poll_fd())
}

/// The process that listens at the other end of a connected Unix socket
fn peer_pid(fd: BorrowedFd) -> Option<libc::pid_t> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred and len are valid for writes, and len is cred's size
    let ret = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (ret == 0 && cred.pid > 0).then_some(cred.pid)
}

/// A Sway or Hyprland output added for a virtual monitor, removed when dropped
pub struct HeadlessOutput {
    method: Method,
    output: Arc<str>,
}

impl HeadlessOutput {
    fn create(method: Method, name: &str, width: u32, height: u32) -> anyhow::Result<Self> {
        match method {
            Method::Sway => {
                // held through the mode set too: Sway adds the output at 1920x1080, and
                // resizing it later would move an output added meanwhile
                let _creating = SWAY_CREATE_LOCK
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let before = sway_outputs()?;
                swaymsg(&["create_output"])?;
                // more than one is new only if something else added one meanwhile:
                // take the newest headless one (wlroots numbers them in order) rather
                // than fail and leave ours behind
                let output = sway_outputs()?
                    .into_iter()
                    .filter(|o| !before.contains(o))
                    .max_by_key(|o| {
                        o.strip_prefix("HEADLESS-")
                            .and_then(|n| n.parse::<u32>().ok())
                    })
                    .context("Sway added no output")?;
                let me = Self::adopt(method, output.into());
                let mode = format!("{width}x{height}@60Hz");
                swaymsg(&["output", &me.output, "mode", "--custom", &mode])?;
                Ok(me)
            }
            Method::Hyprland => {
                // the rule applies to the output once it exists. 0.57 configures in Lua
                // and dropped `keyword`.
                let lua = format!(
                    "hl.monitor({{ output = \"{name}\", mode = \"{width}x{height}@60\", \
                     position = \"auto\", scale = \"1\" }})"
                );
                let rule = format!("{name},{width}x{height}@60,auto,1");
                hyprctl(&["eval", &lua]).or_else(|e| {
                    log::debug!("{e:#}, trying hyprctl keyword");
                    hyprctl(&["keyword", "monitor", &rule])
                })?;
                hyprctl(&["output", "create", "headless", name])?;
                Ok(Self::adopt(method, name.into()))
            }
            Method::Kde | Method::Mutter => anyhow::bail!("{method:?} adds no headless outputs"),
        }
    }

    fn adopt(method: Method, output: Arc<str>) -> Self {
        update_orphans(|lines| {
            lines.push(format!("{}\t{}\t{output}", method.id(), method.instance()));
        });
        Self { method, output }
    }
}

impl Drop for HeadlessOutput {
    fn drop(&mut self) {
        if let Err(e) = remove_output(self.method, &self.output) {
            log::warn!("{}: could not remove the output: {e:?}", self.output);
        }
        let line = format!(
            "{}\t{}\t{}",
            self.method.id(),
            self.method.instance(),
            self.output
        );
        update_orphans(|lines| lines.retain(|l| *l != line));
    }
}

fn remove_output(method: Method, output: &str) -> anyhow::Result<()> {
    match method {
        Method::Sway => swaymsg(&["output", output, "unplug"]).map(drop),
        Method::Hyprland => hyprctl(&["output", "remove", output]),
        Method::Kde | Method::Mutter => Ok(()),
    }
}

/// Removes the headless outputs a previous WayVR added and did not remove, as when it
/// crashed
pub fn remove_orphans() {
    update_orphans(|lines| {
        lines.retain(|line| {
            let mut fields = line.split('\t');
            let (Some(method), Some(instance), Some(output)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return false;
            };
            let method = match method {
                "sway" => Method::Sway,
                "hyprland" => Method::Hyprland,
                _ => return false,
            };
            // another compositor's output of the same name is not ours
            if method.instance() == instance {
                log::info!("{output}: removing the virtual monitor left from last run");
                let _ = remove_output(method, output);
                return false;
            }
            // another login's Sway, which shares the runtime directory: its WayVR
            // removes it
            method == Method::Sway && UnixStream::connect(instance).is_ok()
        });
    });
}

fn update_orphans(f: impl FnOnce(&mut Vec<String>)) {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(ORPHANS_FILE);
    let _guard = ORPHANS_LOCK.lock();
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    f(&mut lines);
    let result = if lines.is_empty() {
        std::fs::remove_file(&path).or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
    } else {
        std::fs::write(&path, lines.join("\n") + "\n")
    };
    if let Err(e) = result {
        log::warn!("{}: {e}", path.display());
    }
}

fn run(mut command: Command, args: &[&str]) -> anyhow::Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .args(args)
        .output()
        .with_context(|| format!("could not run {program}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{program} {}: {}{}",
            args.join(" "),
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(stdout)
}

/// swaymsg at the socket found, which its environment can lack
fn swaymsg(args: &[&str]) -> anyhow::Result<String> {
    let mut command = Command::new("swaymsg");
    if let Some(socket) = sway_socket() {
        command.env("SWAYSOCK", socket);
    }
    run(command, args)
}

/// hyprctl exits with 0 on errors too, and answers "ok" on success
fn hyprctl(args: &[&str]) -> anyhow::Result<()> {
    let answer = run(Command::new("hyprctl"), args)?;
    if answer.trim() != "ok" {
        anyhow::bail!("hyprctl {}: {}", args.join(" "), answer.trim());
    }
    Ok(())
}

fn sway_outputs() -> anyhow::Result<Vec<String>> {
    let json = swaymsg(&["-t", "get_outputs", "-r"])?;
    let outputs: Vec<serde_json::Value> = serde_json::from_str(&json)?;
    Ok(outputs
        .iter()
        .filter_map(|o| o["name"].as_str().map(str::to_string))
        .collect())
}

/// The outputs in the desktop layout now
fn output_names() -> Vec<Arc<str>> {
    WlxClient::new()
        .map(|wl| wl.outputs.values().map(|o| o.name.clone()).collect())
        .unwrap_or_default()
}

/// The name KWin gives the output it adds for a virtual monitor
fn kwin_output(name: &str) -> Arc<str> {
    format!("Virtual-{name}").into()
}

/// What the compositor made, on a worker thread since it can take seconds
enum Created {
    Kde(KdeStream),
    Mutter(MutterSession),
    Headless(HeadlessOutput),
}

fn create(method: Method, name: Arc<str>, width: u32, height: u32) -> anyhow::Result<Created> {
    Ok(match method {
        Method::Kde => {
            let wl = Box::new(WlxClient::new().context("no Wayland connection")?);
            Created::Kde(
                KdeStream::virtual_output(wl, name, width, height, true)
                    .context("KWin did not add the output")?,
            )
        }
        Method::Mutter => Created::Mutter(
            MutterSession::virtual_monitor(name, width, height, true)
                .context("GNOME did not add the monitor")?,
        ),
        Method::Sway | Method::Hyprland => {
            Created::Headless(HeadlessOutput::create(method, &name, width, height)?)
        }
    })
}

pub struct VirtualMonitorBackend {
    name: Arc<str>,
    method: Method,
    width: u32,
    height: u32,
    /// Added again at start, when the desktop may not allow it yet (GNOME refuses while
    /// locked): a failure keeps the monitor and tries again
    restore: bool,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    /// A failure was shown and logged as an error; a restored monitor's later ones are
    /// logged as info
    toasted: bool,
    pending: Option<mpsc::Receiver<anyhow::Result<Created>>>,
    /// Waits for its turn to start the stream, which makes Mutter add the output
    queued: bool,
    /// Attributes set before the screen exists, as the saved ones are right after the
    /// spawn; the screen takes them once made
    attribs: IdMap<BackendAttrib, BackendAttribValue>,
    screen: Option<ScreenBackend>,
    /// After `screen`, so the capture ends before the output goes
    headless: Option<HeadlessOutput>,
    /// The output's name, once found; a headless output's from the start
    output: Option<Arc<str>>,
    /// The name KWin gives the output. Mutter's can't be known ahead, so its output is
    /// the first new one.
    expected: Option<Arc<str>>,
    /// Outputs that existed before
    known: Vec<Arc<str>>,
    /// Done looking for the output
    located: bool,
    /// The mouse maps onto the output
    mapped: bool,
    /// The desktop layout the mapping was read from, see `crate::lvrr::layout_gen`
    seen_gen: u64,
    next_look: Instant,
    give_up: Instant,
}

impl VirtualMonitorBackend {
    fn new(method: Method, name: Arc<str>, width: u32, height: u32, restore: bool) -> Self {
        let now = Instant::now();
        let mut me = Self {
            name,
            method,
            width,
            height,
            restore,
            retry_at: None,
            retry_delay: RETRY_DELAY,
            toasted: false,
            pending: None,
            queued: false,
            attribs: IdMap::new(),
            screen: None,
            headless: None,
            output: None,
            expected: None,
            known: Vec::new(),
            located: false,
            mapped: false,
            seen_gen: 0,
            next_look: now,
            give_up: now + LOCATE_TIMEOUT,
        };
        me.start_create();
        me
    }

    /// Has the compositor add the output, from scratch when trying again
    fn start_create(&mut self) {
        // the capture ends before the output goes
        self.screen = None;
        self.headless = None;
        self.queued = false;
        self.output = None;
        self.expected = None;
        self.known = output_names();
        self.located = false;
        self.mapped = false;
        let now = Instant::now();
        self.next_look = now;
        self.give_up = now + LOCATE_TIMEOUT;
        let (tx, rx) = mpsc::channel();
        let (method, name, width, height) =
            (self.method, self.name.clone(), self.width, self.height);
        std::thread::spawn(move || {
            let _ = tx.send(create(method, name, width, height));
        });
        self.pending = Some(rx);
    }

    fn adopt(&mut self, app: &mut AppState, created: Created) {
        let xr = app.feats.xr_backend;
        match created {
            Created::Kde(stream) => {
                let capture = new_wlx_capture!(app, KdeScreencastCapture::from_stream(stream));
                let screen =
                    ScreenBackend::new_raw(self.name.clone(), xr, CaptureType::PipeWire, capture);
                self.set_screen(app, screen);
                self.expected = Some(kwin_output(&self.name));
            }
            Created::Mutter(session) => {
                let capture = new_wlx_capture!(app, MutterScreencastCapture::from_session(session));
                let screen =
                    ScreenBackend::new_raw(self.name.clone(), xr, CaptureType::PipeWire, capture);
                self.set_screen(app, screen);
                self.queued = true;
            }
            Created::Headless(output) => {
                self.output = Some(output.output.clone());
                self.headless = Some(output);
            }
        }
        self.give_up = Instant::now() + LOCATE_TIMEOUT;
    }

    /// The screen takes the attributes set before it existed: before the mouse is mapped,
    /// which reads the mouse transform back
    fn set_screen(&mut self, app: &mut AppState, mut screen: ScreenBackend) {
        for (_, value) in std::mem::take(&mut self.attribs) {
            screen.set_attrib(app, value);
        }
        self.screen = Some(screen);
    }

    /// Finds the output in the desktop layout, for the mouse; a headless output is
    /// captured from then on
    fn look(&mut self, app: &mut AppState) -> anyhow::Result<()> {
        let now = Instant::now();
        self.next_look = now + LOCATE_INTERVAL;
        let Some(wl) = WlxClient::new() else {
            return Ok(());
        };
        // KWin's name once it is listed, else the first new output
        let wanted = self.output.as_ref().or_else(|| {
            self.expected
                .as_ref()
                .filter(|name| wl.outputs.values().any(|o| o.name == **name))
        });
        // also the outputs KWin named after the other monitors
        let mut claimed = crate::lvrr::virtual_monitor_outputs(app);
        claimed.extend(
            app.lvrr
                .virtual_monitors
                .iter()
                .filter(|m| m.name != self.name)
                .map(|m| kwin_output(&m.name)),
        );
        let found = wl.outputs.values().find(|o| {
            o.logical_size.0 > 0
                && match wanted {
                    Some(name) => o.name == *name,
                    None => !self.known.contains(&o.name) && !claimed.contains(&o.name),
                }
        });
        let Some(output) = found else {
            if now < self.give_up {
                return Ok(());
            }
            if self.screen.is_none() {
                anyhow::bail!("the new output did not show up");
            }
            // the stream works, the mouse can't reach it
            log::warn!("{}: output not found, no mouse input", self.name);
            self.located = true;
            crate::lvrr::end_locate_turn(app, &self.name);
            return Ok(());
        };

        if self.screen.is_none() {
            let screen = ScreenBackend::new_ext_image_copy(output, app)
                .or_else(|| ScreenBackend::new_wlr_screencopy(output, app))
                .context("the compositor offers no screen capture")?;
            self.set_screen(app, screen);
        }
        let screen = self.screen.as_mut().unwrap(); // set just above
        set_geometry(screen, app, output);
        log::info!(
            "{}: output {} at {:?}, {:?}",
            self.name,
            output.name,
            output.logical_pos,
            output.logical_size
        );
        crate::lvrr::set_virtual_monitor_output(app, &self.name, output.name.clone());
        // read just now, with this output in place
        self.seen_gen = crate::lvrr::layout_gen(app);
        self.output = Some(output.name.clone());
        self.located = true;
        self.mapped = true;
        crate::lvrr::end_locate_turn(app, &self.name);
        set_desktop_extent(app, &wl);
        Ok(())
    }

    fn fail(&mut self, app: &mut AppState, error: &anyhow::Error) {
        self.pending = None;
        self.located = true;
        crate::lvrr::end_locate_turn(app, &self.name);
        let first = !self.toasted;
        if first {
            self.toasted = true;
            log::error!("{}: virtual monitor failed: {error:?}", self.name);
            let title = if self.restore {
                "LVRR.VIRTUAL_MONITOR_RETRYING"
            } else {
                "LVRR.VIRTUAL_MONITOR_FAILED"
            };
            Toast::new(
                ToastTopic::Error,
                Some(Translation::from_translation_key(title)),
                // restored monitors can fail together at start, each toast names its own
                Translation::from_raw_text_string(format!("{}: {error:#}", self.name)),
            )
            .submit(app);
        }
        if !self.restore {
            crate::lvrr::remove_virtual_monitor(app, &self.name);
            return;
        }
        // The monitor stays, so its size stays saved and Home can remove it. A half-made
        // headless output goes now, after its capture.
        self.screen = None;
        self.headless = None;
        let delay = self.retry_delay.as_secs();
        if first {
            log::info!("{}: trying again in {delay}s", self.name);
        } else {
            // expected while the desktop refuses, as GNOME does while locked
            log::info!("{}: {error:#}, trying again in {delay}s", self.name);
        }
        self.retry_at = Some(Instant::now() + self.retry_delay);
        let max = match self.method {
            Method::Kde | Method::Mutter => RETRY_DELAY_MAX_STREAM,
            Method::Sway | Method::Hyprland => RETRY_DELAY_MAX,
        };
        self.retry_delay = (self.retry_delay * 2).min(max);
    }

    /// Reads the output's place again once outputs were added or removed, which can
    /// move it. A hidden screen catches up once shown.
    fn follow_layout(&mut self, app: &mut AppState) {
        let layout_gen = crate::lvrr::layout_gen(app);
        if !self.mapped || self.seen_gen == layout_gen {
            return;
        }
        self.seen_gen = layout_gen;
        let (Some(name), Some(screen)) = (&self.output, &mut self.screen) else {
            return;
        };
        let Some(wl) = WlxClient::new() else {
            return;
        };
        // none if the compositor removed it
        let Some(output) = wl
            .outputs
            .values()
            .find(|o| o.name == *name && o.logical_size.0 > 0)
        else {
            return;
        };
        set_geometry(screen, app, output);
        log::info!(
            "{}: output {} now at {:?}, {:?}",
            self.name,
            output.name,
            output.logical_pos,
            output.logical_size
        );
    }

    /// The screen, once its input is set up
    fn input(&mut self) -> Option<&mut ScreenBackend> {
        self.screen.as_mut().filter(|_| self.mapped)
    }
}

/// Maps the mouse onto the output's place in the desktop. The transform goes through the
/// attribute, so a mouse transform the user set stays.
fn set_geometry(screen: &mut ScreenBackend, app: &mut AppState, output: &WlxOutput) {
    screen.logical_pos = vec2(output.logical_pos.0 as f32, output.logical_pos.1 as f32);
    screen.logical_size = vec2(output.logical_size.0 as f32, output.logical_size.1 as f32);
    let transform = match screen.get_attrib(BackendAttrib::MouseTransform) {
        Some(BackendAttribValue::MouseTransform(transform)) => transform,
        _ => MouseTransform::Default,
    };
    screen.set_attrib(app, BackendAttribValue::MouseTransform(transform));
}

/// The mouse moves over the box around all outputs, which changes with each output
pub fn set_desktop_extent(app: &mut AppState, wl: &WlxClient) {
    let extent = wl.get_desktop_extent();
    let origin = wl.get_desktop_origin();
    app.hid_provider
        .inner
        .set_desktop_extent(DVec2::new(extent.0 as _, extent.1 as _));
    app.hid_provider
        .inner
        .set_desktop_origin(DVec2::new(origin.0 as _, origin.1 as _));
}

/// With `restore`, for a monitor added again at start, a failure keeps it and tries again
pub fn spawn(
    app: &mut AppState,
    method: Method,
    name: Arc<str>,
    width: u32,
    height: u32,
    restore: bool,
) {
    let selector = OverlaySelector::Name(name.clone());
    app.tasks.enqueue(TaskType::Overlay(OverlayTask::Spawn(
        selector,
        SpawnPos::Spread,
        Box::new(move |app| {
            let backend = VirtualMonitorBackend::new(method, name.clone(), width, height, restore);
            let mut config = create_screen_from_backend(
                name,
                Transform::Normal,
                &app.session,
                Box::new(backend),
            );
            config.show_on_spawn = true;
            Some(config)
        }),
    )));
}

impl OverlayBackend for VirtualMonitorBackend {
    fn init(&mut self, _app: &mut AppState) -> anyhow::Result<()> {
        Ok(())
    }
    fn should_render(&mut self, app: &mut AppState) -> anyhow::Result<ShouldRender> {
        // A restored monitor tries again with no limit: a failed try leaves nothing behind,
        // and a locked GNOME refuses until unlocked. A hidden one tries once shown.
        if let Some(at) = self.retry_at {
            if Instant::now() < at {
                return Ok(ShouldRender::Unable);
            }
            self.retry_at = None;
            self.start_create();
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Ok(created)) => {
                    self.pending = None;
                    self.adopt(app, created);
                }
                Ok(Err(e)) => {
                    self.fail(app, &e);
                    return Ok(ShouldRender::Unable);
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(ShouldRender::Unable),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.fail(app, &anyhow::anyhow!("the worker thread stopped"));
                    return Ok(ShouldRender::Unable);
                }
            }
        }
        // Mutter adds the output, named Meta-N, once the stream starts: streams start
        // one at a time, so the one new output is this monitor's
        if self.queued {
            let now = Instant::now();
            if !crate::lvrr::take_locate_turn(app, &self.name, now + LOCATE_TIMEOUT) {
                return Ok(ShouldRender::Unable);
            }
            self.queued = false;
            self.known = output_names();
            self.give_up = now + LOCATE_TIMEOUT;
        }
        if !self.located
            && Instant::now() >= self.next_look
            && let Err(e) = self.look(app)
        {
            self.fail(app, &e);
            return Ok(ShouldRender::Unable);
        }
        self.follow_layout(app);
        match self.screen.as_mut() {
            Some(screen) => screen.should_render(app),
            None => Ok(ShouldRender::Unable),
        }
    }
    fn render(&mut self, app: &mut AppState, rdr: &mut RenderResources) -> anyhow::Result<()> {
        match self.screen.as_mut() {
            Some(screen) => screen.render(app, rdr),
            None => Ok(()),
        }
    }
    fn pause(&mut self, app: &mut AppState) -> anyhow::Result<()> {
        self.screen.as_mut().map_or(Ok(()), |s| s.pause(app))
    }
    fn resume(&mut self, app: &mut AppState) -> anyhow::Result<()> {
        self.screen.as_mut().map_or(Ok(()), |s| s.resume(app))
    }
    fn frame_meta(&mut self) -> Option<FrameMeta> {
        self.screen.as_mut().and_then(ScreenBackend::frame_meta)
    }
    fn notify(&mut self, app: &mut AppState, event_data: OverlayEventData) -> anyhow::Result<()> {
        self.screen
            .as_mut()
            .map_or(Ok(()), |s| s.notify(app, event_data))
    }
    fn on_hover(&mut self, app: &mut AppState, hit: &PointerHit) -> HoverResult {
        self.input()
            .map_or_else(HoverResult::consume, |s| s.on_hover(app, hit))
    }
    fn on_left(&mut self, app: &mut AppState, pointer: usize) {
        if let Some(s) = self.input() {
            s.on_left(app, pointer);
        }
    }
    fn on_pointer(&mut self, app: &mut AppState, hit: &PointerHit, pressed: bool) {
        if let Some(s) = self.input() {
            s.on_pointer(app, hit, pressed);
        }
    }
    fn on_scroll(&mut self, app: &mut AppState, hit: &PointerHit, delta: WheelDelta) {
        if let Some(s) = self.input() {
            s.on_scroll(app, hit, delta);
        }
    }
    fn get_interaction_transform(&mut self) -> Option<glam::Affine2> {
        self.screen
            .as_mut()
            .and_then(ScreenBackend::get_interaction_transform)
    }
    fn get_attrib(&self, attrib: BackendAttrib) -> Option<BackendAttribValue> {
        match &self.screen {
            Some(screen) => screen.get_attrib(attrib),
            None => self.attribs.get(attrib).cloned(),
        }
    }
    fn set_attrib(&mut self, app: &mut AppState, value: BackendAttribValue) -> bool {
        if let Some(screen) = &mut self.screen {
            return screen.set_attrib(app, value);
        }
        // kept for the screen if it takes them, stereo only in OpenXR
        let attrib = match value {
            BackendAttribValue::Stereo(_) if app.feats.xr_backend.is_open_xr() => {
                BackendAttrib::Stereo
            }
            BackendAttribValue::StereoFullFrame(_) => BackendAttrib::StereoFullFrame,
            BackendAttribValue::StereoAdjustMouse(_) => BackendAttrib::StereoAdjustMouse,
            BackendAttribValue::MouseTransform(_) => BackendAttrib::MouseTransform,
            _ => return false,
        };
        self.attribs.insert(attrib, value);
        true
    }
}
