//! Ready-made profiles for well-known applications, built on their default shortcuts.
//!
//! Shortcuts are written as the characters the application expects and turned into key
//! positions for the user's keyboard layout when a profile is created from a preset.

use anyhow::{Context, Result};

use crate::layout::Layout;
use crate::settings::{Profile, EXPRESS_KEYS};

pub struct Preset {
    /// Suggested profile name.
    pub name: &'static str,
    pub title: &'static str,
    /// Window classes of the application, bound to the new profile.
    pub apps: &'static [&'static str],
    /// What each express key does and its shortcut, top to bottom.
    pub keys: [(&'static str, &'static str); EXPRESS_KEYS],
    /// What the dial does, then its clockwise and counter-clockwise shortcuts.
    pub dial: (&'static str, &'static str, &'static str),
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        name: "photoshop",
        title: "Photoshop",
        apps: &["photoshop.exe"],
        keys: [
            ("Brush", "B"),
            ("Eraser", "E"),
            ("Eyedropper while held", "Alt"),
            ("Pan while held", "Space"),
            ("Undo", "Ctrl+Z"),
            ("Redo", "Ctrl+Shift+Z"),
            ("Swap colours", "X"),
            ("Save", "Ctrl+S"),
        ],
        dial: ("Brush size", "]", "["),
    },
    Preset {
        name: "krita",
        title: "Krita",
        apps: &["krita"],
        keys: [
            ("Brush", "B"),
            ("Eraser mode", "E"),
            ("Colour picker while held", "Ctrl"),
            ("Pan while held", "Space"),
            ("Undo", "Ctrl+Z"),
            ("Redo", "Ctrl+Shift+Z"),
            ("Swap colours", "X"),
            ("Save", "Ctrl+S"),
        ],
        dial: ("Brush size", "]", "["),
    },
    Preset {
        name: "gimp",
        title: "GIMP",
        apps: &["gimp", "gimp-3.2", "gimp-3.0"],
        keys: [
            ("Paintbrush", "P"),
            ("Eraser", "Shift+E"),
            ("Colour picker while held", "Ctrl"),
            ("Pan while held", "Space"),
            ("Undo", "Ctrl+Z"),
            ("Redo", "Ctrl+Y"),
            ("Swap colours", "X"),
            ("Save", "Ctrl+S"),
        ],
        dial: ("Brush size", "]", "["),
    },
    Preset {
        name: "inkscape",
        title: "Inkscape",
        apps: &["org.inkscape.inkscape", "inkscape"],
        keys: [
            ("Selector", "S"),
            ("Node tool", "N"),
            ("Pencil", "P"),
            ("Constrain while held", "Ctrl"),
            ("Undo", "Ctrl+Z"),
            ("Redo", "Ctrl+Shift+Z"),
            ("Duplicate", "Ctrl+D"),
            ("Save", "Ctrl+S"),
        ],
        dial: ("Zoom", "+", "-"),
    },
    Preset {
        name: "blender",
        title: "Blender (sculpting)",
        apps: &["blender"],
        keys: [
            ("Invert brush while held", "Ctrl"),
            ("Smooth while held", "Shift"),
            ("Brush size", "F"),
            ("Switch mode", "Tab"),
            ("Undo", "Ctrl+Z"),
            ("Redo", "Ctrl+Shift+Z"),
            ("Brush strength", "Shift+F"),
            ("Save", "Ctrl+S"),
        ],
        dial: ("Brush size", "]", "["),
    },
];

pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

impl Preset {
    /// The preset as a profile for a keyboard with the given layout.
    pub fn profile(&self, layout: &Layout) -> Result<Profile> {
        let position = |shortcut: &str| layout.parse(shortcut).with_context(|| format!("preset '{}', shortcut {shortcut}", self.name));
        Ok(Profile {
            keys: self.keys.iter().map(|(_, shortcut)| position(shortcut)).collect::<Result<_>>()?,
            dial_cw: position(self.dial.1)?,
            dial_ccw: position(self.dial.2)?,
            ..Profile::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_on_common_layouts() {
        for (id, variant) in [("us", ""), ("de", ""), ("fr", ""), ("gb", "")] {
            let layout = Layout::load((id.into(), variant.into()), id.into());
            for preset in &PRESETS {
                let profile = preset.profile(&layout).unwrap();
                assert!(profile.keys.iter().all(|k| crate::settings::parse_action(k).is_ok_and(|a| a != crate::settings::Action::None)));
            }
        }
    }

    #[test]
    fn photoshop_on_a_german_keyboard() {
        let layout = Layout::load(("de".into(), String::new()), "German".into());
        let profile = find("Photoshop").unwrap().profile(&layout).unwrap();
        // Undo is Ctrl plus the key labelled Z, which sits where a US keyboard has Y.
        assert_eq!(profile.keys[4], "ctrl+y");
        // The brackets need AltGr on this layout.
        assert_eq!((profile.dial_cw.as_str(), profile.dial_ccw.as_str()), ("rightalt+9", "rightalt+8"));
    }
}
