//! Installs or updates the tablet app over adb, so it never has to be installed by hand.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::protocol;

const PACKAGE: &str = "dev.tabdisplay";
const APK_NAME: &str = "tabdisplay.apk";
const REPO: &str = "filipton/macos-usb-display";

/// Published by .github/workflows/android.yml, one release per protocol version.
fn release_url() -> String {
    format!(
        "https://github.com/{REPO}/releases/download/apk-v{}/{APK_NAME}",
        protocol::VERSION
    )
}

/// The APK to install: `--apk`, then `tabdisplay.apk` next to the host or in the current
/// folder, then a local Gradle build, then the published one (downloaded into a cache).
pub fn find_apk(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        candidates.push(dir.join(APK_NAME));
    }
    candidates.push(PathBuf::from(APK_NAME));
    let outputs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../android/app/build/outputs/apk");
    candidates.push(outputs.join("release/app-release.apk"));
    candidates.push(outputs.join("debug/app-debug.apk"));
    // The newest local build wins, so a fresh `gradlew assembleDebug` is picked up.
    let local = candidates
        .into_iter()
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p);
    local.or_else(download)
}

/// Downloads the published APK: with curl when the repository is public (only when the
/// server has a newer one), else with the GitHub CLI, which can read a private repository.
fn download() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            Some(if cfg!(target_os = "macos") { home.join("Library/Caches") } else { home.join(".cache") })
        })?
        .join("tabdisplay");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("tabdisplay-v{}.apk", protocol::VERSION));
    let part = dir.join("download.part");
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "120", "-o"]).arg(&part);
    if path.is_file() {
        cmd.arg("-z").arg(&path); // skip when ours is up to date
    }
    cmd.arg(release_url()).stderr(Stdio::null());
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false)
        || Command::new("gh")
            .args(["release", "download", &format!("apk-v{}", protocol::VERSION), "--repo", REPO])
            .args(["--pattern", APK_NAME, "--clobber", "--output"])
            .arg(&part)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if ok && part.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        std::fs::rename(&part, &path).ok()?;
    } else {
        let _ = std::fs::remove_file(&part);
        if !ok && !path.is_file() {
            eprintln!(
                "could not download the tablet app from {} (for a private repository, install the \
                 GitHub CLI and run `gh auth login`)",
                release_url()
            );
        }
    }
    path.is_file().then_some(path)
}

fn adb_output(adb: &str, serial: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(adb).args(["-s", serial]).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Installs `apk` unless the tablet already has exactly this build.
pub fn ensure_installed(adb: &str, serial: &str, apk: &Path) {
    let Ok(bytes) = std::fs::read(apk) else {
        eprintln!("cannot read {}", apk.display());
        return;
    };
    let want = format!("{:x}", md5::compute(&bytes));
    let have = adb_output(adb, serial, &["shell", "pm", "path", PACKAGE])
        .and_then(|o| o.lines().find_map(|l| l.trim().strip_prefix("package:").map(str::to_owned)))
        .and_then(|path| adb_output(adb, serial, &["shell", "md5sum", &path]))
        .and_then(|o| o.split_whitespace().next().map(str::to_owned));
    if have.as_deref() == Some(want.as_str()) {
        return;
    }
    println!(
        "{} the tablet app from {}...",
        if have.is_some() { "updating" } else { "installing" },
        apk.display()
    );
    let apk_arg = apk.to_string_lossy();
    let install = || adb_output(adb, serial, &["install", "-r", "-d", &apk_arg]).is_some();
    if install() {
        println!("tablet app installed");
        return;
    }
    // Usually a build signed with a different key (another computer's debug key).
    if have.is_some() {
        println!("reinstalling the tablet app (signed with a different key)");
        let _ = adb_output(adb, serial, &["uninstall", PACKAGE]);
        if install() {
            println!("tablet app installed");
            return;
        }
    }
    eprintln!("installing the tablet app failed; try `{adb} -s {serial} install -r {apk_arg}` to see why");
}
