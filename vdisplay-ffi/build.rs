fn main() {
    if cfg!(target_os = "macos") {
        // Compile the Objective-C wrapper
        cc::Build::new()
            .file("src/virtual_display.mm")
            .cpp(true)
            .flag("-std=c++17")
            .flag("-ObjC++")
            .compile("vdisplay");

        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=IOKit");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");

        println!("cargo:rerun-if-changed=src/virtual_display.mm");
    }
}
