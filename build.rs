// Embeds the app icon and version info into diskvibe.exe (Explorer, taskbar pin, Alt+Tab).
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "DiskVibe");
        res.set("ProductName", "DiskVibe");
        res.set("CompanyName", "Circuit Biscuit");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the app icon: {e}");
        }
    }
}
