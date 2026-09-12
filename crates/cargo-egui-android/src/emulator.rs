//! Drive an Android emulator: lifecycle, input in egui points, and app-private files.
//!
//! An emulator is the only surface short of a phone that has real insets, a real soft keyboard,
//! real dpi and the platform's own networking, so it is where a layout or lifecycle change is
//! actually checked. Everything here is `adb` and the SDK's `emulator` binary; the value is in
//! knowing which incantation is the right one, because several of the obvious ones fail silently:
//!
//! - `input tap` takes **physical pixels**, while everything in an egui app is points. At 560dpi
//!   that is a 3.5x error, which lands on the wrong widget rather than missing outright.
//! - A file pushed into app storage as root keeps the **SELinux category of the app's previous
//!   install**, so after a reinstall every read is denied and the app just looks freshly set up.
//!   `restorecon` does not fix it: it restores the default label, which carries no category.
//! - `adb shell input text` treats a space as an argument separator; it wants `%s`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use crate::{AndroidEnv, adb_path, resolve_android_env};

#[derive(Subcommand, Clone)]
pub enum EmulatorCmd {
    /// List the AVDs this SDK knows about.
    List,
    /// Start an AVD and wait until Android has finished booting.
    Boot {
        /// AVD name; defaults to the only one, or $EGUI_AVD.
        avd: Option<String>,
        /// Boot from a cold image instead of the saved snapshot.
        #[arg(long)]
        cold: bool,
        /// Erase the AVD's user data first.
        #[arg(long)]
        wipe: bool,
        /// Do not restart adbd as root after boot.
        #[arg(long)]
        no_root: bool,
    },
    /// Shut the running emulator down.
    Kill,
    /// Say whether an emulator is up, and its screen and density.
    Status,
    /// Save a screenshot of the current screen.
    Shot {
        /// Destination PNG.
        file: PathBuf,
    },
    /// Tap, in EGUI POINTS — converted to pixels using the device's own density.
    Tap { x: f32, y: f32 },
    /// Swipe, in EGUI POINTS.
    Swipe {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        /// Duration in milliseconds.
        #[arg(long, default_value_t = 300)]
        ms: u32,
    },
    /// Type into the focused field.
    Text { text: String },
    /// Press a key: back, home, enter, tab, del, up, down, left, right, menu, power.
    Key { name: String },
    /// Copy a file into the app's private `files/` directory, owned and labelled so the app can
    /// read it — the way to seed settings without typing them on a phone screen.
    Put {
        /// Local file.
        local: PathBuf,
        /// Destination relative to the app's `files/` directory, e.g. `my-app/config.json`.
        remote: String,
        /// Android package id; defaults to this crate's `package.metadata.android.package`.
        #[arg(long)]
        package: Option<String>,
    },
    /// Read a file back out of the app's private `files/` directory.
    Get {
        /// Source relative to the app's `files/` directory.
        remote: String,
        /// Local destination.
        local: PathBuf,
        #[arg(long)]
        package: Option<String>,
    },
}

pub fn run(cmd: &EmulatorCmd) -> Result<()> {
    let env = resolve_android_env()?;
    match cmd {
        EmulatorCmd::List => list(&env),
        EmulatorCmd::Boot { avd, cold, wipe, no_root } => {
            boot(&env, avd.as_deref(), *cold, *wipe, !*no_root)
        }
        EmulatorCmd::Kill => kill(&env),
        EmulatorCmd::Status => status(&env),
        EmulatorCmd::Shot { file } => shot(&env, file),
        EmulatorCmd::Tap { x, y } => {
            let s = scale(&env)?;
            adb_ok(&env, &["shell", "input", "tap", &px(*x, s), &px(*y, s)])
        }
        EmulatorCmd::Swipe { x1, y1, x2, y2, ms } => {
            let s = scale(&env)?;
            adb_ok(&env, &[
                "shell", "input", "swipe",
                &px(*x1, s), &px(*y1, s), &px(*x2, s), &px(*y2, s),
                &ms.to_string(),
            ])
        }
        EmulatorCmd::Text { text } => adb_ok(&env, &["shell", "input", "text", &escape_text(text)]),
        EmulatorCmd::Key { name } => {
            let code = keycode(name)?;
            adb_ok(&env, &["shell", "input", "keyevent", &code.to_string()])
        }
        EmulatorCmd::Put { local, remote, package } => put(&env, local, remote, package.as_deref()),
        EmulatorCmd::Get { remote, local, package } => get(&env, remote, local, package.as_deref()),
    }
}

fn emulator_bin(env: &AndroidEnv) -> PathBuf {
    let candidate = env.sdk.join("emulator/emulator");
    if candidate.is_file() { candidate } else { PathBuf::from("emulator") }
}

fn list(env: &AndroidEnv) -> Result<()> {
    let names = avds(env)?;
    if names.is_empty() {
        println!("no AVDs; create one in Android Studio's Device Manager or with avdmanager");
        return Ok(());
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

fn avds(env: &AndroidEnv) -> Result<Vec<String>> {
    let out = Command::new(emulator_bin(env))
        .arg("-list-avds")
        .env("PATH", &env.path)
        .output()
        .context("running emulator -list-avds (is the SDK's emulator package installed?)")?;
    if !out.status.success() {
        bail!("emulator -list-avds failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("INFO"))
        .map(str::to_string)
        .collect())
}

/// Which AVD to boot: the one named, else $EGUI_AVD, else the only one there is.
fn pick_avd(env: &AndroidEnv, asked: Option<&str>) -> Result<String> {
    if let Some(n) = asked {
        return Ok(n.to_string());
    }
    if let Ok(n) = std::env::var("EGUI_AVD") {
        if !n.trim().is_empty() {
            return Ok(n);
        }
    }
    let names = avds(env)?;
    match names.len() {
        0 => bail!("no AVDs; create one in Android Studio's Device Manager or with avdmanager"),
        1 => Ok(names[0].clone()),
        _ => bail!("several AVDs ({}); name one or set EGUI_AVD", names.join(", ")),
    }
}

fn boot(env: &AndroidEnv, avd: Option<&str>, cold: bool, wipe: bool, root: bool) -> Result<()> {
    if let Some(serial) = running(env)? {
        println!("{serial} is already running");
        return Ok(());
    }
    let name = pick_avd(env, avd)?;
    println!("starting {name}");
    let mut cmd = Command::new(emulator_bin(env));
    cmd.arg("-avd").arg(&name).arg("-no-boot-anim");
    if cold {
        cmd.arg("-no-snapshot-load");
    }
    if wipe {
        cmd.arg("-wipe-data");
    }
    // Detached, with its output discarded: this returns once Android is up, and the emulator
    // outlives it.
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.env("PATH", &env.path);
    cmd.spawn().context("spawning the emulator")?;

    let adb = adb_path(env);
    let mut wait = Command::new(&adb);
    wait.arg("wait-for-device").env("PATH", &env.path);
    let status = wait.status().context("running adb wait-for-device")?;
    if !status.success() {
        bail!("adb wait-for-device failed");
    }
    // `wait-for-device` returns as soon as adbd answers, which is long before the UI exists.
    // `sys.boot_completed` is the property that means Android is actually up.
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if getprop(env, "sys.boot_completed")? == "1" {
            if root {
                let _ = adb_out(env, &["root"]);
            }
            println!("booted");
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("emulator did not finish booting within 5 minutes")
}

fn kill(env: &AndroidEnv) -> Result<()> {
    if running(env)?.is_none() {
        println!("no emulator running");
        return Ok(());
    }
    adb_ok(env, &["emu", "kill"])?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if running(env)?.is_none() {
            println!("stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // The process is usually gone well before this; what lingers is adb's own stale row.
    println!("stopped (an `offline` row may sit in `adb devices` briefly; it clears itself)");
    Ok(())
}

fn status(env: &AndroidEnv) -> Result<()> {
    let Some(serial) = running(env)? else {
        println!("no emulator running");
        return Ok(());
    };
    let size = adb_out(env, &["shell", "wm", "size"])?;
    let density = adb_out(env, &["shell", "wm", "density"])?;
    let sdk = getprop(env, "ro.build.version.sdk")?;
    let abis = getprop(env, "ro.product.cpu.abilist")?;
    println!("{serial}  API {sdk}");
    println!("  {}", size.trim());
    println!("  {}", density.trim());
    println!("  abis: {abis}");
    println!("  1 egui point = {:.2} px", scale(env)?);
    Ok(())
}

/// The serial of the attached emulator, if one is up and online.
fn running(env: &AndroidEnv) -> Result<Option<String>> {
    let text = adb_out(env, &["devices"])?;
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(s), Some("device")) if s.starts_with("emulator-") => Some(s.to_string()),
                _ => None,
            }
        })
        .next())
}

fn shot(env: &AndroidEnv, file: &Path) -> Result<()> {
    if let Some(p) = file.parent() {
        if !p.as_os_str().is_empty() {
            std::fs::create_dir_all(p)?;
        }
    }
    let out = Command::new(adb_path(env))
        .args(["exec-out", "screencap", "-p"])
        .env("PATH", &env.path)
        .output()
        .context("running adb exec-out screencap")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!("screencap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    std::fs::write(file, &out.stdout).with_context(|| format!("writing {}", file.display()))?;
    println!("{}", file.display());
    Ok(())
}

/// Physical pixels per egui point, from the device's own reported density.
fn scale(env: &AndroidEnv) -> Result<f32> {
    let text = adb_out(env, &["shell", "wm", "density"])?;
    Ok(parse_density(&text).map(|d| d / 160.0).unwrap_or(1.0))
}

/// The effective density out of `wm density`, preferring an override over the physical value.
pub(crate) fn parse_density(text: &str) -> Option<f32> {
    let value = |prefix: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(prefix))
            .and_then(|v| v.trim().parse::<f32>().ok())
    };
    value("Override density:").or_else(|| value("Physical density:"))
}

fn px(points: f32, scale: f32) -> String {
    format!("{}", (points * scale).round() as i64)
}

/// `input text` splits on spaces, so they have to arrive as `%s`.
pub(crate) fn escape_text(s: &str) -> String {
    s.replace(' ', "%s")
}

pub(crate) fn keycode(name: &str) -> Result<u32> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "home" => 3,
        "back" => 4,
        "up" => 19,
        "down" => 20,
        "left" => 21,
        "right" => 22,
        "power" => 26,
        "enter" => 66,
        "del" | "backspace" => 67,
        "tab" => 61,
        "menu" => 82,
        "escape" | "esc" => 111,
        other => bail!(
            "unknown key `{other}`; try back, home, enter, tab, del, up, down, left, right, menu, escape, power"
        ),
    })
}

fn put(env: &AndroidEnv, local: &Path, remote: &str, package: Option<&str>) -> Result<()> {
    let pkg = package.map(str::to_string).map_or_else(package_from_manifest, Ok)?;
    if !local.is_file() {
        bail!("{} is not a file", local.display());
    }
    let rel = remote.trim_start_matches('/');
    let files = format!("/data/data/{pkg}/files");
    let dest = format!("{files}/{rel}");
    let parent = dest.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| files.clone());

    // Writing under another app's data directory needs root, which an emulator's userdebug build
    // gives us and a production phone does not.
    require_root(env)?;
    adb_ok(env, &["shell", "mkdir", "-p", &parent])?;
    adb_ok(env, &["push", &local.display().to_string(), &dest])?;

    // Owned by the app, or it cannot rewrite its own settings and every save is silently lost.
    let uid = adb_out(env, &["shell", "stat", "-c", "%u", &format!("/data/data/{pkg}")])?;
    let uid = uid.trim();
    if uid.is_empty() {
        bail!("{pkg} is not installed on this device");
    }
    adb_ok(env, &["shell", "chown", "-R", &format!("{uid}:{uid}"), &parent])?;
    // And labelled with the app's *current* SELinux category. Each install picks a new one, so a
    // file pushed as root keeps the previous install's and every read is denied — silently, since
    // an unreadable settings file is indistinguishable from a missing one.
    let ctx = adb_out(env, &["shell", "stat", "-c", "%C", &files])?;
    let ctx = ctx.trim();
    if !ctx.is_empty() {
        adb_ok(env, &["shell", "chcon", "-R", ctx, &parent])?;
    }
    println!("{} -> {dest}", local.display());
    Ok(())
}

fn get(env: &AndroidEnv, remote: &str, local: &Path, package: Option<&str>) -> Result<()> {
    let pkg = package.map(str::to_string).map_or_else(package_from_manifest, Ok)?;
    require_root(env)?;
    let src = format!("/data/data/{pkg}/files/{}", remote.trim_start_matches('/'));
    adb_ok(env, &["pull", &src, &local.display().to_string()])?;
    Ok(())
}

fn require_root(env: &AndroidEnv) -> Result<()> {
    let out = adb_out(env, &["root"])?;
    let lower = out.to_ascii_lowercase();
    if lower.contains("cannot run as root") || lower.contains("production builds") {
        bail!("this device does not allow `adb root`; app-private files need an emulator or a userdebug build");
    }
    // adbd restarts when it changes uid, so the next command would race the socket coming back.
    if lower.contains("restarting") {
        let _ = Command::new(adb_path(env))
            .arg("wait-for-device")
            .env("PATH", &env.path)
            .status();
    }
    Ok(())
}

/// The app's package id from `package.metadata.android.package` in the current directory.
fn package_from_manifest() -> Result<String> {
    let raw = std::fs::read_to_string("Cargo.toml")
        .context("reading Cargo.toml (run from the app directory, or pass --package)")?;
    let manifest: toml::Table = toml::from_str(&raw).context("parsing Cargo.toml")?;
    manifest
        .get("package")
        .and_then(|v| v.get("metadata"))
        .and_then(|v| v.get("android"))
        .and_then(|v| v.get("package"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .context("no package.metadata.android.package in Cargo.toml; pass --package")
}

fn adb_out(env: &AndroidEnv, args: &[&str]) -> Result<String> {
    let out = Command::new(adb_path(env))
        .args(args)
        .env("PATH", &env.path)
        .output()
        .with_context(|| format!("running adb {}", args.join(" ")))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("adb {} failed: {}", args.join(" "), err.trim());
    }
    Ok(text)
}

fn adb_ok(env: &AndroidEnv, args: &[&str]) -> Result<()> {
    let out = adb_out(env, args)?;
    let text = out.trim();
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

fn getprop(env: &AndroidEnv, name: &str) -> Result<String> {
    Ok(adb_out(env, &["shell", "getprop", name])?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_prefers_an_override_over_the_physical_value() {
        assert_eq!(parse_density("Physical density: 560"), Some(560.0));
        assert_eq!(
            parse_density("Physical density: 560\nOverride density: 420"),
            Some(420.0),
            "an override is what the app actually lays out against"
        );
        assert_eq!(parse_density("nothing useful"), None);
    }

    #[test]
    fn points_convert_to_pixels_at_the_device_density() {
        // 560dpi is 3.5x, which is the difference between hitting a button and missing the row.
        assert_eq!(px(122.0, 3.5), "427");
        assert_eq!(px(0.0, 3.5), "0");
        assert_eq!(px(44.0, 1.0), "44");
    }

    #[test]
    fn spaces_survive_input_text() {
        assert_eq!(escape_text("hello world"), "hello%sworld");
        assert_eq!(escape_text("nospace"), "nospace");
    }

    #[test]
    fn keys_are_named_not_numbered() {
        assert_eq!(keycode("back").unwrap(), 4);
        assert_eq!(keycode("BACK").unwrap(), 4);
        assert_eq!(keycode("enter").unwrap(), 66);
        assert!(keycode("frobnicate").is_err());
    }
}
