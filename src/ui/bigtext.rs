// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Three-row block typography for panel headers.
//!
//! A logo says *which* venue at a glance but not *what* it is; spelling the
//! venue out in large type does both, and it costs no more vertical space than
//! the header already has.
//!
//! Glyphs are stored as PIXEL GRIDS, not as pre-drawn characters. A terminal
//! cell holds two pixels (`▀` upper, `▄` lower, `█` both), so a pre-drawn font
//! can only ever move in whole cells — which left the letters sitting high in
//! the box with an empty band underneath. A bitmap can be shifted by a single
//! pixel, i.e. half a cell, which is exactly what it takes to centre a
//! five-pixel letter inside a six-pixel (three-row) box.

use ratatui::prelude::*;

/// A glyph as pixel rows, `#` set and anything else clear. Five rows tall.
type Glyph = &'static [&'static str];

const BLANK: Glyph = &["", "", "", "", ""];

/// Pixel rows of the glyph, top to bottom.
fn glyph(c: char) -> Glyph {
    match c.to_ascii_uppercase() {
        'A' => &[".##.", "#..#", "####", "#..#", "#..#"],
        'B' => &["###.", "#..#", "###.", "#..#", "###."],
        'C' => &[".###", "#...", "#...", "#...", ".###"],
        'D' => &["###.", "#..#", "#..#", "#..#", "###."],
        'E' => &["####", "#...", "###.", "#...", "####"],
        'F' => &["####", "#...", "###.", "#...", "#..."],
        'G' => &[".###", "#...", "#.##", "#..#", ".###"],
        'H' => &["#..#", "#..#", "####", "#..#", "#..#"],
        'I' => &["#", "#", "#", "#", "#"],
        'J' => &["...#", "...#", "...#", "#..#", ".##."],
        'K' => &["#..#", "#.#.", "##..", "#.#.", "#..#"],
        'L' => &["#...", "#...", "#...", "#...", "####"],
        'M' => &["#...#", "##.##", "#.#.#", "#...#", "#...#"],
        'N' => &["#...#", "##..#", "#.#.#", "#..##", "#...#"],
        'O' => &[".##.", "#..#", "#..#", "#..#", ".##."],
        'P' => &["###.", "#..#", "###.", "#...", "#..."],
        'Q' => &[".##.", "#..#", "#..#", "#.#.", ".#.#"],
        'R' => &["###.", "#..#", "###.", "#.#.", "#..#"],
        'S' => &[".###", "#...", ".##.", "...#", "###."],
        'T' => &["#####", "..#..", "..#..", "..#..", "..#.."],
        'U' => &["#..#", "#..#", "#..#", "#..#", ".##."],
        'V' => &["#...#", "#...#", "#...#", ".#.#.", "..#.."],
        'W' => &["#...#", "#...#", "#.#.#", "##.##", "#...#"],
        'X' => &["#...#", ".#.#.", "..#..", ".#.#.", "#...#"],
        'Y' => &["#...#", ".#.#.", "..#..", "..#..", "..#.."],
        'Z' => &["####", "...#", "..#.", ".#..", "####"],
        '0' => &[".##.", "#..#", "#..#", "#..#", ".##."],
        '1' => &[".#.", "##.", ".#.", ".#.", "###"],
        '2' => &["###.", "...#", ".##.", "#...", "####"],
        '3' => &["###.", "...#", ".##.", "...#", "###."],
        '4' => &["#..#", "#..#", "####", "...#", "...#"],
        '5' => &["####", "#...", "###.", "...#", "###."],
        '6' => &[".###", "#...", "###.", "#..#", ".##."],
        '7' => &["####", "...#", "..#.", ".#..", ".#.."],
        '8' => &[".##.", "#..#", ".##.", "#..#", ".##."],
        '9' => &[".##.", "#..#", ".###", "...#", "###."],
        '.' => &[".", ".", ".", ".", "#"],
        ' ' => &["  ", "  ", "  ", "  ", "  "],
        _ => BLANK,
    }
}

/// Pixel rows in the rendered box: three cells, two pixels each.
const BOX_ROWS: usize = 6;
/// Glyph height in pixels.
const GLYPH_ROWS: usize = 5;
/// Blank pixel rows above the glyph. One row of six, with the glyph's five
/// filling the rest, puts the letter half a cell lower than a pre-drawn font
/// could manage — which is the whole reason for the bitmap.
const TOP_PAD: usize = 1;

fn glyph_width(c: char) -> usize {
    glyph(c).iter().map(|r| r.chars().count()).max().unwrap_or(0)
}

/// Whether the pixel at (`x`, `py`) of this glyph is set, where `py` indexes the
/// six-row box rather than the glyph itself.
fn lit(c: char, x: usize, py: usize) -> bool {
    if py < TOP_PAD || py >= TOP_PAD + GLYPH_ROWS {
        return false;
    }
    glyph(c)
        .get(py - TOP_PAD)
        .and_then(|row| row.chars().nth(x))
        .map(|p| p == '#')
        .unwrap_or(false)
}

/// Width in cells the text will occupy, so a caller can decide whether it fits
/// before rendering it.
pub fn width(text: &str) -> u16 {
    let n: usize = text.chars().map(|c| glyph_width(c) + 1).sum();
    n.saturating_sub(1) as u16
}

/// Render `text` as three styled lines.
pub fn render(text: &str, style: Style) -> Vec<Line<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut lines = Vec::with_capacity(BOX_ROWS / 2);
    for cell_row in 0..BOX_ROWS / 2 {
        let (top_py, bot_py) = (cell_row * 2, cell_row * 2 + 1);
        let mut row = String::new();
        for (i, c) in chars.iter().enumerate() {
            if i > 0 {
                row.push(' ');
            }
            for x in 0..glyph_width(*c) {
                row.push(match (lit(*c, x, top_py), lit(*c, x, bot_py)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
        }
        lines.push(Line::from(Span::styled(row, style)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_is_five_pixel_rows() {
        for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789. ".chars() {
            assert_eq!(glyph(c).len(), GLYPH_ROWS, "{c:?} is not five rows");
        }
    }

    #[test]
    fn a_five_row_glyph_is_centred_in_a_six_row_box() {
        // This is the point of the bitmap: one blank pixel row above and none
        // below, so the letter sits half a cell lower than a pre-drawn font
        // could place it.
        assert_eq!(TOP_PAD + GLYPH_ROWS, BOX_ROWS);
        // The very top pixel row is blank …
        assert!(!lit('A', 0, 0) && !lit('A', 1, 0));
        // … and the glyph's own first row lands on the row below it.
        assert!(lit('A', 1, TOP_PAD), "glyph should start one pixel down");
    }

    #[test]
    fn the_baseline_reaches_the_bottom_of_the_box() {
        // 'L' has a full-width foot; it must occupy the LAST pixel row, so no
        // empty band is left under the text.
        assert!(lit('L', 0, BOX_ROWS - 1));
        assert!(lit('L', 3, BOX_ROWS - 1));
    }

    #[test]
    fn render_produces_three_aligned_lines() {
        let lines = render("PONS", Style::default());
        assert_eq!(lines.len(), 3);
        let w: Vec<usize> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.chars().count()).sum())
            .collect();
        assert!(w.iter().all(|x| *x == w[0]), "lines are ragged: {w:?}");
        assert_eq!(w[0], width("PONS") as usize);
    }

    #[test]
    fn unknown_characters_render_blank_rather_than_panicking() {
        // Venue names are display text; a stray character must not be fatal.
        assert_eq!(render("A→B", Style::default()).len(), 3);
    }

    #[test]
    fn width_matches_what_a_header_can_fit() {
        for name in ["UNISWAP V3", "UNISWAP V4", "PONS", "FLAUNCH", "ROBINHOOD", "SOLANA", "PUMP.FUN"] {
            assert!(width(name) > 0);
            assert!(width(name) < 70, "{name} is too wide: {}", width(name));
        }
    }
}
