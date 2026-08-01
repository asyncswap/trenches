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
pub struct BaseCurrency {
    /// ISO code — "USD", "EUR", "JPY", "NGN".
    pub code: String,
    /// Units of `code` per USD. Exactly 1.0 for USD itself.
    pub per_usd: f64,
}

impl Default for BaseCurrency {
    fn default() -> Self {
        // Dollars until told otherwise, and dollars if a fetch never lands.
        // A rate of 1.0 against USD is not an approximation.
        BaseCurrency { code: "USD".into(), per_usd: 1.0 }
    }
}

fn current() -> &'static Mutex<BaseCurrency> {
    static CUR: OnceLock<Mutex<BaseCurrency>> = OnceLock::new();
    CUR.get_or_init(|| Mutex::new(BaseCurrency::default()))
}

/// The whole rate table from the last successful fetch, so switching currency
/// is instant and does not need the network.
fn table() -> &'static Mutex<HashMap<String, f64>> {
    static T: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

impl BaseCurrency {
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
            "CAD" => "C$".into(),
            "AUD" => "A$".into(),
            "NZD" => "NZ$".into(),
            "MXN" => "MX$".into(),
            "CHF" => "Fr".into(),
            "SEK" | "NOK" | "DKK" => format!("{} ", self.code),
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
        *lock(current()) = BaseCurrency::default();
        return true;
    }
    if !is_fiat(&code) {
        return false; // an asset Coinbase prices is not a currency to read in
    }
    let rate = lock(table()).get(&code).copied();
    match rate {
        Some(r) if r.is_finite() && r > 0.0 => {
            *lock(current()) = BaseCurrency { code, per_usd: r };
            true
        }
        _ => false,
    }
}

/// ISO 4217 — the currencies a country issues.
///
/// Coinbase's rate table is not a currency list. It carries every asset they
/// price, so the picker offered ETH, BTC and a few hundred tokens alongside the
/// dollar. Two things wrong with that: a list nobody can find EUR in is not a
/// picker, and ETH is already the QUOTE asset of most pools here — "PnL in ETH"
/// converted from USD by an exchange rate, sitting beside a pooled-ETH figure
/// that was never converted at all, is a screen with two different ETHs on it.
///
/// So: money issued by a state. Filtering by an explicit list rather than by
/// excluding known crypto, because the exclusion list grows every week and the
/// inclusion list has not changed materially in years.
const FIAT: [&str; 162] = [
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT",
    "BGN", "BHD", "BIF", "BMD", "BND", "BOB", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD", "CAD",
    "CDF", "CHF", "CLP", "CNY", "COP", "CRC", "CUP", "CVE", "CZK", "DJF", "DKK", "DOP", "DZD",
    "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP", "GEL", "GGP", "GHS", "GIP", "GMD", "GNF",
    "GTQ", "GYD", "HKD", "HNL", "HRK", "HTG", "HUF", "IDR", "ILS", "IMP", "INR", "IQD", "IRR",
    "ISK", "JEP", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF", "KPW", "KRW", "KWD", "KYD",
    "KZT", "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD", "MMK", "MNT",
    "MOP", "MRU", "MUR", "MVR", "MWK", "MXN", "MYR", "MZN", "NAD", "NGN", "NIO", "NOK", "NPR",
    "NZD", "OMR", "PAB", "PEN", "PGK", "PHP", "PKR", "PLN", "PYG", "QAR", "RON", "RSD", "RUB",
    "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SLL", "SOS", "SRD", "SSP",
    "STN", "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD", "TWD", "TZS",
    "UAH", "UGX", "USD", "UYU", "UZS", "VES", "VND", "VUV", "WST", "XAF", "XCD", "XCG", "XDR",
    "XOF", "XPF", "YER", "ZAR", "ZMW", "ZWL",
];

/// Whether a code is a currency rather than an asset Coinbase happens to price.
pub fn is_fiat(code: &str) -> bool {
    FIAT.contains(&code)
}

/// The flag for a currency, derived rather than tabulated.
///
/// A currency code is its country's ISO 3166 code plus a letter for the
/// currency — USD is US, GBP is GB, JPY is JP — and a flag emoji is just those
/// two letters as regional indicators. So 160 flags come from three lines
/// instead of a table that would go stale the next time a country redenominates.
///
/// The `X` codes are the exceptions by design: XAF, XOF, XPF and XDR belong to
/// unions and institutions rather than countries, and there is no flag to
/// derive. They get a neutral mark instead of a wrong one.
pub fn flag(code: &str) -> String {
    let b = code.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_uppercase() || !b[1].is_ascii_uppercase() {
        return "\u{1f4b1}".into();
    }
    if b[0] == b'X' {
        return "\u{1f4b1}".into(); // supranational: no country, no flag
    }
    let ri = |c: u8| char::from_u32(0x1F1E6 + (c - b'A') as u32).unwrap_or('?');
    format!("{}{}", ri(b[0]), ri(b[1]))
}

/// One picker row: flag, code, and the glyph figures will actually wear.
///
/// The symbol in parentheses is the point — `CAD (C$)` tells you what you are
/// about to start reading, where a bare `CAD` leaves you to find out after the
/// whole screen has changed.
pub fn label(code: &str) -> String {
    let d = BaseCurrency { code: code.to_string(), per_usd: 1.0 };
    format!("{}  {}  ({})", flag(code), code, d.symbol().trim())
}

/// Every currency we have a rate for, sorted, with the majors first.
///
/// Not alphabetical throughout: a picker that opens on AED and needs scrolling
/// to reach EUR is a picker that has ranked completeness over use.
pub fn available() -> Vec<String> {
    const MAJORS: [&str; 10] =
        ["USD", "EUR", "GBP", "JPY", "CNY", "CAD", "AUD", "CHF", "INR", "SGD"];
    let t = lock(table());
    let mut rest: Vec<String> = t
        .keys()
        .filter(|k| is_fiat(k) && !MAJORS.contains(&k.as_str()))
        .cloned()
        .collect();
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

    // These exercise `BaseCurrency` directly rather than the process-global.
    //
    // The globals are shared by every test in the binary, including the
    // formatter tests in `view` that assert on a `$` — so a test that switched
    // the global to EUR could fail an unrelated test running beside it, which
    // it did. Behaviour worth testing does not need to be tested through a
    // singleton.
    fn at(code: &str, per_usd: f64) -> BaseCurrency {
        BaseCurrency { code: code.into(), per_usd }
    }

    #[test]
    fn dollars_are_not_converted_at_all() {
        let d = BaseCurrency::default();
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
        assert_eq!(at("CAD", 1.37).symbol(), "C$");
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

#[cfg(test)]
mod picker_tests {
    use super::*;

    /// The bug the picker shipped with: Coinbase's rate table is every asset
    /// they price, so the list ran USD, EUR, GBP … then 1INCH, AAVE, ADA and a
    /// few hundred more.
    #[test]
    fn tokens_are_not_currencies() {
        for asset in ["ETH", "BTC", "1INCH", "AAVE", "ADA", "AERO", "APE", "ARB", "AVAX"] {
            assert!(!is_fiat(asset), "{asset} is an asset, not a currency");
        }
    }

    #[test]
    fn real_currencies_survive_the_filter() {
        for money in ["USD", "EUR", "GBP", "JPY", "NGN", "ZAR", "INR", "BRL", "SGD"] {
            assert!(is_fiat(money), "{money} is a currency");
        }
    }

    /// A ticker that collides with a currency code must not be selectable just
    /// because Coinbase has a rate for it.
    #[test]
    fn selecting_an_asset_is_refused() {
        assert!(!select("ETH"));
        assert!(!select("BTC"));
    }

    #[test]
    fn a_flag_comes_from_the_country_in_the_code() {
        assert_eq!(flag("USD"), "🇺🇸");
        assert_eq!(flag("GBP"), "🇬🇧");
        assert_eq!(flag("JPY"), "🇯🇵");
        assert_eq!(flag("EUR"), "🇪🇺");
    }

    /// Union currencies have no country to take a flag from, and a wrong flag
    /// is worse than none.
    #[test]
    fn supranational_currencies_get_a_neutral_mark() {
        assert_eq!(flag("XOF"), "💱");
        assert_eq!(flag("XDR"), "💱");
    }

    #[test]
    fn a_row_says_what_you_will_be_reading() {
        assert_eq!(label("USD"), "🇺🇸  USD  ($)");
        assert_eq!(label("CAD"), "🇨🇦  CAD  (C$)");
        assert_eq!(label("CHF"), "🇨🇭  CHF  (Fr)");
    }
}
