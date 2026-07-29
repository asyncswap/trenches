// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Chain logos, drawn as half-block pixel art.
//!
//! Ghostty (and most modern terminals) can display real images via the Kitty
//! graphics protocol, but a protocol image sits OUTSIDE ratatui's cell model:
//! it has to be re-emitted on every redraw, it doesn't clip to widget bounds,
//! and it leaves artifacts on a dashboard that repaints several times a second.
//!
//! Half-blocks avoid all of that. `▀` with a foreground and background colour
//! packs two vertical pixels into one cell, so a logo is just styled text —
//! themable, clippable, and free to redraw. The cost is resolution: a logo is
//! twice as wide as it is tall in cells.
//!
//! Logos are authored as character grids so they can be edited by hand: each
//! character is a key into the sprite's palette, and `.` is transparent.

use ratatui::prelude::*;

/// A pixel logo: a grid of palette keys plus the colours they map to.
pub struct Logo {
    /// Rows of palette keys. `.` means transparent.
    pub rows: &'static [&'static str],
    /// `(key, r, g, b)` for every non-transparent character.
    pub palette: &'static [(char, u8, u8, u8)],
}

impl Logo {
    fn color(&self, key: char) -> Option<Color> {
        self.palette
            .iter()
            .find(|(c, ..)| *c == key)
            .map(|(_, r, g, b)| Color::Rgb(*r, *g, *b))
    }

    fn pixel(&self, x: usize, y: usize) -> Option<Color> {
        let row = self.rows.get(y)?;
        let key = row.chars().nth(x)?;
        if key == '.' {
            return None;
        }
        self.color(key)
    }

    /// Width in terminal cells (one cell per pixel column).
    pub fn width(&self) -> u16 {
        self.rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16
    }

    /// Height in terminal cells — two pixel rows per cell, rounded up.
    pub fn height(&self) -> u16 {
        self.rows.len().div_ceil(2) as u16
    }

    /// Render sampled to fit a `cells_w` x `cells_h` box.
    ///
    /// This is an ordinary ratatui widget: styled cells that clip, scroll and
    /// vanish with the widget that owns them. That is the whole reason it
    /// exists — a graphics-protocol image cannot do any of those things, so
    /// using one on a surface that repaints constantly means hand-managing its
    /// lifetime. Here the framework does it.
    pub fn render_fit(&self, cells_w: u16, cells_h: u16) -> Vec<Line<'static>> {
        let (sw, sh) = (self.width() as usize, self.rows.len());
        if sw == 0 || sh == 0 || cells_w == 0 || cells_h == 0 {
            return Vec::new();
        }
        let (pw, ph) = (cells_w as usize, cells_h as usize * 2);
        let mut lines = Vec::with_capacity(cells_h as usize);
        for cy in 0..cells_h as usize {
            let mut spans = Vec::with_capacity(pw);
            for cx in 0..pw {
                let sx = cx * sw / pw;
                // Nearest-neighbour: the artwork is flat colour, so averaging
                // would only muddy it.
                let top = self.pixel(sx, (cy * 2) * sh / ph);
                let bot = self.pixel(sx, (cy * 2 + 1) * sh / ph);
                let mut style = Style::default();
                if let Some(c) = top {
                    style = style.fg(c);
                }
                if let Some(c) = bot {
                    style = style.bg(c);
                }
                spans.push(Span::styled(if top.is_some() { "▀" } else { " " }, style));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    /// Render to lines of `▀`, where the glyph's foreground is the upper pixel
    /// and its background the lower one.
    ///
    /// A transparent pixel leaves that half unset so the panel behind shows
    /// through — which is what keeps the logo theme-aware instead of punching a
    /// black rectangle into a light theme.
    pub fn render(&self) -> Vec<Line<'static>> {
        let w = self.width() as usize;
        let mut lines = Vec::with_capacity(self.height() as usize);
        for cell_y in 0..self.height() as usize {
            let (top_y, bot_y) = (cell_y * 2, cell_y * 2 + 1);
            let mut spans = Vec::with_capacity(w);
            for x in 0..w {
                let top = self.pixel(x, top_y);
                let bot = self.pixel(x, bot_y);
                let mut style = Style::default();
                if let Some(c) = top {
                    style = style.fg(c);
                }
                if let Some(c) = bot {
                    style = style.bg(c);
                }
                // With no upper pixel there is nothing to draw in the glyph, so
                // emit a space and let the background carry the lower pixel.
                let glyph = if top.is_some() { "▀" } else { " " };
                spans.push(Span::styled(glyph, style));
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

/// Robinhood's feather symbol, converted from their official
/// `RH_symbol_neon.svg` — a single flat neon, so one palette entry.
pub const ROBINHOOD: Logo = Logo {
    rows: &[
        "...............AAAAAAAA.",
        ".............AAAAAAAAAAA",
        "............AAAAAAAAAAAA",
        "...........AAAAAAAAAAAAA",
        "..................AAAAAA",
        ".........AAAAAA...AAAAAA",
        ".......AAAAAAAA...AAAAA.",
        "......AAAAAAA..AA.AAAA..",
        ".....AAAAAAA..AAA.AAA...",
        "....AAAAAAA..AAAA.AA....",
        "...AAAAAAA..AAAAA.A.....",
        "...AAAAAAA.AAAAAA.......",
        "...AAAAAA..AAAAAA.......",
        "...AAAAA..AAAAAAA.......",
        "...AAAA..AAAAAAA........",
        "...AAA..AAAAAAA.........",
        "...AAA.AAAAAAA..........",
        "...AA..AAAAAA...........",
        "..AA..AAA...............",
        "..AA....................",
        ".AA.....................",
        ".AA.....................",
        "AA......................",
        "A.......................",
    ],
    palette: &[
        ('A', 0xCC, 0xFF, 0x00),
    ],
};

/// Pons' mark, from the logo in the project's assets.
pub const PONS: Logo = Logo {
    rows: &[
        "........................",
        "........................",
        ".......AABBBCCC.A.......",
        "......CBBBBBBBBB.A......",
        ".....ABBBBBBBBBBBBA.....",
        ".....ABBBBBBBBCCBBC.....",
        ".....CBBCCCCCCCCCAA.....",
        ".....CBBCCCCCCAAAAA.....",
        ".....ABCCCAAAAAAAAA.....",
        ".....ABCAAAAAAAAAAA.....",
        ".....ABAAAAAAAAAAAA.....",
        ".....ABAAAAAAAAAAAA.....",
        ".....ACAAAAAAAAAAAA.....",
        ".....ACAAAACAAAAAA......",
        ".....ACAAAAA............",
        ".....ACAAAAA............",
        ".....AAAAAAA............",
        ".....AAAAAAA............",
        ".....AAAAAAA............",
        ".....AAAAAAA............",
        "......AAAAAA............",
        ".......AAAA.............",
        "........................",
        "........................",
    ],
    palette: &[
        ('A', 0xAB, 0xB5, 0xAA),
        ('B', 0xD4, 0xD9, 0xD3),
        ('C', 0xD1, 0xD6, 0xD0),
    ],
};

/// Uniswap's icon, from their official `Uniswap_icon_pink.svg`.
pub const UNISWAP: Logo = Logo {
    rows: &[
        "........................",
        "........................",
        "..A....A................",
        "...A...AA..A............",
        "....AA.AAA..AAAAAA.A....",
        ".....AA.AA...AAAAAAA....",
        ".......AAA....AAAAAA....",
        ".......A.......AAAA.AA..",
        "....AAAA......AA.....A..",
        "....A.........AAAAAAA...",
        "...AA...AA.....AAAAAAAA.",
        "...AA............AAAAAAA",
        "...A...............AAAAA",
        "..AA.............AAA.AAA",
        ".AAA............AAAAA..A",
        "AAAA...........A..AAAA..",
        "AAAAAA......AA.....AAAA.",
        "AAAAAA.............AAAA.",
        ".AAAAA..AAAAA.......AAA.",
        ".AAAA.AAAAAAAA......AAA.",
        ".......AAA...AA......A..",
        ".........A...AA.........",
        "..............A.........",
        "...............A........",
    ],
    palette: &[
        ('A', 0xF5, 0x0D, 0xB4),
    ],
};

/// The pump.fun logomark, from `pump-logomark.svg`.
pub const PUMP_FUN: Logo = Logo {
    rows: &[
        "..............BBBBBB....",
        "............BBBAAABBB...",
        "...........BBAAAAAAABB..",
        "..........BBAAAAAAAAABB.",
        ".........BBAAAAAAAAAABB.",
        "........BBAAAAAAAAAAAABB",
        ".......BBAAAAAAAAAAAAABB",
        "......BBBAAAAAAAAAAAAABB",
        ".....BBBBAAAAAAAAAAAAABB",
        ".....BBBBBBAAAAAAAAAABB.",
        "....BBBBBBBBAAAAAAAAABB.",
        "...BBBBBBBBBBAAAAAAABB..",
        "..BBBBBBBBBBBBAAAAABB...",
        ".BBBBBBBBBBBBBBAAABB....",
        ".BBBBBBBBBBBBBBBABB.....",
        "BBBBBBBBBBBBBBBBBBB.....",
        "BBBBBBBBBBBBBBBBBB......",
        "BBBBBBBBBBBBBBBBB.......",
        "BBBBBBBBBBBBBBBB........",
        ".BBBBBBBBBBBBBB.........",
        ".BBBBBBBBBBBBB..........",
        "..BBBBBBBBBBB...........",
        "...BBBBBBBBB............",
        "....BBBBBB..............",
    ],
    palette: &[
        ('A', 0xFF, 0xFF, 0xFF),
        ('B', 0x5F, 0xCB, 0x88),
    ],
};

/// Solana's logo mark, converted from the official `solanaLogoMark.svg`.
pub const SOLANA: Logo = Logo {
    rows: &[
        "....TTTTGGGGGGGGGGGGGGGG",
        "...TTTTTTGGGGGGGGGGGGGGG",
        "..TTTTTTTTGGGGGGGGGGGGG.",
        ".TTTTTTTTTTGGGGGGGGGGG..",
        "TTTTTTTTTTTTGGGGGGGGG...",
        "TTTTTTTTTTTTTGGGGGGG....",
        "........................",
        "........................",
        "........................",
        "PPPPTTTTTTTTTTTTTGGG....",
        "PPPPPTTTTTTTTTTTTTGGG...",
        ".PPPPPTTTTTTTTTTTTTGGG..",
        "..PPPPPTTTTTTTTTTTTTGGG.",
        "...PPPPPTTTTTTTTTTTTTGGG",
        "....PPPPPTTTTTTTTTTTTTGG",
        "........................",
        "........................",
        "........................",
        "....PPPPPPPPPTTTTTTTTTTT",
        "...PPPPPPPPPPPTTTTTTTTTT",
        "..PPPPPPPPPPPPPTTTTTTTT.",
        ".PPPPPPPPPPPPPPPTTTTTT..",
        "PPPPPPPPPPPPPPPPPTTTT...",
        "PPPPPPPPPPPPPPPPPPTT....",
    ],
    palette: &[
        ('P', 0x99, 0x45, 0xFF),
        ('T', 0x51, 0x9B, 0xC8),
        ('G', 0x14, 0xF1, 0x95),
    ],
};

/// Block art for a venue, falling back to the network's own mark. Mirrors
/// `image::for_venue`, but as a widget rather than a graphics placement.
pub fn for_venue(venue: crate::ui::image::Venue, network: &str) -> Option<&'static Logo> {
    use crate::ui::image::Venue;
    match venue {
        Venue::PumpFun => Some(&PUMP_FUN),
        Venue::Pons => Some(&PONS),
        Venue::Uniswap => Some(&UNISWAP),
        Venue::Chain => for_network(network),
    }
}

/// The logo for a network, matched on its registry name.
///
/// `None` for anything unbranded — a local dev node has no logo, and borrowing
/// the nearest one would misrepresent what you are connected to.
pub fn for_network(name: &str) -> Option<&'static Logo> {
    let n = name.to_lowercase();
    if n.starts_with("solana") {
        Some(&SOLANA)
    } else if n.starts_with("robinhood") {
        Some(&ROBINHOOD)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_two_pixels_per_cell() {
        assert_eq!(SOLANA.width(), 24);
        assert_eq!(SOLANA.height(), 12, "24 pixel rows pack into 12 cells");
        assert_eq!(SOLANA.render().len(), 12);
    }

    #[test]
    fn every_row_is_the_same_width() {
        // A short row would silently shift pixels left on that line.
        for logo in [&ROBINHOOD, &SOLANA] {
            let w = logo.width() as usize;
            for (i, r) in logo.rows.iter().enumerate() {
                assert_eq!(r.chars().count(), w, "row {i} is ragged");
            }
        }
    }

    #[test]
    fn every_key_used_has_a_colour() {
        // A missing palette entry renders as an invisible hole, which is much
        // harder to spot than a compile error.
        for logo in [&ROBINHOOD, &SOLANA] {
            for r in logo.rows {
                for c in r.chars().filter(|c| *c != '.') {
                    assert!(logo.color(c).is_some(), "no palette entry for {c:?}");
                }
            }
        }
    }

    #[test]
    fn transparent_pixels_leave_the_background_alone() {
        // The top-left corner is transparent in both halves, so that cell must
        // set neither colour — otherwise a light theme gets a black notch.
        let lines = SOLANA.render();
        let first = &lines[0].spans[0];
        assert_eq!(first.content, " ");
        assert!(first.style.fg.is_none() && first.style.bg.is_none());
    }

    #[test]
    fn render_fit_matches_the_box_it_is_given() {
        let l = SOLANA.render_fit(6, 3);
        assert_eq!(l.len(), 3, "one line per cell row");
        assert_eq!(l[0].spans.len(), 6, "one span per cell column");
        assert!(SOLANA.render_fit(0, 3).is_empty());
        assert!(SOLANA.render_fit(6, 0).is_empty());
    }

    #[test]
    fn only_branded_networks_get_a_logo() {
        assert!(for_network("solana-mainnet").is_some());
        assert!(for_network("robinhood-mainnet").is_some());
        assert!(for_network("robinhood-testnet").is_some());
        // A local dev node is EVM, but it is not Robinhood.
        // The dashboards pass the DISPLAY name, not the registry id — both
        // forms have to resolve or the header logo silently vanishes.
        assert!(for_network("Solana Mainnet").is_some());
        assert!(for_network("Robinhood Mainnet").is_some());
        assert!(for_network("anvil-local").is_none());
        assert!(for_network("Anvil Local").is_none());
        assert!(for_network("").is_none());
    }
}
