//! Finds the icon of the application a window class belongs to, through the installed
//! desktop entries and icon themes.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

struct Entry {
    /// File name without `.desktop`, lowercase.
    stem: String,
    wm_class: Option<String>,
    icon: String,
}

pub struct IconIndex {
    entries: Vec<Entry>,
    icon_dirs: Vec<PathBuf>,
}

const THEMES: [&str; 4] = ["hicolor", "breeze", "breeze-dark", "Adwaita"];
const SIZES: [&str; 7] = ["256x256", "128x128", "scalable", "96x96", "64x64", "512x512", "48x48"];

impl IconIndex {
    pub fn load() -> IconIndex {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let data_dirs = [
            home.join(".local/share"),
            home.join(".local/share/flatpak/exports/share"),
            PathBuf::from("/var/lib/flatpak/exports/share"),
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
            // The host's applications as seen from inside a Flatpak sandbox.
            PathBuf::from("/run/host/usr/local/share"),
            PathBuf::from("/run/host/usr/share"),
        ];
        let mut entries = Vec::new();
        for dir in &data_dirs {
            let Ok(files) = fs::read_dir(dir.join("applications")) else { continue };
            for path in files.flatten().map(|f| f.path()).filter(|p| p.extension().is_some_and(|e| e == "desktop")) {
                let Ok(text) = fs::read_to_string(&path) else { continue };
                // Only the main group counts; actions further down repeat some keys.
                let main = text.lines().skip_while(|l| l.trim() != "[Desktop Entry]").skip(1).take_while(|l| !l.starts_with('['));
                let (mut icon, mut wm_class) = (None, None);
                for line in main {
                    if let Some(value) = line.strip_prefix("Icon=") {
                        icon = Some(value.trim().to_string());
                    } else if let Some(value) = line.strip_prefix("StartupWMClass=") {
                        wm_class = Some(value.trim().to_lowercase());
                    }
                }
                let (Some(icon), Some(stem)) = (icon, path.file_stem()) else { continue };
                entries.push(Entry { stem: stem.to_string_lossy().to_lowercase(), wm_class, icon });
            }
        }
        IconIndex { entries, icon_dirs: data_dirs.iter().map(|d| d.join("icons")).collect() }
    }

    /// The icon for a window class, as a `file://` address.
    pub fn lookup(&self, class: &str) -> Option<String> {
        let class = class.to_lowercase();
        let last = |name: &str| name.rsplit('.').next().unwrap_or(name).to_string();
        let rules: [&dyn Fn(&Entry) -> bool; 4] = [
            &|e| e.wm_class.as_deref() == Some(class.as_str()),
            &|e| e.stem == class,
            // "org.kde.krita" is the entry for the window class "krita", and the other way round.
            &|e| last(&e.stem) == class,
            &|e| e.stem == last(&class),
        ];
        let entry = rules.iter().find_map(|rule| self.entries.iter().find(|e| rule(e)))?;
        let file = self.icon_file(&entry.icon)?;
        Some(format!("file://{}", file.display()))
    }

    fn icon_file(&self, name: &str) -> Option<PathBuf> {
        if name.starts_with('/') {
            return Path::new(name).exists().then(|| PathBuf::from(name));
        }
        let mut candidates = Vec::new();
        for dir in &self.icon_dirs {
            for theme in THEMES {
                for size in SIZES {
                    for extension in ["png", "svg"] {
                        candidates.push(dir.join(format!("{theme}/{size}/apps/{name}.{extension}")));
                    }
                }
                // Breeze sorts by kind first and size second.
                candidates.push(dir.join(format!("{theme}/apps/64/{name}.svg")));
                candidates.push(dir.join(format!("{theme}/apps/48/{name}.svg")));
            }
        }
        for extension in ["png", "svg"] {
            candidates.push(PathBuf::from(format!("/usr/share/pixmaps/{name}.{extension}")));
        }
        candidates.into_iter().find(|path| path.exists())
    }
}
