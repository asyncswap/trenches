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

/// How far back the summary line looks. Bound to `1` / `2` / `3`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    fn days(self) -> i64 {
        match self {
            Range::Day => 1,
            Range::Week => 7,
            Range::Month => 30,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Range::Day => "1D",
            Range::Week => "7D",
            Range::Month => "30D",
        }
    }
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
        format!("{sign}${a:.0}")
    } else if a > 0.0 {
        // Sub-dollar days are still days you traded; rounding them to $0 would
        // make a cell look empty when it is not.
        format!("{sign}${a:.2}")
    } else {
        "$0".into()
    }
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

    let fills = ledger::load_all();
    let today = ledger::date_of(ledger::now());
    let (mut year, mut month) = (today.y, today.m);
    // Which day the winners/losers list is showing. `None` = the whole month,
    // which is what you want the moment it opens.
    let mut sel: Option<u32> = None;
    let mut range = Range::Week;

    loop {
        term.draw(|f| draw(f, &fills, year, month, sel, range, today))?;

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
            KeyCode::Char('h') => sel = step_day(sel, -1, year, month),
            KeyCode::Char('l') => sel = step_day(sel, 1, year, month),
            KeyCode::Char('k') => sel = step_day(sel, -7, year, month),
            KeyCode::Char('j') => sel = step_day(sel, 7, year, month),
            KeyCode::Char('1') => range = Range::Day,
            KeyCode::Char('2') => range = Range::Week,
            KeyCode::Char('3') => range = Range::Month,
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
fn step_day(sel: Option<u32>, by: i32, y: i32, m: u32) -> Option<u32> {
    let last = ledger::days_in_month(y, m);
    let cur = match sel {
        Some(d) => d as i32,
        None => {
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
        Constraint::Length(20), // the grid
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

    header(f, rows[0], fills, year, month, range, today);
    grid(f, rows[1], &by_day, year, month, sel, today);
    breakdown(f, rows[2], &by_day, year, month, sel);
    keys(f, rows[3]);
}

/// Range total, month total, and how the days split.
fn header(
    f: &mut Frame,
    area: Rect,
    fills: &[Fill],
    year: i32,
    month: u32,
    range: Range,
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
    let r = summarise(&recent);

    let month_fills: Vec<&Fill> = fills
        .iter()
        .filter(|f| {
            let d = ledger::date_of(f.ts);
            d.y == year && d.m == month
        })
        .collect();
    let m = summarise(&month_fills);

    let label = |s: &str| {
        Span::styled(
            format!("{s} "),
            Style::default().fg(widgets::border_color()).add_modifier(Modifier::BOLD),
        )
    };
    let val = |v: f64| {
        Span::styled(
            format!("{}  ", money(v)),
            Style::default().fg(tone_color(pnl_tone(v))).add_modifier(Modifier::BOLD),
        )
    };
    let dim = |s: String| Span::styled(s, Style::default().fg(tone_color(Tone::Dim)));

    let win_rate = |d: &Day| {
        if d.trades == 0 {
            "no trades".to_string()
        } else {
            format!("{}/{} won  ", d.wins, d.trades)
        }
    };

    let body = vec![Line::from(vec![
        label(range.label()),
        val(r.pnl_usd),
        dim(win_rate(&r)),
        label("   MONTH"),
        val(m.pnl_usd),
        dim(win_rate(&m)),
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
    // Every cell is the same width, so the columns line up under their headings
    // without a table widget and its borders eating four rows of a fourteen-row
    // box. One space between cells reads as a gap between tiles.
    const W: usize = 11;
    const GAP: &str = " ";

    // Centred in the panel. Seven fixed-width columns do not grow with the box,
    // so on a wide terminal the whole month sat against the left edge with half
    // the panel empty beside it.
    let grid_w = (W * 7 + 6) as u16;
    let pad = " ".repeat(((area.width.saturating_sub(2).saturating_sub(grid_w)) / 2) as usize);

    let mut lines: Vec<Line> = Vec::new();
    let mut head: Vec<Span> = vec![Span::raw(pad.clone())];
    for (i, n) in NAMES.iter().enumerate() {
        if i > 0 {
            head.push(Span::raw(GAP));
        }
        head.push(Span::styled(
            format!("{n:^W$}"),
            Style::default().fg(widgets::border_color()).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(head));

    let lead = ledger::weekday(year, month, 1) as usize;
    let last = ledger::days_in_month(year, month) as usize;

    // 6 rows covers every month layout — 31 days starting on a Sunday is the
    // worst case at 37 cells.
    for week in 0..6 {
        // Two lines per week: the date, then what it made.
        let mut top: Vec<Span> = vec![Span::raw(pad.clone())];
        let mut bot: Vec<Span> = vec![Span::raw(pad.clone())];
        let mut any = false;

        for wd in 0..7 {
            let idx = week * 7 + wd;
            if wd > 0 {
                top.push(Span::raw(GAP));
                bot.push(Span::raw(GAP));
            }
            if idx < lead || idx - lead >= last {
                top.push(Span::raw(" ".repeat(W)));
                bot.push(Span::raw(" ".repeat(W)));
                continue;
            }
            any = true;
            let d = (idx - lead + 1) as u32;
            let day = summarise(&by_day[d as usize]);
            let is_today = today.y == year && today.m == month && today.d == d;
            let is_sel = sel == Some(d);

            // The date. Selected wins over today: today is a fact you can
            // re-derive, the selection is the one you just made.
            // The tile's own background, shared by both of its rows so the cell
            // reads as one block rather than two lines that happen to line up.
            let fill = if is_sel {
                Some(widgets::bg_selection())
            } else if day.trades > 0 {
                Some(widgets::bg_highlight())
            } else {
                None
            };
            let tile = |st: Style| match fill {
                Some(bg) => st.bg(bg),
                None => st,
            };

            let date_style = tile(if is_sel {
                Style::default().fg(widgets::fg_highlight()).add_modifier(Modifier::BOLD)
            } else if is_today {
                Style::default().fg(tone_color(Tone::Accent)).add_modifier(Modifier::BOLD)
            } else if day.trades > 0 {
                Style::default().fg(tone_color(Tone::Normal))
            } else {
                Style::default().fg(tone_color(Tone::Dim))
            });
            top.push(Span::styled(format!("{:^W$}", format!("{d}")), date_style));

            // The figure. A day with no trades gets a rule rather than "$0" —
            // a flat day and a day you sat out are different facts.
            let (text, tone) = if day.trades == 0 {
                ("·".to_string(), Tone::Dim)
            } else {
                (money(day.pnl_usd), pnl_tone(day.pnl_usd))
            };
            let mut st = tile(Style::default().fg(tone_color(tone)));
            if day.trades > 0 {
                st = st.add_modifier(Modifier::BOLD);
            }
            bot.push(Span::styled(format!("{text:^W$}"), st));
        }

        if !any {
            break;
        }
        lines.push(Line::from(top));
        lines.push(Line::from(bot));
        // A blank row between weeks: without it the tiles stack into one column
        // of colour and the week boundaries disappear.
        lines.push(Line::from(""));
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
                format!("{:>8}  ", format!("{:+.0}%", fl.ret_pct())),
                Style::default().fg(tone_color(pnl_tone(fl.pnl))),
            ),
            Span::styled(
                format!("{:>7}  ", fl.held()),
                Style::default().fg(tone_color(Tone::Info)),
            ),
            Span::styled(
                format!("{:.4} {}", fl.pnl, fl.quote_sym),
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
            Span::styled("1 2 3", key),
            Span::styled(" 1D 7D 30D   ", dim),
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

    #[test]
    fn money_stays_inside_a_calendar_cell() {
        // Every cell is 11 columns; a figure that overflows would shove the
        // whole week's columns out of alignment.
        for v in [0.0, 0.42, -0.42, 9.0, -940.0, 1234.0, -98_765.0, 4_200_000.0] {
            assert!(money(v).chars().count() <= 9, "{v} → {}", money(v));
        }
        assert_eq!(money(0.0), "$0");
        assert_eq!(money(-940.0), "-$940");
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
        assert_eq!(step_day(Some(25), 7, 2024, 2), Some(29));
        assert_eq!(step_day(Some(3), -7, 2024, 2), Some(1));
        // From no selection, forward starts at the 1st and back at the last.
        assert_eq!(step_day(None, 1, 2024, 2), Some(1));
        assert_eq!(step_day(None, -1, 2024, 2), Some(29));
        // A non-leap February stops a day earlier.
        assert_eq!(step_day(None, -1, 2023, 2), Some(28));
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
    fn ranges_count_whole_days_back_including_today() {
        // "1D" is today, not the last 24 hours — a trade this morning belongs
        // to today however many hours ago it was.
        assert_eq!(Range::Day.days(), 1);
        assert_eq!(Range::Week.days(), 7);
        assert_eq!(Range::Month.days(), 30);
    }
}
