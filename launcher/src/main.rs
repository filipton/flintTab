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
    cmd.args(std::env::args_os().skip(1))
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
