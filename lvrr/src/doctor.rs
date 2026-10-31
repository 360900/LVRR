//! `lvrr doctor`: checks the PC for VR (network, GPU, runtimes, Steam, input and audio), and
//! fixes what it can.
//!
//! Opening the firewall and starting Avahi need admin rights, so `lvrr doctor --fix` and the
//! first start run `lvrr setup-system` through pkexec. Checks run without them, and most
//! distributions keep the firewall's rules from users. Then firewalld tells them if polkit
//! allows it without a password; otherwise the check goes by what LVRR opened: its ufw
//! profile, or the firewalld zones it lists in firewall-opened for this install.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitCode, ExitStatus, Stdio};

use serde_json::{Value, json};

use crate::bus::{self, Server};
use crate::checks;
use crate::server::{self, State};

/// mDNS, which the headset uses to find the server
const MDNS: u16 = 5353;

const UFW_CONF: &str = "/etc/ufw/ufw.conf";
const UFW_RULES: [&str; 2] = ["/etc/ufw/user.rules", "/etc/ufw/user6.rules"];
const UFW_PROFILE: &str = "/etc/ufw/applications.d/lvrr";

const SETUP_USAGE: &str =
    "usage: lvrr setup-system [--firewall [--port N] [--tcp-only] [--no-mdns]] [--avahi]";
/// Avahi, which the server needs to publish itself
const AVAHI_UNIT: &str = "avahi-daemon.service";
/// `lvrr setup-system`'s exit code when only starting Avahi failed, so `fix_system` still
/// notes the firewall it opened; it exits with 1 for other errors
const AVAHI_FAILED: u8 = 2;

pub const FIX: &str = "lvrr doctor --fix";

/// Whether notices go to someone with LVRR's window and no terminal, as from the app
/// menu, so they point to the window's PC > Diagnostics
pub fn in_window() -> bool {
    !crate::interactive() && crate::window_available()
}

/// Written once the first start asked to open the firewall, so it asks only once
const ASKED_FILE: &str = "firewall-asked";
/// The same for starting Avahi
const AVAHI_ASKED_FILE: &str = "avahi-asked";
/// What `lvrr setup-system` opened in which firewalld zones of which install, written
/// next to ASKED_FILE for the check, which can't read the zones on most distributions
const OPENED_FILE: &str = "firewall-opened";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub message: String,
    pub fix: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Firewall {
    Ufw,
    Firewalld,
}

/// What the headset needs let in, from the server config
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Needs {
    /// The server's port, for TCP
    port: u16,
    /// The same port for UDP, which the server streams over without `tcp-only`
    udp: bool,
    /// mDNS, while the server publishes itself
    mdns: bool,
}

impl Needs {
    fn from_config(config: &Value) -> Self {
        Needs {
            port: crate::server_port(config),
            udp: config.get("tcp-only").and_then(Value::as_bool) != Some(true),
            mdns: checks::publishing(config),
        }
    }

    /// The arguments that pass them on to `lvrr setup-system --firewall`
    fn args(&self) -> Vec<String> {
        let mut args = vec!["--port".to_string(), self.port.to_string()];
        if !self.udp {
            args.push("--tcp-only".to_string());
        }
        if !self.mdns {
            args.push("--no-mdns".to_string());
        }
        args
    }

    /// Such as "TCP and UDP port 9757 and mDNS" or "TCP port 9800"
    fn describe(&self) -> String {
        let protocols = if self.udp { "TCP and UDP" } else { "TCP" };
        let mdns = if self.mdns { " and mDNS" } else { "" };
        format!("{protocols} port {}{mdns}", self.port)
    }
}

/// What `lvrr setup-system` sets up as root
#[derive(Debug, PartialEq, Eq)]
struct Setup {
    /// What to open in the firewall, with `--firewall`
    firewall: Option<Needs>,
    /// Whether to start Avahi, with `--avahi`
    avahi: bool,
}

impl Setup {
    /// `lvrr setup-system`'s arguments. The firewall's options default to what the server
    /// needs without a config.
    fn from_args(args: &[String]) -> Result<Self, String> {
        let mut firewall = false;
        let mut avahi = false;
        let default = Needs {
            port: crate::DEFAULT_PORT,
            udp: true,
            mdns: true,
        };
        let mut needs = default;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--firewall" => firewall = true,
                "--avahi" => avahi = true,
                "--port" => match args.next().and_then(|port| port.parse::<u16>().ok()) {
                    Some(port) if port != 0 => needs.port = port,
                    _ => {
                        return Err(format!(
                            "setup-system: --port needs a number from 1 to 65535\n\n{SETUP_USAGE}"
                        ));
                    }
                },
                "--tcp-only" => needs.udp = false,
                "--no-mdns" => needs.mdns = false,
                _ => {
                    return Err(format!(
                        "setup-system: unknown argument {arg}\n\n{SETUP_USAGE}"
                    ));
                }
            }
        }
        if !firewall && !avahi {
            return Err(format!(
                "setup-system: needs --firewall, --avahi or both\n\n{SETUP_USAGE}"
            ));
        }
        if !firewall && needs != default {
            return Err(format!(
                "setup-system: --port, --tcp-only and --no-mdns go with --firewall\n\n{SETUP_USAGE}"
            ));
        }
        Ok(Setup {
            firewall: firewall.then_some(needs),
            avahi,
        })
    }

    /// The arguments that pass it on to `lvrr setup-system`
    fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(needs) = &self.firewall {
            args.push("--firewall".to_string());
            args.extend(needs.args());
        }
        if self.avahi {
            args.push("--avahi".to_string());
        }
        args
    }
}

/// What the firewall lets in: the server's port over TCP and UDP, and mDNS
#[derive(Default, Debug, PartialEq, Eq)]
struct Open {
    tcp: bool,
    udp: bool,
    mdns: bool,
}

impl Open {
    /// What `needs` asks for that isn't let in
    fn missing(&self, needs: &Needs) -> Vec<String> {
        let mut missing = Vec::new();
        if !self.tcp {
            missing.push(format!("TCP port {}", needs.port));
        }
        if needs.udp && !self.udp {
            missing.push(format!("UDP port {}", needs.port));
        }
        if needs.mdns && !self.mdns {
            missing.push(format!("mDNS (UDP port {MDNS})"));
        }
        missing
    }

    /// A rule letting in `protocol` on `ports`; `port` is the server's
    fn add(&mut self, protocol: &str, ports: &str, port: u16) {
        let (tcp, udp) = match protocol {
            "any" => (true, true),
            "tcp" => (true, false),
            "udp" => (false, true),
            _ => (false, false),
        };
        if port_in(ports, port) {
            self.tcp |= tcp;
            self.udp |= udp;
        }
        if port_in(ports, MDNS) {
            self.mdns |= udp;
        }
    }

    /// A firewalld service; WiVRn's opens the default port only
    fn add_service(&mut self, name: &str, port: u16) {
        match name {
            "wivrn" | "lvrr" => self.add("any", &crate::DEFAULT_PORT.to_string(), port),
            "mdns" => self.add("udp", &MDNS.to_string(), port),
            _ => (),
        }
    }
}

/// `ports` is `any`, a port, or a list of ports and ranges (`9000:9999` or `9000-9999`)
fn port_in(ports: &str, port: u16) -> bool {
    ports == "any"
        || ports.split(',').any(|p| match p.split_once([':', '-']) {
            Some((low, high)) => {
                low.parse().is_ok_and(|low: u16| low <= port)
                    && high.parse().is_ok_and(|high: u16| port <= high)
            }
            None => p.parse() == Ok(port),
        })
}

/// Incoming rules from ufw's `### tuple ###` lines:
/// `action protocol dport dst sport src [dapp sapp] direction [comment=hex]`
fn ufw_open(rules: &str, port: u16) -> Open {
    let mut open = Open::default();
    for line in rules.lines() {
        let Some(tuple) = line.strip_prefix("### tuple ### ") else {
            continue;
        };
        // ufw writes a rule's comment last, and its own reader cuts it off too
        let tuple = tuple
            .split_once(" comment=")
            .map_or(tuple, |(rule, _)| rule);
        let fields: Vec<&str> = tuple.split_whitespace().collect();
        let [action, protocol, dport, _dst, sport, ..] = fields[..] else {
            continue;
        };
        let incoming = fields.last().is_some_and(|d| d.starts_with("in"));
        // logged rules are allow_log or allow_log-all; route:allow is for forwarded traffic
        let allows = matches!(action.split('_').next(), Some("allow" | "limit"));
        // the headset sends from any port
        if allows && incoming && sport == "any" {
            open.add(protocol, dport, port);
        }
    }
    open
}

/// Ports and services of a firewalld zone file. A zone that accepts
/// everything is open.
fn firewalld_zone_open(xml: &str, port: u16) -> Open {
    let mut open = Open::default();
    for element in xml.split('<').skip(1) {
        if element.starts_with("zone ") && attribute(element, "target") == Some("ACCEPT") {
            open.add("any", "any", port);
        } else if element.starts_with("port ") {
            if let (Some(ports), Some(protocol)) =
                (attribute(element, "port"), attribute(element, "protocol"))
            {
                open.add(protocol, ports, port);
            }
        } else if element.starts_with("service ")
            && let Some(name) = attribute(element, "name")
        {
            open.add_service(name, port);
        }
    }
    open
}

/// A zone's runtime rules from `firewall-cmd --zone=Z --list-all`, what
/// `firewalld_zone_open` reads from its file
fn firewalld_list_open(text: &str, port: u16) -> Open {
    let mut open = Open::default();
    for line in text.lines() {
        let Some((key, values)) = line.trim().split_once(':') else {
            continue;
        };
        for value in values.split_whitespace() {
            match key {
                "target" if value == "ACCEPT" => open.add("any", "any", port),
                "services" => open.add_service(value, port),
                "ports" => {
                    if let Some((ports, protocol)) = value.split_once('/') {
                        open.add(protocol, ports, port);
                    }
                }
                _ => (),
            }
        }
    }
    open
}

/// What LVRR's ufw application profile opens: `ports=9757|5353/udp`, each entry
/// ports with an optional protocol
fn ufw_profile_open(profile: &str, port: u16) -> Open {
    let mut open = Open::default();
    let entries = profile
        .lines()
        .filter_map(|line| line.strip_prefix("ports="))
        .flat_map(|ports| ports.split('|'));
    for entry in entries {
        let entry = entry.trim();
        let (ports, protocol) = entry.split_once('/').unwrap_or((entry, "any"));
        open.add(protocol, ports, port);
    }
    open
}

fn attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = element.find(&key)? + key.len();
    element[start..].split('"').next()
}

fn active_firewall() -> Option<Firewall> {
    let ufw_enabled =
        fs::read_to_string(UFW_CONF).is_ok_and(|c| c.lines().any(|l| l.trim() == "ENABLED=yes"));
    if ufw_enabled {
        return Some(Firewall::Ufw);
    }
    let firewalld = Command::new("systemctl")
        .args(["--quiet", "is-active", "firewalld"])
        .status()
        .is_ok_and(|s| s.success());
    firewalld.then_some(Firewall::Firewalld)
}

/// What ufw lets in, or None when its rules need admin rights to read, as on Debian
/// and Ubuntu
fn read_ufw(port: u16) -> Result<Option<Open>, String> {
    let mut rules = String::new();
    for path in UFW_RULES {
        match fs::read_to_string(path) {
            Ok(text) => rules.push_str(&text),
            Err(e) if e.kind() == ErrorKind::PermissionDenied => return Ok(None),
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    Ok(Some(ufw_open(&rules, port)))
}

/// What LVRR's ufw profile opens, which any user can read: setup-system makes it 0644,
/// like ufw's own profiles, and keeps it only once `ufw allow LVRR` worked.
fn read_ufw_profile(port: u16) -> Open {
    fs::read_to_string(UFW_PROFILE)
        .map(|profile| ufw_profile_open(&profile, port))
        .unwrap_or_default()
}

/// firewalld tells any user its default zone; its config is root's on most distributions
fn default_zone() -> String {
    let asked = Command::new("firewall-cmd")
        .arg("--get-default-zone")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    asked
        .or_else(|| {
            let conf = fs::read_to_string("/etc/firewalld/firewalld.conf").ok()?;
            conf.lines()
                .find_map(|l| l.strip_prefix("DefaultZone=").map(|z| z.trim().to_string()))
        })
        .filter(|z| !z.is_empty())
        .unwrap_or_else(|| "public".to_string())
}

/// The zones `firewall-cmd --get-active-zones` lists, with their interfaces
fn active_zones(text: &str) -> Vec<(String, Vec<String>)> {
    let mut zones: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some(zone) = line.split_whitespace().next() {
                zones.push((zone.to_string(), Vec::new()));
            }
        } else if let (Some(interfaces), Some((_, list))) =
            (line.trim().strip_prefix("interfaces:"), zones.last_mut())
        {
            list.extend(interfaces.split_whitespace().map(str::to_string));
        }
    }
    zones
}

/// Interfaces with a default route, from /proc/net/route and ipv6_route: the
/// ones the headset's network is on
fn default_route_interfaces(ipv4: &str, ipv6: &str) -> Vec<String> {
    let v4 = ipv4.lines().skip(1).filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(1) == Some(&"00000000") && fields.get(7) == Some(&"00000000"))
            .then(|| fields[0].to_string())
    });
    let v6 = ipv6.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let default = fields.first().is_some_and(|d| d.bytes().all(|b| b == b'0'))
            && fields.get(1) == Some(&"00");
        default
            .then(|| fields.last().map(|i| i.to_string()))
            .flatten()
    });
    let mut interfaces: Vec<String> = v4.chain(v6).filter(|i| i != "lo").collect();
    interfaces.sort();
    interfaces.dedup();
    interfaces
}

/// The firewalld zones of the interfaces with a default route. firewalld puts
/// an interface no zone names in the default zone.
fn firewalld_zones() -> Vec<String> {
    // asked only when needed, as it runs firewall-cmd
    let mut default = None;
    let active = Command::new("firewall-cmd")
        .arg("--get-active-zones")
        .output()
        .map(|o| active_zones(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let interfaces = default_route_interfaces(
        &fs::read_to_string("/proc/net/route").unwrap_or_default(),
        &fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default(),
    );
    let mut zones: Vec<String> = interfaces
        .iter()
        .map(|interface| {
            active
                .iter()
                .find(|(_, interfaces)| interfaces.contains(interface))
                .map_or_else(
                    || default.get_or_insert_with(default_zone).clone(),
                    |(zone, _)| zone.clone(),
                )
        })
        .collect();
    if zones.is_empty() {
        zones.push(default.unwrap_or_else(default_zone));
    }
    zones.sort();
    zones.dedup();
    zones
}

/// What a firewalld zone lets in, or None when its rules need admin rights to read. Most
/// distributions keep /etc/firewalld root's; then firewall-cmd tells the runtime rules if
/// polkit lets this user read them without a password, which `may_list` keeps for the
/// next zone.
fn read_zone(zone: &str, port: u16, may_list: &mut Option<bool>) -> Result<Option<Open>, String> {
    let custom = format!("/etc/firewalld/zones/{zone}.xml");
    match fs::read_to_string(&custom) {
        Ok(xml) => return Ok(Some(firewalld_zone_open(&xml, port))),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let allowed = *may_list.get_or_insert_with(firewalld_config_readable);
            return Ok(if allowed { list_zone(zone, port) } else { None });
        }
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(format!("{custom}: {e}")),
        Err(_) => (),
    }
    let stock = format!("/usr/lib/firewalld/zones/{zone}.xml");
    match fs::read_to_string(&stock) {
        Ok(xml) => Ok(Some(firewalld_zone_open(&xml, port))),
        Err(e) if e.kind() == ErrorKind::NotFound => Err(format!("no file for zone {zone}")),
        Err(e) => Err(format!("{stock}: {e}")),
    }
}

/// Whether polkit lets this process read firewalld's config without a password.
/// firewalld asks for one otherwise, which a check must not make it do.
fn firewalld_config_readable() -> bool {
    Command::new("pkcheck")
        .args([
            "--action-id",
            "org.fedoraproject.FirewallD1.config.info",
            "--process",
        ])
        .arg(process::id().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A zone's runtime rules from firewall-cmd
fn list_zone(zone: &str, port: u16) -> Option<Open> {
    let output = Command::new("firewall-cmd")
        .arg(format!("--zone={zone}"))
        .arg("--list-all")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(firewalld_list_open(
        &String::from_utf8_lossy(&output.stdout),
        port,
    ))
}

/// What `lvrr setup-system` opened in which firewalld zones, kept in OPENED_FILE
#[derive(Debug, PartialEq, Eq)]
struct Opened {
    needs: Needs,
    zones: Vec<String>,
    /// The machine ID of the install whose firewalld it opened, as the home it is kept in
    /// may be shared with or kept from another install
    machine: String,
}

impl Opened {
    fn path() -> Result<PathBuf, String> {
        Ok(crate::config_path()?.with_file_name(OPENED_FILE))
    }

    /// The marker, if this install wrote it
    fn read() -> Option<Self> {
        Self::parse(&fs::read_to_string(Self::path().ok()?).ok()?)?.here(&machine_id())
    }

    fn parse(text: &str) -> Option<Self> {
        let opened: Value = serde_json::from_str(text).ok()?;
        Some(Opened {
            needs: Needs {
                port: u16::try_from(opened.get("port")?.as_u64()?).ok()?,
                udp: opened.get("udp")?.as_bool()?,
                mdns: opened.get("mdns")?.as_bool()?,
            },
            zones: opened
                .get("zones")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            machine: opened.get("machine")?.as_str()?.to_string(),
        })
    }

    fn to_json(&self) -> String {
        json!({
            "port": self.needs.port,
            "udp": self.needs.udp,
            "mdns": self.needs.mdns,
            "zones": self.zones,
            "machine": self.machine,
        })
        .to_string()
    }

    /// Itself if the install with machine ID `machine` wrote it; another install's zones say
    /// nothing about this one's firewalld
    fn here(self, machine: &str) -> Option<Self> {
        (self.machine == machine).then_some(self)
    }

    /// Also lists the zones `old` opened for the same needs, such as another network's
    fn keep(mut self, old: Option<Opened>) -> Self {
        if let Some(old) = old.filter(|old| old.needs == self.needs) {
            self.zones.extend(old.zones);
            self.zones.sort();
            self.zones.dedup();
        }
        self
    }

    /// Whether it let in what `needs` asks for in `zone`
    fn covers(&self, zone: &str, needs: &Needs) -> bool {
        self.zones.iter().any(|z| z == zone)
            && self.needs.port == needs.port
            && (self.needs.udp || !needs.udp)
            && (self.needs.mdns || !needs.mdns)
    }
}

/// This install's machine ID, which any user can read; "" without one
fn machine_id() -> String {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .map(|id| id.trim().to_string())
        .find(|id| !id.is_empty())
        .unwrap_or_default()
}

/// Notes what `lvrr setup-system` just opened in firewalld's zones
fn mark_opened(needs: &Needs) -> Result<(), String> {
    let path = Opened::path()?;
    let opened = Opened {
        needs: *needs,
        zones: firewalld_zones(),
        machine: machine_id(),
    }
    .keep(Opened::read());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    fs::write(&path, opened.to_json()).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Firewall services with hand-written rules, which only root can read
fn rules_services() -> Vec<&'static str> {
    ["nftables", "iptables", "ip6tables"]
        .into_iter()
        .filter(|service| {
            Command::new("systemctl")
                .args(["--quiet", "is-active", &format!("{service}.service")])
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect()
}

/// What to add to hand-written rules. Inserted first, so a reject rule
/// before the end doesn't win.
fn rules_lines(services: &[&str], needs: &Needs) -> Vec<String> {
    let port = needs.port;
    let udp: Vec<String> = [needs.udp.then_some(port), needs.mdns.then_some(MDNS)]
        .into_iter()
        .flatten()
        .map(|port| port.to_string())
        .collect();
    let mut lines = Vec::new();
    for service in services {
        match *service {
            "nftables" => {
                lines.push(format!(
                    "nft insert rule inet filter input tcp dport {port} accept"
                ));
                if !udp.is_empty() {
                    lines.push(format!(
                        "nft insert rule inet filter input udp dport {{ {} }} accept",
                        udp.join(", ")
                    ));
                }
            }
            tool => {
                lines.push(format!("{tool} -I INPUT -p tcp --dport {port} -j ACCEPT"));
                for udp in &udp {
                    lines.push(format!("{tool} -I INPUT -p udp --dport {udp} -j ACCEPT"));
                }
            }
        }
    }
    lines
}

pub fn firewall(config: &Value) -> Check {
    let needs = Needs::from_config(config);
    let (status, message, fixable) = match active_firewall() {
        None => match rules_services()[..] {
            [] => (
                Status::Ok,
                "Neither ufw nor firewalld is on, nothing to open.".to_string(),
                false,
            ),
            ref services => (
                Status::Warn,
                format!(
                    "{} is on, and its rules can't be read without admin rights. Make sure they let in {}, for example with the default table and chain names:\n{}",
                    services.join(" and "),
                    needs.describe(),
                    rules_lines(services, &needs).join("\n")
                ),
                false,
            ),
        },
        Some(Firewall::Firewalld) => {
            let mut blocked = Vec::new();
            // zones whose rules need admin rights to read, and those of them LVRR opened
            let mut hidden = Vec::new();
            let mut opened = Vec::new();
            let mut error = None;
            let zones = firewalld_zones();
            let marker = Opened::read();
            let mut may_list = None;
            for zone in &zones {
                match read_zone(zone, needs.port, &mut may_list) {
                    Ok(Some(open)) if open.missing(&needs).is_empty() => (),
                    Ok(Some(open)) => blocked.push(format!(
                        "zone {zone} blocks {}",
                        open.missing(&needs).join(", ")
                    )),
                    Ok(None) if marker.as_ref().is_some_and(|m| m.covers(zone, &needs)) => {
                        opened.push(zone.as_str())
                    }
                    Ok(None) => hidden.push(zone.as_str()),
                    Err(e) => error = Some(e),
                }
            }
            if !blocked.is_empty() {
                (
                    Status::Fail,
                    format!("firewalld {}.", blocked.join(", ")),
                    true,
                )
            } else if !hidden.is_empty() {
                (
                    Status::Warn,
                    format!(
                        "firewalld's rules for zone {} need admin rights to read. Fixing opens {} there.",
                        hidden.join(", "),
                        needs.describe()
                    ),
                    true,
                )
            } else if let Some(e) = error {
                (
                    Status::Warn,
                    format!("Could not read the firewalld rules: {e}"),
                    true,
                )
            } else if !opened.is_empty() {
                (
                    Status::Ok,
                    format!(
                        "LVRR opened {} in firewalld (zone {}); its rules need admin rights to read.",
                        needs.describe(),
                        opened.join(", ")
                    ),
                    false,
                )
            } else {
                (
                    Status::Ok,
                    format!(
                        "firewalld lets the headset in on {} (zone {}).",
                        needs.describe(),
                        zones.join(", ")
                    ),
                    false,
                )
            }
        }
        Some(Firewall::Ufw) => match read_ufw(needs.port) {
            Ok(Some(open)) if open.missing(&needs).is_empty() => (
                Status::Ok,
                format!("ufw lets the headset in on {}.", needs.describe()),
                false,
            ),
            Ok(Some(open)) => (
                Status::Fail,
                format!("ufw blocks {}.", open.missing(&needs).join(", ")),
                true,
            ),
            // what LVRR did, not what ufw does: `ufw delete` and `ufw reset` keep the profile
            Ok(None) if read_ufw_profile(needs.port).missing(&needs).is_empty() => (
                Status::Ok,
                format!(
                    "LVRR opened {} in ufw; its rules need admin rights to read.",
                    needs.describe()
                ),
                false,
            ),
            Ok(None) => (
                Status::Warn,
                format!(
                    "ufw's rules need admin rights to read. Fixing adds LVRR's rule for {}.",
                    needs.describe()
                ),
                true,
            ),
            Err(e) => (
                Status::Warn,
                format!("Could not read the ufw rules: {e}"),
                true,
            ),
        },
    };
    Check {
        id: "firewall",
        title: "Firewall",
        status,
        message,
        fix: fixable.then_some(FIX),
    }
}

/// The ufw application profile `ufw allow LVRR` opens
fn ufw_profile(needs: &Needs) -> String {
    let server = if needs.udp {
        needs.port.to_string()
    } else {
        format!("{}/tcp", needs.port)
    };
    let mdns = if needs.mdns {
        format!("|{MDNS}/udp")
    } else {
        String::new()
    };
    format!(
        "[LVRR]
title=LVRR
description=LVRR VR streaming server and headset discovery
ports={server}{mdns}
"
    )
}

/// The firewall-cmd calls that open what `needs` asks for in `zone`. Each runs for
/// the runtime config, then the permanent one, as `--reload` would drop the runtime
/// rules of the admin and other tools. Given --add-service with --add-port,
/// firewall-cmd adds only the service, so mDNS gets a call of its own.
fn firewalld_calls(zone: &str, needs: &Needs) -> Vec<Vec<String>> {
    let zone = format!("--zone={zone}");
    let mut ports = vec![zone.clone(), format!("--add-port={}/tcp", needs.port)];
    if needs.udp {
        ports.push(format!("--add-port={}/udp", needs.port));
    }
    let mut calls = vec![ports];
    if needs.mdns {
        calls.push(vec![zone, "--add-service=mdns".to_string()]);
    }
    calls
        .into_iter()
        .flat_map(|runtime| {
            let permanent = [vec!["--permanent".to_string()], runtime.clone()].concat();
            [runtime, permanent]
        })
        .collect()
}

/// `lvrr setup-system [--firewall [--port N] [--tcp-only] [--no-mdns]] [--avahi]`, as root:
/// opens what the headset needs in the active firewall, and starts Avahi, which the server
/// needs to publish itself. Root doesn't see the user's server config, so `fix_system`
/// passes on what the firewall needs. One failing doesn't keep the other from running.
/// An error comes with the exit code: AVAHI_FAILED when only Avahi failed, else 1.
pub fn setup_system(args: &[String]) -> Result<(), (String, u8)> {
    let setup = Setup::from_args(args).map_err(|e| (e, 1))?;
    if !is_root() {
        return Err(("needs admin rights, run `lvrr doctor --fix`".to_string(), 1));
    }
    let mut errors = Vec::new();
    if let Some(needs) = &setup.firewall
        && let Err(e) = setup_firewall(needs)
    {
        errors.push(e);
    }
    let firewall_failed = !errors.is_empty();
    if setup.avahi {
        // checks::avahi() offers this fix with systemd only
        let started = if checks::systemd_booted() {
            checks::run("systemctl", &["enable", "--now", AVAHI_UNIT])
        } else {
            Err(
                "starting Avahi needs systemd, start the avahi-daemon service with your init system"
                    .to_string(),
            )
        };
        if let Err(e) = started {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else if firewall_failed {
        Err((errors.join("; "), 1))
    } else {
        Err((errors.join("; "), AVAHI_FAILED))
    }
}

/// Opens what the headset needs in the active firewall
fn setup_firewall(needs: &Needs) -> Result<(), String> {
    match active_firewall() {
        None => Ok(()),
        Some(Firewall::Ufw) => {
            let old = fs::read(UFW_PROFILE).ok();
            fs::write(UFW_PROFILE, ufw_profile(needs))
                .map_err(|e| format!("writing {UFW_PROFILE}: {e}"))?;
            // `allow` keeps an LVRR rule it already has; this moves it to the profile's ports
            let _ = checks::run("ufw", &["app", "update", "LVRR"]);
            let added = checks::run("ufw", &["allow", "LVRR"]);
            // where users can't read ufw's rules, the check goes by the profile
            if added.is_err() {
                let _ = match old {
                    Some(old) => fs::write(UFW_PROFILE, old),
                    None => fs::remove_file(UFW_PROFILE),
                };
            }
            added?;
            // the check reads it as the user; a new file got the umask pkexec kept, such as
            // 027, and an earlier fix's file keeps the mode it had
            fs::set_permissions(UFW_PROFILE, fs::Permissions::from_mode(0o644))
                .map_err(|e| format!("setting the mode of {UFW_PROFILE}: {e}"))
        }
        Some(Firewall::Firewalld) => {
            for zone in firewalld_zones() {
                for call in firewalld_calls(&zone, needs) {
                    let args: Vec<&str> = call.iter().map(String::as_str).collect();
                    checks::run("firewall-cmd", &args)?;
                }
            }
            Ok(())
        }
    }
}

fn is_root() -> bool {
    fs::read_to_string("/proc/self/status").is_ok_and(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|uids| uids.split_whitespace().nth(1))
            == Some("0")
    })
}

/// Runs `lvrr setup-system` with admin rights; pkexec asks for the password once for all
/// of it
fn fix_system(setup: &Setup) -> Result<(), String> {
    let output = Command::new("pkexec")
        .arg(crate::lvrr_exe()?)
        .arg("setup-system")
        .args(setup.args())
        // kept for what setup-system runs; output() would close it
        .stdin(Stdio::inherit())
        // like checks::run, keeps stdout for `lvrr doctor --json`'s report
        .stdout(std::io::stderr())
        // why it failed, for the dashboard and the first start's notice; pkexec prompts in
        // a terminal on /dev/tty
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("running pkexec: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // still for a terminal or the journal, without setup-system's own error, which the
    // caller reports from what this returns
    for line in stderr.lines().filter(|line| !line.starts_with("lvrr: ")) {
        eprintln!("{line}");
    }
    // the check can't read firewalld's zones on most distributions; with AVAHI_FAILED the
    // firewall opened all the same
    let opened = output.status.success() || output.status.code() == Some(i32::from(AVAHI_FAILED));
    if opened
        && let Some(needs) = &setup.firewall
        && active_firewall() == Some(Firewall::Firewalld)
        && let Err(e) = mark_opened(needs)
    {
        eprintln!("lvrr: {e}");
    }
    if !output.status.success() {
        return Err(setup_error(output.status, &stderr));
    }
    Ok(())
}

/// Why `pkexec lvrr setup-system` failed: pkexec's own exit codes, else the error
/// setup-system printed last
fn setup_error(status: ExitStatus, stderr: &str) -> String {
    match status.code() {
        Some(126) => "the admin password prompt was closed".to_string(),
        Some(127) => {
            "not authorized, or no polkit authentication agent runs on this desktop".to_string()
        }
        _ => stderr
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("lvrr: "))
            .map_or_else(
                || format!("pkexec lvrr setup-system: {status}"),
                str::to_string,
            ),
    }
}

/// On the first start, offers what needs admin rights, with one password: opening the
/// firewall if it blocks the headset or its rules can't be read, and starting Avahi if the
/// server needs it to publish itself. Asks only once for each; after that, `lvrr doctor`
/// tells about them, and the start's report about those that fail. Hand-written nftables
/// or iptables rules are left alone. Returns the checks it ran and didn't act on, for the
/// start's report.
pub fn first_start(config_dir: &Path, config: &Value) -> Vec<Check> {
    let mut kept = Vec::new();
    let mut offered = Vec::new();
    let quick = [
        (ASKED_FILE, firewall as fn(&Value) -> Check),
        (AVAHI_ASKED_FILE, checks::avahi),
    ];
    for (file, run) in quick {
        let asked = config_dir.join(file);
        if asked.exists() {
            continue;
        }
        let check = run(config);
        if check.status == Status::Ok || check.fix.is_none() {
            kept.push(check);
        } else if let Err(e) = fs::write(&asked, "") {
            // without the marker it would ask at every start
            eprintln!("lvrr: writing {}: {e}", asked.display());
            kept.push(check);
        } else {
            offered.push(check);
        }
    }
    if offered.is_empty() {
        return kept;
    }
    let problems: Vec<String> = offered
        .iter()
        .map(|c| format!("{}: {}", c.title, c.message))
        .collect();
    crate::report(
        "Setting up this PC for LVRR",
        &format!("{}\nThis asks for an admin password.", problems.join("\n")),
    );
    let setup = Setup {
        firewall: offered
            .iter()
            .any(|c| c.id == "firewall")
            .then(|| Needs::from_config(config)),
        avahi: offered.iter().any(|c| c.id == "avahi"),
    };
    if let Err(e) = fix_system(&setup) {
        let again = if in_window() {
            "Try again from PC > Diagnostics.".to_string()
        } else {
            format!("Run `{FIX}` to try again.")
        };
        crate::report("LVRR could not set up this PC", &format!("{e}. {again}"));
    }
    kept
}

fn checks(root: &Path) -> Vec<Check> {
    let config = crate::read_config();
    let session_running = bus::session().is_ok_and(|conn| {
        matches!(server::state(&conn, root), State::Ours)
            && Server::new(&conn)
                .get::<bool>("SessionRunning")
                .unwrap_or(false)
    });
    // read once for both Steam checks
    let flatpak = checks::steam_flatpak();
    vec![
        // first: when it fails, the checks below judge the server's defaults
        checks::config(),
        firewall(&config),
        checks::avahi(&config),
        checks::port(root, &config),
        checks::vulkan(),
        checks::openxr(root, session_running),
        checks::openvr(root, &config),
        checks::steam(root, flatpak.as_ref()),
        checks::pressure_vessel(flatpak.as_ref()),
        checks::uinput(&config),
        checks::usb(root, &config),
        checks::pipewire(),
    ]
}

/// The checks `lvrr doctor --fix` fixes: those that aren't OK and have a fix, or with
/// `--fix ID` the check `only` if it is one of them
fn fixable(report: &[Check], only: Option<&str>) -> Vec<&'static str> {
    report
        .iter()
        .filter(|c| c.status != Status::Ok && c.fix.is_some())
        .filter(|c| only.is_none_or(|id| id == c.id))
        .map(|c| c.id)
        .collect()
}

/// What `--fix ID` tells when the check `id` has no fix, as when it is OK
fn no_fix(report: &[Check], id: &str) -> String {
    match &fixable(report, None)[..] {
        [] => format!("{id} has no fix"),
        ids => format!(
            "{id} has no fix, the checks with a fix are {}",
            ids.join(", ")
        ),
    }
}

/// Runs the fixes of the checks `ids`. Returns whether it ran any, and why those that failed did.
fn run_fixes(root: &Path, ids: &[&str]) -> (bool, Vec<String>) {
    let mut errors = Vec::new();
    // one pkexec call, so one password, for what needs root
    let setup = Setup {
        firewall: ids
            .contains(&"firewall")
            .then(|| Needs::from_config(&crate::read_config())),
        avahi: ids.contains(&"avahi"),
    };
    let mut fixed = setup.firewall.is_some() || setup.avahi;
    if fixed && let Err(e) = fix_system(&setup) {
        errors.push(e);
    }
    for id in ids {
        let result = match *id {
            "steam" => checks::fix_steam(root),
            "pressure-vessel" => checks::fix_pressure_vessel(),
            _ => continue,
        };
        if let Err(e) = result {
            errors.push(e);
        }
        fixed = true;
    }
    (fixed, errors)
}

/// `lvrr doctor [--json] [--fix [ID]]`
pub fn main(args: &[String]) -> ExitCode {
    let mut json = false;
    let mut fix = false;
    // `--fix ID`: the one check to fix, for the dashboard's Fix buttons
    let mut only = None;
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--fix" => {
                fix = true;
                only = args.next_if(|id| !id.starts_with('-')).map(String::as_str);
            }
            _ => {
                eprintln!("lvrr doctor: unknown argument {arg}");
                return ExitCode::FAILURE;
            }
        }
    }
    let root = match crate::install_root() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("lvrr: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut report = checks(&root);
    if let Some(id) = only
        && !report.iter().any(|c| c.id == id)
    {
        let ids: Vec<&str> = report.iter().map(|c| c.id).collect();
        eprintln!(
            "lvrr doctor: no check {id}, the checks are {}",
            ids.join(", ")
        );
        return ExitCode::FAILURE;
    }
    let (fixed, errors) = if fix {
        let ids = fixable(&report, only);
        // not in the JSON's errors: the dashboard offers Fix only where there is one, and
        // the check may have turned OK since
        if let Some(id) = only
            && ids.is_empty()
        {
            eprintln!("lvrr doctor: {}", no_fix(&report, id));
        }
        run_fixes(&root, &ids)
    } else {
        (false, Vec::new())
    };
    // with --json, the report carries them
    if !json {
        for e in &errors {
            eprintln!("lvrr: {e}");
        }
    }
    if fixed {
        report = checks(&root);
    }
    if json {
        println!("{}", to_json(&report, &errors));
    } else {
        for c in &report {
            let message = c.message.replace('\n', "\n      ");
            println!("{:4}  {}: {message}", c.status.name(), c.title);
            // that check's alone; without the ID it fixes them all, with one password
            if let Some(fix) = c.fix {
                println!("      fix: {fix} {}", c.id);
            }
        }
    }
    if report.iter().any(|c| c.status == Status::Fail) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The report read by the dashboard, see `lvrr_common::doctor`, with why fixes failed
fn to_json(checks: &[Check], errors: &[String]) -> String {
    let items: Vec<Value> = checks
        .iter()
        .map(|c| {
            let mut item = json!({
                "id": c.id,
                "title": c.title,
                "status": c.status.name(),
                "message": c.message,
            });
            if let Some(fix) = c.fix {
                item["fix"] = fix.into();
            }
            item
        })
        .collect();
    let mut report = json!({ "checks": items });
    if !errors.is_empty() {
        report["errors"] = errors.into();
    }
    report.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // From a ufw with the ports opened by hand, and an mDNS rule by source port only
    const UFW: &str = "\
### tuple ### allow any any 0.0.0.0/0 5353 0.0.0.0/0 in
-A ufw-user-input -p udp --sport 5353 -j ACCEPT
### tuple ### allow any 9757 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp --dport 9757 -j ACCEPT
### tuple ### allow any any 0.0.0.0/0 9757 0.0.0.0/0 in
";

    /// What the server needs without a config
    const DEFAULT: Needs = Needs {
        port: 9757,
        udp: true,
        mdns: true,
    };

    #[test]
    fn needs() {
        assert_eq!(Needs::from_config(&Value::Null), DEFAULT);
        let config = json!({"port": 9800, "tcp-only": true, "publish-service": null});
        let custom = Needs {
            port: 9800,
            udp: false,
            mdns: false,
        };
        assert_eq!(Needs::from_config(&config), custom);
        assert_eq!(DEFAULT.describe(), "TCP and UDP port 9757 and mDNS");
        assert_eq!(custom.describe(), "TCP port 9800");
    }

    #[test]
    fn setup_args() {
        let parse = |args: &[&str]| {
            Setup::from_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };
        let setup = |firewall, avahi| Setup { firewall, avahi };
        // the firewall's default to what the server needs without a config
        assert_eq!(parse(&["--firewall"]), Ok(setup(Some(DEFAULT), false)));
        let custom = Needs {
            port: 9800,
            udp: false,
            mdns: false,
        };
        // as fix_system passes them on
        for wanted in [
            setup(Some(DEFAULT), false),
            setup(Some(custom), true),
            setup(None, true),
        ] {
            assert_eq!(Setup::from_args(&wanted.args()), Ok(wanted));
        }
        assert!(parse(&[]).is_err());
        // the firewall's options without it
        assert!(parse(&["--avahi", "--tcp-only"]).is_err());
        assert!(parse(&["--firewall", "--port", "0"]).is_err());
        assert!(parse(&["--firewall", "--port", "65536"]).is_err());
        assert!(parse(&["--firewall", "--port"]).is_err());
        assert!(parse(&["--firewall", "9800"]).is_err());
    }

    #[test]
    fn ufw_profile_ports() {
        assert!(ufw_profile(&DEFAULT).ends_with("\nports=9757|5353/udp\n"));
        let tcp_only = Needs {
            port: 9800,
            udp: false,
            ..DEFAULT
        };
        assert!(ufw_profile(&tcp_only).ends_with("\nports=9800/tcp|5353/udp\n"));
        let no_mdns = Needs {
            port: 9800,
            mdns: false,
            ..DEFAULT
        };
        assert!(ufw_profile(&no_mdns).ends_with("\nports=9800\n"));
        // the check reads them back where ufw's rules are root's
        let read = |needs: &Needs, port| ufw_profile_open(&ufw_profile(needs), port);
        assert!(read(&DEFAULT, 9757).missing(&DEFAULT).is_empty());
        assert!(read(&tcp_only, 9800).missing(&tcp_only).is_empty());
        let custom = Needs {
            port: 9800,
            ..DEFAULT
        };
        assert_eq!(read(&tcp_only, 9800).missing(&custom), ["UDP port 9800"]);
        assert_eq!(
            read(&no_mdns, 9800).missing(&custom),
            ["mDNS (UDP port 5353)"]
        );
        assert_eq!(
            read(&DEFAULT, 9800).missing(&custom),
            ["TCP port 9800", "UDP port 9800"]
        );
        let range = "[LVRR]\nports=9757/tcp|9700:9800/udp\n";
        assert_eq!(
            ufw_profile_open(range, 9757).missing(&DEFAULT),
            ["mDNS (UDP port 5353)"]
        );
    }

    #[test]
    fn ufw_rules() {
        assert_eq!(
            ufw_open(UFW, 9757),
            Open {
                tcp: true,
                udp: true,
                mdns: false
            }
        );
        // mDNS isn't needed while publishing is off
        let no_mdns = Needs {
            mdns: false,
            ..DEFAULT
        };
        assert!(ufw_open(UFW, 9757).missing(&no_mdns).is_empty());
        let profile = "### tuple ### allow tcp 9757 0.0.0.0/0 any 0.0.0.0/0 LVRR - in\n\
                       ### tuple ### allow udp 9757,5353 0.0.0.0/0 any 0.0.0.0/0 LVRR - in\n";
        assert!(ufw_open(profile, 9757).missing(&DEFAULT).is_empty());
        let custom = Needs {
            port: 9800,
            ..DEFAULT
        };
        assert_eq!(
            ufw_open(profile, 9800).missing(&custom),
            ["TCP port 9800", "UDP port 9800"]
        );
        let tcp = "### tuple ### allow tcp 9800 0.0.0.0/0 any 0.0.0.0/0 LVRR - in\n";
        let tcp_only = Needs {
            port: 9800,
            udp: false,
            mdns: false,
        };
        assert!(ufw_open(tcp, 9800).missing(&tcp_only).is_empty());
        let outgoing = "### tuple ### allow any 9757 0.0.0.0/0 any 0.0.0.0/0 out\n";
        assert_eq!(ufw_open(outgoing, 9757), Open::default());
        let range = "### tuple ### allow udp 5000:6000 0.0.0.0/0 any 192.168.1.0/24 in\n";
        assert!(ufw_open(range, 9757).mdns);
        // `ufw allow 9757 comment VR`, and a logged mDNS rule
        let commented = "### tuple ### allow any 9757 0.0.0.0/0 any 0.0.0.0/0 in comment=5652\n\
                         ### tuple ### allow_log udp 5353 0.0.0.0/0 any 0.0.0.0/0 in\n";
        assert!(ufw_open(commented, 9757).missing(&DEFAULT).is_empty());
        let route = "### tuple ### route:allow_log any 9757 0.0.0.0/0 any 0.0.0.0/0 in\n";
        assert_eq!(ufw_open(route, 9757), Open::default());
    }

    #[test]
    fn firewalld_zone() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<zone>
  <short>Public</short>
  <service name="ssh"/>
  <service name="mdns"/>
  <port port="9757" protocol="tcp"/>
  <port protocol="udp" port="9700-9800"/>
</zone>"#;
        assert!(firewalld_zone_open(xml, 9757).missing(&DEFAULT).is_empty());
        let custom = Needs {
            port: 9800,
            ..DEFAULT
        };
        assert_eq!(
            firewalld_zone_open(xml, 9800).missing(&custom),
            ["TCP port 9800"]
        );
        let service = r#"<zone><service name="wivrn"/></zone>"#;
        assert_eq!(
            firewalld_zone_open(service, 9757).missing(&DEFAULT),
            ["mDNS (UDP port 5353)"]
        );
        // WiVRn's service opens the default port only
        assert_eq!(firewalld_zone_open(service, 9800).missing(&custom).len(), 3);
        let tcp = r#"<zone><port port="9800" protocol="tcp"/></zone>"#;
        let tcp_only = Needs {
            port: 9800,
            udp: false,
            mdns: false,
        };
        assert!(firewalld_zone_open(tcp, 9800).missing(&tcp_only).is_empty());
        let trusted = r#"<zone target="ACCEPT"><short>Trusted</short></zone>"#;
        assert!(
            firewalld_zone_open(trusted, 9757)
                .missing(&DEFAULT)
                .is_empty()
        );
        let drop = r#"<zone target="DROP"><service name="ssh"/></zone>"#;
        assert_eq!(firewalld_zone_open(drop, 9757).missing(&DEFAULT).len(), 3);
    }

    #[test]
    fn firewalld_list() {
        // `firewall-cmd --zone=public --list-all`; forwarded and source ports and rich
        // rules don't count
        let list = "public (default, active)
  target: default
  ingress-priority: 0
  egress-priority: 0
  icmp-block-inversion: no
  interfaces: wlp2s0
  sources:
  services: dhcpv6-client mdns ssh
  ports: 9757/tcp 9700-9800/udp
  protocols:
  forward: yes
  masquerade: no
  forward-ports:
\tport=9800:proto=tcp:toport=22:toaddr=
  source-ports: 9800/tcp
  icmp-blocks:
  rich rules:
\trule family=\"ipv6\" source address=\"fe80::/10\" port port=\"9800\" protocol=\"tcp\" accept
";
        assert!(firewalld_list_open(list, 9757).missing(&DEFAULT).is_empty());
        let custom = Needs {
            port: 9800,
            ..DEFAULT
        };
        assert_eq!(
            firewalld_list_open(list, 9800).missing(&custom),
            ["TCP port 9800"]
        );
        let trusted = "trusted (active)\n  target: ACCEPT\n  services:\n  ports:\n";
        assert!(
            firewalld_list_open(trusted, 9800)
                .missing(&custom)
                .is_empty()
        );
        let service = "home\n  target: DROP\n  services: ssh wivrn\n  ports:\n";
        assert_eq!(
            firewalld_list_open(service, 9757).missing(&DEFAULT),
            ["mDNS (UDP port 5353)"]
        );
        assert_eq!(firewalld_list_open(service, 9800).missing(&custom).len(), 3);
    }

    #[test]
    fn firewalld_marker() {
        let zones = |names: &[&str]| names.iter().map(|z| z.to_string()).collect();
        let machine = || "4e1f".to_string();
        let home = Opened {
            needs: DEFAULT,
            zones: zones(&["home"]),
            machine: machine(),
        };
        assert_eq!(Opened::parse(&home.to_json()).as_ref(), Some(&home));
        assert!(Opened::parse("").is_none());
        assert!(Opened::parse(r#"{"port": 9757, "zones": ["home"]}"#).is_none());
        // without the install that wrote it
        let no_machine = r#"{"port": 9757, "udp": true, "mdns": true, "zones": ["home"]}"#;
        assert!(Opened::parse(no_machine).is_none());
        // another install's, from a home they share, says nothing about this firewalld
        let read = |id: &str| Opened::parse(&home.to_json())?.here(id);
        assert_eq!(read("4e1f").as_ref(), Some(&home));
        assert!(read("9a07").is_none());
        assert!(home.covers("home", &DEFAULT));
        // a zone it didn't open, such as another network's
        assert!(!home.covers("public", &DEFAULT));
        // the port changed since
        let custom = Needs {
            port: 9800,
            ..DEFAULT
        };
        assert!(!home.covers("home", &custom));
        let tcp_only = Needs {
            udp: false,
            ..DEFAULT
        };
        assert!(home.covers("home", &tcp_only));
        let tcp_home = Opened {
            needs: tcp_only,
            zones: zones(&["home"]),
            machine: machine(),
        };
        assert!(tcp_home.covers("home", &tcp_only));
        assert!(!tcp_home.covers("home", &DEFAULT));
        // zones opened for the same needs stay, others go
        let work = Opened {
            needs: DEFAULT,
            zones: zones(&["work"]),
            machine: machine(),
        };
        assert_eq!(work.keep(Some(home)).zones, ["home", "work"]);
        let custom_work = Opened {
            needs: custom,
            zones: zones(&["work"]),
            machine: machine(),
        };
        assert_eq!(custom_work.keep(Some(tcp_home)).zones, ["work"]);
    }

    #[test]
    fn firewalld_active_zones() {
        let text = "home (default)\n  interfaces: wlp2s0 enp3s0\ndocker\n  interfaces: docker0\nlibvirt\n  sources: 192.168.122.0/24\n";
        assert_eq!(
            active_zones(text),
            [
                (
                    "home".to_string(),
                    vec!["wlp2s0".to_string(), "enp3s0".to_string()]
                ),
                ("docker".to_string(), vec!["docker0".to_string()]),
                ("libvirt".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn default_routes() {
        let ipv4 =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlp2s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlp2s0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
";
        let ipv6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000258 00000002 00000000 00000003 enp3s0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001 wlp2s0
";
        assert_eq!(default_route_interfaces(ipv4, ipv6), ["enp3s0", "wlp2s0"]);
    }

    #[test]
    fn hand_written_rules() {
        let lines = rules_lines(&["nftables", "ip6tables"], &DEFAULT);
        assert_eq!(
            lines[1],
            "nft insert rule inet filter input udp dport { 9757, 5353 } accept"
        );
        assert_eq!(lines[4], "ip6tables -I INPUT -p udp --dport 5353 -j ACCEPT");
        assert_eq!(lines.len(), 5);
        let tcp_only = Needs {
            port: 9800,
            udp: false,
            ..DEFAULT
        };
        assert_eq!(
            rules_lines(&["nftables", "iptables"], &tcp_only),
            [
                "nft insert rule inet filter input tcp dport 9800 accept",
                "nft insert rule inet filter input udp dport { 5353 } accept",
                "iptables -I INPUT -p tcp --dport 9800 -j ACCEPT",
                "iptables -I INPUT -p udp --dport 5353 -j ACCEPT",
            ]
        );
        let no_mdns = Needs {
            mdns: false,
            ..tcp_only
        };
        assert_eq!(
            rules_lines(&["nftables"], &no_mdns),
            ["nft insert rule inet filter input tcp dport 9800 accept"]
        );
    }

    #[test]
    fn firewalld_setup() {
        let calls = |zone: &str, needs: &Needs| -> Vec<String> {
            firewalld_calls(zone, needs)
                .iter()
                .map(|call| call.join(" "))
                .collect()
        };
        // runtime and permanent alike, without a reload; mDNS in a call of its own
        assert_eq!(
            calls("home", &DEFAULT),
            [
                "--zone=home --add-port=9757/tcp --add-port=9757/udp",
                "--permanent --zone=home --add-port=9757/tcp --add-port=9757/udp",
                "--zone=home --add-service=mdns",
                "--permanent --zone=home --add-service=mdns",
            ]
        );
        let tcp_only = Needs {
            port: 9800,
            udp: false,
            mdns: false,
        };
        assert_eq!(
            calls("public", &tcp_only),
            [
                "--zone=public --add-port=9800/tcp",
                "--permanent --zone=public --add-port=9800/tcp",
            ]
        );
    }

    #[test]
    fn report_json() {
        let checks = [
            Check {
                id: "firewall",
                title: "Firewall",
                status: Status::Fail,
                message: "ufw blocks \"9757\"\n".to_string(),
                fix: Some(FIX),
            },
            Check {
                id: "pipewire",
                title: "Audio",
                status: Status::Ok,
                message: "Running.".to_string(),
                fix: None,
            },
        ];
        // serde_json sorts the keys, the dashboard reads them by name
        let report: Value = serde_json::from_str(&to_json(&checks, &[])).unwrap();
        assert_eq!(
            report,
            json!({"checks": [
                {
                    "id": "firewall",
                    "title": "Firewall",
                    "status": "fail",
                    "message": "ufw blocks \"9757\"\n",
                    "fix": "lvrr doctor --fix"
                },
                {"id": "pipewire", "title": "Audio", "status": "ok", "message": "Running."},
            ]})
        );
        // why fixes failed, only when some did
        let errors = ["the admin password prompt was closed".to_string()];
        let report: Value = serde_json::from_str(&to_json(&checks[1..], &errors)).unwrap();
        assert_eq!(
            report["errors"],
            json!(["the admin password prompt was closed"])
        );
    }

    #[test]
    fn setup_errors() {
        use std::os::unix::process::ExitStatusExt;
        let exit = |code: i32| ExitStatus::from_raw(code << 8);
        // pkexec's own: the prompt dismissed, not authorized or no agent
        let dismissed = "Error executing command as another user: Request dismissed\n";
        assert_eq!(
            setup_error(exit(126), dismissed),
            "the admin password prompt was closed"
        );
        let no_agent = "Error executing command as another user: No authentication agent found.\n";
        assert_eq!(
            setup_error(exit(127), no_agent),
            "not authorized, or no polkit authentication agent runs on this desktop"
        );
        // setup-system's, after what the tools it ran printed
        let stderr = "Error: INVALID_ZONE: home\nlvrr: firewall-cmd --zone=home --add-port=9757/tcp: exit status: 112\n";
        assert_eq!(
            setup_error(exit(1), stderr),
            "firewall-cmd --zone=home --add-port=9757/tcp: exit status: 112"
        );
        // only Avahi failed, after the firewall opened
        let masked = "Failed to enable unit: Unit file /etc/systemd/system/avahi-daemon.service is masked.\nlvrr: systemctl enable --now avahi-daemon.service: exit status: 1\n";
        assert_eq!(
            setup_error(exit(AVAHI_FAILED.into()), masked),
            "systemctl enable --now avahi-daemon.service: exit status: 1"
        );
        assert_eq!(
            setup_error(exit(1), ""),
            "pkexec lvrr setup-system: exit status: 1"
        );
    }

    #[test]
    fn fix_one() {
        let check = |id, status, fix| Check {
            id,
            title: "",
            status,
            message: String::new(),
            fix,
        };
        let report = [
            check("firewall", Status::Fail, Some(FIX)),
            check("avahi", Status::Ok, None),
            check("vulkan", Status::Fail, None),
            check("steam", Status::Warn, Some(FIX)),
        ];
        assert_eq!(fixable(&report, None), ["firewall", "steam"]);
        assert_eq!(fixable(&report, Some("steam")), ["steam"]);
        // nothing to fix there
        assert!(fixable(&report, Some("avahi")).is_empty());
        assert!(fixable(&report, Some("vulkan")).is_empty());
        assert_eq!(
            no_fix(&report, "avahi"),
            "avahi has no fix, the checks with a fix are firewall, steam"
        );
        assert_eq!(no_fix(&report[1..3], "vulkan"), "vulkan has no fix");
    }
}
