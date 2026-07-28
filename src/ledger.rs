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
}

impl Fill {
    /// Profit in dollars, at the rate that applied when it was made.
    pub fn usd(&self) -> f64 {
        self.pnl * self.quote_usd
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
}

/// Where one account's fills live. Per account, because PnL is per wallet —
/// two wallets' trades in one file could only ever be added up wrongly.
pub fn path(account: &str) -> String {
    // Account labels come from config and can hold anything; keep the filename
    // to characters that survive every filesystem we run on.
    let safe: String = account
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("{}/fills-{}.jsonl", crate::STATE_DIR, safe)
}

/// Append one fill. Failures are swallowed on purpose: a full disk must not
/// turn a completed sell into an error the trader has to think about.
pub fn append(account: &str, f: &Fill) {
    use std::io::Write;
    let Ok(line) = serde_json::to_string(f) else { return };
    let _ = std::fs::create_dir_all(crate::STATE_DIR);
    if let Ok(mut h) = std::fs::OpenOptions::new().create(true).append(true).open(path(account)) {
        let _ = writeln!(h, "{line}");
    }
}

/// Every account's fills, merged, oldest first.
///
/// The calendar is about a person's trading, not one keypair's: EVM and Solana
/// sign with different keys and so write different files, but a Tuesday is one
/// Tuesday. Each fill carries its own chain, so they stay tellable apart.
pub fn load_all() -> Vec<Fill> {
    let Ok(dir) = std::fs::read_dir(crate::STATE_DIR) else { return Vec::new() };
    let mut v: Vec<Fill> = dir
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("fills-") && n.ends_with(".jsonl")
        })
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .flat_map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Fill>(l).ok())
                .collect::<Vec<_>>()
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
        };
        assert!((f.usd() - 200.0).abs() < 1e-9);
        assert!((f.ret_pct() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_free_bag_does_not_report_an_infinite_return() {
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
        };
        assert_eq!(f.ret_pct(), 0.0);
        assert!((f.usd() - 150.0).abs() < 1e-9);
    }

    #[test]
    fn account_labels_cannot_escape_the_state_directory() {
        // Labels come from config; a path separator in one must not aim the
        // ledger at another directory.
        assert_eq!(path("../../etc/passwd"), format!("{}/fills-------etc-passwd.jsonl", crate::STATE_DIR));
        assert_eq!(path("main"), format!("{}/fills-main.jsonl", crate::STATE_DIR));
    }
}
