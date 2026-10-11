//! Installs what the program needs outside itself: the background service, a menu entry, and
//! permission to use the tablet. This is what makes a bare AppImage a working installation.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::env;

use anyhow::{bail, Context, Result};

use crate::{driver, in_flatpak};

const SERVICE: &str = "inkswitch.service";
const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/70-inkswitch.rules";
const UDEV_RULE: &str = include_str!("../packaging/70-inkswitch.rules");
/// The official driver's rule, which grants the same access.
const VENDOR_UDEV_RULE: &str = "/lib/udev/rules.d/10-xp-pen.rules";
const DESKTOP_ENTRY: &str = include_str!("../packaging/inkswitch.desktop");
const ICON: &str = include_str!("../packaging/inkswitch.svg");

/// Where this program lives: the AppImage file when run from one, not its temporary mount.
fn program() -> Result<PathBuf> {
    match env::var_os("APPIMAGE") {
        Some(appimage) => Ok(PathBuf::from(appimage)),
        None => env::current_exe().context("finding this program's path"),
    }
}

struct Files {
    unit: PathBuf,
    desktop: PathBuf,
    icon: PathBuf,
}

fn files() -> Result<Files> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
    let xdg = |var: &str, default: &str| env::var_os(var).map(PathBuf::from).unwrap_or_else(|| home.join(default));
    let data = xdg("XDG_DATA_HOME", ".local/share");
    Ok(Files {
        unit: xdg("XDG_CONFIG_HOME", ".config").join("systemd/user").join(SERVICE),
        desktop: data.join("applications/inkswitch.desktop"),
        icon: data.join("icons/hicolor/scalable/apps/inkswitch.svg"),
    })
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl").arg("--user").args(args).status().is_ok_and(|s| s.success())
}

/// Whether this user can already create input devices and talk to the tablet.
fn has_device_access() -> bool {
    let uinput = OpenOptions::new().write(true).open("/dev/uinput").is_ok();
    // Without a tablet plugged in there is nothing to try, so only an installed rule counts.
    let tablet = driver::find_tablet().is_some_and(|t| driver::Usb::open(&t).is_ok());
    uinput && tablet
}

/// Installs the udev rule, asking for the administrator password in a dialog.
fn install_udev_rule(staging: &Path) -> Result<()> {
    write(staging, UDEV_RULE)?;
    let script = format!(
        "install -m 644 '{}' '{UDEV_RULE_PATH}' && udevadm control --reload-rules && udevadm trigger",
        staging.display()
    );
    let installed = Command::new("pkexec").args(["sh", "-c", &script]).status().is_ok_and(|s| s.success());
    let _ = fs::remove_file(staging);
    if !installed {
        bail!(
            "could not install the permission rule. Save these lines as {UDEV_RULE_PATH} as root, then replug the tablet:\n\n{UDEV_RULE}"
        );
    }
    Ok(())
}

/// Asks the desktop, through the background portal, to start the driver at login (or not to).
fn request_autostart(enable: bool) -> Result<()> {
    use zbus::zvariant::Value;
    let mut options = std::collections::HashMap::new();
    options.insert("reason", Value::from("Run the tablet driver"));
    options.insert("autostart", Value::from(enable));
    options.insert("commandline", Value::from(vec!["inkswitch", "daemon"]));
    zbus::blocking::Connection::session()?
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Background"),
            "RequestBackground",
            &("", options),
        )
        .context("asking the desktop to start the driver at login")?;
    Ok(())
}

/// Inside a Flatpak sandbox there is no systemd and no way to become root: the driver runs as
/// a second instance of the application, started at login by the desktop.
fn install_in_flatpak() -> Result<()> {
    if !Path::new(&format!("/run/host{UDEV_RULE_PATH}")).exists() && !has_device_access() {
        bail!(
            "this user may not use the tablet yet. Run this once in a terminal, then replug the tablet and try again:\n\n\
             sudo tee {UDEV_RULE_PATH} <<'EOF'\n{UDEV_RULE}EOF\nsudo udevadm control --reload-rules && sudo udevadm trigger"
        );
    }
    request_autostart(true)?;
    // A new instance of the application, so that it keeps running when this window closes.
    Command::new("flatpak-spawn")
        .args(["inkswitch", "daemon"])
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("starting the driver")?;
    println!("the driver is running and will start at login");
    Ok(())
}

pub fn install() -> Result<()> {
    if in_flatpak() {
        return install_in_flatpak();
    }
    let files = files()?;
    let program = program()?;
    let program = program.to_string_lossy();
    if program.contains(char::is_whitespace) {
        bail!("{program} has a space in its path; move it somewhere without one and run this again");
    }

    write(&files.icon, ICON)?;
    write(&files.desktop, &DESKTOP_ENTRY.replace("Exec=inkswitch", &format!("Exec={program}")))?;
    write(
        &files.unit,
        &format!(
            "[Unit]\nDescription=Inkswitch tablet driver and per-application profiles\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n\
             [Service]\nExecStart={program} daemon\nRestart=on-failure\nRestartSec=3\n\n\
             [Install]\nWantedBy=graphical-session.target\n"
        ),
    )?;

    let rule_present = Path::new(UDEV_RULE_PATH).exists() || Path::new(VENDOR_UDEV_RULE).exists();
    if !rule_present && !has_device_access() {
        install_udev_rule(&files.unit.with_extension("rules"))?;
        println!("installed {UDEV_RULE_PATH}");
    }

    systemctl(&["daemon-reload"]);
    if !systemctl(&["enable", SERVICE]) || !systemctl(&["restart", SERVICE]) {
        bail!("the menu entry is installed, but the background service could not be started; run `{program} daemon` to see why");
    }
    println!("installed the menu entry and the background service for {program}");
    Ok(())
}

pub fn remove() -> Result<()> {
    if in_flatpak() {
        request_autostart(false)?;
        println!("the driver will no longer start at login; it keeps running until you log out");
        return Ok(());
    }
    let files = files()?;
    systemctl(&["disable", "--now", SERVICE]);
    for file in [&files.unit, &files.desktop, &files.icon] {
        let _ = fs::remove_file(file);
    }
    systemctl(&["daemon-reload"]);
    println!("removed the menu entry and the background service; profiles and settings were kept");
    if Path::new(UDEV_RULE_PATH).exists() {
        println!("the permission rule needs root to remove: sudo rm {UDEV_RULE_PATH}");
    }
    Ok(())
}
