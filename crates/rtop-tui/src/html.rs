// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Renders a terminal buffer as a standalone HTML page, used to turn
//! headless frames into screenshots (`make shots`) for visual review and
//! documentation.

use std::fmt::Write;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use crate::theme;

fn css(c: Color, fallback: Color) -> String {
    let c = if c == Color::Reset { fallback } else { c };
    let (r, g, b) = match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (204, 204, 204),
        Color::DarkGray => (118, 118, 118),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (229, 229, 229),
        Color::Indexed(i) => (i, i, i),
        Color::Reset => (0, 0, 0),
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn escape(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c => out.push(c),
        }
    }
}

/// `title` goes into `<title>`; the page is sized to the buffer.
pub fn buffer_to_html(buf: &Buffer, title: &str) -> String {
    let area = buf.area;
    let mut body = String::new();
    for y in 0..area.height {
        let mut run_style: Option<(String, String, bool, bool)> = None;
        let mut run = String::new();
        let flush =
            |style: &Option<(String, String, bool, bool)>, run: &mut String, body: &mut String| {
                if let Some((fg, bg, bold, ul)) = style {
                    let _ = write!(
                        body,
                        "<span style=\"color:{fg};background:{bg}{}{}\">",
                        if *bold { ";font-weight:700" } else { "" },
                        if *ul {
                            ";text-decoration:underline"
                        } else {
                            ""
                        }
                    );
                    escape(run, body);
                    body.push_str("</span>");
                }
                run.clear();
            };
        let mut skip = 0;
        for x in 0..area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buf[(area.x + x, area.y + y)];
            let sym = cell.symbol();
            let w = unicode_width::UnicodeWidthStr::width(sym);
            if w > 1 {
                skip = w - 1;
            }
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let (mut fg, mut bg) = (css(cell.fg, theme::TEXT), css(cell.bg, theme::BG));
            if reversed {
                std::mem::swap(&mut fg, &mut bg);
            }
            let style = Some((
                fg,
                bg,
                cell.modifier.contains(Modifier::BOLD),
                cell.modifier.contains(Modifier::UNDERLINED),
            ));
            if style != run_style {
                flush(&run_style, &mut run, &mut body);
                run_style = style;
            }
            run.push_str(if sym.is_empty() { " " } else { sym });
        }
        flush(&run_style, &mut run, &mut body);
        body.push('\n');
    }
    let mut t = String::new();
    escape(title, &mut t);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title><style>\
         html,body{{margin:0;background:{bg}}}\
         pre{{margin:0;padding:12px;font-family:'DejaVu Sans Mono','Cascadia Mono','JetBrains Mono',monospace;font-size:14px;line-height:17px;letter-spacing:0;white-space:pre;display:inline-block}}\
         span{{display:inline}}</style></head><body><pre>{body}</pre></body></html>",
        bg = css(theme::BG, theme::BG)
    )
}
