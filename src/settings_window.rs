// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! The persistent settings window: the API endpoint, key and model, the two
//! trigger shortcuts and whether to launch on login.
//!
//! Laid out as a native elementary settings form — right-aligned labels in the
//! left column, controls in the right column of a single grid.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, DropDown, Entry,
    EventControllerKey, Grid, HeaderBar, Label, MenuButton, Orientation, PasswordEntry,
    StringList, StringObject, Switch, Widget,
};

use crate::api;
use crate::autostart;
use crate::config::Config;
use crate::keybinding;
use crate::tools;

/// Which shortcut a capture button is currently recording for.
#[derive(Clone, Copy, PartialEq)]
enum ShortcutTarget {
    Open,
    Screenshot,
}

/// Show the settings window, creating it if it does not exist yet.
pub fn present(app: &Application, config: &Rc<RefCell<Config>>) {
    // Reuse an existing settings window if one is already open.
    if let Some(window) = app
        .windows()
        .into_iter()
        .find(|w| w.widget_name() == "settings")
    {
        window.present();
        return;
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Beckon")
        .resizable(false)
        .default_width(380)
        .build();
    window.set_widget_name("settings");

    // Flat, backgroundless header that blends into the window: window controls
    // and the menu only, no title text.
    let header = HeaderBar::new();
    header.add_css_class("flat");
    header.set_title_widget(Some(&Label::new(None)));
    let menu_model = gio::Menu::new();
    menu_model.append(Some("Quit Beckon"), Some("app.quit"));
    let menu_button = MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu_model)
        .build();
    header.pack_end(&menu_button);
    window.set_titlebar(Some(&header));

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .build();

    // --- Heading: bold title + a purple enable toggle ----------------------
    let heading = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(16)
        .build();
    // Full-width so the gradient spans the window; vertical padding lives in CSS
    // so the gradient fills the whole section.
    heading.add_css_class("app-heading");

    let title = Label::new(Some("Beckon"));
    title.add_css_class("app-title");
    title.set_halign(Align::Center);
    heading.append(&title);

    let enable_switch = Switch::builder()
        .active(config.borrow().enabled)
        .halign(Align::Center)
        .build();
    enable_switch.add_css_class("brand");
    heading.append(&enable_switch);
    content.append(&heading);

    let grid = Grid::builder()
        .row_spacing(12)
        .column_spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    // --- API endpoint, key and model ----------------------------------------
    let url_entry = Entry::builder()
        .text(&config.borrow().api_base_url)
        .placeholder_text("https://api.openai.com/v1")
        .hexpand(true)
        .build();
    add_row(&grid, 0, "API URL", &url_entry);

    let key_entry = PasswordEntry::builder()
        .show_peek_icon(true)
        .hexpand(true)
        .build();
    key_entry.set_text(&config.borrow().api_key);
    add_row(&grid, 1, "API Key", &key_entry);

    // The model picker lists whatever the endpoint's /models reports. Until a
    // fetch succeeds (or when it fails) it holds just the configured model.
    let model_list = StringList::new(&[]);
    if !config.borrow().model.is_empty() {
        model_list.append(&config.borrow().model);
    }
    let model_dropdown = DropDown::builder()
        .model(&model_list)
        .enable_search(true)
        .hexpand(true)
        .build();
    // Search only filters if the dropdown knows how to turn each item into a
    // string to match against — point it at the StringObject's `string`.
    model_dropdown.set_expression(Some(gtk4::PropertyExpression::new(
        StringObject::static_type(),
        gtk4::Expression::NONE,
        "string",
    )));
    add_row(&grid, 2, "Model", &model_dropdown);

    // Repopulating the list fires selection notifications; ignore them.
    let repopulating = Rc::new(Cell::new(false));
    {
        let config = config.clone();
        let repopulating = repopulating.clone();
        model_dropdown.connect_selected_item_notify(move |dropdown| {
            if repopulating.get() {
                return;
            }
            let Some(item) = dropdown.selected_item().and_downcast::<StringObject>() else {
                return;
            };
            let mut config = config.borrow_mut();
            config.model = item.string().to_string();
            config.save();
        });
    }

    let refresh_models = {
        let config = config.clone();
        let model_list = model_list.clone();
        let model_dropdown = model_dropdown.clone();
        let repopulating = repopulating.clone();
        Rc::new(move || {
            let (base_url, api_key, current) = {
                let config = config.borrow();
                (config.api_base_url.clone(), config.api_key.clone(), config.model.clone())
            };
            if base_url.is_empty() {
                return;
            }
            let (sender, receiver) = async_channel::bounded::<Result<Vec<String>, String>>(1);
            api::list_models(
                api::ApiConfig { base_url, api_key, model: String::new() },
                sender,
            );
            let model_list = model_list.clone();
            let model_dropdown = model_dropdown.clone();
            let repopulating = repopulating.clone();
            glib::spawn_future_local(async move {
                let Ok(Ok(mut models)) = receiver.recv().await else {
                    return; // fetch failed: keep whatever the list holds
                };
                if models.is_empty() {
                    return;
                }
                // The configured model stays available even if unlisted.
                if !current.is_empty() && !models.contains(&current) {
                    models.insert(0, current.clone());
                }
                repopulating.set(true);
                model_list.splice(
                    0,
                    model_list.n_items(),
                    &models.iter().map(String::as_str).collect::<Vec<_>>(),
                );
                if let Some(index) = models.iter().position(|m| *m == current) {
                    model_dropdown.set_selected(index as u32);
                }
                repopulating.set(false);
            });
        })
    };
    refresh_models();

    // Saving on every keystroke is fine for a TOML write, but refetching the
    // model list is debounced until typing pauses.
    let refetch_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let schedule_refresh = {
        let refresh_models = refresh_models.clone();
        let refetch_timer = refetch_timer.clone();
        Rc::new(move || {
            if let Some(source) = refetch_timer.borrow_mut().take() {
                source.remove();
            }
            let refresh_models = refresh_models.clone();
            let timer = refetch_timer.clone();
            let source = glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                timer.borrow_mut().take();
                refresh_models();
            });
            refetch_timer.borrow_mut().replace(source);
        })
    };

    {
        let config = config.clone();
        let schedule_refresh = schedule_refresh.clone();
        url_entry.connect_changed(move |entry| {
            {
                let mut config = config.borrow_mut();
                config.api_base_url = entry.text().trim().to_string();
                config.save();
            }
            schedule_refresh();
        });
    }
    {
        let config = config.clone();
        let schedule_refresh = schedule_refresh.clone();
        key_entry.connect_changed(move |entry| {
            {
                let mut config = config.borrow_mut();
                config.api_key = entry.text().trim().to_string();
                config.save();
            }
            schedule_refresh();
        });
    }

    // --- System prompt ------------------------------------------------------
    let system_view = gtk4::TextView::builder()
        .wrap_mode(gtk4::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .build();
    system_view.buffer().set_text(&config.borrow().system_prompt);
    let system_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_height(72)
        .max_content_height(160)
        .has_frame(true)
        .hexpand(true)
        .child(&system_view)
        .build();
    system_scroll.add_css_class("system-prompt");
    add_top_row(&grid, 3, "System prompt", &system_scroll);

    {
        let config = config.clone();
        system_view.buffer().connect_changed(move |buffer| {
            let (start, end) = buffer.bounds();
            let text = buffer.text(&start, &end, false);
            let mut config = config.borrow_mut();
            config.system_prompt = text.to_string();
            config.save();
        });
    }

    // --- Shortcuts -----------------------------------------------------------
    let shortcut_button = Button::with_label(&accel_to_label(&config.borrow().shortcut));
    shortcut_button.set_hexpand(true);
    shortcut_button.set_tooltip_text(Some("Click, then press the new combination"));
    add_row(&grid, 4, "Shortcut", &shortcut_button);

    let screenshot_button =
        Button::with_label(&accel_to_label(&config.borrow().screenshot_shortcut));
    screenshot_button.set_hexpand(true);
    screenshot_button.set_tooltip_text(Some(
        "Opens the prompt with a screenshot of your screen attached",
    ));
    add_row(&grid, 5, "Screenshot shortcut", &screenshot_button);

    // One shared capture state: only one button records at a time, and the
    // single window-level key controller writes to whichever field is armed.
    let capturing: Rc<Cell<Option<ShortcutTarget>>> = Rc::new(Cell::new(None));
    arm_capture(&shortcut_button, &screenshot_button, &capturing, ShortcutTarget::Open, config);
    arm_capture(&screenshot_button, &shortcut_button, &capturing, ShortcutTarget::Screenshot, config);

    let key_controller = EventControllerKey::new();
    {
        let capturing = capturing.clone();
        let config = config.clone();
        let shortcut_button = shortcut_button.clone();
        let screenshot_button = screenshot_button.clone();
        key_controller.connect_key_pressed(move |_, key, _, state| {
            let Some(target) = capturing.get() else {
                return glib::Propagation::Proceed;
            };
            let button = match target {
                ShortcutTarget::Open => &shortcut_button,
                ShortcutTarget::Screenshot => &screenshot_button,
            };
            let current = |config: &Config| match target {
                ShortcutTarget::Open => config.shortcut.clone(),
                ShortcutTarget::Screenshot => config.screenshot_shortcut.clone(),
            };
            let reset = |button: &Button, label: &str| {
                capturing.set(None);
                button.remove_css_class("suggested-action");
                button.set_label(label);
            };
            if key == gdk::Key::Escape {
                reset(button, &accel_to_label(&current(&config.borrow())));
                return glib::Propagation::Stop;
            }
            if is_modifier_key(key) {
                return glib::Propagation::Stop; // wait for the real key
            }
            let mods = state & gtk4::accelerator_get_default_mod_mask();
            if !gtk4::accelerator_valid(key, mods) {
                return glib::Propagation::Stop;
            }
            let accel = gtk4::accelerator_name(key, mods).to_string();
            reset(button, &accel_to_label(&accel));

            let mut config = config.borrow_mut();
            match target {
                ShortcutTarget::Open => config.shortcut = accel,
                ShortcutTarget::Screenshot => config.screenshot_shortcut = accel,
            }
            config.save();
            keybinding::apply(&config);
            glib::Propagation::Stop
        });
    }
    window.add_controller(key_controller);

    // --- Tools --------------------------------------------------------------
    // One checkbox per available tool; ticking adds its name to the enabled
    // list the spotlight passes to the model.
    let tools_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    for info in tools::catalog() {
        let check = CheckButton::builder()
            .label(info.label)
            .active(config.borrow().enabled_tools.iter().any(|t| t == info.name))
            .tooltip_text(info.description)
            .build();
        {
            let config = config.clone();
            let name = info.name;
            check.connect_toggled(move |check| {
                let mut config = config.borrow_mut();
                config.enabled_tools.retain(|t| t != name);
                if check.is_active() {
                    config.enabled_tools.push(name.to_string());
                }
                config.save();
            });
        }
        tools_box.append(&check);
    }
    add_top_row(&grid, 6, "Tools", &tools_box);

    // --- Start on login ----------------------------------------------------
    let login_switch = Switch::builder()
        .active(config.borrow().start_on_login)
        .halign(Align::Start)
        .valign(Align::Center)
        .build();
    add_row(&grid, 7, "Start on login", &login_switch);

    {
        let config = config.clone();
        login_switch.connect_active_notify(move |switch| {
            let enabled = switch.is_active();
            let mut config = config.borrow_mut();
            config.start_on_login = enabled;
            config.save();
            autostart::set_enabled(enabled);
        });
    }

    // The form follows the enable state.
    grid.set_sensitive(config.borrow().enabled);
    {
        let config = config.clone();
        let grid = grid.clone();
        enable_switch.connect_active_notify(move |switch| {
            let enabled = switch.is_active();
            let mut config = config.borrow_mut();
            config.enabled = enabled;
            config.save();
            keybinding::apply(&config);
            grid.set_sensitive(enabled);
        });
    }

    content.append(&grid);
    window.set_child(Some(&content));
    window.present();
}

/// Wire a shortcut button to arm capture for `target`, restoring the other
/// button's label if it was mid-capture.
fn arm_capture(
    button: &Button,
    other: &Button,
    capturing: &Rc<Cell<Option<ShortcutTarget>>>,
    target: ShortcutTarget,
    config: &Rc<RefCell<Config>>,
) {
    let capturing = capturing.clone();
    let other = other.clone();
    let config = config.clone();
    button.connect_clicked(move |button| {
        if let Some(previous) = capturing.get() {
            if previous != target {
                let config = config.borrow();
                let accel = match previous {
                    ShortcutTarget::Open => &config.shortcut,
                    ShortcutTarget::Screenshot => &config.screenshot_shortcut,
                };
                other.remove_css_class("suggested-action");
                other.set_label(&accel_to_label(accel));
            }
        }
        capturing.set(Some(target));
        button.set_label("Press keys…");
        button.add_css_class("suggested-action");
    });
}

/// Attach a labelled control as one form row: right-aligned label in column 0,
/// control in column 1.
fn add_row(grid: &Grid, row: i32, label: &str, control: &impl IsA<Widget>) {
    let label = Label::builder()
        .label(label)
        .halign(Align::End)
        .valign(Align::Center)
        .build();
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(control, 1, row, 1, 1);
}

/// Like `add_row`, but pins the label to the top of a tall control (e.g. the
/// multi-line system-prompt box) instead of centring it.
fn add_top_row(grid: &Grid, row: i32, label: &str, control: &impl IsA<Widget>) {
    let label = Label::builder()
        .label(label)
        .halign(Align::End)
        .valign(Align::Start)
        .margin_top(6)
        .build();
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(control, 1, row, 1, 1);
}

/// Render a stored GTK accelerator (e.g. `<Control><Shift>space`) as a
/// human-readable label (e.g. `Ctrl+Shift+Space`).
fn accel_to_label(accelerator: &str) -> String {
    match gtk4::accelerator_parse(accelerator) {
        Some((key, mods)) if key != gdk::Key::VoidSymbol => {
            gtk4::accelerator_get_label(key, mods).to_string()
        }
        _ => accelerator.to_string(),
    }
}

fn is_modifier_key(key: gdk::Key) -> bool {
    matches!(
        key,
        gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R
            | gdk::Key::Super_L
            | gdk::Key::Super_R
            | gdk::Key::Meta_L
            | gdk::Key::Meta_R
            | gdk::Key::Hyper_L
            | gdk::Key::Hyper_R
            | gdk::Key::ISO_Level3_Shift
            | gdk::Key::Caps_Lock
            | gdk::Key::Num_Lock
    )
}
