//! What the user has chosen, kept as JSON in the app's private files.

use egui_mobile::theme::ThemeConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Which microphone path Android is asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MicPath {
    /// Android's speech-recognition source: no gain riding, far-field where the phone has it.
    #[default]
    Speech,
    /// The raw capsule, for comparing against the tuned path.
    Raw,
    /// Whatever the phone does by default.
    Standard,
}

impl MicPath {
    pub const ALL: [MicPath; 3] = [MicPath::Speech, MicPath::Raw, MicPath::Standard];

    pub fn label(self) -> &'static str {
        match self {
            MicPath::Speech => "Speech",
            MicPath::Raw => "Raw",
            MicPath::Standard => "Standard",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the model pack to transcribe with.
    pub model: String,
    /// A Whisper language code, or `auto` to detect it per recording.
    pub language: String,
    /// Show a rough transcript of the last few seconds while recording.
    pub live_preview: bool,
    /// Transcribe recordings as they are made, rather than only on request.
    pub auto_transcribe: bool,
    /// Keep each finished note's text as a file in the phone's Documents folder.
    pub save_text: bool,
    /// Tell a recording's voices apart once it is transcribed, and label each speaker's turns.
    pub speakers: bool,
    pub mic: MicPath,
    /// Debug: replace the NPU with a scripted engine, to exercise the app where there is none.
    pub simulate: bool,
    /// The notification prompt is shown once, on the first recording.
    pub notifications_asked: bool,
    pub theme: ThemeConfig,
}

impl Default for Settings {
    fn default() -> Self {
        let mut theme = ThemeConfig::default();
        theme.select_preset("galactic");
        Self {
            model: crate::models::CATALOG[0].name.to_string(),
            language: "auto".into(),
            live_preview: true,
            auto_transcribe: true,
            save_text: true,
            speakers: true,
            mic: MicPath::default(),
            simulate: false,
            notifications_asked: false,
            theme,
        }
    }
}

impl Settings {
    /// The language to force, if one is chosen.
    pub fn forced_language(&self) -> Option<String> {
        (self.language != "auto" && !self.language.is_empty()).then(|| self.language.clone())
    }

    /// Read settings, falling back to defaults for a missing or unreadable file.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&temporary, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip_and_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("scribe-settings-{}", std::process::id()));
        let path = dir.join("nested").join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let mut settings = Settings::default();
        settings.language = "de".into();
        settings.live_preview = false;
        settings.mic = MicPath::Raw;
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        assert_eq!(settings.forced_language().as_deref(), Some("de"));
        assert_eq!(Settings::default().forced_language(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_from_an_older_build_keeps_what_it_has() {
        let dir = std::env::temp_dir().join(format!("scribe-settings-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"language":"fr","some_removed_field":3}"#).unwrap();
        let settings = Settings::load(&path);
        assert_eq!(settings.language, "fr");
        // What the older build never wrote takes its default, text files included.
        assert!(settings.auto_transcribe && settings.save_text);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
