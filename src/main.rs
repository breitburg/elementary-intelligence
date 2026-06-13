// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

mod api;
mod app;
mod autostart;
mod blur;
mod config;
mod keybinding;
mod markdown;
mod screenshot;
mod settings_window;
mod spotlight;
mod tools;

use gtk4::prelude::*;

/// Application id, also used as the config directory and icon name.
pub const APP_ID: &str = "com.github.breitburg.beckon";

fn main() -> gtk4::glib::ExitCode {
    app::build().run()
}
