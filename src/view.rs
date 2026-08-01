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
    /// Owned, not `&'static str`: a header often has to name the pool's quote
    /// asset, and which asset that is only becomes known at runtime. "pooled
    /// ETH" printed over a USDG pool's depth is a unit error on screen.
    pub title: String,
    pub width: Width,
}

impl Col {
    pub fn fixed(title: impl Into<String>, w: u16) -> Col {
        Col { title: title.into(), width: Width::Fixed(w) }
    }
    pub fn min(title: impl Into<String>, w: u16) -> Col {
        Col { title: title.into(), width: Width::Min(w) }
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
/// Serialized as the per-coin candle history, so a chart survives a restart.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// The same candle in a different unit.
    ///
    /// Price to market cap is multiplication by supply, so every level scales
    /// together and the shape is untouched — which is why the chart can switch
    /// units without recomputing anything from the tape. Volume is quote-side
    /// and is deliberately NOT scaled: it is already money.
    pub fn scaled(self, k: f64) -> Candle {
        Candle { o: self.o * k, h: self.h * k, l: self.l * k, c: self.c * k, ..self }
    }
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
    /// Unit label for the y axis ("SOL" / "ETH"), or "" when the values are
    /// money and carry their own symbol.
    pub unit: String,
    /// Values are money — format them as money rather than as a raw quantity.
    pub money: bool,
    /// The newest candle's bucket, on the same clock as `Candle::t`. Lets the
    /// x axis say how long ago each column was without knowing what that clock
    /// means.
    pub now_t: i64,
    /// When set, the panel menu ([t] Trades · [v] Candles · …) rides the top
    /// border with this key highlighted — every view names its siblings.
    pub active_key: Option<char>,
    /// OUR trades — (unix seconds, price, is_buy) — drawn as horizontal
    /// lines at each fill's PRICE, the way TradingView draws a position:
    /// where you got in and out, readable against where price is now.
    pub trades: Vec<(i64, f64, bool)>,
}

/// Re-bucket candles into a coarser interval — the cheap path for big
/// intervals: history is aggregated ONCE into 1m candles and stored; a 4h or
/// 1d chart folds those instead of re-walking every trade ever seen. Gaps
/// between stored candles flat-fill and every bucket opens at the previous
/// close, same grammar as `candles_of`.
pub fn fold_candles(base: &[Candle], iv_secs: u64) -> Vec<Candle> {
    let iv = iv_secs.max(1) as i64;
    let mut out: Vec<Candle> = Vec::new();
    for c in base {
        let b = c.t - c.t.rem_euclid(iv);
        match out.last_mut() {
            Some(k) if k.t == b => {
                k.h = k.h.max(c.h);
                k.l = k.l.min(c.l);
                k.c = c.c;
                k.v += c.v;
            }
            prev => {
                let o = prev.as_ref().map_or(c.o, |k| k.c);
                if let Some(k) = prev {
                    // Flat-fill silent buckets between the last one and this.
                    let gaps = ((b - k.t) / iv - 1).max(0);
                    let (kc, kt) = (k.c, k.t);
                    for g in 1..=gaps {
                        out.push(Candle { o: kc, h: kc, l: kc, c: kc, v: 0.0, t: kt + g * iv });
                    }
                }
                // Same rule as candles_of: the chained open lives in the
                // body, never in the wick range.
                out.push(Candle { o, h: c.h, l: c.l, c: c.c, v: c.v, t: b });
            }
        }
    }
    out
}

/// Extend `out` with flat candles up to `now`'s bucket — at most half of
/// `max`, so live silence shows without draining real history out of the
/// window. The tail end of `candles_of`, shared so folded charts extend too.
pub fn extend_flat_to_now(out: &mut Vec<Candle>, iv_secs: u64, now: i64, max: usize) {
    let iv = iv_secs.max(1) as i64;
    let Some(last) = out.last().copied() else { return };
    let nb = now - now.rem_euclid(iv);
    let cap = i64::try_from(max / 2).unwrap_or(i64::MAX);
    let gaps = ((nb - last.t) / iv).clamp(0, cap);
    for k in 1..=gaps {
        out.push(Candle { o: last.c, h: last.c, l: last.c, c: last.c, v: 0.0, t: last.t + k * iv });
    }
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
                // Saturating: `usize::MAX as i64` is -1, and clamp(0, -1)
                // panics — a cap has to stay a cap however large it is asked.
                let cap = i64::try_from(max).unwrap_or(i64::MAX);
                let gaps = ((b - bucket) / iv - 1).clamp(0, cap);
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
            // as candles jumping around. The BODY (open..close) covers that
            // ground by construction; high/low stay the candle's own trades —
            // stretching them to the chained open painted a full-height
            // one-column wick line whenever the gap dwarfed the real range.
            None => {
                let o = out.last().map_or(p, |k| k.c);
                Candle { o, h: p, l: p, c: p, v, t: b }
            }
            Some(k) => Candle { o: k.o, h: k.h.max(p), l: k.l.min(p), c: p, v: k.v + v, ..k },
        });
    }
    if let Some(k) = cur {
        out.push(k);
    }
    if let Some(now) = now {
        extend_flat_to_now(&mut out, interval_secs, now, max);
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
    fn a_huge_cap_is_a_cap_not_a_panic() {
        // usize::MAX as i64 is -1; clamp(0, -1) asserts. Any tape spanning
        // two buckets crashed the whole app when a caller passed MAX.
        let pts = [(100, 3.0, 1.0), (500, 4.0, 1.0)];
        let c = candles_of(&pts, 60, usize::MAX, Some(700));
        assert!(c.len() >= 2);
    }

    #[test]
    fn folding_minute_candles_into_hours_keeps_the_story() {
        // Three 1m candles across two hour-buckets, with a silent hour after
        // the first: fold must group, flat-fill, and chain the opens.
        let m = |t: i64, o: f64, h: f64, l: f64, c: f64| Candle { o, h, l, c, v: 1.0, t };
        let base = [m(0, 1.0, 4.0, 1.0, 2.0), m(60, 2.0, 5.0, 2.0, 3.0), m(7200, 9.0, 9.0, 8.0, 8.5)];
        let f = fold_candles(&base, 3600);
        assert_eq!(f.len(), 3);
        assert_eq!((f[0].o, f[0].h, f[0].c), (1.0, 5.0, 3.0));
        assert_eq!((f[1].o, f[1].c, f[1].v), (3.0, 3.0, 0.0), "silent hour is flat");
        assert_eq!(f[2].o, 3.0, "opens where the flat closed");
        assert_eq!(f[2].l, 8.0, "wick stays the candle's own trades");
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
        // The BODY spans the chained ground (open 5, close 9); the wick range
        // stays the candle's own trades — a chained-open wick painted a
        // full-height line whenever a gap dwarfed the real range.
        assert_eq!(c[1].l, 9.0);
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

/// A compact money figure: `$4.20M`, `$840k`, `$120`. Shared by every chain's
/// tables.
///
/// Takes USD, because USD is what the feeds quote and what the ledger records.
/// Renders in whatever currency the reader chose — the conversion lives here so
/// that every one of these call sites follows without knowing about it, and so
/// that nothing on the way to disk is ever converted.
pub fn usd_compact(x: f64) -> String {
    let sym = crate::base_currency::symbol();
    let x = crate::base_currency::from_usd(x);
    if x >= 1e6 {
        format!("{sym}{:.2}M", x / 1e6)
    } else if x >= 1e3 {
        // Two decimals: "$9k" hides the difference between $9,001 and $9,999,
        // which is exactly the movement a tape is being read for.
        format!("{sym}{:.2}k", x / 1e3)
    } else {
        format!("{sym}{x:.2}")
    }
}

/// A per-token price in dollars, however small.
///
/// A memecoin trades at $0.00000256, and every general-purpose formatter
/// renders that as `$0.00`. Two decimals is right for a balance and useless
/// for a price whose whole story is in the seventh place.
///
/// So leading zeros are counted rather than printed, the way every chart site
/// writes them: `$0.0₅256` is `$0.00000256`. The subscript is the number of
/// zeros after the point, so the eye reads the magnitude in one glyph instead
/// of counting. Above a thousandth this is just a normal price with enough
/// decimals to be one.
pub fn usd_price(x: f64) -> String {
    usd_price_sig(x, 3)
}

/// The same price with two significant digits and no trailing zeros, for
/// somewhere two of them have to fit side by side.
///
/// An LP range is a pair of boundaries, not a price anyone trades at, and six
/// decimals on each turns `[$0.02, $0.08]` into something that runs off the
/// end of its column and gets truncated mid-number — which is how the second
/// bound lost its dollar sign.
pub fn usd_price_brief(x: f64) -> String {
    // Past a thousand, a price is read in magnitude rather than in digits, and
    // an LP bound can run to eight figures. `$63205347.123` is not a number
    // anyone reads; `$63.21M` is.
    if x >= 1_000.0 && x.is_finite() {
        return usd_compact(x);
    }
    let s = usd_price_sig(x, 2);
    // `$0.020` says nothing `$0.02` does not.
    if s.contains('.') && !s.contains('\u{2080}') && s.ends_with('0') {
        return s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    s
}

/// `sig` counts the digits kept after the leading zeros.
fn usd_price_sig(x: f64, sig: u32) -> String {
    if !x.is_finite() || x <= 0.0 {
        return "—".to_string();
    }
    let sym = crate::base_currency::symbol();
    let x = crate::base_currency::from_usd(x);
    if x >= 1.0 {
        return format!("{sym}{x:.*}", sig as usize + 1);
    }
    if x >= 0.001 {
        // Enough places to show `sig` real digits after however many zeros.
        let zeros = (-x.log10().ceil()).max(0.0) as usize;
        return format!("{sym}{x:.*}", zeros + sig as usize);
    }
    // How many zeros sit between the point and the first real digit.
    let zeros = (-x.log10().floor() - 1.0) as usize;
    // Beyond eighteen there is nothing left to say — that is past the
    // resolution of the units themselves.
    if zeros > 18 {
        return format!("~{sym}0");
    }
    let digits = (x * 10f64.powi(zeros as i32 + sig as i32)).round() as u64;
    const SUB: [char; 10] = ['\u{2080}', '\u{2081}', '\u{2082}', '\u{2083}', '\u{2084}',
                             '\u{2085}', '\u{2086}', '\u{2087}', '\u{2088}', '\u{2089}'];
    let sub: String = zeros.to_string().chars().filter_map(|c| c.to_digit(10)).map(|d| SUB[d as usize]).collect();
    format!("{sym}0.0{sub}{digits}")
}

/// The dollar tag that rides beside a figure in the native currency:
/// `($12.34)`, or `(-$12.34)` for a loss.
///
/// The sign goes OUTSIDE the dollar sign — `-$12.34`, not `$-12.34` — because
/// the second reads as a typo at a glance, and a loss is the number you least
/// want to misread.
///
/// `None` when there is no rate yet or there is nothing to price: an
/// approximate zero is noise, and a made-up rate is worse than no number.
pub fn usd_tag(amount: f64, rate: f64) -> Option<String> {
    if rate <= 0.0 || amount == 0.0 || !amount.is_finite() || !rate.is_finite() {
        return None;
    }
    let v = amount * rate;
    let sign = if v < 0.0 { "-" } else { "" };
    Some(format!("({sign}{})", usd_compact(v.abs())))
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
    // Seconds, minutes, hours, DAYS. Stopping at minutes turned an
    // eight-hour-old launch into "507m", which nobody reads as a duration —
    // you have to divide it in your head to find out you are looking at
    // yesterday.
    if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{}m", (secs / 60.0) as u64)
    } else if secs < 86_400.0 {
        format!("{}h", (secs / 3600.0) as u64)
    } else {
        format!("{}d", (secs / 86_400.0) as u64)
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
    let last = name.split(['-', '_']).rfind(|s| !s.is_empty()).unwrap_or(name);
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

#[cfg(test)]
mod usd_tag_tests {
    use super::*;

    /// A loss reads as -$12.34, never $-12.34 — the second scans as a typo,
    /// and the losing number is the one you least want to misread.
    #[test]
    fn a_loss_puts_the_sign_before_the_dollar() {
        assert_eq!(usd_tag(-0.01, 1_234.0).as_deref(), Some("(-$12.34)"));
        assert_eq!(usd_tag(0.01, 1_234.0).as_deref(), Some("($12.34)"));
    }

    /// No rate, no number. An approximate zero is noise, and inventing a rate
    /// to fill the column would be worse than leaving it empty.
    #[test]
    fn nothing_to_price_shows_nothing() {
        assert_eq!(usd_tag(1.0, 0.0), None, "no rate yet");
        assert_eq!(usd_tag(1.0, -5.0), None, "a negative rate is not a rate");
        assert_eq!(usd_tag(0.0, 1_234.0), None, "zero is not worth approximating");
        assert_eq!(usd_tag(f64::NAN, 1_234.0), None);
        assert_eq!(usd_tag(1.0, f64::INFINITY), None);
    }

    /// It rides on the compact scale the tables already use, so a wallet and a
    /// tape row never disagree about what $9,001 is called.
    #[test]
    fn it_uses_the_same_scale_as_every_other_table() {
        assert_eq!(usd_tag(1.0, 9_001.0).as_deref(), Some("($9.00k)"));
        assert_eq!(usd_tag(2.0, 1_000_000.0).as_deref(), Some("($2.00M)"));
        assert_eq!(usd_tag(0.5, 100.0).as_deref(), Some("($50.00)"));
    }
}

#[cfg(test)]
mod age_tests {
    use super::*;

    /// Minutes stop at an hour. "507m" is not a duration anyone reads — you
    /// have to divide it in your head to discover you are looking at yesterday.
    #[test]
    fn an_age_climbs_through_the_units() {
        assert_eq!(age_compact(45.0), "45s");
        assert_eq!(age_compact(60.0), "1m");
        assert_eq!(age_compact(183.0 * 60.0), "3h");
        assert_eq!(age_compact(507.0 * 60.0), "8h");
        assert_eq!(age_compact(86_400.0), "1d");
        assert_eq!(age_compact(3.0 * 86_400.0), "3d");
    }

    /// The boundaries land on the unit above, not one short of it.
    #[test]
    fn the_boundaries_are_clean() {
        assert_eq!(age_compact(59.0), "59s");
        assert_eq!(age_compact(3_599.0), "59m");
        assert_eq!(age_compact(3_600.0), "1h");
        assert_eq!(age_compact(86_399.0), "23h");
    }
}

#[cfg(test)]
mod usd_price_tests {
    use super::usd_price;

    #[test]
    fn a_memecoin_price_keeps_its_significant_digits() {
        // The value behind "$2.56k market cap" on a 731163386 tokens/ETH pool.
        assert_eq!(usd_price(0.000_002_56), "$0.0₅256");
        assert_eq!(usd_price(0.000_000_001_23), "$0.0₈123");
    }

    #[test]
    fn ordinary_prices_are_written_as_ordinary_prices() {
        assert_eq!(usd_price(1.5), "$1.5000");
        // Three significant digits, not six decimals: `$0.012500` padded two
        // places that carry nothing.
        assert_eq!(usd_price(0.0125), "$0.0125");
    }

    /// The form an LP range needs: two of these have to sit inside one column.
    #[test]
    fn the_brief_form_drops_what_it_does_not_need() {
        use super::usd_price_brief;
        assert_eq!(usd_price_brief(0.020_137), "$0.02");
        assert_eq!(usd_price_brief(0.08), "$0.08");
        // Still says something below a cent, where rounding to $0.00 would not.
        assert_eq!(usd_price_brief(0.000_002_56), "$0.0₅26");
    }

    #[test]
    fn nothing_is_not_priced() {
        assert_eq!(usd_price(0.0), "—");
        assert_eq!(usd_price(f64::NAN), "—");
    }
}

#[cfg(test)]
mod lp_bound_tests {
    use super::usd_price_brief;

    /// An LP bound can run to eight figures. Digits stop being readable long
    /// before that, and the column is nineteen wide.
    #[test]
    fn a_large_bound_is_written_in_magnitude() {
        assert_eq!(usd_price_brief(63_205_347.0), "$63.21M");
        assert_eq!(usd_price_brief(8_210.0), "$8.21k");
    }

    /// Small bounds keep their significant digits — that is the end of the
    /// range anyone is actually reading.
    #[test]
    fn a_small_bound_keeps_its_digits() {
        assert_eq!(usd_price_brief(0.02), "$0.02");
        assert_eq!(usd_price_brief(0.000_002_56), "$0.0₅26");
    }
}
