#[path = "build/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/icon.rs");
    println!("cargo:rerun-if-changed=src/ui/glyph.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Derive every resolution from the glyph's geometry, avoiding a stale or
    // machine-local asset. No icon generation happens at application startup.
    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"))
        .join("kyouko.ico");
    std::fs::write(&path, icon::encode()).expect("write multi-resolution application icon");
    let mut resources = winres::WindowsResource::new();
    resources.set_icon_with_id(path.to_str().expect("UTF-8 icon path"), "1");
    resources.compile().unwrap_or_else(|error| {
        panic!(
            "Windows resource compilation failed: {error}. Install the Windows SDK Resource \
             Compiler (rc.exe) for MSVC, or windres for MinGW. Icon embedding is required."
        )
    });
}
