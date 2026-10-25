//! D-Bus: LVRR's server, owner lookups and desktop notifications.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dbus::arg::{Get, PropMap};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::{Connection, Proxy};
use dbus::message::MatchRule;

/// The server keeps WiVRn's names, see the plan's rebrand rules
pub const NAME: &str = "io.github.wivrn.Server";
const PATH: &str = "/io/github/wivrn/Server";
const INTERFACE: &str = "io.github.wivrn.Server";
const TIMEOUT: Duration = Duration::from_secs(5);

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

pub fn session() -> Result<Connection, String> {
    Connection::new_session().map_err(|e| format!("connecting to the session bus: {e}"))
}

fn bus_proxy(conn: &Connection) -> Proxy<'_, &Connection> {
    conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", TIMEOUT)
}

pub fn has_owner(conn: &Connection, name: &str) -> bool {
    bus_proxy(conn)
        .method_call("org.freedesktop.DBus", "NameHasOwner", (name,))
        .is_ok_and(|(owned,): (bool,)| owned)
}

/// The process that owns `name`, if any
pub fn owner_pid(conn: &Connection, name: &str) -> Option<u32> {
    bus_proxy(conn)
        .method_call(
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            (name,),
        )
        .ok()
        .map(|(pid,): (u32,)| pid)
}

pub struct Server<'a>(Proxy<'a, &'a Connection>);

impl<'a> Server<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Server(conn.with_proxy(NAME, PATH, TIMEOUT))
    }

    pub fn get<T: for<'b> Get<'b> + 'static>(&self, property: &str) -> Result<T, String> {
        self.0
            .get(INTERFACE, property)
            .map_err(|e| format!("reading {property} from the server: {e}"))
    }

    pub fn call(&self, method: &str) -> Result<(), String> {
        self.0
            .method_call(INTERFACE, method, ())
            .map_err(|e| format!("{method} on the server: {e}"))
    }

    /// The PIN, or "" while a session runs
    pub fn enable_pairing(&self, timeout_secs: i32) -> Result<String, String> {
        self.0
            .method_call(INTERFACE, "EnablePairing", (timeout_secs,))
            .map(|(pin,): (String,)| pin)
            .map_err(|e| format!("EnablePairing on the server: {e}"))
    }
}

/// Shows a desktop notification. Failures are ignored: callers print the
/// text as well.
pub fn notify(summary: &str, body: &str) {
    if let Ok(conn) = Connection::new_session() {
        let _ = show(&conn, summary, body, &[], -1);
    }
}

fn show(
    conn: &Connection,
    summary: &str,
    body: &str,
    actions: &[&str],
    timeout_ms: i32,
) -> Result<u32, dbus::Error> {
    let icon = icon();
    let proxy = conn.with_proxy(NOTIFICATIONS, NOTIFICATIONS_PATH, TIMEOUT);
    proxy
        .method_call(
            NOTIFICATIONS,
            "Notify",
            (
                "LVRR",
                0u32,
                icon.as_str(),
                summary,
                body,
                actions.to_vec(),
                PropMap::new(),
                timeout_ms,
            ),
        )
        .map(|(id,): (u32,)| id)
}

/// Asks with a notification button. False when the notification is
/// dismissed, or after `timeout`.
pub fn confirm(summary: &str, body: &str, button: &str, timeout: Duration) -> Result<bool, String> {
    let conn = session()?;
    let (tx, rx) = mpsc::channel();
    let closed = tx.clone();
    let watch = |member: &'static str| MatchRule::new_signal(NOTIFICATIONS, member);
    // Subscribed before showing it, so a quick answer isn't missed
    conn.add_match(
        watch("ActionInvoked"),
        move |(id, key): (u32, String), _: &Connection, _: &dbus::Message| {
            let _ = tx.send((id, Some(key)));
            true
        },
    )
    .and_then(|_| {
        conn.add_match(
            watch("NotificationClosed"),
            move |(id, _reason): (u32, u32), _: &Connection, _: &dbus::Message| {
                let _ = closed.send((id, None));
                true
            },
        )
    })
    .map_err(|e| format!("watching notifications: {e}"))?;
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let id = show(&conn, summary, body, &["yes", button], timeout_ms)
        .map_err(|e| format!("showing a notification: {e}"))?;
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        conn.process(left.min(Duration::from_millis(200)))
            .map_err(|e| format!("waiting for an answer: {e}"))?;
        while let Ok((answered, key)) = rx.try_recv() {
            if answered == id {
                return Ok(key.as_deref() == Some("yes"));
            }
        }
    }
    let _ = conn
        .with_proxy(NOTIFICATIONS, NOTIFICATIONS_PATH, TIMEOUT)
        .method_call::<(), _, _, _>(NOTIFICATIONS, "CloseNotification", (id,));
    Ok(false)
}

/// The installed LVRR icon, if any
fn icon() -> String {
    let Ok(root) = crate::install_root() else {
        return String::new();
    };
    [
        "io.github.lvrr.svg",
        "lvrr.svg",
        "io.github.wivrn.wivrn.svg",
    ]
    .iter()
    .map(|name| root.join("share/icons/hicolor/scalable/apps").join(name))
    .find(|path| path.exists())
    .map(|path| path_string(&path))
    .unwrap_or_default()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
