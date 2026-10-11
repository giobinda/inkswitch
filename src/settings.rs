//! Profiles: what the tablet's keys, dial and pen do, and how they are written down.
//!
//! An action is text such as `ctrl+s` (keys held while the button is held), `5 1` (several
//! combinations tapped one after the other), `pen-button-1` or `none`. Key names are the Linux
//! input names without the `KEY_` prefix and stand for key positions on a US layout.

use anyhow::{bail, Result};
use evdev::KeyCode;
use serde::{Deserialize, Serialize};

pub const EXPRESS_KEYS: usize = 8;

pub const PEN_BUTTON_1: &str = "pen-button-1";
pub const PEN_BUTTON_2: &str = "pen-button-2";

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Profile {
    /// Action per express key, top to bottom.
    pub keys: Vec<String>,
    pub dial_cw: String,
    pub dial_ccw: String,
    pub pen_button1: String,
    pub pen_button2: String,
    /// Control point of the pressure curve; [0.5, 0.5] is linear.
    pub pressure: [f32; 2],
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            keys: vec![String::new(); EXPRESS_KEYS],
            dial_cw: String::new(),
            dial_ccw: String::new(),
            pen_button1: PEN_BUTTON_1.into(),
            pen_button2: PEN_BUTTON_2.into(),
            pressure: [0.5, 0.5],
        }
    }
}

pub type Chord = Vec<KeyCode>;

#[derive(Clone, PartialEq, Debug)]
pub enum Action {
    None,
    /// The pen's own buttons, passed on to applications as such.
    Stylus1,
    Stylus2,
    /// Held for as long as the button is held.
    Chord(Chord),
    /// Tapped in order when the button goes down.
    Sequence(Vec<Chord>),
}

/// A profile in the form the driver works with.
pub struct Compiled {
    pub keys: Vec<Action>,
    pub dial_cw: Action,
    pub dial_ccw: Action,
    pub pen_button1: Action,
    pub pen_button2: Action,
    pub pressure: [f32; 2],
}

impl Profile {
    pub fn compile(&self) -> Compiled {
        // Text that doesn't parse does nothing; the GUI refuses to save such text.
        let action = |s: &String| parse_action(s).unwrap_or(Action::None);
        Compiled {
            keys: self.keys.iter().map(action).collect(),
            dial_cw: action(&self.dial_cw),
            dial_ccw: action(&self.dial_ccw),
            pen_button1: action(&self.pen_button1),
            pen_button2: action(&self.pen_button2),
            pressure: self.pressure,
        }
    }

    /// The first problem in any of the profile's actions.
    pub fn validate(&self) -> Result<()> {
        let all = self.keys.iter().chain([&self.dial_cw, &self.dial_ccw, &self.pen_button1, &self.pen_button2]);
        for text in all {
            parse_action(text)?;
        }
        Ok(())
    }
}

const ALIASES: [(&str, &str); 27] = [
    ("ctrl", "leftctrl"),
    ("control", "leftctrl"),
    ("shift", "leftshift"),
    ("alt", "leftalt"),
    ("altgr", "rightalt"),
    ("super", "leftmeta"),
    ("meta", "leftmeta"),
    ("win", "leftmeta"),
    ("escape", "esc"),
    ("return", "enter"),
    ("del", "delete"),
    ("ins", "insert"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("period", "dot"),
    (",", "comma"),
    (".", "dot"),
    ("/", "slash"),
    ("\\", "backslash"),
    ("'", "apostrophe"),
    (";", "semicolon"),
    ("[", "leftbrace"),
    ("]", "rightbrace"),
    ("`", "grave"),
    ("=", "equal"),
    ("-", "minus"),
    ("spacebar", "space"),
];

fn parse_key(name: &str) -> Result<KeyCode> {
    let lower = name.trim().to_lowercase();
    let canonical = ALIASES.iter().find(|(alias, _)| *alias == lower).map_or(lower.as_str(), |(_, k)| k);
    match format!("KEY_{}", canonical.to_uppercase()).parse::<KeyCode>() {
        Ok(key) => Ok(key),
        Err(_) => bail!("unknown key \"{name}\""),
    }
}

fn parse_chord(text: &str) -> Result<Chord> {
    text.split('+').map(parse_key).collect()
}

pub fn parse_action(text: &str) -> Result<Action> {
    let text = text.trim();
    match text.to_lowercase().as_str() {
        "" | "none" => return Ok(Action::None),
        PEN_BUTTON_1 => return Ok(Action::Stylus1),
        PEN_BUTTON_2 => return Ok(Action::Stylus2),
        _ => {}
    }
    let mut chords = text.split_whitespace().map(parse_chord).collect::<Result<Vec<Chord>>>()?;
    Ok(if chords.len() == 1 { Action::Chord(chords.remove(0)) } else { Action::Sequence(chords) })
}

/// The name a key is written with in a profile.
pub fn key_name(key: KeyCode) -> String {
    let name = format!("{key:?}").trim_start_matches("KEY_").to_lowercase();
    match name.as_str() {
        "leftctrl" => "ctrl".into(),
        "leftshift" => "shift".into(),
        "leftalt" => "alt".into(),
        "leftmeta" => "super".into(),
        _ => name,
    }
}

fn chord_text(codes: impl Iterator<Item = u16>) -> String {
    codes.map(|c| key_name(KeyCode::new(c))).collect::<Vec<_>>().join("+")
}

/// Converts the vendor's `29:37+31:39` (keys held together) or `6:14,2:10` (keys in a row)
/// notation; the number before each colon is already a Linux key code.
fn vendor_keys(codes: &str) -> Option<String> {
    let code = |pair: &str| pair.split(':').next()?.trim().parse::<u16>().ok();
    let chords = codes
        .split(',')
        .map(|chord| chord.split('+').map(code).collect::<Option<Vec<u16>>>().map(|c| chord_text(c.into_iter())))
        .collect::<Option<Vec<String>>>()?;
    Some(chords.join(" "))
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, tag: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|c| c.has_tag_name(tag))
}

/// Builds a profile from the official driver's config.xml, taking the tablet model with the
/// most customised entries (the file holds every model the driver knows).
pub fn import_vendor(xml: &str) -> Option<Profile> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let custom = |n: &roxmltree::Node| n.descendants().filter(|d| d.text().is_some_and(|t| t.contains('|'))).count();
    let device = doc.root_element().children().filter(|n| n.is_element()).max_by_key(custom)?;
    let common = child(device, "CommonAPP")?;
    if custom(&common) == 0 {
        return None;
    }
    let mut profile = Profile::default();

    if let Some(keys) = child(common, "K") {
        for (i, slot) in profile.keys.iter_mut().enumerate() {
            let text = child(keys, &format!("K{}", i + 1)).and_then(|k| k.text().map(str::to_string));
            // "1|Ctrl+S|Ctrl+S|29:37+31:39"
            if let Some(action) = text.and_then(|t| vendor_keys(t.rsplit('|').next()?)) {
                *slot = action;
            }
        }
    }
    // "4|\,'|\|'|43:51|40:48": the last two fields are the two directions.
    if let Some(text) = child(common, "R").and_then(|r| child(r, "W1")).and_then(|w| w.text()) {
        let mut fields = text.rsplit('|');
        let (second, first) = (fields.next(), fields.next());
        if let (Some(cw), Some(ccw)) = (first.and_then(vendor_keys), second.and_then(vendor_keys)) {
            profile.dial_cw = cw;
            profile.dial_ccw = ccw;
        }
    }
    // Pen settings are stored per pen model; DeviceCurPenID says which one is in use.
    let pen_id = child(device, "DeviceInfo")
        .and_then(|i| child(i, "DeviceCurPenID"))
        .and_then(|n| n.text()?.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let pen_tag = if pen_id == 0 { "Pen".to_string() } else { format!("Pen{}", pen_id + 1) };
    if let Some(pen) = child(common, &pen_tag) {
        let number = |tag: &str| child(pen, tag).and_then(|n| n.text()?.trim().parse::<f32>().ok());
        if let (Some(x), Some(y)) = (number("P1X"), number("P1Y")) {
            profile.pressure = [x.clamp(0.05, 0.95), y.clamp(0.05, 0.95)];
        }
        // Buttons set to a keyboard shortcut ("1|label|label|codes") carry over; mouse clicks
        // ("2|Middle click|8") are what the plain pen buttons already do.
        for (tag, slot) in [("PenBtn1", &mut profile.pen_button1), ("PenBtn2", &mut profile.pen_button2)] {
            let text = child(pen, tag).and_then(|n| n.text()).unwrap_or("");
            if text.starts_with("1|") {
                if let Some(action) = text.rsplit('|').next().and_then(vendor_keys) {
                    *slot = action;
                }
            }
        }
    }
    Some(profile)
}

/// Maps raw pressure (0..=1) through a quadratic curve from (0,0) to (1,1) bent towards
/// `control`: below the diagonal gives a firmer pen, above it a softer one.
pub fn pressure_curve(control: [f32; 2], p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    let (cx, cy) = (control[0].clamp(0.01, 0.99), control[1].clamp(0.0, 1.0));
    let a = 1.0 - 2.0 * cx;
    // Solve x(t) = 2(1-t)t*cx + t^2 = p for t.
    let t = if a.abs() < 1e-4 { p / (2.0 * cx) } else { (-2.0 * cx + (4.0 * cx * cx + 4.0 * a * p).sqrt()) / (2.0 * a) };
    let t = t.clamp(0.0, 1.0);
    (2.0 * (1.0 - t) * t * cy + t * t).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions() {
        assert_eq!(parse_action("").unwrap(), Action::None);
        assert_eq!(parse_action("Ctrl+S").unwrap(), Action::Chord(vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_S]));
        assert_eq!(
            parse_action("5 1").unwrap(),
            Action::Sequence(vec![vec![KeyCode::KEY_5], vec![KeyCode::KEY_1]])
        );
        assert_eq!(parse_action("shift+space").unwrap(), Action::Chord(vec![KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_SPACE]));
        assert_eq!(parse_action("\\").unwrap(), Action::Chord(vec![KeyCode::KEY_BACKSLASH]));
        assert_eq!(parse_action("pen-button-2").unwrap(), Action::Stylus2);
        assert!(parse_action("ctrl+nope").is_err());
    }

    #[test]
    fn vendor_notation() {
        assert_eq!(vendor_keys("29:37+31:39").unwrap(), "ctrl+s");
        assert_eq!(vendor_keys("6:14,2:10").unwrap(), "5 1");
        assert_eq!(vendor_keys("43:51").unwrap(), "backslash");
    }

    #[test]
    fn vendor_import() {
        let xml = r#"<PenTableLists><Other><CommonAPP><K><K1 id="0"/></K></CommonAPP></Other>
            <Mine><DeviceInfo><DeviceCurPenID>2</DeviceCurPenID></DeviceInfo><CommonAPP>
            <Pen3><PenBtn1 id="1">2|Middle click|8</PenBtn1><P1X>0.25</P1X><P1Y>0.75</P1Y></Pen3>
            <K><K1 id="1">1|Ctrl+S|Ctrl+S|29:37+31:39</K1><K3 id="1">1|5,1|5,1|6:14,2:10</K3></K>
            <R id="2"><W1 id="2">4|\,'|\|'|43:51|40:48</W1></R></CommonAPP></Mine></PenTableLists>"#;
        let p = import_vendor(xml).unwrap();
        assert_eq!(p.keys[0], "ctrl+s");
        assert_eq!(p.keys[1], "");
        assert_eq!(p.keys[2], "5 1");
        assert_eq!((p.dial_cw.as_str(), p.dial_ccw.as_str()), ("backslash", "apostrophe"));
        assert_eq!(p.pen_button1, PEN_BUTTON_1);
        assert_eq!(p.pressure, [0.25, 0.75]);
        p.validate().unwrap();
    }

    #[test]
    fn curve() {
        for p in [0.0, 0.3, 1.0] {
            assert!((pressure_curve([0.5, 0.5], p) - p).abs() < 1e-4);
        }
        assert!(pressure_curve([0.25, 0.75], 0.3) > 0.3);
        assert!(pressure_curve([0.75, 0.25], 0.3) < 0.3);
    }
}
