fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        // NDK r27 uses 4 KB pages by default. Align the bundled library for 16 KB devices too.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-z,max-page-size=16384,-z,common-page-size=16384");
        tauri_build::build();
    }
}
