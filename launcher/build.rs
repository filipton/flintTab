// The bundle (TD_PAYLOAD: a .tar.zst made by tools/bundle-linux.sh) is built into the binary.
fn main() {
    println!("cargo:rerun-if-env-changed=TD_PAYLOAD");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("payload.tar.zst");
    let bytes = match std::env::var("TD_PAYLOAD") {
        Ok(p) => {
            println!("cargo:rerun-if-changed={p}");
            std::fs::read(&p).expect("TD_PAYLOAD: cannot read the bundle")
        }
        Err(_) => Vec::new(),
    };
    // Names the unpacked folder, so a new release never runs an old one's files.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in &bytes {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    println!("cargo:rustc-env=PAYLOAD_ID={h:016x}");
    std::fs::write(out, bytes).unwrap();
}
