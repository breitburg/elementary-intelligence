// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! Minimal markdown → Pango markup conversion for assistant messages: fenced
//! code blocks, inline code, bold, italic, headings and list bullets.
//!
//! Tolerant of incomplete input by construction — the streaming UI re-renders
//! the full accumulated text on every delta, so an unclosed `**` or backtick
//! is emitted literally (escaped) rather than as a dangling Pango tag.

use gtk4::glib;

pub fn to_pango(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;
    let mut first = true;

    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        if fence {
            // The fence line itself (with any language tag) is dropped.
            in_code_block = !in_code_block;
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;

        if in_code_block {
            out.push_str("<tt>");
            out.push_str(&escape(line));
            out.push_str("</tt>");
        } else {
            out.push_str(&line_to_pango(line));
        }
    }
    // An unterminated fence at the end of the stream: nothing to close, every
    // code line carries its own <tt> pair.
    out
}

fn line_to_pango(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    // Headings render bold.
    if let Some(heading) = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("## "))
        .or_else(|| trimmed.strip_prefix("# "))
    {
        return format!("<b>{}</b>", inline_to_pango(heading));
    }

    // List markers become bullets.
    if let Some(item) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
        return format!("{indent}  •  {}", inline_to_pango(item));
    }
    if let Some(dot) = trimmed.find(". ") {
        if dot > 0 && trimmed[..dot].chars().all(|c| c.is_ascii_digit()) {
            return format!("{indent}{}  {}", &trimmed[..dot + 1], inline_to_pango(&trimmed[dot + 2..]));
        }
    }

    format!("{indent}{}", inline_to_pango(trimmed))
}

/// One left-to-right scan over a line: `` ` ``, `**` and `*` spans, each only
/// emitted as a tag when its closing marker exists; otherwise literal.
fn inline_to_pango(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while !rest.is_empty() {
        let next = ["`", "**", "*"]
            .iter()
            .filter_map(|m| rest.find(m).map(|i| (i, *m)))
            .min_by_key(|(i, m)| (*i, std::cmp::Reverse(m.len())));
        let Some((start, marker)) = next else {
            out.push_str(&escape(rest));
            break;
        };

        out.push_str(&escape(&rest[..start]));
        let after = &rest[start + marker.len()..];
        match after.find(marker).filter(|&end| end > 0) {
            Some(end) => {
                let inner = &after[..end];
                match marker {
                    // Inline code is opaque: markers inside are literal.
                    "`" => out.push_str(&format!("<tt>{}</tt>", escape(inner))),
                    "**" => out.push_str(&format!("<b>{}</b>", inline_to_pango(inner))),
                    _ => out.push_str(&format!("<i>{}</i>", inline_to_pango(inner))),
                }
                rest = &after[end + marker.len()..];
            }
            None => {
                // Unclosed marker (mid-stream or literal): emit as-is.
                out.push_str(&escape(marker));
                rest = after;
            }
        }
    }
    out
}

fn escape(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

#[cfg(test)]
mod tests {
    use super::to_pango;

    #[test]
    fn bold() {
        assert_eq!(to_pango("a **b** c"), "a <b>b</b> c");
    }

    #[test]
    fn italic() {
        assert_eq!(to_pango("a *b* c"), "a <i>b</i> c");
    }

    #[test]
    fn inline_code_keeps_markers_literal() {
        assert_eq!(to_pango("use `a * b` here"), "use <tt>a * b</tt> here");
    }

    #[test]
    fn fenced_block_escapes_markup() {
        assert_eq!(
            to_pango("```rust\nlet a = b < c;\n```"),
            "<tt>let a = b &lt; c;</tt>"
        );
    }

    #[test]
    fn dangling_bold_is_literal() {
        assert_eq!(to_pango("a **b"), "a **b");
    }

    #[test]
    fn unterminated_fence_mid_stream() {
        assert_eq!(to_pango("hi\n```\ncode <"), "hi\n<tt>code &lt;</tt>");
    }

    #[test]
    fn bullets_and_headings() {
        assert_eq!(to_pango("# Title\n- item"), "<b>Title</b>\n  •  item");
        assert_eq!(to_pango("1. first"), "1.  first");
    }
}
