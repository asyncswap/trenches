//! Real terminal images via the Kitty graphics protocol.
//!
//! An image is not a ratatui widget: it sits on its own layer, so drawing over
//! it does not erase it and it must be taken down explicitly.
//!
//! That is handled with a GENERATION counter rather than by callers tracking
//! screens. Any screen that takes over the terminal calls `clear()` once when
//! it starts, which bumps the generation; a `Placement` carries the generation
//! it drew under, so the next dashboard frame sees a stale key and re-draws
//! itself. Nothing has to know which screens exist, and a value change that
//! clears nothing leaves the key identical — so the image holds still instead
//! of blinking.
//!
//! Half-block art (`logo.rs`) works everywhere but is visibly pixelated. Where
//! the terminal supports it — Ghostty, Kitty, WezTerm — an actual PNG looks the
//! way a logo should, so it is used in preference and the block art stays as
//! the fallback.
//!
//! Images live on a layer OUTSIDE ratatui's cell grid: drawing over the region
//! does not erase them. So each placement is explicitly deleted before the next
//! one, and a placement is only re-emitted when what it shows actually changes
//! — re-sending every frame makes the image visibly flicker.

use std::io::Write;

/// Base64 without pulling in a crate. The alphabet is fixed and the payload is
/// a few kilobytes, so a table-free encoder is simpler than a dependency.
fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Whether this terminal can display graphics.
///
/// Detected from the environment rather than by querying the terminal: a
/// capability query has to be read back from stdin, which fights with the
/// TUI's own input loop. The cost of guessing wrong is only that a logo falls
/// back to block art.
pub fn supported() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some() || std::env::var_os("WEZTERM_PANE").is_some() {
        return true;
    }
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    ["ghostty", "wezterm", "kitty"].iter().any(|t| prog.contains(t) || term.contains(t))
}

/// Bumped by every `clear()`. A `Placement` that drew under an older value
/// knows its image is gone and re-draws without being told by whom.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Remove every image this program has placed.
///
/// Call this once at the start of any screen that takes over the terminal.
pub fn clear() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !supported() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b_Ga=d\x1b\\");
    let _ = out.flush();
}

/// Draw `png` into a `cols` x `rows` cell box whose top-left is at (`col`,
/// `row`), both 0-indexed. The image is scaled to fill that box.
pub fn place(png: &[u8], col: u16, row: u16, cols: u16, rows: u16) {
    if !supported() || cols == 0 || rows == 0 {
        return;
    }
    let mut out = std::io::stdout();
    // Terminal cursor addressing is 1-indexed.
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);

    // f=100: PNG payload. a=T: transmit and display. C=1: leave the cursor put,
    // so the placement can't scroll the view. m=1 marks "more chunks follow" —
    // the protocol caps each escape at 4096 base64 bytes.
    let payload = base64(png);
    let mut chunks = payload.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            let _ = write!(
                out,
                "\x1b_Ga=T,f=100,C=1,c={cols},r={rows},m={more};{}\x1b\\",
                std::str::from_utf8(chunk).unwrap_or("")
            );
            first = false;
        } else {
            let _ = write!(out, "\x1b_Gm={more};{}\x1b\\", std::str::from_utf8(chunk).unwrap_or(""));
        }
    }
    let _ = out.flush();
}

/// A single on-screen image, redrawn only when it actually changes.
///
/// The dashboard repaints many times a second. Re-emitting a graphics escape
/// each frame makes the image strobe, so the placement is compared against what
/// is already on screen — artwork identity plus geometry, since a resize moves
/// it — and re-sent only on a real change.
#[derive(Default)]
pub struct Placement {
    shown: Option<(usize, u16, u16, u16, u16, (u16, u16), u64)>,
}

impl Placement {
    /// Whether this placement currently has an image on screen.
    pub fn is_showing(&self) -> bool {
        self.shown.is_some()
    }

    /// Forget what is on screen without clearing, so the next `show` redraws.
    pub fn forget(&mut self) {
        self.shown = None;
    }

    /// Show `png` in the given cell box. `id` distinguishes artwork whose
    /// geometry is identical (switching chains, say).
    ///
    /// `term` is the terminal's current size. A resize repaints the whole
    /// screen and drops placed images, but the logo box sits at the top-left so
    /// its own coordinates do not change — without the terminal size in the key
    /// the placement would look unchanged and never be re-emitted, leaving the
    /// header blank until something else forced it.
    pub fn show(&mut self, png: &'static [u8], id: usize, x: u16, y: u16, w: u16, h: u16, term: (u16, u16)) {
        let key = (id, x, y, w, h, term, generation());
        if self.shown == Some(key) || !supported() {
            return;
        }
        // Take down our own previous placement, then draw. This bumps the
        // generation, so the key is recomputed after the clear.
        clear();
        place(png, x, y, w, h);
        self.shown = Some((id, x, y, w, h, term, generation()));
    }
}

/// A square logo box at the left of a header, and the text offset that clears
/// it. Sized from the header's inner height so it stays square: cells are about
/// twice as tall as wide, so a `h`-row square needs `2h` columns.
pub fn header_box(inner: ratatui::layout::Rect) -> (ratatui::layout::Rect, u16) {
    let side = inner.height.min(3);
    let cols = side * 2;
    (
        ratatui::layout::Rect { x: inner.x, y: inner.y, width: cols, height: side },
        cols + 1,
    )
}

/// Logo artwork for each chain, embedded so the binary stays self-contained.
pub const ROBINHOOD_PNG: &[u8] = include_bytes!("../../assets/robinhood.png");
pub const SOLANA_PNG: &[u8] = include_bytes!("../../assets/solana.png");
pub const PUMP_PNG: &[u8] = include_bytes!("../../assets/pump.png");
pub const PONS_PNG: &[u8] = include_bytes!("../../assets/pons.png");
pub const UNISWAP_PNG: &[u8] = include_bytes!("../../assets/uniswap.png");

impl Venue {
    /// The venue spelled out for large type in the header.
    pub fn display_name(&self, network: &str) -> String {
        match self {
            Venue::PumpFun => "PUMP.FUN".to_string(),
            Venue::Pons => "PONS".to_string(),
            Venue::Uniswap => "UNISWAP".to_string(),
            Venue::Chain => {
                let n = network.to_lowercase();
                if n.starts_with("solana") {
                    "SOLANA".to_string()
                } else if n.starts_with("robinhood") {
                    "ROBINHOOD".to_string()
                } else {
                    network.to_uppercase()
                }
            }
        }
    }
}

/// The PNG for a venue, falling back to the network's own mark.
pub fn for_venue(venue: Venue, network: &str) -> Option<&'static [u8]> {
    match venue {
        Venue::PumpFun => Some(PUMP_PNG),
        Venue::Pons => Some(PONS_PNG),
        Venue::Uniswap => Some(UNISWAP_PNG),
        Venue::Chain => for_network(network),
    }
}


/// Where a trade is actually happening, most specific first.
///
/// The chain is the least interesting fact about a position: what matters is
/// the venue you are exposed to. A launchpad is more specific than the AMM it
/// graduates into, which is more specific than the chain underneath — so the
/// logo resolves in that order and only falls back to the chain when nothing
/// more specific is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    /// A launchpad: pump.fun. Only the Solana dashboard selects this, so an
    /// EVM-only build never constructs it.
    #[cfg_attr(not(feature = "solana"), allow(dead_code))]
    PumpFun,
    /// Pons. Detected from the pool having a Pons `TokenLaunched` block: only
    /// graduation discovery sets one, so its presence IS the signal that this
    /// token came off Pons rather than being a plain Uniswap pair.
    Pons,
    /// A plain AMM trade.
    Uniswap,
    /// Nothing more specific known — fall back to the chain's own mark.
    /// Selected when no coin is loaded, which only the Solana screen can be.
    #[cfg_attr(not(feature = "solana"), allow(dead_code))]
    Chain,
}

/// The PNG for a network, matched on its registry name. `None` when the
/// network has no brand of its own — a local dev node, say.
pub fn for_network(name: &str) -> Option<&'static [u8]> {
    let n = name.to_lowercase();
    if n.starts_with("solana") {
        // The chain's own mark. pump.fun's logomark is the venue, not the
        // chain, and belongs on the trading screen instead.
        Some(SOLANA_PNG)
    } else if n.starts_with("robinhood") {
        Some(ROBINHOOD_PNG)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Bytes above 0x7f must not be mangled.
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn embedded_assets_are_real_pngs() {
        for png in [ROBINHOOD_PNG, SOLANA_PNG, PUMP_PNG, PONS_PNG, UNISWAP_PNG] {
            assert!(png.len() > 1000, "asset looks truncated");
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
        }
    }

    #[test]
    fn a_clear_makes_every_placement_stale() {
        // This is what frees callers from tracking screens: a screen clears on
        // entry, and the dashboard notices on its own.
        let before = generation();
        clear();
        assert!(generation() > before, "clear must invalidate placements");
    }

    #[test]
    fn only_branded_networks_have_artwork() {
        assert_eq!(for_network("solana-mainnet"), Some(SOLANA_PNG));
        assert_eq!(for_network("robinhood-testnet"), Some(ROBINHOOD_PNG));
        // Display names too: that is what the dashboards pass in.
        assert_eq!(for_network("Solana Mainnet"), Some(SOLANA_PNG));
        assert_eq!(for_network("Robinhood Mainnet"), Some(ROBINHOOD_PNG));
        assert_eq!(for_network("anvil-local"), None);
    }
}
