//! Installs or updates the tablet app over adb, so it never has to be installed by hand.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const PACKAGE: &str = "dev.tabdisplay";
const APK_NAME: &str = "tabdisplay.apk";
const REPO: &str = "filipton/macos-usb-display";

/// Attached to every release by tools/release.sh.
fn release_url() -> String {
    format!("https://github.com/{REPO}/releases/latest/download/{APK_NAME}")
}

/// The APK to install: `--apk`, then `tabdisplay.apk` next to the host or in the current
/// folder, then a local Gradle build, then the one built into the host, then the published
/// one (downloaded into a cache).
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
    local.or_else(embedded).or_else(download)
}

/// The app built into release hosts, written to the cache folder (once per build).
fn embedded() -> Option<PathBuf> {
    static APK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded.apk"));
    if APK.is_empty() {
        return None;
    }
    let path = cache_dir()?.join(format!("tabdisplay-{:x}.apk", md5::compute(APK)));
    if !path.is_file() {
        let part = path.with_extension("part");
        std::fs::write(&part, APK).ok()?;
        std::fs::rename(&part, &path).ok()?;
    }
    Some(path)
}

/// The host's cache folder (created if missing).
pub fn cache_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            Some(if cfg!(target_os = "macos") { home.join("Library/Caches") } else { home.join(".cache") })
        })?
        .join("tabdisplay");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Downloads the published APK: with curl when the repository is public (only when the
/// server has a newer one), else with the GitHub CLI, which can read a private repository.
fn download() -> Option<PathBuf> {
    let dir = cache_dir()?;
    let path = dir.join("tabdisplay-latest.apk");
    let part = dir.join("download.part");
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "120", "-o"]).arg(&part);
    if path.is_file() {
        cmd.arg("-z").arg(&path); // skip when ours is up to date
    }
    cmd.arg(release_url()).stderr(Stdio::null());
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false)
        || Command::new("gh")
            .args(["release", "download", "--repo", REPO])
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

/// How to install adb here, ready to copy.
fn adb_help() {
    let cmd = if cfg!(target_os = "macos") {
        "brew install android-platform-tools"
    } else {
        // /etc/os-release: ID=ubuntu, ID_LIKE="debian" ...
        let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let ids: Vec<&str> = os
            .lines()
            .filter_map(|l| l.strip_prefix("ID=").or_else(|| l.strip_prefix("ID_LIKE=")))
            .flat_map(|v| v.trim_matches('"').split_whitespace())
            .collect();
        let is = |id: &str| ids.contains(&id);
        if is("debian") || is("ubuntu") {
            "sudo apt install adb"
        } else if is("fedora") || is("rhel") {
            "sudo dnf install android-tools"
        } else if is("arch") {
            "sudo pacman -S android-tools"
        } else if is("opensuse") || is("suse") {
            "sudo zypper install android-tools"
        } else {
            "install adb (Android platform-tools) with your package manager"
        }
    };
    eprintln!("adb is needed to talk to the tablet. Install it, then run this again:\n\n    {cmd}\n");
}

/// The adb to use: `wanted` if it runs, else Google's platform-tools, downloaded once into the
/// cache folder (published for macOS and x86-64 Linux), so nothing has to be installed.
pub fn find_adb(wanted: &str) -> String {
    let runs = |adb: &Path| Command::new(adb).arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if wanted != "adb" || runs(Path::new(wanted)) {
        return wanted.to_owned();
    }
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "darwin",
        ("linux", "x86_64") => "linux",
        _ => {
            adb_help();
            return wanted.to_owned();
        }
    };
    let Some(dir) = cache_dir() else { return wanted.to_owned() };
    let adb = dir.join("platform-tools/adb");
    if runs(&adb) {
        return adb.to_string_lossy().into_owned();
    }
    println!("adb is not installed: downloading Android's platform-tools...");
    let url = format!("https://dl.google.com/android/repository/platform-tools-latest-{platform}.zip");
    let fetched = (|| -> anyhow::Result<()> {
        let mut zip = Vec::new();
        std::io::Read::read_to_end(&mut ureq::get(&url).call()?.into_body().into_reader(), &mut zip)?;
        zip::ZipArchive::new(std::io::Cursor::new(zip))?.extract(&dir)?;
        Ok(())
    })();
    match fetched {
        Ok(()) if runs(&adb) => adb.to_string_lossy().into_owned(),
        Ok(()) => {
            eprintln!("the downloaded adb does not run");
            adb_help();
            wanted.to_owned()
        }
        Err(e) => {
            eprintln!("could not download adb from {url}: {e:#}");
            adb_help();
            wanted.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    /// Downloads platform-tools into a temporary cache: `cargo test -- --ignored adb`.
    #[test]
    #[ignore]
    fn adb_download() {
        let dir = std::env::temp_dir().join(format!("tabdisplay-adb-test-{}", std::process::id()));
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", &dir);
            std::env::set_var("PATH", "");
        }
        let adb = super::find_adb("adb");
        assert!(adb.ends_with("platform-tools/adb"), "{adb}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
