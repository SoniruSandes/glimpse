// Embeds app.rc (the EXE icon + version info) when building for Windows.
fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icons/app.ico");

    // `cfg(windows)` here is the machine running the build; the target is what we embed for.
    #[cfg(windows)]
    {
        let target_is_windows = std::env::var("CARGO_CFG_TARGET_OS")
            .map(|os| os == "windows")
            .unwrap_or(false);
        if target_is_windows {
            embed_resource::compile("app.rc", embed_resource::NONE);
        }
    }
}