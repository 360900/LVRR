//! Builds, installs and packages LVRR from its components.
//!
//! Each component keeps its own build system: xrizer and wayvr are separate
//! cargo workspaces, WiVRn is CMake (and Gradle for the APK).

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod rebrand;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  build [--release] [--prefix P] [--i686] [--only C,...]
        build the components (xrizer, wivrn, wayvr, lvrr) for prefix P
        (default /opt/lvrr); --i686 also builds the 32-bit xrizer
  install [--destdir D]
        install the last build into its prefix, under D if given
  dist [--prefix P]
        release build with --i686, packed into target/dist/lvrr-<version>-x86_64.tar.gz
  apk [--debug] [-- GRADLE_ARGS...]
        build the headset APK into target/dist (needs the Android SDK and NDK)
  run [BUILD_ARGS...] [-- LVRR_ARGS...]
        build into target/stage, install there and start its lvrr, for testing
        from the tree; BUILD_ARGS as for build, without --prefix
  client
        build WiVRn's desktop headset client into target/client-stage, for testing
        the server without a headset
  rebrand [--check]
        show LVRR instead of WiVRn and WayVR to users; run after merging an upstream
  version
        print the LVRR version";

const COMPONENTS: [&str; 4] = ["xrizer", "wivrn", "wayvr", "lvrr"];
const DEFAULT_PREFIX: &str = "/opt/lvrr";
const I686: &str = "i686-unknown-linux-gnu";

/// Set by `cargo run` for xtask itself. RUSTUP_TOOLCHAIN would also override
/// any rust-toolchain.toml inside the components.
const CARGO_RUN_ENV: [&str; 6] = [
    "RUSTUP_TOOLCHAIN",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_CRATE_NAME",
    "CARGO_BIN_NAME",
    "CARGO_PRIMARY_PACKAGE",
];

type Result<T = ()> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let res = match args.first().map(String::as_str) {
        Some("build") => build(rest),
        Some("install") => install(rest),
        Some("dist") => dist(rest),
        Some("apk") => apk(rest),
        Some("client") => client(rest),
        Some("run") => run_staged(rest),
        Some("rebrand") => rebrand::run(rest),
        Some("version") => version().map(|v| println!("{v}")),
        _ => Err(USAGE.into()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn target() -> PathBuf {
    root().join("target")
}

/// Settings of the last `build`, which `install` needs.
struct Build {
    prefix: PathBuf,
    release: bool,
    i686: bool,
}

impl Build {
    fn path() -> PathBuf {
        target().join("lvrr-build")
    }

    fn save(&self) -> Result {
        fs::create_dir_all(target()).map_err(|e| format!("creating target: {e}"))?;
        let text = format!(
            "prefix={}\nrelease={}\ni686={}\n",
            self.prefix.display(),
            self.release,
            self.i686
        );
        fs::write(Self::path(), text).map_err(|e| format!("writing build settings: {e}"))
    }

    fn load() -> Result<Self> {
        let text = fs::read_to_string(Self::path())
            .map_err(|_| "no build found, run `cargo xtask build` first".to_string())?;
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .ok_or_else(|| format!("{}: missing {key}", Self::path().display()))
        };
        Ok(Self {
            prefix: get("prefix")?.into(),
            release: get("release")? == "true",
            i686: get("i686")? == "true",
        })
    }

    fn profile(&self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }
}

fn build(args: &[String]) -> Result {
    let mut b = Build {
        prefix: DEFAULT_PREFIX.into(),
        release: false,
        i686: false,
    };
    let mut only = COMPONENTS.to_vec();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--release" => b.release = true,
            "--i686" => b.i686 = true,
            "--prefix" => b.prefix = value(&mut it, arg)?.into(),
            "--only" => {
                only = value(&mut it, arg)?.split(',').collect();
                if let Some(c) = only.iter().find(|c| !COMPONENTS.contains(c)) {
                    return Err(format!("unknown component {c}"));
                }
            }
            _ => return Err(format!("build: unknown argument {arg}\n\n{USAGE}")),
        }
    }
    if !b.prefix.is_absolute() {
        return Err("--prefix must be an absolute path".into());
    }
    b.save()?;

    let ver = version()?;
    for c in COMPONENTS.iter().filter(|c| only.contains(c)) {
        eprintln!("xtask: building {c}");
        match *c {
            "xrizer" => build_xrizer(&b, &ver)?,
            "wivrn" => build_wivrn(&b, &ver)?,
            "wayvr" => build_wayvr(&b)?,
            "lvrr" => build_lvrr(&b, &ver)?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn build_xrizer(b: &Build, ver: &str) -> Result {
    // A version that changes every commit would rebuild all of xrizer, so dev
    // builds use a fixed one.
    let xrizer_version = if b.release {
        format!("lvrr-{ver}")
    } else {
        "lvrr-dev".into()
    };
    let targets = if b.i686 {
        vec![None, Some(I686)]
    } else {
        vec![None]
    };
    for t in targets {
        let mut cmd = cargo(&root().join("xrizer"));
        cmd.args(["xbuild", "--locked", "--features", "static-openxr"])
            .env("CARGO_TARGET_DIR", target().join("xrizer"))
            .env("XRIZER_VERSION", &xrizer_version);
        if b.release {
            cmd.arg("--release");
        }
        if let Some(t) = t {
            cmd.args(["--target", t]);
        }
        run(cmd)?;
    }
    Ok(())
}

fn build_wivrn(b: &Build, ver: &str) -> Result {
    let dir = target().join("wivrn");
    // Put LVRR's xrizer ahead of the places WiVRn searches by default.
    let compat = format!(
        "{}:{}",
        b.prefix.join("lib/xrizer").display(),
        wivrn_compat_search_path()?
    );
    let mut cmd = Command::new("cmake");
    cmd.arg("-S")
        .arg(root().join("wivrn"))
        .arg("-B")
        .arg(&dir)
        .args(["-G", "Ninja"])
        .arg(format!(
            "-DCMAKE_BUILD_TYPE={}",
            if b.release {
                "Release"
            } else {
                "RelWithDebInfo"
            }
        ))
        .arg(format!("-DCMAKE_INSTALL_PREFIX={}", b.prefix.display()))
        .args([
            "-DCMAKE_INSTALL_LIBDIR=lib",
            "-DWIVRN_BUILD_SERVER=ON",
            "-DWIVRN_BUILD_DASHBOARD=OFF",
            "-DWIVRN_BUILD_CLIENT=OFF",
            "-DWIVRN_BUILD_WIVRNCTL=ON",
            "-DWIVRN_BUILD_TEST=ON",
        ])
        // WiVRn would describe the LVRR checkout anyway; passing it keeps the
        // server and APK on the same string.
        .arg(format!("-DGIT_DESC={ver}"))
        .arg(format!("-DGIT_COMMIT={}", git(&["rev-parse", "HEAD"])?))
        .arg(format!("-DOVR_COMPAT_SEARCH_PATH={compat}"));
    run(cmd)?;
    let mut cmd = Command::new("cmake");
    cmd.arg("--build").arg(&dir);
    run(cmd)
}

/// The upstream default of OVR_COMPAT_SEARCH_PATH, read from WiVRn's CMakeLists.txt.
fn wivrn_compat_search_path() -> Result<String> {
    let path = root().join("wivrn/CMakeLists.txt");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.split_once("set(OVR_COMPAT_SEARCH_PATH \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(value, _)| value.to_string())
        .ok_or_else(|| format!("{}: OVR_COMPAT_SEARCH_PATH not found", path.display()))
}

fn build_wayvr(b: &Build) -> Result {
    let mut cmd = cargo(&root().join("wayvr"));
    cmd.args([
        "build", "--locked", "-p", "wayvr", "-p", "wayvrctl", "-p", "lvrr-ui",
    ])
    .env("CARGO_TARGET_DIR", target().join("wayvr"));
    if b.release {
        cmd.arg("--release");
    }
    run(cmd)
}

fn build_lvrr(b: &Build, ver: &str) -> Result {
    let mut cmd = cargo(root());
    cmd.args(["build", "--locked", "-p", "lvrr"])
        .env("CARGO_TARGET_DIR", target())
        .env("LVRR_VERSION", ver);
    if b.release {
        cmd.arg("--release");
    }
    run(cmd)
}

fn install(args: &[String]) -> Result {
    let mut destdir: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--destdir" => destdir = Some(absolute(value(&mut it, arg)?)?),
            _ => return Err(format!("install: unknown argument {arg}\n\n{USAGE}")),
        }
    }
    let b = Build::load()?;
    let prefix = match &destdir {
        Some(d) => d.join(b.prefix.strip_prefix("/").unwrap()),
        None => b.prefix.clone(),
    };

    let mut cmd = Command::new("cmake");
    cmd.arg("--install").arg(target().join("wivrn"));
    if let Some(d) = &destdir {
        cmd.env("DESTDIR", d);
    }
    run(cmd)?;

    // Only bin/ is needed at runtime, laid out like an OpenVR runtime.
    let profile = b.profile();
    let xrizer = target().join("xrizer");
    let xrizer_dst = prefix.join("lib/xrizer/bin");
    let xrizer_bin = xrizer.join(profile).join("bin");
    copy(
        &xrizer_bin.join("linux64/vrclient.so"),
        &xrizer_dst.join("linux64/vrclient.so"),
        0o755,
    )?;
    copy(
        &xrizer_bin.join("version.txt"),
        &xrizer_dst.join("version.txt"),
        0o644,
    )?;
    if b.i686 {
        copy(
            &xrizer.join(I686).join(profile).join("bin/vrclient.so"),
            &xrizer_dst.join("vrclient.so"),
            0o755,
        )?;
    }

    for bin in ["wayvr", "wayvrctl", "lvrr-ui"] {
        copy(
            &target().join("wayvr").join(profile).join(bin),
            &prefix.join("bin").join(bin),
            0o755,
        )?;
    }
    copy(
        &target().join(profile).join("lvrr"),
        &prefix.join("bin/lvrr"),
        0o755,
    )?;

    // WiVRn's dashboard installed the icon; LVRR doesn't build it.
    copy(
        &root().join("packaging/logo/lvrr.svg"),
        &prefix.join("share/icons/hicolor/scalable/apps/io.github.lvrr.svg"),
        0o644,
    )?;
    let template = root().join("packaging/lvrr.desktop.in");
    let desktop = fs::read_to_string(&template)
        .map_err(|e| format!("{}: {e}", template.display()))?
        .replace("@PREFIX@", &b.prefix.display().to_string());
    // named after the window's app id, so desktops match the two
    let applications = prefix.join("share/applications");
    write(&applications.join("io.github.lvrr.desktop"), &desktop)?;
    match fs::remove_file(applications.join("lvrr.desktop")) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            return Err(format!("removing the old lvrr.desktop: {e}"));
        }
        _ => {}
    }

    eprintln!("xtask: installed to {}", prefix.display());
    Ok(())
}

fn dist(args: &[String]) -> Result {
    let mut prefix = DEFAULT_PREFIX.to_string();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--prefix" => prefix = value(&mut it, arg)?.into(),
            _ => return Err(format!("dist: unknown argument {arg}\n\n{USAGE}")),
        }
    }
    build(&strings(&["--release", "--i686", "--prefix", &prefix]))?;

    let dist = target().join("dist");
    let stage = dist.join("root");
    remove_dir(&stage)?;
    install(&strings(&["--destdir", &stage.display().to_string()]))?;

    let tarball = dist.join(format!("lvrr-{}-x86_64.tar.gz", version()?));
    let mut cmd = Command::new("tar");
    cmd.arg("-C").arg(&stage).arg("-czf").arg(&tarball).arg(".");
    run(cmd)?;
    remove_dir(&stage)?;
    println!("{}", tarball.display());
    Ok(())
}

fn apk(args: &[String]) -> Result {
    let mut debug = false;
    let mut gradle_args: &[String] = &[];
    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "--debug" => debug = true,
            "--" => {
                gradle_args = &args[i + 1..];
                break;
            }
            _ => return Err(format!("apk: unknown argument {arg}\n\n{USAGE}")),
        }
    }

    let ver = version()?;
    let wivrn = root().join("wivrn");
    let (suffix, name, task, kind) = if debug {
        (".debug", "LVRR Debug", "assembleDebug", "debug")
    } else {
        ("", "LVRR", "assembleRelease", "release")
    };
    let mut cmd = Command::new(wivrn.join("gradlew"));
    cmd.current_dir(&wivrn)
        .arg(format!("-Psuffix={suffix}"))
        .arg(format!("-Pwivrn_app_name={name}"))
        .arg(format!(
            "-Pwivrn_version={}",
            ver.strip_prefix('v').unwrap_or(&ver)
        ))
        .arg(format!(
            "-Pwivrn_versionCode={}",
            git(&["rev-list", "--count", "HEAD"])?
        ))
        .arg("-Pwivrn_css=images/wivrn-release.css")
        .arg(format!(
            "-Pfetchcontent_base_dir={}",
            target().join("android-fetch").display()
        ))
        .args(gradle_args)
        .arg(task);
    run(cmd)?;

    let out = wivrn.join("build/outputs/apk").join(kind);
    let entries = fs::read_dir(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let dist = target().join("dist");
    let mut found = false;
    for entry in entries.flatten() {
        let src = entry.path();
        let Some(file) = src.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !file.ends_with(".apk") {
            continue;
        }
        let unsigned = file.contains("unsigned");
        let dst = dist.join(format!(
            "lvrr-{ver}{}{}.apk",
            if debug { "-debug" } else { "" },
            if unsigned { "-unsigned" } else { "" }
        ));
        copy(&src, &dst, 0o644)?;
        if unsigned {
            eprintln!(
                "xtask: {} is unsigned and cannot be installed; set signingKeyPassword in wivrn/gradle.properties and add wivrn/ks.keystore",
                dst.display()
            );
        }
        println!("{}", dst.display());
        found = true;
    }
    if !found {
        return Err(format!("no APK in {}", out.display()));
    }
    Ok(())
}

fn client(args: &[String]) -> Result {
    if let Some(arg) = args.first() {
        return Err(format!("client: unknown argument {arg}\n\n{USAGE}"));
    }
    let dir = target().join("wivrn-client");
    let prefix = target().join("client-stage");
    let mut cmd = Command::new("cmake");
    cmd.arg("-S")
        .arg(root().join("wivrn"))
        .arg("-B")
        .arg(&dir)
        .args(["-G", "Ninja", "-DCMAKE_BUILD_TYPE=RelWithDebInfo"])
        .arg(format!("-DCMAKE_INSTALL_PREFIX={}", prefix.display()))
        .args([
            "-DCMAKE_INSTALL_LIBDIR=lib",
            "-DWIVRN_BUILD_CLIENT=ON",
            "-DWIVRN_BUILD_SERVER=OFF",
            "-DWIVRN_BUILD_SERVER_LIBRARY=OFF",
            "-DWIVRN_BUILD_WIVRNCTL=OFF",
            "-DWIVRN_BUILD_DASHBOARD=OFF",
        ])
        .arg(format!("-DGIT_DESC={}", version()?))
        .arg(format!("-DGIT_COMMIT={}", git(&["rev-parse", "HEAD"])?));
    run(cmd)?;
    let mut cmd = Command::new("cmake");
    cmd.arg("--build").arg(&dir);
    run(cmd)?;
    let mut cmd = Command::new("cmake");
    cmd.arg("--install").arg(&dir);
    run(cmd)?;
    println!("{}", prefix.join("bin/wivrn").display());
    Ok(())
}

/// The LVRR version. WiVRn's GitVersion.cmake runs the same command, so the
/// APK, which Gradle configures on its own, reports the same string.
fn version() -> Result<String> {
    git(&["describe", "--tags", "--always"])
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(root())
        .args(args)
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A cargo command for the workspace in `dir`. Cargo reads `.cargo/config.toml`
/// and `rust-toolchain.toml` from the working directory, not the manifest.
fn cargo(dir: &Path) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(dir);
    for key in CARGO_RUN_ENV {
        cmd.env_remove(key);
    }
    for (key, _) in env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("CARGO_PKG_")) {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn run(mut cmd: Command) -> Result {
    eprintln!("+ {cmd:?}");
    let status = cmd
        .status()
        .map_err(|e| format!("running {:?}: {e}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed: {status}", cmd.get_program()))
    }
}

/// Copies `src` (following symlinks) to `dst`. The old file is removed first,
/// so a running server or game keeps its copy.
fn copy(src: &Path, dst: &Path, mode: u32) -> Result {
    if let Some(dir) = dst.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    match fs::remove_file(dst) {
        Ok(()) => (),
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Err(e) => return Err(format!("removing {}: {e}", dst.display())),
    }
    fs::copy(src, dst).map_err(|e| format!("copying {}: {e}", src.display()))?;
    fs::set_permissions(dst, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("setting mode of {}: {e}", dst.display()))
}

fn write(dst: &Path, contents: &str) -> Result {
    if let Some(dir) = dst.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    fs::write(dst, contents).map_err(|e| format!("writing {}: {e}", dst.display()))
}

fn remove_dir(dir: &Path) -> Result {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", dir.display())),
    }
}

fn absolute(path: &str) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|e| format!("{path}: {e}"))
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a str> {
    it.next()
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Builds and installs into target/stage, then becomes its lvrr
fn run_staged(args: &[String]) -> Result {
    use std::os::unix::process::CommandExt;

    let (build_args, lvrr_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    if build_args.iter().any(|a| a == "--prefix") {
        return Err("run: always uses target/stage as the prefix".into());
    }
    let stage = target().join("stage");
    let mut b = strings(&["--prefix", &stage.to_string_lossy()]);
    b.extend_from_slice(build_args);
    build(&b)?;
    install(&[])?;
    let lvrr = stage.join("bin/lvrr");
    let err = Command::new(&lvrr).args(lvrr_args).exec();
    Err(format!("running {}: {err}", lvrr.display()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}
