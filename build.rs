//! Embeds the icon and version information in Windows executables.

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/windows/zapfast.ico");
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("packaging/windows/zapfast.ico")
            .set("ProductName", "ZapExt")
            .set("FileDescription", "ZapExt")
            .set("CompanyName", "vitorhubdev")
            .set(
                "LegalCopyright",
                "ZapExt fork by vitorhubdev, based on ZapFast under MIT",
            );
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows resources not embedded: {error}");
        }
    }
}
