// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The currency the screen is read in.
//!
//! Everything this app measures is priced in USD, because that is what the
//! feeds quote and what the ledger has always written down. That is a fine
//! internal unit and a poor one to read a number in if dollars are not the
//! money you actually hold — a profit in a currency you have to convert in
//! your head is a profit you cannot feel.
//!
//! So USD stays the unit of RECORD and becomes only one possible unit of
//! DISPLAY. Nothing stored changes: `Fill.quote_usd` is still the dollar rate
//! that applied on the day of the trade, which is the whole reason the calendar
//! does not rewrite March's profit every time the market moves. The conversion
//! happens in the formatters, at the moment of drawing, and nowhere else.
//!
//! Rates come from Coinbase in one request — `exchange-rates?currency=USD`
//! returns every currency it knows against the dollar, so selecting a new one
//! costs nothing and works offline for as long as the last fetch is good.
//!
//! WHAT THIS IS NOT: a second source of truth about value. A converted figure
//! is the USD figure times a rate fetched at some point today. It is right for
//! reading and wrong for accounting, and the two are kept apart on purpose.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::lock;

/// The selected currency and what one USD is worth in it.
#[derive(Clone, Debug)]
pub struct Display {
    /// ISO code — "USD", "EUR", "JPY", "NGN".
    pub code: String,
    /// Units of `code` per USD. Exactly 1.0 for USD itself.
    pub per_usd: f64,
}

impl Default for Display {
    fn default() -> Self {
        // Dollars until told otherwise, and dollars if a fetch never lands.
        // A rate of 1.0 against USD is not an approximation.
        Display { code: "USD".into(), per_usd: 1.0 }
    }
}

fn current() -> &'static Mutex<Display> {
    static CUR: OnceLock<Mutex<Display>> = OnceLock::new();
    CUR.get_or_init(|| Mutex::new(Display::default()))
}

/// The whole rate table from the last successful fetch, so switching currency
/// is instant and does not need the network.
fn table() -> &'static Mutex<HashMap<String, f64>> {
    static T: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Display {
    /// A USD amount, in this currency.
    pub fn from_usd(&self, usd: f64) -> f64 {
        if !usd.is_finite() { usd } else { usd * self.per_usd }
    }

    /// The prefix a figure wears. A symbol where one is unambiguous in a
    /// terminal, and the ISO code otherwise.
    ///
    /// `$` for USD, `€` for EUR — those read as money instantly. But `$` is
    /// also CAD, AUD, NZD, HKD and a dozen more, and a Canadian reading `$` on
    /// a screen that means USD is being actively misled. So anything that
    /// shares a glyph gets its code instead: `CA$`, `A$`. Being unmistakable
    /// matters more here than being pretty.
    pub fn symbol(&self) -> String {
        match self.code.as_str() {
            "USD" => "$".into(),
            "EUR" => "€".into(),
            "GBP" => "£".into(),
            "JPY" => "¥".into(),
            "CNY" => "CN¥".into(),
            "INR" => "₹".into(),
            "KRW" => "₩".into(),
            "NGN" => "₦".into(),
            "RUB" => "₽".into(),
            "TRY" => "₺".into(),
            "BRL" => "R$".into(),
            "CAD" => "CA$".into(),
            "AUD" => "A$".into(),
            "NZD" => "NZ$".into(),
            "MXN" => "MX$".into(),
            "CHF" => "CHF ".into(),
            "SEK" | "NOK" | "DKK" => format!("{} ", self.code),
            // Crypto as a display currency is legitimate — "how many sats is
            // this" is a real question — and no glyph for it is universal.
            "BTC" => "₿".into(),
            "ETH" => "Ξ".into(),
            _ => format!("{} ", self.code),
        }
    }
}

/// The selected currency's code.
pub fn code() -> String {
    lock(current()).code.clone()
}

/// Whether the screen is in dollars — the case where no conversion happens at
/// all, and the one every formatter can take a shortcut for.
pub fn is_usd() -> bool {
    lock(current()).code == "USD"
}

/// A USD amount, in the selected currency.
pub fn from_usd(usd: f64) -> f64 {
    lock(current()).from_usd(usd)
}

/// The selected currency's prefix.
pub fn symbol() -> String {
    lock(current()).symbol()
}

/// Choose the display currency from the rates already fetched.
///
/// Returns false when the code is unknown, rather than selecting a currency
/// whose rate would silently be 1.0 — a screen quietly showing dollars while
/// labelled yen is worse than a refusal.
pub fn select(code: &str) -> bool {
    let code = code.trim().to_uppercase();
    if code == "USD" {
        *lock(current()) = Display::default();
        return true;
    }
    let rate = lock(table()).get(&code).copied();
    match rate {
        Some(r) if r.is_finite() && r > 0.0 => {
            *lock(current()) = Display { code, per_usd: r };
            true
        }
        _ => false,
    }
}

/// Every currency we have a rate for, sorted, with the majors first.
///
/// Not alphabetical throughout: a picker that opens on AED and needs scrolling
/// to reach EUR is a picker that has ranked completeness over use.
pub fn available() -> Vec<String> {
    const MAJORS: [&str; 10] =
        ["USD", "EUR", "GBP", "JPY", "CNY", "CAD", "AUD", "CHF", "INR", "BTC"];
    let t = lock(table());
    let mut rest: Vec<String> =
        t.keys().filter(|k| !MAJORS.contains(&k.as_str())).cloned().collect();
    rest.sort();
    let mut out: Vec<String> = MAJORS
        .iter()
        .filter(|m| **m == "USD" || t.contains_key(**m))
        .map(|m| m.to_string())
        .collect();
    out.extend(rest);
    out
}

/// Re-read the selected currency's rate after a refresh, so a session left open
/// overnight is not still converting at yesterday's number.
fn resync() {
    let code = code();
    if code == "USD" {
        return;
    }
    if let Some(r) = lock(table()).get(&code).copied() {
        if r.is_finite() && r > 0.0 {
            lock(current()).per_usd = r;
        }
    }
}

/// Fetch every currency against the dollar. One request, all of them.
///
/// Best-effort like the rest of the price feed: on any failure the previous
/// table stands, which means a dropped network changes nothing on screen rather
/// than reverting every figure to dollars underneath the reader.
pub async fn refresh() -> usize {
    let url = "https://api.coinbase.com/v2/exchange-rates?currency=USD";
    let Ok(resp) = reqwest::Client::new()
        .get(url)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(6))
        .send()
        .await
    else {
        return 0;
    };
    let Ok(json) = resp.json::<serde_json::Value>().await else {
        return 0;
    };
    let Some(rates) = json.get("data").and_then(|d| d.get("rates")).and_then(|r| r.as_object())
    else {
        return 0;
    };
    let mut next = HashMap::new();
    for (k, v) in rates {
        // Coinbase sends rates as STRINGS. Parsed, not assumed to be numbers.
        if let Some(r) = v.as_str().and_then(|s| s.parse::<f64>().ok()) {
            if r.is_finite() && r > 0.0 {
                next.insert(k.to_uppercase(), r);
            }
        }
    }
    if next.is_empty() {
        return 0;
    }
    let n = next.len();
    *lock(table()) = next;
    resync();
    n
}

/// Where the choice sleeps between sessions.
///
/// In the cache beside `theme.txt`, not in the config: it is a display
/// preference somebody picked from a menu, the same kind of thing as a colour
/// scheme, and the app writing into a file it asks you to hand-edit is what the
/// config/cache split exists to avoid.
fn saved_path() -> std::path::PathBuf {
    std::path::Path::new(crate::state_dir()).join("currency.txt")
}

pub fn save(code: &str) {
    let _ = std::fs::create_dir_all(crate::state_dir());
    let _ = std::fs::write(saved_path(), code);
}

pub fn saved() -> Option<String> {
    std::fs::read_to_string(saved_path())
        .ok()
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
}

/// Fetch the table and restore the saved choice, at startup.
///
/// Silent on failure, and silent about USD: someone who never chose a currency
/// does not need to be told their dollars are dollars.
pub async fn restore() -> Option<String> {
    let want = saved()?;
    if want == "USD" {
        return None;
    }
    if lock(table()).is_empty() {
        refresh().await;
    }
    select(&want).then_some(want)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise `Display` directly rather than the process-global.
    //
    // The globals are shared by every test in the binary, including the
    // formatter tests in `view` that assert on a `$` — so a test that switched
    // the global to EUR could fail an unrelated test running beside it, which
    // it did. Behaviour worth testing does not need to be tested through a
    // singleton.
    fn at(code: &str, per_usd: f64) -> Display {
        Display { code: code.into(), per_usd }
    }

    #[test]
    fn dollars_are_not_converted_at_all() {
        let d = Display::default();
        assert_eq!(d.code, "USD");
        assert_eq!(d.from_usd(12.34), 12.34);
        assert_eq!(d.symbol(), "$");
    }

    #[test]
    fn a_selected_currency_scales_every_figure() {
        let d = at("EUR", 0.92);
        assert!((d.from_usd(100.0) - 92.0).abs() < 1e-9);
        assert_eq!(d.symbol(), "€");
    }

    /// `$` belongs to more than one currency, and a Canadian reading it as USD
    /// is being misled by the thing that was supposed to help.
    #[test]
    fn currencies_that_share_a_glyph_are_disambiguated() {
        assert_eq!(at("CAD", 1.37).symbol(), "CA$");
        assert_eq!(at("AUD", 1.5).symbol(), "A$");
        assert_ne!(at("CAD", 1.37).symbol(), at("USD", 1.0).symbol());
    }

    /// An unknown code gets its ISO code rather than a borrowed glyph.
    #[test]
    fn an_unlisted_currency_still_says_what_it_is() {
        assert_eq!(at("ZMW", 26.0).symbol(), "ZMW ");
    }

    /// A rate that never arrived must not quietly render as dollars.
    #[test]
    fn an_unknown_code_is_refused_rather_than_shown_as_dollars() {
        // No rate in the table for a code that does not exist, so `select`
        // declines and whatever was chosen before stands.
        assert!(!select("ZZZ"));
    }

    #[test]
    fn a_non_finite_amount_passes_through_untouched() {
        assert!(at("EUR", 0.92).from_usd(f64::NAN).is_nan());
    }
}
