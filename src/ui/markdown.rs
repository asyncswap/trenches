// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Markdown rendered into themed ratatui lines, for in-app docs and tutorials.
//!
//! Modelled on steer's TUI renderer: parse with `pulldown-cmark` and map the
//! event stream onto styled spans, using THIS app's palette rather than
//! hardcoded colours, so docs follow the active theme like every other panel.
//!
//! Deliberately not mdfried's approach — that renders images, big headers and
//! mermaid via a terminal graphics stack, which is a viewer application in its
//! own right. Docs here live inside an existing panel and need to scroll, clip
//! and theme like ordinary text.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};
use ratatui::prelude::*;

use super::widgets::tone_color;
use crate::view::Tone;

/// Parse `md` into styled lines.
///
/// Inline styles nest, so the renderer keeps a small style stack rather than a
/// single "current style" — otherwise the end of a bold run inside a heading
/// would clear the heading's own styling too.
pub fn render(md: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut stack: Vec<Style> = Vec::new();
    let mut list_depth: usize = 0;
    let mut in_code_block = false;
    // An H1 is rendered in large block type, so its text has to be collected
    // whole before anything is emitted — the parser hands it over in pieces.
    let mut h1: Option<String> = None;

    let base = Style::default().fg(tone_color(Tone::Normal));
    let cur = |stack: &Vec<Style>| stack.last().copied().unwrap_or(base);

    let flush = |lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>| {
        lines.push(Line::from(std::mem::take(spans)));
    };

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::Heading(level, ..)) => {
                if !spans.is_empty() {
                    flush(&mut lines, &mut spans);
                }
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                // H1/H2 carry the accent; deeper headings just bold, so a long
                // document does not turn into a wall of colour.
                if level == HeadingLevel::H1 {
                    h1 = Some(String::new());
                }
                let tone = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => Tone::Accent,
                    _ => Tone::Info,
                };
                stack.push(Style::default().fg(tone_color(tone)).add_modifier(Modifier::BOLD));
            }
            Event::End(Tag::Heading(..)) => {
                let style = cur(&stack);
                stack.pop();
                match h1.take() {
                    // Large type, but only if it fits a normal terminal width —
                    // a long title would clip into unreadable blocks.
                    Some(t) if !t.is_empty() && super::bigtext::width(&t) <= 76 => {
                        spans.clear();
                        lines.extend(super::bigtext::render(&t, style));
                        lines.push(Line::from(""));
                    }
                    // Too long for block type: emit the buffered text plainly
                    // rather than dropping it on the floor.
                    Some(t) => {
                        spans.push(Span::styled(t, style));
                        flush(&mut lines, &mut spans);
                    }
                    None => flush(&mut lines, &mut spans),
                }
            }
            Event::Start(Tag::Strong) => {
                stack.push(cur(&stack).add_modifier(Modifier::BOLD));
            }
            Event::Start(Tag::Emphasis) => {
                stack.push(cur(&stack).add_modifier(Modifier::ITALIC));
            }
            Event::End(Tag::Strong) | Event::End(Tag::Emphasis) => {
                stack.pop();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                if !spans.is_empty() {
                    flush(&mut lines, &mut spans);
                }
                stack.push(Style::default().fg(tone_color(Tone::Good)));
            }
            Event::End(Tag::CodeBlock(_)) => {
                in_code_block = false;
                stack.pop();
                if !spans.is_empty() {
                    flush(&mut lines, &mut spans);
                }
            }
            Event::Start(Tag::List(_)) => list_depth += 1,
            Event::End(Tag::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                if list_depth == 0 {
                    lines.push(Line::from(""));
                }
            }
            Event::Start(Tag::Item) => {
                spans.push(Span::styled(
                    format!("{}• ", "  ".repeat(list_depth.saturating_sub(1))),
                    Style::default().fg(tone_color(Tone::Accent)),
                ));
            }
            Event::End(Tag::Item) => flush(&mut lines, &mut spans),
            Event::End(Tag::Paragraph) => {
                flush(&mut lines, &mut spans);
                lines.push(Line::from(""));
            }
            Event::Code(t) => {
                // Inline code shares the code-block colour so `b` in prose reads
                // the same as a key in a fenced block.
                spans.push(Span::styled(
                    t.to_string(),
                    Style::default().fg(tone_color(Tone::Good)),
                ));
            }
            Event::Text(t) => {
                if in_code_block {
                    // A fenced block arrives as text with its own newlines.
                    for (i, part) in t.split('\n').enumerate() {
                        if i > 0 {
                            flush(&mut lines, &mut spans);
                        }
                        spans.push(Span::styled(format!("  {part}"), cur(&stack)));
                    }
                } else if let Some(buf) = h1.as_mut() {
                    buf.push_str(&t);
                } else {
                    spans.push(Span::styled(t.to_string(), cur(&stack)));
                }
            }
            Event::SoftBreak => spans.push(Span::styled(" ", cur(&stack))),
            Event::HardBreak => flush(&mut lines, &mut spans),
            Event::Rule => {
                flush(&mut lines, &mut spans);
                lines.push(Line::from(Span::styled(
                    "─".repeat(60),
                    Style::default().fg(super::widgets::border_color()),
                )));
            }
            _ => {}
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_and_prose_survive_the_round_trip() {
        // H2 and below stay plain text; only H1 becomes block type.
        let out = render("## Title\n\nSome **bold** prose.\n");
        let t = text_of(&out);
        assert!(t.contains("Title"), "heading text lost: {t:?}");
        assert!(t.contains("bold"), "inline text lost: {t:?}");
    }

    #[test]
    fn nested_styles_do_not_clear_the_heading() {
        // The reason for a style stack: ending the bold run inside a heading
        // must restore the heading style, not the document default.
        let out = render("## Head **bold** tail\n");
        let head = &out[out.len() - 1];
        let tail = head.spans.last().expect("a trailing span");
        assert!(
            tail.style.add_modifier.contains(Modifier::BOLD),
            "heading styling was dropped after the nested span"
        );
    }

    #[test]
    fn an_h1_renders_as_large_type() {
        // Three rows of block glyphs, not one line of text.
        let out = render("# TRENCHES\n");
        let t = text_of(&out);
        assert!(t.contains('█') || t.contains('▀'), "H1 should be block type: {t:?}");
        assert!(!t.contains("TRENCHES"), "the literal text should be replaced by glyphs");
    }

    #[test]
    fn a_long_h1_falls_back_to_plain_text() {
        // Large type that does not fit would clip into unreadable blocks.
        let long = "# This heading is far too long to render as block glyphs\n";
        let t = text_of(&render(long));
        assert!(t.contains("far too long"), "expected plain text fallback: {t:?}");
    }

    #[test]
    fn list_items_become_their_own_lines() {
        let out = render("- one\n- two\n- three\n");
        let t = text_of(&out);
        assert_eq!(t.matches('•').count(), 3, "expected three bullets: {t:?}");
    }

    #[test]
    fn code_blocks_keep_their_line_breaks() {
        let out = render("```\na\nb\n```\n");
        let t = text_of(&out);
        assert!(t.contains("a") && t.contains("b"));
        assert!(t.lines().count() >= 2, "code block collapsed: {t:?}");
    }

    #[test]
    fn empty_input_is_not_a_panic() {
        assert!(render("").is_empty());
    }
}
