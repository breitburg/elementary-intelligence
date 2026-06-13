// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! The Spotlight-style entry: a compact, borderless prompt that expands into
//! an in-place chat once the first message is sent. Conversation state lives
//! in this window's closures, so dismissing it ends the conversation.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gdk;
use gtk4::glib;
use gtk4::pango::WrapMode;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, Box as GtkBox, Button, ContentFit, Entry, EventControllerKey, Image, Label,
    Orientation, Picture, PolicyType, Revealer, RevealerTransitionType, ScrolledWindow, Separator,
    Window,
};

use serde_json::{json, Value};

use crate::api::{self, ChatEvent};
use crate::blur;
use crate::config::Config;
use crate::markdown;
use crate::settings_window;

/// Corner radius of the card, in pixels. Single source of truth: it both
/// clips the compositor blur region (below) and is substituted into the
/// stylesheet's `border-radius` at load time (see `app::load_css`), so the
/// frosted-glass blur and the drawn card always share the same corners.
pub const CORNER_RADIUS: u32 = 8;

/// Fixed width of the card, in pixels. The window never changes width — only
/// its height grows as the chat reveals — so the compositor keeps it centered
/// without horizontal drift.
const WINDOW_WIDTH: i32 = 620;

/// Trim surrounding whitespace and collapse any run of three or more newlines
/// down to a blank-line gap, recursively. Applied to both the user's message
/// and the streaming reply before display.
fn clean(text: &str) -> String {
    let mut text = text.trim().to_string();
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text
}

/// Longest edge, in pixels, of an attached image as shown inline in the chat.
/// The full-resolution data URL still rides along to the model; only the
/// on-screen thumbnail is bounded.
const ATTACHMENT_MAX_EDGE: f64 = 220.0;

/// Decode a `data:…;base64,…` URL into a GPU texture, or `None` if it isn't a
/// base64 data URL or the bytes don't parse as an image. Used to render the
/// images carried in a message's content parts back into the chat.
fn texture_from_data_url(data_url: &str) -> Option<gdk::Texture> {
    let base64 = data_url.split_once(";base64,").map(|(_, data)| data)?;
    let bytes = glib::Bytes::from_owned(glib::base64_decode(base64));
    gdk::Texture::from_bytes(&bytes).ok()
}

/// A bounded, left-aligned thumbnail for an inline image attachment. The aspect
/// ratio is preserved and the longest edge is capped at `ATTACHMENT_MAX_EDGE`.
fn attachment_thumbnail(texture: &gdk::Texture) -> Picture {
    let picture = Picture::for_paintable(texture);
    picture.set_halign(Align::Start);
    picture.set_content_fit(ContentFit::Contain);
    picture.set_can_shrink(true);
    let (width, height) = (texture.width() as f64, texture.height() as f64);
    let scale = (ATTACHMENT_MAX_EDGE / width.max(height)).min(1.0);
    picture.set_size_request((width * scale).round() as i32, (height * scale).round() as i32);
    picture.add_css_class("user-attachment");
    picture
}

/// Duration over which a freshly arrived chunk fades from faint to opaque.
const FADE: std::time::Duration = std::time::Duration::from_millis(250);

/// Streaming render state. Text that has finished fading is kept in `settled`
/// and rendered with full markdown; chunks still within the fade window trail
/// it as plain, alpha-ramped spans so new text fades in as it arrives.
#[derive(Default)]
struct FadeState {
    settled: String,
    /// Cached `markdown::to_pango(&settled)`, recomputed only when `settled`
    /// grows — the fade ticker renders every frame and must not re-parse the
    /// whole body each time.
    settled_markup: String,
    pending: Vec<(String, std::time::Instant)>,
}

impl FadeState {
    fn push(&mut self, chunk: String, now: std::time::Instant) {
        self.pending.push((chunk, now));
    }

    /// Move chunks whose fade has completed into the settled body. Arrivals are
    /// monotonic, so expired chunks are always at the front.
    fn settle(&mut self, now: std::time::Instant) {
        let mut grew = false;
        while self
            .pending
            .first()
            .is_some_and(|(_, arrival)| now.duration_since(*arrival) >= FADE)
        {
            let (text, _) = self.pending.remove(0);
            self.settled.push_str(&text);
            grew = true;
        }
        if grew {
            self.settled_markup = markdown::to_pango(&self.settled);
        }
    }

    /// Markdown for the settled body, followed by each still-fading chunk in a
    /// span whose alpha reflects how far through the fade it is.
    fn to_markup(&self, now: std::time::Instant) -> String {
        let mut markup = self.settled_markup.clone();
        for (text, arrival) in &self.pending {
            let progress = now.duration_since(*arrival).as_secs_f64() / FADE.as_secs_f64();
            let percent = (progress.clamp(0.0, 1.0) * 100.0).max(1.0) as u32;
            markup.push_str(&format!(
                "<span alpha=\"{percent}%\">{}</span>",
                glib::markup_escape_text(text)
            ));
        }
        markup
    }
}

/// Build, wire up and present the entry window. `screenshot` is an OpenAI
/// `image_url` data URL attached to the first message. Returns the window so
/// the caller can track and toggle it.
pub fn present(app: &Application, config: &Rc<RefCell<Config>>, screenshot: Option<String>) -> Window {
    let window = Window::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .default_width(WINDOW_WIDTH)
        .build();
    window.set_widget_name("spotlight");
    window.add_css_class("spotlight");

    // The card carries the rounded background and shadow; the surrounding
    // window stays transparent so the corners read as rounded and the shadow
    // has a gutter to render into.
    let card = GtkBox::builder().orientation(Orientation::Vertical).build();
    card.add_css_class("spotlight-card");
    // Fix the card's width so the window never reflows horizontally as replies
    // stream in; long lines wrap instead. The 32px CSS margin sits outside this
    // request, so the toplevel ends up exactly WINDOW_WIDTH wide.
    card.set_size_request(WINDOW_WIDTH - 64, -1);

    let entry_row = GtkBox::builder().orientation(Orientation::Horizontal).build();
    entry_row.add_css_class("entry-row");

    // The search icon is a standalone widget (not the Entry's built-in primary
    // icon) so the attachment chip can sit between it and the text field.
    let search_icon = Image::from_icon_name("edit-find-symbolic");
    search_icon.add_css_class("search-icon");
    search_icon.set_valign(Align::Center);
    entry_row.append(&search_icon);

    let entry = Entry::builder()
        .placeholder_text("Ask anything…")
        .has_frame(false)
        .hexpand(true)
        .build();
    entry.add_css_class("spotlight-entry");
    entry_row.append(&entry);

    // Attachment chip at the trailing edge, only while a screenshot is pending
    // for the first send.
    let chip = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .valign(Align::Center)
        .visible(screenshot.is_some())
        .build();
    chip.add_css_class("attachment-chip");
    chip.append(&Image::from_icon_name("image-x-generic-symbolic"));
    chip.append(&Label::new(Some("Screenshot")));
    entry_row.append(&chip);

    // Settings shortcut at the trailing edge of the field.
    let settings_button = Button::from_icon_name("applications-system-symbolic");
    settings_button.add_css_class("flat");
    settings_button.set_valign(Align::Center);
    settings_button.set_tooltip_text(Some("Settings"));
    entry_row.append(&settings_button);

    card.append(&entry_row);

    // The conversation slides open below the entry; the window only ever
    // grows downward, so the pill's top edge stays put.
    let messages = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    messages.add_css_class("messages");

    let scrolled = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .child(&messages)
        .build();

    // A rule separates the field from the conversation; it lives inside the
    // revealer so it slides in with the chat and leaves the collapsed pill clean.
    let chat_area = GtkBox::builder().orientation(Orientation::Vertical).build();
    let separator = Separator::new(Orientation::Horizontal);
    separator.add_css_class("field-separator");
    chat_area.append(&separator);
    chat_area.append(&scrolled);

    let revealer = Revealer::builder()
        .transition_type(RevealerTransitionType::SlideDown)
        .transition_duration(250)
        .reveal_child(false)
        .child(&chat_area)
        .build();
    revealer.add_css_class("chat-revealer");
    card.append(&revealer);

    window.set_child(Some(&card));

    // Conversation state, dropped with the window: fresh chat per invocation.
    let history: Rc<RefCell<Vec<Value>>> = Rc::new(RefCell::new(Vec::new()));
    let streaming = Rc::new(Cell::new(false));
    let screenshot = Rc::new(RefCell::new(screenshot));
    // Handle to the in-flight reply task, so Escape-to-clear can abort it
    // (dropping the channel receiver, which stops the worker thread).
    let active_stream: Rc<RefCell<Option<glib::JoinHandle<()>>>> = Rc::new(RefCell::new(None));
    // The first user message is hidden while the chat is a single exchange; it
    // is revealed once a second turn arrives and the history becomes worth
    // scrolling back through. Holds the whole message row (text plus any
    // attachment thumbnails), so the image hides and reveals with the text.
    let first_user_message: Rc<RefCell<Option<GtkBox>>> = Rc::new(RefCell::new(None));

    // Stick to the bottom while the reply streams in, but let the user scroll
    // up and stay there. The label only resizes after `set_markup` returns, so
    // scrolling happens on the adjustment's own change notifications.
    let stick_to_bottom = Rc::new(Cell::new(true));
    let adjustment = scrolled.vadjustment();
    {
        let stick = stick_to_bottom.clone();
        adjustment.connect_value_changed(move |adj| {
            stick.set(adj.value() + adj.page_size() >= adj.upper() - 1.0);
        });
    }
    {
        let stick = stick_to_bottom.clone();
        adjustment.connect_changed(move |adj| {
            if stick.get() {
                adj.set_value(adj.upper() - adj.page_size());
            }
        });
    }

    {
        let app = app.clone();
        let config = config.clone();
        let window = window.clone();
        settings_button.connect_clicked(move |_| {
            settings_window::present(&app, &config);
            window.close();
        });
    }

    // Enter → append the message and stream the reply into the card.
    {
        let config = config.clone();
        let chip = chip.clone();
        let revealer = revealer.clone();
        let messages = messages.clone();
        let history = history.clone();
        let streaming = streaming.clone();
        let screenshot = screenshot.clone();
        let first_user_message = first_user_message.clone();
        let active_stream = active_stream.clone();
        entry.connect_activate(move |entry| {
            let text = entry.text();
            let message = clean(&text);
            if message.is_empty() || streaming.get() {
                return;
            }

            // Any attachments pending in the field ride along on this message
            // as Responses API content parts; the field's chip clears once
            // they've been consumed. Currently sourced from a screenshot, but
            // the rendering below treats them generically.
            let attachments: Vec<String> = screenshot.borrow_mut().take().into_iter().collect();
            if !attachments.is_empty() {
                chip.set_visible(false);
            }

            // A plain string for text-only turns; a content-part array once
            // there's at least one image to carry alongside the text.
            let content = if attachments.is_empty() {
                json!(message)
            } else {
                let mut parts = vec![json!({"type": "input_text", "text": message})];
                for data_url in &attachments {
                    parts.push(json!({"type": "input_image", "image_url": data_url}));
                }
                Value::Array(parts)
            };
            let is_first_turn = history.borrow().is_empty();
            history.borrow_mut().push(json!({"role": "user", "content": content}));
            entry.set_text("");
            // Past the first turn, the field invites a follow-up.
            entry.set_placeholder_text(Some("Follow up…"));

            // The user's turn is a column: attachment thumbnails stacked above
            // the text, so an image shows in the chat the same way it was sent.
            let user_message = GtkBox::builder()
                .orientation(Orientation::Vertical)
                .spacing(8)
                .halign(Align::Start)
                .build();
            for data_url in &attachments {
                if let Some(texture) = texture_from_data_url(data_url) {
                    user_message.append(&attachment_thumbnail(&texture));
                }
            }
            let user_label = Label::builder()
                .label(&message)
                .halign(Align::Start)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(WrapMode::WordChar)
                .max_width_chars(40)
                .selectable(true)
                .build();
            user_label.add_css_class("user-message");
            user_message.append(&user_label);
            if is_first_turn {
                // A plain opening turn stays hidden until a follow-up arrives, so
                // a single exchange shows just the answer. A turn carrying an
                // attachment is always shown — the user added the image to see it
                // land in the chat.
                if attachments.is_empty() {
                    user_message.set_visible(false);
                    *first_user_message.borrow_mut() = Some(user_message.clone());
                }
            } else {
                // Extra breathing room above each follow-up question, setting it
                // apart from the previous reply.
                user_message.set_margin_top(12);
                if let Some(first) = first_user_message.borrow_mut().take() {
                    // Second turn: the conversation now has history worth showing.
                    first.set_visible(true);
                }
            }
            messages.append(&user_message);

            // First send: reveal the chat below the entry. The blur region
            // from map is kept as-is — Gala forbids a second get_panel on the
            // same surface (fatal protocol error) — and its inset-based region
            // already follows the growing window.
            if !revealer.reveals_child() {
                revealer.set_reveal_child(true);
            }

            let reply_label = Label::builder()
                .halign(Align::Fill)
                .hexpand(true)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(WrapMode::WordChar)
                .selectable(true)
                .use_markup(true)
                .build();
            reply_label.add_css_class("assistant-message");

            // A spinner stands in for the reply until the first token lands;
            // the reply label is only added to the tree once there's text, so
            // nothing but the spinner shows while waiting.
            let spinner = gtk4::Spinner::builder().halign(Align::Start).build();
            spinner.add_css_class("reply-spinner");
            spinner.start();
            messages.append(&spinner);

            streaming.set(true);
            let (api_config, system_prompt) = {
                let config = config.borrow();
                (
                    api::ApiConfig {
                        base_url: config.api_base_url.clone(),
                        api_key: config.api_key.clone(),
                        model: config.model.clone(),
                    },
                    config.system_prompt.trim().to_string(),
                )
            };
            // Prepend the configured system prompt to the turn, if any.
            let mut payload = Vec::new();
            if !system_prompt.is_empty() {
                payload.push(json!({"role": "system", "content": system_prompt}));
            }
            payload.extend(history.borrow().iter().cloned());
            let (sender, receiver) = async_channel::unbounded::<ChatEvent>();
            api::stream_chat(api_config, payload, sender);

            let history = history.clone();
            let streaming = streaming.clone();
            let messages = messages.clone();
            let handle = glib::spawn_future_local(async move {
                // Remove the spinner if it's still attached.
                let remove_spinner = {
                    let messages = messages.clone();
                    let spinner = spinner.clone();
                    move || {
                        if spinner.parent().is_some() {
                            messages.remove(&spinner);
                        }
                    }
                };
                // Swap the spinner out for the reply label the first time there
                // is something to show (a token or an error). Idempotent.
                let present_reply = {
                    let messages = messages.clone();
                    let reply_label = reply_label.clone();
                    let remove_spinner = remove_spinner.clone();
                    move || {
                        remove_spinner();
                        if reply_label.parent().is_none() {
                            messages.append(&reply_label);
                        }
                    }
                };
                let fade = Rc::new(RefCell::new(FadeState::default()));
                let finished = Rc::new(Cell::new(false));
                let ticking = Rc::new(Cell::new(false));

                let render = {
                    let reply_label = reply_label.clone();
                    let fade = fade.clone();
                    move || reply_label.set_markup(&fade.borrow().to_markup(std::time::Instant::now()))
                };

                // While chunks are fading, keep re-rendering on a frame timer so
                // their alpha ramps even when the stream pauses; the timer stops
                // itself once everything has settled (or the stream finishes).
                let start_ticker = {
                    let fade = fade.clone();
                    let ticking = ticking.clone();
                    let finished = finished.clone();
                    let render = render.clone();
                    move || {
                        if ticking.get() {
                            return;
                        }
                        ticking.set(true);
                        let fade = fade.clone();
                        let ticking = ticking.clone();
                        let finished = finished.clone();
                        let render = render.clone();
                        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
                            if finished.get() {
                                ticking.set(false);
                                return glib::ControlFlow::Break;
                            }
                            fade.borrow_mut().settle(std::time::Instant::now());
                            render();
                            if fade.borrow().pending.is_empty() {
                                ticking.set(false);
                                glib::ControlFlow::Break
                            } else {
                                glib::ControlFlow::Continue
                            }
                        });
                    }
                };

                let mut accumulated = String::new();
                let mut errored = false;
                while let Ok(event) = receiver.recv().await {
                    match event {
                        ChatEvent::Delta(delta) => {
                            accumulated.push_str(&delta);
                            present_reply();
                            fade.borrow_mut().push(delta, std::time::Instant::now());
                            render();
                            start_ticker();
                        }
                        ChatEvent::Done => break,
                        ChatEvent::Error(message) => {
                            errored = true;
                            present_reply();
                            let error_line =
                                format!("<i>{}</i>", glib::markup_escape_text(&message));
                            if accumulated.is_empty() {
                                reply_label.add_css_class("error");
                                reply_label.set_markup(&error_line);
                            } else {
                                // Keep the partial answer visible; the error
                                // class would tint all of it, so skip it here.
                                reply_label.set_markup(&format!(
                                    "{}\n\n{error_line}",
                                    markdown::to_pango(&clean(&accumulated))
                                ));
                            }
                            break;
                        }
                    }
                }
                // Stop any in-flight fade. If the stream ended without content,
                // just drop the spinner — no empty reply label is added.
                finished.set(true);
                remove_spinner();
                if !errored && !accumulated.is_empty() {
                    // Snap to the final, fully opaque markdown render.
                    reply_label.set_markup(&markdown::to_pango(&clean(&accumulated)));
                    history
                        .borrow_mut()
                        .push(json!({"role": "assistant", "content": accumulated}));
                }
                streaming.set(false);
            });
            *active_stream.borrow_mut() = Some(handle);
        });
    }

    // Esc → clear the conversation if there is one, otherwise dismiss. The first
    // press resets to an empty prompt; a second (now-empty) press closes.
    let key_controller = EventControllerKey::new();
    {
        let window = window.clone();
        let history = history.clone();
        let messages = messages.clone();
        let revealer = revealer.clone();
        let entry_for_key = entry.clone();
        let first_user_message = first_user_message.clone();
        let streaming = streaming.clone();
        let active_stream = active_stream.clone();
        key_controller.connect_key_pressed(move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            let has_chat = revealer.reveals_child() || !history.borrow().is_empty();
            if !has_chat {
                window.close();
                return glib::Propagation::Stop;
            }
            // Clear: abort any in-flight reply, drop history and message widgets,
            // collapse the chat, and restore the initial prompt.
            if let Some(handle) = active_stream.borrow_mut().take() {
                handle.abort();
            }
            streaming.set(false);
            history.borrow_mut().clear();
            first_user_message.borrow_mut().take();
            while let Some(child) = messages.first_child() {
                messages.remove(&child);
            }
            revealer.set_reveal_child(false);
            entry_for_key.set_text("");
            entry_for_key.set_placeholder_text(Some("Ask anything…"));
            glib::Propagation::Stop
        });
    }
    window.add_controller(key_controller);

    // Clicking away (losing focus) → dismiss. The compositor's keybinding
    // handling can bounce focus off the freshly mapped window, so the close
    // stays disarmed during a grace period. If the bounce won, focus is taken
    // back — with the grace renewed, since that present bounces in turn and
    // an armed close would read it as the user clicking away.
    let grace = Rc::new(Cell::new(true));
    {
        let grace = grace.clone();
        let window = window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            if window.is_visible() && !window.is_active() {
                eprintln!("spotlight: focus lost during grace, presenting again");
                window.present();
                let grace = grace.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    grace.set(false);
                });
            } else {
                grace.set(false);
            }
        });
    }
    window.connect_is_active_notify(move |window| {
        if !window.is_active() && !grace.get() {
            eprintln!("spotlight: dismissed on focus loss");
            window.close();
        }
    });

    // Ask the compositor to blur the desktop behind the card (Pantheon shell).
    window.connect_map(|window| blur::apply(window, CORNER_RADIUS));

    window.present();
    entry.grab_focus();
    window
}
