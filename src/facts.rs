// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! What never changes about a token, fetched once.
//!
//! Discovery used to re-read the symbol, pool fee, socials and total supply of
//! every remembered token on every 1.2-second round — seven RPC calls per
//! token per round for values that cannot change. With 150 remembered tokens
//! that alone was most of the traffic that earned the 429s.
//!
//! This is the other half of the fix that `src/rpc.rs` starts: the transport
//! spreads and paces the calls; this module makes most of them unnecessary.
//! Facts are kept in memory and mirrored to disk, so a restart does not
//! re-interrogate the chain about tokens it already knows.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::lock;

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;

use crate::contracts::{IPonsFactory, IV3Pool, PONS_FACTORY};
use crate::engine;

/// The immutable facts for one token. Fields fill in as they are first
/// needed — `fee` only once a pool is known, `launch_block` only if asked.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    #[serde(default)]
    pub sym: String,
    /// The v3 pool's fee tier. 0 = not fetched yet.
    #[serde(default)]
    pub fee: u32,
    /// Total supply in human units. 0 = not fetched yet.
    #[serde(default)]
    pub supply: f64,
    #[serde(default)]
    pub meta: engine::Meta,
    /// The Pons graduation block. None = never asked, Some(0) = asked and the
    /// chain had no answer (not a Pons launch).
    #[serde(default)]
    pub launch_block: Option<u64>,
    /// ERC-20 decimals. None = never answered; only a successful, sane answer
    /// is cached, so a failed read is retried rather than frozen at a guess.
    #[serde(default)]
    pub decimals: Option<u8>,
}

impl Facts {
    /// Everything the discovery table needs is present.
    fn complete_for(&self, pool: Option<Address>) -> bool {
        !self.sym.is_empty() && self.supply > 0.0 && (pool.is_none() || self.fee > 0)
    }
}

fn path() -> String {
    // Pons is Robinhood Chain's launchpad and these facts only come from
    // there, so one file needs no chain key — same rule as the recents file.
    format!("{}/tokenfacts-pons.json", crate::state_dir())
}

fn store() -> &'static Mutex<HashMap<Address, Facts>> {
    static STORE: OnceLock<Mutex<HashMap<Address, Facts>>> = OnceLock::new();
    STORE.get_or_init(|| {
        let map = std::fs::read_to_string(path())
            .ok()
            .and_then(|text| serde_json::from_str::<HashMap<Address, Facts>>(&text).ok())
            .unwrap_or_default();
        if !map.is_empty() {
            crate::trace(&format!("facts: loaded {} known tokens", map.len()));
        }
        Mutex::new(map)
    })
}

/// Write-behind: at most one disk write per WRITE_EVERY, because facts arrive
/// in bursts (a new round of launches) and the file is small either way.
const WRITE_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

fn save() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    {
        let mut last = lock(&LAST);
        if last.is_some_and(|t| t.elapsed() < WRITE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let text = {
        let map = lock(store());
        serde_json::to_string(&*map)
    };
    if let Ok(text) = text {
        // Temp + rename, so an interrupted write cannot leave a half-file.
        let p = path();
        let tmp = format!("{p}.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

pub fn get(token: Address) -> Option<Facts> {
    lock(store()).get(&token).cloned()
}

fn put(token: Address, facts: Facts) {
    lock(store()).insert(token, facts);
    save();
}

/// The facts for `token`, fetching whatever is missing — ONCE, ever.
///
/// The fetches are the same calls `full_row` used to make every round; the
/// difference is they now happen a single time per token per lifetime of the
/// facts file.
pub async fn ensure<P: Provider>(provider: &P, token: Address, pool: Option<Address>) -> Facts {
    let mut f = get(token).unwrap_or_default();
    if f.complete_for(pool) {
        return f;
    }
    if f.sym.is_empty() {
        f.sym = engine::read_symbol(provider, token).await;
    }
    if f.fee == 0 {
        if let Some(pool) = pool {
            f.fee = IV3Pool::new(pool, provider)
                .fee()
                .call()
                .await
                .map(|v| v._0.to_string().parse().unwrap_or(10_000))
                .unwrap_or(10_000);
        }
    }
    if f.supply <= 0.0 {
        f.supply = crate::contracts::IERC20::new(token, provider)
            .totalSupply()
            .call()
            .await
            .map(|s| s._0.to_string().parse::<f64>().unwrap_or(0.0) / 1e18)
            .unwrap_or(0.0);
    }
    if f.meta.is_empty() {
        f.meta = engine::fetch_token_meta(provider, token).await;
    }
    put(token, f.clone());
    f
}

/// The Pons graduation block for `token`, cached forever after the first ask.
/// None if the log scan errored (so a failed read is retried next time);
/// a token that genuinely never graduated caches Some(0) and is not re-asked.
pub async fn launch_block<P: Provider>(provider: &P, token: Address) -> Option<u64> {
    if let Some(f) = get(token) {
        if let Some(b) = f.launch_block {
            return (b > 0).then_some(b);
        }
    }
    let filter = Filter::new()
        .address(PONS_FACTORY)
        .event_signature(IPonsFactory::TokenLaunched::SIGNATURE_HASH)
        .topic1(token.into_word())
        .from_block(0);
    let logs = tokio::time::timeout(std::time::Duration::from_secs(6), provider.get_logs(&filter))
        .await
        .ok()?
        .ok()?;
    let block = logs.first().and_then(|l| l.block_number).unwrap_or(0);
    let mut f = get(token).unwrap_or_default();
    f.launch_block = Some(block);
    put(token, f);
    (block > 0).then_some(block)
}

/// The token's decimals, cached forever after the first successful read.
///
/// Decimals cannot change, but a FAILED read is not cached: the old behavior
/// (fall back to 18 and retry on the next switch) is kept for errors, because
/// freezing a guessed 18 onto a 6-decimal token would corrupt every amount.
pub async fn decimals<P: Provider>(provider: &P, token: Address) -> Option<u8> {
    if let Some(f) = get(token) {
        if let Some(d) = f.decimals {
            return Some(d);
        }
    }
    let d = crate::contracts::IERC20::new(token, provider).decimals().call().await.ok()?._0;
    // Sanity-bound it: a nonsense value would corrupt every amount.
    if !(1..=36).contains(&d) {
        return None;
    }
    let mut f = get(token).unwrap_or_default();
    f.decimals = Some(d);
    put(token, f);
    Some(d)
}

/// Record a launch block discovery already paid for (the graduation scan sees
/// the block in the event), so `launch_block` never has to ask the chain.
pub fn record_launch(token: Address, block: u64) {
    if block == 0 {
        return;
    }
    let mut f = get(token).unwrap_or_default();
    if f.launch_block != Some(block) {
        f.launch_block = Some(block);
        put(token, f);
    }
}

/// Merge facts a caller already has — a symbol carried in a launch event, a
/// metadata blob fetched from IPFS — without asking the chain for anything.
/// Flaunch launches use this: their PoolCreated event and tokenUri carry what
/// Pons tokens need contract calls for.
pub fn merge(token: Address, apply: impl FnOnce(&mut Facts)) {
    let mut f = get(token).unwrap_or_default();
    apply(&mut f);
    put(token, f);
}

/// Just the total supply, cached forever after the first successful read.
/// For tokens whose OTHER facts don't come from contract calls (Flaunch:
/// symbol and socials arrive with the launch event), `ensure` would spend
/// three doomed calls probing Pons metadata that isn't there.
pub async fn ensure_supply<P: Provider>(provider: &P, token: Address) -> f64 {
    let mut f = get(token).unwrap_or_default();
    if f.supply > 0.0 {
        return f.supply;
    }
    f.supply = crate::contracts::IERC20::new(token, provider)
        .totalSupply()
        .call()
        .await
        .map(|s| s._0.to_string().parse::<f64>().unwrap_or(0.0) / 1e18)
        .unwrap_or(0.0);
    if f.supply > 0.0 {
        put(token, f.clone());
    }
    f.supply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fact_set_is_complete_only_when_everything_needed_is_present() {
        let mut f = Facts { sym: "PONZI".into(), supply: 1e9, fee: 0, ..Default::default() };
        assert!(f.complete_for(None), "no pool means the fee is not needed");
        assert!(!f.complete_for(Some(Address::ZERO)), "a pool means the fee IS needed");
        f.fee = 10_000;
        assert!(f.complete_for(Some(Address::ZERO)));
    }
}
