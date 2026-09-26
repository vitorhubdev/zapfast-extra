//! One-time adoption of an earlier install.
//!
//! This is the only module that names ZapFast, ZapExt, FastsApp, and the old
//! keyring service. It reads those locations so an existing session and its
//! archive key can move; nothing else in the program accepts them.

/// `directories` qualifier used before Vespera.
pub const LEGACY_QUALIFIER: &str = "me";
/// `directories` organization used before Vespera.
pub const LEGACY_ORGANIZATION: &str = "paolino";
/// Application names, newest first. An existing destination is left untouched.
pub const LEGACY_APP_NAMES: &[&str] = &["zapfast", "fastsapp", "fastwhatsapp"];
/// eframe persistence ids for those same installs.
pub const LEGACY_EFRAME_IDS: &[&str] = LEGACY_APP_NAMES;
/// Executable stems that used to hold the instance port.
pub const LEGACY_EXECUTABLES: &[&str] = &["zapext", "zapfast", "fastsapp"];
/// macOS bundle names that are no longer accepted.
pub const LEGACY_APP_BUNDLES: &[&str] = &["ZapExt.app", "ZapFast.app", "FastsApp.app"];
/// Command name printed by `--version` before Vespera.
pub const LEGACY_COMMAND: &str = "zapext";
/// OS keyring service that stored the archive key.
pub const LEGACY_KEYRING_SERVICE: &str = "rocks.zapfast.ZapFast";
/// OS keyring service that stores the archive key now.
pub const KEYRING_SERVICE: &str = "io.github.vitorhubdev.Vespera";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_names_stay_available_for_the_one_move() {
        assert_eq!(LEGACY_APP_NAMES, ["zapfast", "fastsapp", "fastwhatsapp"]);
        assert_eq!(LEGACY_EXECUTABLES, ["zapext", "zapfast", "fastsapp"]);
        assert_eq!(
            LEGACY_APP_BUNDLES,
            ["ZapExt.app", "ZapFast.app", "FastsApp.app"]
        );
        assert_eq!(LEGACY_COMMAND, "zapext");
        assert_eq!(LEGACY_KEYRING_SERVICE, "rocks.zapfast.ZapFast");
        assert_eq!(KEYRING_SERVICE, "io.github.vitorhubdev.Vespera");
        assert_eq!(LEGACY_QUALIFIER, "me");
        assert_eq!(LEGACY_ORGANIZATION, "paolino");
    }
}
