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

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// One physical keypress is one event, whoever reads it.
///
/// The user's terminal delivers a single press as two events a few
/// milliseconds apart. Each screen reads keys in its own loop, so the modal
/// that consumed the first copy is often gone when the second arrives — and
/// the letters of a password replayed into the dashboard as commands, walking
/// the panel to Logs after every wallet unlock. One shared gate, consulted by
/// every reader, drops the ghost no matter which loop it lands in. No human
/// repeats a key inside 25ms; nothing real is lost.
pub fn fresh_key(code: KeyCode) -> bool {
    use std::sync::Mutex;
    static LAST: Mutex<Option<(KeyCode, std::time::Instant)>> = Mutex::new(None);
    let mut g = crate::lock(&LAST);
    if let Some((c, at)) = *g {
        if c == code && at.elapsed().as_millis() < 25 {
            crate::trace(&format!("input: dropped duplicate {code:?}"));
            return false;
        }
    }
    *g = Some((code, std::time::Instant::now()));
    true
}

/// Arrow-key list selection. Returns the chosen index, or None if cancelled.
pub fn select(term: &mut Term, title: &str, items: &[String]) -> eyre::Result<Option<usize>> {
    // This screen owns the terminal now: take down any image the previous one
    // left, which also marks every placement stale so it redraws on return.
    image::clear();
    let mut state = ListState::default();
    state.select(Some(0));
    // Highlight-to-copy, the same as the dashboard and the calendar.
    //
    // Mouse capture is on for the whole app, so the terminal's own selection is
    // off — a screen that ignores mouse events cannot be copied from at all.
    // This one lists contract addresses, which exist to be copied.
    let mut msel = crate::ui::mouse::Selection::default();
    let mut copy_armed = false;
    let mut copied: Option<usize> = None;
    loop {
        let mut grabbed: Option<String> = None;
        term.draw(|f| {
            widgets::paint_bg(f);
            let area = centered(f.area(), 80, items.len() as u16 + 4);
            let foot = match copied {
                Some(n) => format!(" copied {n} characters "),
                None => " ↑/↓ move   enter select   drag copy   q quit ".to_string(),
            };
            let list = List::new(items.iter().map(|s| ListItem::new(s.as_str())))
                .block(widgets::themed_block(format!(" {title} ")).title_bottom(foot))
                .highlight_style(Style::default().fg(widgets::bg_base()).bg(widgets::tone_color(crate::view::Tone::Info)))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, area, &mut state);
            crate::ui::mouse::paint(f, &msel);
            if copy_armed {
                if let Some((a, b)) = msel.region() {
                    grabbed = Some(crate::ui::mouse::selected_text(f.buffer_mut(), a, b));
                }
            }
        })?;
        if let Some(t) = grabbed {
            copy_armed = false;
            msel.clear();
            if !t.is_empty() {
                crate::ui::mouse::copy(&t);
                copied = Some(t.chars().count());
            }
        }
        crate::ui_alive();
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Mouse(m) => {
                    use crossterm::event::MouseEventKind as MK;
                    match m.kind {
                        MK::ScrollUp => {
                            let i = state.selected().unwrap_or(0);
                            state.select(Some(i.saturating_sub(3)));
                        }
                        MK::ScrollDown => {
                            let i = state.selected().unwrap_or(0);
                            state.select(Some((i + 3).min(items.len().saturating_sub(1))));
                        }
                        _ => {
                            if msel.on_mouse(m) {
                                copy_armed = true; // extracted on the next frame
                            }
                        }
                    }
                    continue;
                }
                Event::Key(k) => {
                    if !fresh_key(k.code) { continue; }
                    if widgets::theme_key(term, k.code)? { continue; }
                    copied = None; // the note belongs to the moment
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
                _ => {}
            }
        }
    }
}

/// A short, centred picker that sits over the screen rather than replacing it.
///
/// `select` sizes its box to hold every item, which is right for a handful of
/// accounts and wrong for a hundred and sixty currencies: the box grows past
/// the screen, the borders leave, and a menu becomes a wall of text. This one
/// is a fixed panel that scrolls, so the list length stops being the layout.
///
/// `jump` lets a letter seek — pressing `e` walks to EUR, then to EGP. With a
/// list this long, arrow keys are not navigation, they are a chore.
pub fn select_overlay(
    term: &mut Term,
    title: &str,
    items: &[String],
    start: usize,
) -> eyre::Result<Option<usize>> {
    image::clear();
    let mut state = ListState::default();
    state.select(Some(start.min(items.len().saturating_sub(1))));
    // How far a page moves. Taken from the panel as drawn rather than fixed,
    // because a "page" that is not what you can see is not a page.
    let mut page = 10usize;
    loop {
        term.draw(|f| {
            widgets::paint_bg(f);
            // Tall enough to be worth scrolling, short enough to read as a
            // panel rather than a page.
            let h = (f.area().height * 3 / 5).clamp(7, 22);
            page = (h as usize).saturating_sub(2).max(1); // less the two borders
            let area = centered(f.area(), 34, h);
            f.render_widget(ratatui::widgets::Clear, area);
            let sel = state.selected().unwrap_or(0) + 1;
            let list = List::new(items.iter().map(|s| ListItem::new(s.as_str())))
                .block(
                    widgets::themed_block(format!(" {title} "))
                        .title_bottom(format!(
                            " {sel}/{}  jk ↑↓ · ^d ^u · g G · enter · esc ",
                            items.len()
                        )),
                )
                .highlight_style(
                    Style::default()
                        .fg(widgets::bg_base())
                        .bg(widgets::tone_color(crate::view::Tone::Info)),
                )
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, area, &mut state);
        })?;
        crate::ui_alive();
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let ev = event::read()?;
        let i = state.selected().unwrap_or(0);
        // The wheel, for the hand that is already on the mouse. `select` has
        // had this all along; this list is the longer of the two and had only
        // the keyboard.
        if let Event::Mouse(m) = ev {
            use crossterm::event::MouseEventKind as K;
            match m.kind {
                K::ScrollUp => state.select(Some(i.saturating_sub(3))),
                K::ScrollDown => state.select(Some((i + 3).min(items.len() - 1))),
                _ => {}
            }
            continue;
        }
        let Event::Key(k) = ev else { continue };
        if !fresh_key(k.code) { continue; }
        if widgets::theme_key(term, k.code)? { continue; }
        match k.code {
            // hjkl moves, as everywhere else here.
            //
            // Letters used to SEEK: `j` walked JPY → JMD → JOD and `h` walked
            // HKD → HUF, so a hand in the vim position went sideways through
            // four currencies while the list never scrolled. Seeking is gone
            // rather than merely moved off those four keys — one key meaning
            // "move" in one list and "find" in another is the thing that made
            // this confusing, and a wrong guess about which is worse in a
            // screen you are about to press enter on.
            KeyCode::Up | KeyCode::Char('k') => {
                state.select(Some(if i == 0 { items.len() - 1 } else { i - 1 }))
            }
            KeyCode::Down | KeyCode::Char('j') => state.select(Some((i + 1) % items.len())),
            // Horizontal keys in a vertical list: inert, not surprising.
            KeyCode::Char('h') | KeyCode::Char('l') => {}
            // The vim page keys, on the panel's real height: ctrl-d/u move a
            // half screen, ctrl-f/b a whole one. A list this long is walked in
            // pages, and reaching for the arrow key 80 times is not walking.
            KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                state.select(Some((i + page / 2).min(items.len() - 1)))
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                state.select(Some(i.saturating_sub(page / 2)))
            }
            KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                state.select(Some((i + page).min(items.len() - 1)))
            }
            KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                state.select(Some(i.saturating_sub(page)))
            }
            KeyCode::PageDown => state.select(Some((i + page).min(items.len() - 1))),
            KeyCode::PageUp => state.select(Some(i.saturating_sub(page))),
            // gg is two keystrokes for what one can say here. G is vim's, and g
            // is its opposite — no other letter does anything, so neither can
            // be mistaken for a jump to a row beginning with it.
            KeyCode::Char('g') => state.select(Some(0)),
            KeyCode::Char('G') => state.select(Some(items.len() - 1)),
            KeyCode::Home => state.select(Some(0)),
            KeyCode::End => state.select(Some(items.len() - 1)),
            KeyCode::Enter => return Ok(state.selected()),
            KeyCode::Esc => return Ok(None),
            _ => {}
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
                if !fresh_key(k.code) { continue; }
                if widgets::theme_key(term, k.code)? { continue; }
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
///
/// Shortcuts is the one page that is not a file. It is rendered from shortcuts.json,
/// which is also what the in-app help and the website read, so the page cannot
/// describe a binding this build does not have. Embedding `docs/shortcuts.md` here
/// meant the docs screen was only as fresh as the last time someone remembered
/// to regenerate it — a test caught that, but catching it is worse than not
/// being able to get it wrong.
///
/// `docs/shortcuts.md` still exists, for people reading the repository rather than
/// running the app, and is still checked against the source. Nothing the app
/// shows depends on it now.
pub fn doc_pages() -> &'static [(&'static str, &'static str)] {
    static DOCS: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    DOCS.get_or_init(|| {
        // Leaked on purpose: one allocation that lives as long as the process,
        // which is exactly what the `include_str!` entries around it are. The
        // alternative is threading a lifetime through six call sites to avoid
        // a few hundred bytes.
        let page: &'static str = Box::leak(crate::shortcuts::markdown().into_boxed_str());
        vec![
            ("Welcome", include_str!("../docs/welcome.md")),
            ("Overview", include_str!("../docs/overview.md")),
            // Early, and before the how-to pages. What a tool refuses to do is
            // worth knowing before learning to drive it.
            ("Manifesto", include_str!("../docs/manifesto.md")),
            // Setup first, then the shortcuts. The bindings only mean something
            // once you have an account and an endpoint to use them against.
            ("Accounts", include_str!("../docs/wallets.md")),
            ("Config", include_str!("../docs/config.md")),
            ("Shortcuts", page),
            ("Chart", include_str!("../docs/chart.md")),
            ("Terms", include_str!("../docs/terms.md")),
            ("Privacy", include_str!("../docs/privacy.md")),
            ("License", include_str!("../docs/license.md")),
            ("Support", include_str!("../docs/support.md")),
            // Last, and the reason the reader is a sequence rather than a menu:
            // someone who scrolls to the end should find a door, not run out of
            // pages.
            ("Finish", include_str!("../docs/finish.md")),
        ]
    })
}

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
    new_wallet: &mut MakeWallet<'_>,
) -> eyre::Result<bool> {
    docs_inner(term, true, Some(new_wallet))
}

/// The docs, opened with `D` from inside a session. Esc returns to where you
/// came from.
/// A callback the docs screen can hand the terminal to so the reader can make
/// an account without leaving the page. Named because the bare shape —
/// `dyn FnMut(&mut Term) -> Result<()>` — says nothing about what it does.
pub type MakeWallet<'a> = dyn FnMut(&mut Term) -> eyre::Result<()> + 'a;

pub fn docs(term: &mut Term) -> eyre::Result<()> {
    docs_inner(term, false, None).map(|_| ())
}

fn docs_inner(
    term: &mut Term,
    start: bool,
    mut new_wallet: Option<&mut MakeWallet<'_>>,
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

            let items: Vec<ListItem> = doc_pages().iter().map(|(t, _)| ListItem::new(*t)).collect();
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

            let block = widgets::themed_block(format!(" {} ", doc_pages()[sel].0))
                // The footer names what THIS page can do. A fixed strip listing
                // every key would be a second shortcuts index nobody reads.
                .title_bottom(if !start {
                    " j/k or tab switch · ↑/↓ scroll · e set API keys · T theme · esc back ".to_string()
                } else {
                    let action = match doc_pages()[sel].0 {
                        "Accounts" => " · W make an account",
                        "Config" => " · e set API keys",
                        "Finish" => " · W account · e API keys",
                        _ => "",
                    };
                    format!(" j/k switch · ↑/↓ scroll · T theme{action} · enter start · q quit ")
                });
            let inner = block.inner(cols[1]);
            let body = markdown::render(doc_pages()[sel].1);

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
                if !fresh_key(k.code) { continue; }
                let switch = |forward: bool, sel: &mut usize, scroll: &mut u16| {
                    *sel = if forward {
                        (*sel + 1) % doc_pages().len()
                    } else {
                        (*sel + doc_pages().len() - 1) % doc_pages().len()
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

/// The display-currency picker, for whichever dashboard asked.
///
/// Both chains show figures in the chosen currency, so both need the key that
/// changes it. Lifting it here rather than copying it is the same reasoning as
/// the shortcuts file: two copies of a screen drift, and this one has an
/// opinion about what to say when Coinbase is unreachable.
///
/// Returns the line to show, or `None` if the picker was dismissed.
pub async fn currency_picker(term: &mut Term) -> eyre::Result<Option<String>> {
    if crate::base_currency::available().len() < 2 {
        crate::base_currency::refresh().await;
    }
    let codes = crate::base_currency::available();
    if codes.len() < 2 {
        return Ok(Some(
            "Could not reach Coinbase for exchange rates, so the currency list is empty. Figures stay in USD."
                .to_string(),
        ));
    }
    let here = crate::base_currency::code();
    let labels: Vec<String> = codes
        .iter()
        .map(|c| {
            let row = crate::base_currency::label(c);
            if *c == here { format!("{row}   ✓") } else { row }
        })
        .collect();
    // Opens ON the current currency, not at the top: the list is 160 long and
    // the row you care about most is the one you are already using.
    let at = codes.iter().position(|c| *c == here).unwrap_or(0);
    let Some(i) = select_overlay(term, "Display currency", &labels, at)? else {
        return Ok(None);
    };
    let pick = codes[i].clone();
    Ok(Some(if crate::base_currency::select(&pick) {
        crate::base_currency::save(&pick);
        format!("Reading in {pick}. Prices are still recorded in USD — only the display changed.")
    } else {
        format!("No rate for {pick} yet, so the display stays in {here}.")
    }))
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
                if !fresh_key(k.code) { continue; }
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
                if !fresh_key(k.code) { continue; }
                if widgets::theme_key(term, k.code)? { continue; }
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
            match event::read()? {
                // A paste arrives as ONE event carrying the whole string, so it
                // never passes through `fresh_key`. That matters: the ghost-key
                // filter cannot tell a duplicate keypress from the second '0' of
                // a pasted "00", and a 64-character private key almost always
                // contains a repeated character. Without this, pasting a key
                // silently lost bytes and the import failed as "not a valid
                // private key".
                Event::Paste(s) => buf.push_str(s.trim()),
                Event::Key(k) => {
                    // A physical press reported twice is the whole reason
                    // `fresh_key` exists. Where the terminal tells us the event
                    // KIND, the duplicate is identifiable outright and there is
                    // no need to guess from timing.
                    if k.kind == KeyEventKind::Release { continue; }
                    // Text characters must NOT go through the ghost filter: it
                    // drops a repeat inside 25ms, which is indistinguishable
                    // from the second '0' of a key or the 'll' of a seed word.
                    // The filter still guards the control keys, where a stray
                    // repeat would submit or cancel the screen twice.
                    if !matches!(k.code, KeyCode::Char(_)) && !fresh_key(k.code) { continue; }
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
                _ => {}
            }
        }
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
            match event::read()? {
                // A paste arrives as ONE event carrying the whole string, so it
                // never passes through `fresh_key`. That matters: the ghost-key
                // filter cannot tell a duplicate keypress from the second '0' of
                // a pasted "00", and a 64-character private key almost always
                // contains a repeated character. Without this, pasting a key
                // silently lost bytes and the import failed as "not a valid
                // private key".
                Event::Paste(s) => buf.push_str(s.trim()),
                Event::Key(k) => {
                    // A physical press reported twice is the whole reason
                    // `fresh_key` exists. Where the terminal tells us the event
                    // KIND, the duplicate is identifiable outright and there is
                    // no need to guess from timing.
                    if k.kind == KeyEventKind::Release { continue; }
                    // Text characters must NOT go through the ghost filter: it
                    // drops a repeat inside 25ms, which is indistinguishable
                    // from the second '0' of a key or the 'll' of a seed word.
                    // The filter still guards the control keys, where a stray
                    // repeat would submit or cancel the screen twice.
                    if !matches!(k.code, KeyCode::Char(_)) && !fresh_key(k.code) { continue; }
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
                _ => {}
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


#[cfg(feature = "agent")]
/// A modal wait on the copilot: spinner, the question, Esc to cancel. The
/// render loop is never blocked by a language model — this loop polls the
/// answer channel and the keyboard at 50ms.
pub fn wait_for_answer<T: Send + 'static>(
    term: &mut Term,
    question: &str,
    rx: &std::sync::mpsc::Receiver<T>,
) -> eyre::Result<Option<T>> {
    use crossterm::event::{self, Event, KeyCode};
    let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let mut i = 0usize;
    loop {
        if let Ok(ans) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
            return Ok(Some(ans));
        }
        i = (i + 1) % spin.len();
        let q = question.to_string();
        term.draw(|f| {
            let area = f.area();
            let block = widgets::themed_block(" Copilot ");
            let inner = ratatui::layout::Rect {
                x: area.width / 8,
                y: area.height / 3,
                width: area.width * 3 / 4,
                height: 5,
            };
            let text = ratatui::widgets::Paragraph::new(vec![
                ratatui::text::Line::from(format!("{} thinking about: {q}", spin[i])),
                ratatui::text::Line::from(""),
                ratatui::text::Line::from("esc cancels — the market keeps running behind this"),
            ])
            .wrap(ratatui::widgets::Wrap { trim: true })
            .block(block);
            f.render_widget(ratatui::widgets::Clear, inner);
            f.render_widget(text, inner);
        })?;
        if event::poll(std::time::Duration::from_millis(30))? {
            if let Event::Key(k) = event::read()? {
                if !fresh_key(k.code) { continue; }
                if k.code == KeyCode::Esc {
                    return Ok(None);
                }
            }
        }
    }
}

#[cfg(feature = "agent")]
/// A full-screen scrollable text view — the copilot's answer, readable at
/// length. ↑/↓ and PgUp/PgDn scroll, anything else closes.
pub fn text_view(term: &mut Term, title: &str, text: &str) -> eyre::Result<()> {
    use crossterm::event::{self, Event, KeyCode};
    let mut scroll: u16 = 0;
    loop {
        term.draw(|f| {
            let area = f.area();
            let para = ratatui::widgets::Paragraph::new(text.to_string())
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((scroll, 0))
                .block(widgets::themed_block(title));
            f.render_widget(ratatui::widgets::Clear, area);
            f.render_widget(para, area);
        })?;
        if event::poll(std::time::Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                if !fresh_key(k.code) { continue; }
                if widgets::theme_key(term, k.code)? { continue; }
                match k.code {
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::Down => scroll = scroll.saturating_add(1),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown => scroll = scroll.saturating_add(10),
                    KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                    KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    _ => return Ok(()),
                }
            }
        }
    }
}


#[cfg(feature = "agent")]
/// The copilot, full screen: the conversation above, a live input below with
/// a blinking cursor, answers streaming in as claude writes them. Enter asks;
/// Esc cancels a stream in flight, and closes the screen when idle. The
/// market keeps trading behind this — the poller never stops.
pub fn chat_screen(
    term: &mut Term,
    chat: &mut crate::agent::Chat,
    context: &dyn Fn() -> String,
) -> eyre::Result<()> {
    use crate::agent::StreamEvent;
    use crossterm::event::{self, Event, KeyCode};

    // This screen owns the terminal now: take down any image the dashboard
    // left (the coin logo is a graphics PLACEMENT, not cells — it floats over
    // whatever is drawn under it until explicitly cleared). Clearing also
    // marks placements stale, so the dashboard redraws its logo on return.
    image::clear();

    fn wrap_into<'a>(
        lines: &mut Vec<ratatui::text::Line<'a>>,
        head: &str,
        text: &str,
        width: usize,
        style: ratatui::style::Style,
    ) {
        let mut first = true;
        for para in text.split('\n') {
            let mut line = String::new();
            for word in para.split_whitespace() {
                let lead = if first { head.len() } else { 2 };
                if !line.is_empty() && lead + line.len() + 1 + word.len() > width {
                    let prefix = if first { head.to_string() } else { "  ".to_string() };
                    lines.push(ratatui::text::Line::styled(format!("{prefix}{line}"), style));
                    first = false;
                    line.clear();
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            let prefix = if first { head.to_string() } else { "  ".to_string() };
            lines.push(ratatui::text::Line::styled(format!("{prefix}{line}"), style));
            first = false;
        }
    }

    let mut input = String::new();
    let mut streaming: Option<std::sync::mpsc::Receiver<StreamEvent>> = None;
    let mut partial = String::new();
    let mut from_bottom: u16 = 0; // 0 = pinned to the newest line
    let t0 = std::time::Instant::now();

    loop {
        // Drain whatever claude has written since the last frame.
        let mut finished = false;
        if let Some(rx) = &streaming {
            loop {
                match rx.try_recv() {
                    Ok(StreamEvent::Delta(d)) => {
                        partial.push_str(&d);
                        from_bottom = 0;
                    }
                    Ok(StreamEvent::Done { session_id }) => {
                        if session_id.is_some() {
                            chat.session_id = session_id;
                        }
                        if !partial.is_empty() {
                            chat.transcript.push((false, std::mem::take(&mut partial)));
                        }
                        finished = true;
                        break;
                    }
                    Ok(StreamEvent::Fail(e)) => {
                        chat.transcript.push((false, format!("({e})")));
                        partial.clear();
                        finished = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if !partial.is_empty() {
                            chat.transcript.push((false, std::mem::take(&mut partial)));
                        }
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            streaming = None;
        }

        let busy = streaming.is_some();
        let blink = (t0.elapsed().as_millis() / 500).is_multiple_of(2);
        term.draw(|f| {
            let area = f.area();
            let chunks = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    ratatui::layout::Constraint::Min(3),
                    ratatui::layout::Constraint::Length(3),
                ])
                .split(area);

            let width = chunks[0].width.saturating_sub(2).max(10) as usize;
            let bold = ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD);
            let plain = ratatui::style::Style::default();
            let mut lines: Vec<ratatui::text::Line> = Vec::new();
            for (who, text) in chat.transcript.iter() {
                wrap_into(&mut lines, if *who { "you ▸ " } else { "claude ▸ " }, text, width, if *who { bold } else { plain });
                lines.push(ratatui::text::Line::from(""));
            }
            if busy {
                let live = format!("{partial}{}", if blink { "▌" } else { " " });
                wrap_into(&mut lines, "claude ▸ ", &live, width, plain);
            }
            let total = lines.len() as u16;
            let view_h = chunks[0].height.saturating_sub(2);
            let max_scroll = total.saturating_sub(view_h);
            let s = max_scroll.saturating_sub(from_bottom.min(max_scroll));
            let para = ratatui::widgets::Paragraph::new(lines)
                .scroll((s, 0))
                .block(widgets::themed_block(" Copilot — the market keeps running behind this "));
            f.render_widget(ratatui::widgets::Clear, area);
            f.render_widget(para, chunks[0]);

            let cursor = if busy { "" } else if blink { "▌" } else { " " };
            let hint = if busy { "esc stops the answer" } else { "enter asks · esc closes · ↑/↓ scroll" };
            let input_line = ratatui::text::Line::from(vec![
                ratatui::text::Span::styled("❯ ", bold),
                ratatui::text::Span::raw(format!("{input}{cursor}")),
                ratatui::text::Span::styled(
                    format!("   {hint}"),
                    ratatui::style::Style::default().fg(widgets::tone_color(crate::view::Tone::Dim)),
                ),
            ]);
            let inp = ratatui::widgets::Paragraph::new(input_line).block(widgets::themed_block(" Ask "));
            f.render_widget(inp, chunks[1]);
        })?;

        if event::poll(std::time::Duration::from_millis(33))? {
            if let Event::Key(k) = event::read()? {
                if !fresh_key(k.code) { continue; }
                if k.kind == crossterm::event::KeyEventKind::Release {
                    continue;
                }
                match k.code {
                    KeyCode::Esc => {
                        if busy {
                            streaming = None;
                            if !partial.is_empty() {
                                let cut = format!("{} (stopped)", std::mem::take(&mut partial));
                                chat.transcript.push((false, cut));
                            }
                        } else {
                            return Ok(());
                        }
                    }
                    KeyCode::Enter => {
                        if !busy && !input.trim().is_empty() {
                            let q = std::mem::take(&mut input);
                            chat.transcript.push((true, q.clone()));
                            partial.clear();
                            from_bottom = 0;
                            streaming = Some(crate::agent::spawn_stream(
                                chat.session_id.clone(),
                                context(),
                                q,
                            ));
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Up => from_bottom = from_bottom.saturating_add(1),
                    KeyCode::Down => from_bottom = from_bottom.saturating_sub(1),
                    KeyCode::PageUp => from_bottom = from_bottom.saturating_add(10),
                    KeyCode::PageDown => from_bottom = from_bottom.saturating_sub(10),
                    KeyCode::Char(c) => input.push(c),
                    _ => {}
                }
            }
        }
    }
}
