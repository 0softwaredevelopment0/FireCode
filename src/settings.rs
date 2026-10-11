use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Browser profile layout used as the token source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserPreset {
    #[default]
    Firefox,
    Chrome,
    Yandex,
}

impl BrowserPreset {
    pub fn display_name(self) -> &'static str {
        match self {
            BrowserPreset::Firefox => "Firefox",
            BrowserPreset::Chrome => "Chrome",
            BrowserPreset::Yandex => "Yandex",
        }
    }

    /// Default profile directory for the preset, if it can be guessed.
    /// Firefox profiles are resolved from profiles.ini elsewhere.
    pub fn default_profile_dir(self) -> Option<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        let dir = match self {
            BrowserPreset::Firefox => return None,
            BrowserPreset::Chrome => local
                .join("Google")
                .join("Chrome")
                .join("User Data")
                .join("Default"),
            BrowserPreset::Yandex => local
                .join("Yandex")
                .join("YandexBrowser")
                .join("User Data")
                .join("Default"),
        };
        Some(dir)
    }

    pub fn all() -> [BrowserPreset; 3] {
        [
            BrowserPreset::Firefox,
            BrowserPreset::Chrome,
            BrowserPreset::Yandex,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    #[serde(default)]
    pub allow_sudo: bool,
    /// Prepend AGENTS.md (from the exe directory, if present) to every
    /// outgoing user message.
    #[serde(default)]
    pub include_agents_md: bool,
    /// Agent working directory. `"local"` (the default) means the directory
    /// the app was launched from; anything else is a custom path.
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
}

fn default_working_dir() -> String {
    "local".to_owned()
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            allow_sudo: false,
            include_agents_md: false,
            working_dir: default_working_dir(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowserSettings {
    #[serde(default)]
    pub preset: BrowserPreset,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub agent: AgentSettings,
    #[serde(default)]
    pub browser: BrowserSettings,
}

pub fn settings_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "0softwaredevelopment0", "FireCode")
        .ok_or_else(|| anyhow::anyhow!("cannot resolve config directory"))?;
    Ok(dirs.config_dir().join("settings.toml"))
}

pub fn load_or_create() -> Result<Settings> {
    let path = settings_path()?;
    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let settings: Settings = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        return Ok(settings);
    }
    let settings = Settings::default();
    save(&settings)?;
    Ok(settings)
}

pub fn save(settings: &Settings) -> Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let serialized = toml::to_string_pretty(settings).context("failed to serialize settings")?;
    fs::write(&path, &serialized)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_firefox_without_sudo() {
        let settings = Settings::default();
        assert_eq!(settings.browser.preset, BrowserPreset::Firefox);
        assert!(!settings.agent.allow_sudo);
    }

    #[test]
    fn defaults_point_working_dir_at_local() {
        let settings = Settings::default();
        assert_eq!(settings.agent.working_dir, "local");
    }

    #[test]
    fn settings_round_trip() {
        let settings = Settings {
            agent: AgentSettings {
                allow_sudo: true,
                include_agents_md: true,
                working_dir: "C:\\work".to_owned(),
            },
            browser: BrowserSettings {
                preset: BrowserPreset::Chrome,
            },
        };
        let raw = toml::to_string_pretty(&settings).expect("serialize");
        let parsed: Settings = toml::from_str(&raw).expect("parse");
        assert!(parsed.agent.allow_sudo);
        assert!(parsed.agent.include_agents_md);
        assert_eq!(parsed.agent.working_dir, "C:\\work");
        assert_eq!(parsed.browser.preset, BrowserPreset::Chrome);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let parsed: Settings = toml::from_str("[agent]\n").expect("parse");
        assert!(!parsed.agent.allow_sudo);
        assert_eq!(parsed.browser.preset, BrowserPreset::Firefox);
    }

    #[test]
    fn chromium_profile_dirs_point_at_default_profile() {
        let chrome = BrowserPreset::Chrome
            .default_profile_dir()
            .expect("chrome dir");
        assert!(chrome.ends_with("Default"));
        assert!(chrome.to_string_lossy().contains("Chrome"));
        let yandex = BrowserPreset::Yandex
            .default_profile_dir()
            .expect("yandex dir");
        assert!(yandex.ends_with("Default"));
        assert!(yandex.to_string_lossy().contains("Yandex"));
    }
}
