//! Settings window (`inkswitch gui`): profiles, what they do, and which apps load them.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use eframe::egui;

use super::icons::IconIndex;
use super::layout::Layout;
use super::presets::PRESETS;
use super::settings::{key_name, parse_action, parse_key, pressure_curve, Action, Chord, Profile, PEN_BUTTON_1, PEN_BUTTON_2};
use super::{
    bind_app, daemon_running, keep, new_from_preset, new_profile, notify_daemon, remove_profile, rename_profile, unbind_app,
    gui_app_id, use_profile, vendor_driver_running, Config, Paths,
};

/// How often profiles and settings are re-read from disk.
const REFRESH: Duration = Duration::from_secs(1);
const OUTPUT_REFRESH: Duration = Duration::from_secs(10);
/// How often the tablet's key state is polled, to light up the key being pressed.
const INPUT_POLL: Duration = Duration::from_millis(120);
/// How long a dial step stays lit.
const DIAL_FLASH_MS: u128 = 450;
const MESSAGE_TIME: Duration = Duration::from_secs(6);

const GREEN: egui::Color32 = egui::Color32::from_rgb(60, 170, 90);
const RED: egui::Color32 = egui::Color32::from_rgb(200, 70, 70);

const TILE: egui::Vec2 = egui::vec2(158.0, 186.0);
const TILE_ICON: f32 = 64.0;

/// Everything read from disk, re-read once per `REFRESH`.
#[derive(Default)]
struct Snapshot {
    profiles: Vec<String>,
    cfg: Config,
    active: Option<String>,
    last_used: BTreeMap<String, u128>,
    recent: Vec<(String, String)>,
    daemon: bool,
    vendor_driver: bool,
    tablet: String,
}

#[derive(Clone, PartialEq)]
enum Page {
    /// The profile picker.
    Home,
    /// The editor of one profile.
    Profile(String),
    General,
}

/// A control that can be assigned an action.
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Key(usize),
    DialCw,
    DialCcw,
    PenButton1,
    PenButton2,
}

/// What was done to a tile of the profile picker.
enum TileAction {
    Use(String),
    Edit(String),
}

impl Slot {
    /// The key a control's name is stored under in a profile.
    fn id(self) -> String {
        match self {
            Slot::Key(i) => format!("key{}", i + 1),
            Slot::DialCw => "dial_cw".into(),
            Slot::DialCcw => "dial_ccw".into(),
            Slot::PenButton1 => "pen_button1".into(),
            Slot::PenButton2 => "pen_button2".into(),
        }
    }

    fn action(self, profile: &Profile) -> &str {
        match self {
            Slot::Key(i) => profile.keys.get(i).map_or("", String::as_str),
            Slot::DialCw => &profile.dial_cw,
            Slot::DialCcw => &profile.dial_ccw,
            Slot::PenButton1 => &profile.pen_button1,
            Slot::PenButton2 => &profile.pen_button2,
        }
    }
}

/// The "type a shortcut" dialog.
struct Typing {
    slot: Slot,
    text: String,
    error: Option<String>,
}

struct App {
    paths: Paths,
    conn: Option<zbus::blocking::Connection>,
    layout: Layout,
    icons: IconIndex,
    /// Icon address per window class, looked up once.
    icon_cache: HashMap<String, Option<String>>,
    snap: Snapshot,
    refreshed: Option<Instant>,
    outputs: Vec<String>,
    outputs_refreshed: Option<Instant>,
    page: Page,
    /// The profile in use, for the overview on the main screen.
    in_use: Option<(String, Profile)>,
    /// The shown profile as edited, and as last written to disk.
    edit: Profile,
    saved: Profile,
    /// The slot waiting for a shortcut to be pressed, and whether it is added to what is there.
    recording: Option<(Slot, bool)>,
    typing: Option<Typing>,
    /// Text of the new-profile and rename fields while they are open.
    new_name: Option<String>,
    /// The preset the new profile starts from, or `None` for a copy of the shown profile.
    new_preset: Option<usize>,
    rename: Option<String>,
    manual_class: Option<String>,
    confirm_delete: bool,
    message: Option<(bool, String, Instant)>,
    /// Express keys currently held on the tablet, and the direction of a recent dial step.
    held_keys: u16,
    dial_step: i32,
}

/// Names of the connected screens, as the kernel lists them ("card1-DP-1" is the screen DP-1).
fn list_outputs() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    let mut outputs: Vec<String> = entries
        .flatten()
        .filter(|e| std::fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "connected"))
        .filter_map(|e| e.file_name().to_string_lossy().split_once('-').map(|(_, name)| name.to_string()))
        .collect();
    outputs.sort();
    outputs
}

/// The position of a key as egui reports it, by its Linux key name.
fn key_token(key: egui::Key) -> Option<String> {
    use egui::Key::*;
    let fixed = match key {
        Space => "space",
        Enter => "enter",
        Escape => "esc",
        Tab => "tab",
        Backspace => "backspace",
        Delete => "delete",
        Insert => "insert",
        Home => "home",
        End => "end",
        PageUp => "pageup",
        PageDown => "pagedown",
        ArrowLeft => "left",
        ArrowRight => "right",
        ArrowUp => "up",
        ArrowDown => "down",
        Minus => "minus",
        Equals | Plus => "equal",
        Comma => "comma",
        Period => "dot",
        Slash => "slash",
        Backslash => "backslash",
        OpenBracket => "leftbrace",
        CloseBracket => "rightbrace",
        Semicolon => "semicolon",
        Quote => "apostrophe",
        Backtick => "grave",
        _ => {
            // Letters, digits and function keys are named after themselves.
            let name = key.name().trim_start_matches("Num").to_lowercase();
            return parse_key(&name).is_ok().then_some(name);
        }
    };
    Some(fixed.to_string())
}

fn dot(ui: &mut egui::Ui, color: egui::Color32) {
    // As tall as a button, so it lines up with the text next to it.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, ui.spacing().interact_size.y), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

/// A titled panel grouping related settings.
fn card(ui: &mut egui::Ui, title: &str, contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).corner_radius(8).inner_margin(14).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title).strong().size(17.0));
        ui.add_space(4.0);
        contents(ui);
    });
    ui.add_space(10.0);
}

fn chord_storage_text(chord: &Chord) -> String {
    chord.iter().map(|k| key_name(*k)).collect::<Vec<_>>().join("+")
}

impl App {
    fn slot_text(&mut self, slot: Slot) -> &mut String {
        match slot {
            Slot::Key(i) => &mut self.edit.keys[i],
            Slot::DialCw => &mut self.edit.dial_cw,
            Slot::DialCcw => &mut self.edit.dial_ccw,
            Slot::PenButton1 => &mut self.edit.pen_button1,
            Slot::PenButton2 => &mut self.edit.pen_button2,
        }
    }

    fn say(&mut self, is_error: bool, text: impl Into<String>) {
        self.message = Some((is_error, text.into(), Instant::now()));
    }

    fn open(&mut self, page: Page) {
        self.page = page;
        self.confirm_delete = false;
        self.recording = None;
        self.typing = None;
        self.rename = None;
        self.manual_class = None;
        self.edit = match &self.page {
            Page::Profile(name) => self.paths.load_profile(name).unwrap_or_default(),
            Page::Home | Page::General => Profile::default(),
        };
        self.saved = self.edit.clone();
    }

    fn refresh(&mut self) {
        let p = &self.paths;
        let cfg = match p.load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                self.message = Some((true, format!("{e:#}"), Instant::now()));
                Config::default()
            }
        };
        self.snap = Snapshot {
            profiles: p.profile_names().unwrap_or_default(),
            cfg,
            active: p.active(),
            last_used: p.last_used(),
            recent: p.recent_windows(),
            daemon: daemon_running(self.conn.as_ref()),
            vendor_driver: vendor_driver_running(),
            tablet: p.tablet_status(),
        };
        self.refreshed = Some(Instant::now());
        if self.outputs_refreshed.is_none_or(|t| t.elapsed() >= OUTPUT_REFRESH) {
            self.outputs = list_outputs();
            self.outputs_refreshed = Some(Instant::now());
        }
        let (id, name) = Layout::current_id(self.conn.as_ref());
        if id != self.layout.id {
            self.layout = Layout::load(id, name);
        }

        self.in_use = self.snap.active.clone().and_then(|name| Some((name.clone(), self.paths.load_profile(&name).ok()?)));

        if let Page::Profile(name) = &self.page {
            let profiles = &self.snap.profiles;
            if !profiles.contains(name) {
                self.open(Page::Home);
            } else if self.edit == self.saved {
                // Pick up changes made elsewhere.
                let name = name.clone();
                self.edit = self.paths.load_profile(&name).unwrap_or_default();
                self.saved = self.edit.clone();
            }
        }
    }

    /// Runs an action, shows its outcome and reloads from disk.
    fn run(&mut self, ok: impl Into<String>, action: impl FnOnce(&Paths) -> Result<()>) {
        match action(&self.paths) {
            Ok(()) => self.say(false, ok),
            Err(e) => self.say(true, format!("{e:#}")),
        }
        self.refresh();
    }

    /// Writes the edited profile as soon as it differs from disk.
    fn save_edits(&mut self) {
        let Page::Profile(name) = self.page.clone() else { return };
        if self.edit == self.saved {
            return;
        }
        match self.paths.save_profile(&name, &self.edit) {
            Ok(()) => {
                self.saved = self.edit.clone();
                notify_daemon();
            }
            Err(e) => self.say(true, format!("Could not save: {e:#}")),
        }
    }

    /// Reads which express keys are held and whether the dial just moved.
    fn poll_tablet_input(&mut self) {
        self.held_keys = 0;
        self.dial_step = 0;
        if !self.snap.daemon {
            return;
        }
        let Ok(text) = std::fs::read_to_string(self.paths.state.join("input")) else { return };
        let mut fields = text.split_whitespace();
        let (Some(keys), Some(dial), Some(at)) = (fields.next(), fields.next(), fields.next()) else { return };
        self.held_keys = keys.parse().unwrap_or(0);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        if now.saturating_sub(at.parse().unwrap_or(0)) < DIAL_FLASH_MS {
            self.dial_step = dial.parse().unwrap_or(0);
        }
    }

    /// Turns the next key press into the action of the slot being recorded.
    fn record(&mut self, ctx: &egui::Context) {
        let Some((slot, append)) = self.recording else { return };
        let (key, typed) = ctx.input_mut(|i| {
            let key = i.events.iter().find_map(|e| match e {
                egui::Event::Key { key, physical_key, pressed: true, repeat: false, modifiers } => {
                    Some((physical_key.unwrap_or(*key), *modifiers))
                }
                _ => None,
            });
            let typed = i.events.iter().find_map(|e| match e {
                egui::Event::Text(text) => text.chars().next().filter(|c| !c.is_whitespace() && !c.is_control()),
                _ => None,
            });
            if key.is_some() || typed.is_some() {
                // Keep the key press from also reaching the rest of the window.
                i.events.retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)));
            }
            (key, typed)
        });
        // A typed character says exactly which key and level it came from, AltGr included.
        let command = key.is_some_and(|(_, m)| m.ctrl || m.alt);
        let mut chord = typed.filter(|_| !command).and_then(|c| self.layout.chord_for_char(c));
        if chord.is_none() {
            let Some((key, modifiers)) = key else { return };
            let Some(code) = key_token(key).and_then(|t| parse_key(&t).ok()) else { return };
            let held = [
                (modifiers.ctrl, evdev::KeyCode::KEY_LEFTCTRL),
                (modifiers.shift, evdev::KeyCode::KEY_LEFTSHIFT),
                (modifiers.alt, evdev::KeyCode::KEY_LEFTALT),
            ];
            let mut keys: Chord = held.into_iter().filter(|(down, _)| *down).map(|(_, k)| k).collect();
            keys.push(code);
            chord = Some(keys);
        }
        let Some(chord) = chord else { return };
        self.finish_recording(slot, append, chord_storage_text(&chord));
    }

    /// Stores a recorded shortcut, after the existing one when `append` is set.
    fn finish_recording(&mut self, slot: Slot, append: bool, recorded: String) {
        let current = self.slot_text(slot);
        let keeps = append && matches!(parse_action(current), Ok(Action::Chord(_) | Action::Sequence(_)));
        *current = if keeps { format!("{current} {recorded}") } else { recorded };
        self.recording = None;
    }

    /// An action in words: what to show on its button, and what it types if that isn't obvious.
    fn describe(&self, text: &str) -> (String, Option<String>) {
        let label = |key: &evdev::KeyCode| match *key {
            evdev::KeyCode::BTN_RIGHT => "Right click".to_string(),
            evdev::KeyCode::BTN_MIDDLE => "Middle click".to_string(),
            key => self.layout.label(key),
        };
        let pretty = |chord: &Chord| chord.iter().map(label).collect::<Vec<_>>().join(" + ");
        match parse_action(text) {
            Ok(Action::None) => ("Nothing".into(), None),
            Ok(Action::Stylus1 | Action::Stylus2) => ("Pen button".into(), None),
            Ok(Action::Chord(chord)) => (pretty(&chord), self.layout.typed_char(&chord).map(|c| format!("types  {c}"))),
            Ok(Action::Sequence(chords)) => (chords.iter().map(pretty).collect::<Vec<_>>().join(",  then  "), None),
            Err(_) => (text.to_string(), Some("not understood".into())),
        }
    }

    /// One "label [shortcut] [More]" row of a grid; `lit` marks the control as being pressed.
    fn shortcut_row(&mut self, ui: &mut egui::Ui, label: &str, slot: Slot, lit: bool) {
        let is_pen = matches!(slot, Slot::PenButton1 | Slot::PenButton2);
        let is_dial = matches!(slot, Slot::DialCw | Slot::DialCcw);
        ui.horizontal(|ui| {
            dot(ui, if lit { GREEN } else { egui::Color32::TRANSPARENT });
            let mut text = egui::RichText::new(label);
            if lit {
                text = text.strong();
            }
            ui.label(text);
        });
        ui.horizontal(|ui| {
            // Named after what it does; left empty, the shortcut itself stands in.
            let id = slot.id();
            let mut name = self.edit.names.get(&id).cloned().unwrap_or_default();
            let unnamed = self.describe(slot.action(&self.edit)).0;
            let field = egui::TextEdit::singleline(&mut name).hint_text(unnamed).desired_width(150.0);
            if ui.add(field).on_hover_text("What this does, for the overview. For example: Undo").changed() {
                if name.trim().is_empty() {
                    self.edit.names.remove(&id);
                } else {
                    self.edit.names.insert(id, name);
                }
            }
            let size = egui::vec2(260.0, 30.0);
            if let Some((_, append)) = self.recording.filter(|(s, _)| *s == slot) {
                let waiting = egui::RichText::new("Press keys, or click here…").italics();
                let target = ui
                    .add_sized(size, egui::Button::new(waiting).selected(true))
                    .on_hover_text("Press the shortcut on your keyboard, or click here with the mouse button you want (hold Shift, Ctrl or Alt for a combination)");
                let clicks = [(target.clicked(), "leftclick"), (target.secondary_clicked(), "rightclick"), (target.middle_clicked(), "middleclick")];
                if let Some((_, button)) = clicks.into_iter().find(|(clicked, _)| *clicked) {
                    let modifiers = ui.input(|i| i.modifiers);
                    let held = [(modifiers.ctrl, "ctrl"), (modifiers.shift, "shift"), (modifiers.alt, "alt")];
                    let mut parts: Vec<&str> = held.into_iter().filter(|(down, _)| *down).map(|(_, name)| name).collect();
                    parts.push(button);
                    self.finish_recording(slot, append, parts.join("+"));
                } else if ui.button("Cancel").clicked() {
                    self.recording = None;
                }
                return;
            }
            let current = self.slot_text(slot).clone();
            let (shown, hint) = self.describe(&current);
            let is_empty = matches!(parse_action(&current), Ok(Action::None));
            let mut text = egui::RichText::new(shown);
            text = if is_empty { text.weak() } else { text.strong() };
            if ui.add_sized(size, egui::Button::new(text).selected(lit)).on_hover_text("Click, then press the shortcut on your keyboard or a mouse button").clicked() {
                self.recording = Some((slot, false));
            }
            ui.menu_button("More", |ui| {
                if !is_dial {
                    for (label, value) in [("Hold Ctrl", "ctrl"), ("Hold Shift", "shift"), ("Hold Alt", "alt"), ("Hold Space", "space")] {
                        if ui.button(label).clicked() {
                            *self.slot_text(slot) = value.into();
                            ui.close_menu();
                        }
                    }
                    ui.separator();
                }
                ui.menu_button("Mouse button", |ui| {
                    let plain = [("Click", "leftclick"), ("Right click", "rightclick"), ("Middle click", "middleclick"), ("Double click", "leftclick leftclick")];
                    let combined = [("Shift + Click", "shift+leftclick"), ("Ctrl + Click", "ctrl+leftclick"), ("Alt + Click", "alt+leftclick")];
                    for (i, (label, value)) in plain.into_iter().chain(combined).enumerate() {
                        if i == plain.len() {
                            ui.separator();
                        }
                        if ui.button(label).clicked() {
                            *self.slot_text(slot) = value.into();
                            ui.close_menu();
                        }
                    }
                });
                ui.separator();
                let has_keys = matches!(parse_action(&current), Ok(Action::Chord(_) | Action::Sequence(_)));
                if ui.add_enabled(has_keys, egui::Button::new("Add a second shortcut after this one")).clicked() {
                    self.recording = Some((slot, true));
                    ui.close_menu();
                }
                if ui.button("Type it in…").on_hover_text("For keys that can't be recorded, such as Super or F13").clicked() {
                    let text = parse_action(&current).map(|a| self.layout.action_text(&a)).unwrap_or_default();
                    self.typing = Some(Typing { slot, text, error: None });
                    ui.close_menu();
                }
                ui.separator();
                if is_pen && ui.button("Pen button (the application decides)").clicked() {
                    *self.slot_text(slot) = if slot == Slot::PenButton1 { PEN_BUTTON_1 } else { PEN_BUTTON_2 }.into();
                    ui.close_menu();
                }
                if ui.add_enabled(!is_empty, egui::Button::new("Nothing")).clicked() {
                    self.slot_text(slot).clear();
                    ui.close_menu();
                }
            });
            if let Some(hint) = hint {
                ui.weak(hint);
            }
        });
        ui.end_row();
    }

    fn typing_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut typing) = self.typing.take() else { return };
        let mut done = false;
        let mut cancelled = false;
        egui::Window::new("Type a shortcut")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Write it as on your {} keyboard, for example Ctrl+Z, Shift+Click, RightClick or F5.\nA single character such as \\ is turned into the keys that type it.\nSeparate several shortcuts with spaces to press them one after the other.",
                    self.layout.name
                ));
                ui.add_space(6.0);
                let field = ui.add(egui::TextEdit::singleline(&mut typing.text).desired_width(f32::INFINITY));
                let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                field.request_focus();
                if let Some(error) = &typing.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    done = ui.button("OK").clicked() || entered;
                    cancelled = ui.button("Cancel").clicked();
                });
            });
        if cancelled {
            return;
        }
        if done {
            match self.layout.parse(&typing.text) {
                Ok(stored) => {
                    *self.slot_text(typing.slot) = stored;
                    return;
                }
                Err(e) => typing.error = Some(format!("{e:#}")),
            }
        }
        self.typing = Some(typing);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !self.snap.daemon {
                dot(ui, RED);
                ui.label("The tablet driver is not running");
                if self.snap.vendor_driver {
                    ui.weak("(the official XP-Pen driver is running instead)");
                } else if ui.button("Set up and start").on_hover_text("Installs the background service and, if needed, permission to use the tablet").clicked() {
                    self.run("Driver started", |_| {
                        super::setup::install()?;
                        // Give it a moment so the indicator is right on the reload that follows.
                        std::thread::sleep(Duration::from_millis(400));
                        Ok(())
                    });
                }
            } else if let Some(name) = self.snap.tablet.strip_prefix("connected: ") {
                dot(ui, GREEN);
                ui.label(format!("{name} connected"));
            } else {
                dot(ui, RED);
                ui.label("No tablet connected");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("Keyboard layout: {}", self.layout.name));
            });
        });
    }

    /// The icon of the application a profile is for, if one can be found.
    fn profile_icon(&mut self, name: &str) -> Option<String> {
        let mut classes: Vec<String> = self.snap.cfg.apps.iter().filter(|(_, p)| *p == name).map(|(a, _)| a.clone()).collect();
        // A profile named after an application gets its icon even before being tied to it.
        classes.push(name.to_lowercase());
        let icons = &self.icons;
        classes.into_iter().find_map(|class| self.icon_cache.entry(class.clone()).or_insert_with(|| icons.lookup(&class)).clone())
    }

    /// Background and frame shared by all tiles; returns the tile's area and click response.
    fn tile_frame(ui: &mut egui::Ui, highlighted: bool) -> (egui::Rect, egui::Response) {
        let (rect, response) = ui.allocate_exact_size(TILE, egui::Sense::click());
        let visuals = ui.visuals();
        let fill = if response.contains_pointer() { visuals.widgets.hovered.weak_bg_fill } else { visuals.faint_bg_color };
        let stroke = if highlighted { egui::Stroke::new(2.0_f32, GREEN) } else { visuals.widgets.noninteractive.bg_stroke };
        ui.painter().rect(rect, 12, fill, stroke, egui::StrokeKind::Inside);
        (rect, response)
    }

    /// The area inside a tile, laid out top to bottom and centred.
    fn tile_contents(ui: &mut egui::Ui, rect: egui::Rect) -> egui::Ui {
        let layout = egui::Layout::top_down(egui::Align::Center);
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(10.0)).layout(layout));
        // Text in a tile is part of the button, not something to select.
        inner.style_mut().interaction.selectable_labels = false;
        inner.add_space(6.0);
        inner
    }

    fn profile_tile(&mut self, ui: &mut egui::Ui, name: &str) -> Option<TileAction> {
        let is_active = self.snap.active.as_deref() == Some(name);
        let icon = self.profile_icon(name);
        let apps: Vec<&str> = self.snap.cfg.apps.iter().filter(|(_, p)| *p == name).map(|(a, _)| a.as_str()).collect();
        let (rect, response) = Self::tile_frame(ui, is_active);
        let mut inner = Self::tile_contents(ui, rect);
        match icon {
            Some(address) => {
                inner.add(egui::Image::new(address).fit_to_exact_size(egui::vec2(TILE_ICON, TILE_ICON)));
            }
            None => {
                // No application icon: a coloured disc with the profile's initial.
                let (area, _) = inner.allocate_exact_size(egui::vec2(TILE_ICON, TILE_ICON), egui::Sense::hover());
                let hue = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) % 360;
                let color: egui::Color32 = egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.45, 0.55, 1.0).into();
                inner.painter().circle_filled(area.center(), TILE_ICON / 2.0 - 4.0, color);
                let initial = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
                inner.painter().text(area.center(), egui::Align2::CENTER_CENTER, initial, egui::FontId::proportional(30.0), egui::Color32::WHITE);
            }
        }
        inner.add_space(4.0);
        inner.add(egui::Label::new(egui::RichText::new(name).strong().size(16.0)).truncate());
        if is_active {
            inner.colored_label(GREEN, "In use");
        } else if apps.is_empty() {
            inner.weak("Click to use");
        } else {
            inner.add(egui::Label::new(egui::RichText::new(format!("with {}", apps[0])).weak()).truncate());
        }
        inner.add_space(2.0);
        let edit = inner.button("Edit").on_hover_text("Change what the keys, dial and pen do in this profile");
        if edit.clicked() || response.double_clicked() {
            return Some(TileAction::Edit(name.to_string()));
        }
        let hint = if is_active { "This profile is in use" } else { "Switch the tablet to this profile" };
        response.on_hover_text(hint).clicked().then(|| TileAction::Use(name.to_string()))
    }

    /// What a control is called on the overview, and the shortcut behind the name if it has one.
    fn title(&self, profile: &Profile, slot: Slot) -> (String, Option<String>) {
        let shortcut = self.describe(slot.action(profile)).0;
        match profile.names.get(&slot.id()).map(|n| n.trim()).filter(|n| !n.is_empty()) {
            Some(name) => (name.to_string(), Some(shortcut)),
            None => (shortcut, None),
        }
    }

    /// A drawing of the tablet with what each control does in the profile in use.
    fn overview(&self, ui: &mut egui::Ui) {
        let Some((name, profile)) = &self.in_use else { return };
        card(ui, &format!("{name}: what the tablet does now"), |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 340.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            let visuals = ui.visuals();
            let (strong, weak) = (visuals.strong_text_color(), visuals.weak_text_color());
            let (name_font, detail_font) = (egui::FontId::proportional(15.0), egui::FontId::proportional(12.5));
            let line = egui::Stroke::new(1.0_f32, weak);

            // The tablet: a body with a strip of controls on its left and the screen beside it.
            let labels_width = (rect.width() * 0.45).min(360.0);
            let body = egui::Rect::from_min_max(rect.left_top() + egui::vec2(labels_width, 4.0), rect.right_bottom() - egui::vec2(4.0, 4.0));
            painter.rect_filled(body, 18, egui::Color32::from_gray(48));
            let strip = 56.0;
            let screen = egui::Rect::from_min_max(body.left_top() + egui::vec2(strip + 4.0, 16.0), body.right_bottom() - egui::vec2(16.0, 16.0));
            painter.rect_filled(screen, 6, visuals.extreme_bg_color);

            // Ten rows down the strip: four keys, the dial taking two, four keys.
            let row_height = (body.height() - 28.0) / 10.0;
            let row_y = |row: usize| body.top() + 14.0 + row_height * (row as f32 + 0.5);
            let strip_x = body.left() + strip / 2.0 + 2.0;
            let label_x = body.left() - 26.0;
            let label = |row: usize, slot: Slot, extra: Option<&str>, target_x: f32| {
                let (title, shortcut) = self.title(profile, slot);
                let empty = slot.action(profile).trim().is_empty() && shortcut.is_none();
                let at = egui::pos2(label_x, row_y(row));
                let drawn = painter.text(at, egui::Align2::RIGHT_CENTER, title, name_font.clone(), if empty { weak } else { strong });
                let detail: Vec<&str> = extra.into_iter().chain(shortcut.as_deref()).collect();
                if !detail.is_empty() {
                    let at = egui::pos2(drawn.left() - 12.0, row_y(row));
                    painter.text(at, egui::Align2::RIGHT_CENTER, detail.join("  ·  "), detail_font.clone(), weak);
                }
                painter.line_segment([egui::pos2(label_x + 8.0, row_y(row)), egui::pos2(target_x, row_y(row))], line);
            };
            for key in 0..8 {
                let row = if key < 4 { key } else { key + 2 };
                let cap = egui::Rect::from_center_size(egui::pos2(strip_x, row_y(row)), egui::vec2(30.0, row_height - 9.0));
                let lit = self.held_keys & (1 << key) != 0;
                painter.rect_filled(cap, 4, if lit { GREEN } else { egui::Color32::from_gray(120) });
                label(row, Slot::Key(key), None, cap.left() - 4.0);
            }
            let dial = egui::pos2(strip_x, (row_y(4) + row_y(5)) / 2.0);
            let radius = row_height * 0.72;
            let dial_color = if self.dial_step != 0 { GREEN } else { egui::Color32::from_gray(120) };
            painter.circle_stroke(dial, radius, egui::Stroke::new(5.0_f32, dial_color));
            label(4, Slot::DialCw, Some("dial clockwise"), dial.x - radius - 6.0);
            label(5, Slot::DialCcw, Some("dial counter-clockwise"), dial.x - radius - 6.0);

            // The pen's two buttons, written on the screen.
            let mut at = screen.left_top() + egui::vec2(18.0, 16.0);
            painter.text(at, egui::Align2::LEFT_TOP, "Pen", name_font.clone(), strong);
            for (what, slot) in [("Lower button", Slot::PenButton1), ("Upper button", Slot::PenButton2)] {
                at.y += 26.0;
                let (title, shortcut) = self.title(profile, slot);
                let drawn = painter.text(at, egui::Align2::LEFT_TOP, what, detail_font.clone(), weak);
                let drawn = painter.text(egui::pos2(drawn.right() + 12.0, at.y - 2.0), egui::Align2::LEFT_TOP, title, name_font.clone(), strong);
                if let Some(shortcut) = shortcut {
                    painter.text(egui::pos2(drawn.right() + 12.0, at.y), egui::Align2::LEFT_TOP, shortcut, detail_font.clone(), weak);
                }
            }
            ui.weak("Press a key on the tablet to see it light up. Use Edit on the profile to change or name what the keys do.");
        });
    }

    fn home_page(&mut self, ui: &mut egui::Ui) {
        let mut open_settings = false;
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("Profiles").size(24.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                open_settings = ui.button("Settings").on_hover_text("Screen, keyboard layout and other general settings").clicked();
            });
        });
        ui.weak("Click a profile to switch the tablet to it. A profile tied to an application switches on by itself when that application comes to the front.");
        ui.add_space(12.0);

        // The profile in use first, then the most recently used.
        let mut names = self.snap.profiles.clone();
        names.sort_by_key(|n| (self.snap.active.as_ref() != Some(n), Reverse(self.snap.last_used.get(n).copied().unwrap_or(0)), n.clone()));
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
            for name in &names {
                if let Some(done) = self.profile_tile(ui, name) {
                    action = Some(done);
                }
            }
            let (rect, response) = Self::tile_frame(ui, false);
            let mut inner = Self::tile_contents(ui, rect);
            let (area, _) = inner.allocate_exact_size(egui::vec2(TILE_ICON, TILE_ICON), egui::Sense::hover());
            inner.painter().text(area.center(), egui::Align2::CENTER_CENTER, "+", egui::FontId::proportional(56.0), inner.visuals().weak_text_color());
            inner.add_space(4.0);
            inner.label(egui::RichText::new("New profile").size(16.0));
            inner.weak("Blank or from a preset");
            if response.on_hover_text("Create a profile, for example for another application").clicked() {
                self.new_name = Some(String::new());
            }
        });
        ui.add_space(14.0);
        self.overview(ui);
        match action {
            Some(TileAction::Use(name)) => self.run(format!("Now using '{name}'"), |p| use_profile(p, &name)),
            Some(TileAction::Edit(name)) => self.open(Page::Profile(name)),
            None => {}
        }
        if open_settings {
            self.open(Page::General);
        }
    }

    fn new_profile_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut text) = self.new_name.take() else { return };
        let source = self.snap.active.clone();
        let mut keep_open = true;
        let mut create = false;
        let name = |text: &str| text.trim().replace(' ', "-");
        egui::Window::new("New profile")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label("Start from");
                let copy_label = source.as_ref().map_or("An empty profile".to_string(), |s| format!("A copy of '{s}'"));
                let chosen = self.new_preset.map_or(copy_label.clone(), |i| format!("The usual {} shortcuts", PRESETS[i].title));
                let before = self.new_preset;
                egui::ComboBox::from_id_salt("new-source").selected_text(chosen).width(ui.available_width()).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.new_preset, None, copy_label);
                    for (i, preset) in PRESETS.iter().enumerate() {
                        ui.selectable_value(&mut self.new_preset, Some(i), format!("The usual {} shortcuts", preset.title));
                    }
                });
                if let Some(preset) = self.new_preset.map(|i| &PRESETS[i]) {
                    if before != self.new_preset && (text.trim().is_empty() || PRESETS.iter().any(|p| p.name == text.trim())) {
                        text = preset.name.to_string();
                    }
                    ui.add_space(4.0);
                    egui::Grid::new("preset-preview").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                        for (i, (what, shortcut)) in preset.keys.iter().enumerate() {
                            ui.weak(format!("Key {}", i + 1));
                            ui.weak(format!("{what}  ({shortcut})"));
                            ui.end_row();
                        }
                        ui.weak("Dial");
                        ui.weak(format!("{} and {}  ({} and {})", preset.dial[0].0, preset.dial[1].0.to_lowercase(), preset.dial[0].1, preset.dial[1].1));
                        ui.end_row();
                    });
                    ui.weak("It switches on by itself when the application is in front.");
                }
                ui.add_space(8.0);
                ui.label("Name");
                let field = ui.add(egui::TextEdit::singleline(&mut text).hint_text("for example krita").desired_width(f32::INFINITY));
                let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let exists = self.snap.profiles.contains(&name(&text));
                if exists {
                    ui.colored_label(ui.visuals().warn_fg_color, "That name is taken.");
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let valid = !name(&text).is_empty() && !exists;
                    create = (ui.add_enabled(valid, egui::Button::new("Create")).clicked() || entered) && valid;
                    if ui.button("Cancel").clicked() {
                        keep_open = false;
                    }
                });
            });
        if create {
            let name = name(&text);
            match self.new_preset {
                Some(i) => {
                    let result = new_from_preset(&self.paths, &name, &PRESETS[i], &self.layout);
                    self.run(format!("Created '{name}'"), |_| result);
                }
                None => self.run(format!("Created '{name}'"), |p| new_profile(p, &name, source.as_deref(), &[])),
            }
            if self.snap.profiles.contains(&name) {
                self.open(Page::Profile(name));
                keep_open = false;
            }
        }
        if keep_open {
            self.new_name = Some(text);
        } else {
            self.new_preset = None;
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, name: &str) {
        if ui.button("Back to profiles").clicked() {
            self.open(Page::Home);
            return;
        }
        ui.add_space(6.0);
        let is_active = self.snap.active.as_deref() == Some(name);
        if let Some(mut text) = self.rename.take() {
            let mut keep_open = true;
            ui.horizontal(|ui| {
                let field = ui.add(egui::TextEdit::singleline(&mut text).desired_width(220.0));
                let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !entered {
                    field.request_focus();
                }
                let new = text.trim().replace(' ', "-");
                let valid = !new.is_empty() && new != name && !self.snap.profiles.contains(&new);
                let save = ui.add_enabled(valid, egui::Button::new("Rename")).clicked();
                if ui.button("Cancel").clicked() {
                    keep_open = false;
                }
                if (save || entered) && valid {
                    self.run(format!("Renamed to '{new}'"), |p| rename_profile(p, name, &new));
                    if self.snap.profiles.contains(&new) {
                        self.open(Page::Profile(new));
                    }
                    keep_open = false;
                }
            });
            if keep_open {
                self.rename = Some(text);
            }
        } else {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new(name).size(24.0));
                ui.add_space(8.0);
                if is_active {
                    dot(ui, GREEN);
                    ui.label("In use now");
                } else if ui.button("Use now").on_hover_text("Switch the tablet to this profile").clicked() {
                    self.run(format!("Now using '{name}'"), |p| use_profile(p, name));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.confirm_delete {
                        if ui.button("Cancel").clicked() {
                            self.confirm_delete = false;
                        }
                        if ui.button(egui::RichText::new("Really delete").color(RED)).clicked() {
                            self.run(format!("Deleted '{name}'"), |p| remove_profile(p, name));
                        }
                    } else {
                        let deletable = self.snap.profiles.len() > 1;
                        if ui.add_enabled(deletable, egui::Button::new("Delete")).on_disabled_hover_text("The last profile can't be deleted").clicked() {
                            self.confirm_delete = true;
                        }
                        if ui.button("Rename").clicked() {
                            self.rename = Some(name.to_string());
                        }
                    }
                });
            });
        }
        ui.weak("Changes are saved and applied as you make them.");
        ui.add_space(8.0);
    }

    fn applications(&mut self, ui: &mut egui::Ui, name: &str) {
        ui.label("The tablet switches to this profile when one of these applications is in front.");
        ui.add_space(2.0);
        let bound: Vec<String> = self.snap.cfg.apps.iter().filter(|(_, p)| *p == name).map(|(a, _)| a.clone()).collect();
        let mut unbind = None;
        let mut bind = None;
        ui.horizontal_wrapped(|ui| {
            for app in &bound {
                egui::Frame::group(ui.style()).corner_radius(12).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
                    ui.label(app);
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        unbind = Some(app.clone());
                    }
                });
            }
            ui.menu_button("+  Add application", |ui| {
                let candidates: Vec<&(String, String)> =
                    self.snap.recent.iter().filter(|(class, _)| self.snap.cfg.apps.get(class).is_none_or(|o| o != name)).collect();
                if candidates.is_empty() {
                    ui.weak(if self.snap.daemon { "Open the application once and it will be listed here." } else { "Start the tablet driver to list applications here." });
                }
                for (class, title) in candidates {
                    let mut label = format!("{class}   ·   {title}");
                    if let Some(owner) = self.snap.cfg.apps.get(class) {
                        label.push_str(&format!("   (now: {owner})"));
                    }
                    if ui.button(label).clicked() {
                        bind = Some(class.clone());
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Other…").clicked() {
                    self.manual_class = Some(String::new());
                    ui.close_menu();
                }
            });
        });
        if bound.is_empty() {
            ui.weak("None yet, so this profile is only used when you pick it with “Use now”.");
        }
        if let Some(mut class) = self.manual_class.take() {
            let mut keep_open = true;
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut class).hint_text("Window class, e.g. krita").desired_width(220.0));
                if ui.add_enabled(!class.trim().is_empty(), egui::Button::new("Add")).clicked() {
                    bind = Some(class.trim().to_string());
                    keep_open = false;
                }
                if ui.button("Cancel").clicked() {
                    keep_open = false;
                }
            });
            if keep_open {
                self.manual_class = Some(class);
            }
        }
        if let Some(app) = unbind {
            self.run(format!("{app} no longer switches to '{name}'"), |p| unbind_app(p, &app));
        }
        if let Some(app) = bind {
            self.run(format!("{app} now switches to '{name}'"), |p| {
                bind_app(p, name, &app)?;
                notify_daemon();
                Ok(())
            });
        }
    }

    fn pressure_editor(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(170.0, 170.0), egui::Sense::click_and_drag());
            let to_screen = |x: f32, y: f32| egui::pos2(rect.left() + x * rect.width(), rect.bottom() - y * rect.height());
            if let Some(pos) = response.interact_pointer_pos() {
                let x = ((pos.x - rect.left()) / rect.width()).clamp(0.05, 0.95);
                let y = ((rect.bottom() - pos.y) / rect.height()).clamp(0.05, 0.95);
                self.edit.pressure = [(x * 100.0).round() / 100.0, (y * 100.0).round() / 100.0];
            }
            let painter = ui.painter_at(rect);
            let visuals = ui.visuals();
            painter.rect_filled(rect, 6.0, visuals.extreme_bg_color);
            painter.line_segment([to_screen(0.0, 0.0), to_screen(1.0, 1.0)], egui::Stroke::new(1.0_f32, visuals.weak_text_color()));
            let control = self.edit.pressure;
            let curve: Vec<egui::Pos2> = (0..=40).map(|i| i as f32 / 40.0).map(|p| to_screen(p, pressure_curve(control, p))).collect();
            painter.add(egui::Shape::line(curve, egui::Stroke::new(2.5_f32, visuals.selection.bg_fill)));
            painter.circle_filled(to_screen(control[0], control[1]), 6.0, visuals.strong_text_color());

            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(match control {
                    [x, y] if (x - y).abs() < 0.03 => "Neutral: pressure is passed on as it is.",
                    [x, y] if y > x => "Soft: a light touch already gives a strong line.",
                    _ => "Firm: you have to press harder for a strong line.",
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (label, preset) in [("Soft", [0.3, 0.7]), ("Neutral", [0.5, 0.5]), ("Firm", [0.7, 0.3])] {
                        if ui.add(egui::Button::new(label).selected(control == preset)).clicked() {
                            self.edit.pressure = preset;
                        }
                    }
                });
                ui.add_space(4.0);
                ui.weak("Or drag the dot. Left to right is how hard you press,\nbottom to top is how much the application sees.");
            });
        });
    }

    fn profile_page(&mut self, ui: &mut egui::Ui, name: &str) {
        self.header(ui, name);
        if self.page != Page::Profile(name.to_string()) {
            return;
        }
        card(ui, "Applications", |ui| self.applications(ui, name));
        card(ui, "Express keys", |ui| {
            ui.label("Click a shortcut to change it. Press a key on the tablet and it lights up here.");
            ui.weak("The first field is a name for what the key does, such as Undo. It is shown on the overview; without one the shortcut is shown.");
            ui.weak("A shortcut stays held for as long as you hold the key, so a key set to Ctrl or Space works like the real one.");
            ui.weak("Mouse clicks land where the pen is while it is over the tablet, otherwise at the mouse pointer.");
            ui.add_space(4.0);
            egui::Grid::new("keys").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
                for i in 0..self.edit.keys.len() {
                    let lit = self.held_keys & (1 << i) != 0;
                    self.shortcut_row(ui, &format!("Key {}", i + 1), Slot::Key(i), lit);
                }
            });
        });
        card(ui, "Dial", |ui| {
            ui.label("Each click of the dial presses the shortcut once.");
            ui.add_space(4.0);
            egui::Grid::new("dial").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
                self.shortcut_row(ui, "Clockwise", Slot::DialCw, self.dial_step == 1);
                self.shortcut_row(ui, "Counter-clockwise", Slot::DialCcw, self.dial_step == -1);
            });
        });
        card(ui, "Pen", |ui| {
            ui.label("Left as “Pen button”, each application decides what the button does (usually middle and right click).");
            ui.add_space(4.0);
            egui::Grid::new("pen").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
                self.shortcut_row(ui, "Lower button", Slot::PenButton1, false);
                self.shortcut_row(ui, "Upper button", Slot::PenButton2, false);
            });
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Pressure").strong());
            ui.add_space(2.0);
            self.pressure_editor(ui);
        });
    }

    fn general_page(&mut self, ui: &mut egui::Ui) {
        if ui.button("Back to profiles").clicked() {
            self.open(Page::Home);
            return;
        }
        ui.add_space(6.0);
        ui.heading(egui::RichText::new("Settings").size(24.0));
        ui.weak("These apply to every profile.");
        ui.add_space(8.0);
        let current = self.snap.cfg.clone();
        let mut cfg = current.clone();
        card(ui, "Screen", |ui| {
            ui.label("Which screen the pen draws on. Pick the tablet's own screen.");
            ui.add_space(2.0);
            let text = if cfg.output.is_empty() { "Let the desktop decide".to_string() } else { cfg.output.clone() };
            egui::ComboBox::from_id_salt("output").selected_text(text).width(240.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut cfg.output, String::new(), "Let the desktop decide");
                for output in &self.outputs {
                    ui.selectable_value(&mut cfg.output, output.clone(), output);
                }
            });
        });
        card(ui, "Other applications", |ui| {
            ui.label("What happens when an application without its own profile comes to the front.");
            ui.add_space(2.0);
            let text = if cfg.fallback == "keep" { "Keep the profile that is in use".to_string() } else { format!("Switch to '{}'", cfg.fallback) };
            egui::ComboBox::from_id_salt("fallback").selected_text(text).width(240.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut cfg.fallback, keep(), "Keep the profile that is in use");
                for name in &self.snap.profiles {
                    ui.selectable_value(&mut cfg.fallback, name.clone(), format!("Switch to '{name}'"));
                }
            });
        });
        card(ui, "Keyboard layout", |ui| {
            ui.label(format!("{}. Shortcuts are shown and recorded as on your keyboard.", self.layout.name));
            ui.weak("This follows the layout set in the desktop's keyboard settings. Profiles keep working if you change it, since they remember the physical keys.");
        });
        if cfg != current {
            self.run("Saved", |p| {
                p.save_config(&cfg)?;
                notify_daemon();
                Ok(())
            });
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        self.poll_tablet_input();
        ctx.request_repaint_after(INPUT_POLL);
        self.record(ctx);
        if self.message.as_ref().is_some_and(|(_, _, at)| at.elapsed() > MESSAGE_TIME) {
            self.message = None;
        }

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(5.0);
            self.status_bar(ui);
            ui.add_space(5.0);
        });
        if let Some((is_error, text, _)) = &self.message {
            egui::TopBottomPanel::bottom("message").show(ctx, |ui| {
                ui.add_space(4.0);
                let color = if *is_error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                ui.colored_label(color, text);
                ui.add_space(4.0);
            });
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.page.clone() {
                Page::Home => self.home_page(ui),
                Page::Profile(name) => self.profile_page(ui, &name),
                Page::General => self.general_page(ui),
            });
        });
        self.typing_dialog(ctx);
        self.new_profile_dialog(ctx);
        self.save_edits();
    }
}

pub fn run(paths: Paths, edit: Option<String>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Inkswitch")
            .with_app_id(gui_app_id())
            .with_inner_size([900.0, 940.0])
            .with_min_inner_size([720.0, 440.0]),
        ..Default::default()
    };
    let conn = zbus::blocking::Connection::session().ok();
    let (id, name) = Layout::current_id(conn.as_ref());
    let app = App {
        paths,
        layout: Layout::load(id, name),
        icons: IconIndex::load(),
        icon_cache: HashMap::new(),
        conn,
        snap: Snapshot::default(),
        refreshed: None,
        outputs: Vec::new(),
        outputs_refreshed: None,
        // A profile that doesn't exist falls back to the main screen on the first refresh.
        page: edit.map_or(Page::Home, Page::Profile),
        in_use: None,
        edit: Profile::default(),
        saved: Profile::default(),
        recording: None,
        typing: None,
        new_name: None,
        new_preset: None,
        rename: None,
        manual_class: None,
        confirm_delete: false,
        message: None,
        held_keys: 0,
        dial_step: 0,
    };
    eframe::run_native(
        "Inkswitch",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            cc.egui_ctx.style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(8.0, 6.0);
                style.spacing.button_padding = egui::vec2(10.0, 5.0);
                for font in style.text_styles.values_mut() {
                    font.size *= 1.1;
                }
            });
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("opening the window: {e}"))
}
