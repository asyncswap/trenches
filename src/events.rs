// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! The event log: what happened, when, and with what.
//!
//! This is the record a person reads — in the app with `l`, or in
//! `~/.trenches/session-<ts>.log` afterwards. It is not the trace file. The
//! distinction is the whole point of this module:
//!
//! - **Trace** (`crate::trace`) records how the machinery behaved: every poll,
//!   every market read, every RPC round trip. It answers "why is nothing on
//!   screen" and it is far too dense to read.
//! - **Events** record what *happened*: an order, a fill, a wallet unlocked, a
//!   pool selected, an RPC that refused. Every line here is one thing a person
//!   would care about, and nothing repeats itself ten times a second.
//!
//! Putting the poll stream on the log screen buried the handful of lines that
//! carried information under thousands that did not, which is worse than having
//! no log at all: it looks like a record while hiding the record.
//!
//! Every event carries a timestamp, a level, what happened, and the details
//! that make it actionable — the account, the pool, the token, the amount, the
//! transaction. A line you cannot act on is a line not worth writing.

use std::sync::{Mutex, OnceLock};

/// How much attention a line deserves. Drives its colour on screen and lets the
/// reader skim for trouble.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Something the user did: chose a chain, unlocked an account, picked a pool.
    Action,
    /// Money moved, or was asked to: an order sent, a fill, realised PnL.
    Trade,
    /// Worked, but not as intended: a rate limit, a retry, a stale read.
    Warn,
    /// Did not work: a revert, a refused RPC, a failed send.
    Error,
    /// Worth recording, nothing to do about it.
    Info,
}

impl Level {
    /// The tag written at the head of the line. Fixed width, so the details of
    /// consecutive lines sit in a column and can be read down.
    pub fn tag(self) -> &'static str {
        match self {
            Level::Action => "ACTION",
            Level::Trade => "TRADE ",
            Level::Warn => "WARN  ",
            Level::Error => "ERROR ",
            Level::Info => "INFO  ",
        }
    }

    /// Recover the level from a rendered line, for colouring on screen.
    pub fn of(line: &str) -> Option<Level> {
        let body = line.get(11..)?; // past "[HH:MM:SS] "
        for l in [Level::Action, Level::Trade, Level::Warn, Level::Error, Level::Info] {
            if body.starts_with(l.tag().trim_end()) {
                return Some(l);
            }
        }
        None
    }
}

/// The in-memory event ring, for the log screen.
fn ring() -> &'static Mutex<std::collections::VecDeque<String>> {
    static RING: OnceLock<Mutex<std::collections::VecDeque<String>>> = OnceLock::new();
    RING.get_or_init(Default::default)
}

/// The last 500 events, oldest first.
pub fn recent() -> Vec<String> {
    ring().lock().map(|r| r.iter().cloned().collect()).unwrap_or_default()
}

/// Record an event.
///
/// `what` is the thing that happened, in words. `details` are the facts that
/// make it useful later — an address, a pool, an amount, a hash. They are
/// rendered `key=value`, so a log line can be grepped as well as read.
pub fn log(level: Level, what: &str, details: &[(&str, String)]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = render(now, level, what, details);

    if let Ok(mut r) = ring().lock() {
        r.push_back(line.clone());
        while r.len() > 500 {
            r.pop_front();
        }
    }
    // The same line to the session file, so the record outlives the process.
    // This is what someone attaches to a bug report.
    write_session(&line);
}

/// Build the line. Separate from `log` and free of globals, so the format is
/// testable on its own — the ring is shared process-wide, and tests that read
/// it back race each other for what "the last line" means.
fn render(now: u64, level: Level, what: &str, details: &[(&str, String)]) -> String {
    let (h, m, s) = ((now % 86400) / 3600, (now % 3600) / 60, now % 60);
    let mut line = format!("[{h:02}:{m:02}:{s:02}] {} {what}", level.tag());
    for (k, v) in details {
        // Empty details are dropped rather than written as `key=` — a blank
        // value tells the reader nothing and costs a column.
        if !v.is_empty() {
            line.push_str(&format!("  {k}={v}"));
        }
    }
    line
}

fn write_session(line: &str) {
    use std::io::Write;
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let f = FILE.get_or_init(|| {
        std::fs::create_dir_all(crate::state_dir()).ok()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/session-{ts}.log", crate::state_dir()))
            .ok()
            .map(Mutex::new)
    });
    if let Some(f) = f {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

// ---- the shorthands used at call sites ------------------------------------
//
// Named for what they record rather than for their level, so a call site reads
// as the thing that happened.

/// Something the user did.
pub fn action(what: &str, details: &[(&str, String)]) {
    log(Level::Action, what, details);
}

/// An order, a fill, or money accounted for.
pub fn trade(what: &str, details: &[(&str, String)]) {
    log(Level::Trade, what, details);
}

/// Degraded, but still working.
pub fn warn(what: &str, details: &[(&str, String)]) {
    log(Level::Warn, what, details);
}

/// Failed.
pub fn error(what: &str, details: &[(&str, String)]) {
    log(Level::Error, what, details);
}

/// Worth knowing, nothing to do.
pub fn info(what: &str, details: &[(&str, String)]) {
    log(Level::Info, what, details);
}

/// Shorten an address for a log line: `0x1234…cdef`.
///
/// The full value goes in the file for the ones that matter (transactions), but
/// a 42-character address repeated down a column is unreadable, and the ends are
/// what anyone actually compares.
pub fn short(addr: &str) -> String {
    if addr.len() > 12 {
        format!("{}…{}", &addr[..6], &addr[addr.len() - 4..])
    } else {
        addr.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_carries_its_time_level_and_details() {
        let l = render(45_296, Level::Action, "Account unlocked", &[("account", "0xabc".into())]);
        assert_eq!(l, "[12:34:56] ACTION Account unlocked  account=0xabc");
    }

    #[test]
    fn empty_details_are_left_out_rather_than_written_blank() {
        let l = render(0, Level::Info, "Pool cleared", &[("pool", String::new()), ("chain", "robin".into())]);
        assert!(!l.contains("pool="), "a blank value earns no column: {l}");
        assert!(l.contains("chain=robin"));
    }

    #[test]
    fn a_rendered_line_still_knows_its_level() {
        for lvl in [Level::Action, Level::Trade, Level::Warn, Level::Error, Level::Info] {
            let l = render(0, lvl, "x", &[]);
            assert_eq!(Level::of(&l), Some(lvl), "{l}");
        }
    }

    #[test]
    fn logging_puts_the_line_where_the_screen_reads_it() {
        let marker = "a marker no other test writes";
        log(Level::Info, marker, &[]);
        assert!(recent().iter().any(|l| l.contains(marker)), "not in the ring");
    }

    #[test]
    fn addresses_are_shortened_from_both_ends() {
        assert_eq!(short("0xbF93d16A2A0BD298bb274ba8E824097BD1122671"), "0xbF93…2671");
        // Short enough to read whole is left alone.
        assert_eq!(short("ETH"), "ETH");
    }
}
