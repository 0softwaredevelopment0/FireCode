use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

pub const TOKEN_KIND_LOCAL_STORAGE: &str = "localStorage";
pub const TOKEN_KIND_COOKIE: &str = "cookie";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub profile: ProfileConfig,
    #[serde(default = "default_site_id")]
    pub default_site: String,
    #[serde(default)]
    pub sites: BTreeMap<String, SiteConfig>,
    #[serde(default = "default_presets")]
    pub presets: BTreeMap<String, PresetConfig>,
    #[serde(default = "default_preset_id")]
    pub default_preset: String,
    #[serde(default)]
    pub agent: AgentConfig,
}

/// Coding-agent loop settings. `working_dir` empty = the exe directory
/// (the app is portable, AGENTS.md lives next to it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_agent_shell")]
    pub shell: String,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default = "default_agent_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_agent_max_steps")]
    pub max_steps: u32,
}

fn default_agent_shell() -> String {
    "powershell".to_owned()
}

fn default_agent_timeout() -> u64 {
    300
}

fn default_agent_max_steps() -> u32 {
    25
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            shell: default_agent_shell(),
            working_dir: String::new(),
            timeout_secs: default_agent_timeout(),
            max_steps: default_agent_max_steps(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetConfig {
    pub display_name: String,
    /// Site id from `sites` this preset talks to.
    pub site: String,
    /// Model selector sent with each prompt (e.g. deepseek_chat).
    pub model_class: String,
    pub thinking_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteConfig {
    pub display_name: String,
    #[serde(default)]
    pub api_base: String,
    #[serde(default)]
    pub token: TokenSourceConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenSourceConfig {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub db_path: String,
    #[serde(default)]
    pub host_pattern: String,
    #[serde(default)]
    pub keys: Vec<String>,
}

fn default_site_id() -> String {
    "deepseek".to_owned()
}

fn default_preset_id() -> String {
    "deepseek".to_owned()
}

fn default_presets() -> BTreeMap<String, PresetConfig> {
    let mut presets = BTreeMap::new();
    presets.insert(
        default_preset_id(),
        PresetConfig {
            display_name: "DeepSeek (chat)".to_owned(),
            site: default_site_id(),
            model_class: "deepseek_chat".to_owned(),
            thinking_enabled: false,
        },
    );
    presets
}

impl Config {
    pub fn default_with_profile(profile_path: &str) -> Self {
        let mut sites = BTreeMap::new();
        sites.insert(
            default_site_id(),
            SiteConfig {
                display_name: "DeepSeek".to_owned(),
                api_base: "https://chat.deepseek.com".to_owned(),
                token: TokenSourceConfig {
                    kind: TOKEN_KIND_LOCAL_STORAGE.to_owned(),
                    db_path:
                        "storage/default/https+++chat.deepseek.com/ls/data.sqlite".to_owned(),
                    host_pattern: String::new(),
                    keys: vec!["userToken".to_owned()],
                },
            },
        );
        Self {
            profile: ProfileConfig {
                path: profile_path.to_owned(),
            },
            default_site: default_site_id(),
            sites,
            presets: default_presets(),
            default_preset: default_preset_id(),
            agent: AgentConfig::default(),
        }
    }

    pub fn site(&self, id: &str) -> Result<&SiteConfig> {
        self.sites
            .get(id)
            .ok_or_else(|| anyhow!("site '{id}' not found in config"))
    }

    pub fn preset(&self, id: &str) -> Result<&PresetConfig> {
        self.presets
            .get(id)
            .ok_or_else(|| anyhow!("preset '{id}' not found in config"))
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "0softwaredevelopment0", "FireCode")
        .ok_or_else(|| anyhow!("cannot resolve config directory"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn load_or_create() -> Result<(Config, PathBuf)> {
    let path = config_path()?;
    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        return Ok((config, path));
    }

    let profile = detect_default_profile().unwrap_or_default();
    let config = Config::default_with_profile(&profile.display().to_string());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let serialized = toml::to_string_pretty(&config).context("failed to serialize config")?;
    fs::write(&path, &serialized)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok((config, path))
}

/// Detects the default Firefox profile directory via profiles.ini.
pub fn detect_default_profile() -> Option<PathBuf> {
    let ini = dirs_home()
        .map(|home| {
            home.join("AppData")
                .join("Roaming")
                .join("Mozilla")
                .join("Firefox")
                .join("profiles.ini")
        })
        .filter(|p| p.exists())?;

    parse_profiles_ini(&ini)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

fn parse_profiles_ini(ini: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(ini).ok()?;
    let base = ini.parent()?;

    let mut install_default: Option<String> = None;
    let mut profile_defaults: Vec<String> = Vec::new();

    let mut current_section = String::new();
    let mut section_path: Option<String> = None;
    let mut section_is_default = false;

    let close_section = |section: &str,
                             path: &mut Option<String>,
                             is_default: &mut bool,
                             profile_defaults: &mut Vec<String>| {
        if section.starts_with("Profile") && *is_default {
            if let Some(p) = path.take() {
                profile_defaults.push(p);
            }
        }
        *is_default = false;
    };

    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            close_section(
                &current_section,
                &mut section_path,
                &mut section_is_default,
                &mut profile_defaults,
            );
            current_section = line[1..line.len() - 1].to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if current_section.starts_with("Install") && key.eq_ignore_ascii_case("default") {
            install_default.get_or_insert(value.to_owned());
        }
        if current_section.starts_with("Profile") {
            if key.eq_ignore_ascii_case("path") {
                section_path = Some(value.to_owned());
            }
            if key.eq_ignore_ascii_case("default") && value == "1" {
                section_is_default = true;
            }
        }
    }
    close_section(
        &current_section,
        &mut section_path,
        &mut section_is_default,
        &mut profile_defaults,
    );

    let rel = install_default.or_else(|| profile_defaults.first().cloned())?;
    let rel_path = PathBuf::from(&rel);
    if rel_path.is_absolute() {
        Some(rel_path)
    } else {
        Some(base.join(rel_path))
    }
}
