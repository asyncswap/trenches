// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Chain-agnostic view models: the shape the UI renders, with NO chain types in
//! it — no `Address`, no `TxHash`, no `Pubkey`, no `Bot`. Every chain adapter
//! (EVM today, Solana next) formats its native state into these plain structs,
//! and `ui/` renders them. That's the seam that lets both chains share one UI.
//!
//! Colours are expressed as semantic `Tone`s, not palette values — the mapping
//! to actual colours lives in `ui`, so a theme change touches one place.
//!
//! A few constructors are only exercised by one chain's screens, so they look
//! unused in a single-feature build; they're part of the shared vocabulary.
#![allow(dead_code)]

/// Semantic colour role. `ui::tone_color` maps these to the terminal palette.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tone {
    Normal, // default foreground
    Dim,    // de-emphasised (addresses, hashes, hints)
    Good,   // profit, confirmed, buys
    Bad,    // loss, reverted, sells
    Warn,   // pending, caution
    Info,   // neutral highlight (venue tags, headers)
    Accent, // mode / attention (magenta-ish)
    Mine,   // our own activity (stands out from the market)
    /// Parameter names / field labels. Rendered in the accent-border colour so
    /// panel frames, titles and labels form one visual family.
    Label,
}

/// One rendered cell: text plus how to style it.
#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
    pub bold: bool,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Cell {
        Cell { text: text.into(), tone: Tone::Normal, bold: false }
    }
    pub fn toned(text: impl Into<String>, tone: Tone) -> Cell {
        Cell { text: text.into(), tone, bold: false }
    }
    pub fn bold(text: impl Into<String>, tone: Tone) -> Cell {
        Cell { text: text.into(), tone, bold: true }
    }
    /// An empty spacer cell.
    pub fn blank() -> Cell {
        Cell::new(String::new())
    }
}

/// How wide a column renders: a fixed width, or "take the rest".
#[derive(Clone, Copy, Debug)]
pub enum Width {
    Fixed(u16),
    Min(u16),
}

/// A table column: heading + width.
#[derive(Clone, Debug)]
pub struct Col {
    pub title: &'static str,
    pub width: Width,
}

impl Col {
    pub fn fixed(title: &'static str, w: u16) -> Col {
        Col { title, width: Width::Fixed(w) }
    }
    pub fn min(title: &'static str, w: u16) -> Col {
        Col { title, width: Width::Min(w) }
    }
}

/// A table of rows — used for discovery lists, orders, and the tape alike.
/// `selectable` rows get a highlight + `▸` cursor; `row_tone` tints a whole row
/// (e.g. our own trades in the tape).
#[derive(Clone, Debug, Default)]
pub struct TableView {
    pub title: String,
    /// When set, the panel menu rides the top border, this key highlighted.
    pub active_key: Option<char>,
    pub cols: Vec<Col>,
    pub rows: Vec<Vec<Cell>>,
    /// Optional per-row background emphasis (index-aligned with `rows`).
    pub row_mine: Vec<bool>,
    /// Shown centred when `rows` is empty (e.g. "scanning…").
    pub empty_note: String,
    /// Draw a health light before the title.
    ///
    /// For the screens you sit and wait on. An empty list and a refused
    /// endpoint look identical without it, and only one of them is worth
    /// waiting through.
    pub health: bool,
}

impl TableView {
    pub fn new(title: impl Into<String>, cols: Vec<Col>) -> TableView {
        TableView {
            active_key: None, title: title.into(), cols, ..Default::default() }
    }
    pub fn push(&mut self, row: Vec<Cell>) {
        self.rows.push(row);
        self.row_mine.push(false);
    }
    /// Push a row flagged as ours (rendered with the "mine" emphasis).
    pub fn push_mine(&mut self, row: Vec<Cell>, mine: bool) {
        self.rows.push(row);
        self.row_mine.push(mine);
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// A bordered text panel (market / wallet): a title plus lines of styled spans.
#[derive(Clone, Debug, Default)]
pub struct PanelView {
    /// When set, the panel menu rides the top border, this key highlighted.
    pub active_key: Option<char>,
    pub title: String,
    pub lines: Vec<Vec<Cell>>,
}

impl PanelView {
    pub fn new(title: impl Into<String>) -> PanelView {
        PanelView {
            active_key: None, title: title.into(), lines: Vec::new() }
    }
    /// One plain line of text.
    pub fn line(&mut self, text: impl Into<String>) {
        self.lines.push(vec![Cell::new(text)]);
    }
    /// One line, whole-line tone.
    pub fn line_toned(&mut self, text: impl Into<String>, tone: Tone) {
        self.lines.push(vec![Cell::toned(text, tone)]);
    }
    /// A `label  value` line where only the value is toned — the common case.
    pub fn kv(&mut self, label: impl Into<String>, value: impl Into<String>, tone: Tone) {
        self.lines.push(vec![Cell::new(label), Cell::bold(value, tone)]);
    }
    /// A line assembled from arbitrary spans.
    pub fn spans(&mut self, cells: Vec<Cell>) {
        self.lines.push(cells);
    }
}

/// Marker shape for a scatter series.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shape {
    /// A single point per datum.
    Dot,
    /// A filled disc whose radius scales with the datum's magnitude.
    Disc,
    /// A hollow square outline — used to pin OUR trades over market activity.
    Ring,
}

/// One scatter series (e.g. "buys", "you sell").
#[derive(Clone, Debug)]
pub struct Series {
    pub name: String,
    pub tone: Tone,
    pub shape: Shape,
    /// Raw data points (x, y) in axis units — the renderer expands Disc/Ring.
    pub points: Vec<(f64, f64)>,
}

/// A scatter axis: title, upper bound, and the tick labels to print.
#[derive(Clone, Debug, Default)]
pub struct AxisView {
    pub title: String,
    pub max: f64,
    pub labels: Vec<String>,
}

/// One OHLC candle — the unit TradingView made everyone fluent in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candle {
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    /// Quote-side volume traded inside this candle.
    pub v: f64,
    /// Unix second this candle's bucket starts at — what lets a trade
    /// timestamp find its column again (entry/exit markers).
    pub t: i64,
}

impl Candle {
    pub fn up(&self) -> bool {
        self.c >= self.o
    }
}

/// A candlestick chart: candles oldest-first, plus what to call the numbers.
#[derive(Clone, Debug, Default)]
pub struct CandleView {
    pub title: String,
    pub candles: Vec<Candle>,
    pub interval_secs: u64,
    /// Unit label for the y axis ("SOL" / "ETH").
    pub unit: &'static str,
    /// When set, the panel menu ([t] Trades · [v] Candles · …) rides the top
    /// border with this key highlighted — every view names its siblings.
    pub active_key: Option<char>,
    /// OUR trades — (unix seconds, is_buy) — drawn as vertical marker lines
    /// through the candle they landed in, so entries and exits sit on the
    /// chart the way they sit in memory.
    pub trades: Vec<(i64, bool)>,
}

/// Aggregate raw trades into time-bucketed candles, oldest-first.
///
/// `points` is (unix seconds, price, quote volume) in ANY order — the tape
/// accumulates newest-first and merges out-of-order fetches, so ordering here
/// rather than trusting the caller is what keeps a candle's open/close honest.
/// Buckets nobody traded in carry the previous close as a flat candle, so
/// quiet seconds read as a flat line instead of the chart silently skipping
/// time — and every candle OPENS at the previous candle's close, so the line
/// of prices is continuous across the whole chart. At most `max` candles come
/// back — the newest.
///
/// `now` (unix seconds, or the chain clock the points use) extends the flat
/// line to the present: five silent minutes draw as five minutes of flat, not
/// a chart frozen at the last trade. Safe even with late-arriving trades —
/// candles rebuild from the raw tape every frame, so a bucket that showed
/// flat becomes a real candle the moment its trade lands.
pub fn candles_of(points: &[(i64, f64, f64)], interval_secs: u64, max: usize, now: Option<i64>) -> Vec<Candle> {
    let iv = interval_secs.max(1) as i64;
    let mut pts: Vec<(i64, f64, f64)> =
        points.iter().copied().filter(|(t, p, _)| *t > 0 && *p > 0.0).collect();
    if pts.is_empty() {
        return Vec::new();
    }
    pts.sort_by_key(|(t, _, _)| *t);

    let mut out: Vec<Candle> = Vec::new();
    let mut bucket = pts[0].0 - pts[0].0.rem_euclid(iv);
    let mut cur: Option<Candle> = None;
    for (t, p, v) in pts {
        let b = t - t.rem_euclid(iv);
        if b != bucket {
            // Close the bucket in hand…
            if let Some(k) = cur.take() {
                out.push(k);
            }
            // …and flat-fill the silent buckets between it and this one.
            // When a gap is wider than `max` the fill is clamped to the LAST
            // `gaps` buckets before this one — same flat line on screen, and
            // every candle still carries its true time.
            if let Some(last) = out.last().copied() {
                let gaps = ((b - bucket) / iv - 1).clamp(0, max as i64);
                for k in (1..=gaps).rev() {
                    let t = b - k * iv;
                    out.push(Candle { o: last.c, h: last.c, l: last.c, c: last.c, v: 0.0, t });
                }
            }
            bucket = b;
        }
        cur = Some(match cur {
            // A new candle opens where the previous one CLOSED, not at its own
            // first trade — that's the grammar every charting tool taught:
            // price is continuous, so a gap between close and next-open reads
            // as candles jumping around. The chained open joins the range, so
            // the body/wick covers the ground from prev-close to first trade.
            None => {
                let o = out.last().map_or(p, |k| k.c);
                Candle { o, h: o.max(p), l: o.min(p), c: p, v, t: b }
            }
            Some(k) => Candle { o: k.o, h: k.h.max(p), l: k.l.min(p), c: p, v: k.v + v, ..k },
        });
    }
    if let Some(k) = cur {
        out.push(k);
    }
    if let (Some(now), Some(last)) = (now, out.last().copied()) {
        // At most HALF the window of live flat — enough that quiet reads as
        // quiet, never so much that it drains the real candles out of the
        // window. A coin reopened a day after its last trade used to show
        // 240 flats and none of the saved history the tape had just loaded.
        let nb = now - now.rem_euclid(iv);
        let gaps = ((nb - last.t) / iv).clamp(0, (max / 2) as i64);
        for k in 1..=gaps {
            out.push(Candle { o: last.c, h: last.c, l: last.c, c: last.c, v: 0.0, t: last.t + k * iv });
        }
    }
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

#[cfg(test)]
mod candle_tests {
    use super::*;

    #[test]
    fn trades_in_one_bucket_fold_into_one_honest_candle() {
        // Out of order on purpose: the tape merges fetches out of order too.
        let pts = [(103, 5.0, 1.0), (101, 2.0, 1.0), (100, 3.0, 1.0), (104, 4.0, 1.0)];
        let c = candles_of(&pts, 10, 100, None);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].o, c[0].h, c[0].l, c[0].c, c[0].v), (3.0, 5.0, 2.0, 4.0, 4.0));
        assert!(c[0].up());
    }

    #[test]
    fn silence_since_the_last_trade_draws_flat_to_now() {
        // One trade at 100, and it is now 152: buckets 110..150 are silent.
        // The chart must show five flat candles after the real one — a live
        // flat line — instead of freezing at the last trade.
        let pts = [(100, 3.0, 1.0)];
        let c = candles_of(&pts, 10, 100, Some(152));
        // Buckets 110..150 inclusive — the CURRENT, in-progress bucket draws
        // too, so the flat line reaches the right edge of "now".
        assert_eq!(c.len(), 6);
        for flat in &c[1..] {
            assert_eq!((flat.o, flat.c, flat.v), (3.0, 3.0, 0.0));
        }
        assert_eq!(c.last().unwrap().t, 150);
        // And without a clock, nothing is invented.
        assert_eq!(candles_of(&pts, 10, 100, None).len(), 1);
    }

    #[test]
    fn old_history_survives_the_live_flat_extension() {
        // Ten real 1s candles from yesterday, reopened hours later: the flat
        // line to "now" must not flood the window and push them all out.
        let pts: Vec<(i64, f64, f64)> = (0..10).map(|i| (1000 + i, 2.0, 1.0)).collect();
        let c = candles_of(&pts, 1, 240, Some(90_000));
        assert_eq!(c.iter().filter(|k| k.v > 0.0).count(), 10, "history drained");
        assert!(c.len() <= 240);
    }

    #[test]
    fn each_candle_opens_at_the_previous_close() {
        // Bucket 1 closes at 5; bucket 2's first trade is way up at 9. The
        // second candle must open at 5 (and its low reach down to it), not
        // open at 9 and float disconnected from the line of prices.
        let pts = [(100, 3.0, 1.0), (105, 5.0, 1.0), (112, 9.0, 1.0)];
        let c = candles_of(&pts, 10, 100, None);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].o, 5.0);
        assert_eq!(c[1].l, 5.0);
        assert_eq!(c[1].c, 9.0);
        assert!(c[1].up());
    }

    #[test]
    fn silence_reads_as_a_flat_line_not_skipped_time() {
        let pts = [(100, 3.0, 1.0), (145, 6.0, 1.0)];
        let c = candles_of(&pts, 10, 100, None);
        // 100s, three silent buckets (110/120/130), then 140s.
        assert_eq!(c.len(), 5);
        for flat in &c[1..4] {
            assert_eq!((flat.o, flat.h, flat.l, flat.c, flat.v), (3.0, 3.0, 3.0, 3.0, 0.0));
        }
        assert_eq!(c[4].c, 6.0);
    }

    #[test]
    fn only_the_newest_max_candles_survive() {
        let pts: Vec<(i64, f64, f64)> = (0..50).map(|i| (i * 10, i as f64 + 1.0, 1.0)).collect();
        let c = candles_of(&pts, 10, 8, None);
        assert_eq!(c.len(), 8);
        assert_eq!(c.last().unwrap().c, 50.0);
    }

    #[test]
    fn zero_prices_and_times_are_junk_not_data() {
        let pts = [(0, 5.0, 1.0), (100, 0.0, 1.0), (100, 2.0, 1.0)];
        let c = candles_of(&pts, 10, 10, None);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].o, 2.0);
    }
}

/// The candle-interval ladder, seconds. Bounded by what the live tape can
/// hold — longer candles (4h, 1d) want persisted trade history, which is the
/// next step, not this one.
pub const IV_STEPS: [u64; 10] = [1, 5, 15, 60, 300, 600, 900, 3600, 14400, 86400];

pub fn iv_step(cur: u64, up: bool) -> u64 {
    let i = IV_STEPS.iter().position(|s| *s == cur).unwrap_or(3);
    IV_STEPS[if up { (i + 1).min(IV_STEPS.len() - 1) } else { i.saturating_sub(1) }]
}

pub fn iv_label(iv: u64) -> String {
    if iv < 60 {
        format!("{iv}s")
    } else if iv < 3600 {
        format!("{}m", iv / 60)
    } else if iv < 86_400 {
        format!("{}h", iv / 3600)
    } else {
        format!("{}d", iv / 86_400)
    }
}

/// A scatter plot with a horizontal key strip above it.
#[derive(Clone, Debug, Default)]
pub struct ScatterView {
    pub title: String,
    pub series: Vec<Series>,
    pub x: AxisView,
    pub y: AxisView,
    /// Extra note appended to the key strip (e.g. "bigger = more ETH").
    pub key_note: String,
}

/// Map a value onto a log10 axis whose floor is `min`.
///
/// Trade sizes span orders of magnitude — one 3 ETH whale beside hundreds of
/// 0.0001 ETH trades. On a linear axis that outlier sets the top and everything
/// else collapses onto the bottom row, leaving a mostly-empty rectangle.
/// Callers scale their points through this and set the axis bounds to match. Values at or below the
/// floor land on 0 rather than diverging to negative infinity.
/// An ETH amount, with the precision the number actually needs.
///
/// A fixed four decimals renders anything under a ten-thousandth of an ETH as
/// "0.0000" — which is most of what a small account holds, and reads as
/// nothing at all. Precision scales instead: whole numbers do not need six
/// decimals, and dust needs every one it can get.
pub fn eth(v: f64) -> String {
    let a = v.abs();
    if a == 0.0 {
        "0".to_string()
    } else if a >= 1_000.0 {
        format!("{v:.2}")
    } else if a >= 1.0 {
        format!("{v:.4}")
    } else if a >= 0.000_001 {
        format!("{v:.6}")
    } else {
        // Below a millionth, six decimals is "0.000000" again. Eight is where
        // this stops: past that it is closer to a rounding error than a balance.
        format!("{v:.8}")
    }
}

pub fn log_scale(v: f64, min: f64) -> f64 {
    if v <= min || min <= 0.0 {
        0.0
    } else {
        (v / min).log10()
    }
}

/// Compact USD: `$4.20M`, `$840k`, `$120`. Shared by every chain's tables.
pub fn usd_compact(x: f64) -> String {
    if x >= 1e6 {
        format!("${:.2}M", x / 1e6)
    } else if x >= 1e3 {
        // Two decimals: "$9k" hides the difference between $9,001 and $9,999,
        // which is exactly the movement a tape is being read for.
        format!("${:.2}k", x / 1e3)
    } else {
        format!("${x:.2}")
    }
}

/// Marker for our own rows on a tape.
///
/// Emoji presentation (U+2B50) rather than the text-weight `★` (U+2605): the
/// text star renders at glyph weight and disappears in a dense table, which is
/// the one row you must never miss. It is double-width, so the marker column is
/// exactly 2 — a third cell just pushes the first real column right.
pub const MINE_MARK: &str = "⭐";

/// Compact elapsed time: `42s`, `7m`, `3h`.
pub fn age_compact(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{}m", (secs / 60.0) as u64)
    } else {
        format!("{}h", (secs / 3600.0) as u64)
    }
}

impl TableView {
    /// Every row must have exactly one cell per column.
    ///
    /// A mismatch doesn't error at render time — ratatui just drops or shifts
    /// cells, so a missing cell silently slides every later column one to the
    /// left (a mint appearing under "age"). Cheap to assert, invisible otherwise.
    pub fn is_well_formed(&self) -> bool {
        self.rows.iter().all(|r| r.len() == self.cols.len())
    }
}

/// Turn a registry network id into a display label: `solana-mainnet` ->
/// `Solana Mainnet`. Header titles name the chain the bot is trading, so a
/// misread there is expensive — the label is derived from config rather than
/// hard-coded per screen.
pub fn pretty_network(name: &str) -> String {
    let words: Vec<String> = name
        .split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() { name.to_string() } else { words.join(" ") }
}

/// Just the environment part of a network id: `robinhood-mainnet` -> `Mainnet`.
///
/// The chain is already named by the logo beside it, so repeating it in the
/// title is noise — what still matters is which environment you are trading.
pub fn network_env(name: &str) -> String {
    let last = name.split(['-', '_']).filter(|s| !s.is_empty()).next_back().unwrap_or(name);
    pretty_network(last)
}

#[cfg(test)]
mod network_label_tests {
    use super::pretty_network;

    #[test]
    fn only_the_environment_survives_in_a_title() {
        assert_eq!(super::network_env("robinhood-mainnet"), "Mainnet");
        assert_eq!(super::network_env("solana-mainnet"), "Mainnet");
        assert_eq!(super::network_env("robinhood_testnet"), "Testnet");
        // A single-word name is its own environment.
        assert_eq!(super::network_env("devnet"), "Devnet");
        assert_eq!(super::network_env(""), "");
    }

    #[test]
    fn network_ids_become_display_labels() {
        assert_eq!(pretty_network("solana-mainnet"), "Solana Mainnet");
        assert_eq!(pretty_network("robinhood-mainnet"), "Robinhood Mainnet");
        assert_eq!(pretty_network("robinhood_testnet"), "Robinhood Testnet");
        // Already-pretty names survive untouched.
        assert_eq!(pretty_network("Base"), "Base");
        // Degenerate input falls back to the raw name rather than going blank.
        assert_eq!(pretty_network(""), "");
        assert_eq!(pretty_network("--"), "--");
    }
}

/// SOL amounts at a readable precision.
///
/// Four decimals on a 2,200 SOL pool is noise — nobody reads the ten-thousandth
/// of a SOL in a two-thousand-SOL pool. But a fresh bonding curve holds a
/// fraction of a SOL, where those digits are the whole signal, so precision
/// scales with size rather than being fixed either way.
pub fn sol_compact(x: f64) -> String {
    let a = x.abs();
    if a >= 1_000.0 {
        format!("{x:.0}")
    } else if a >= 100.0 {
        format!("{x:.1}")
    } else if a >= 1.0 {
        format!("{x:.2}")
    } else {
        format!("{x:.4}")
    }
}

#[cfg(test)]
mod sol_compact_tests {
    use super::sol_compact;

    #[test]
    fn precision_scales_with_size() {
        assert_eq!(sol_compact(2200.3289), "2200");
        assert_eq!(sol_compact(150.55), "150.6");
        assert_eq!(sol_compact(9.8299), "9.83");
        // A fresh curve holds fractions of a SOL — those digits ARE the signal.
        assert_eq!(sol_compact(0.1975), "0.1975");
        assert_eq!(sol_compact(0.0), "0.0000");
    }
}

#[cfg(test)]
mod log_scale_tests {
    use super::log_scale;

    #[test]
    fn decades_are_evenly_spaced() {
        // The whole point: each 10x step takes the same vertical distance, so a
        // 0.0001 trade and a 3 ETH trade both get room instead of one of them
        // owning the axis.
        let min = 1e-4;
        assert!((log_scale(1e-4, min) - 0.0).abs() < 1e-9);
        assert!((log_scale(1e-3, min) - 1.0).abs() < 1e-9);
        assert!((log_scale(1e-2, min) - 2.0).abs() < 1e-9);
        assert!((log_scale(1e-1, min) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn values_below_the_floor_clamp_instead_of_diverging() {
        // log(0) is -inf, which would blow up the plot bounds.
        assert_eq!(log_scale(0.0, 1e-4), 0.0);
        assert_eq!(log_scale(-5.0, 1e-4), 0.0);
        assert_eq!(log_scale(1.0, 0.0), 0.0);
    }
}

#[cfg(test)]
mod eth_format_tests {
    use super::eth;

    #[test]
    fn small_amounts_keep_their_digits() {
        // The case that started this: four decimals called it nothing.
        assert_eq!(eth(0.000_062), "0.000062");
        assert_eq!(eth(0.000_001), "0.000001");
        assert_eq!(eth(0.000_000_5), "0.00000050");
    }

    #[test]
    fn large_amounts_do_not_carry_noise() {
        assert_eq!(eth(1_234.5), "1234.50");
        assert_eq!(eth(2.5), "2.5000");
    }

    #[test]
    fn zero_is_zero_rather_than_a_row_of_noughts() {
        assert_eq!(eth(0.0), "0");
    }

    #[test]
    fn the_sign_survives() {
        assert_eq!(eth(-0.000_062), "-0.000062");
    }
}
