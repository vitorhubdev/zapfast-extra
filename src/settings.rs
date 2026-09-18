//! User preferences stored in JSON.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::System => "Follow system",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// Filename of the selected local JSON palette.
    pub custom_theme: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::theme::custom::read_cached_theme",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_theme_cache: Option<crate::theme::custom::CustomTheme>,
    #[serde(
        default,
        deserialize_with = "crate::theme::custom::read_cached_theme",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_theme_cache: Option<crate::theme::custom::CustomTheme>,
    /// egui zoom factor.
    pub zoom: f32,
    pub sidebar_width: f32,
    /// Whether Enter sends and Shift+Enter adds a line. Off swaps them.
    pub enter_sends: bool,
    /// Send read receipts, subject to the account privacy setting.
    pub send_read_receipts: bool,
    /// Send typing state while composing.
    pub send_typing: bool,
    /// Download attachments when they enter view instead of on click.
    #[serde(alias = "auto_download_images")]
    pub auto_download: bool,
    /// Show sender avatars outside groups too.
    pub show_sender_pictures: bool,
    /// Last open chat, restored at startup.
    pub last_chat: Option<String>,
    pub show_shortcut_hints: bool,
    /// Recently used emoji, newest first.
    pub recent_emoji: Vec<String>,
    /// Start the next voice message in a chat when one finishes.
    #[serde(default = "default_true")]
    pub play_next_audio: bool,
    /// Keep the app linked in the tray when the window closes.
    pub keep_running_in_background: bool,
    /// Desktop notifications while away from the chat.
    pub notifications: bool,
    /// Ask GitHub once a day whether a newer release exists.
    pub check_for_updates: bool,
    /// Download verified updates in the background; restarting remains explicit.
    pub download_updates_automatically: bool,
    /// Prefer address-book names over public profile names.
    pub names_from_contacts: bool,
    /// Also add saved contacts to the phone's address book.
    pub save_contacts_to_phone: bool,
    /// Picker tab reopened above the composer. Old files default to emoji.
    #[serde(default, deserialize_with = "picker_tab_from_name")]
    pub picker_tab: crate::model::PickerTab,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            custom_theme: None,
            custom_theme_cache: None,
            system_theme_cache: None,
            zoom: 1.0,
            sidebar_width: 320.0,
            enter_sends: true,
            send_read_receipts: true,
            send_typing: true,
            auto_download: true,
            show_sender_pictures: false,
            last_chat: None,
            show_shortcut_hints: true,
            recent_emoji: Vec::new(),
            play_next_audio: true,
            keep_running_in_background: true,
            notifications: true,
            check_for_updates: true,
            download_updates_automatically: false,
            names_from_contacts: true,
            save_contacts_to_phone: true,
            picker_tab: crate::model::PickerTab::Emoji,
        }
    }
}

/// Whether a preference that defaults to on is missing from the file.
fn default_true() -> bool {
    true
}

impl Settings {
    pub(crate) fn cached_palette(&self) -> Option<crate::theme::Palette> {
        let theme = if self.custom_theme.is_some() {
            self.custom_theme_cache.as_ref()
        } else if self.theme == ThemeChoice::System {
            self.system_theme_cache.as_ref()
        } else {
            None
        };
        theme.map(|theme| theme.palette)
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(settings) => settings,
                Err(error) => {
                    log::warn!("settings file is unreadable, using defaults: {error}");
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!("could not read settings: {error}");
                Self::default()
            }
        }
    }

    /// Atomically replaces the settings file through a temporary file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, contents)?;
        std::fs::rename(&temp, path)
    }
}

/// Reads the stored picker tab, mapping retired names to emoji.
///
/// The picker once had a GIF tab. A file written then still has to open with
/// every other setting intact, so an unknown name falls back to emoji instead
/// of discarding the whole file.
fn picker_tab_from_name<'de, D>(deserializer: D) -> Result<crate::model::PickerTab, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let name = String::deserialize(deserializer)?;
    Ok(match name.as_str() {
        "stickers" => crate::model::PickerTab::Stickers,
        "favorites" => crate::model::PickerTab::Favorites,
        _ => crate::model::PickerTab::Emoji,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let parsed: Settings =
            serde_json::from_str(r#"{"theme":"light","future_field":1}"#).expect("parses");
        assert_eq!(parsed.theme, ThemeChoice::Light);
        assert!(parsed.enter_sends);
        assert!(parsed.check_for_updates);
        assert!(!parsed.download_updates_automatically);
    }

    #[test]
    fn damaged_theme_cache_does_not_discard_other_settings() {
        let settings: Settings = serde_json::from_str(r#"{"custom_theme":"mine.json","custom_theme_cache":{"damaged":true},"enter_sends":false}"#).unwrap();
        assert!(!settings.enter_sends);
        assert!(settings.custom_theme_cache.is_none());
        assert_eq!(settings.custom_theme.as_deref(), Some("mine.json"));
    }

    #[test]
    fn a_retired_gif_picker_tab_keeps_the_other_settings() {
        let settings: Settings =
            serde_json::from_str(r#"{"picker_tab":"gifs","zoom":1.25,"enter_sends":false}"#)
                .unwrap();
        assert_eq!(settings.picker_tab, crate::model::PickerTab::Emoji);
        assert_eq!(settings.zoom, 1.25);
        assert!(!settings.enter_sends);
        let stickers: Settings = serde_json::from_str(r#"{"picker_tab":"stickers"}"#).unwrap();
        assert_eq!(stickers.picker_tab, crate::model::PickerTab::Stickers);
        let favorites: Settings = serde_json::from_str(r#"{"picker_tab":"favorites"}"#).unwrap();
        assert_eq!(favorites.picker_tab, crate::model::PickerTab::Favorites);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("zapfast-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let settings = Settings {
            zoom: 1.25,
            enter_sends: false,
            ..Settings::default()
        };
        settings.save(&path).expect("saves");
        assert_eq!(Settings::load(&path), settings);
        let _ = std::fs::remove_dir_all(dir);
    }
}
