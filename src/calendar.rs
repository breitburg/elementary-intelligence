// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! Calendar toolset backed by Evolution Data Server (EDS) over D-Bus.
//!
//! elementary's Calendar app is a frontend on EDS, so the same data is reachable
//! through the session-bus `org.gnome.evolution.dataserver.Calendar8` factory.
//! The flow for any calendar is: `CalendarFactory.OpenCalendar(uid)` returns a
//! per-calendar object path, `Calendar.Open()` activates it, then `GetObjectList`
//! / `GetObject` read and `CreateObjects` / `ModifyObjects` / `RemoveObjects`
//! write. Sources (the available calendars) come from the `Sources5` registry's
//! ObjectManager.
//!
//! All D-Bus uses `gtk4::gio` (no extra dependency), mirroring `screenshot.rs`.
//! The executors run on the API worker thread (blocking, `Send + Sync`); gio's
//! synchronous calls are safe there — GDBus drives them on its own worker.
//!
//! Dates are handled as UTC with exact integer math (no `chrono`): query bounds
//! are formatted as EDS `make-time` UTC strings, and stored event times are
//! displayed as their wall-clock value with a `UTC`/`all day` marker rather than
//! being converted between zones.

use std::sync::atomic::{AtomicU64, Ordering};

use gtk4::gio;
use gtk4::glib::Variant;
use gtk4::prelude::*;
use serde_json::json;

use crate::datetime::{epoch_to_date, epoch_to_make_time, now_epoch, parse_iso};
use crate::tools::{truncate, Tool, MAX_OUTPUT_BYTES};

const CAL_DEST: &str = "org.gnome.evolution.dataserver.Calendar8";
const CAL_FACTORY_PATH: &str = "/org/gnome/evolution/dataserver/CalendarFactory";
const CAL_FACTORY_IFACE: &str = "org.gnome.evolution.dataserver.CalendarFactory";
const CAL_IFACE: &str = "org.gnome.evolution.dataserver.Calendar";
const SOURCES_DEST: &str = "org.gnome.evolution.dataserver.Sources5";
const SOURCES_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";

/// UID of the built-in writable local calendar ("Personal"); the default
/// create/modify target when the model names no calendar.
const DEFAULT_CAL_UID: &str = "system-calendar";

/// D-Bus timeout. Kept modest so a stalled network calendar can't hang the
/// worker thread for long (the agent loop has no per-tool timeout of its own).
const DBUS_TIMEOUT_MS: i32 = 5000;

/// `ECalObjModType` GEnum nick for modifying/removing a single (non-recurring)
/// instance. The local backend rejects `all` (it advertises `no-thisandprior`),
/// and we always operate on a whole object by UID, so `this` is correct.
const MOD_TYPE_THIS: &str = "this";

/// Builds the calendar tools exposed to the model. One settings toggle
/// ("calendar") enables this whole set.
pub fn tools() -> Vec<Tool> {
    vec![
        list_events_tool(),
        create_event_tool(),
        modify_event_tool(),
        delete_event_tool(),
    ]
}

// ---------------------------------------------------------------------------
// D-Bus plumbing
// ---------------------------------------------------------------------------

fn session_bus() -> Result<gio::DBusConnection, String> {
    gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|err| format!("session bus: {err}"))
}

/// One blocking D-Bus call. `reply_type` is left unchecked (`None`): we read the
/// reply variant structurally, so EDS adding fields can't break us.
fn call(
    conn: &gio::DBusConnection,
    path: &str,
    iface: &str,
    method: &str,
    params: Option<&Variant>,
) -> Result<Variant, String> {
    conn.call_sync(
        Some(CAL_DEST),
        path,
        iface,
        method,
        params,
        None,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        gio::Cancellable::NONE,
    )
    .map_err(|err| format!("{method}: {err}"))
}

/// `OpenCalendar(uid)` → object path, then `Open()` to activate it.
fn open_calendar(conn: &gio::DBusConnection, uid: &str) -> Result<String, String> {
    let reply = call(
        conn,
        CAL_FACTORY_PATH,
        CAL_FACTORY_IFACE,
        "OpenCalendar",
        Some(&(uid,).to_variant()),
    )?;
    let path = reply
        .child_value(0)
        .str()
        .map(|s| s.to_string())
        .ok_or("OpenCalendar: unexpected reply")?;
    call(conn, &path, CAL_IFACE, "Open", None)?;
    Ok(path)
}

/// Events matching an EDS S-expression query, as raw iCalendar strings.
fn get_object_list(
    conn: &gio::DBusConnection,
    path: &str,
    query: &str,
) -> Result<Vec<String>, String> {
    let reply = call(
        conn,
        path,
        CAL_IFACE,
        "GetObjectList",
        Some(&(query,).to_variant()),
    )?;
    Ok(string_array(&reply.child_value(0)))
}

/// One object's iCalendar by UID (empty `rid` = the master, non-recurring).
/// Returns `Err` if the calendar doesn't hold it.
fn get_object(
    conn: &gio::DBusConnection,
    path: &str,
    uid: &str,
) -> Result<String, String> {
    let reply = call(
        conn,
        path,
        CAL_IFACE,
        "GetObject",
        Some(&(uid, "").to_variant()),
    )?;
    reply
        .child_value(0)
        .str()
        .map(|s| s.to_string())
        .ok_or_else(|| "GetObject: unexpected reply".to_string())
}

/// Create one event (a bare VEVENT — EDS rejects a VCALENDAR wrapper here).
fn create_object(conn: &gio::DBusConnection, path: &str, ics: &str) -> Result<(), String> {
    call(
        conn,
        path,
        CAL_IFACE,
        "CreateObjects",
        Some(&(vec![ics.to_string()], 0u32).to_variant()),
    )?;
    Ok(())
}

fn modify_object(conn: &gio::DBusConnection, path: &str, ics: &str) -> Result<(), String> {
    call(
        conn,
        path,
        CAL_IFACE,
        "ModifyObjects",
        Some(&(vec![ics.to_string()], MOD_TYPE_THIS, 0u32).to_variant()),
    )?;
    Ok(())
}

fn remove_object(conn: &gio::DBusConnection, path: &str, uid: &str) -> Result<(), String> {
    let targets = vec![(uid.to_string(), String::new())];
    call(
        conn,
        path,
        CAL_IFACE,
        "RemoveObjects",
        Some(&(targets, MOD_TYPE_THIS, 0u32).to_variant()),
    )?;
    Ok(())
}

#[derive(Clone)]
struct CalSource {
    uid: String,
    name: String,
}

/// All calendar sources from the `Sources5` ObjectManager. A source is a
/// calendar if its `Data` INI contains a `[Calendar]` group.
fn list_calendar_sources(conn: &gio::DBusConnection) -> Result<Vec<CalSource>, String> {
    let reply = conn
        .call_sync(
            Some(SOURCES_DEST),
            SOURCES_PATH,
            OBJECT_MANAGER_IFACE,
            "GetManagedObjects",
            None,
            None,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .map_err(|err| format!("GetManagedObjects: {err}"))?;

    // Reply: (a{o a{s a{s v}}}) — managed objects, each a path → interfaces map.
    let objects = reply.child_value(0);
    let mut sources = Vec::new();
    for i in 0..objects.n_children() {
        let entry = objects.child_value(i); // {o, a{s a{s v}}}
        let ifaces = entry.child_value(1);
        let Some(data) = prop_string(&ifaces, "Data") else {
            continue;
        };
        if !data.contains("[Calendar]") {
            continue;
        }
        let Some(uid) = prop_string(&ifaces, "UID") else {
            continue;
        };
        let name = ini_value(&data, "DisplayName").unwrap_or_else(|| uid.clone());
        sources.push(CalSource { uid, name });
    }
    Ok(sources)
}

/// Find the string value of property `key` anywhere in an interfaces map
/// (`a{s a{s v}}`), unwrapping the `v` wrapper.
fn prop_string(ifaces: &Variant, key: &str) -> Option<String> {
    for i in 0..ifaces.n_children() {
        let iface_entry = ifaces.child_value(i); // {s, a{s v}}
        let props = iface_entry.child_value(1); // a{s v}
        for j in 0..props.n_children() {
            let prop = props.child_value(j); // {s, v}
            if prop.child_value(0).str() == Some(key) {
                return prop
                    .child_value(1)
                    .as_variant()
                    .and_then(|v| v.str().map(|s| s.to_string()));
            }
        }
    }
    None
}

/// Value of `key=` from a `.source` INI string. Matches the bare key only, so
/// localized variants like `DisplayName[de]=` are ignored.
fn ini_value(ini: &str, key: &str) -> Option<String> {
    for line in ini.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Extract a `Vec<String>` from a GVariant array (`as`).
fn string_array(arr: &Variant) -> Vec<String> {
    (0..arr.n_children())
        .filter_map(|i| arr.child_value(i).str().map(|s| s.to_string()))
        .collect()
}

/// Open every source and return the (path, source) of the first holding `uid`.
fn find_event(
    conn: &gio::DBusConnection,
    sources: &[CalSource],
    uid: &str,
) -> Result<(String, CalSource), String> {
    for src in sources {
        if let Ok(path) = open_calendar(conn, &src.uid) {
            if let Ok(ics) = get_object(conn, &path, uid) {
                if !ics.is_empty() {
                    return Ok((path, src.clone()));
                }
            }
        }
    }
    Err(format!("no event with uid \"{uid}\" found in any calendar"))
}

/// Resolve the target calendar for a write: by name/uid substring if the model
/// named one, else the default local calendar (falling back to the first).
fn resolve_target(sources: &[CalSource], named: Option<&str>) -> Result<CalSource, String> {
    match named {
        Some(name) => {
            let needle = name.to_lowercase();
            sources
                .iter()
                .find(|s| s.uid == name || s.name.to_lowercase().contains(&needle))
                .cloned()
                .ok_or_else(|| format!("no calendar matching \"{name}\""))
        }
        None => sources
            .iter()
            .find(|s| s.uid == DEFAULT_CAL_UID)
            .or_else(|| sources.first())
            .cloned()
            .ok_or_else(|| "no calendars available".to_string()),
    }
}

// ---------------------------------------------------------------------------
// iCalendar (RFC 5545) — minimal parse and build
// ---------------------------------------------------------------------------

struct Event {
    summary: String,
    start_raw: Option<String>,
    end_raw: Option<String>,
    all_day: bool,
    location: Option<String>,
    uid: Option<String>,
    calendar: String,
    sort_key: String,
}

/// Unfold logical lines: a line beginning with a space or tab continues the
/// previous one (RFC 5545 §3.1).
fn unfold(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(line.to_string());
    }
    lines
}

/// Split `NAME;PARAM=x:VALUE` into (uppercased name, params, value) at the first
/// unquoted colon.
fn split_property(line: &str) -> Option<(String, String, String)> {
    let mut in_quote = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ':' if !in_quote => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let (name, params) = match head.find(';') {
        Some(s) => (&head[..s], &head[s + 1..]),
        None => (head, ""),
    };
    Some((name.to_ascii_uppercase(), params.to_string(), value.to_string()))
}

/// Parse one VEVENT's inner lines into an [`Event`] (calendar set by caller).
fn parse_vevent(lines: &[String]) -> Event {
    let mut ev = Event {
        summary: String::new(),
        start_raw: None,
        end_raw: None,
        all_day: false,
        location: None,
        uid: None,
        calendar: String::new(),
        sort_key: "0".to_string(),
    };
    for line in lines {
        let Some((name, params, value)) = split_property(line) else {
            continue;
        };
        match name.as_str() {
            "SUMMARY" => ev.summary = unescape(&value),
            "DTSTART" => {
                ev.all_day = is_date_only(&params, &value);
                ev.start_raw = Some(value);
            }
            "DTEND" => ev.end_raw = Some(value),
            "LOCATION" => ev.location = Some(unescape(&value)),
            "UID" => ev.uid = Some(value),
            _ => {}
        }
    }
    ev.sort_key = sort_key(&ev.start_raw);
    ev
}

/// Parse every VEVENT in a reply element (a bare VEVENT or a VCALENDAR holding
/// one or more).
fn parse_events(blob: &str) -> Vec<Event> {
    let lines = unfold(blob);
    let mut events = Vec::new();
    let mut start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            start = Some(i + 1);
        } else if trimmed.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(s) = start.take() {
                events.push(parse_vevent(&lines[s..i]));
            }
        }
    }
    events
}

fn is_date_only(params: &str, value: &str) -> bool {
    params
        .split(';')
        .any(|p| p.eq_ignore_ascii_case("VALUE=DATE"))
        || (!value.contains('T') && value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()))
}

/// Zero-padded 14-digit key (YYYYMMDDHHMMSS) for chronological sorting.
fn sort_key(start_raw: &Option<String>) -> String {
    let digits: String = start_raw
        .iter()
        .flat_map(|s| s.chars())
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    format!("{digits:0<14}")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Render an iCalendar date/time value for display. All-day → `YYYY-MM-DD`;
/// timed → `YYYY-MM-DD HH:MM` with a `UTC` suffix when the value carries `Z`.
fn format_dt(raw: &str, all_day: bool) -> String {
    let utc = raw.ends_with('Z');
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if all_day || (!raw.contains('T') && digits.len() == 8) {
        if digits.len() >= 8 {
            return format!("{}-{}-{}", &digits[0..4], &digits[4..6], &digits[6..8]);
        }
        return raw.to_string();
    }
    if digits.len() >= 12 {
        let base = format!(
            "{}-{}-{} {}:{}",
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            &digits[8..10],
            &digits[10..12]
        );
        return if utc { format!("{base} UTC") } else { base };
    }
    raw.to_string()
}

/// Build a bare VEVENT for creation.
fn build_vevent(
    uid: &str,
    start: &str,
    all_day: bool,
    end: Option<&str>,
    summary: &str,
    location: Option<&str>,
    description: Option<&str>,
) -> Result<String, String> {
    let (start_secs, _) = parse_iso(start)?;
    let mut v = String::from("BEGIN:VEVENT\r\n");
    v.push_str(&format!("UID:{uid}\r\n"));
    v.push_str(&format!("DTSTAMP:{}\r\n", epoch_to_make_time(now_epoch())));
    if all_day {
        v.push_str(&format!("DTSTART;VALUE=DATE:{}\r\n", epoch_to_date(start_secs)));
        let end_secs = match end {
            Some(e) => parse_iso(e)?.0,
            None => start_secs + 86_400,
        };
        v.push_str(&format!("DTEND;VALUE=DATE:{}\r\n", epoch_to_date(end_secs)));
    } else {
        v.push_str(&format!("DTSTART:{}\r\n", epoch_to_make_time(start_secs)));
        let end_secs = match end {
            Some(e) => parse_iso(e)?.0,
            None => start_secs + 3600,
        };
        v.push_str(&format!("DTEND:{}\r\n", epoch_to_make_time(end_secs)));
    }
    v.push_str(&format!("SUMMARY:{}\r\n", escape(summary)));
    if let Some(loc) = location.filter(|l| !l.is_empty()) {
        v.push_str(&format!("LOCATION:{}\r\n", escape(loc)));
    }
    if let Some(desc) = description.filter(|d| !d.is_empty()) {
        v.push_str(&format!("DESCRIPTION:{}\r\n", escape(desc)));
    }
    v.push_str("END:VEVENT\r\n");
    Ok(v)
}

/// A `DTSTART`/`DTEND` property line from ISO input, emitting `;VALUE=DATE` for
/// date-only values and a UTC timestamp otherwise.
fn dt_property(name: &str, iso: &str) -> Result<String, String> {
    let (secs, has_time) = parse_iso(iso)?;
    Ok(if has_time {
        format!("{name}:{}", epoch_to_make_time(secs))
    } else {
        format!("{name};VALUE=DATE:{}", epoch_to_date(secs))
    })
}

/// Replace (or remove) all lines of property `name` in an unfolded VEVENT,
/// inserting `new_line` just before `END:VEVENT`.
fn set_property(lines: &mut Vec<String>, name: &str, new_line: Option<String>) {
    lines.retain(|l| {
        let prop = l
            .split([';', ':'])
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        prop != name
    });
    if let Some(line) = new_line {
        let pos = lines
            .iter()
            .position(|l| l.trim().eq_ignore_ascii_case("END:VEVENT"))
            .unwrap_or(lines.len());
        lines.insert(pos, line);
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn list_events_tool() -> Tool {
    Tool::new(
        "calendar_list_events",
        "List calendar events in a time range across all the user's calendars. \
         Returns each event's title, time, location, calendar, and uid (use the \
         uid to modify or delete an event).",
        json!({
            "type": "object",
            "properties": {
                "start": {"type": "string", "description": "Start of range, ISO 8601 (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SSZ). Naive times are UTC. Defaults to now."},
                "end": {"type": "string", "description": "End of range, ISO 8601. Defaults to 7 days after start."},
                "calendar": {"type": "string", "description": "Optional: only this calendar (case-insensitive name substring)."}
            },
            "additionalProperties": false
        }),
        |args| {
            let now = now_epoch();
            let (start_secs, _) = match args["start"].as_str() {
                Some(s) => parse_iso(s)?,
                None => (now, false),
            };
            let (end_secs, _) = match args["end"].as_str() {
                Some(s) => parse_iso(s)?,
                None => (start_secs + 7 * 86_400, false),
            };
            let filter = args["calendar"].as_str().map(|s| s.to_lowercase());

            let conn = session_bus()?;
            let sources = list_calendar_sources(&conn)?;
            let query = format!(
                "(occur-in-time-range? (make-time \"{}\") (make-time \"{}\"))",
                epoch_to_make_time(start_secs),
                epoch_to_make_time(end_secs)
            );

            let mut events = Vec::new();
            let mut notes = Vec::new();
            for src in &sources {
                if let Some(f) = &filter {
                    if !src.name.to_lowercase().contains(f.as_str()) {
                        continue;
                    }
                }
                match open_calendar(&conn, &src.uid)
                    .and_then(|path| get_object_list(&conn, &path, &query))
                {
                    Ok(blobs) => {
                        for blob in blobs {
                            for mut ev in parse_events(&blob) {
                                ev.calendar = src.name.clone();
                                events.push(ev);
                            }
                        }
                    }
                    Err(err) => notes.push(format!("- {} ({}): {}", src.name, src.uid, err)),
                }
            }
            events.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

            let mut out = String::new();
            if events.is_empty() {
                out.push_str(&format!(
                    "No events found between {} and {}.",
                    format_dt(&epoch_to_make_time(start_secs), false),
                    format_dt(&epoch_to_make_time(end_secs), false)
                ));
            } else {
                for ev in &events {
                    out.push_str(&render_event(ev));
                }
            }
            if !notes.is_empty() {
                out.push_str("\nNotes (calendars that could not be read):\n");
                out.push_str(&notes.join("\n"));
            }
            truncate(&mut out, MAX_OUTPUT_BYTES);
            Ok(out)
        },
    )
}

fn render_event(ev: &Event) -> String {
    let title = if ev.summary.is_empty() {
        "(no title)"
    } else {
        &ev.summary
    };
    let mut s = format!("• {title}  [{}]\n", ev.calendar);
    let when = match (&ev.start_raw, &ev.end_raw) {
        (Some(start), Some(end)) => format!(
            "    {} – {}\n",
            format_dt(start, ev.all_day),
            format_dt(end, ev.all_day)
        ),
        (Some(start), None) => format!("    {}\n", format_dt(start, ev.all_day)),
        _ => String::new(),
    };
    s.push_str(&when);
    if let Some(loc) = ev.location.as_deref().filter(|l| !l.is_empty()) {
        s.push_str(&format!("    Location: {loc}\n"));
    }
    if let Some(uid) = &ev.uid {
        s.push_str(&format!("    uid: {uid}\n"));
    }
    s
}

fn create_event_tool() -> Tool {
    Tool::new(
        "calendar_create_event",
        "Create a new calendar event. Writes to the user's real calendar.",
        json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string", "description": "Event title."},
                "start": {"type": "string", "description": "Start, ISO 8601. A date only (YYYY-MM-DD) creates an all-day event; include a time for a timed event. Naive times are UTC."},
                "end": {"type": "string", "description": "End, ISO 8601. Defaults to 1 hour after a timed start, or the next day for all-day."},
                "location": {"type": "string"},
                "description": {"type": "string"},
                "calendar": {"type": "string", "description": "Optional target calendar (name substring or uid). Defaults to the local Personal calendar."}
            },
            "required": ["summary", "start"],
            "additionalProperties": false
        }),
        |args| {
            let summary = args["summary"]
                .as_str()
                .ok_or("`summary` must be a string")?;
            let start = args["start"].as_str().ok_or("`start` must be a string")?;
            let (_, has_time) = parse_iso(start)?;
            let all_day = !has_time;

            let conn = session_bus()?;
            let sources = list_calendar_sources(&conn)?;
            let target = resolve_target(&sources, args["calendar"].as_str())?;
            let path = open_calendar(&conn, &target.uid)?;

            let uid = new_uid();
            let ics = build_vevent(
                &uid,
                start,
                all_day,
                args["end"].as_str(),
                summary,
                args["location"].as_str(),
                args["description"].as_str(),
            )?;
            create_object(&conn, &path, &ics)?;
            Ok(format!(
                "Created \"{summary}\" in {} (uid: {uid}).",
                target.name
            ))
        },
    )
}

fn modify_event_tool() -> Tool {
    Tool::new(
        "calendar_modify_event",
        "Modify an existing event, identified by its uid (from calendar_list_events). \
         Only the fields you provide are changed. Writes to the user's real calendar.",
        json!({
            "type": "object",
            "properties": {
                "uid": {"type": "string", "description": "uid of the event to modify."},
                "summary": {"type": "string"},
                "start": {"type": "string", "description": "New start, ISO 8601 (date only = all-day)."},
                "end": {"type": "string", "description": "New end, ISO 8601."},
                "location": {"type": "string"},
                "description": {"type": "string"}
            },
            "required": ["uid"],
            "additionalProperties": false
        }),
        |args| {
            let uid = args["uid"].as_str().ok_or("`uid` must be a string")?;
            let summary = args["summary"].as_str();
            let start = args["start"].as_str();
            let end = args["end"].as_str();
            let location = args["location"].as_str();
            let description = args["description"].as_str();
            if summary.is_none()
                && start.is_none()
                && end.is_none()
                && location.is_none()
                && description.is_none()
            {
                return Err("nothing to modify: provide at least one field to change".into());
            }

            let conn = session_bus()?;
            let sources = list_calendar_sources(&conn)?;
            let (path, src) = find_event(&conn, &sources, uid)?;
            let existing = get_object(&conn, &path, uid)?;
            let mut lines = unfold(&existing);

            if let Some(s) = summary {
                set_property(&mut lines, "SUMMARY", Some(format!("SUMMARY:{}", escape(s))));
            }
            if let Some(s) = start {
                set_property(&mut lines, "DTSTART", Some(dt_property("DTSTART", s)?));
            }
            if let Some(e) = end {
                set_property(&mut lines, "DTEND", Some(dt_property("DTEND", e)?));
            }
            if let Some(l) = location {
                set_property(&mut lines, "LOCATION", Some(format!("LOCATION:{}", escape(l))));
            }
            if let Some(d) = description {
                set_property(
                    &mut lines,
                    "DESCRIPTION",
                    Some(format!("DESCRIPTION:{}", escape(d))),
                );
            }

            let new_ics = lines.join("\r\n");
            modify_object(&conn, &path, &new_ics)?;
            Ok(format!("Modified event {uid} in {}.", src.name))
        },
    )
}

fn delete_event_tool() -> Tool {
    Tool::new(
        "calendar_delete_event",
        "Delete an event, identified by its uid (from calendar_list_events). \
         Permanently removes it from the user's real calendar.",
        json!({
            "type": "object",
            "properties": {
                "uid": {"type": "string", "description": "uid of the event to delete."}
            },
            "required": ["uid"],
            "additionalProperties": false
        }),
        |args| {
            let uid = args["uid"].as_str().ok_or("`uid` must be a string")?;
            let conn = session_bus()?;
            let sources = list_calendar_sources(&conn)?;
            let (path, src) = find_event(&conn, &sources, uid)?;
            remove_object(&conn, &path, uid)?;
            Ok(format!("Deleted event {uid} from {}.", src.name))
        },
    )
}

/// Process-unique event UID.
fn new_uid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("beckon-{}-{}-{n}@beckon", now_epoch(), std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_dt_variants() {
        assert_eq!(format_dt("20260613", true), "2026-06-13");
        assert_eq!(format_dt("20260613T143000Z", false), "2026-06-13 14:30 UTC");
        assert_eq!(format_dt("20260613T143000", false), "2026-06-13 14:30");
    }

    #[test]
    fn parse_events_handles_folding() {
        // A folded DESCRIPTION (continuation line begins with a space).
        // RFC 5545 unfolding removes the CRLF *and* the continuation's leading
        // space, so a fold split mid-word rejoins with no gap.
        let ics = "BEGIN:VEVENT\r\nUID:abc\r\nSUMMARY:Team sy\r\n nc\r\nDTSTART:20260615T090000Z\r\nDTEND:20260615T093000Z\r\nLOCATION:Room 4\r\nEND:VEVENT\r\n";
        let events = parse_events(ics);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.summary, "Team sync");
        assert_eq!(ev.uid.as_deref(), Some("abc"));
        assert_eq!(ev.location.as_deref(), Some("Room 4"));
        assert!(!ev.all_day);
        assert_eq!(ev.sort_key, "20260615090000");
    }

    #[test]
    fn all_day_detection() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20260620\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\n";
        let ev = &parse_events(ics)[0];
        assert!(ev.all_day);
    }

    #[test]
    fn build_vevent_timed_and_all_day() {
        let timed = build_vevent("u1", "2026-06-20T15:00:00", false, None, "Meet; greet", None, None).unwrap();
        assert!(timed.contains("DTSTART:20260620T150000Z\r\n"));
        assert!(timed.contains("DTEND:20260620T160000Z\r\n")); // +1h default
        assert!(timed.contains("SUMMARY:Meet\\; greet\r\n")); // escaped semicolon

        let allday = build_vevent("u2", "2026-06-20", true, None, "Trip", None, None).unwrap();
        assert!(allday.contains("DTSTART;VALUE=DATE:20260620\r\n"));
        assert!(allday.contains("DTEND;VALUE=DATE:20260621\r\n")); // next day default
    }

    // Live end-to-end CRUD against the running Evolution Data Server. Ignored by
    // default (touches the real calendar / needs the session bus); run with
    // `cargo test --release -- --ignored live_crud`.
    #[test]
    #[ignore]
    fn live_crud_roundtrip() {
        let conn = session_bus().expect("session bus");
        let sources = list_calendar_sources(&conn).expect("list sources");
        assert!(
            sources.iter().any(|s| s.uid == DEFAULT_CAL_UID),
            "expected a {DEFAULT_CAL_UID} source, got {:?}",
            sources.iter().map(|s| &s.uid).collect::<Vec<_>>()
        );
        let path = open_calendar(&conn, DEFAULT_CAL_UID).expect("open");

        let uid = new_uid();
        let ics = build_vevent(&uid, "2026-06-20T15:00:00", false, None, "Beckon CRUD test", Some("Room 1"), None)
            .expect("build");
        create_object(&conn, &path, &ics).expect("create");

        let fetched = get_object(&conn, &path, &uid).expect("get after create");
        assert!(fetched.contains("SUMMARY:Beckon CRUD test"));

        let mut lines = unfold(&fetched);
        set_property(&mut lines, "SUMMARY", Some("SUMMARY:Beckon CRUD modified".to_string()));
        modify_object(&conn, &path, &lines.join("\r\n")).expect("modify");
        let after = get_object(&conn, &path, &uid).expect("get after modify");
        assert!(after.contains("SUMMARY:Beckon CRUD modified"));

        remove_object(&conn, &path, &uid).expect("remove");
        assert!(get_object(&conn, &path, &uid).is_err(), "event should be gone");
    }

    #[test]
    fn set_property_replaces_in_place() {
        let mut lines = unfold("BEGIN:VEVENT\r\nUID:u\r\nSUMMARY:Old\r\nEND:VEVENT\r\n");
        set_property(&mut lines, "SUMMARY", Some("SUMMARY:New".to_string()));
        let out = lines.join("\r\n");
        assert!(out.contains("SUMMARY:New"));
        assert!(!out.contains("SUMMARY:Old"));
        assert!(out.contains("UID:u")); // untouched
        // The new line lands before END:VEVENT.
        assert!(out.find("SUMMARY:New").unwrap() < out.find("END:VEVENT").unwrap());
    }
}
