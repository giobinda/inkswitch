//! Driver and per-application profiles for XP-Pen tablets on KDE Plasma (Wayland).
//!
//! One background process (`daemon`) talks to the tablet, turns its reports into pen and
//! keyboard input according to the active profile, and switches profiles when KWin reports
//! that an application bound to one got focus. `gui` and the other subcommands edit the
//! profiles and tell the daemon to reload.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::{env, fs, thread};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

mod driver;
mod gui;
mod icons;
mod layout;
mod presets;
mod settings;
mod setup;

use settings::Profile;

const BUS_NAME: &str = "io.github.giobinda.Inkswitch";
const BUS_PATH: &str = "/io/github/giobinda/Inkswitch";
const KWIN_SCRIPT_NAME: &str = "inkswitch";
const KWIN_SCRIPT: &str = include_str!("kwin_script.js");

/// Window class of the `gui` subcommand. As a Flatpak it has to be the application's id, so
/// the desktop finds the right icon.
const GUI_APP_ID: &str = "inkswitch";
const FLATPAK_ID: &str = "io.github.giobinda.Inkswitch";

fn in_flatpak() -> bool {
    env::var_os("FLATPAK_ID").is_some()
}

fn gui_app_id() -> &'static str {
    if in_flatpak() { FLATPAK_ID } else { GUI_APP_ID }
}

/// Whether a window class is our own settings window; focusing it never switches profiles.
fn is_own_window(class: &str) -> bool {
    class.eq_ignore_ascii_case(GUI_APP_ID) || class.eq_ignore_ascii_case(FLATPAK_ID)
}
const RECENT_WINDOWS: usize = 20;
/// The official driver's settings, imported once as the first profile.
const VENDOR_CONFIG: &str = "/usr/lib/pentablet/conf/xppen/config.xml";
/// Where a Flatpak sandbox sees the host's /usr.
const HOST_ROOT: &str = "/run/host";
const VENDOR_BIN: &str = "/usr/lib/pentablet/PenTablet";

#[derive(Parser)]
#[command(version, about = "Driver and per-application profiles for XP-Pen tablets")]
struct Cli {
    #[command(subcommand)]
    /// Opens the settings window when left out
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a profile as a copy of another one (the active one by default) or from a preset
    New {
        name: String,
        #[arg(long)]
        from: Option<String>,
        /// Start from the usual shortcuts of an application: photoshop, krita, gimp, inkscape, blender
        #[arg(long, conflicts_with = "from")]
        preset: Option<String>,
        /// Window class to bind to this profile (see `status` for the last focused window)
        #[arg(long)]
        app: Vec<String>,
    },
    /// Bind a window class to an existing profile
    Bind { name: String, app: String },
    /// Remove the binding for a window class
    Unbind { app: String },
    /// Switch to a profile now
    Use { name: String },
    /// Delete a profile and its bindings
    Remove { name: String },
    /// List profiles and their bound applications
    List,
    /// Show the active profile, the tablet and the last focused window
    Status,
    /// Run the tablet driver and switch profiles with the focused window
    Daemon,
    /// Open the settings window
    Gui {
        /// Open straight on the editor of this profile
        #[arg(long)]
        edit: Option<String>,
    },
    /// Show what a connected tablet reports (stop the daemon first to see raw reports)
    Probe {
        /// Seconds of raw reports to print
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Install the background service, the menu entry and access to the tablet for this user
    Setup {
        /// Undo the installation instead (profiles are kept)
        #[arg(long)]
        remove: bool,
    },
    /// Create the virtual pen without a tablet and sweep it across the screen
    #[command(hide = true)]
    TestPen,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Config {
    /// Profile for applications without a binding, or "keep" to leave the current one active.
    #[serde(default = "keep")]
    fallback: String,
    /// Screen the pen is mapped to; empty leaves the choice to the desktop.
    #[serde(default)]
    output: String,
    /// Window class (lowercase) -> profile name.
    #[serde(default)]
    apps: BTreeMap<String, String>,
}

fn keep() -> String {
    "keep".into()
}

impl Default for Config {
    fn default() -> Self {
        Config { fallback: keep(), output: String::new(), apps: BTreeMap::new() }
    }
}

struct Window {
    class: String,
    name: String,
    caption: String,
}

/// Replaces a file in one step so the daemon never reads a half-written one.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Clone)]
struct Paths {
    config: PathBuf,
    profiles: PathBuf,
    state: PathBuf,
}

impl Paths {
    fn new() -> Result<Self> {
        let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
        let xdg = |var: &str, default: &str| {
            env::var_os(var).map(PathBuf::from).unwrap_or_else(|| home.join(default))
        };
        let base = xdg("XDG_CONFIG_HOME", ".config").join("inkswitch");
        let state = xdg("XDG_STATE_HOME", ".local/state").join("inkswitch");
        let profiles = base.join("profiles");
        fs::create_dir_all(&profiles)?;
        fs::create_dir_all(&state)?;
        Ok(Paths { config: base.join("config.toml"), profiles, state })
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.profiles.join(format!("{name}.toml"))
    }

    fn load_profile(&self, name: &str) -> Result<Profile> {
        let text = fs::read_to_string(self.profile(name)).with_context(|| format!("no profile named '{name}'"))?;
        let mut profile: Profile = toml::from_str(&text).with_context(|| format!("reading profile '{name}'"))?;
        profile.keys.resize(settings::EXPRESS_KEYS, String::new());
        Ok(profile)
    }

    fn save_profile(&self, name: &str, profile: &Profile) -> Result<()> {
        check_name(name)?;
        write_atomic(&self.profile(name), &toml::to_string_pretty(profile)?)
    }

    fn load_config(&self) -> Result<Config> {
        match fs::read_to_string(&self.config) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", self.config.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.config.display())),
        }
    }

    fn save_config(&self, cfg: &Config) -> Result<()> {
        write_atomic(&self.config, &toml::to_string_pretty(cfg)?)
    }

    fn active(&self) -> Option<String> {
        let s = fs::read_to_string(self.state.join("active")).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    fn set_active(&self, name: &str) -> Result<()> {
        fs::write(self.state.join("active"), name)?;
        let mut used = self.last_used();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        used.insert(name.to_string(), now);
        let lines: Vec<String> = used.iter().map(|(profile, at)| format!("{profile}\t{at}\n")).collect();
        let _ = fs::write(self.state.join("last_used"), lines.concat());
        Ok(())
    }

    /// When each profile was last switched to, in milliseconds since the epoch.
    fn last_used(&self) -> BTreeMap<String, u128> {
        let Ok(text) = fs::read_to_string(self.state.join("last_used")) else { return BTreeMap::new() };
        text.lines()
            .filter_map(|line| line.split_once('\t'))
            .filter_map(|(profile, at)| Some((profile.to_string(), at.parse().ok()?)))
            .collect()
    }

    fn profile_names(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(&self.profiles)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Makes sure there is at least one profile, starting from the official driver's settings
    /// when they are around.
    fn ensure_default(&self) -> Result<()> {
        if !self.profile_names()?.is_empty() {
            return Ok(());
        }
        let imported = fs::read_to_string(VENDOR_CONFIG)
            .or_else(|_| fs::read_to_string(format!("{HOST_ROOT}{VENDOR_CONFIG}")))
            .ok()
            .and_then(|xml| settings::import_vendor(&xml));
        if imported.is_some() {
            eprintln!("created profile 'default' from the official driver's settings");
        }
        self.save_profile("default", &imported.unwrap_or_default())?;
        self.set_active("default")
    }

    /// The profile the driver should be using right now.
    fn current_profile(&self) -> Profile {
        self.active().and_then(|name| self.load_profile(&name).ok()).unwrap_or_default()
    }

    /// Connection state as last reported by the daemon.
    fn tablet_status(&self) -> String {
        fs::read_to_string(self.state.join("tablet")).unwrap_or_default().trim().to_string()
    }

    /// Recently focused applications as (class, title), most recent first.
    fn recent_windows(&self) -> Vec<(String, String)> {
        let Ok(s) = fs::read_to_string(self.state.join("recent_windows")) else { return Vec::new() };
        s.lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(class, title)| (class.to_string(), title.to_string()))
            .collect()
    }

    fn remember_window(&self, win: &Window) {
        let class = win.class.to_lowercase();
        let _ = fs::write(
            self.state.join("last_window"),
            format!("class: {}\nname: {}\ntitle: {}\n", win.class, win.name, win.caption),
        );
        if class.is_empty() || is_own_window(&class) {
            return;
        }
        let mut recent = self.recent_windows();
        recent.retain(|(c, _)| *c != class);
        recent.insert(0, (class, win.caption.replace(['\t', '\n'], " ")));
        recent.truncate(RECENT_WINDOWS);
        let lines: Vec<String> = recent.iter().map(|(c, t)| format!("{c}\t{t}\n")).collect();
        let _ = fs::write(self.state.join("recent_windows"), lines.concat());
    }
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name != "keep"
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("invalid profile name {name:?}: use letters, digits, '-' and '_' (\"keep\" is reserved)");
    }
    Ok(())
}

/// Tells a running daemon to pick up changed profiles or settings.
fn notify_daemon() {
    let Ok(conn) = zbus::blocking::Connection::session() else { return };
    let _ = conn.call_method(Some(BUS_NAME), BUS_PATH, Some(BUS_NAME), "Reload", &());
}

fn new_profile(paths: &Paths, name: &str, from: Option<&str>, apps: &[String]) -> Result<()> {
    check_name(name)?;
    if paths.profile(name).exists() {
        bail!("a profile named '{name}' already exists");
    }
    let source = match from.map(str::to_string).or_else(|| paths.active()) {
        Some(source) => paths.load_profile(&source)?,
        None => Profile::default(),
    };
    paths.save_profile(name, &source)?;
    let mut cfg = paths.load_config()?;
    for a in apps {
        cfg.apps.insert(a.to_lowercase(), name.to_string());
    }
    paths.save_config(&cfg)
}

/// Creates a profile with an application's usual shortcuts and binds that application to it,
/// unless it already has a profile.
fn new_from_preset(paths: &Paths, name: &str, preset: &presets::Preset, layout: &layout::Layout) -> Result<()> {
    check_name(name)?;
    if paths.profile(name).exists() {
        bail!("a profile named '{name}' already exists");
    }
    paths.save_profile(name, &preset.profile(layout)?)?;
    let mut cfg = paths.load_config()?;
    for app in preset.apps {
        cfg.apps.entry(app.to_string()).or_insert_with(|| name.to_string());
    }
    paths.save_config(&cfg)?;
    notify_daemon();
    Ok(())
}

fn bind_app(paths: &Paths, name: &str, app: &str) -> Result<()> {
    if !paths.profile(name).exists() {
        bail!("no profile named '{name}'");
    }
    let mut cfg = paths.load_config()?;
    cfg.apps.insert(app.to_lowercase(), name.to_string());
    paths.save_config(&cfg)
}

fn unbind_app(paths: &Paths, app: &str) -> Result<()> {
    let mut cfg = paths.load_config()?;
    if cfg.apps.remove(&app.to_lowercase()).is_none() {
        bail!("{app} is not bound to a profile");
    }
    paths.save_config(&cfg)
}

fn use_profile(paths: &Paths, name: &str) -> Result<()> {
    if !paths.profile(name).exists() {
        bail!("no profile named '{name}'");
    }
    paths.set_active(name)?;
    notify_daemon();
    Ok(())
}

fn rename_profile(paths: &Paths, old: &str, new: &str) -> Result<()> {
    check_name(new)?;
    if paths.profile(new).exists() {
        bail!("a profile named '{new}' already exists");
    }
    fs::rename(paths.profile(old), paths.profile(new)).with_context(|| format!("no profile named '{old}'"))?;
    let mut cfg = paths.load_config()?;
    for profile in cfg.apps.values_mut().chain([&mut cfg.fallback]) {
        if profile == old {
            *profile = new.to_string();
        }
    }
    paths.save_config(&cfg)?;
    if paths.active().as_deref() == Some(old) {
        paths.set_active(new)?;
    }
    notify_daemon();
    Ok(())
}

fn remove_profile(paths: &Paths, name: &str) -> Result<()> {
    fs::remove_file(paths.profile(name)).with_context(|| format!("no profile named '{name}'"))?;
    let mut cfg = paths.load_config()?;
    cfg.apps.retain(|_, p| p != name);
    if cfg.fallback == name {
        cfg.fallback = keep();
    }
    paths.save_config(&cfg)?;
    if paths.active().as_deref() == Some(name) {
        let _ = fs::remove_file(paths.state.join("active"));
    }
    notify_daemon();
    Ok(())
}

/// Whether the daemon is up, judged by its name on the session bus.
fn daemon_running(conn: Option<&zbus::blocking::Connection>) -> bool {
    let dbus = Some("org.freedesktop.DBus");
    conn.and_then(|c| c.call_method(dbus, "/org/freedesktop/DBus", dbus, "NameHasOwner", &(BUS_NAME,)).ok())
        .and_then(|reply| reply.body().deserialize::<bool>().ok())
        .unwrap_or(false)
}

/// The official driver claims the tablet for itself; both cannot run at once.
fn vendor_driver_running() -> bool {
    let Ok(dir) = fs::read_dir("/proc") else { return false };
    dir.filter_map(|e| e.ok()).any(|e| fs::read_link(e.path().join("exe")).is_ok_and(|exe| exe == Path::new(VENDOR_BIN)))
}

enum Msg {
    Focus(Window),
    Reload,
}

struct Service {
    tx: mpsc::Sender<Msg>,
}

#[zbus::interface(name = "io.github.giobinda.Inkswitch")]
impl Service {
    fn focus(&self, class: String, name: String, caption: String) {
        let _ = self.tx.send(Msg::Focus(Window { class, name, caption }));
    }

    fn reload(&self) {
        let _ = self.tx.send(Msg::Reload);
    }
}

fn load_kwin_script(conn: &zbus::blocking::Connection, paths: &Paths) -> Result<()> {
    let file = paths.state.join("kwin_script.js");
    fs::write(&file, KWIN_SCRIPT)?;
    let kwin = Some("org.kde.KWin");
    let scripting = Some("org.kde.kwin.Scripting");
    conn.call_method(kwin, "/Scripting", scripting, "unloadScript", &(KWIN_SCRIPT_NAME,))?;
    let id: i32 = conn
        .call_method(kwin, "/Scripting", scripting, "loadScript", &(file.to_string_lossy().as_ref(), KWIN_SCRIPT_NAME))?
        .body()
        .deserialize()?;
    if id < 0 {
        bail!("KWin refused to load the focus-tracking script");
    }
    conn.call_method(kwin, format!("/Scripting/Script{id}"), Some("org.kde.kwin.Script"), "run", &())?;
    Ok(())
}

fn target_profile(cfg: &Config, win: &Window) -> Option<String> {
    let lookup = |key: &str| cfg.apps.get(&key.to_lowercase()).cloned();
    lookup(&win.class)
        .or_else(|| lookup(&win.name))
        .or_else(|| (cfg.fallback != "keep").then(|| cfg.fallback.clone()))
}

fn daemon(paths: &Paths) -> Result<()> {
    if vendor_driver_running() {
        bail!("the official XP-Pen driver (PenTablet) is running; quit it first, both cannot use the tablet at once");
    }
    paths.ensure_default()?;
    let (tx, rx) = mpsc::channel();
    let conn = zbus::blocking::connection::Builder::session()?
        .name(BUS_NAME)
        .context("is another inkswitch daemon already running?")?
        .serve_at(BUS_PATH, Service { tx })?
        .build()?;

    let shared = driver::Shared::new(paths.current_profile().compile());
    {
        let (shared, conn, paths, status_paths) = (shared.clone(), conn.clone(), paths.clone(), paths.clone());
        let input_file = paths.state.join("input");
        thread::spawn(move || {
            driver::run(
                shared,
                move |status| {
                    let _ = fs::write(status_paths.state.join("tablet"), status);
                },
                move |pen, _ranges| {
                    let output = paths.load_config().map(|c| c.output).unwrap_or_default();
                    if output.is_empty() {
                        return;
                    }
                    // KWin needs a moment to notice the new device.
                    for _ in 0..20 {
                        thread::sleep(std::time::Duration::from_millis(150));
                        if driver::map_to_output(&conn, pen, &output).is_ok() {
                            return;
                        }
                    }
                    eprintln!("could not map the pen to {output}");
                },
                // Lets the settings window show which key is being pressed.
                move |keys, dial| {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                    let _ = fs::write(&input_file, format!("{keys} {dial} {now}"));
                },
            )
        });
    }
    // Something in the desktop has to tell us which window is in front: a KWin script we load
    // ourselves on Plasma, a shell extension on GNOME. Elsewhere profiles are picked by hand.
    if setup::on_gnome() {
        eprintln!("waiting for the Inkswitch GNOME Shell extension to report window focus (`inkswitch setup` installs it)");
    } else {
        match load_kwin_script(&conn, paths) {
            Ok(()) => eprintln!("watching window focus"),
            Err(e) => eprintln!("profiles will not follow the focused window (this needs KDE Plasma 6 or GNOME): {e:#}"),
        }
    }

    let mut output = paths.load_config().map(|c| c.output).unwrap_or_default();
    while let Ok(msg) = rx.recv() {
        // Re-read every time so changes made by the GUI or CLI take effect immediately.
        let cfg = match paths.load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("{e:#}");
                continue;
            }
        };
        if let Msg::Focus(win) = &msg {
            paths.remember_window(win);
            if !is_own_window(&win.class) {
                if let Some(profile) = target_profile(&cfg, win) {
                    if paths.active().as_deref() != Some(&profile) && paths.profile(&profile).exists() {
                        eprintln!("{} -> profile '{profile}'", win.class);
                        let _ = paths.set_active(&profile);
                    }
                }
            }
        }
        shared.set_profile(paths.current_profile().compile());
        if cfg.output != output {
            output = cfg.output;
            if let Some(pen) = shared.pen_sysname.lock().unwrap().as_deref() {
                if let Err(e) = driver::map_to_output(&conn, pen, &output) {
                    eprintln!("could not map the pen to {output:?}: {e:#}");
                }
            }
        }
    }
    Ok(())
}

/// Checks that the desktop accepts our virtual pen as a drawing tablet, without a tablet.
fn test_pen() -> Result<()> {
    use evdev::{AbsoluteAxisCode as Abs, EventType, InputEvent, KeyCode};
    let ranges = driver::Ranges { max_x: 69080, max_y: 38860, max_pressure: 16383, lines_per_inch: 5080 };
    let mut pen = driver::create_pen("inkswitch test", 0, &ranges)?;
    let sysname = driver::sysname(&mut pen).context("the pen device has no event node")?;
    println!("pen device: {sysname}");
    thread::sleep(std::time::Duration::from_millis(800));
    let conn = zbus::blocking::Connection::session()?;
    for property in ["name", "tabletTool", "size", "outputName"] {
        let reply = conn.call_method(
            Some("org.kde.KWin"),
            format!("/org/kde/KWin/InputDevice/{sysname}"),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.kde.KWin.InputDevice", property),
        );
        match reply {
            Ok(r) => println!("KWin {property}: {:?}", r.body().deserialize::<zbus::zvariant::OwnedValue>()?),
            Err(e) => println!("KWin {property}: unavailable ({e})"),
        }
    }
    for step in 0..=40 {
        let (x, y) = (ranges.max_x * (20 + step) / 80, ranges.max_y * (20 + step) / 80);
        pen.emit(&[
            InputEvent::new(EventType::ABSOLUTE.0, Abs::ABS_X.0, x),
            InputEvent::new(EventType::ABSOLUTE.0, Abs::ABS_Y.0, y),
            InputEvent::new(EventType::KEY.0, KeyCode::BTN_TOOL_PEN.0, 1),
        ])?;
        thread::sleep(std::time::Duration::from_millis(15));
    }
    pen.emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_TOOL_PEN.0, 0)])?;
    Ok(())
}

fn main() -> Result<()> {
    // Die quietly when the reader of our output goes away (`inkswitch list | head -1`).
    // SAFETY: called before any other thread exists; SIG_DFL is always a valid handler.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    let paths = Paths::new()?;
    match cli.cmd.unwrap_or(Cmd::Gui { edit: None }) {
        Cmd::New { name, from, preset, app } => {
            paths.ensure_default()?;
            match preset {
                Some(preset) => {
                    let known = || presets::PRESETS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ");
                    let preset = presets::find(&preset).with_context(|| format!("no preset named '{preset}'; there are: {}", known()))?;
                    let (id, title) = layout::Layout::current_id(zbus::blocking::Connection::session().ok().as_ref());
                    new_from_preset(&paths, &name, preset, &layout::Layout::load(id, title))?;
                    for a in &app {
                        bind_app(&paths, &name, a)?;
                    }
                }
                None => new_profile(&paths, &name, from.as_deref(), &app)?,
            }
            println!("created '{name}'");
        }
        Cmd::Bind { name, app } => {
            bind_app(&paths, &name, &app)?;
            println!("{app} -> '{name}'");
        }
        Cmd::Unbind { app } => unbind_app(&paths, &app)?,
        Cmd::Use { name } => {
            use_profile(&paths, &name)?;
            println!("switched to '{name}'");
        }
        Cmd::Remove { name } => remove_profile(&paths, &name)?,
        Cmd::List => {
            paths.ensure_default()?;
            let cfg = paths.load_config()?;
            let active = paths.active();
            for name in paths.profile_names()? {
                let apps: Vec<&str> =
                    cfg.apps.iter().filter(|(_, p)| **p == name).map(|(a, _)| a.as_str()).collect();
                let mark = if active.as_deref() == Some(&name) { '*' } else { ' ' };
                println!("{mark} {name:<16} {}", apps.join(", "));
            }
            println!("\nother applications: {}", cfg.fallback);
        }
        Cmd::Status => {
            println!("active profile: {}", paths.active().as_deref().unwrap_or("(none)"));
            if daemon_running(zbus::blocking::Connection::session().ok().as_ref()) {
                println!("driver: running, tablet {}", paths.tablet_status());
            } else {
                println!("driver: not running");
            }
            match fs::read_to_string(paths.state.join("last_window")) {
                Ok(w) => print!("last focused window:\n{w}"),
                Err(_) => println!("last focused window: unknown"),
            }
        }
        Cmd::Daemon => daemon(&paths)?,
        Cmd::Gui { edit } => {
            paths.ensure_default()?;
            gui::run(paths, edit)?
        }
        Cmd::Probe { seconds } => driver::probe(seconds)?,
        Cmd::Setup { remove: false } => setup::install()?,
        Cmd::Setup { remove: true } => setup::remove()?,
        Cmd::TestPen => test_pen()?,
    }
    Ok(())
}
