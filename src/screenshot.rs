// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! Full-screen capture via Gala's `org.gnome.Shell.Screenshot` D-Bus
//! interface — the same non-interactive path elementary's own screenshot tool
//! uses, so no portal permission dialog is involved.

use std::fs;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use gdk_pixbuf::{InterpType, Pixbuf};

/// Cap the longer dimension so a 4K shot doesn't balloon into a ~10 MB
/// base64 payload; 1568 px is the common vision-model sweet spot.
const MAX_WIDTH: i32 = 1568;

/// Capture the screen and return it as an OpenAI `image_url` data URL.
/// Must be called before the spotlight window exists so it isn't in the shot;
/// the synchronous D-Bus call is invisible with no window mapped.
pub fn capture() -> Result<String, String> {
    let path = glib::tmp_dir().join(format!("elementary-intelligence-{}.png", std::process::id()));
    let path_str = path.to_string_lossy().into_owned();

    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|err| format!("session bus: {err}"))?;
    let result = connection
        .call_sync(
            Some("org.gnome.Shell.Screenshot"),
            "/org/gnome/Shell/Screenshot",
            "org.gnome.Shell.Screenshot",
            "Screenshot",
            // (include_cursor, flash, filename)
            Some(&(false, false, path_str.as_str()).to_variant()),
            glib::VariantTy::new("(bs)").ok().as_deref(),
            gio::DBusCallFlags::NONE,
            3000,
            gio::Cancellable::NONE,
        )
        .map_err(|err| format!("screenshot call: {err}"))?;

    let (success, filename_used) = result
        .get::<(bool, String)>()
        .ok_or_else(|| "unexpected reply type".to_string())?;
    if !success {
        let _ = fs::remove_file(&path);
        return Err("compositor reported failure".to_string());
    }

    let encoded = encode(&filename_used);
    let _ = fs::remove_file(&filename_used);
    if filename_used != path_str {
        let _ = fs::remove_file(&path);
    }
    encoded
}

fn encode(filename: &str) -> Result<String, String> {
    let pixbuf = Pixbuf::from_file(filename).map_err(|err| format!("load: {err}"))?;
    let pixbuf = if pixbuf.width() > MAX_WIDTH {
        let height = pixbuf.height() * MAX_WIDTH / pixbuf.width();
        pixbuf
            .scale_simple(MAX_WIDTH, height.max(1), InterpType::Bilinear)
            .ok_or_else(|| "downscale failed".to_string())?
    } else {
        pixbuf
    };
    let bytes = pixbuf
        .save_to_bufferv("png", &[])
        .map_err(|err| format!("encode: {err}"))?;
    Ok(format!("data:image/png;base64,{}", glib::base64_encode(&bytes)))
}
