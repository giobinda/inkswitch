//! Settings window (`xppen-profiles gui`): profiles, what they do, and which apps load them.

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui;

use super::settings::{parse_action, pressure_curve, Profile, PEN_BUTTON_1, PEN_BUTTON_2};
use super::{
    bind_app, daemon_running, keep, new_profile, notify_daemon, remove_profile, unbind_app, use_profile,
    vendor_driver_running, Config, Paths, GUI_APP_ID,
};

const REFRESH: Duration = Duration::from_secs(1);
const OUTPUT_REFRESH: Duration = Duration::from_secs(10);

/// Everything read from disk, re-read once per `REFRESH`.
#[derive(Default)]
struct Snapshot {
    profiles: Vec<String>,
    cfg: Config,
    active: Option<String>,
    recent: Vec<(String, String)>,
    daemon: bool,
    vendor_driver: bool,
    tablet: String,
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

struct App {
    paths: Paths,
    snap: Snapshot,
    refreshed: Option<Instant>,
    outputs: Vec<String>,
    outputs_refreshed: Option<Instant>,
    selected: Option<String>,
    /// The selected profile as shown, and as last written to disk.
    edit: Profile,
    saved: Profile,
    /// The slot waiting for a shortcut to be pressed.
    recording: Option<Slot>,
    new_name: String,
    manual_class: String,
    confirm_delete: bool,
    message: Option<(bool, String)>,
}

/// Names of the connected screens.
fn list_outputs() -> Vec<String> {
    let Ok(out) = Command::new("kscreen-doctor").arg("-o").output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // "Output: 1 eDP-1 <uuid>", wrapped in colour codes
            let plain: String = line.split('\u{1b}').map(|part| part.split_once('m').map_or(part, |(_, rest)| rest)).collect();
            let mut words = plain.split_whitespace();
            (words.next() == Some("Output:")).then(|| words.nth(1).map(str::to_string)).flatten()
        })
        .collect()
}

/// The profile-file name of a key as egui reports it (by position, so layouts don't matter).
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
            return parse_action(&name).is_ok().then_some(name);
        }
    };
    Some(fixed.to_string())
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

    fn select(&mut self, name: Option<String>) {
        self.selected = name;
        self.confirm_delete = false;
        self.recording = None;
        self.edit = self.selected.as_ref().and_then(|n| self.paths.load_profile(n).ok()).unwrap_or_default();
        self.saved = self.edit.clone();
    }

    fn refresh(&mut self) {
        let p = &self.paths;
        let cfg = match p.load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                self.message = Some((true, format!("{e:#}")));
                Config::default()
            }
        };
        self.snap = Snapshot {
            profiles: p.profile_names().unwrap_or_default(),
            cfg,
            active: p.active(),
            recent: p.recent_windows(),
            daemon: daemon_running(),
            vendor_driver: vendor_driver_running(),
            tablet: p.tablet_status(),
        };
        self.refreshed = Some(Instant::now());
        if self.outputs_refreshed.is_none_or(|t| t.elapsed() >= OUTPUT_REFRESH) {
            self.outputs = list_outputs();
            self.outputs_refreshed = Some(Instant::now());
        }

        let profiles = &self.snap.profiles;
        if self.selected.as_ref().is_none_or(|s| !profiles.contains(s)) {
            let pick = self.snap.active.clone().filter(|a| profiles.contains(a)).or_else(|| profiles.first().cloned());
            self.select(pick);
        } else if self.edit == self.saved {
            // Pick up changes made elsewhere, unless there are edits here that couldn't be saved.
            let name = self.selected.clone();
            self.select(name);
        }
    }

    /// Runs an action, shows its outcome in the status line and reloads from disk.
    fn run(&mut self, ok: impl Into<String>, action: impl FnOnce(&Paths) -> Result<()>) {
        self.message = Some(match action(&self.paths) {
            Ok(()) => (false, ok.into()),
            Err(e) => (true, format!("{e:#}")),
        });
        self.refresh();
    }

    /// Writes the edited profile once it differs from disk and every action in it is valid.
    fn save_edits(&mut self) {
        let Some(name) = self.selected.clone() else { return };
        if self.edit == self.saved {
            return;
        }
        match self.edit.validate().and_then(|()| self.paths.save_profile(&name, &self.edit)) {
            Ok(()) => {
                self.saved = self.edit.clone();
                self.message = None;
                notify_daemon();
            }
            Err(e) => self.message = Some((true, format!("Not saved: {e:#}"))),
        }
    }

    /// Turns the next key press into the action of the slot being recorded.
    fn record(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.recording else { return };
        let pressed = ctx.input_mut(|i| {
            let found = i.events.iter().find_map(|e| match e {
                egui::Event::Key { key, physical_key, pressed: true, modifiers, .. } => {
                    Some((physical_key.unwrap_or(*key), *modifiers))
                }
                _ => None,
            });
            if found.is_some() {
                // Keep the key press from also reaching a focused text field.
                i.events.retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)));
            }
            found
        });
        let Some((key, modifiers)) = pressed else { return };
        let Some(token) = key_token(key) else { return };
        let mut parts = Vec::new();
        for (held, name) in [(modifiers.ctrl, "ctrl"), (modifiers.shift, "shift"), (modifiers.alt, "alt")] {
            if held {
                parts.push(name.to_string());
            }
        }
        parts.push(token);
        *self.slot_text(slot) = parts.join("+");
        self.recording = None;
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let dot = |ui: &mut egui::Ui, on: bool, label: &str| {
            let color = if on { egui::Color32::from_rgb(60, 170, 90) } else { egui::Color32::from_rgb(200, 70, 70) };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.5, color);
            ui.label(label);
        };
        ui.horizontal(|ui| {
            if !self.snap.daemon {
                dot(ui, false, "Driver not running");
                if self.snap.vendor_driver {
                    ui.weak("(the official XP-Pen driver is running instead)");
                } else if ui.button("Start").clicked() {
                    self.run("Driver started", |_| {
                        let ok = Command::new("systemctl")
                            .args(["--user", "start", "xppen-profiles.service"])
                            .status()
                            .is_ok_and(|s| s.success());
                        if !ok {
                            return Err(anyhow!("could not start xppen-profiles.service"));
                        }
                        // Give it a moment so the indicator is right on the reload that follows.
                        std::thread::sleep(Duration::from_millis(400));
                        Ok(())
                    });
                }
            } else if let Some(name) = self.snap.tablet.strip_prefix("connected: ") {
                dot(ui, true, name);
            } else {
                dot(ui, false, "Tablet not connected");
            }
        });
    }

    fn profile_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading("Profiles");
        ui.add_space(4.0);
        let mut clicked = None;
        for name in &self.snap.profiles {
            let is_active = self.snap.active.as_ref() == Some(name);
            let mut text = egui::RichText::new(name);
            if is_active {
                text = text.strong();
            }
            ui.horizontal(|ui| {
                if ui.selectable_label(self.selected.as_ref() == Some(name), text).clicked() {
                    clicked = Some(name.clone());
                }
                if is_active {
                    ui.weak("active");
                }
            });
        }
        if let Some(name) = clicked {
            self.select(Some(name));
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(match &self.selected {
            Some(selected) => format!("New profile, starting as a copy of '{selected}':"),
            None => "New profile:".to_string(),
        });
        let entered = ui
            .add(egui::TextEdit::singleline(&mut self.new_name).hint_text("name, e.g. krita").desired_width(f32::INFINITY))
            .lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let name = self.new_name.trim().to_string();
        let exists = self.snap.profiles.contains(&name);
        let create = ui.add_enabled(!name.is_empty() && !exists, egui::Button::new("Create")).clicked();
        if exists {
            ui.weak("A profile with this name exists.");
        }
        if (create || entered) && !name.is_empty() && !exists {
            let from = self.selected.clone();
            self.run(format!("Created '{name}'"), |p| new_profile(p, &name, from.as_deref(), &[]));
            if self.snap.profiles.contains(&name) {
                self.new_name.clear();
                self.select(Some(name));
            }
        }
    }

    /// One "label [action] [Record]" row; returns whether the row's extra button was clicked.
    fn action_row(&mut self, ui: &mut egui::Ui, label: &str, slot: Slot, extra: Option<&str>) -> bool {
        let mut extra_clicked = false;
        ui.label(label);
        ui.horizontal(|ui| {
            let recording = self.recording == Some(slot);
            let invalid = parse_action(self.slot_text(slot)).is_err();
            let mut field = egui::TextEdit::singleline(self.slot_text(slot)).desired_width(190.0);
            field = if recording { field.hint_text("press the shortcut…") } else { field.hint_text("nothing") };
            if invalid {
                field = field.text_color(ui.visuals().error_fg_color);
            }
            ui.add_enabled(!recording, field);
            if recording {
                if ui.button("Cancel").clicked() {
                    self.recording = None;
                }
            } else if ui.button("Record").on_hover_text("Press the shortcut on your keyboard").clicked() {
                self.slot_text(slot).clear();
                self.recording = Some(slot);
            }
            if let Some(extra) = extra {
                extra_clicked = ui.button(extra).clicked();
            }
        });
        ui.end_row();
        extra_clicked
    }

    fn pressure_editor(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(150.0, 150.0), egui::Sense::click_and_drag());
            let to_screen = |x: f32, y: f32| egui::pos2(rect.left() + x * rect.width(), rect.bottom() - y * rect.height());
            if let Some(pos) = response.interact_pointer_pos() {
                let x = ((pos.x - rect.left()) / rect.width()).clamp(0.05, 0.95);
                let y = ((rect.bottom() - pos.y) / rect.height()).clamp(0.05, 0.95);
                self.edit.pressure = [(x * 100.0).round() / 100.0, (y * 100.0).round() / 100.0];
            }
            let painter = ui.painter_at(rect);
            let visuals = ui.visuals();
            painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
            let faint = egui::Stroke::new(1.0_f32, visuals.weak_text_color());
            painter.line_segment([to_screen(0.0, 0.0), to_screen(1.0, 1.0)], faint);
            let control = self.edit.pressure;
            let curve: Vec<egui::Pos2> =
                (0..=40).map(|i| i as f32 / 40.0).map(|p| to_screen(p, pressure_curve(control, p))).collect();
            painter.add(egui::Shape::line(curve, egui::Stroke::new(2.0_f32, visuals.selection.bg_fill)));
            painter.circle_filled(to_screen(control[0], control[1]), 5.0, visuals.strong_text_color());

            ui.vertical(|ui| {
                ui.label("Drag the dot to shape how hard you have\nto press. Across: how hard you press.\nUp: how much the application sees.");
                ui.add_space(4.0);
                ui.weak(match control {
                    [x, y] if (x - y).abs() < 0.03 => "Neutral",
                    [x, y] if y > x => "Softer: full pressure comes sooner",
                    _ => "Firmer: you have to press harder",
                });
                if ui.add_enabled(control != [0.5, 0.5], egui::Button::new("Reset")).clicked() {
                    self.edit.pressure = [0.5, 0.5];
                }
            });
        });
    }

    fn applications(&mut self, ui: &mut egui::Ui, name: &str) {
        ui.strong("Applications");
        ui.label("This profile loads when one of these gets focus:");
        let bound: Vec<String> =
            self.snap.cfg.apps.iter().filter(|(_, p)| *p == name).map(|(a, _)| a.clone()).collect();
        let mut unbind = None;
        ui.horizontal_wrapped(|ui| {
            if bound.is_empty() {
                ui.weak("none yet");
            }
            for app in &bound {
                egui::Frame::group(ui.style()).inner_margin(egui::Margin::symmetric(6, 2)).show(ui, |ui| {
                    ui.label(app);
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        unbind = Some(app.clone());
                    }
                });
            }
        });
        if let Some(app) = unbind {
            self.run(format!("Removed {app} from '{name}'"), |p| unbind_app(p, &app));
        }

        ui.add_space(6.0);
        let mut bind = None;
        egui::ComboBox::from_id_salt("recent").selected_text("Add a recently used application…").width(300.0).show_ui(ui, |ui| {
            if self.snap.recent.is_empty() {
                ui.weak(if self.snap.daemon {
                    "Nothing seen yet. Focus the application once, then come back."
                } else {
                    "Start the driver to see applications here."
                });
            }
            for (class, title) in &self.snap.recent {
                let owner = self.snap.cfg.apps.get(class);
                if owner.is_some_and(|o| o == name) {
                    continue;
                }
                let mut label = format!("{class}  —  {title}");
                if let Some(o) = owner {
                    label.push_str(&format!("  (now: {o})"));
                }
                if ui.selectable_label(false, label).clicked() {
                    bind = Some(class.clone());
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.manual_class).hint_text("or type a window class").desired_width(200.0));
            let class = self.manual_class.trim().to_string();
            if ui.add_enabled(!class.is_empty(), egui::Button::new("Add")).clicked() {
                bind = Some(class);
                self.manual_class.clear();
            }
        });
        if let Some(app) = bind {
            self.run(format!("{app} now loads '{name}'"), |p| bind_app(p, name, &app));
        }
    }

    fn profile_details(&mut self, ui: &mut egui::Ui) {
        let Some(name) = self.selected.clone() else {
            ui.label("No profiles yet. Create one on the left.");
            return;
        };
        let is_active = self.snap.active.as_ref() == Some(&name);

        ui.horizontal(|ui| {
            ui.heading(&name);
            if is_active {
                ui.weak("active");
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!is_active, egui::Button::new("Use now")).clicked() {
                self.run(format!("Using '{name}'"), |p| use_profile(p, &name));
            }
            if self.confirm_delete {
                if ui.button("Really delete").clicked() {
                    self.run(format!("Deleted '{name}'"), |p| remove_profile(p, &name));
                    return;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_delete = false;
                }
            } else if ui.add_enabled(self.snap.profiles.len() > 1, egui::Button::new("Delete")).clicked() {
                self.confirm_delete = true;
            }
        });
        if self.selected.as_ref() != Some(&name) {
            return;
        }

        ui.add_space(14.0);
        self.applications(ui, &name);

        ui.add_space(14.0);
        ui.strong("Express keys");
        ui.label("Shortcuts are held for as long as the key is held. Several shortcuts separated by spaces are tapped one after the other.");
        ui.add_space(2.0);
        egui::Grid::new("keys").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for i in 0..self.edit.keys.len() {
                self.action_row(ui, &format!("Key {}", i + 1), Slot::Key(i), None);
            }
        });

        ui.add_space(14.0);
        ui.strong("Dial");
        egui::Grid::new("dial").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            self.action_row(ui, "Clockwise", Slot::DialCw, None);
            self.action_row(ui, "Counter-clockwise", Slot::DialCcw, None);
        });

        ui.add_space(14.0);
        ui.strong("Pen buttons");
        ui.label("As a pen button, applications decide what it does (usually middle and right click). A shortcut replaces that.");
        ui.add_space(2.0);
        egui::Grid::new("pen").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            if self.action_row(ui, "Lower button", Slot::PenButton1, Some("Pen button")) {
                self.edit.pen_button1 = PEN_BUTTON_1.into();
            }
            if self.action_row(ui, "Upper button", Slot::PenButton2, Some("Pen button")) {
                self.edit.pen_button2 = PEN_BUTTON_2.into();
            }
        });

        ui.add_space(14.0);
        ui.strong("Pressure");
        ui.add_space(2.0);
        self.pressure_editor(ui);
        ui.add_space(8.0);
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        let current = self.snap.cfg.clone();
        let mut cfg = current.clone();
        ui.horizontal(|ui| {
            ui.label("When any other application gets focus:");
            let text = if cfg.fallback == "keep" { "keep the current profile".to_string() } else { format!("use '{}'", cfg.fallback) };
            egui::ComboBox::from_id_salt("fallback").selected_text(text).show_ui(ui, |ui| {
                ui.selectable_value(&mut cfg.fallback, keep(), "keep the current profile");
                for name in &self.snap.profiles {
                    ui.selectable_value(&mut cfg.fallback, name.clone(), format!("use '{name}'"));
                }
            });
            ui.add_space(16.0);
            ui.label("Pen draws on:");
            let text = if cfg.output.is_empty() { "chosen by the desktop".to_string() } else { cfg.output.clone() };
            egui::ComboBox::from_id_salt("output").selected_text(text).show_ui(ui, |ui| {
                ui.selectable_value(&mut cfg.output, String::new(), "chosen by the desktop");
                for output in &self.outputs {
                    ui.selectable_value(&mut cfg.output, output.clone(), output);
                }
            });
        });
        if cfg != current {
            self.run("Saved", |p| {
                p.save_config(&cfg)?;
                notify_daemon();
                Ok(())
            });
        }
        if let Some((is_error, text)) = &self.message {
            let color = if *is_error { ui.visuals().error_fg_color } else { ui.visuals().weak_text_color() };
            ui.colored_label(color, text);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.refresh();
        }
        ctx.request_repaint_after(REFRESH);
        self.record(ctx);

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(4.0);
            self.status_bar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.add_space(4.0);
            self.footer(ui);
            ui.add_space(4.0);
        });
        egui::SidePanel::left("profiles").exact_width(230.0).resizable(false).show(ctx, |ui| self.profile_list(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.profile_details(ui));
        });
        self.save_edits();
    }
}

pub fn run(paths: Paths) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("XP-Pen Profiles")
            .with_app_id(GUI_APP_ID)
            .with_inner_size([820.0, 760.0])
            .with_min_inner_size([680.0, 420.0]),
        ..Default::default()
    };
    let app = App {
        paths,
        snap: Snapshot::default(),
        refreshed: None,
        outputs: Vec::new(),
        outputs_refreshed: None,
        selected: None,
        edit: Profile::default(),
        saved: Profile::default(),
        recording: None,
        new_name: String::new(),
        manual_class: String::new(),
        confirm_delete: false,
        message: None,
    };
    eframe::run_native("XP-Pen Profiles", options, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow!("opening the window: {e}"))
}
