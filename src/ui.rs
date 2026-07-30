// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Native ratatui selection screens — arrow-key list menus and a masked
//! password field — so the whole bot (selection + trading) is one TUI app.
//!
//! `widgets` holds the chain-agnostic renderers (table / panel / scatter / help)
//! that draw `view::*` models, shared by every chain adapter.

pub mod bigtext;
pub mod image;
pub mod logo;
pub mod markdown;
pub mod mouse;
pub mod widgets;

use std::io::Stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use ratatui::{prelude::*, widgets::*};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Arrow-key list selection. Returns the chosen index, or None if cancelled.
pub fn select(term: &mut Term, title: &str, items: &[String]) -> eyre::Result<Option<usize>> {
    // This screen owns the terminal now: take down any image the previous one
    // left, which also marks every placement stale so it redraws on return.
    image::clear();
    let mut state = ListState::default();
    state.select(Some(0));
    loop {
        term.draw(|f| {
            widgets::paint_bg(f);
            let area = centered(f.area(), 80, items.len() as u16 + 4);
            let list = List::new(items.iter().map(|s| ListItem::new(s.as_str())))
                .block(
                    widgets::themed_block(format!(" {title} ")).title_bottom(" ↑/↓ move   enter select   q quit "),
                )
                .highlight_style(Style::default().fg(widgets::bg_base()).bg(widgets::tone_color(crate::view::Tone::Info)))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, area, &mut state);
        })?;
        crate::ui_alive();
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some(if i == 0 { items.len() - 1 } else { i - 1 }));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some((i + 1) % items.len()));
                    }
                    KeyCode::Enter => return Ok(state.selected()),
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

/// One row of a `select_table`.
pub struct PickRow {
    pub cells: Vec<String>,
}

impl PickRow {
    pub fn new<I: Into<String>>(cells: impl IntoIterator<Item = I>) -> PickRow {
        PickRow { cells: cells.into_iter().map(Into::into).collect() }
    }
}

/// A table picker with column headings and the chain's logo beside it.
///
/// A plain list forced every field into one padded string, so numbers never
/// lined up. Columns keep them aligned.
///
/// The art sits in a square panel to the RIGHT at the same height and follows
/// the highlighted row: a real PNG where the terminal supports graphics, block
/// art everywhere else.
pub fn select_table(
    term: &mut Term,
    title: &str,
    headers: &[&str],
    widths: &[u16],
    rows: &[PickRow],
    logo_for: impl Fn(usize) -> Option<&'static widgets::logo_reexport::Logo>,
    png_for: impl Fn(usize) -> Option<&'static [u8]>,
) -> eyre::Result<Option<usize>> {
    // This screen owns the terminal now: take down any image the previous one
    // left, which also marks every placement stale so it redraws on return.
    image::clear();
    let first = 0usize;
    let mut state = TableState::default();
    state.select(Some(first));
    let step = |from: usize, down: bool| -> usize {
        let n = rows.len().max(1);
        if down { (from + 1) % n } else { (from + n - 1) % n }
    };

    let mut art = image::Placement::default();
    let mut art_box: Option<(u16, u16, u16, u16)> = None;
    // Terminal size travels with the placement key: a resize drops placed
    // images without moving this box, so without it the logo never comes back.
    let mut term_size = (0u16, 0u16);

    let result = loop {
        let sel = state.selected().unwrap_or(first);
        term.draw(|f| {
            widgets::paint_bg(f);
            term_size = (f.area().width, f.area().height);
            // The mark's height in rows, chosen rather than inherited.
            //
            // This used to fall out of the list length: two chains meant a
            // six-row panel meant a four-row logo, so the better the list got
            // the smaller the artwork became. Twelve rows is about three times
            // that, and it is what makes the chain picker feel like a choice
            // rather than a form.
            const ART: u16 = 12;
            let list_h = rows.len() as u16 + if headers.is_empty() { 4 } else { 5 };
            // Never taller than the terminal — a panel that overflows is
            // centred off the top and loses its first row.
            let h = list_h.max(ART + 2).min(f.area().height.saturating_sub(2).max(6));
            // Cells are about twice as tall as wide, so a square mark needs
            // roughly twice as many columns as rows.
            let art_w = h.saturating_sub(2) * 2;
            let table_w: u16 = widths.iter().sum::<u16>() + widths.len() as u16 + 4;
            let area = centered(f.area(), table_w + art_w, h);
            let cols = Layout::horizontal([Constraint::Min(30), Constraint::Length(art_w)]).split(area);

            // No headers means a plain list: skip the header row entirely
            // rather than leaving a blank line where it would have been.
            let hdr = ratatui::widgets::Row::new(headers.to_vec()).style(
                Style::default()
                    .fg(widgets::tone_color(crate::view::Tone::Info))
                    .add_modifier(Modifier::BOLD),
            );
            let trows: Vec<ratatui::widgets::Row> = rows
                .iter()
                .map(|r| ratatui::widgets::Row::new(r.cells.clone()))
                .collect();
            let constraints: Vec<Constraint> = widths.iter().map(|w| Constraint::Length(*w)).collect();
            let mut table = ratatui::widgets::Table::new(trows, constraints)
                .column_spacing(2);
            if !headers.is_empty() {
                table = table.header(hdr);
            }
            let table = table
                .row_highlight_style(
                    Style::default()
                        .fg(widgets::bg_base())
                        .bg(widgets::tone_color(crate::view::Tone::Info)),
                )
                .highlight_symbol("▶ ")
                .block(
                    widgets::themed_block(format!(" {title} "))
                        .title_bottom(" ↑/↓ move   enter select   q quit "),
                );
            f.render_stateful_widget(table, cols[0], &mut state);

            let block = widgets::themed_block("");
            let inner = block.inner(cols[1]);
            f.render_widget(block, cols[1]);
            art_box = Some((inner.x, inner.y, inner.width, inner.height));

            if !image::supported() {
                if let Some(l) = logo_for(sel) {
                    let pad = inner.height.saturating_sub(l.height()) / 2;
                    let mut lines = vec![Line::from(""); pad as usize];
                    lines.extend(l.render());
                    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
                }
            }
        })?;

        // Emitted after the frame so ratatui's output cannot cover it.
        match (art_box, png_for(sel)) {
            (Some((x, y, w, hh)), Some(png)) if hh.min(w / 2) > 0 => {
                let side = hh.min(w / 2);
                art.show(png, sel, x + (w - side * 2) / 2, y + (hh - side) / 2, side * 2, side, term_size);
            }
            // An unbranded network has no artwork, so the previous chain's logo
            // has to come DOWN — `forget` only resets the tracker, it does not
            // erase what is on screen, which is how anvil ended up wearing
            // Robinhood's feather.
            _ => {
                if art.is_showing() {
                    image::clear();
                    art.forget();
                }
            }
        }

        crate::ui_alive();

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => state.select(Some(step(sel, false))),
                    KeyCode::Down | KeyCode::Char('j') => state.select(Some(step(sel, true))),
                    KeyCode::Enter => break Some(sel),
                    KeyCode::Char('q') | KeyCode::Esc => break None,
                    _ => {}
                }
            }
        }
    };
    // An image left behind would sit over whatever screen comes next.
    image::clear();
    Ok(result)
}

/// Bundled documentation, embedded so the binary stays self-contained.
pub const DOCS: &[(&str, &str)] = &[
    ("Welcome", include_str!("../docs/welcome.md")),
    ("Overview", include_str!("../docs/overview.md")),
    // Setup first, then the shortcuts. The bindings only mean something once
    // you have an account and an endpoint to use them against.
    ("Accounts", include_str!("../docs/wallets.md")),
    ("Config", include_str!("../docs/config.md")),
    ("Shortcuts", include_str!("../docs/keys.md")),
    ("Chart", include_str!("../docs/chart.md")),
    ("Terms", include_str!("../docs/terms.md")),
    ("Privacy", include_str!("../docs/privacy.md")),
    ("License", include_str!("../docs/license.md")),
    ("Support", include_str!("../docs/support.md")),
    // Last, and the reason the reader is a sequence rather than a menu: someone
    // who scrolls to the end should find a door, not run out of pages.
    ("Finish", include_str!("../docs/finish.md")),
];

/// Scrollable markdown viewer for the bundled docs.
///
/// Left column picks a document, right renders it. Rendering happens per frame
/// rather than being cached: the docs are a few kilobytes, and a cache would
/// have to be invalidated on every theme change.
/// Enter API keys and endpoints, and write them to the config.
///
/// Reachable from the start screen, because "edit this JSON file" is a poor
/// answer to "how do I make it fast" when the app is already open and knows
/// which fields it wants. Everything here ends up in the same file the Config
/// page describes — this is a way in, not a second source of truth.
pub fn endpoints_screen(term: &mut Term) -> eyre::Result<()> {
    loop {
        let names = crate::config::network_names();
        if names.is_empty() {
            select(term, "No networks in your config", &["Back".to_string()])?;
            return Ok(());
        }
        let mut items = names.clone();
        items.push("Done".to_string());

        let Some(i) = select(term, "Which chain's RPC endpoints?", &items)? else {
            return Ok(());
        };
        if i >= names.len() {
            return Ok(());
        }
        let net = &names[i];

        // Solana providers usually serve websockets on a different host, so it
        // is asked for separately rather than derived from the HTTP URL.
        let solana = net.to_lowercase().starts_with("solana");
        let mut fields: Vec<(&str, &str, &str)> = vec![(
            "rpc",
            "RPC URL(s)",
            "One URL, or several separated by commas — requests rotate across all of them.",
        )];
        if solana {
            fields.push((
                "ws",
                "Websocket URL(s)",
                "Optional. One or several, comma-separated. Often a different host than the RPC.",
            ));
        }

        for (field, title, hint) in fields {
            // Esc on any prompt stops the walk rather than skipping to the next
            // question: backing out should not quietly leave half a setup.
            let Some(v) = input(term, &format!("{net} — {title}"), hint)? else {
                break;
            };
            if let Err(e) = crate::config::set_network_field(net, field, &v) {
                select(term, &format!("Could not save: {e}"), &["Back".to_string()])?;
                break;
            }
        }

        select(
            term,
            &format!("Saved to {}", crate::config::config_path().display()),
            &["Back".to_string()],
        )?;
    }
}

/// The docs, as the screen the app opens on.
///
/// Same reader, different contract: `enter` starts and `q` leaves, and the
/// footer says so. Returns whether to go on — `false` means the user quit from
/// here rather than starting.
pub fn start_screen(
    term: &mut Term,
    new_wallet: &mut dyn FnMut(&mut Term) -> eyre::Result<()>,
) -> eyre::Result<bool> {
    docs_inner(term, true, Some(new_wallet))
}

/// The docs, opened with `D` from inside a session. Esc returns to where you
/// came from.
pub fn docs(term: &mut Term) -> eyre::Result<()> {
    docs_inner(term, false, None).map(|_| ())
}

fn docs_inner(
    term: &mut Term,
    start: bool,
    mut new_wallet: Option<&mut dyn FnMut(&mut Term) -> eyre::Result<()>>,
) -> eyre::Result<bool> {
    let (mut sel, mut scroll) = (0usize, 0u16);
    // Written by the draw closure so the key handler can clamp against what was
    // actually laid out, rather than guessing.
    let mut max_scroll: u16 = 0;
    // Mouse capture is on process-wide for highlight-to-copy, which takes the
    // terminal's native selection with it — so this screen, like the
    // dashboards, does its own: drag paints, release copies, wheel scrolls.
    let mut msel = mouse::Selection::default();
    let mut copy_armed = false;
    loop {
        let mut grabbed: Option<String> = None;
        term.draw(|f| {
            widgets::paint_bg(f);
            image::clear();
            let cols = Layout::horizontal([Constraint::Length(20), Constraint::Min(30)])
                .split(f.area());

            let items: Vec<ListItem> = DOCS.iter().map(|(t, _)| ListItem::new(*t)).collect();
            let mut st = ListState::default();
            st.select(Some(sel));
            f.render_stateful_widget(
                List::new(items)
                    .block(widgets::themed_block(" Docs "))
                    .highlight_style(
                        Style::default()
                            .fg(widgets::bg_base())
                            .bg(widgets::tone_color(crate::view::Tone::Info)),
                    )
                    .highlight_symbol("▶ "),
                cols[0],
                &mut st,
            );

            let block = widgets::themed_block(format!(" {} ", DOCS[sel].0))
                // The footer names what THIS page can do. A fixed strip listing
                // every key would be a second shortcuts index nobody reads.
                .title_bottom(if !start {
                    " j/k or tab switch · ↑/↓ scroll · e set API keys · T theme · esc back ".to_string()
                } else {
                    let action = match DOCS[sel].0 {
                        "Accounts" => " · W make an account",
                        "Config" => " · e set API keys",
                        "Finish" => " · W account · e API keys",
                        _ => "",
                    };
                    format!(" j/k switch · ↑/↓ scroll · T theme{action} · enter start · q quit ")
                });
            let inner = block.inner(cols[1]);
            let body = markdown::render(DOCS[sel].1);

            // Wrapping turns one long line into several, so the scroll limit
            // has to count laid-out rows, not source lines — and only the
            // widget's own word-wrapper knows that number. Estimating it by
            // dividing character counts undercounted (a word that doesn't fit
            // moves WHOLE to the next row), which clamped the scroll short and
            // cut the tail off longer pages.
            let par = Paragraph::new(body).wrap(Wrap { trim: false });
            let wrapped = par.line_count(inner.width.max(1));
            max_scroll = (wrapped as u16).saturating_sub(inner.height);
            scroll = scroll.min(max_scroll);

            f.render_widget(par.scroll((scroll, 0)).block(block), cols[1]);
            mouse::paint(f, &msel);
            if copy_armed {
                if let Some((a, b)) = msel.region() {
                    grabbed = Some(mouse::selected_text(f.buffer_mut(), a, b));
                }
            }
        })?;
        if let Some(t) = grabbed {
            copy_armed = false;
            msel.clear();
            if !t.is_empty() {
                mouse::copy(&t);
            }
        }

        crate::ui_alive();

        if event::poll(Duration::from_millis(200))? {
            let ev = event::read()?;
            if let Event::Mouse(m) = ev {
                use crossterm::event::MouseEventKind as K;
                match m.kind {
                    K::ScrollDown => scroll = scroll.saturating_add(2).min(max_scroll),
                    K::ScrollUp => scroll = scroll.saturating_sub(2),
                    _ => {
                        if msel.on_mouse(m) {
                            copy_armed = true; // extracted on the next frame
                        }
                    }
                }
            }
            if let Event::Key(k) = ev {
                let switch = |forward: bool, sel: &mut usize, scroll: &mut u16| {
                    *sel = if forward {
                        (*sel + 1) % DOCS.len()
                    } else {
                        (*sel + DOCS.len() - 1) % DOCS.len()
                    };
                    *scroll = 0;
                };
                match k.code {
                    // On the start screen these part company: `enter` goes on
                    // to the chain picker and `q` leaves the app. Everywhere
                    // else both simply close the reader.
                    KeyCode::Enter if start => return Ok(true),
                    KeyCode::Char('q') if start => return Ok(false),
                    // The actions a page talks about, available on the page
                    // that talks about them. Reading "press W to make an
                    // account" and then having to leave to do it is the kind of
                    // gap that turns a five-minute setup into an evening.
                    KeyCode::Char('e') => {
                        endpoints_screen(term)?;
                    }
                    KeyCode::Char('W') if start => {
                        if let Some(f) = new_wallet.as_deref_mut() {
                            f(term)?;
                        }
                    }
                    // Themes work here so the reader can pick one while there is
                    // still a lot of text on screen to judge it against.
                    KeyCode::Char('T') => {
                        if let Some(name) = widgets::theme_picker(term)? {
                            widgets::set_theme(&name, true);
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
                    // hjkl and tab all move between documents; the arrows scroll
                    // the one you are reading.
                    KeyCode::Tab
                    | KeyCode::Char('j')
                    | KeyCode::Char('l')
                    | KeyCode::Right => switch(true, &mut sel, &mut scroll),
                    KeyCode::BackTab
                    | KeyCode::Char('k')
                    | KeyCode::Char('h')
                    | KeyCode::Left => switch(false, &mut sel, &mut scroll),
                    KeyCode::Down => scroll = scroll.saturating_add(1).min(max_scroll),
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll = scroll.saturating_add(10).min(max_scroll),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::Home => scroll = 0,
                    KeyCode::End => scroll = max_scroll,
                    _ => {}
                }
            }
        }
    }
}

/// Yes/no confirmation./// Yes/no confirmation. Returns true only on an explicit yes.
///
/// Guards actions that are easy to trigger by accident and impossible to undo —
/// quitting mid-position being the one that actually bit. Defaults to NO: enter
/// and escape both decline, so a stray keypress cannot confirm.
pub fn confirm(term: &mut Term, question: &str) -> eyre::Result<bool> {
    loop {
        term.draw(|f| {
            widgets::paint_bg(f);
            image::clear();
            let area = centered(f.area(), (question.len() as u16 + 10).max(40), 5);
            let body = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    question.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                widgets::themed_block(" Confirm ")
                    .title_bottom(Line::from(" y / n or esc ").centered()),
            );
            f.render_widget(body, area);
        })?;

        crate::ui_alive();

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                        return Ok(false)
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Checkbox multi-select: space toggles a tick on the highlighted row, enter
/// confirms. `max` caps how many can be ticked. Returns the ticked indices, or
/// None if cancelled.
pub fn multi_select(
    term: &mut Term,
    title: &str,
    items: &[String],
    max: usize,
) -> eyre::Result<Option<Vec<usize>>> {
    // This screen owns the terminal now: take down any image the previous one
    // left, which also marks every placement stale so it redraws on return.
    image::clear();
    let mut state = ListState::default();
    state.select(Some(0));
    let mut checked = vec![false; items.len()];
    loop {
        let ticked = checked.iter().filter(|b| **b).count();
        term.draw(|f| {
            // Same as the dashboards: without this the panel sits on the
            // terminal's own background, so a theme only half applies.
            widgets::paint_bg(f);
            let area = centered(f.area(), 80, items.len() as u16 + 4);
            let rows = items.iter().enumerate().map(|(i, s)| {
                let mark = if checked[i] { "[x] " } else { "[ ] " };
                let style = if checked[i] {
                    Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Good))
                } else {
                    Style::default()
                };
                ListItem::new(Span::styled(format!("{mark}{s}"), style))
            });
            let list = List::new(rows)
                .block(
                    crate::ui::widgets::themed_block(format!(" {title}  ({ticked}/{max}) "))
                        .title_bottom(" ↑/↓ move   space tick   enter confirm   q quit "),
                )
                .highlight_style(Style::default().fg(crate::ui::widgets::bg_base()).bg(crate::ui::widgets::tone_color(crate::view::Tone::Info)))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, area, &mut state);
        })?;

        crate::ui_alive();

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some(if i == 0 { items.len() - 1 } else { i - 1 }));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some((i + 1) % items.len()));
                    }
                    KeyCode::Char(' ') => {
                        let i = state.selected().unwrap_or(0);
                        if checked[i] {
                            checked[i] = false;
                        } else if ticked < max {
                            checked[i] = true;
                        }
                    }
                    KeyCode::Enter => {
                        return Ok(Some(
                            checked
                                .iter()
                                .enumerate()
                                .filter_map(|(i, b)| b.then_some(i))
                                .collect(),
                        ));
                    }
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

/// Free text entry (visible). Returns the typed string, or None if cancelled.
/// `hint` is shown dimmed below the field.
pub fn input(term: &mut Term, title: &str, hint: &str) -> eyre::Result<Option<String>> {
    // This screen owns the terminal now: take down any image the previous one
    // left, which also marks every placement stale so it redraws on return.
    image::clear();
    let mut buf = String::new();
    loop {
        term.draw(|f| {
            // Same as the dashboards: without this the panel sits on the
            // terminal's own background, so a theme only half applies.
            widgets::paint_bg(f);
            let area = centered(f.area(), 74, 6);
            let p = Paragraph::new(vec![
                Line::from(vec![
                    Span::raw(buf.clone()),
                    Span::styled("▏", Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Info))),
                ]),
                Line::from(Span::styled(hint, Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Dim)))),
            ])
            .block(
                crate::ui::widgets::themed_block(format!(" {title} ")).title_bottom(" enter submit   esc cancel "),
            );
            f.render_widget(Clear, area);
            f.render_widget(p, area);
        })?;

        crate::ui_alive();

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Enter => return Ok(Some(buf.trim().to_string())),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
            }
        }
    }
}

/// Masked password entry. Returns the typed string, or None if cancelled.
/// A password that wipes itself when it goes out of scope.
///
/// A `String` freed normally leaves its bytes in the heap until something else
/// happens to reuse them — readable from a core dump, a swap file, or a
/// debugger attached to the process. The window is small and the risk is not
/// theoretical: this is the one secret the user types by hand, and the file it
/// unlocks is designed to be safe to copy precisely BECAUSE the password is not
/// stored anywhere. Leaving it in freed memory undoes that.
///
/// Deref means callers use it exactly like a `&str`.
pub struct Secret(String);

impl std::ops::Deref for Secret {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

pub fn password(term: &mut Term, title: &str) -> eyre::Result<Option<String>> {
    // This screen owns the terminal now: take down any image the previous one
    // left, which also marks every placement stale so it redraws on return.
    image::clear();
    let mut buf = String::new();
    loop {
        term.draw(|f| {
            // Same as the dashboards: without this the panel sits on the
            // terminal's own background, so a theme only half applies.
            widgets::paint_bg(f);
            let area = centered(f.area(), 60, 5);
            let masked: String = "•".repeat(buf.chars().count());
            let p = Paragraph::new(Line::from(vec![
                Span::raw(masked),
                Span::styled("▏", Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Info))),
            ]))
            .block(
                crate::ui::widgets::themed_block(format!(" {title} ")).title_bottom(" enter submit   esc cancel "),
            );
            f.render_widget(Clear, area);
            f.render_widget(p, area);
        })?;

        crate::ui_alive();

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Enter => return Ok(Some(buf)),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
            }
        }
    }
}

/// A centered rect `w` cols wide and `h` rows tall within `area`.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}
