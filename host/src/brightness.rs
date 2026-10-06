//! The tablet's screen brightness, set from this computer: `--brightness`, and on macOS the
//! brightness keys while the mouse is on the tablet's display (macOS has no brightness of
//! its own for a virtual display). Remembered per tablet, across sessions and runs.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
};

use crate::{app, protocol};

/// One tablet's brightness for one session.
pub struct Brightness {
    tablet: String,
    tx: mpsc::Sender<Vec<u8>>,
    /// The tablet's own setting (KIND_BRIGHTNESS), where keys start from when none is set here.
    own: Mutex<Option<u8>>,
}

/// Per tablet: the level set from here (None: the tablet's own). Read from `--brightness` or
/// the saved file the first time a tablet connects.
static LEVELS: Mutex<Option<HashMap<String, Option<u8>>>> = Mutex::new(None);

/// `--brightness`: a percentage, or None for the tablet's own setting.
#[derive(Clone, Copy)]
pub struct Level(pub Option<u8>);

/// `--brightness`: 0..=100, or "tablet" for the tablet's own setting.
pub fn parse_arg(s: &str) -> Result<Level, String> {
    if s == "tablet" || s == "auto" {
        return Ok(Level(None));
    }
    match s.trim_end_matches('%').parse::<u8>() {
        Ok(v) if v <= 100 => Ok(Level(Some(v))),
        _ => Err("a percentage 0-100, or \"tablet\"".into()),
    }
}

fn file(tablet: &str) -> Option<PathBuf> {
    let name: String = tablet.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    Some(app::cache_dir()?.join(format!("brightness-{name}")))
}

fn save(tablet: &str, level: Option<u8>) {
    let Some(f) = file(tablet) else { return };
    match level {
        Some(v) => std::fs::write(f, v.to_string()).ok(),
        None => std::fs::remove_file(f).ok(),
    };
}

impl Brightness {
    /// Sends the tablet its brightness for this session (`arg`: `--brightness`, which wins
    /// over the saved level the first time this tablet connects).
    pub fn start(tablet: &str, arg: Option<Level>, tx: mpsc::Sender<Vec<u8>>) -> Arc<Self> {
        let level = {
            let mut levels = LEVELS.lock().unwrap();
            let levels = levels.get_or_insert_with(HashMap::new);
            *levels.entry(tablet.to_owned()).or_insert_with(|| match arg {
                Some(Level(level)) => {
                    save(tablet, level);
                    level
                }
                None => file(tablet)
                    .and_then(|f| std::fs::read_to_string(f).ok())
                    .and_then(|s| parse_arg(s.trim()).ok()?.0),
            })
        };
        let b = Arc::new(Self { tablet: tablet.to_owned(), tx, own: Mutex::new(None) });
        b.send(level);
        if let Some(v) = level {
            println!("tablet brightness {v}%");
        }
        b
    }

    fn send(&self, level: Option<u8>) {
        self.tx.send(protocol::brightness_msg(level.unwrap_or(protocol::BRIGHTNESS_TABLET))).ok();
    }

    /// KIND_BRIGHTNESS: the tablet's own setting.
    pub fn tablet_reported(&self, v: u8) {
        *self.own.lock().unwrap() = Some(v.min(100));
    }

    /// Up or down `steps` of `of` across the range (macOS: 16 per key, 64 with ⌥⇧).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn step(&self, steps: i32, of: i32) -> u8 {
        let level = {
            let mut levels = LEVELS.lock().unwrap();
            let levels = levels.get_or_insert_with(HashMap::new);
            let current = levels.get(&self.tablet).copied().flatten().or(*self.own.lock().unwrap()).unwrap_or(50);
            let at = (current as f64 * of as f64 / 100.0).round() as i32;
            let level = ((at + steps).clamp(0, of) as f64 * 100.0 / of as f64).round() as u8;
            levels.insert(self.tablet.clone(), Some(level));
            level
        };
        self.send(Some(level));
        save(&self.tablet, Some(level));
        level
    }
}
