//! ANSI → ratatui converter for program output shown in the preview pane:
//! `git diff --color=always`, `git show`, `glow`, `viu -b`, and any
//! per-filetype previewer a user configures. Supports 16-color + bright,
//! 256-color, truecolor (both `;` and `:` forms), bold/dim/italic/underline/
//! strikethrough/reverse, resets, and backspace overstrike. Style carries
//! across lines because renderers routinely open a colour on one line and
//! close it on a later one. Every other escape sequence — cursor movement,
//! erases, private modes, OSC hyperlinks, DCS/APC payloads such as the
//! Kitty graphics query `viu` emits when stdout is a pipe — is consumed
//! WHOLE so none of its bytes leak into the text.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Convert one chunk of ANSI-colored text into ratatui lines.
pub fn to_lines(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut style = Style::default();
    for raw in input.lines() {
        let (line, next) = parse_line(raw, style);
        lines.push(line);
        style = next;
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

/// Parse one line starting from `start` (the style the previous line ended
/// in); returns the line and the style it ends in.
fn parse_line(raw: &str, start: Style) -> (Line<'static>, Style) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = start;
    let mut text = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {}
            '\r' => continue,
            '\u{8}' => {
                // Overstrike (man/less): `x\bx` = bold x, `_\bx` / `x\b_` =
                // underlined x. A plain backspace just discards the
                // character it steps over, like a terminal overwrite.
                let Some(prev) = text.pop() else { continue };
                let next = chars.peek().copied();
                let overstruck = match next {
                    Some(n) if n == prev => Some((prev, Modifier::BOLD)),
                    Some(n) if prev == '_' => Some((n, Modifier::UNDERLINED)),
                    Some('_') => Some((prev, Modifier::UNDERLINED)),
                    _ => None,
                };
                if let Some((ch, modifier)) = overstruck {
                    chars.next();
                    if !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    spans.push(Span::styled(ch.to_string(), style.add_modifier(modifier)));
                }
                continue;
            }
            _ => {
                text.push(c);
                continue;
            }
        }
        // An escape sequence. Decide its shape from the next byte.
        let Some(&kind) = chars.peek() else { break };
        match kind {
            // OSC / DCS / SOS / PM / APC carry a string payload through
            // BEL or ST (ESC \). Hyperlink metadata, Kitty graphics commands
            // and queries, terminal titles: none of it is text.
            ']' | 'P' | 'X' | '^' | '_' => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            '[' => {
                chars.next();
                // CSI: parameter bytes 0x30–0x3F, intermediate bytes
                // 0x20–0x2F, one final byte 0x40–0x7E (ECMA-48 5.4).
                let mut params = String::new();
                let mut private = false;
                let mut intermediate = false;
                let mut terminator = None;
                for c in chars.by_ref() {
                    match c {
                        '0'..='9' | ';' | ':' => params.push(c),
                        '<'..='?' => private = true,
                        ' '..='/' => intermediate = true,
                        '@'..='~' => {
                            terminator = Some(c);
                            break;
                        }
                        // Malformed: stop at the first byte outside the
                        // grammar and treat it as consumed.
                        _ => break,
                    }
                }
                if terminator == Some('m') && !private && !intermediate {
                    if !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = apply_sgr(style, &params);
                }
                // Cursor moves, erases, modes (`ESC[?25l`), device queries
                // (`ESC[c`): consumed whole, irrelevant for static text.
            }
            // nF sequences: ESC, one or more intermediates 0x20–0x2F, then a
            // final 0x30–0x7E — charset selection like `ESC(B`.
            ' '..='/' => {
                while chars.peek().is_some_and(|c| (' '..='/').contains(c)) {
                    chars.next();
                }
                if chars.peek().is_some_and(|c| ('0'..='~').contains(c)) {
                    chars.next();
                }
            }
            // Single-byte Fp/Fe/Fs sequences: `ESC7`, `ESC8`, `ESC=`, `ESC>`,
            // `ESCM`, `ESCc` …
            '0'..='~' => {
                chars.next();
            }
            // ESC followed by a control character: drop only the ESC.
            _ => {}
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    (Line::from(spans), style)
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    // An empty slot (`38:2::r:g:b`, the colour-space id nobody fills in) is
    // `None`; an empty list means a single 0.
    let codes: Vec<Option<u16>> = params
        .split([';', ':'])
        .map(|p| if p.is_empty() { None } else { p.parse().ok() })
        .collect();
    let codes = if params.is_empty() {
        vec![Some(0)]
    } else {
        codes
    };
    let code = |index: usize| codes.get(index).copied().flatten();
    let mut i = 0;
    while i < codes.len() {
        match code(i).unwrap_or(0) {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 | 21 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            n @ 30..=37 => style = style.fg(basic_color(n - 30, false)),
            39 => style = style.fg(Color::Reset),
            n @ 40..=47 => style = style.bg(basic_color(n - 40, false)),
            49 => style = style.bg(Color::Reset),
            n @ 90..=97 => style = style.fg(basic_color(n - 90, true)),
            n @ 100..=107 => style = style.bg(basic_color(n - 100, true)),
            n @ (38 | 48 | 58) => {
                // 38;5;n (indexed) or 38;2;r;g;b (truecolor), colon form
                // optionally with an empty colour-space slot. 58 (underline
                // colour) is parsed the same way and ignored.
                let apply = |style: Style, c: Color| match n {
                    38 => style.fg(c),
                    48 => style.bg(c),
                    _ => style,
                };
                match code(i + 1) {
                    Some(5) => {
                        if let Some(v) = code(i + 2) {
                            style = apply(style, Color::Indexed(v as u8));
                        }
                        i += 2;
                    }
                    Some(2) => {
                        let skip_space = codes.get(i + 2).is_some_and(Option::is_none);
                        let base = if skip_space { i + 3 } else { i + 2 };
                        if let (Some(r), Some(g), Some(b)) =
                            (code(base), code(base + 1), code(base + 2))
                        {
                            style = apply(style, Color::Rgb(r as u8, g as u8, b as u8));
                        }
                        i = base + 2;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

fn basic_color(n: u16, bright: bool) -> Color {
    let color = match (n, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    };
    // On a light terminal background the profile's white / light grey are
    // invisible; hand those back to the default foreground instead.
    if crate::ui::is_light() && matches!(color, Color::White | Color::Gray) {
        Color::Reset
    } else {
        color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let lines = to_lines("hello\nworld");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "hello");
    }

    #[test]
    fn git_diff_colors_map() {
        // Green addition, red removal — git's default diff palette.
        let lines = to_lines("\u{1b}[32m+added\u{1b}[m\n\u{1b}[31m-removed\u{1b}[m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].to_string(), "+added");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn bold_headers_and_resets() {
        let lines = to_lines("\u{1b}[1mdiff --git a/x b/x\u{1b}[m plain");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].to_string(), "diff --git a/x b/x plain");
    }

    #[test]
    fn indexed_and_truecolor() {
        let lines = to_lines("\u{1b}[38;5;208morange\u{1b}[m \u{1b}[38;2;10;20;30mrgb\u{1b}[m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(lines[0].spans[1].style.fg, None);
        assert_eq!(lines[0].spans[2].style.fg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn colon_form_truecolor_with_and_without_colour_space_slot() {
        let lines = to_lines("\u{1b}[38:2::1:2:3ma\u{1b}[m\u{1b}[48:2:4:5:6mb\u{1b}[m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Rgb(4, 5, 6)));
        assert_eq!(lines[0].to_string(), "ab");
    }

    #[test]
    fn style_carries_across_lines_until_reset() {
        let lines = to_lines("\u{1b}[31mfirst\nsecond\u{1b}[m\nthird");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[0].style.fg, None);
    }

    #[test]
    fn strikethrough_and_underline_colour_parameters() {
        let lines = to_lines("\u{1b}[9mgone\u{1b}[29m \u{1b}[58;2;9;9;9;4mu\u{1b}[m");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(
            !lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        // 58;2;r;g;b consumed its three colour parameters; the trailing 4
        // still applied as underline.
        assert!(
            lines[0].spans[2]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(lines[0].to_string(), "gone u");
    }

    #[test]
    fn non_sgr_escapes_are_dropped() {
        let lines = to_lines("a\u{1b}[2Kb\u{1b}[Hc");
        assert_eq!(lines[0].to_string(), "abc");
    }

    #[test]
    fn private_modes_queries_and_charset_escapes_are_dropped_whole() {
        // ESC[?25l (hide cursor), ESC[c (DA1 — viu emits this), ESC[>4;2m
        // (private SGR-like), ESC(B (charset), ESC7 / ESC8 (save/restore).
        let lines =
            to_lines("a\u{1b}[?25lb\u{1b}[cc\u{1b}[>4;2md\u{1b}(Be\u{1b}7f\u{1b}8g\u{1b}[?1049h");
        assert_eq!(lines[0].to_string(), "abcdefg");
        assert!(lines[0].spans.iter().all(|s| s.style == Style::default()));
    }

    #[test]
    fn dcs_and_apc_payloads_are_consumed_through_st() {
        // The Kitty graphics query viu sends when stdout is not a terminal,
        // then a DCS sixel-ish payload, then real text.
        let lines = to_lines(
            "\u{1b}_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\u{1b}\\\u{1b}Pq#0;2;0;0;0#0~~\u{1b}\\\u{1b}[38;2;22;25;52m\u{1b}[48;2;24;26;54m▄\u{1b}[0m",
        );
        assert_eq!(lines[0].to_string(), "▄");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(22, 25, 52)));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Rgb(24, 26, 54)));
    }

    #[test]
    fn osc_hyperlinks_are_consumed_through_bel_or_st() {
        let bel =
            to_lines("Read \u{1b}]8;id=docs;https://example.com\u{7}the docs\u{1b}]8;;\u{7} now");
        assert_eq!(bel[0].to_string(), "Read the docs now");

        let st = to_lines(
            "Read \u{1b}]8;id=docs;https://example.com\u{1b}\\the docs\u{1b}]8;;\u{1b}\\ now",
        );
        assert_eq!(st[0].to_string(), "Read the docs now");
    }

    #[test]
    fn backspace_overstrike_renders_bold_and_underline() {
        let lines = to_lines("N\u{8}NAME _\u{8}u x\u{8}_ a\u{8}b");
        assert_eq!(lines[0].to_string(), "NAME u x b");
        let bold = &lines[0].spans[0];
        assert_eq!(bold.content, "N");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let underlined: Vec<_> = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(underlined, vec!["u", "x"]);
    }
}
