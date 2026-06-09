// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! System-wide hotkey registration.
//!
//! Wayland has no in-process global key grab, and Pantheon ships no
//! GlobalShortcuts portal. The reliable path is a *custom keybinding* in
//! `org.gnome.settings-daemon.plugins.media-keys`, which elementary's
//! settings-daemon honours: the compositor runs our command when the combo is
//! pressed, and that command re-invokes us with `--spotlight`.

use std::env;

use gtk4::gio;
use gtk4::prelude::*;

use crate::config::Config;

const MEDIA_KEYS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const CUSTOM_KEYBINDING_SCHEMA: &str =
    "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
/// Dedicated, app-specific relay paths so we never collide with the user's
/// own custom shortcuts.
const RELAY_PATH: &str =
    "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/elementary-intelligence/";
const SCREENSHOT_RELAY_PATH: &str =
    "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/elementary-intelligence-screenshot/";

/// Absolute command the compositor runs when a hotkey fires.
fn command(args: &str) -> String {
    let exe = env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "elementary-intelligence".to_string());
    format!("{exe} {args}")
}

/// Register (or update) both system-wide shortcuts from the config. When
/// disabled the bindings are cleared so the entries stay but do nothing —
/// re-enabling simply restores the accelerators.
pub fn apply(config: &Config) {
    write_relay(
        RELAY_PATH,
        "Elementary Intelligence",
        &command("--spotlight"),
        if config.enabled { &config.shortcut } else { "" },
    );
    write_relay(
        SCREENSHOT_RELAY_PATH,
        "Elementary Intelligence (Screenshot)",
        &command("--spotlight --screenshot"),
        if config.enabled { &config.screenshot_shortcut } else { "" },
    );

    // Make sure our relay paths are listed in the media-keys custom bindings.
    let media_keys = gio::Settings::new(MEDIA_KEYS_SCHEMA);
    let mut paths: Vec<String> = media_keys
        .strv("custom-keybindings")
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut changed = false;
    for relay_path in [RELAY_PATH, SCREENSHOT_RELAY_PATH] {
        if !paths.iter().any(|p| p == relay_path) {
            paths.push(relay_path.to_string());
            changed = true;
        }
    }
    if changed {
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let _ = media_keys.set_strv("custom-keybindings", refs);
    }
    gio::Settings::sync();
}

fn write_relay(path: &str, name: &str, command: &str, binding: &str) {
    let relay = gio::Settings::with_path(CUSTOM_KEYBINDING_SCHEMA, path);
    let _ = relay.set_string("name", name);
    let _ = relay.set_string("command", command);
    let _ = relay.set_string("binding", binding);
}
