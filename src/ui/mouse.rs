// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Highlight-to-copy, the way a terminal native would do it.
//!
//! With mouse capture on, the terminal stops doing its own selection — the
//! app sees the drag instead. So the app does what the terminal would have:
//! track the drag, paint the selected cells inverted, and on release put the
//! text on the clipboard. Copying goes out as OSC 52 (the terminal writes the
//! clipboard, which survives ssh) and, on macOS, through `pbcopy` as well —
//! belt and braces, since a few terminals ship with OSC 52 writes disabled.
//!
//! Selection is LINEAR, like a terminal's: first row from the anchor column,
//! full rows in between, last row up to the cursor. Trailing whitespace is
//! trimmed per row, because a TUI row is padded to the screen edge and nobody
//! ever meant to copy forty spaces.

use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::Modifier;
use ratatui::Frame;

/// A drag in progress (or just finished). Lives in each dashboard's loop.
#[derive(Default)]
pub struct Selection {
    anchor: Option<(u16, u16)>,
    cursor: (u16, u16),
    /// Painted while dragging; cleared once copied.
    pub live: bool,
}

impl Selection {
    /// Feed a mouse event. Returns true when a drag FINISHED and there is a
    /// region worth copying.
    pub fn on_mouse(&mut self, me: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::MouseEventKind as K;
        match me.kind {
            K::Down(crossterm::event::MouseButton::Left) => {
                self.anchor = Some((me.column, me.row));
                self.cursor = (me.column, me.row);
                self.live = true;
                false
            }
            K::Drag(crossterm::event::MouseButton::Left) => {
                if self.anchor.is_some() {
                    self.cursor = (me.column, me.row);
                }
                false
            }
            K::Up(crossterm::event::MouseButton::Left) => {
                let had = self.anchor.is_some();
                self.cursor = (me.column, me.row);
                self.live = false;
                had
            }
            _ => false,
        }
    }

    /// The selection's endpoints in reading order, or None before any drag.
    pub fn region(&self) -> Option<((u16, u16), (u16, u16))> {
        let a = self.anchor?;
        let b = self.cursor;
        // Reading order: earlier row first; same row, earlier column first.
        if (b.1, b.0) < (a.1, a.0) {
            Some((b, a))
        } else {
            Some((a, b))
        }
    }

    /// Forget the selection (after the copy, or on a pool switch).
    pub fn clear(&mut self) {
        self.anchor = None;
        self.live = false;
    }
}

/// The text under a linear selection, read straight off the rendered buffer.
pub fn selected_text(buf: &Buffer, from: (u16, u16), to: (u16, u16)) -> String {
    let mut lines = Vec::new();
    for y in from.1..=to.1.min(buf.area.height.saturating_sub(1)) {
        let x0 = if y == from.1 { from.0 } else { 0 };
        let x1 = if y == to.1 { to.0 } else { buf.area.width.saturating_sub(1) };
        let mut line = String::new();
        let mut x = x0;
        while x <= x1.min(buf.area.width.saturating_sub(1)) {
            let cell = &buf[Position { x, y }];
            let sym = cell.symbol();
            line.push_str(if sym.is_empty() { " " } else { sym });
            // A wide glyph occupies extra cells that render as "" — skip them.
            x += (unicode_width(sym).max(1)) as u16;
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n").trim_end().to_string()
}

fn unicode_width(s: &str) -> usize {
    // Good enough for cell math: emoji and CJK are 2, everything else 1.
    // (ratatui pads the following cell with an empty symbol either way.)
    match s.chars().next() {
        Some(c) if (c as u32) >= 0x1100 && !c.is_ascii() && s.chars().count() == 1 => 2,
        _ => 1,
    }
}

/// Paint the live selection as reversed video, over whatever was drawn.
pub fn paint(f: &mut Frame, sel: &Selection) {
    if !sel.live {
        return;
    }
    let Some((from, to)) = sel.region() else { return };
    let buf = f.buffer_mut();
    for y in from.1..=to.1.min(buf.area.height.saturating_sub(1)) {
        let x0 = if y == from.1 { from.0 } else { 0 };
        let x1 = if y == to.1 { to.0 } else { buf.area.width.saturating_sub(1) };
        for x in x0..=x1.min(buf.area.width.saturating_sub(1)) {
            buf[Position { x, y }].modifier.toggle(Modifier::REVERSED);
        }
    }
}

/// Put `text` on the clipboard: OSC 52 through the terminal (works over ssh),
/// plus `pbcopy` on macOS for terminals that ignore OSC 52 writes.
pub fn copy(text: &str) {
    use std::io::Write;
    if text.is_empty() {
        return;
    }
    let b64 = base64(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();

    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

/// Tiny base64 — the same no-dependency choice the Solana wire format made.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backwards_drag_reads_forwards() {
        let mut s = Selection::default();
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind, x, y| MouseEvent { kind, column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE };
        s.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 10, 5));
        s.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 2, 3));
        assert!(s.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), 2, 3)));
        assert_eq!(s.region(), Some(((2, 3), (10, 5))));
    }

    #[test]
    fn selected_text_trims_the_padding_a_row_never_meant() {
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 2));
        buf.set_string(0, 0, "0xabc     ", ratatui::style::Style::default());
        buf.set_string(0, 1, "hi        ", ratatui::style::Style::default());
        let t = selected_text(&buf, (0, 0), (9, 1));
        assert_eq!(t, "0xabc\nhi");
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
