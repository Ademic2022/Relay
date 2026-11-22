//! Work around screencapturekit's build script only knowing the Xcode.app
//! toolchain layout: with Command Line Tools selected, the Swift
//! compatibility libraries live under `<dev dir>/usr/lib/swift/macosx`.
fn main() {
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let dev = std::env::var("DEVELOPER_DIR").ok().or_else(|| {
        std::process::Command::new("xcode-select")
            .arg("-p")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });
    if let Some(dev) = dev {
        for sub in [
            "usr/lib/swift/macosx",
            "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx",
        ] {
            let p = std::path::Path::new(&dev).join(sub);
            if p.is_dir() {
                println!("cargo:rustc-link-search=native={}", p.display());
            }
        }
    }
}
