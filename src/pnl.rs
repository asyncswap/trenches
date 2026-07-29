// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The PnL calendar (`L`).
//!
//! A month of trading as a grid, one cell per day, coloured by whether that day
//! made money. A running total tells you nothing about *when* — it is the days
//! that repeat that are worth knowing about, and a shape is the only way to see
//! a run of them.
//!
//! Everything here reads [`crate::ledger`] and nothing else. It never touches
//! the trading loop, the RPC or the wallet: opening it cannot cost you a trade,
//! and it works with the network down.

use std::io::Stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use crate::ledger::{self, Date, Fill};
use crate::ui::widgets::{self, themed_block, tone_color};
use crate::view::Tone;

type Term = Terminal<CrosstermBackend<Stdout>>;

/// How far back the summary line looks. Bound to `1` through `5`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Day,
    Week,
    Month,
    Year,
    All,
}

impl Range {
    /// Days counted back from today. All-time gets a span wider than any
    /// plausible history rather than a special case at every call site.
    fn days(self) -> i64 {
        match self {
            Range::Day => 1,
            Range::Week => 7,
            Range::Month => 30,
            Range::Year => 365,
            Range::All => i64::MAX / 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Range::Day => "1D",
            Range::Week => "7D",
            Range::Month => "30D",
            Range::Year => "1Y",
            Range::All => "ALL",
        }
    }
}

/// How many week-rows a month occupies, Monday-first. Between 4 and 6.
fn weeks_in(year: i32, month: u32) -> usize {
    let lead = ledger::weekday(year, month, 1) as usize;
    (lead + ledger::days_in_month(year, month) as usize).div_ceil(7)
}

/// A day's trading, already totalled.
struct Day {
    pnl_usd: f64,
    trades: usize,
    wins: usize,
}

/// Total a slice of fills into one day's figures.
fn summarise(fills: &[&Fill]) -> Day {
    Day {
        pnl_usd: fills.iter().map(|f| f.usd()).sum(),
        trades: fills.len(),
        wins: fills.iter().filter(|f| f.pnl > 0.0).count(),
    }
}

/// Dollars, compact enough for a calendar cell. `$1.2K`, `-$430`, `$0`.
///
/// A cell is nine columns wide and has to carry a sign, so the usual two
/// decimals are not affordable — and on a day that made four figures they are
/// not information either.
fn money(v: f64) -> String {
    let sign = if v < 0.0 { "-" } else { "" };
    let a = v.abs();
    if a >= 1_000_000.0 {
        format!("{sign}${:.1}M", a / 1e6)
    } else if a >= 1_000.0 {
        format!("{sign}${:.1}K", a / 1e3)
    } else if a >= 1.0 {
        format!("{sign}${a:.2}")
    } else if a > 0.0 {
        // Sub-dollar days are still days you traded; rounding them to $0 would
        // make a cell look empty when it is not.
        format!("{sign}${a:.3}")
    } else {
        "$0".into()
    }
}

/// Mix a colour toward the panel background.
///
/// A tile painted in the full tone is too loud to read a figure off, and the
/// figure is the only reason the tile exists. A tint keeps the green/red signal
/// while leaving enough contrast for the number on top of it.
fn tint(c: ratatui::style::Color, amount: f64) -> ratatui::style::Color {
    use ratatui::style::Color;
    let (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) = (c, widgets::bg_panel()) else {
        return c;
    };
    let mix = |a: u8, b: u8| ((a as f64) * amount + (b as f64) * (1.0 - amount)) as u8;
    Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb))
}

/// The colour a figure carries: green up, red down, dim flat.
fn pnl_tone(v: f64) -> Tone {
    if v > 0.0 {
        Tone::Good
    } else if v < 0.0 {
        Tone::Bad
    } else {
        Tone::Dim
    }
}

/// The calendar screen. Blocks until Esc, then hands the terminal back.
pub fn screen(term: &mut Term) -> eyre::Result<()> {
    // This screen owns the terminal now: take down any image the last one left.
    // Clearing also marks every placement stale, so the dashboard redraws its
    // logo when we return.
    crate::ui::image::clear();

    // Read once, here, on the way in.
    //
    // On demand is the whole of it: this screen owns the terminal while it is
    // open, so no trade can land underneath it and there is nothing to poll
    // for. Every time you press `L` you get the ledger as it stands, today
    // included; the days behind it were settled when they ended and do not
    // change.
    let fills = ledger::load_all();
    let today = ledger::date_of(ledger::now());
    let (mut year, mut month) = (today.y, today.m);
    // Which day the winners/losers list is showing. `None` = the whole month,
    // which is what you want the moment it opens.
    let mut sel: Option<u32> = None;
    let mut range = Range::Week;

    loop {
        term.draw(|f| draw(f, &fills, year, month, sel, range, today))?;

        crate::ui_alive();

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('L') => return Ok(()),
            // Months on the arrows. The selected day is dropped rather than
            // carried across: "the 31st" does not exist in every month, and a
            // selection that silently moved to a different date would be worse
            // than none.
            KeyCode::Left => {
                let (y, m) = ledger::shift_month(year, month, -1);
                year = y;
                month = m;
                sel = None;
            }
            KeyCode::Right => {
                let (y, m) = ledger::shift_month(year, month, 1);
                year = y;
                month = m;
                sel = None;
            }
            // hjkl walks the grid itself — h/l a day, j/k a week, because a
            // week is what the grid's rows literally are. One set of keys for
            // moving through the data, which is how every other list in the app
            // behaves.
            // Landing on a day snaps the range back to a single day, whatever it
            // was before. A "30D" total sitting above one highlighted date is
            // asking to be misread as that date's.
            KeyCode::Char('h') => {
                sel = step_day(sel, -1, year, month, today);
                range = Range::Day;
            }
            KeyCode::Char('l') => {
                sel = step_day(sel, 1, year, month, today);
                range = Range::Day;
            }
            KeyCode::Char('k') => {
                sel = step_day(sel, -7, year, month, today);
                range = Range::Day;
            }
            KeyCode::Char('j') => {
                sel = step_day(sel, 7, year, month, today);
                range = Range::Day;
            }
            KeyCode::Char('1') => range = Range::Day,
            KeyCode::Char('2') => range = Range::Week,
            KeyCode::Char('3') => range = Range::Month,
            KeyCode::Char('4') => range = Range::Year,
            KeyCode::Char('5') => range = Range::All,
            // Back to the whole month, and back to this month.
            KeyCode::Backspace => sel = None,
            KeyCode::Char('t') => {
                year = today.y;
                month = today.m;
                sel = None;
            }
            _ => {}
        }
    }
}

/// Move the selected day, clamped to the month. No selection means start from
/// the first or last day, depending on which way you asked to go.
fn step_day(sel: Option<u32>, by: i32, y: i32, m: u32, today: Date) -> Option<u32> {
    let last = ledger::days_in_month(y, m);
    let cur = match sel {
        Some(d) => d as i32,
        // Nothing selected yet. On the month you are actually in, the first
        // press lands on today — that is where you are, and counting from the
        // 1st to find it is work the app can do for you. On any other month
        // there is no "here", so it starts at whichever end you came from.
        None => {
            if today.y == y && today.m == m {
                return Some(today.d);
            }
            if by > 0 {
                0
            } else {
                last as i32 + 1
            }
        }
    };
    Some((cur + by).clamp(1, last as i32) as u32)
}

fn draw(
    f: &mut Frame,
    fills: &[Fill],
    year: i32,
    month: u32,
    sel: Option<u32>,
    range: Range,
    today: Date,
) {
    widgets::paint_bg(f);

    let rows = Layout::vertical([
        Constraint::Length(3),  // totals
        // Sized to the month on screen: a heading, then three rows per week with
        // a blank row between them, plus the block's own border. A fixed height
        // either clipped February or left a hole under a long month.
        Constraint::Length(weeks_in(year, month) as u16 * 4 + 2), // the grid
        Constraint::Min(5),     // winners / losers
        Constraint::Length(1),  // keys
    ])
    .split(f.area());

    // Bucket this month's fills by day once, rather than filtering the whole
    // ledger inside all 42 cells.
    let mut by_day: Vec<Vec<&Fill>> = vec![Vec::new(); 32];
    for fl in fills {
        let d = ledger::date_of(fl.ts);
        if d.y == year && d.m == month {
            by_day[d.d as usize].push(fl);
        }
    }

    header(f, rows[0], fills, &by_day, year, month, range, sel, today);
    grid(f, rows[1], &by_day, year, month, sel, today);
    breakdown(f, rows[2], &by_day, year, month, sel);
    keys(f, rows[3]);
}

/// Range total, month total, and how the days split.
#[allow(clippy::too_many_arguments)]
fn header(
    f: &mut Frame,
    area: Rect,
    fills: &[Fill],
    by_day: &[Vec<&Fill>],
    year: i32,
    month: u32,
    range: Range,
    sel: Option<u32>,
    today: Date,
) {
    // The range is counted back from TODAY, not from the month on screen —
    // "last 7 days" means the last seven days whichever month you are looking at.
    let cutoff = ledger::days_from_civil(today.y, today.m, today.d) - (range.days() - 1);
    let recent: Vec<&Fill> = fills
        .iter()
        .filter(|f| {
            let d = ledger::date_of(f.ts);
            ledger::days_from_civil(d.y, d.m, d.d) >= cutoff
        })
        .collect();
    // With a day selected, "1D" means THAT day rather than today — the cursor is
    // the thing you are asking about. Widen the range and it goes back to being
    // a window counted off today, which is the only reading "30D" can have.
    let r = match sel {
        Some(d) if range == Range::Day => summarise(&by_day[d as usize]),
        _ => summarise(&recent),
    };

    let month_fills: Vec<&Fill> = fills
        .iter()
        .filter(|f| {
            let d = ledger::date_of(f.ts);
            d.y == year && d.m == month
        })
        .collect();
    let m = summarise(&month_fills);

    // Neither of these pads. Spacing is decided below, where it can be measured
    // against the box; a closure that quietly adds its own is a closure whose
    // output is wider than the string the layout was computed from.
    let label = |s: &str| {
        Span::styled(
            s.to_string(),
            Style::default().fg(widgets::border_color()).add_modifier(Modifier::BOLD),
        )
    };
    let val = |v: f64| {
        Span::styled(
            money(v),
            Style::default().fg(tone_color(pnl_tone(v))).add_modifier(Modifier::BOLD),
        )
    };
    let dim = |s: String| Span::styled(s, Style::default().fg(tone_color(Tone::Dim)));

    // The centre follows the cursor: while a day is selected it reports that
    // day, and falls back to the month once the selection is cleared. Reading a
    // month's win rate while a single day is highlighted invites you to read it
    // as that day's.
    let focus = match sel {
        Some(d) => summarise(&by_day[d as usize]),
        None => summarise(&month_fills),
    };
    let scope = match sel {
        Some(d) => format!("{} {d}", ledger::MONTHS[(month - 1) as usize]),
        None => "MONTH".to_string(),
    };
    let rate = if focus.trades == 0 {
        format!("{scope} · no trades")
    } else {
        format!(
            "{scope} · {}/{} Wins · {:.0}% Win rate",
            focus.wins,
            focus.trades,
            focus.wins as f64 / focus.trades as f64 * 100.0
        )
    };
    // Range left, MONTH right, the win rate centred between them — and the two
    // sides mirror, so each label sits against its own edge with its figure
    // inboard of it.
    //
    // Every width here is the width of a span that actually gets rendered. The
    // previous version measured strings the closures then padded, so the row
    // came out six columns wider than the box and the right-hand figure was
    // truncated at the border.
    let l_lab = format!("{} ", range.label());
    let l_val = money(r.pnl_usd);
    let r_val = money(m.pnl_usd);
    let r_lab = " MONTH";

    let inner = area.width.saturating_sub(2) as usize;
    let used = l_lab.chars().count()
        + l_val.chars().count()
        + rate.chars().count()
        + r_val.chars().count()
        + r_lab.chars().count();
    let slack = inner.saturating_sub(used);
    // Split so the two gaps add back to exactly the slack. Halving twice loses
    // the odd column, and losing it on the right is what pushes past the edge.
    let gap_l = slack / 2;
    let gap_r = slack - gap_l;

    let body = vec![Line::from(vec![
        label(&l_lab),
        val(r.pnl_usd),
        Span::raw(" ".repeat(gap_l)),
        dim(rate),
        Span::raw(" ".repeat(gap_r)),
        val(m.pnl_usd),
        label(r_lab),
    ])];

    f.render_widget(
        Paragraph::new(body).block(themed_block(format!(
            " PnL — {} {} ",
            ledger::MONTHS[(month - 1) as usize],
            year
        ))),
        area,
    );
}

/// The month grid: a week per row, Monday first.
fn grid(
    f: &mut Frame,
    area: Rect,
    by_day: &[Vec<&Fill>],
    year: i32,
    month: u32,
    sel: Option<u32>,
    today: Date,
) {
    const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    // The figure's width. Nine columns holds the widest thing money() produces
    // (`-$999.99`, `-$999.9K`) with a column to spare either side.
    const W: usize = 9;
    // The tile including its border. A box eleven columns wide and three rows
    // tall is about 11:6 once the terminal's 1:2 cell is accounted for; at
    // thirteen columns it read as a wide, flat slab.
    const BOX: usize = W + 2;
    // Between tiles. Boxes used to share their edge column, so two traded days
    // side by side joined into what looked like one table rather than two days.
    const GAP: usize = 2;

    // Centred in the panel. Seven fixed-width columns do not grow with the box,
    // so on a wide terminal the whole month sat against the left edge with half
    // the panel empty beside it.
    let grid_w = (BOX * 7 + GAP * 6) as u16;
    let pad = " ".repeat(((area.width.saturating_sub(2).saturating_sub(grid_w)) / 2) as usize);

    let mut lines: Vec<Line> = Vec::new();
    let mut head: Vec<Span> = vec![Span::raw(pad.clone())];
    for (i, n) in NAMES.iter().enumerate() {
        if i > 0 {
            head.push(Span::raw(" ".repeat(GAP)));
        }
        head.push(Span::styled(
            format!("{n:^BOX$}"),
            Style::default().fg(widgets::border_color()).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(head));

    let lead = ledger::weekday(year, month, 1) as usize;
    let last = ledger::days_in_month(year, month) as usize;

    // The day in a given slot, or None where the month has not started or has
    // already ended.
    let day_at = |week: usize, wd: usize| -> Option<u32> {
        let idx = week * 7 + wd;
        (idx >= lead && idx - lead < last).then(|| (idx - lead + 1) as u32)
    };
    let weeks = weeks_in(year, month);

    // Where the cursor is, as a (week, weekday) pair.
    let cursor = sel.and_then(|d| {
        (1..=last as u32).contains(&d).then(|| {
            let idx = lead + d as usize - 1;
            (idx / 7, idx % 7)
        })
    });

    for week in 0..weeks {
        // A blank row between weeks, so one week's tiles do not sit against the
        // next week's.
        if week > 0 {
            lines.push(Line::from(""));
        }

        // Three rows per week, and the box is drawn INSIDE them rather than
        // around them — an outlined day is exactly as tall as a plain one.
        let mut top: Vec<Span> = vec![Span::raw(pad.clone())];
        let mut mid: Vec<Span> = vec![Span::raw(pad.clone())];
        let mut bot: Vec<Span> = vec![Span::raw(pad.clone())];

        for wd in 0..7 {
            if wd > 0 {
                for row in [&mut top, &mut mid, &mut bot] {
                    row.push(Span::raw(" ".repeat(GAP)));
                }
            }
            let Some(d) = day_at(week, wd) else {
                for row in [&mut top, &mut mid, &mut bot] {
                    row.push(Span::raw(" ".repeat(BOX)));
                }
                continue;
            };
            let day = summarise(&by_day[d as usize]);
            let is_today = today.y == year && today.m == month && today.d == d;
            let text = if day.trades == 0 { format!("{d}") } else { money(day.pnl_usd) };

            let is_cursor = cursor == Some((week, wd));
            if is_cursor || day.trades > 0 {
                // One shape for every day worth looking at: a solid block in
                // the day's own colour. Outlines for traded days and a fill for
                // the cursor meant the grid used two languages to say one thing,
                // and moving the cursor changed a day's shape as well as its
                // brightness.
                //
                // The cursor is the same block, brighter — so it carries each
                // day's colour with it as it moves rather than replacing it, and
                // a day you never traded lights up in the accent because it has
                // no colour of its own to keep.
                let tone = if day.trades == 0 { Tone::Accent } else { pnl_tone(day.pnl_usd) };
                let strength = if is_cursor { 0.55 } else { 0.22 };
                let st = Style::default()
                    .bg(tint(tone_color(tone), strength))
                    // The day's colour at full strength on a muted block. Dark
                    // type worked when the fill was near-solid; against a
                    // quieter one it is the figure that should carry the colour
                    // and the fill that should recede.
                    .fg(tone_color(tone))
                    .add_modifier(Modifier::BOLD);
                top.push(Span::styled(" ".repeat(BOX), st));
                mid.push(Span::styled(format!("{text:^BOX$}"), st));
                bot.push(Span::styled(" ".repeat(BOX), st));
            } else {
                let fg = if is_today { Tone::Accent } else { Tone::Dim };
                let mut st = Style::default().fg(tone_color(fg));
                if is_today {
                    st = st.add_modifier(Modifier::BOLD);
                }
                top.push(Span::raw(" ".repeat(BOX)));
                mid.push(Span::styled(format!("{text:^BOX$}"), st));
                bot.push(Span::raw(" ".repeat(BOX)));
            }
        }

        lines.push(Line::from(top));
        lines.push(Line::from(mid));
        lines.push(Line::from(bot));
    }

    f.render_widget(Paragraph::new(lines).block(themed_block(" Calendar ")), area);
}

/// Winners and losers for the selected day, or for the whole month.
fn breakdown(f: &mut Frame, area: Rect, by_day: &[Vec<&Fill>], year: i32, month: u32, sel: Option<u32>) {
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);

    let mut trades: Vec<&Fill> = match sel {
        Some(d) => by_day[d as usize].clone(),
        None => by_day.iter().flatten().copied().collect(),
    };
    // Biggest first, in both directions — the tail of a list of 40 trades is
    // not what anyone opened this to see.
    trades.sort_by(|a, b| b.usd().partial_cmp(&a.usd()).unwrap_or(std::cmp::Ordering::Equal));

    let scope = match sel {
        Some(d) => format!("{d} {} {year}", ledger::MONTHS[(month - 1) as usize]),
        None => format!("{} {year}", ledger::MONTHS[(month - 1) as usize]),
    };

    let row = |fl: &Fill| {
        Line::from(vec![
            Span::styled(
                format!("{:<12}", truncate(&fl.sym, 12)),
                Style::default().fg(tone_color(Tone::Normal)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>10}  ", money(fl.usd())),
                Style::default().fg(tone_color(pnl_tone(fl.pnl))).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>8}  ", fl.ret_col()),
                Style::default().fg(tone_color(pnl_tone(fl.pnl))),
            ),
            Span::styled(
                format!("{:>7}  ", fl.held()),
                Style::default().fg(tone_color(Tone::Info)),
            ),
            Span::styled(
                // Scaled, like everywhere else: at four decimals a winning
                // trade and a losing one both read "0.0000", which is the one
                // thing this column exists to tell apart.
                format!("{} {}", crate::view::eth(fl.pnl), fl.quote_sym),
                Style::default().fg(tone_color(Tone::Dim)),
            ),
        ])
    };

    let empty = |what: &str| {
        vec![Line::from(Span::styled(
            format!("  no {what} in {scope}"),
            Style::default().fg(tone_color(Tone::Dim)),
        ))]
    };

    let wins: Vec<Line> = {
        let v: Vec<Line> = trades.iter().filter(|f| f.pnl > 0.0).map(|f| row(f)).collect();
        if v.is_empty() { empty("winners") } else { v }
    };
    let losses: Vec<Line> = {
        let mut v: Vec<&&Fill> = trades.iter().filter(|f| f.pnl < 0.0).collect();
        v.reverse(); // worst first
        let v: Vec<Line> = v.into_iter().map(|f| row(f)).collect();
        if v.is_empty() { empty("losers") } else { v }
    };

    f.render_widget(
        Paragraph::new(wins).block(themed_block(format!(" Winners — {scope} "))),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(losses).block(themed_block(format!(" Losers — {scope} "))),
        cols[1],
    );
}

fn keys(f: &mut Frame, area: Rect) {
    let dim = Style::default().fg(tone_color(Tone::Dim));
    let key = Style::default().fg(tone_color(Tone::Info)).add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  h j k l", key),
            Span::styled(" day   ", dim),
            Span::styled("← →", key),
            Span::styled(" month   ", dim),
            Span::styled("1 2 3 4 5", key),
            Span::styled(" 1D 7D 30D 1Y ALL   ", dim),
            Span::styled("bksp", key),
            Span::styled(" whole month   ", dim),
            Span::styled("t", key),
            Span::styled(" today   ", dim),
            Span::styled("esc", key),
            Span::styled(" back", dim),
        ])),
        area,
    );
}

/// Cut a ticker to fit, with an ellipsis so a truncated name cannot be mistaken
/// for a shorter one.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(ts: u64, pnl: f64, usd: f64) -> Fill {
        Fill {
            ts,
            chain: "test".into(),
            sym: "AAA".into(),
            token: "0x1".into(),
            pnl,
            cost: 1.0,
            proceeds: 1.0 + pnl,
            quote_sym: "ETH".into(),
            quote_usd: usd,
            tx: "0x0".into(),
            held_secs: Some(7),
        }
    }

    /// Render the grid to a test backend and read the tiles back off it.
    ///
    /// Every day worth looking at is the same shape — a solid block in its own
    /// colour — and the cursor is that block, brighter. It used to be outlines
    /// for traded days and a fill for the cursor, which meant moving the cursor
    /// changed a day's shape as well as its brightness.
    #[test]
    fn traded_days_and_the_cursor_are_both_solid_blocks() {
        use ratatui::backend::TestBackend;

        // Two traded days: one the cursor sits on, one it does not.
        let (traded, selected) = (10u32, 20u32);
        let f = fill(0, 1.0, 1.0);
        let mut by_day: Vec<Vec<&Fill>> = vec![Vec::new(); 32];
        by_day[traded as usize].push(&f);
        by_day[selected as usize].push(&f);

        let today = Date { y: 2026, m: 7, d: 1 };
        let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
        term.draw(|fr| grid(fr, fr.area(), &by_day, 2026, 7, Some(selected), today)).unwrap();
        let buf = term.backend().buffer().clone();

        // No box drawing anywhere inside the panel: one language, not two.
        for y in 1..buf.area.height - 1 {
            for x in 1..buf.area.width - 1 {
                let ch = buf[(x, y)].symbol();
                assert!(
                    !"┌┐└┘─│".contains(ch),
                    "an outline survives at {x},{y}"
                );
            }
        }

        // Two filled blocks, three rows each, in two different colours — the
        // cursor's brighter than the day it is not on. The panel background is
        // read off the buffer (its dominant colour), NOT from the live theme:
        // other tests switch the global theme in parallel, so by the time this
        // assertion runs `widgets::bg_panel()` can name a different colour
        // than the one the draw above actually painted.
        let mut fills: std::collections::BTreeMap<String, usize> = Default::default();
        for y in 1..buf.area.height - 1 {
            for x in 1..buf.area.width - 1 {
                if let Some(bg) = buf[(x, y)].style().bg {
                    *fills.entry(format!("{bg:?}")).or_default() += 1;
                }
            }
        }
        if let Some(panel) = fills.iter().max_by_key(|(_, n)| **n).map(|(c, _)| c.clone()) {
            fills.remove(&panel);
        }
        assert_eq!(fills.len(), 2, "expected two tile colours, got {fills:?}");
        for (colour, cells) in &fills {
            assert!(*cells >= 3, "{colour} covers only {cells} cells");
        }
    }

    /// A filled day occupies exactly the rows an untraded one does.
    ///
    /// The tile used to be drawn AROUND the day, adding a row above and below,
    /// which made every traded day two rows taller than its neighbours. The
    /// grid keeps one rhythm now.
    #[test]
    fn a_filled_day_is_no_taller_than_a_plain_one() {
        use ratatui::backend::TestBackend;

        let f = fill(0, 1.0, 1.0);
        let mut by_day: Vec<Vec<&Fill>> = vec![Vec::new(); 32];
        by_day[10].push(&f);
        let today = Date { y: 2026, m: 7, d: 1 };

        // Where the last day of the month lands, with and without a traded day
        // above it in the grid.
        let row_of_31 = |by_day: &Vec<Vec<&Fill>>| -> u16 {
            let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
            term.draw(|fr| grid(fr, fr.area(), by_day, 2026, 7, None, today)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .find(|&y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .contains("31")
                })
                .expect("the 31st never rendered")
        };

        assert_eq!(row_of_31(&by_day), row_of_31(&vec![Vec::new(); 32]));
    }

    /// Moving the cursor must not move anything else.
    ///
    /// The rule rows above and below the tile were drawn only around the
    /// selected week, which made that week two rows taller than the others — so
    /// every row below the cursor jumped each time it changed week. They are
    /// permanent now, and this is what says so.
    #[test]
    fn the_grid_does_not_shift_when_the_cursor_moves() {
        use ratatui::backend::TestBackend;

        let by_day: Vec<Vec<&Fill>> = vec![Vec::new(); 32];
        let today = Date { y: 2026, m: 7, d: 1 };

        // Where the last day of the month renders, under three different cursors.
        let row_of_last_day = |sel: Option<u32>| -> u16 {
            let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
            term.draw(|fr| grid(fr, fr.area(), &by_day, 2026, 7, sel, today)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .find(|&y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .contains("31")
                })
                .expect("the 31st never rendered")
        };

        let unselected = row_of_last_day(None);
        assert_eq!(row_of_last_day(Some(1)), unselected, "cursor on week 1 moved the grid");
        assert_eq!(row_of_last_day(Some(15)), unselected, "cursor on week 3 moved the grid");
        assert_eq!(row_of_last_day(Some(31)), unselected, "cursor on the last week moved the grid");
    }

    #[test]
    fn money_stays_inside_a_calendar_cell() {
        // The figure sits in 9 columns; one that overflows would shove the
        // whole week's columns out of alignment.
        // Three decimals below a dollar, two above: a day worth fractions of a
        // cent still has to read as a number rather than rounding to nothing.
        for v in [0.0, 0.42, -0.42, 9.0, -940.0, 1234.0, -98_765.0, 4_200_000.0] {
            assert!(money(v).chars().count() <= 9, "{v} → {}", money(v));
        }
        assert_eq!(money(0.0), "$0");
        assert_eq!(money(-940.0), "-$940.00");
        assert_eq!(money(0.001), "$0.001");
        assert_eq!(money(1234.0), "$1.2K");
        assert_eq!(money(4_200_000.0), "$4.2M");
    }

    #[test]
    fn a_day_totals_in_the_dollars_of_each_fill() {
        // Two trades, two different ETH prices. The day made 0.1*2000 plus
        // -0.05*3000 = $50, NOT 0.05 ETH at either rate.
        let a = fill(0, 0.1, 2000.0);
        let b = fill(0, -0.05, 3000.0);
        let d = summarise(&[&a, &b]);
        assert!((d.pnl_usd - 50.0).abs() < 1e-9);
        assert_eq!(d.trades, 2);
        assert_eq!(d.wins, 1);
    }

    #[test]
    fn day_selection_clamps_to_the_month_it_is_in() {
        // February 2024 has 29 days; stepping a week off the end must not
        // select the 32nd of February.
        // A month other than the one we are in, so "start on today" does not
        // apply and the ends are what a first press lands on.
        let elsewhere = Date { y: 2026, m: 7, d: 28 };
        assert_eq!(step_day(Some(25), 7, 2024, 2, elsewhere), Some(29));
        assert_eq!(step_day(Some(3), -7, 2024, 2, elsewhere), Some(1));
        // From no selection, forward starts at the 1st and back at the last.
        assert_eq!(step_day(None, 1, 2024, 2, elsewhere), Some(1));
        assert_eq!(step_day(None, -1, 2024, 2, elsewhere), Some(29));
        // A non-leap February stops a day earlier.
        assert_eq!(step_day(None, -1, 2023, 2, elsewhere), Some(28));
    }

    #[test]
    fn a_flat_day_and_an_untraded_day_are_not_the_same() {
        // A day whose wins and losses cancelled reads "$0"; a day you sat out
        // has no figure at all. Collapsing them would claim you broke even on a
        // day you never opened the app.
        let a = fill(0, 0.1, 1000.0);
        let b = fill(0, -0.1, 1000.0);
        let traded = summarise(&[&a, &b]);
        let idle = summarise(&[]);
        assert_eq!(traded.pnl_usd, 0.0);
        assert_eq!(traded.trades, 2);
        assert_eq!(idle.trades, 0);
    }

    #[test]
    fn tickers_that_do_not_fit_are_marked_as_cut() {
        assert_eq!(truncate("PEPE", 12), "PEPE");
        assert_eq!(truncate("SUPERLONGTICKERNAME", 12), "SUPERLONGTI…");
        assert_eq!(truncate("SUPERLONGTICKERNAME", 12).chars().count(), 12);
    }

    #[test]
    fn the_first_move_lands_on_today_in_the_current_month() {
        let today = Date { y: 2026, m: 7, d: 28 };
        // Whichever direction you press first, you start where you are.
        assert_eq!(step_day(None, 1, 2026, 7, today), Some(28));
        assert_eq!(step_day(None, -1, 2026, 7, today), Some(28));
        assert_eq!(step_day(None, 7, 2026, 7, today), Some(28));
        // A month you are only looking at has no "here" to start from.
        assert_eq!(step_day(None, 1, 2026, 6, today), Some(1));
        assert_eq!(step_day(None, -1, 2026, 6, today), Some(30));
        // Once something is selected it moves normally.
        assert_eq!(step_day(Some(28), 1, 2026, 7, today), Some(29));
    }

    #[test]
    fn ranges_count_whole_days_back_including_today() {
        // "1D" is today, not the last 24 hours — a trade this morning belongs
        // to today however many hours ago it was.
        assert_eq!(Range::Day.days(), 1);
        assert_eq!(Range::Week.days(), 7);
        assert_eq!(Range::Month.days(), 30);
        assert_eq!(Range::Year.days(), 365);
    }

    #[test]
    fn all_time_reaches_back_past_any_fill_without_overflowing() {
        // All-time is a very large span rather than a special case, so the
        // cutoff subtraction has to stay inside i64 for it to be safe.
        let today = ledger::days_from_civil(2026, 7, 28);
        let cutoff = today.checked_sub(Range::All.days() - 1);
        assert!(cutoff.is_some(), "all-time cutoff overflowed");
        // And it must predate anything a ledger could hold — day 0 is 1970.
        assert!(cutoff.unwrap() < ledger::days_from_civil(1970, 1, 1));
    }
}
