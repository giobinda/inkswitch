//! The user's keyboard layout, for showing and entering shortcuts the way their keyboard is
//! labelled. Profiles store key positions; this translates between positions and characters.

use std::ffi::CString;

use anyhow::{bail, Result};
use evdev::KeyCode;
use xkbcommon_dl::{xkb_context_flags, xkb_keymap_compile_flags, xkb_rule_names, xkbcommon_option};

use crate::settings::{key_name, parse_key, Action, Chord};

/// Shift states a character can sit on: plain, Shift, AltGr, Shift+AltGr.
const LEVELS: usize = 4;
const KEY_CODES: usize = 256;

/// Keys named by function rather than by the character they type.
const NAMED: [(KeyCode, &str); 26] = [
    (KeyCode::BTN_LEFT, "Click"),
    (KeyCode::BTN_RIGHT, "RightClick"),
    (KeyCode::BTN_MIDDLE, "MiddleClick"),
    (KeyCode::KEY_LEFTCTRL, "Ctrl"),
    (KeyCode::KEY_LEFTSHIFT, "Shift"),
    (KeyCode::KEY_LEFTALT, "Alt"),
    (KeyCode::KEY_RIGHTALT, "AltGr"),
    (KeyCode::KEY_LEFTMETA, "Super"),
    (KeyCode::KEY_RIGHTCTRL, "RightCtrl"),
    (KeyCode::KEY_RIGHTSHIFT, "RightShift"),
    (KeyCode::KEY_RIGHTMETA, "RightSuper"),
    (KeyCode::KEY_SPACE, "Space"),
    (KeyCode::KEY_ENTER, "Enter"),
    (KeyCode::KEY_ESC, "Esc"),
    (KeyCode::KEY_TAB, "Tab"),
    (KeyCode::KEY_BACKSPACE, "Backspace"),
    (KeyCode::KEY_DELETE, "Del"),
    (KeyCode::KEY_INSERT, "Ins"),
    (KeyCode::KEY_HOME, "Home"),
    (KeyCode::KEY_END, "End"),
    (KeyCode::KEY_PAGEUP, "PageUp"),
    (KeyCode::KEY_PAGEDOWN, "PageDown"),
    (KeyCode::KEY_LEFT, "Left"),
    (KeyCode::KEY_RIGHT, "Right"),
    (KeyCode::KEY_UP, "Up"),
    (KeyCode::KEY_DOWN, "Down"),
];

/// Other spellings accepted when a shortcut is typed in.
const SPELLINGS: [(&str, KeyCode); 7] = [
    ("strg", KeyCode::KEY_LEFTCTRL),
    ("control", KeyCode::KEY_LEFTCTRL),
    ("win", KeyCode::KEY_LEFTMETA),
    ("meta", KeyCode::KEY_LEFTMETA),
    ("escape", KeyCode::KEY_ESC),
    ("return", KeyCode::KEY_ENTER),
    ("delete", KeyCode::KEY_DELETE),
];

pub struct Layout {
    /// XKB layout and variant, e.g. ("de", "").
    pub id: (String, String),
    /// Name for display, e.g. "German".
    pub name: String,
    /// Character per key code and level.
    chars: Vec<[Option<char>; LEVELS]>,
}

impl Layout {
    /// The layout the desktop is currently using, falling back to US.
    pub fn current_id(conn: Option<&zbus::blocking::Connection>) -> ((String, String), String) {
        let kde = |method: &str| conn?.call_method(Some("org.kde.keyboard"), "/Layouts", Some("org.kde.KeyboardLayouts"), method, &()).ok();
        let layouts: Option<Vec<(String, String, String)>> = kde("getLayoutsList").and_then(|r| r.body().deserialize().ok());
        let index: Option<u32> = kde("getLayout").and_then(|r| r.body().deserialize().ok());
        match layouts.zip(index).and_then(|(list, i)| list.into_iter().nth(i as usize)) {
            Some((layout, variant, name)) => ((layout, variant), name),
            None => (("us".into(), String::new()), "English (US)".into()),
        }
    }

    pub fn load(id: (String, String), name: String) -> Layout {
        let chars = compile(&id.0, &id.1).unwrap_or_else(|| vec![[None; LEVELS]; KEY_CODES]);
        Layout { id, name, chars }
    }

    fn char_at(&self, key: KeyCode, level: usize) -> Option<char> {
        self.chars.get(usize::from(key.0))?.get(level).copied().flatten()
    }

    /// The key and level that type `c`, preferring the fewest modifiers.
    fn find(&self, c: char) -> Option<(KeyCode, usize)> {
        (0..LEVELS).find_map(|level| {
            let code = self.chars.iter().position(|levels| levels[level] == Some(c))?;
            Some((KeyCode::new(code as u16), level))
        })
    }

    /// The keys to press to type `c`.
    pub fn chord_for_char(&self, c: char) -> Option<Chord> {
        let (key, level) = self.find(c)?;
        let mut chord = Vec::new();
        if level & 1 != 0 {
            chord.push(KeyCode::KEY_LEFTSHIFT);
        }
        if level & 2 != 0 {
            chord.push(KeyCode::KEY_RIGHTALT);
        }
        chord.push(key);
        Some(chord)
    }

    /// What is printed on a key.
    pub fn label(&self, key: KeyCode) -> String {
        if let Some((_, name)) = NAMED.iter().find(|(k, _)| *k == key) {
            return name.to_string();
        }
        match self.char_at(key, 0) {
            Some(c) => {
                // 'ß' has no single upper-case form; leave such letters alone.
                let upper: Vec<char> = c.to_uppercase().collect();
                if upper.len() == 1 { upper[0].to_string() } else { c.to_string() }
            }
            None => key_name(key).to_uppercase(),
        }
    }

    /// The character a combination types, when it is only Shift/AltGr plus one key.
    pub fn typed_char(&self, chord: &Chord) -> Option<char> {
        let (key, modifiers) = chord.split_last()?;
        let mut level = 0;
        for modifier in modifiers {
            level |= match *modifier {
                KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => 1,
                KeyCode::KEY_RIGHTALT => 2,
                _ => return None,
            };
        }
        if level == 0 { None } else { self.char_at(*key, level) }
    }

    pub fn chord_text(&self, chord: &Chord) -> String {
        chord.iter().map(|k| self.label(*k)).collect::<Vec<_>>().join("+")
    }

    /// An action the way it is typed in: "Ctrl+Z", or "5 1" for shortcuts in a row.
    pub fn action_text(&self, action: &Action) -> String {
        match action {
            Action::Chord(chord) => self.chord_text(chord),
            Action::Sequence(chords) => chords.iter().map(|c| self.chord_text(c)).collect::<Vec<_>>().join(" "),
            Action::None | Action::Stylus1 | Action::Stylus2 => String::new(),
        }
    }

    fn parse_key(&self, token: &str) -> Result<Chord> {
        let lower = token.to_lowercase();
        if let Some((key, _)) = NAMED.iter().find(|(_, name)| name.to_lowercase() == lower) {
            return Ok(vec![*key]);
        }
        if let Some((_, key)) = SPELLINGS.iter().find(|(spelling, _)| *spelling == lower) {
            return Ok(vec![*key]);
        }
        let mut chars = token.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            // "Ctrl+Z" means the Z key, not Shift+Z.
            let plain = c.to_lowercase().next().unwrap_or(c);
            return match self.chord_for_char(plain).or_else(|| self.chord_for_char(c)) {
                Some(chord) => Ok(chord),
                None => bail!("your keyboard layout has no key for \"{c}\""),
            };
        }
        // Anything else by its Linux key name, e.g. F5 or KP1.
        parse_key(token).map(|key| vec![key])
    }

    /// Reads typed-in shortcuts ("Ctrl+Z", "Shift+#", "5 1") into the positions to press,
    /// written the way profiles store them.
    pub fn parse(&self, text: &str) -> Result<String> {
        let mut chords = Vec::new();
        for word in text.split_whitespace() {
            let mut tokens = vec![String::new()];
            for c in word.chars() {
                let current = tokens.last_mut().unwrap();
                // A plus is a separator unless it stands where a key is expected.
                if c == '+' && !current.is_empty() {
                    tokens.push(String::new());
                } else {
                    current.push(c);
                }
            }
            let mut chord = Chord::new();
            for token in &tokens {
                if token.is_empty() {
                    bail!("\"{word}\" ends with a +");
                }
                for key in self.parse_key(token)? {
                    if !chord.contains(&key) {
                        chord.push(key);
                    }
                }
            }
            chords.push(chord.iter().map(|k| key_name(*k)).collect::<Vec<_>>().join("+"));
        }
        Ok(chords.join(" "))
    }
}

/// Builds the character table of an XKB layout.
fn compile(layout: &str, variant: &str) -> Option<Vec<[Option<char>; LEVELS]>> {
    let xkb = xkbcommon_option()?;
    let (layout, variant) = (CString::new(layout).ok()?, CString::new(variant).ok()?);
    let names = xkb_rule_names {
        rules: std::ptr::null(),
        model: std::ptr::null(),
        layout: layout.as_ptr(),
        variant: variant.as_ptr(),
        options: std::ptr::null(),
    };
    let mut chars = vec![[None; LEVELS]; KEY_CODES];
    // SAFETY: plain libxkbcommon calls; every pointer is checked before use, `names` and its
    // strings outlive the compile call, and both objects are released before returning.
    unsafe {
        let context = (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
        if context.is_null() {
            return None;
        }
        let keymap = (xkb.xkb_keymap_new_from_names)(context, &names, xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS);
        if keymap.is_null() {
            (xkb.xkb_context_unref)(context);
            return None;
        }
        for (code, levels) in chars.iter_mut().enumerate() {
            // XKB key codes are the kernel's plus 8.
            let xkb_code = code as u32 + 8;
            let available = (xkb.xkb_keymap_num_levels_for_key)(keymap, xkb_code, 0) as usize;
            for (level, slot) in levels.iter_mut().enumerate().take(available) {
                let mut syms = std::ptr::null();
                if (xkb.xkb_keymap_key_get_syms_by_level)(keymap, xkb_code, 0, level as u32, &mut syms) == 1 && !syms.is_null() {
                    *slot = char::from_u32((xkb.xkb_keysym_to_utf32)(*syms)).filter(|c| !c.is_control() && !c.is_whitespace());
                }
            }
        }
        (xkb.xkb_keymap_unref)(keymap);
        (xkb.xkb_context_unref)(context);
    }
    Some(chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn german() -> Layout {
        Layout::load(("de".into(), String::new()), "German".into())
    }

    #[test]
    fn german_labels() {
        let de = german();
        assert_eq!(de.label(KeyCode::KEY_Y), "Z");
        assert_eq!(de.label(KeyCode::KEY_BACKSLASH), "#");
        assert_eq!(de.label(KeyCode::KEY_MINUS), "ß");
        assert_eq!(de.label(KeyCode::KEY_F5), "F5");
        assert_eq!(de.chord_text(&vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_Y]), "Ctrl+Z");
        assert_eq!(de.typed_char(&vec![KeyCode::KEY_RIGHTALT, KeyCode::KEY_MINUS]), Some('\\'));
        assert_eq!(de.typed_char(&vec![KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_BACKSLASH]), Some('\''));
        assert_eq!(de.typed_char(&vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_S]), None);
    }

    #[test]
    fn german_input() {
        let de = german();
        assert_eq!(de.parse("Ctrl+Z").unwrap(), "ctrl+y");
        assert_eq!(de.parse("strg+s").unwrap(), "ctrl+s");
        assert_eq!(de.parse("\\").unwrap(), "rightalt+minus");
        assert_eq!(de.parse("'").unwrap(), "shift+backslash");
        assert_eq!(de.parse("Ctrl++").unwrap(), "ctrl+rightbrace");
        assert_eq!(de.parse("5 1").unwrap(), "5 1");
        assert_eq!(de.parse("Shift+Space").unwrap(), "shift+space");
        assert_eq!(de.parse("F5").unwrap(), "f5");
        assert_eq!(de.parse("Shift+Click").unwrap(), "shift+leftclick");
        assert_eq!(de.parse("Strg+RightClick").unwrap(), "ctrl+rightclick");
        assert!(de.parse("Ctrl+").is_err());
        assert!(de.parse("Ctrl+Nope").is_err());
    }

    #[test]
    fn round_trip() {
        let de = german();
        for stored in ["ctrl+y", "shift+space", "rightalt+minus", "5 1", "ctrl", "shift+leftclick", "leftclick leftclick"] {
            let action = crate::settings::parse_action(stored).unwrap();
            assert_eq!(de.parse(&de.action_text(&action)).unwrap(), stored);
        }
    }
}
