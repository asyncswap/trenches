// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Generic, chain-agnostic renderers. Everything here draws a `view::*` model
//! and knows nothing about EVM or Solana — that's what lets both chains share
//! one dashboard. The semantic `Tone` → palette mapping lives here too, so a
//! theme change is a single edit.
//!
//! Some components (`panel`, `Cursor`, `Nav`) are currently only used by the
//! Solana dashboard, so they read as dead code in a default (EVM-only) build.
//! They're part of the shared surface, not leftovers.
#![allow(dead_code)]

use crossterm::event::KeyCode;
use ratatui::{prelude::*, widgets::*};

use crate::view::{AxisView, Cell as VCell, PanelView, ScatterView, Shape, TableView, Tone, Width};


/// Map a `Tone` to the opaline semantic token that carries it.
fn token_for(t: Tone) -> &'static str {
    use opaline::names::tokens::*;
    match t {
        Tone::Normal => TEXT_PRIMARY,
        Tone::Dim => TEXT_MUTED,
        Tone::Good => SUCCESS,
        Tone::Bad => ERROR,
        Tone::Warn => WARNING,
        Tone::Info => INFO,
        Tone::Accent => ACCENT_PRIMARY,
        Tone::Mine => ACCENT_SECONDARY,
        Tone::Label => BORDER_FOCUSED,
    }
}

/// Terminal-default palette, used when no theme could be loaded.
fn fallback_color(t: Tone) -> Color {
    match t {
        Tone::Normal => Color::Gray,
        Tone::Dim => Color::DarkGray,
        Tone::Good => Color::Green,
        Tone::Bad => Color::Red,
        Tone::Warn => Color::Yellow,
        Tone::Info => Color::Cyan,
        Tone::Accent => Color::Magenta,
        Tone::Mine => Color::LightCyan,
        Tone::Label => Color::Cyan,
    }
}


// ---- contrast -------------------------------------------------------------
//
// A theme's own tokens are authored against ITS background. Dim text in
// particular is deliberately low-contrast, so on a light theme it can sit within
// a few percent luminance of the panel and become unreadable. Rather than
// hand-tuning per theme, every resolved tone is checked against the background
// and nudged until it clears a measured floor.

/// WCAG relative luminance (sRGB, gamma-corrected).
fn luminance(c: Color) -> Option<f64> {
    let Color::Rgb(r, g, b) = c else { return None };
    let lin = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    Some(0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b))
}

/// WCAG contrast ratio between two colours (1.0 = identical, 21.0 = max).
fn contrast(a: Color, b: Color) -> Option<f64> {
    let (la, lb) = (luminance(a)?, luminance(b)?);
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// Blend `c` toward `target` by `t` (0..1).
fn blend(c: Color, target: Color, t: f64) -> Color {
    let (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) = (c, target) else { return c };
    let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(mix(r, tr), mix(g, tg), mix(b, tb))
}

/// Minimum contrast a text colour must have against its background. Below WCAG
/// AA (4.5) on purpose: this preserves the theme's intended hierarchy — dim
/// should still read as dim — while guaranteeing it stays legible.
const MIN_CONTRAST: f64 = 3.2;

/// Push `fg` away from `bg` until it clears `MIN_CONTRAST`, moving toward black
/// on a light background and white on a dark one. Returns `fg` unchanged when it
/// already passes, so themes that were authored well are untouched.
/// Fit `fg` so it clears `MIN_CONTRAST` against EVERY background it may be drawn
/// on. Adjusting against them one at a time doesn't work — a later fix can undo
/// an earlier one (that regression collapsed kanagawa-lotus to 1.48). The
/// direction is chosen from `primary` (the panel) so a single move satisfies all.
fn ensure_contrast_all(fg: Color, backgrounds: &[Color], primary: Color) -> Color {
    let passes = |c: Color| {
        backgrounds
            .iter()
            .all(|bg| contrast(c, *bg).map_or(true, |r| r >= MIN_CONTRAST))
    };
    if passes(fg) {
        return fg;
    }
    let target = if luminance(primary).unwrap_or(0.0) > 0.5 {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(255, 255, 255)
    };
    let mut best = fg;
    for i in 1..=20 {
        let candidate = blend(fg, target, i as f64 / 20.0);
        best = candidate;
        if passes(candidate) {
            break;
        }
    }
    best
}

fn ensure_contrast(fg: Color, bg: Color) -> Color {
    let Some(current) = contrast(fg, bg) else { return fg };
    if current >= MIN_CONTRAST {
        return fg;
    }
    // Move toward whichever extreme is further from the background.
    let bg_lum = luminance(bg).unwrap_or(0.0);
    let target = if bg_lum > 0.5 { Color::Rgb(0, 0, 0) } else { Color::Rgb(255, 255, 255) };
    let mut best = fg;
    // Walk in small steps and stop at the FIRST passing blend, so the colour
    // stays as close to the theme author's intent as legibility allows.
    for i in 1..=20 {
        let candidate = blend(fg, target, i as f64 / 20.0);
        best = candidate;
        if contrast(candidate, bg).is_some_and(|c| c >= MIN_CONTRAST) {
            break;
        }
    }
    best
}

/// How visibly different a row background must be from the panel, as a
/// normalised RGB distance (0..1).
///
/// This has to carry the whole "this row is mine" signal: ratatui lets each CELL
/// set its own foreground, and those override the row-level style, so tinting
/// the row's text does nothing in the tape (every cell is already coloured by
/// venue/action). Only the background survives.
const MIN_BG_DISTANCE: f64 = 0.20;

/// Perceptual-ish RGB distance, normalised to 0..1. Green is weighted highest
/// because the eye is most sensitive to it. Used instead of WCAG contrast for
/// backgrounds: a tint can be obviously different (hue shift) while sitting at
/// almost the same luminance, which a contrast ratio would score as identical.
fn color_distance(a: Color, b: Color) -> Option<f64> {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) else { return None };
    let (dr, dg, db) = (
        (r1 as f64 - r2 as f64) / 255.0,
        (g1 as f64 - g2 as f64) / 255.0,
        (b1 as f64 - b2 as f64) / 255.0,
    );
    Some(((2.0 * dr * dr + 4.0 * dg * dg + 3.0 * db * db) / 9.0).sqrt())
}

/// Push a background further from the panel while KEEPING its hue, so a theme's
/// own choice is strengthened rather than replaced. Direction follows the panel:
/// darker on a light theme, lighter on a dark one.
fn strengthen_bg(bg: Color, panel: Color) -> Color {
    let target = if luminance(panel).unwrap_or(0.0) > 0.5 {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(255, 255, 255)
    };
    let mut best = bg;
    for i in 1..=14 {
        let candidate = blend(bg, target, i as f64 * 0.06);
        best = candidate;
        if color_distance(candidate, panel).is_some_and(|d| d >= MIN_BG_DISTANCE) {
            break;
        }
    }
    best
}

/// Row background for OUR trades: the panel tinted toward the theme's WARNING
/// colour — the same yellow/amber the tape uses for "pending".
///
/// Every theme defines `warning` as a yellow-ish hue, so this gives the warm
/// yellow highlight that reads best across all 39, while still being each
/// theme's own yellow rather than one hardcoded value. Warm also separates it
/// cleanly from the cool accents themes use for selection/info.
///
/// Blends only as far as it takes to clear `MIN_BG_DISTANCE`, so the band pops
/// without becoming saturated enough to drown the cell colours drawn on it.
fn warn_highlight(panel: Color, warn: Color) -> Color {
    let mut best = blend(panel, warn, 0.08);
    for i in 1..=16 {
        let candidate = blend(panel, warn, i as f64 * 0.05);
        best = candidate;
        if color_distance(candidate, panel).is_some_and(|d| d >= MIN_BG_DISTANCE) {
            break;
        }
    }
    best
}

/// Row styling from the theme's own `active_selected` style (falling back to
/// `selected`). Kept as the fallback when a theme has no usable warning colour.
///
/// Every opaline theme already defines what "this is the active row" looks like —
/// a matched fg/bg pair the author picked, e.g.
/// `active_selected = { fg = "accent.primary", bg = "bg.active", bold = true }`.
/// Using it gives a highlight that feels native to each of the 39 themes instead
/// of one hardcoded colour imposed on all of them, and it's automatically right
/// for light vs dark.
///
/// Returns `(background, foreground)`. The pair is accepted only if the
/// background is genuinely distinct from the panel; where a theme makes it a
/// whisper it's strengthened (hue kept) and the foreground refitted to match.
///
/// The row carries its OWN foreground rather than the per-cell colours: in
/// ratatui a cell's fg overrides the row's, and green-buy/red-sell would not
/// survive on a saturated band anyway.
fn selected_row_style(th: &opaline::Theme, panel: Color) -> (Color, Color) {
    let rgb = |c: opaline::OpalineColor| Color::Rgb(c.r, c.g, c.b);
    for name in [opaline::names::styles::ACTIVE_SELECTED, opaline::names::styles::SELECTED] {
        let Some(st) = th.try_style(name) else { continue };
        let (Some(fg), Some(bg)) = (st.fg, st.bg) else { continue };
        let (mut fg, mut bg) = (rgb(fg), rgb(bg));
        if color_distance(bg, panel).is_none_or(|d| d < MIN_BG_DISTANCE) {
            bg = strengthen_bg(bg, panel);
        }
        // The theme chose that fg for its ORIGINAL bg — refit if we moved it.
        fg = ensure_contrast(fg, bg);
        return (bg, fg);
    }
    // No selected style defined — fall back to a warm band.
    if luminance(panel).unwrap_or(0.0) > 0.5 {
        (Color::Rgb(146, 88, 0), Color::Rgb(255, 248, 232))
    } else {
        (Color::Rgb(214, 158, 46), Color::Rgb(28, 20, 4))
    }
}

/// Build a row background by tinting the panel with `accent`.
///
/// Tinting (rather than lightening/darkening) is what keeps this safe: it shifts
/// hue while staying near the panel's luminance, so text already fitted to the
/// panel stays readable on the highlight. Pushing toward black/white instead
/// breaks light themes — a darkened highlight collides with text that was
/// darkened for the light panel.
///
/// Starts from the theme's own `bg.highlight` and only tints further if that is
/// too close to the panel to notice.
fn tinted_bg(theme_bg: Color, panel: Color, accent: Color) -> Color {
    if color_distance(theme_bg, panel).is_some_and(|d| d >= MIN_BG_DISTANCE) {
        return theme_bg; // theme already provides a visible highlight
    }
    let mut best = theme_bg;
    for i in 1..=12 {
        let candidate = blend(panel, accent, i as f64 * 0.05);
        best = candidate;
        if color_distance(candidate, panel).is_some_and(|d| d >= MIN_BG_DISTANCE) {
            break;
        }
    }
    best
}

/// Every tone in a fixed order, so a resolved palette is a plain array.
const TONES: [Tone; 9] = [
    Tone::Normal, Tone::Dim, Tone::Good, Tone::Bad,
    Tone::Warn, Tone::Info, Tone::Accent, Tone::Mine, Tone::Label,
];

fn tone_index(t: Tone) -> usize {
    match t {
        Tone::Normal => 0,
        Tone::Dim => 1,
        Tone::Good => 2,
        Tone::Bad => 3,
        Tone::Warn => 4,
        Tone::Info => 5,
        Tone::Accent => 6,
        Tone::Mine => 7,
        Tone::Label => 8,
    }
}

/// A resolved palette. Tokens are looked up ONCE per theme change, never per
/// cell: `tone_color` runs thousands of times a frame, so it stays an index.
#[derive(Clone)]
struct Palette {
    name: String,
    tones: [Color; 9],
    bg_base: Color,
    bg_panel: Color,
    bg_highlight: Color,
    fg_highlight: Color,
    bg_selection: Color,
    border: Color,
    border_focused: Color,
}

impl Palette {
    /// Resolve a builtin theme by kebab-case id. `None` if unknown.
    fn load(name: &str) -> Option<Palette> {
        let th = opaline::builtins::load_by_name(name)?;
        let rgb = |c: opaline::OpalineColor| Color::Rgb(c.r, c.g, c.b);
        let mut tones = [Color::Reset; 9];
        for t in TONES {
            tones[tone_index(t)] = rgb(th.color(token_for(t)));
        }
        let bg = |token: &str, fb: Color| if th.has_token(token) { rgb(th.color(token)) } else { fb };
        use opaline::names::tokens as tk;
        let panel_bg = bg(tk::BG_PANEL, Color::Reset);
        // Settle the row backgrounds BEFORE fitting text to them, so the text
        // contrast pass targets the colours actually rendered.
        // Our-trade band: the theme's own WARNING yellow, tinted over the panel.
        // Native to each theme, reliably warm, and distinct from the cool
        // selection accent — so "mine" and "cursor" never look alike.
        let warn_raw = rgb(th.color(token_for(Tone::Warn)));
        let hl_bg = warn_highlight(panel_bg, warn_raw);
        let (_, hl_fg) = selected_row_style(&th, panel_bg);
        let sel_bg = tinted_bg(bg(tk::BG_SELECTION, Color::Rgb(40, 40, 70)), panel_bg, tones[tone_index(Tone::Info)]);
        // Text appears on the panel AND on selected/highlighted rows — enforce
        // the legibility floor against each.
        let surfaces = [panel_bg, sel_bg, hl_bg];
        for t in TONES {
            let i = tone_index(t);
            tones[i] = ensure_contrast_all(tones[i], &surfaces, panel_bg);
        }
        Some(Palette {
            name: name.to_string(),
            tones,
            bg_base: bg(tk::BG_BASE, Color::Reset),
            bg_panel: panel_bg,
            bg_highlight: hl_bg,
            fg_highlight: hl_fg,
            bg_selection: sel_bg,
            border: bg(tk::BORDER_UNFOCUSED, Color::DarkGray),
            border_focused: bg(tk::BORDER_FOCUSED, Color::Cyan),
        })
    }

    fn fallback() -> Palette {
        let mut tones = [Color::Reset; 9];
        for t in TONES {
            tones[tone_index(t)] = fallback_color(t);
        }
        Palette {
            name: "(none)".into(),
            tones,
            bg_base: Color::Reset,
            bg_panel: Color::Reset,
            bg_highlight: Color::Rgb(58, 46, 12),
            fg_highlight: Color::Rgb(255, 224, 130),
            bg_selection: Color::Rgb(40, 40, 70),
            border: Color::DarkGray,
            border_focused: Color::Cyan,
        }
    }
}

/// The live palette. `RwLock` so the picker can swap it mid-session; reads are
/// uncontended and cheap.
fn palette() -> &'static std::sync::RwLock<Palette> {
    static P: std::sync::OnceLock<std::sync::RwLock<Palette>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        // Startup precedence: saved choice, then BOT_THEME, then opaline default.
        let name = load_saved_theme()
            .or_else(|| std::env::var("BOT_THEME").ok())
            .unwrap_or_else(|| "default".into());
        let p = Palette::load(&name)
            .or_else(|| Palette::load("default"))
            .unwrap_or_else(Palette::fallback);
        std::sync::RwLock::new(p)
    })
}

/// Where the chosen theme is remembered.
///
/// Derived from `state_dir()`, not hardcoded. It used to be the literal
/// `.trenches/theme.txt`, relative to whatever directory the binary was
/// launched from — so the save created the real state directory and then wrote
/// the file somewhere else entirely, and the next launch from a different
/// directory found nothing. The theme silently reverted every time.
fn theme_path() -> std::path::PathBuf {
    std::path::Path::new(crate::state_dir()).join("theme.txt")
}

fn load_saved_theme() -> Option<String> {
    std::fs::read_to_string(theme_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// All builtin themes as `(id, display name)`, sorted by id.
pub fn theme_list() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = opaline::builtins::builtin_names()
        .iter()
        .map(|(id, disp)| ((*id).to_string(), (*disp).to_string()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The active theme's id.
pub fn current_theme() -> String {
    palette().read().map(|p| p.name.clone()).unwrap_or_default()
}

/// Switch themes live. Returns false (leaving the current theme intact) if the
/// name is not a known builtin. `persist` remembers it for the next launch.
pub fn set_theme(name: &str, persist: bool) -> bool {
    let Some(next) = Palette::load(name) else { return false };
    if let Ok(mut p) = palette().write() {
        *p = next;
    }
    if persist {
        let _ = std::fs::create_dir_all(crate::state_dir());
        let _ = std::fs::write(theme_path(), name);
    }
    true
}

/// Panel background (`bg.panel`).
pub fn bg_panel() -> Color {
    palette().read().map(|p| p.bg_panel).unwrap_or(Color::Reset)
}
/// Whole-app background (`bg.base`).
pub fn bg_base() -> Color {
    palette().read().map(|p| p.bg_base).unwrap_or(Color::Reset)
}
/// Frame colour for panels — the theme's `border.focused` token.
///
/// Every panel uses the SAME accented frame (the one the theme picker shows),
/// by preference: `border.unfocused` is often near-invisible, and the normal
/// text colour makes frames compete with content.
pub fn border_color() -> Color {
    palette().read().map(|p| p.border_focused).unwrap_or(Color::Cyan)
}
/// Frame colour for the focused/active panel.
pub fn border_focused_color() -> Color {
    palette().read().map(|p| p.border_focused).unwrap_or(Color::Cyan)
}

/// A bordered block carrying the theme's frame colour, panel background AND
/// default foreground.
///
/// The foreground matters as much as the background: most dashboard text is
/// `Span::raw(...)` with no explicit colour, which inherits whatever the buffer
/// already holds. Without setting `fg` here that's the TERMINAL's default — so
/// on a light theme the labels stayed terminal-light while the themed spans went
/// dark, and the two didn't match.
pub fn themed_block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.into())
        .border_style(Style::default().fg(border_color()))
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .style(Style::default().bg(bg_panel()).fg(tone_color(Tone::Normal)))
}

/// A themed block whose title is a styled `Line`, so a logo can sit inline with
/// the title text.
pub fn themed_block_line(title: Line<'static>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color()))
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .style(Style::default().bg(bg_panel()).fg(tone_color(Tone::Normal)))
}

/// Same, for the focused/active panel.
pub fn themed_block_focused(title: impl Into<String>) -> Block<'static> {
    themed_block(title).border_style(Style::default().fg(border_focused_color()))
}

/// Paint the whole frame with `bg.base` and the theme's default text colour.
/// Call FIRST in a draw pass — this is what unstyled `Span::raw` text inherits.
pub fn paint_bg(f: &mut Frame) {
    f.render_widget(
        Block::default().style(Style::default().bg(bg_base()).fg(tone_color(Tone::Normal))),
        f.area(),
    );
}

pub fn bg_highlight() -> Color {
    palette().read().map(|p| p.bg_highlight).unwrap_or(Color::Rgb(58, 46, 12))
}
pub fn fg_highlight() -> Color {
    palette().read().map(|p| p.fg_highlight).unwrap_or(Color::Rgb(255, 224, 130))
}
pub fn bg_selection() -> Color {
    palette().read().map(|p| p.bg_selection).unwrap_or(Color::Rgb(40, 40, 70))
}

/// The one place semantic tones become colours — themed via opaline, so every
/// panel and table on both chains restyles from a single token table.
pub fn tone_color(t: Tone) -> Color {
    palette().read().map(|p| p.tones[tone_index(t)]).unwrap_or_else(|_| fallback_color(t))
}

fn style_of(c: &VCell) -> Style {
    let s = Style::default().fg(tone_color(c.tone));
    if c.bold {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

fn span_of(c: &VCell) -> Span<'static> {
    Span::styled(c.text.clone(), style_of(c))
}

fn constraints_of(t: &TableView) -> Vec<Constraint> {
    t.cols
        .iter()
        .map(|c| match c.width {
            Width::Fixed(w) => Constraint::Length(w),
            Width::Min(w) => Constraint::Min(w),
        })
        .collect()
}

/// Row background for our own activity — the tape's ★ highlight, generalised.
fn mine_style() -> Style {
    Style::default().bg(bg_highlight()).add_modifier(Modifier::BOLD)
}

/// Draw a `TableView`. Pass `state` to show a selection cursor (discovery
/// screens); pass `None` for read-only tables (orders, tape).
/// The title, with a health light in front of it when the view asks for one.
///
/// Green answering, amber refusing some, red nothing getting through — read
/// off the shared RPC counters, which whichever chain is running feeds.
fn table_title(t: &TableView) -> Line<'static> {
    if !t.health {
        return Line::from(t.title.clone());
    }
    let tone = match crate::rpcstats::health() {
        crate::rpcstats::Health::Ok => Tone::Good,
        crate::rpcstats::Health::Degraded => Tone::Warn,
        crate::rpcstats::Health::Down => Tone::Bad,
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled("●", Style::default().fg(tone_color(tone))),
        Span::raw(t.title.clone()),
    ])
}

pub fn table(f: &mut Frame, area: Rect, t: &TableView, state: Option<&mut TableState>) {
    // Empty state gets the WHOLE area, centred — putting it in the first cell
    // truncates it to that column's width ("scanning pum").
    if t.rows.is_empty() {
        let block = themed_block_line(table_title(t));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let note = if t.empty_note.is_empty() { "nothing yet" } else { &t.empty_note };
        let mut lines = vec![Line::from("")];
        for (i, l) in note.split('\n').enumerate() {
            // First line carries the message; any others are a quieter hint.
            let tone = if i == 0 { Tone::Normal } else { Tone::Dim };
            lines.push(Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(tone_color(tone)).add_modifier(Modifier::ITALIC),
            )));
        }
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    }

    let header = Row::new(t.cols.iter().map(|c| c.title))
        .style(Style::default().fg(tone_color(Tone::Info)).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = {
        t.rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let row = Row::new(r.iter().map(span_of).collect::<Vec<_>>());
                if t.row_mine.get(i).copied().unwrap_or(false) {
                    // Only the background marks the row — cell colours (green
                    // buy / red sell) are kept, so it still reads as a trade.
                    row.style(mine_style())
                } else {
                    row
                }
            })
            .collect()
    };
    let block = themed_block_line(table_title(t));
    let widths = constraints_of(t);
    match state {
        Some(st) => {
            let w = Table::new(rows, widths)
                .header(header)
                .column_spacing(1)
                .row_highlight_style(
                    Style::default()
                        .bg(bg_selection())
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ")
                .block(block);
            f.render_stateful_widget(w, area, st);
        }
        None => {
            let w = Table::new(rows, widths).header(header).column_spacing(1).block(block);
            f.render_widget(w, area);
        }
    }
}

/// Draw a bordered text panel (market / wallet).
pub fn panel(f: &mut Frame, area: Rect, p: &PanelView) {
    let lines: Vec<Line> = p
        .lines
        .iter()
        .map(|cells| Line::from(cells.iter().map(span_of).collect::<Vec<_>>()))
        .collect();
    let w = Paragraph::new(lines).block(themed_block(p.title.clone()));
    f.render_widget(w, area);
}

// ---- scatter -------------------------------------------------------------

/// Radius, in terminal cells, of one plotted trade. ONE trade is ONE blob —
/// deliberately NOT scaled by size: the y-axis already encodes ETH, so scaling
/// the marker too was both redundant and misleading (a large trade rendered as a
/// cluster of points that read like many separate trades).
const DOT_RADIUS: i32 = 1;
/// Our own trades get a slightly larger hollow outline so they stand out.
const RING_RADIUS: i32 = 2;

/// Expand one datum into a small filled blob of fixed size. `cx`/`cy` are
/// data-units-per-cell, so it stays round rather than stretched by the axes.
fn expand_disc(x: f64, y: f64, cx: f64, cy: f64, out: &mut Vec<(f64, f64)>) {
    let r = DOT_RADIUS;
    for i in -r..=r {
        for j in -r..=r {
            if i * i + j * j <= r * r {
                out.push((x + i as f64 * cx, y + j as f64 * cy));
            }
        }
    }
}

/// Expand one datum into a hollow SQUARE outline of fixed size — used to pin our
/// own trades so they read as clearly not-solid over the market's filled dots.
fn expand_ring(x: f64, y: f64, cx: f64, cy: f64, out: &mut Vec<(f64, f64)>) {
    let r = RING_RADIUS;
    for i in -r..=r {
        for j in -r..=r {
            if i.abs() == r || j.abs() == r {
                out.push((x + i as f64 * cx, y + j as f64 * cy));
            }
        }
    }
}

fn axis(a: &AxisView) -> Axis<'static> {
    Axis::default()
        .title(a.title.clone())
        // 4% of headroom past the last tick. Bounded exactly at `max`, the
        // extreme points land on the frame and the end labels touch the corners.
        .bounds([0.0, a.max * 1.08])
        .labels(a.labels.iter().map(|s| Span::raw(s.clone())).collect::<Vec<_>>())
        .style(Style::default().fg(tone_color(Tone::Dim)))
}

/// Draw a scatter plot with a one-line key strip above it, so labels never sit
/// on top of the data. Disc/Ring series are size-scaled by their y magnitude.
pub fn scatter(f: &mut Frame, area: Rect, s: &ScatterView) {
    // Cap the plot height. Full-screen, a scatter is mostly empty space: the
    // data is a horizontal band and the extra rows add nothing but distance
    // between the axis labels and the points.
    const MAX_PLOT_H: u16 = 24;
    let area = Rect {
        height: area.height.min(MAX_PLOT_H + 1),
        ..area
    };
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(3)])
        .split(area);
    // A blank row under the key strip: it sat directly on the block's top rule,
    // which read as one crowded band of text.
    let (key_area, plot) = (rows[0], rows[2]);

    // Data units per terminal cell. What is subtracted is everything that is
    // NOT plot: 2 borders + 10+10 padding + the y-label gutter across, and
    // 2 borders + 2+1 padding + the x-label row down. Discs and rings are drawn
    // in data units, so if these drift the shapes come out as ellipses.
    let cx = s.x.max / (plot.width.saturating_sub(34).max(1)) as f64;
    let cy = s.y.max / (plot.height.saturating_sub(8).max(1)) as f64;

    // Expand every series first — Dataset borrows its data slice.
    let expanded: Vec<Vec<(f64, f64)>> = s
        .series
        .iter()
        .map(|se| {
            let mut v = Vec::new();
            for &(x, y) in &se.points {
                match se.shape {
                    Shape::Dot => v.push((x, y)),
                    Shape::Disc => expand_disc(x, y, cx, cy, &mut v),
                    Shape::Ring => expand_ring(x, y, cx, cy, &mut v),
                }
            }
            v
        })
        .collect();

    // No grid. A terminal has no hairlines: Braille marks read as data and half
    // blocks paint as bars, and either way the mesh competes with the points it
    // is meant to serve. The axis labels carry the scale on their own.
    let mut datasets: Vec<Dataset> = Vec::new();
    datasets.extend(s
        .series
        .iter()
        .zip(expanded.iter())
        .map(|(se, pts)| {
            let style = Style::default().fg(tone_color(se.tone));
            Dataset::default()
                .name(se.name.clone())
                // Braille packs 2x4 points into one cell, so dots are fine
                // rather than blocky and dense data does not smear together.
                // (Same choice bottom makes for its graphs.)
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Scatter)
                .style(if se.shape == Shape::Ring { style.add_modifier(Modifier::BOLD) } else { style })
                .data(pts)
        }));

    let chart = Chart::new(datasets)
        .block(themed_block(s.title.clone()).padding(ratatui::widgets::Padding::new(10, 10, 2, 1)))
        // The plot area is not covered by the block's own style, so without
        // this it shows the terminal's background rather than the theme's.
        .style(Style::default().bg(bg_panel()))
        .x_axis(axis(&s.x))
        .y_axis(axis(&s.y))
        .legend_position(None); // our own key strip is drawn above, outside the plot
    f.render_widget(chart, plot);

    // Key strip: one glyph per series (● filled, □ hollow) + an optional note.
    let mut spans: Vec<Span> = Vec::new();
    for se in &s.series {
        let glyph = if se.shape == Shape::Ring { "□" } else { "●" };
        spans.push(Span::styled(
            format!("  {glyph} {}", se.name),
            Style::default().fg(tone_color(se.tone)).add_modifier(Modifier::BOLD),
        ));
    }
    if !s.key_note.is_empty() {
        spans.push(Span::styled(format!("   · {}", s.key_note), Style::default().fg(tone_color(Tone::Dim))));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), key_area);
}

// ---- help overlay --------------------------------------------------------

/// Centred rect `w` percent wide and `h` rows tall.
pub fn centered_rect(area: Rect, pct_w: u16, h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Keybinding overlay. Items are `(section, "key|description")`; an item with a
/// non-empty section renders as a heading instead.
pub fn help(f: &mut Frame, items: &[(&str, &str)], title: &str) {
    let lines: Vec<Line> = items
        .iter()
        .map(|(section, kv)| {
            if !section.is_empty() {
                Line::from(Span::styled(
                    section.to_string(),
                    Style::default().fg(tone_color(Tone::Info)).add_modifier(Modifier::BOLD),
                ))
            } else {
                let (key, desc) = kv.split_once('|').unwrap_or(("", kv));
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{key:<9}"), Style::default().fg(tone_color(Tone::Warn)).add_modifier(Modifier::BOLD)),
                    Span::styled(desc.to_string(), Style::default().fg(tone_color(Tone::Normal))),
                ])
            }
        })
        .collect();
    let area = centered_rect(f.area(), 60, items.len() as u16 + 2);
    let p = Paragraph::new(lines).block(themed_block_focused(title.to_string()));
    f.render_widget(Clear, area);
    f.render_widget(p, area);
}

// ---- selection cursor ----------------------------------------------------

/// What a keypress meant to a list/table screen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Nav {
    /// Nothing relevant — keep looping.
    Idle,
    /// Selection moved.
    Moved,
    /// Confirm the highlighted row.
    Enter,
    /// Leave the screen.
    Back,
    /// Switch tab.
    Tab,
}

/// Selection state for a live-refreshing table: owns the index and clamps it as
/// rows arrive. Each screen keeps its own loop (so it can refresh data) and just
/// feeds keys through `on_key`, then renders with `state_for`.
#[derive(Default)]
pub struct Cursor {
    pub sel: usize,
    state: TableState,
}

impl Cursor {
    pub fn new() -> Cursor {
        Cursor::default()
    }
    /// Apply a keypress against a list of `len` rows.
    pub fn on_key(&mut self, code: KeyCode, len: usize) -> Nav {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                Nav::Moved
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    self.sel = (self.sel + 1).min(len - 1);
                }
                Nav::Moved
            }
            KeyCode::Enter => Nav::Enter,
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => Nav::Tab,
            KeyCode::Esc | KeyCode::Char('q') => Nav::Back,
            _ => Nav::Idle,
        }
    }
    /// Clamp to the current row count and hand back the ratatui state to render.
    pub fn state_for(&mut self, len: usize) -> &mut TableState {
        if len == 0 {
            self.sel = 0;
            self.state.select(None);
        } else {
            self.sel = self.sel.min(len - 1);
            self.state.select(Some(self.sel));
        }
        &mut self.state
    }
    /// Reset to the top (e.g. after switching tabs).
    pub fn reset(&mut self) {
        self.sel = 0;
        self.state.select(Some(0));
    }
}

/// Serialises tests that mutate the global theme — they share one palette, so
/// running them concurrently makes assertions read another test's theme.
#[cfg(test)]
static THEME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// Builtin themes load by KEBAB-case name. The underscore form matches the
    /// .toml filenames but does NOT resolve — documenting the wrong one would
    /// make BOT_THEME silently fall back to the default.
    #[test]
    fn builtin_themes_load_by_kebab_name() {
        for name in ["tokyo-night", "gruvbox-dark", "catppuccin-mocha", "nord", "dracula"] {
            assert!(opaline::builtins::load_by_name(name).is_some(), "{name} should load");
        }
        assert!(opaline::builtins::load_by_name("tokyo_night").is_none(), "underscores must not resolve");
        assert!(opaline::builtins::load_by_name("default").is_some(), "default must always resolve");
    }

    /// Every Tone must resolve to a token the theme actually defines, otherwise
    /// panels silently render in a fallback colour.
    #[test]
    fn every_tone_resolves_to_a_real_token() {
        let th = opaline::builtins::load_by_name("default").expect("default theme");
        for t in [
            Tone::Normal, Tone::Dim, Tone::Good, Tone::Bad,
            Tone::Warn, Tone::Info, Tone::Accent, Tone::Mine,
        ] {
            let token = token_for(t);
            assert!(th.has_token(token), "{t:?} -> '{token}' missing from theme");
        }
    }

    /// Distinct meanings must stay visually distinct — if success and error
    /// resolved to the same colour, a losing trade would look like a winning one.
    #[test]
    fn good_and_bad_are_distinguishable() {
        assert_ne!(tone_color(Tone::Good), tone_color(Tone::Bad));
        assert_ne!(tone_color(Tone::Normal), tone_color(Tone::Dim));
    }
}

// ---- theme picker --------------------------------------------------------

/// Interactive theme picker with LIVE preview: moving the cursor applies the
/// theme immediately, so the swatches and the surrounding chrome restyle as you
/// browse. Enter keeps it (and remembers it for next launch); Esc restores
/// whatever was active on entry.
///
/// Chain-agnostic, like everything else here — both dashboards call it.
pub fn theme_picker(term: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> eyre::Result<Option<String>> {
    // This screen owns the terminal now: take down any image the last one left.
    // Clearing also marks every placement stale, so the dashboard redraws its
    // logo when we return — no screen has to know about any other.
    crate::ui::image::clear();
    use crossterm::event::{self, Event};

    let themes = theme_list();
    if themes.is_empty() {
        return Ok(None);
    }
    let original = current_theme();
    // Start on the active theme so Esc is a genuine no-op.
    let mut sel = themes.iter().position(|(id, _)| *id == original).unwrap_or(0);
    let mut applied = original.clone();

    loop {
        // Apply as we move — this IS the preview.
        if themes[sel].0 != applied {
            set_theme(&themes[sel].0, false);
            applied = themes[sel].0.clone();
        }

        term.draw(|f| {
            // Repaint the WHOLE frame, not just the popup: picking a light theme
            // on a dark terminal must show the real result, background included.
            f.render_widget(Clear, f.area());
            paint_bg(f);
            let area = centered_rect(f.area(), 70, (themes.len().min(18) + 6) as u16);

            let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(area);

            // Scrolling window over the theme list.
            let h = rows[0].height.saturating_sub(2) as usize;
            let start = sel.saturating_sub(h.saturating_sub(1) / 2).min(themes.len().saturating_sub(h.max(1)));
            let items: Vec<Line> = themes
                .iter()
                .enumerate()
                .skip(start)
                .take(h)
                .map(|(i, (id, disp))| {
                    let picked = i == sel;
                    let marker = if picked { "▸ " } else { "  " };
                    let style = if picked {
                        Style::default().fg(tone_color(Tone::Info)).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(tone_color(Tone::Normal))
                    };
                    Line::from(vec![
                        Span::styled(format!("{marker}{id:<24}"), style),
                        Span::styled(disp.clone(), Style::default().fg(tone_color(Tone::Dim))),
                    ])
                })
                .collect();
            let list = Paragraph::new(items).block(
                themed_block_focused(format!(
                    " theme  {}/{}   ↑/↓ preview · enter keep · esc cancel ",
                    sel + 1,
                    themes.len()
                )),
            );
            f.render_widget(list, rows[0]);

            // Swatch row: every tone, so the effect on real UI colours is visible
            // rather than guessed from a name.
            let swatch = Paragraph::new(Line::from(vec![
                Span::styled(" ● normal", Style::default().fg(tone_color(Tone::Normal))),
                Span::styled("  ● dim", Style::default().fg(tone_color(Tone::Dim))),
                Span::styled("  ● buy", Style::default().fg(tone_color(Tone::Good)).add_modifier(Modifier::BOLD)),
                Span::styled("  ● sell", Style::default().fg(tone_color(Tone::Bad)).add_modifier(Modifier::BOLD)),
                Span::styled("  ● pending", Style::default().fg(tone_color(Tone::Warn))),
                Span::styled("  ● info", Style::default().fg(tone_color(Tone::Info))),
                Span::styled("  ● mode", Style::default().fg(tone_color(Tone::Accent))),
                Span::styled("  ● yours", Style::default().fg(tone_color(Tone::Mine)).add_modifier(Modifier::BOLD)),
            ]))
            .block(themed_block(" preview "));
            f.render_widget(swatch, rows[1]);
        })?;

        crate::ui_alive();

        if event::poll(std::time::Duration::from_millis(150))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => sel = if sel == 0 { themes.len() - 1 } else { sel - 1 },
                    KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1) % themes.len(),
                    KeyCode::Enter => {
                        set_theme(&themes[sel].0, true); // persist the choice
                        return Ok(Some(themes[sel].0.clone()));
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        set_theme(&original, false); // revert the preview
                        return Ok(None);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    #[test]
    fn theme_list_is_populated_and_loadable() {
        let list = theme_list();
        assert!(list.len() > 20, "expected ~39 builtins, got {}", list.len());
        // Every advertised id must actually load — a name in the menu that
        // fails to load would look like the picker is broken.
        for (id, disp) in &list {
            assert!(Palette::load(id).is_some(), "menu lists '{id}' but it will not load");
            assert!(!disp.is_empty(), "'{id}' has no display name");
        }
        assert!(list.iter().any(|(id, _)| id.starts_with("catppuccin")));
    }

    #[test]
    fn set_theme_switches_and_rejects_unknown() {
        let _guard = super::THEME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = current_theme();

        assert!(set_theme("catppuccin-mocha", false));
        assert_eq!(current_theme(), "catppuccin-mocha");
        let mocha_good = tone_color(Tone::Good);

        assert!(set_theme("gruvbox-dark", false));
        assert_eq!(current_theme(), "gruvbox-dark");
        assert_ne!(tone_color(Tone::Good), mocha_good, "themes must actually differ");

        // Unknown name must be a no-op, NOT a silent fallback to default —
        // otherwise a typo would quietly change the user's theme.
        assert!(!set_theme("catppuccin_mocha", false), "underscores must be rejected");
        assert_eq!(current_theme(), "gruvbox-dark", "failed load must leave theme intact");

        set_theme(&before, false);
    }
}

#[cfg(test)]
mod contrast_tests {
    use super::*;

    #[test]
    fn contrast_math_matches_known_values() {
        let black = Color::Rgb(0, 0, 0);
        let white = Color::Rgb(255, 255, 255);
        // Black on white is the maximum possible ratio, 21:1.
        let c = contrast(black, white).unwrap();
        assert!((c - 21.0).abs() < 0.01, "black/white should be 21:1, got {c}");
        // A colour against itself is 1:1.
        assert!((contrast(white, white).unwrap() - 1.0).abs() < 0.001);
    }

    /// The reported bug: dim text on a light panel was illegible. Every tone of
    /// every builtin theme must clear the floor against its own panel background.
    #[test]
    fn every_tone_is_legible_on_every_builtin_theme() {
        let mut checked = 0;
        let mut failures = Vec::new();
        for (id, _) in theme_list() {
            let Some(p) = Palette::load(&id) else { continue };
            // Skip themes that don't define a panel background — nothing to
            // measure against (they inherit the terminal's).
            if !matches!(p.bg_panel, Color::Rgb(..)) {
                continue;
            }
            for t in TONES {
                let fg = p.tones[tone_index(t)];
                let ratio = contrast(fg, p.bg_panel).unwrap_or(0.0);
                checked += 1;
                if ratio < MIN_CONTRAST - 0.01 {
                    failures.push(format!("{id}/{t:?} = {ratio:.2}"));
                }
            }
        }
        assert!(checked > 100, "expected to check many combinations, got {checked}");
        assert!(failures.is_empty(), "illegible tone/background pairs: {failures:?}");
    }

    /// Light themes are the ones that actually broke — assert they're covered
    /// and that the fix pushed text DARKER (toward black) rather than lighter.
    #[test]
    fn light_themes_get_dark_text() {
        for id in ["catppuccin-latte", "github-light", "gruvbox-light", "solarized-light"] {
            let Some(p) = Palette::load(id) else { continue };
            let bg_lum = luminance(p.bg_panel).unwrap_or(0.0);
            assert!(bg_lum > 0.4, "{id} should be a light theme, luminance {bg_lum}");
            let dim = p.tones[tone_index(Tone::Dim)];
            assert!(
                luminance(dim).unwrap_or(1.0) < bg_lum,
                "{id}: dim text must be darker than its light background"
            );
            assert!(contrast(dim, p.bg_panel).unwrap_or(0.0) >= MIN_CONTRAST - 0.01);
        }
    }

    /// Well-authored themes must be left alone — the floor is a safety net, not
    /// a restyle.
    #[test]
    fn already_legible_colours_are_untouched() {
        let bg = Color::Rgb(30, 30, 46); // dark panel
        let fg = Color::Rgb(205, 214, 244); // high-contrast light text
        assert_eq!(ensure_contrast(fg, bg), fg);
    }
}

#[cfg(test)]
mod inheritance_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// The reported bug: most dashboard labels are unstyled `Span::raw`, which
    /// inherit whatever the buffer holds. If the base layer doesn't set a
    /// foreground they fall through to the TERMINAL's default — so on a light
    /// theme the text stayed terminal-light while themed spans went dark, and
    /// the swatch preview didn't match the actual screen.
    #[test]
    fn unstyled_text_inherits_the_theme_foreground() {
        let _guard = super::THEME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_theme("light-owl", false);
        let expected = tone_color(Tone::Normal);
        let expected_bg = bg_panel();

        let mut term = Terminal::new(TestBackend::new(40, 6)).unwrap();
        term.draw(|f| {
            paint_bg(f);
            let p = Paragraph::new(Line::from(Span::raw("plain"))).block(themed_block(" t "));
            f.render_widget(p, f.area());
        })
        .unwrap();

        let buf = term.backend().buffer();
        // Find a cell belonging to the unstyled word.
        let cell = (0..40)
            .flat_map(|x| (0..6).map(move |y| (x, y)))
            .map(|(x, y)| buf[(x, y)].clone())
            .find(|c| c.symbol() == "p")
            .expect("rendered text");

        assert_eq!(cell.fg, expected, "unstyled text must take the theme's normal colour");
        assert_ne!(cell.fg, Color::Reset, "must not fall through to the terminal default");
        assert_eq!(cell.bg, expected_bg, "and sit on the themed panel background");
    }

    /// A light theme must actually produce dark text — the case that broke.
    #[test]
    fn light_theme_yields_dark_default_text() {
        let _guard = super::THEME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_theme("light-owl", false);
        let fg = luminance(tone_color(Tone::Normal)).unwrap_or(1.0);
        let bg = luminance(bg_panel()).unwrap_or(0.0);
        assert!(bg > 0.4, "light-owl should have a light panel, got {bg}");
        assert!(fg < bg, "normal text must be darker than the panel ({fg} vs {bg})");
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;

    /// Our own trades must be spottable at a glance on EVERY theme. Reported
    /// against solarized (invisible) and tokyo-night (too faint).
    #[test]
    fn own_trade_highlight_is_visible_on_every_theme() {
        let mut weak = Vec::new();
        for (id, _) in theme_list() {
            let Some(p) = Palette::load(&id) else { continue };
            if !matches!(p.bg_panel, Color::Rgb(..)) {
                continue;
            }
            let d = color_distance(p.bg_highlight, p.bg_panel).unwrap_or(0.0);
            if d < MIN_BG_DISTANCE - 0.005 {
                weak.push(format!("{id}={d:.3}"));
            }
        }
        assert!(weak.is_empty(), "highlight indistinguishable from panel on: {weak:?}");
    }

    /// The specific themes the user flagged.
    #[test]
    fn flagged_themes_are_fixed() {
        for id in ["solarized-light", "solarized-dark", "tokyo-night"] {
            let Some(p) = Palette::load(id) else { continue };
            let d = color_distance(p.bg_highlight, p.bg_panel).unwrap_or(0.0);
            assert!(d >= MIN_BG_DISTANCE - 0.005, "{id} highlight only {d:.3} from panel");
            // And text must stay readable ON that highlight.
            for t in TONES {
                let tc = contrast(p.tones[tone_index(t)], p.bg_highlight).unwrap_or(0.0);
                assert!(tc >= MIN_CONTRAST - 0.01, "{id}: {t:?} unreadable on highlight ({tc:.2})");
            }
        }
    }
}

#[cfg(test)]
mod amber_tests {
    use super::*;

    /// Our-trade rows must read as WARM (yellow/amber) on every theme — that's
    /// what makes them spottable at a glance, and it keeps them distinct from the
    /// cool accent used for the selection cursor.
    #[test]
    fn highlight_is_warm_and_distinct_on_every_theme() {
        let mut cold = Vec::new();
        let mut faint = Vec::new();
        for (id, _) in theme_list() {
            let Some(p) = Palette::load(&id) else { continue };
            let (Color::Rgb(r, _, b), Some(d)) =
                (p.bg_highlight, color_distance(p.bg_highlight, p.bg_panel))
            else {
                continue;
            };
            // Warm = red channel above blue (yellow/amber sits that way).
            if r <= b {
                cold.push(format!("{id} rgb r{r} b{b}"));
            }
            if d < MIN_BG_DISTANCE - 0.005 {
                faint.push(format!("{id}={d:.3}"));
            }
        }
        assert!(cold.is_empty(), "highlight not warm on: {cold:?}");
        assert!(faint.is_empty(), "highlight too faint on: {faint:?}");
    }

    /// "Mine" and "cursor" must never look alike — they mean different things.
    #[test]
    fn own_trade_band_differs_from_selection_cursor() {
        for id in ["catppuccin-mocha", "tokyo-night", "solarized-light", "light-owl"] {
            let Some(p) = Palette::load(id) else { continue };
            let d = color_distance(p.bg_highlight, p.bg_selection).unwrap_or(0.0);
            assert!(d > 0.05, "{id}: mine-band and cursor look the same ({d:.3})");
        }
    }
}


/// Re-export so selection screens can name `Logo` without reaching across
/// modules in their signatures.
pub mod logo_reexport {
    pub use crate::ui::logo::Logo;
}


#[cfg(test)]
mod table_padding_tests {
    use super::*;
    use crate::view::{Cell, Col, TableView};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// How many blank columns sit between the left border and the first header
    /// label. Anything beyond the marker column's own width is wasted space.
    fn left_gap(t: &TableView) -> usize {
        let mut term = Terminal::new(TestBackend::new(80, 6)).unwrap();
        term.draw(|f| table(f, f.area(), t, None)).unwrap();
        let buf = term.backend().buffer().clone();
        // Row 1 is the header (row 0 is the top border).
        let line: String = (0..80).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        let after_border = &line[line.char_indices().nth(1).unwrap().0..];
        after_border.chars().take_while(|c| *c == ' ').count()
    }

    /// Pins the left edge so it cannot creep. The marker column was 3 wide for
    /// a 2-wide star, which pushed every real column right by a cell.
    #[test]
    fn the_marker_column_costs_no_more_than_its_own_width() {
        let mut t = TableView::new(
            " Trades ".to_string(),
            vec![Col::fixed("", 2), Col::fixed("age", 6), Col::min("sig", 10)],
        );
        t.push(vec![Cell::new(""), Cell::new("3s"), Cell::new("0xabc")]);
        // The star is double-width, so 2 columns is the floor. More than that
        // is padding that pushes every real column right for no reason.
        // 2 for the double-width star, plus ratatui's single column gap. The
        // star cannot be narrower, so this is the floor for a leading marker.
        assert_eq!(left_gap(&t), 3, "left edge should be the marker column plus one gap");
    }
}
