//! LVRR: virtual monitors. The compositor adds an output of the chosen size and WayVR
//! shows it as a screen; the output goes away with the screen.
//!
//! KWin and Mutter add the output for a screencast and remove it when the cast ends,
//! also when WayVR dies. Sway and Hyprland add a headless output on request, which
//! outlives WayVR, so the outputs added are listed in the runtime directory and removed
//! at the next start.

use std::{
    process::Command,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use anyhow::Context;
use glam::{DVec2, vec2};
use wgui::i18n::Translation;
use wlx_capture::{
    frame::Transform,
    kde_screencast::{self, KdeScreencastCapture, KdeStream},
    mutter_screencast::{MutterScreencastCapture, MutterSession},
    wayland::WlxClient,
};
use wlx_common::overlays::{BackendAttrib, BackendAttribValue, ToastTopic};

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
/// Headless outputs this install added, as `method<TAB>instance<TAB>output` lines
const ORPHANS_FILE: &str = "lvrr-virtual-outputs";
static ORPHANS_LOCK: Mutex<()> = Mutex::new(());

/// How the compositor adds outputs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Kde,
    Mutter,
    Sway,
    Hyprland,
}

impl Method {
    pub fn detect() -> Option<Self> {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return Some(Self::Hyprland);
        }
        if std::env::var_os("SWAYSOCK").is_some() {
            return Some(Self::Sway);
        }
        if WlxClient::new().is_some_and(|wl| kde_screencast::supports_virtual(&wl)) {
            return Some(Self::Kde);
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
        let var = match self {
            Self::Sway => "SWAYSOCK",
            Self::Hyprland => "HYPRLAND_INSTANCE_SIGNATURE",
            Self::Kde | Self::Mutter => return String::new(),
        };
        std::env::var(var).unwrap_or_default()
    }
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
                let before = sway_outputs()?;
                run("swaymsg", &["create_output"])?;
                let output = sway_outputs()?
                    .into_iter()
                    .find(|o| !before.contains(o))
                    .context("Sway added no output")?;
                let me = Self::adopt(method, output.into());
                let mode = format!("{width}x{height}@60Hz");
                run(
                    "swaymsg",
                    &["output", &me.output, "mode", "--custom", &mode],
                )?;
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
        Method::Sway => run("swaymsg", &["output", output, "unplug"]).map(drop),
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
            }
            false
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

fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(program)
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

/// hyprctl exits with 0 on errors too, and answers "ok" on success
fn hyprctl(args: &[&str]) -> anyhow::Result<()> {
    let answer = run("hyprctl", args)?;
    if answer.trim() != "ok" {
        anyhow::bail!("hyprctl {}: {}", args.join(" "), answer.trim());
    }
    Ok(())
}

fn sway_outputs() -> anyhow::Result<Vec<String>> {
    let json = run("swaymsg", &["-t", "get_outputs", "-r"])?;
    let outputs: Vec<serde_json::Value> = serde_json::from_str(&json)?;
    Ok(outputs
        .iter()
        .filter_map(|o| o["name"].as_str().map(str::to_string))
        .collect())
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
    pending: Option<mpsc::Receiver<anyhow::Result<Created>>>,
    screen: Option<ScreenBackend>,
    /// After `screen`, so the capture ends before the output goes
    headless: Option<HeadlessOutput>,
    /// The output's name. KWin and Mutter pick it, so it is the first new one
    output: Option<Arc<str>>,
    /// Outputs that existed before
    known: Vec<Arc<str>>,
    /// Done looking for the output
    located: bool,
    /// The mouse maps onto the output
    mapped: bool,
    next_look: Instant,
    give_up: Instant,
}

impl VirtualMonitorBackend {
    fn new(method: Method, name: Arc<str>, width: u32, height: u32) -> Self {
        let known = WlxClient::new()
            .map(|wl| wl.outputs.values().map(|o| o.name.clone()).collect())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn({
            let name = name.clone();
            move || {
                let _ = tx.send(create(method, name, width, height));
            }
        });
        Self {
            name,
            pending: Some(rx),
            screen: None,
            headless: None,
            output: None,
            known,
            located: false,
            mapped: false,
            next_look: Instant::now(),
            give_up: Instant::now() + LOCATE_TIMEOUT,
        }
    }

    fn adopt(&mut self, app: &mut AppState, created: Created) {
        let xr = app.feats.xr_backend;
        match created {
            Created::Kde(stream) => {
                let capture = new_wlx_capture!(app, KdeScreencastCapture::from_stream(stream));
                self.screen = Some(ScreenBackend::new_raw(
                    self.name.clone(),
                    xr,
                    CaptureType::PipeWire,
                    capture,
                ));
            }
            Created::Mutter(session) => {
                let capture = new_wlx_capture!(app, MutterScreencastCapture::from_session(session));
                self.screen = Some(ScreenBackend::new_raw(
                    self.name.clone(),
                    xr,
                    CaptureType::PipeWire,
                    capture,
                ));
            }
            Created::Headless(output) => {
                self.output = Some(output.output.clone());
                self.headless = Some(output);
            }
        }
        self.give_up = Instant::now() + LOCATE_TIMEOUT;
    }

    /// Finds the output in the desktop layout, for the mouse; a headless output is
    /// captured from then on
    fn look(&mut self, app: &mut AppState) -> anyhow::Result<()> {
        let now = Instant::now();
        self.next_look = now + LOCATE_INTERVAL;
        let Some(wl) = WlxClient::new() else {
            return Ok(());
        };
        let claimed = crate::lvrr::virtual_monitor_outputs(app);
        let found = wl.outputs.values().find(|o| {
            o.logical_size.0 > 0
                && match &self.output {
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
            return Ok(());
        };

        if self.screen.is_none() {
            let screen = ScreenBackend::new_ext_image_copy(output, app)
                .or_else(|| ScreenBackend::new_wlr_screencopy(output, app))
                .context("the compositor offers no screen capture")?;
            self.screen = Some(screen);
        }
        let screen = self.screen.as_mut().unwrap(); // set just above
        screen.logical_pos = vec2(output.logical_pos.0 as f32, output.logical_pos.1 as f32);
        screen.logical_size = vec2(output.logical_size.0 as f32, output.logical_size.1 as f32);
        screen.apply_mouse_transform_with_override(Transform::Undefined);
        log::info!(
            "{}: output {} at {:?}, {:?}",
            self.name,
            output.name,
            output.logical_pos,
            output.logical_size
        );
        crate::lvrr::set_virtual_monitor_output(app, &self.name, output.name.clone());
        self.output = Some(output.name.clone());
        self.located = true;
        self.mapped = true;
        set_desktop_extent(app, &wl);
        Ok(())
    }

    fn fail(&mut self, app: &mut AppState, error: &anyhow::Error) {
        log::error!("{}: virtual monitor failed: {error:?}", self.name);
        self.pending = None;
        self.located = true;
        Toast::new(
            ToastTopic::Error,
            Some(Translation::from_translation_key(
                "LVRR.VIRTUAL_MONITOR_FAILED",
            )),
            Translation::from_raw_text_string(format!("{error:#}")),
        )
        .submit(app);
        crate::lvrr::remove_virtual_monitor(app, &self.name);
    }

    /// The screen, once its input is set up
    fn input(&mut self) -> Option<&mut ScreenBackend> {
        self.screen.as_mut().filter(|_| self.mapped)
    }
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

pub fn spawn(app: &mut AppState, method: Method, name: Arc<str>, width: u32, height: u32) {
    let selector = OverlaySelector::Name(name.clone());
    app.tasks.enqueue(TaskType::Overlay(OverlayTask::Spawn(
        selector,
        SpawnPos::Spread,
        Box::new(move |app| {
            let backend = VirtualMonitorBackend::new(method, name.clone(), width, height);
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
        if !self.located
            && Instant::now() >= self.next_look
            && let Err(e) = self.look(app)
        {
            self.fail(app, &e);
            return Ok(ShouldRender::Unable);
        }
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
        self.screen.as_ref().and_then(|s| s.get_attrib(attrib))
    }
    fn set_attrib(&mut self, app: &mut AppState, value: BackendAttribValue) -> bool {
        self.screen
            .as_mut()
            .is_some_and(|s| s.set_attrib(app, value))
    }
}
