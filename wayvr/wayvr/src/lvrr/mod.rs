//! LVRR: the link to the LVRR server, and dashboard features that need the backend.

use std::io::Write;

use lvrr_common::{
    link::LvrrLink,
    state::{LvrrBackend, MessageKind, Notice},
};
use wgui::i18n::Translation;
use wlx_common::overlays::ToastTopic;

use crate::{overlays::toast::Toast, state::AppState};

#[derive(Default)]
pub struct LvrrApp {
    pub link: Option<LvrrLink>,
    pub perf_overlay: bool,
}

pub fn start(app: &mut AppState) {
    if app.lvrr.link.is_some() {
        return;
    }
    match LvrrLink::start() {
        Ok(link) => app.lvrr.link = Some(link),
        Err(e) => log::error!("Could not start the LVRR server link: {e:?}"),
    }
}

/// Shows the server's messages, and failed requests, as toasts
pub fn tick(app: &mut AppState) {
    let notices = match app.lvrr.link.as_mut() {
        Some(link) => link.poll(),
        None => return,
    };
    for notice in notices {
        let (topic, text) = match notice {
            Notice::Message(message) => {
                let topic = match message.kind {
                    MessageKind::Info => ToastTopic::System,
                    MessageKind::Urgent => ToastTopic::Priority,
                    MessageKind::Error => ToastTopic::Error,
                };
                (topic, message.text)
            }
            Notice::RequestFailed { request, error } => {
                (ToastTopic::Error, format!("{request} failed: {error}"))
            }
        };
        Toast::new(
            topic,
            Some(Translation::from_raw_text("LVRR")),
            Translation::from_raw_text_string(text),
        )
        .with_timeout(5.0)
        .submit(app);
    }
}

/// Copies text to the desktop clipboard
pub fn clipboard_set_text(text: &str) -> anyhow::Result<()> {
    for (program, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
    ] {
        let Ok(mut child) = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        child.wait()?;
        return Ok(());
    }
    anyhow::bail!("Install wl-clipboard or xclip to copy text")
}
