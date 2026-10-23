fn main() {
    // Only builds — and only compiles — on a Windows host: `winresource` is a
    // build-dependency under `cfg(windows)`, and a build script's cfg matches
    // the host it runs on. The Windows release is produced on Windows anyway.
    #[cfg(windows)]
    embed_windows_resources();

    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}

#[cfg(windows)]
fn embed_windows_resources() {
    let mut res = winresource::WindowsResource::new();
    // Tolerated as missing so a fresh clone builds before the art does.
    if std::path::Path::new("assets/icon.ico").exists() {
        res.set_icon("assets/icon.ico");
    }
    if std::path::Path::new("assets/app.manifest").exists() {
        res.set_manifest_file("assets/app.manifest");
    }
    res.set("FileDescription", "Sonus");
    res.set("ProductName", "Sonus");
    res.set("LegalCopyright", "MIT licensed");
    if let Err(e) = res.compile() {
        eprintln!("cargo:warning=resource compile failed: {e}");
    }
}
