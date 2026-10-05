use std::process::Command;

// screencapturekit's Swift bridge links libswift_Concurrency via @rpath, but rpath link args from a
// dependency's build script never reach our binary, so add them here (works from any directory,
// unlike a .cargo/config.toml).
fn main() {
    embed_apk();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    if let Ok(out) = Command::new("xcode-select").arg("-p").output()
        && out.status.success()
    {
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
        for lib in [
            format!("{dev}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"),
            format!("{dev}/usr/lib/swift/macosx"),
        ] {
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{lib}");
        }
    }
}

/// Release builds carry the tablet app (TABDISPLAY_APK=path), so the host is one file; others
/// find it on disk or download it (src/app.rs).
fn embed_apk() {
    println!("cargo:rerun-if-env-changed=TABDISPLAY_APK");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded.apk");
    match std::env::var("TABDISPLAY_APK") {
        Ok(apk) => {
            println!("cargo:rerun-if-changed={apk}");
            std::fs::copy(&apk, &out).expect("TABDISPLAY_APK: cannot read the APK");
        }
        Err(_) => std::fs::write(&out, b"").unwrap(),
    }
}
