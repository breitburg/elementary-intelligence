// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::APP_ID;

fn default_true() -> bool {
    true
}

fn default_shortcut() -> String {
    "<Control><Shift>space".to_string()
}

fn default_screenshot_shortcut() -> String {
    "<Control><Shift>s".to_string()
}

fn default_api_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_model() -> String {
    "gpt-4o-mini".to_string()
}

fn default_system_prompt() -> String {
    "You're a helpful assistant called El. You aim to respond in 1-2 sentences, \
     straight to the point."
        .to_string()
}

/// Every field carries a default so configs from older versions (which lack
/// the API fields and may still contain the dropped `[[services]]` table)
/// parse cleanly; unknown keys are ignored and the next save rewrites the
/// file in the current shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Whether the global shortcuts are active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// GTK accelerator string, e.g. `<Control><Shift>space`.
    #[serde(default = "default_shortcut")]
    pub shortcut: String,
    /// Accelerator that opens the entry with a screenshot attached.
    #[serde(default = "default_screenshot_shortcut")]
    pub screenshot_shortcut: String,
    /// Whether to launch the background service on login.
    #[serde(default = "default_true")]
    pub start_on_login: bool,
    /// OpenAI-compatible API root, e.g. `https://api.openai.com/v1`.
    #[serde(default = "default_api_base_url")]
    pub api_base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// System prompt prepended to every conversation. Empty = none.
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    /// Names of the tools the model may call, e.g. `["bash"]`. Empty disables
    /// tool calling entirely. See `tools::catalog` for the available names.
    #[serde(default)]
    pub enabled_tools: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            shortcut: default_shortcut(),
            screenshot_shortcut: default_screenshot_shortcut(),
            start_on_login: true,
            api_base_url: default_api_base_url(),
            api_key: String::new(),
            model: default_model(),
            system_prompt: default_system_prompt(),
            enabled_tools: Vec::new(),
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        let mut path = PathBuf::from(gtk4::glib::user_config_dir());
        path.push(APP_ID);
        path.push("config.toml");
        path
    }

    /// Load the config from disk, falling back to defaults on any error. On
    /// first run the defaults are written out so the file is there to edit.
    pub fn load() -> Self {
        let path = Self::path();
        match fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|err| {
                eprintln!("Could not parse {}: {err}; using defaults", path.display());
                Config::default()
            }),
            Err(_) => {
                let config = Config::default();
                config.save();
                config
            }
        }
    }

    /// Persist the config to disk, creating the directory if needed.
    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                eprintln!("Could not create {}: {err}", parent.display());
                return;
            }
        }
        match toml::to_string_pretty(self) {
            Ok(contents) => {
                if let Err(err) = fs::write(&path, contents) {
                    eprintln!("Could not write {}: {err}", path.display());
                    return;
                }
                // The file holds the API key; keep it private to the user.
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
            Err(err) => eprintln!("Could not serialize config: {err}"),
        }
    }
}
