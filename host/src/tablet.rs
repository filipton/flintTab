//! Lifts the tablet's own refresh-rate caps while the host runs, and puts them back after.
//!
//! Battery saver and Samsung's "Motion smoothness: Standard" both cap the panel at 60 Hz,
//! above anything an app may ask for, and battery saver also slows the video decoder. Each
//! refresh at 90/120 Hz instead of 60 takes up to 6-8 ms off every frame's wait for the panel.
//!
//! The original values go to a file in the cache folder before anything is changed, so they
//! are restored on exit, or on the next start if the host did not get to it (crash, unplug).

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

/// (namespace, key, value while streaming). Keys a device does not have are left alone.
const TWEAKS: &[(&str, &str, &str)] = &[
    ("global", "low_power", "0"),          // battery saver off
    ("secure", "refresh_rate_mode", "1"), // Samsung motion smoothness: adaptive (up to the panel's max)
];

fn saved(serial: &str) -> Option<PathBuf> {
    Some(crate::app::cache_dir()?.join(format!("tablet-settings-{serial}")))
}

fn get(adb: &str, serial: &str, ns: &str, key: &str) -> Option<String> {
    let out = Command::new(adb).args(["-s", serial, "shell", "settings", "get", ns, key]).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn put(adb: &str, serial: &str, ns: &str, key: &str, value: &str) -> bool {
    let args: Vec<&str> = if value == "null" {
        vec!["-s", serial, "shell", "settings", "delete", ns, key]
    } else {
        vec!["-s", serial, "shell", "settings", "put", ns, key, value]
    };
    Command::new(adb).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Saves the current values (unless an earlier run's are still pending) and applies the tweaks.
pub fn apply(adb: &str, serial: &str) {
    let Some(file) = saved(serial) else { return };
    if !file.exists() {
        let lines: Vec<String> = TWEAKS
            .iter()
            .filter_map(|(ns, key, _)| Some(format!("{ns} {key} {}", get(adb, serial, ns, key)?)))
            // An unset low_power is restored by deleting it; a key that does not exist at all
            // (refresh_rate_mode on a non-Samsung tablet) is not touched.
            .filter(|l| !l.ends_with(" null") || l.contains("low_power"))
            .collect();
        if std::fs::write(&file, lines.join("\n")).is_err() {
            return;
        }
    }
    let originals = std::fs::read_to_string(&file).unwrap_or_default();
    let mut changed = Vec::new();
    for (ns, key, value) in TWEAKS {
        let had = originals.lines().find_map(|l| l.strip_prefix(&format!("{ns} {key} ")));
        if had.is_some_and(|v| v != *value) && put(adb, serial, ns, key, value) {
            changed.push(*key);
        }
    }
    if !changed.is_empty() {
        println!("tablet: lifted the 60 Hz caps while streaming ({}); restored on exit", changed.join(", "));
    }
}

/// Puts back what [apply] changed, on every tablet that is still attached.
pub fn restore(adb: &str) {
    let Some(dir) = crate::app::cache_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(serial) = name.strip_prefix("tablet-settings-") else { continue };
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let mut ok = true;
        for l in text.lines() {
            let mut it = l.splitn(3, ' ');
            if let (Some(ns), Some(key), Some(value)) = (it.next(), it.next(), it.next()) {
                ok &= put(adb, serial, ns, key, value);
            }
        }
        if ok {
            let _ = std::fs::remove_file(e.path());
            println!("tablet: settings restored");
        }
    }
}
