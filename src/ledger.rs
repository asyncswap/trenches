// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The fill ledger: every closed trade, on disk, forever.
//!
//! Everything else the app knows about profit dies with the process — realized
//! PnL is a float on `Bot`, the orders table is a ring buffer, and the daily
//! baseline is one number that a restart on the same day silently keeps. None
//! of that can answer "how did last Tuesday go", which is the whole point of a
//! calendar.
//!
//! So sells append one line here. Append-only JSONL, deliberately: a crash
//! mid-write costs the last line and nothing else, and a file that is only ever
//! extended cannot be corrupted by a second instance running beside the first.
//! Buys are not recorded — a buy has no profit yet, and the cost basis it
//! creates is already carried on `Bot` until the sell that consumes it.

use serde::{Deserialize, Serialize};

/// One closed trade, in the currency it was quoted in plus a USD stamp.
///
/// The USD rate is stored per fill rather than converted at read time: what a
/// trade made is what it made on the day, and re-pricing March's profit at
/// today's ETH would rewrite history every time the market moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    /// Unix seconds, when the sell confirmed.
    pub ts: u64,
    /// Which chain — one wallet can trade on several, and a day mixes them.
    pub chain: String,
    /// The memecoin's ticker, for the winners/losers list.
    pub sym: String,
    /// Its address/mint, because tickers are not unique and scams reuse them.
    pub token: String,
    /// Realized profit in the quote currency (ETH or SOL). Signed.
    pub pnl: f64,
    /// What the sold portion cost, same currency. Zero for a free bag.
    pub cost: f64,
    /// What the sell brought in, same currency.
    pub proceeds: f64,
    /// "ETH" / "SOL" — the currency `pnl`, `cost` and `proceeds` are in.
    pub quote_sym: String,
    /// USD per unit of that currency, AT THE TIME OF THE FILL.
    pub quote_usd: f64,
    /// The sell's transaction hash/signature.
    pub tx: String,
    /// How long the position was held, in seconds.
    ///
    /// Optional because the entry time is not always known — a coin bought
    /// before this field existed, or sold from a bag the app did not see bought,
    /// has no start to measure from. Guessing one would make the number a lie
    /// exactly where it is most interesting.
    #[serde(default)]
    pub held_secs: Option<u64>,
    /// What the SELL cost to send, in the quote currency.
    ///
    /// Reported separately rather than hidden inside `proceeds`, because the
    /// two answer different questions: `proceeds` is what the pool paid, this
    /// is what the chain took. `pnl` is already net of both — the buy's gas
    /// went into `cost` when the buy confirmed.
    ///
    /// Zero on fills written before gas was measured, which is a real zero for
    /// this chain more often than not, and in any case not a number to invent.
    #[serde(default)]
    pub gas: f64,
    /// Chained proof over this fill's fields and the proof of the one before
    /// it in this account's file. See `verification`.
    ///
    /// The orders file is rewritten whole on every save, so its chain is
    /// recomputed each time. This one never is: fills are append-only, so each
    /// proof was computed once, at the moment the sell confirmed, and links to
    /// a proof that was already on disk. Rewriting one line means rewriting
    /// every line after it — with the secret.
    ///
    /// Empty on fills written before proofs existed: unverified, not forged.
    #[serde(default)]
    pub proof: String,
    /// Which field set `proof` covers.
    ///
    /// A chained log that is only ever appended cannot be re-proved when the
    /// fields change — and they did: adding `gas` to the covered set
    /// invalidated every proof already on disk, so six honest fills came back
    /// flagged as tampered by a change I made. That is the warning crying
    /// wolf, which is the one failure a warning cannot survive.
    ///
    /// So each fill records the scheme it was written under. A row from an
    /// older scheme is UNVERIFIED — it predates the guarantee — rather than
    /// broken, and rows from the current one still chain to each other.
    ///
    /// 0 = written before proofs were versioned.
    #[serde(default)]
    pub pv: u32,
    /// Whether this fill's proof matched when the file was read. Not persisted
    /// — it is a fact about the last read, not about the record.
    #[serde(skip)]
    pub verified: bool,
}

impl Fill {
    /// Whether this sell had a cost to subtract.
    ///
    /// A sell of tokens the app never saw bought has no basis: the buy happened
    /// in a session before this one, on another machine, or before the basis
    /// was kept on disk at all. `pnl` for such a fill is `proceeds - 0`, which
    /// is the whole sale reported as profit — a $10.63 "win" on a bag that may
    /// have cost $30.
    ///
    /// Derived rather than stored, so it applies to every fill already written
    /// and does not disturb a single proof. An airdropped bag looks the same
    /// from here, and is treated the same on purpose: what this says is "this
    /// app has no record of paying for these", and calling that profit is the
    /// error either way.
    pub fn basis_known(&self) -> bool {
        self.cost > 1e-12 || self.proceeds <= 1e-12
    }

    /// The profit this fill may be added to a total with — zero when the basis
    /// is unknown, because an unknown number is not a large one.
    ///
    /// Not dropped from the ledger and not hidden in the UI: the sale happened
    /// and the proceeds are real. It is only barred from arithmetic that would
    /// present a guess as a measurement.
    pub fn counted_pnl(&self) -> f64 {
        if self.basis_known() { self.pnl } else { 0.0 }
    }

    /// Profit in dollars, at the rate that applied when it was made.
    pub fn usd(&self) -> f64 {
        self.counted_pnl() * self.quote_usd
    }

    /// What came out of the sale, in dollars — known even when the cost is not.
    pub fn proceeds_usd(&self) -> f64 {
        self.proceeds * self.quote_usd
    }

    /// What went in, in dollars. `None` when the app has no record of a buy.
    pub fn cost_usd(&self) -> Option<f64> {
        self.basis_known().then_some(self.cost * self.quote_usd)
    }

    /// How long it was held, short enough for a column. A trade measured in
    /// seconds is the whole point of this app, so seconds are not rounded away.
    pub fn held(&self) -> String {
        match self.held_secs {
            None => "—".into(),
            Some(s) if s < 60 => format!("{s}s"),
            Some(s) if s < 3600 => format!("{}m{}s", s / 60, s % 60),
            Some(s) if s < 86_400 => format!("{}h{}m", s / 3600, (s % 3600) / 60),
            Some(s) => format!("{}d", s / 86_400),
        }
    }

    /// Return on cost, as a percentage. Zero when there was no cost to return
    /// on — a free bag is infinite return, which is not a number to display.
    pub fn ret_pct(&self) -> f64 {
        if self.cost > 1e-12 {
            self.pnl / self.cost * 100.0
        } else {
            0.0
        }
    }

    /// The return as a column, or `—` when there is nothing to divide by.
    ///
    /// A sell with no recorded buy — tokens that arrived some other way, or
    /// were bought before the ledger existed — has no basis, so its return is
    /// undefined rather than zero. Printing "+0%" beside a $1.72 profit says
    /// the trade broke even, which is the opposite of what happened.
    pub fn ret_col(&self) -> String {
        if self.cost > 1e-12 {
            format!("{:+.0}%", self.ret_pct())
        } else {
            "—".to_string()
        }
    }
}

/// Where one account's fills live. Per account, because PnL is per wallet —
/// two wallets' trades in one file could only ever be added up wrongly.
/// Account labels come from config and can hold anything — including the
/// slashes of a Foundry keystore path, and the default `"(no account)"`. Keep
/// the filename to characters that survive every filesystem we run on.
///
/// Public so every per-account file agrees: `daily-` used the raw label and so
/// silently failed to write for any account the ledger was happily renaming.
pub fn safe_account(account: &str) -> String {
    account
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

pub fn path(account: &str) -> String {
    format!("{}/fills-{}.jsonl", crate::state_dir(), safe_account(account))
}

/// The fields a fill's proof covers, in a fixed order.
///
/// Everything that would change what the trade earned or which trade it was.
/// `held_secs` is in because a forged hold time rewrites how the trade was won,
/// and `quote_usd` because re-stamping the rate silently re-prices the day.
/// Adding a field here invalidates every existing proof, which is correct — an
/// old proof did not attest to the new field.
/// The current proof scheme. Bump when `fill_fields` changes; never edit a
/// past version's field list, or the rows written under it stop verifying.
pub const PROOF_VERSION: u32 = 1;

pub fn fill_fields(f: &Fill) -> Vec<String> {
    vec![
        f.ts.to_string(),
        f.chain.clone(),
        f.sym.clone(),
        f.token.clone(),
        format!("{:.18}", f.pnl),
        format!("{:.18}", f.cost),
        format!("{:.18}", f.proceeds),
        f.quote_sym.clone(),
        format!("{:.8}", f.quote_usd),
        f.tx.clone(),
        f.held_secs.map(|s| s.to_string()).unwrap_or_default(),
        format!("{:.18}", f.gas),
    ]
}

/// The proof on the last line of an account's file — the link a new fill
/// chains onto. Empty when the file is new, or ends in a pre-proof line.
fn last_proof(account: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path(account)) else { return String::new() };
    // The last proof FROM THIS SCHEME. Chaining onto an older one would make
    // the new row unverifiable the moment the old row is skipped, which is
    // exactly what skipping it is meant to avoid.
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Fill>(l).ok())
        .find(|f| f.pv == PROOF_VERSION && !f.proof.is_empty())
        .map(|f| f.proof)
        .unwrap_or_default()
}

/// Append one fill, chained onto the proof already on disk.
///
/// Failures are swallowed on purpose: a full disk must not turn a completed
/// sell into an error the trader has to think about.
pub fn append(account: &str, f: &Fill) {
    use std::io::Write;
    // Chain onto what is on disk, NOT onto anything held in memory — another
    // instance may have appended since, and reading the tail is how this stays
    // correct without a lock.
    let mut f = f.clone();
    f.pv = PROOF_VERSION;
    let fields = fill_fields(&f);
    let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    f.proof = crate::verification::proof(&last_proof(account), &refs);
    let Ok(line) = serde_json::to_string(&f) else { return };
    let _ = std::fs::create_dir_all(crate::state_dir());
    if let Ok(mut h) = std::fs::OpenOptions::new().create(true).append(true).open(path(account)) {
        let _ = writeln!(h, "{line}");
    }
}

/// Verify a file's chain in place, marking each fill and returning the index of
/// the first break.
///
/// A broken fill is FLAGGED, never dropped: a profit record that quietly
/// disappears is worse than one you are told to distrust. Everything from the
/// break onward is marked too — a chain that breaks at row `i` says nothing
/// about row `i+1`.
fn verify_chain(fills: &mut [Fill]) -> Option<usize> {
    // A row from an older scheme presents an EMPTY proof to the checker, which
    // treats it as pre-proof and carries the chain past it untouched. It is
    // unverified, not broken: nobody edited it, the rules changed underneath
    // it, and saying "tampered" about that is how a warning stops being read.
    let records: Vec<(String, Vec<String>)> = fills
        .iter()
        .map(|f| {
            if f.pv == PROOF_VERSION {
                (f.proof.clone(), fill_fields(f))
            } else {
                (String::new(), Vec::new())
            }
        })
        .collect();
    let broken = crate::verification::first_broken(&records);
    for (i, f) in fills.iter_mut().enumerate() {
        f.verified =
            f.pv == PROOF_VERSION && !f.proof.is_empty() && broken.is_none_or(|b| i < b);
    }
    broken
}

/// Every account's fills, merged, oldest first.
///
/// The calendar is about a person's trading, not one keypair's: EVM and Solana
/// sign with different keys and so write different files, but a Tuesday is one
/// Tuesday. Each fill carries its own chain, so they stay tellable apart.
/// What one account's fills add up to today, in the quote currency.
///
/// The same arithmetic the calendar does, from the same file, so the wallet's
/// day figure and the calendar's cannot disagree. They used to: the wallet
/// measured the balance against a baseline taken at the start of the day,
/// which counts gas, and counts a deposit as profit.
pub fn today_total(account: &str) -> f64 {
    let today = date_of(now());
    total(account, |f| {
        let d = date_of(f.ts);
        d.y == today.y && d.m == today.m && d.d == today.d
    })
}

/// What one account's fills add up to since a given moment.
pub fn total_since(account: &str, since: u64) -> f64 {
    total(account, |f| f.ts >= since)
}

/// One account's fills, as they are on disk, each marked verified or not.
pub fn load(account: &str) -> Vec<Fill> {
    let Ok(text) = std::fs::read_to_string(path(account)) else { return Vec::new() };
    let mut v: Vec<Fill> = text.lines().filter_map(|l| serde_json::from_str::<Fill>(l).ok()).collect();
    verify_chain(&mut v);
    v
}

/// One account's fills, plus the first row whose proof did not match.
///
/// Separate from `load` because the totals do not care and the UI does: the
/// arithmetic still has to run over every fill — refusing to show a number is
/// not safer than showing one marked untrustworthy — but somebody has to say
/// so out loud, once, naming the file and the row.
pub fn load_checked(account: &str) -> (Vec<Fill>, Option<usize>) {
    let Ok(text) = std::fs::read_to_string(path(account)) else { return (Vec::new(), None) };
    let mut v: Vec<Fill> = text.lines().filter_map(|l| serde_json::from_str::<Fill>(l).ok()).collect();
    let broken = verify_chain(&mut v);
    (v, broken)
}

fn total(account: &str, keep: impl Fn(&Fill) -> bool) -> f64 {
    // `counted_pnl`, not `pnl`: a sell with no recorded buy would otherwise
    // add its entire proceeds to the day as profit.
    load(account).iter().filter(|f| keep(f)).map(|f| f.counted_pnl()).sum()
}

pub fn load_all() -> Vec<Fill> {
    let Ok(dir) = std::fs::read_dir(crate::state_dir()) else { return Vec::new() };
    let mut v: Vec<Fill> = dir
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("fills-") && n.ends_with(".jsonl")
        })
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .flat_map(|t| {
            // Verify PER FILE, before merging. Each account's file is its own
            // chain; interleaving two accounts by timestamp and then checking
            // would break every proof in both.
            let mut v: Vec<Fill> =
                t.lines().filter_map(|l| serde_json::from_str::<Fill>(l).ok()).collect();
            verify_chain(&mut v);
            v
        })
        .collect();
    v.sort_by_key(|f| f.ts);
    v
}

/// Seconds since the unix epoch, now.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Calendar arithmetic
//
// Hand-rolled rather than pulling in chrono: what the calendar needs is a civil
// date from a unix timestamp and back, which is two well-known functions. The
// alternative is a dependency and its timezone database for a month grid.
// ─────────────────────────────────────────────────────────────────────────────

/// A civil date. Local time, because a trading day is the one you lived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub y: i32,
    pub m: u32,
    pub d: u32,
}

/// Days from the unix epoch to a civil date. Howard Hinnant's `days_from_civil`
/// — exact for the whole proleptic Gregorian calendar, no lookup tables.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // March = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era as i64 * 146097 + doe - 719468
}

/// The inverse: a civil date from days since the epoch.
pub fn civil_from_days(z: i64) -> Date {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    Date { y: (if m <= 2 { y + 1 } else { y }) as i32, m, d }
}

/// The local UTC offset in seconds, read once from the system.
///
/// `localtime` on a unix host means reading the TZ database, which is a
/// dependency we do not have. `date +%z` is already installed everywhere we
/// run and costs one process at startup — and a wrong sign here would put
/// every evening trade on the wrong day, so it is worth the exec.
pub fn utc_offset() -> i64 {
    use std::sync::OnceLock;
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let Ok(out) = std::process::Command::new("date").arg("+%z").output() else { return 0 };
        let s = String::from_utf8_lossy(&out.stdout);
        let s = s.trim();
        // "+0530" / "-0800"
        if s.len() < 5 {
            return 0;
        }
        let sign = if s.starts_with('-') { -1 } else { 1 };
        let h: i64 = s[1..3].parse().unwrap_or(0);
        let m: i64 = s[3..5].parse().unwrap_or(0);
        sign * (h * 3600 + m * 60)
    })
}

/// The local civil date a timestamp falls on.
pub fn date_of(ts: u64) -> Date {
    civil_from_days((ts as i64 + utc_offset()).div_euclid(86_400))
}

/// Days in a month, leap years included.
pub fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        // February: the full Gregorian rule, not the divisible-by-4 shortcut.
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Day of week for a date, 0 = Monday … 6 = Sunday.
///
/// Monday-first because that is how the grid is laid out, and a weekend that
/// splits across the two ends of a row is harder to read than one that closes it.
pub fn weekday(y: i32, m: u32, d: u32) -> u32 {
    // 1970-01-01 was a Thursday = 3 in a Monday-first week.
    (days_from_civil(y, m, d) + 3).rem_euclid(7) as u32
}

/// Step a year/month pair by whole months, in either direction.
pub fn shift_month(y: i32, m: u32, by: i32) -> (i32, u32) {
    let total = y * 12 + (m as i32 - 1) + by;
    (total.div_euclid(12), total.rem_euclid(12) as u32 + 1)
}

pub const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_round_trip_across_leap_boundaries() {
        // The days that break naive calendar code: leap day, the century that
        // is not a leap year, the one that is, and the epoch itself.
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 2, 29), // divisible by 400 → leap
            (2024, 2, 29),
            (2026, 12, 31),
            (2100, 3, 1), // divisible by 100, NOT by 400 → not leap
        ] {
            let z = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(z), Date { y, m, d }, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn february_length_follows_the_full_gregorian_rule() {
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        // 1900 is the case the "divisible by 4" shortcut gets wrong.
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
    }

    #[test]
    fn weekdays_match_known_dates() {
        // 0 = Monday. 1970-01-01 was a Thursday; 2026-07-26 is a Sunday.
        assert_eq!(weekday(1970, 1, 1), 3);
        assert_eq!(weekday(2026, 7, 26), 6);
        assert_eq!(weekday(2024, 2, 29), 3);
    }

    #[test]
    fn months_step_across_year_boundaries_in_both_directions() {
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
        assert_eq!(shift_month(2026, 12, 1), (2027, 1));
        assert_eq!(shift_month(2026, 6, -18), (2024, 12));
        assert_eq!(shift_month(2026, 6, 18), (2027, 12));
    }

    #[test]
    fn a_fill_reports_profit_in_the_dollars_of_its_own_day() {
        // The point of stamping the rate: this trade made $200 when ETH was
        // $2000, and it still made $200 after ETH moved.
        let f = Fill {
            ts: 0,
            chain: "robinhood".into(),
            sym: "PEPE".into(),
            token: "0x1".into(),
            pnl: 0.1,
            cost: 0.5,
            proceeds: 0.6,
            quote_sym: "ETH".into(),
            quote_usd: 2000.0,
            tx: "0xabc".into(),
            held_secs: Some(42),
            gas: 0.0,
            proof: String::new(),
            pv: 0,
            verified: true,
        };
        assert!((f.usd() - 200.0).abs() < 1e-9);
        assert!((f.ret_pct() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_bag_with_no_recorded_cost_reports_neither_an_infinite_return_nor_a_profit() {
        let f = Fill {
            ts: 0,
            chain: "solana".into(),
            sym: "BONK".into(),
            token: "So1".into(),
            pnl: 1.0,
            cost: 0.0, // airdropped, or sold past the basis
            proceeds: 1.0,
            quote_sym: "SOL".into(),
            quote_usd: 150.0,
            tx: "sig".into(),
            held_secs: None,
            gas: 0.0,
            proof: String::new(),
            pv: 0,
            verified: true,
        };
        assert_eq!(f.ret_pct(), 0.0);
        // This assertion used to read `usd() == 150.0` — the whole sale
        // counted as profit. That was the bug: a bag with no recorded cost is
        // one the app never saw bought, and its profit is unknown, not equal
        // to its proceeds. The money that came out is still reported.
        assert_eq!(f.usd(), 0.0);
        assert!((f.proceeds_usd() - 150.0).abs() < 1e-9);
    }

    #[test]
    fn account_labels_cannot_escape_the_state_directory() {
        // Labels come from config; a path separator in one must not aim the
        // ledger at another directory.
        assert_eq!(path("../../etc/passwd"), format!("{}/fills-------etc-passwd.jsonl", crate::state_dir()));
        assert_eq!(path("main"), format!("{}/fills-main.jsonl", crate::state_dir()));
    }

#[cfg(test)]
mod ret_col_tests {
    use super::*;

    fn fill(pnl: f64, cost: f64) -> Fill {
        Fill {
            ts: 0, chain: "t".into(), sym: "A".into(), token: "0x1".into(),
            pnl, cost, proceeds: cost + pnl, quote_sym: "ETH".into(),
            quote_usd: 1.0, tx: "0x0".into(), held_secs: None,
            gas: 0.0, proof: String::new(), pv: 0, verified: true,
        }
    }

    #[test]
    fn a_sell_with_no_basis_reports_no_return() {
        // Sold for a profit with nothing recorded as paid: the return is
        // undefined, and saying "+0%" would call it break-even.
        assert_eq!(fill(0.000_931, 0.0).ret_col(), "—");
    }

    /// A fill's proof has to cover the profit, or it proves nothing worth
    /// proving: the number someone would want to change is the number itself.
    #[test]
    fn a_fills_proof_covers_the_money() {
        let base = fill(0.5, 1.0);
        let f = |x: &Fill| {
            let v = fill_fields(x);
            let r: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
            crate::verification::proof("", &r)
        };
        let mut edited = base.clone();
        edited.pnl = 5.0;
        assert_ne!(f(&base), f(&edited), "the profit is covered");

        let mut edited = base.clone();
        edited.quote_usd = 9_999.0;
        assert_ne!(f(&base), f(&edited), "the day's rate is covered");

        let mut edited = base.clone();
        edited.tx = "0xdead".into();
        assert_ne!(f(&base), f(&edited), "which trade it was is covered");

        let mut edited = base.clone();
        edited.held_secs = Some(1);
        assert_ne!(f(&base), f(&edited), "how long it was held is covered");
    }

    /// The regression that prompted versioning: adding `gas` to the covered
    /// fields turned six honest fills into "⚠ unverified" on the calendar. A
    /// row written under an older scheme predates the guarantee; it is not
    /// evidence of tampering, and saying so is how a warning stops being read.
    #[test]
    fn a_fill_from_an_older_scheme_is_unverified_not_broken() {
        let mut chain: Vec<Fill> = Vec::new();
        // Two rows written before proofs were versioned, carrying proofs from
        // a field set that no longer exists.
        for pnl in [0.1, 0.2] {
            let mut f = fill(pnl, 1.0);
            f.pv = 0;
            f.proof = "deadbeef".into();
            chain.push(f);
        }
        // Then two written under the current scheme, chained to each other.
        let mut prev = String::new();
        for pnl in [0.3, 0.4] {
            let mut f = fill(pnl, 1.0);
            f.pv = PROOF_VERSION;
            let v = fill_fields(&f);
            let r: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
            f.proof = crate::verification::proof(&prev, &r);
            prev = f.proof.clone();
            chain.push(f);
        }
        assert_eq!(verify_chain(&mut chain), None, "the old rows do not break it");
        assert!(!chain[0].verified && !chain[1].verified, "old rows are unverified");
        assert!(chain[2].verified && chain[3].verified, "new rows still verify");
    }

    /// And tampering with a CURRENT row is still caught, past the old ones.
    #[test]
    fn versioning_does_not_blunt_the_check() {
        let mut chain: Vec<Fill> = Vec::new();
        let mut old = fill(0.1, 1.0);
        old.pv = 0;
        old.proof = "stale".into();
        chain.push(old);
        let mut prev = String::new();
        for pnl in [0.3, 0.4] {
            let mut f = fill(pnl, 1.0);
            f.pv = PROOF_VERSION;
            let v = fill_fields(&f);
            let r: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
            f.proof = crate::verification::proof(&prev, &r);
            prev = f.proof.clone();
            chain.push(f);
        }
        chain[1].pnl = 99.0; // somebody edits the profit
        assert_eq!(verify_chain(&mut chain), Some(1));
        assert!(!chain[2].verified, "and everything after it");
    }

    /// Editing one fill must flag it AND everything after — a chain that breaks
    /// at row `i` says nothing about row `i+1`.
    #[test]
    fn an_edited_fill_and_everything_after_it_stops_verifying() {
        let mut chain: Vec<Fill> = Vec::new();
        let mut prev = String::new();
        for pnl in [0.1, 0.2, 0.3] {
            let mut f = fill(pnl, 1.0);
            f.pv = PROOF_VERSION; // written under the current scheme
            let v = fill_fields(&f);
            let r: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
            f.proof = crate::verification::proof(&prev, &r);
            prev = f.proof.clone();
            chain.push(f);
        }
        assert_eq!(verify_chain(&mut chain), None, "an untouched ledger verifies");
        assert!(chain.iter().all(|f| f.verified));

        // Someone doubles the middle trade's profit and leaves its proof alone.
        chain[1].pnl = 0.4;
        assert_eq!(verify_chain(&mut chain), Some(1), "the edited row is named");
        assert!(chain[0].verified, "the rows before it are still good");
        assert!(!chain[1].verified && !chain[2].verified, "it and everything after are suspect");
    }

    /// Gas is inside the profit, from both ends of the trade.
    ///
    /// The AI round trip: 0.000408 ETH in, 0.000418 out. Nineteen thousandths
    /// of a dollar of profit on stakes this size is entirely capable of being
    /// less than what the two transactions cost to send, and a screen that
    /// leaves gas out cannot tell a small win from a small loss.
    #[test]
    fn profit_is_net_of_what_both_transactions_cost_to_send() {
        let cost_in = 0.000_408;
        let out = 0.000_418;
        let buy_gas = 0.000_004;
        let sell_gas = 0.000_004;
        // What the engine books: gas in on the basis, gas out of the proceeds.
        let basis = cost_in + buy_gas;
        let proceeds = out - sell_gas;
        let mut f = fill(proceeds - basis, basis);
        f.proceeds = proceeds;
        f.gas = buy_gas + sell_gas;
        f.quote_usd = 1_871.0;

        assert!(f.basis_known());
        // 0.000414 out against 0.000412 in — still a win, but a third of what
        // the gross difference of 0.000010 would have claimed.
        assert!((f.pnl - 0.000_002).abs() < 1e-9, "pnl was {}", f.pnl);
        assert!(f.pnl < out - cost_in, "gas can only make a trade worse");
        assert!(f.gas > 0.0, "and the amount is reported, not just subtracted");
    }

    /// The case that matters most: a trade that looks green gross and is red
    /// once the chain is paid.
    #[test]
    fn a_gross_win_smaller_than_its_gas_is_reported_as_a_loss() {
        let basis = 0.001_000 + 0.000_020; // buy plus its gas
        let proceeds = 0.001_010 - 0.000_020; // sell less its gas
        let mut f = fill(proceeds - basis, basis);
        f.proceeds = proceeds;
        f.gas = 0.000_040;
        assert!(f.pnl < 0.0, "gross +0.00001, net {}", f.pnl);
        assert!(f.counted_pnl() < 0.0, "and it reaches the day total as a loss");
    }

    /// The bug this was written for: TOK was bought in an earlier session, so
    /// its basis was not on disk, so the sell subtracted zero and reported the
    /// entire 0.005679 ETH of proceeds as a $10.63 profit.
    #[test]
    fn a_sell_with_no_recorded_buy_is_not_counted_as_profit() {
        let mut f = fill(0.005_679, 0.0); // pnl == proceeds, cost unknown
        f.proceeds = 0.005_679;
        f.quote_usd = 1_871.0;
        assert!(!f.basis_known(), "no cost against real proceeds means no basis");
        assert_eq!(f.counted_pnl(), 0.0, "an unknown profit is not a large one");
        assert_eq!(f.usd(), 0.0, "and it does not reach the day total");
        assert_eq!(f.cost_usd(), None, "what went in is unknown, not zero");
        assert!(f.proceeds_usd() > 10.0, "what came out is still known");
    }

    /// The number stays a number when the basis IS known — the guard must not
    /// swallow ordinary trades.
    #[test]
    fn an_ordinary_sell_still_counts() {
        let f = fill(0.5, 1.0);
        assert!(f.basis_known());
        assert_eq!(f.counted_pnl(), 0.5);
        assert_eq!(f.cost_usd(), Some(1.0));
    }

    /// A sell that brought in nothing — a failed or dust exit — has no
    /// proceeds to mistake for profit, so it is not "unpriced".
    #[test]
    fn a_sale_of_nothing_is_not_treated_as_an_unknown_basis() {
        let mut f = fill(0.0, 0.0);
        f.proceeds = 0.0;
        assert!(f.basis_known());
        assert_eq!(f.counted_pnl(), 0.0);
    }

    #[test]
    fn an_ordinary_trade_still_reports_a_percentage() {
        assert_eq!(fill(0.5, 1.0).ret_col(), "+50%");
        assert_eq!(fill(-0.5, 1.0).ret_col(), "-50%");
    }
}
}
