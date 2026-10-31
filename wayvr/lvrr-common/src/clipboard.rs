//! Copies text to the desktop clipboard, for the dashboard in VR and the desktop window.

use std::{
	io::{ErrorKind, Write},
	process::{Child, Command, Stdio},
	thread::sleep,
	time::{Duration, Instant},
};

/// How long a program gets to take the text. Without a data-control protocol wl-copy waits for keyboard focus,
/// which may not come, and the dashboard waits on its frame loop.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Copies `text` with wl-copy, or with xclip when that fails, as in an X11 session
pub fn set_text(text: &str) -> anyhow::Result<()> {
	let mut failures = Vec::new();
	let mut missing = Vec::new();
	for (program, package, args) in [
		("wl-copy", "wl-clipboard", &[][..]),
		("xclip", "xclip", &["-selection", "clipboard"][..]),
	] {
		// No output pipes: the copy they leave running in the background would keep them open. stderr is
		// ours, so their messages land next to our log.
		let result = match Command::new(program)
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::null())
			.spawn()
		{
			Ok(mut child) => copy(&mut child, text),
			Err(e) if e.kind() == ErrorKind::NotFound => {
				missing.push(package);
				continue;
			}
			Err(e) => Err(e.into()),
		};
		match result {
			Ok(()) => return Ok(()),
			Err(e) => {
				log::warn!("{program}: {e}");
				failures.push(format!("{program}: {e}"));
			}
		}
	}
	if failures.is_empty() {
		anyhow::bail!("Install wl-clipboard or xclip to copy text");
	}
	let failures = failures.join("; ");
	// one that isn't installed may work where the others failed, as xclip in an X11 session
	if !missing.is_empty() {
		let missing = missing.join(" or ");
		anyhow::bail!("Couldn't copy to the clipboard ({failures}). Install {missing} to copy text.");
	}
	anyhow::bail!("Couldn't copy to the clipboard: {failures}")
}

/// Writes `text` to the program and waits until it has taken it
fn copy(child: &mut Child, text: &str) -> anyhow::Result<()> {
	// dropping stdin ends the text
	let written = child
		.stdin
		.take()
		.map_or(Ok(()), |mut stdin| stdin.write_all(text.as_bytes()));
	let deadline = Instant::now() + TIMEOUT;
	let status = loop {
		if let Some(status) = child.try_wait()? {
			break status;
		}
		if Instant::now() >= deadline {
			child.kill()?;
			child.wait()?;
			anyhow::bail!("timed out");
		}
		sleep(Duration::from_millis(10));
	};
	// a program that failed before reading the text also broke the pipe, and its status says more
	anyhow::ensure!(status.success(), "{status}");
	written?;
	Ok(())
}
