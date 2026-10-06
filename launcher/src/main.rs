//! The Linux release as one file: the host and the GStreamer libraries and plugins it uses,
//! unpacked once into the cache folder and started from there, so nothing has to be installed.
//! (Only what every desktop has comes from the system: glibc, PipeWire, the GPU's video driver.)

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

static PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.tar.zst"));
const ID: &str = env!("PAYLOAD_ID");

fn cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("tabdisplay")
}

fn unpack(dir: &Path) -> std::io::Result<()> {
    let parent = dir.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!("unpacking-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let zst = ruzstd::decoding::StreamingDecoder::new(PAYLOAD).map_err(std::io::Error::other)?;
    tar::Archive::new(zst).unpack(&tmp)?;
    match std::fs::rename(&tmp, dir) {
        Ok(()) => {}
        Err(_) if dir.join("tabdisplay-host").is_file() => {
            let _ = std::fs::remove_dir_all(&tmp); // another copy unpacked it meanwhile
        }
        Err(e) => return Err(e),
    }
    // Older releases' files.
    if let Ok(entries) = std::fs::read_dir(parent) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("bundle-") && e.path() != dir {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Ok(())
}

fn main() {
    if PAYLOAD.is_empty() {
        eprintln!("this launcher was built without a bundle (see tools/bundle-linux.sh)");
        std::process::exit(1);
    }
    let dir = cache().join(format!("bundle-{ID}"));
    if !dir.join("tabdisplay-host").is_file() {
        println!("unpacking (first run of this version)...");
        if let Err(e) = unpack(&dir) {
            eprintln!("cannot unpack into {}: {e}", dir.display());
            std::process::exit(1);
        }
    }
    let lib = dir.join("lib");
    let mut cmd = Command::new(dir.join("tabdisplay-host"));
    cmd.args(std::env::args_os().skip(1));
    // GStreamer installed on the system, at least as new as the bundled one, wins: its VA
    // plugin matches the system's video driver (the bundled 1.24 one took frames and gave
    // nothing back with a newer Intel driver). TD_GSTREAMER=bundled|system decides instead.
    let choice = std::env::var("TD_GSTREAMER").unwrap_or_default();
    let system = match choice.as_str() {
        "bundled" => None,
        _ => system_gstreamer(choice == "system"),
    };
    if let Some(found) = system {
        println!("using this system's GStreamer {found}");
        let err = cmd.exec();
        eprintln!("cannot start the host: {err}");
        std::process::exit(1);
    }
    cmd
        // The host puts the original back for the programs it runs (adb, ...).
        .env("TD_ORIG_LD_LIBRARY_PATH", std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default())
        .env("LD_LIBRARY_PATH", &lib)
        // Only the bundled plugins (matching this GStreamer), scanned in-process.
        .env("GST_PLUGIN_SYSTEM_PATH_1_0", lib.join("gstreamer-1.0"))
        .env_remove("GST_PLUGIN_PATH_1_0")
        .env_remove("GST_PLUGIN_PATH")
        .env("GST_REGISTRY_1_0", dir.join("registry.bin"))
        .env("GST_REGISTRY_FORK", "no");
    let err = cmd.exec();
    eprintln!("cannot start the host: {err}");
    std::process::exit(1);
}

/// Minor version of the bundled GStreamer (1.x); the system's must be at least this.
const BUNDLED_MINOR: u32 = 24;

/// Plugins the host's pipelines need, and the encoders (one of them).
const NEEDED: &[&str] = &["coreelements", "app", "pipewire", "videoparsersbad"];
const CONVERT: &[&str] = &["videoconvertscale", "videoconvert"];
const ENCODERS: &[&str] = &["va", "nvcodec", "x264"];

/// The system's GStreamer ("1.26 in /usr/lib/gstreamer-1.0"), when it has what the host
/// needs; `any_version`: also when it is older than the bundled one.
fn system_gstreamer(any_version: bool) -> Option<String> {
    let dirs = ["/usr/lib/x86_64-linux-gnu", "/usr/lib64", "/usr/lib", "/usr/lib/aarch64-linux-gnu"];
    for d in dirs {
        let d = Path::new(d);
        let plugins = d.join("gstreamer-1.0");
        let has = |p: &str| plugins.join(format!("libgst{p}.so")).is_file();
        if !NEEDED.iter().all(|p| has(p)) || !CONVERT.iter().any(|p| has(p)) || !ENCODERS.iter().any(|p| has(p)) {
            continue;
        }
        // libgstreamer-1.0.so.0.2603.0: minor 26
        let minor = std::fs::read_dir(d).ok()?.flatten().find_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let v = n.strip_prefix("libgstreamer-1.0.so.0.")?;
            v.split('.').next()?.parse::<u32>().ok().map(|v| v / 100)
        })?;
        if minor >= BUNDLED_MINOR || any_version {
            return Some(format!("1.{minor} in {}", plugins.display()));
        }
    }
    None
}
