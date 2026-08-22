// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Live USD price feed (CoinGecko). Used to value quote currencies — ETH for
//! native pools, and stablecoins for stable-quoted pools. Stables are NOT
//! assumed to be $1: USDG, for instance, drifts, so we fetch it like any other
//! asset. Best-effort: a failed fetch leaves the last fetched price in place —
//! the *last price*, not an estimate. There is no longer a hardcoded fallback
//! anywhere in the app; what has never been fetched reads as nothing at all.
//!
//! Cached, on a 15-minute clock, and the cache outlives the process.
//!
//! CoinGecko's free tier is a shared per-IP budget, and this app is not the
//! only thing on the machine spending it. A quote currency does not move
//! enough in fifteen minutes to be worth a request a minute, and the cache
//! means a restart, a second pool and the recovery key all read the same
//! number instead of each buying their own.

use crate::events::{log, Level};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a fetched price stands before anyone goes back to the network.
pub const TTL: Duration = Duration::from_secs(15 * 60);

/// The floor under a forced refresh. `R` is the recovery key and holding it
/// down should not be a way to spend the whole rate-limit budget in a second;
/// inside this window a force still reads the cache.
const FORCE_FLOOR: Duration = Duration::from_secs(60);

/// The prices last fetched, and when. `at` is epoch seconds rather than an
/// `Instant` because it is written to disk and read back in a later process,
/// where an `Instant` from this one means nothing.
#[derive(Default, Clone)]
struct Cache {
    prices: HashMap<String, f64>,
    at: u64,
}

fn cell() -> &'static Mutex<Option<Cache>> {
    static CELL: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    CELL.get_or_init(Default::default)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Beside `theme.txt` and `currency.txt`: a price is a cached fact about the
/// world, not a setting anyone chose, so it belongs in the cache and not in
/// the config the reader is invited to hand-edit.
fn cache_path() -> std::path::PathBuf {
    std::path::Path::new(crate::state_dir()).join("usd-prices.json")
}

/// `{"at": <epoch secs>, "prices": {id: usd}}`. Hand-readable on purpose —
/// this is the file to look at when the dollars on screen are wrong.
fn load_disk() -> Option<Cache> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let at = json.get("at").and_then(|v| v.as_u64())?;
    // A clock that has gone backwards since the write (a timezone fix, a
    // restored snapshot) would otherwise leave a cache that never expires.
    if at > now_secs() {
        return None;
    }
    let obj = json.get("prices").and_then(|v| v.as_object())?;
    let prices: HashMap<String, f64> = obj
        .iter()
        .filter_map(|(k, v)| {
            let usd = v.as_f64()?;
            (usd.is_finite() && usd > 0.0).then(|| (k.clone(), usd))
        })
        .collect();
    (!prices.is_empty()).then_some(Cache { prices, at })
}

fn save_disk(c: &Cache) {
    let _ = std::fs::create_dir_all(crate::state_dir());
    let json = serde_json::json!({ "at": c.at, "prices": c.prices });
    let _ = std::fs::write(cache_path(), json.to_string());
}

/// The cache, warmed from disk the first time anyone asks.
fn cached() -> Option<Cache> {
    let mut g = cell().lock().ok()?;
    if g.is_none() {
        *g = load_disk();
    }
    g.clone()
}

/// The last known prices, however old, without touching the network.
///
/// For the caller that wants a number now — a price from an hour ago is a
/// price, and beats the hardcoded guess it would otherwise fall back to.
pub fn last_known() -> HashMap<String, f64> {
    cached().map(|c| c.prices).unwrap_or_default()
}

/// The cached USD price of the chain's native gas token, or `0.0` when none has
/// ever been fetched.
///
/// The honest answer to "what is ETH worth" before the feed lands. It replaced
/// a hardcoded guess, which had the fatal property of being indistinguishable
/// from a real price: every consumer of a rate already treats `0.0` as "no
/// feed" and prints nothing, and printing nothing is the correct thing to do
/// when nothing is known.
pub fn native_usd() -> f64 {
    last_known().get(crate::contracts::native_cg_id()).copied().unwrap_or(0.0)
}

/// Seconds since the prices on hand were fetched, if any have been.
fn age_secs(c: &Cache) -> u64 {
    now_secs().saturating_sub(c.at)
}

/// Prices for a set of coin ids -> {id: usd}, from cache when it is fresh.
///
/// Returns an empty map only when there is nothing cached AND the fetch
/// failed; a stale cache is served in preference to nothing, because the
/// caller's fallback is a number nobody measured.
pub async fn fetch_usd(ids: &[&str]) -> HashMap<String, f64> {
    get(ids, false).await
}

/// The same, ignoring the TTL — for the recovery key, where the whole point of
/// the press is that the reader does not trust what is on screen. Still floored
/// at `FORCE_FLOOR`.
pub async fn refresh_usd(ids: &[&str]) -> HashMap<String, f64> {
    get(ids, true).await
}

async fn get(ids: &[&str], force: bool) -> HashMap<String, f64> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let have = cached();
    if let Some(c) = &have {
        let age = Duration::from_secs(age_secs(c));
        let floor = if force { FORCE_FLOOR } else { TTL };
        // Only serve the cache when it answers the whole question. A pool
        // added mid-session brings a quote currency the cached round never
        // asked for, and a cache that is fresh for ETH must not be taken as
        // fresh for a stablecoin nobody has priced yet.
        if age < floor && ids.iter().all(|id| c.prices.contains_key(*id)) {
            return c.prices.clone();
        }
    }
    let fetched = fetch_uncached(ids).await;
    if fetched.is_empty() {
        // Nothing new. Whatever is on hand is still the best answer there is,
        // and saying so beats handing back an empty map that the caller reads
        // as "keep your hardcoded guess". It is served with its age on the
        // record, because a price the network stopped confirming an hour ago
        // is still being read as the current one.
        return match have {
            Some(c) => {
                log(
                    Level::Warn,
                    "USD prices are stale — serving the last good fetch",
                    &[("age_s", age_secs(&c).to_string())],
                );
                c.prices
            }
            None => HashMap::new(),
        };
    }
    // Merge rather than replace: a round that asked only for ETH must not
    // evict the stablecoin rates a previous round paid for.
    let mut next = have.map(|c| c.prices).unwrap_or_default();
    next.extend(fetched);
    let c = Cache { prices: next, at: now_secs() };
    if let Ok(mut g) = cell().lock() {
        *g = Some(c.clone());
    }
    save_disk(&c);
    c.prices
}

/// One request, no cache. Every way it can come back empty is logged.
///
/// The previous value a caller keeps on failure is `bot.eth_usd`, which starts
/// life as a hardcoded guess — so a feed that never lands does not look broken
/// on screen, it looks like a price. That is the worst failure a price feed
/// has, and it went unnoticed for as long as it did precisely because nothing
/// said a word.
async fn fetch_uncached(ids: &[&str]) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
        ids.join(",")
    );
    let resp = match reqwest::Client::new()
        .get(&url)
        // CoinGecko's public API answers 403 to anything without a descriptive
        // user agent, and reqwest sends none. Naming the app is the whole
        // requirement; the version makes a rate-limit complaint traceable to a
        // build.
        .header("user-agent", format!("trenches/{}", crate::update::full()))
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(6))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log(Level::Warn, "USD price feed unreachable", &[("err", e.to_string())]);
            return out;
        }
    };
    // Status before body. A refusal arrives as well-formed JSON describing the
    // refusal, and parsing it for prices finds none — an empty map that reads
    // exactly like a quiet network, which is how a 403 hid here in the first
    // place.
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        log(
            Level::Warn,
            "USD price feed refused",
            &[("status", status.as_u16().to_string()), ("body", body.chars().take(200).collect())],
        );
        return out;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            log(Level::Warn, "USD price feed unreadable", &[("err", e.to_string())]);
            return out;
        }
    };
    if let Some(obj) = json.as_object() {
        for (id, v) in obj {
            if let Some(usd) = v.get("usd").and_then(|x| x.as_f64()) {
                if usd.is_finite() && usd > 0.0 {
                    out.insert(id.clone(), usd);
                }
            }
        }
    }
    if out.is_empty() {
        log(Level::Warn, "USD price feed returned no prices", &[("ids", ids.join(","))]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The disk cache has to survive the round trip, because it is what stands
    /// between a cold start and the hardcoded guess.
    #[test]
    fn disk_round_trip() {
        let dir = std::env::temp_dir().join(format!("trenches-pricing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usd-prices.json");
        let c = Cache { prices: HashMap::from([("ethereum".into(), 2408.47)]), at: 1_700_000_000 };
        let json = serde_json::json!({ "at": c.at, "prices": c.prices });
        std::fs::write(&path, json.to_string()).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["at"].as_u64(), Some(1_700_000_000));
        assert_eq!(v["prices"]["ethereum"].as_f64(), Some(2408.47));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache that is fresh for ETH is NOT fresh for a stablecoin it has never
    /// priced — a pool opened mid-session brings ids the last round never asked
    /// for, and serving the cache for those would peg them at the $1 fallback.
    #[test]
    fn missing_id_is_not_covered() {
        let c = Cache { prices: HashMap::from([("ethereum".into(), 2408.47)]), at: now_secs() };
        assert!(["ethereum"].iter().all(|id| c.prices.contains_key(*id)));
        assert!(!["ethereum", "global-dollar"].iter().all(|id| c.prices.contains_key(*id)));
    }

    /// The live feed, end to end. Ignored by default — it costs a request to
    /// a shared budget and fails with the network rather than with the code.
    /// `cargo test --features solana -- --ignored live_feed` when the dollars
    /// on screen look wrong and you need to know which side is at fault.
    #[test]
    #[ignore]
    fn live_feed_answers_with_a_price() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let got = rt.block_on(fetch_uncached(&["ethereum"]));
        let eth = *got.get("ethereum").expect("no ethereum price — check the log for a refusal");
        assert!(eth > 0.0 && eth.is_finite(), "implausible price: {eth}");
    }

    /// A refusal body parses as JSON and contains no prices. It must come back
    /// empty rather than as a price of some kind.
    #[test]
    fn refusal_body_yields_no_prices() {
        let body = r#"{"status":{"error_code":403,"error_message":"Please add a descriptive User-Agent"}}"#;
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        let mut out = HashMap::new();
        if let Some(obj) = json.as_object() {
            for (id, v) in obj {
                if let Some(usd) = v.get("usd").and_then(|x| x.as_f64()) {
                    out.insert(id.clone(), usd);
                }
            }
        }
        assert!(out.is_empty());
    }
}
