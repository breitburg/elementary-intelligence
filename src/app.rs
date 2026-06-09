// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! Application wiring: single-instance lifecycle, the background hold, the
//! `--spotlight` command line and the global stylesheet.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk::Display;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, CssProvider, Settings};

use crate::config::Config;
use crate::keybinding;
use crate::screenshot;
use crate::settings_window;
use crate::spotlight;
use crate::APP_ID;

const STYLE: &str = include_str!("../data/style.css");

pub fn build() -> Application {
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    // Shared, mutable config for the lifetime of the process.
    let config = Rc::new(RefCell::new(Config::load()));

    app.connect_startup(glib::clone!(
        #[strong]
        config,
        move |app| {
            load_css();
            follow_color_scheme();

            // Stay resident so the hotkey can reach us with no window open.
            std::mem::forget(app.hold());

            // Make sure the system-wide shortcuts reflect the saved state.
            keybinding::apply(&config.borrow());

            let quit = gio::SimpleAction::new("quit", None);
            quit.connect_activate(glib::clone!(
                #[weak]
                app,
                move |_, _| app.quit()
            ));
            app.add_action(&quit);
        }
    ));

    app.connect_command_line(glib::clone!(
        #[strong]
        config,
        move |app, cmdline| {
            let has_flag = |flag: &str| {
                cmdline
                    .arguments()
                    .iter()
                    .any(|arg| arg.to_string_lossy() == flag)
            };

            if has_flag("--spotlight") {
                toggle_spotlight(app, &config, has_flag("--screenshot"));
            } else if cmdline.is_remote() {
                // The user launched the app again while it was already running
                // (e.g. clicked the icon) — open settings. The initial
                // background launch shows nothing.
                settings_window::present(app, &config);
            }
            0
        }
    ));

    app
}

thread_local! {
    /// A present is scheduled but its settle delay hasn't elapsed yet.
    static PRESENT_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Show the entry, or dismiss it if it is already open (toggle).
fn toggle_spotlight(app: &Application, config: &Rc<RefCell<Config>>, with_screenshot: bool) {
    if !config.borrow().enabled {
        return;
    }
    if let Some(window) = app
        .windows()
        .into_iter()
        .find(|w| w.widget_name() == "spotlight" && w.is_visible())
    {
        window.close();
        return;
    }
    if PRESENT_PENDING.with(|p| p.get()) {
        return;
    }
    PRESENT_PENDING.with(|p| p.set(true));

    // Capture strictly before the window exists so it isn't in the shot.
    let shot = with_screenshot
        .then(|| {
            screenshot::capture()
                .map_err(|err| eprintln!("Could not capture screenshot: {err}"))
                .ok()
        })
        .flatten();

    // Presenting in the same instant the hotkey fires gets the window's focus
    // bounced by the compositor's keybinding handling, and the click-away
    // close then dismisses it. A short settle delay sidesteps the bounce —
    // the screenshot path always worked only because the blocking capture
    // provided that delay implicitly.
    let app = app.clone();
    let config = config.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
        PRESENT_PENDING.with(|p| p.set(false));
        spotlight::present(&app, &config, shot);
    });
}

/// Follow the desktop light/dark preference, mirroring it exactly: dark only
/// for "prefer-dark", light otherwise. elementary reports plain "default" (not
/// "prefer-light") when leaving dark, so anything but "prefer-dark" must reset
/// to light — otherwise the app stays stuck dark.
fn follow_color_scheme() {
    let Some(settings) = Settings::default() else {
        return;
    };
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return;
    };
    if source.lookup("org.gnome.desktop.interface", true).is_none() {
        return;
    }

    let interface = gio::Settings::new("org.gnome.desktop.interface");
    apply_color_scheme(&interface, &settings);
    interface.connect_changed(
        Some("color-scheme"),
        glib::clone!(
            #[weak]
            settings,
            move |interface, _| apply_color_scheme(interface, &settings)
        ),
    );
    // Keep the subscription alive for the lifetime of the process.
    std::mem::forget(interface);
}

fn apply_color_scheme(interface: &gio::Settings, settings: &Settings) {
    let dark = interface.string("color-scheme") == "prefer-dark";
    settings.set_property("gtk-application-prefer-dark-theme", dark);
}

fn load_css() {
    let css = STYLE.replace("{corner_radius}", &spotlight::CORNER_RADIUS.to_string());
    let provider = CssProvider::new();
    provider.load_from_data(&css);
    if let Some(display) = Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
