//! Embeds the icon and version information in Windows executables.

/// Packs "major.minor.patch" the way a Windows version resource expects it.
#[cfg(windows)]
fn packed_version(version: &str) -> Option<u64> {
    let mut parts = version
        .split('.')
        .map(|part| part.trim().parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next()??;
    if [major, minor, patch].iter().any(|part| *part > 0xFFFF) {
        return None;
    }
    Some((major << 48) | (minor << 32) | (patch << 16))
}

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/windows/vespera.ico");
        println!("cargo:rerun-if-changed=VERSION");
        // The fork's VERSION is what users see in the file properties; the
        // crate keeps the upstream package version for compatibility.
        let version = std::fs::read_to_string("VERSION").unwrap_or_default();
        let version = version.trim();
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("packaging/windows/vespera.ico")
            .set("ProductName", "ZapExt")
            .set("FileDescription", "ZapExt")
            .set("CompanyName", "vitorhubdev")
            .set(
                "LegalCopyright",
                "ZapExt fork by vitorhubdev, based on ZapFast under MIT",
            );
        if !version.is_empty() {
            resource
                .set("FileVersion", version)
                .set("ProductVersion", version);
        }
        if let Some(packed) = packed_version(version) {
            resource
                .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
                .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
        }
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows resources not embedded: {error}");
        }
    }
}
