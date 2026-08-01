// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Pons-graduation pool discovery. A background task scans the Pons launch
//! factory's `TokenLaunched` events, then fetches each WETH-paired pool's live
//! metrics concurrently and publishes rows AS THEY ARRIVE — so the screen fills
//! in immediately instead of blocking. Ranked by metadata completeness (filled
//! socials/site = more serious launch), then tx/sec (activity).
//!
//! NOTE: this module also holds the DORMANT v4/USDG + Verified-pool discovery
//! (big-fish sweep, `scan_v4_inits`, `verified_rows`, tab/venue consts, etc.),
//! parked for an off-trade-loop redesign — hence the module-wide dead_code allow.
#![allow(dead_code)]

use std::io::Stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::{SolCall, SolEvent};
use crossterm::event::{self, Event, KeyCode};
use ratatui::{prelude::*, widgets::*};

use crate::contracts::{
    IERC20, IFlaunchPositionManager, IPonsFactory, IPonsV2Factory, IStateView, IV3Factory, IV3Pool,
    FLAUNCH_FEE_EST, FLAUNCH_PM, PONS_FACTORY, PONS_V2_FACTORY, POOL_MANAGER, STATE_VIEW,
    V3_FACTORY, WETH,
};
use crate::engine;

type Term = Terminal<CrosstermBackend<Stdout>>;

const SWAP_V3: B256 =
    alloy::primitives::b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");

const SECS_PER_BLOCK: f64 = 0.1; // ~10 blocks/sec on Robinhood Chain
const LAUNCH_WINDOW: u64 = 6_000; // ~10 min of launches to discover
const ACTIVITY_WINDOW: u64 = 200; // ~20 s sample for tx/sec
const MAX_SCAN: usize = 24; // newest launches to track (bounds RPC)
const CONCURRENCY: usize = 12; // in-flight RPC cap
const RPC_TIMEOUT: Duration = Duration::from_secs(6);
const META_FIELDS: u8 = 7; // logo, description, twitter, telegram, discord, website, farcaster
const HOT_TX_PER_SEC: f64 = 1.0; // threshold for the 🔥 (active) marker + top-of-list
const DISPLAY_MAX: usize = 10; // explore list shows only the top N (by rank)
const HOT_MKTCAP_ETH: f64 = 5.0; // 🔥 fire needs cap ≥ this (and < 1 min old); bumped to top
const MIN_MKTCAP_ETH: f64 = 2.0; // Discovery hides pools below this market cap

// Tiered discovery: small-caps must be FRESH, "big fish" show at ANY age.
const FRESH_MAX_SECS: f64 = 120.0; // ≥2 ETH caps only show if this fresh (≤2 min)
// Discovery shows EVERYTHING. These were 16.0 ETH of market cap and 1.5 ETH
// pooled, which is a guess about what someone wants to trade — and we do not
// know that yet. A launch worth looking at is small by definition, so the
// threshold was hiding the thing the screen exists to find. They stay as named
// constants, at zero, because the plan is to let the user set them.
const BIG_MKTCAP_ETH: f64 = 0.0;
const BIG_LOOKBACK: u64 = 36_000; // ~60 min of blocks to sweep for big fish
const BIG_SCAN_CAP: usize = 900; // safety cap on candidates per big-fish sweep
// Still a PRE-filter, not a quality bar: it decides which pools are worth a
// market-cap lookup, and the RPC budget is metered. A pool holding literally
// nothing cannot be traded, so a dust floor costs no discovery and saves calls.
const BIG_POOL_MIN_ETH: f64 = 0.0000001;
const BIG_EVERY: u32 = 8; // run the big-fish sweep every N cycles (~10 s), not every cycle
const BATCH_SIZE: usize = 100; // eth_calls per JSON-RPC batch request
const TAB_NAMES: [&str; 2] = ["Discovery", "Verified"]; // Discovery screen tabs
const TAB_COUNT: usize = TAB_NAMES.len();

/// A discoverable pool: a token plus the venue it trades on. Carries the full
/// `PoolKind` (v3 addr or v4 pool_id) and `Quote` (ETH or a stablecoin like USDG)
/// so v3 memecoins, v4/USDG memecoins, and stock tokens all fit one type.
#[derive(Clone)]
pub struct Grad {
    pub token: Address,
    pub kind: engine::PoolKind, // v3 pool_addr / v4 pool_id
    pub quote: engine::Quote,   // ETH or a stablecoin (USDG)
    pub sym: String,
    pub fee: u32,
    pub launch_block: u64,
    pub meta: engine::Meta, // on-chain socials/metadata (score() = filled fields)
}

impl Grad {
    /// A stable identity for dedup ONLY (v3 address padded into a B256).
    pub fn pool_key(&self) -> B256 {
        match self.kind {
            engine::PoolKind::V3 { pool_addr, .. } => pool_addr.into_word(),
            // A curve has no pool; its own address is the stable identity, and
            // it is unique per launch.
            engine::PoolKind::PonsCurve { curve, .. } => curve.into_word(),
            engine::PoolKind::V4 { pool_id, .. }
            | engine::PoolKind::FlaunchV4 { pool_id, .. } => pool_id,
        }
    }
    /// Human display of the venue id — the 20-byte pool ADDRESS for v3, the
    /// 32-byte pool_id for v4. Never the zero-padded form.
    pub fn pool_display(&self) -> String {
        match self.kind {
            engine::PoolKind::V3 { pool_addr, .. } => format!("{pool_addr:#x}"),
            engine::PoolKind::PonsCurve { curve, .. } => format!("{curve:#x}"),
            engine::PoolKind::V4 { pool_id, .. }
            | engine::PoolKind::FlaunchV4 { pool_id, .. } => format!("{pool_id:#x}"),
        }
    }
    /// True when WETH/quote is token0 (only meaningful for ETH-quoted v3 pools).
    fn weth0(&self) -> bool {
        matches!(self.kind, engine::PoolKind::V3 { weth_is_token0: true, .. })
    }
}

/// A discovery row: a graduation plus its live metrics.
#[derive(Clone)]
struct Row {
    grad: Grad,
    pooled_eth: f64,
    mkt_cap_eth: f64,
    tx_per_sec: f64,
    my_bal: f64,
    head_block: u64,   // current block when measured (for age)
    verified: bool,    // a hand-curated verified pool (shown on the Verified tab)
}

fn uf(u: impl ToString) -> f64 {
    u.to_string().parse::<f64>().unwrap_or(0.0)
}

/// 🔥 fire = active (tx/sec ≥ threshold) AND market cap ≥ threshold, but only
/// within the first minute after graduation (a *fresh* fast pumper — a pool that
/// only crosses 4 ETH later doesn't get the star).
fn is_fire(r: &Row) -> bool {
    r.tx_per_sec >= HOT_TX_PER_SEC && r.mkt_cap_eth >= HOT_MKTCAP_ETH && age_secs(r) <= 60.0
}

fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        // NEWEST first, always. This ranked by market cap, which is the wrong
        // way round for a screen you watch to catch launches: the biggest cap
        // is by definition the one that already ran, and a brand-new mint —
        // the only kind you can still get in front of — started at the bottom
        // of the list. Now that discovery shows every pool it finds, ranking
        // by size would bury the small new ones under everything that already
        // happened.
        //
        // The rest only break ties between pools from the same block.
        b.grad
            .launch_block
            .cmp(&a.grad.launch_block)
            .then(is_fire(b).cmp(&is_fire(a)))
            .then(b.mkt_cap_eth.partial_cmp(&a.mkt_cap_eth).unwrap_or(std::cmp::Ordering::Equal))
            .then(b.grad.meta.score().cmp(&a.grad.meta.score()))
            .then(b.tx_per_sec.partial_cmp(&a.tx_per_sec).unwrap_or(std::cmp::Ordering::Equal))
    });
}

/// The chain's public RPC, kept only as the last-resort URL for the raw batch
/// caller when no balanced pool is live (tests, odd startup orders). All
/// routed traffic — including wide `eth_getLogs`, which Alchemy's free tier
/// caps at 10 blocks — now flows through `src/rpc.rs`, which steers each
/// request to an endpoint that can answer it.
const PUBLIC_RPC: &str = "https://rpc.mainnet.chain.robinhood.com/rpc";

/// Fast first pass: just the (token, pool, block) list from TokenLaunched —
/// one getLogs, no per-token reads, so it returns immediately.
/// Blocks per `getLogs` call.
///
/// This chain's RPC returns a bare "internal server error" on a wide range —
/// 36,000 blocks fails outright — and the failure used to be swallowed, so a
/// broken query looked exactly like a quiet market. Chunking keeps every request
/// inside what the node will answer.
const LOG_CHUNK: u64 = 2_000;

/// How many chunks a NARROW scan may spend. When every endpoint caps
/// `eth_getLogs` at ten blocks, a multi-thousand-block backfill would take
/// hundreds of calls a round — degraded mode instead follows the freshest
/// few chunks and says so, which keeps live discovery alive on a capped pool.
const NARROW_SCAN_CHUNKS: u64 = 8;

/// The span the pool can actually serve, and a floor for `from` once the
/// pool is narrow. Returns (from, chunk).
fn clamp_scan(what: &str, from: u64, to: u64, default_chunk: u64) -> (u64, u64) {
    let chunk = crate::rpc::shared_log_chunk(default_chunk);
    if chunk >= default_chunk {
        return (from, chunk);
    }
    let max_blocks = chunk.saturating_mul(NARROW_SCAN_CHUNKS);
    let lo = from.max(to.saturating_sub(max_blocks.saturating_sub(1)));
    if lo > from {
        crate::trace(&format!(
            "{what} clamped {from}..{to} -> {lo}..{to}: every endpoint caps getLogs at {chunk} blocks"
        ));
    }
    (lo, chunk)
}

/// One pons v2 launch, as announced by the factory.
///
/// The pool does not exist yet. A v2 launch opens on a bonding curve holding
/// the entire supply, and a Uniswap v4 pool is created only when the curve
/// sells out — so `curve` is the only place to read a price from until then,
/// and `phase` on the factory record says which of the two applies.
#[derive(Clone, Copy, Debug)]
pub struct PonsV2Cand {
    pub token: Address,
    pub curve: Address,
    /// What the launch is priced in. Zero means native ETH; anything else is
    /// an approved ERC-20, and then EVERYTHING — price, the graduation target,
    /// creator payouts — is denominated in it rather than in ETH.
    pub pair_token: Address,
    pub threshold: U256,
    pub block: u64,
}

fn decode_pons_v2_log(lg: &alloy::rpc::types::Log) -> Option<PonsV2Cand> {
    let t = lg.topics();
    if t.len() < 4 || t[0] != IPonsV2Factory::TokenLaunched::SIGNATURE_HASH {
        return None;
    }
    let d = lg.data().data.as_ref();
    // pairToken, launchConfigId, graduationThreshold — three words, unindexed.
    if d.len() < 96 {
        return None;
    }
    Some(PonsV2Cand {
        token: Address::from_word(t[1]),
        curve: Address::from_word(t[2]),
        pair_token: Address::from_slice(&d[12..32]),
        threshold: U256::from_be_slice(&d[64..96]),
        block: lg.block_number.unwrap_or(0),
    })
}

fn decode_pons_log(lg: &alloy::rpc::types::Log) -> Option<(Address, Address, u64)> {
    let topics = lg.topics();
    if topics.len() < 4 {
        return None;
    }
    let token = Address::from_word(topics[1]);
    let data = lg.data().data.clone();
    let b = data.as_ref();
    if b.len() < 64 {
        return None;
    }
    let pair = Address::from_slice(&b[12..32]);
    let pool = Address::from_slice(&b[44..64]);
    (pair == WETH).then_some((token, pool, lg.block_number.unwrap_or(0)))
}

/// One chunked getLogs covers EVERY launchpad: both factory addresses, both
/// event topics, dispatched by the emitting address. Scanning them separately
/// doubled the widest, most rate-limited request the app makes, every round.
async fn scan_launchpads<P: Provider>(
    provider: &P,
    from: u64,
    to: u64,
) -> (Vec<(Address, Address, u64)>, Vec<FlCand>) {
    let mut logs = Vec::new();
    let (from, chunk) = clamp_scan("launch scan", from, to, LOG_CHUNK);
    let mut start = from;
    let (mut ok, mut failed) = (0u32, 0u32);
    while start <= to {
        let end = (start + chunk - 1).min(to);
        let filter = Filter::new()
            .address(vec![PONS_FACTORY, FLAUNCH_PM, PONS_V2_FACTORY])
            .event_signature(vec![
                IPonsFactory::TokenLaunched::SIGNATURE_HASH,
                IFlaunchPositionManager::PoolCreated::SIGNATURE_HASH,
                // v2's TokenLaunched is a DIFFERENT event from v1's, so both
                // are matched by topic and told apart by it below.
                IPonsV2Factory::TokenLaunched::SIGNATURE_HASH,
            ])
            .from_block(start)
            .to_block(end);
        match tokio::time::timeout(RPC_TIMEOUT, provider.get_logs(&filter)).await {
            Ok(Ok(l)) => {
                ok += 1;
                logs.extend(l);
            }
            // Say so. An empty trenches screen should never be able to mean
            // "the query failed" without leaving a trace of it.
            Ok(Err(e)) => {
                failed += 1;
                crate::trace(&format!("launch scan {start}..{end} failed: {e}"));
                // Rate limits are the failure people actually hit, and they are
                // the reason the screen looks empty. Say so where it is read.
                let msg = e.to_string();
                let rate_limited = msg.contains("429") || msg.to_lowercase().contains("rate limit");
                crate::events::log(
                    if rate_limited { crate::events::Level::Warn } else { crate::events::Level::Error },
                    if rate_limited { "Discovery rate limited by the RPC" } else { "Discovery scan failed" },
                    &[("blocks", format!("{start}..{end}")), ("reason", msg.chars().take(120).collect())],
                );
            }
            Err(_) => {
                failed += 1;
                crate::trace(&format!("launch scan {start}..{end} timed out"));
            }
        }
        start = end + 1;
    }
    let mut seen = std::collections::HashSet::new();
    let mut pons = Vec::new();
    let mut fl = Vec::new();
    let mut v2 = Vec::new();
    for lg in &logs {
        if lg.address() == PONS_V2_FACTORY {
            if let Some(c) = decode_pons_v2_log(lg) {
                if seen.insert(c.token.into_word()) {
                    v2.push(c);
                }
            }
        } else if lg.address() == PONS_FACTORY {
            if let Some(c) = decode_pons_log(lg) {
                if seen.insert(c.1.into_word()) {
                    pons.push(c);
                }
            }
        } else if let Some(c) = decode_flaunch_log(lg) {
            if seen.insert(c.pool_id) {
                fl.push(c);
            }
        }
    }
    // Not routed anywhere yet — a v2 launch trades on a CURVE until it
    // graduates, which is a venue the engine does not have. Seeing them is the
    // first half; trading them is the second.
    if !v2.is_empty() {
        crate::trace(&format!("launch scan: {} pons v2 launch(es)", v2.len()));
        for c in &v2 {
            crate::trace(&format!(
                "  pons v2 {:#x} curve {:#x} pair {:#x} threshold {}",
                c.token, c.curve, c.pair_token, c.threshold
            ));
        }
    }
    crate::trace(&format!(
        "launch scan {from}..{to}: {ok} chunks ok, {failed} failed, {} pons + {} flaunch",
        pons.len(),
        fl.len()
    ));
    pons.sort_by(|a, b| b.2.cmp(&a.2)); // newest first
    pons.truncate(MAX_SCAN);
    fl.sort_by(|a, b| b.block.cmp(&a.block));
    fl.truncate(MAX_SCAN);
    (pons, fl)
}

/// One row from cached facts + batch-read metrics. No RPC of its own: the
/// facts were fetched once ever, the metrics arrive from the round's single
/// batched `eth_call`, and the swap count from the round's single tape scan.
#[allow(clippy::too_many_arguments)]
fn build_row(
    token: Address,
    pool_addr: Address,
    block: u64,
    f: &crate::facts::Facts,
    sqrt: f64,
    liq: f64,
    my_bal: f64,
    swaps_in_window: usize,
    head: u64,
) -> Row {
    let grad = Grad {
        token,
        kind: engine::PoolKind::V3 { pool_addr, weth_is_token0: WETH < token },
        quote: engine::Quote::Eth,
        sym: f.sym.clone(),
        fee: if f.fee > 0 { f.fee } else { 10_000 },
        launch_block: block,
        meta: f.meta.clone(),
    };
    // Pooled ETH = ETH virtual reserve of the active liquidity (L/√P) — the SAME
    // measure as the tape/telemetry liq_eth, so Discovery reconciles with them.
    let pooled_eth = if sqrt > 0.0 {
        (if grad.weth0() { liq / sqrt } else { liq * sqrt }) / 1e18
    } else {
        0.0
    };
    let p_raw = sqrt * sqrt;
    let tokens_per_eth = if grad.weth0() { p_raw } else if p_raw > 0.0 { 1.0 / p_raw } else { 0.0 };
    let eth_per_token = if tokens_per_eth > 0.0 { 1.0 / tokens_per_eth } else { 0.0 };
    let mkt_cap_eth = f.supply * eth_per_token;
    let secs = ACTIVITY_WINDOW as f64 * SECS_PER_BLOCK;
    let tx_per_sec = if secs > 0.0 { swaps_in_window as f64 / secs } else { 0.0 };

    Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec, my_bal, head_block: head, verified: false }
}

/// Seconds since a row's pool graduated (from the block delta at measure time).
fn age_secs(r: &Row) -> f64 {
    r.head_block.saturating_sub(r.grad.launch_block) as f64 * SECS_PER_BLOCK
}

// ---- Flaunch launch discovery ----
// Flaunch coins launch straight into a v4 pool (flETH-paired, Flaunch hook) on
// the same PoolManager the app already trades, announced by the Flaunch
// PositionManager's PoolCreated event. Unlike Pons there is no bonding phase to
// graduate from: the event IS the launch, and it carries the symbol, name and
// metadata URI inline — so a scan needs no per-token reads at all.

/// v4 PoolManager `Swap` topic0 — for the tx/sec sample of a Flaunch pool.
const SWAP_V4: B256 =
    alloy::primitives::b256!("40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");

/// One Flaunch launch, as decoded from PoolCreated (plus what recents cached).
#[derive(Clone)]
pub struct FlCand {
    pub token: Address,
    pub pool_id: B256,
    pub coin_is_0: bool, // _currencyFlipped: the coin is currency0, flETH currency1
    pub block: u64,
    pub sym: String,
    pub token_uri: String,
    pub flaunch_at: u64, // scheduled go-live (unix secs); 0 = live at creation
}

/// Chunked PoolCreated scan over the Flaunch PositionManager — the same shape
/// (and the same failure reporting) as the Pons `scan_candidates`.
async fn scan_flaunch<L: Provider>(logs: &L, from: u64, to: u64) -> Vec<FlCand> {
    let mut raw = Vec::new();
    let (from, chunk) = clamp_scan("flaunch scan", from, to, LOG_CHUNK);
    let mut start = from;
    let (mut ok, mut failed) = (0u32, 0u32);
    while start <= to {
        let end = (start + chunk - 1).min(to);
        let filter = Filter::new()
            .address(FLAUNCH_PM)
            .event_signature(IFlaunchPositionManager::PoolCreated::SIGNATURE_HASH)
            .from_block(start)
            .to_block(end);
        match tokio::time::timeout(RPC_TIMEOUT, logs.get_logs(&filter)).await {
            Ok(Ok(l)) => {
                ok += 1;
                raw.extend(l);
            }
            Ok(Err(e)) => {
                failed += 1;
                crate::trace(&format!("flaunch scan {start}..{end} failed: {e}"));
                let msg = e.to_string();
                let rate_limited = msg.contains("429") || msg.to_lowercase().contains("rate limit");
                crate::events::log(
                    if rate_limited { crate::events::Level::Warn } else { crate::events::Level::Error },
                    if rate_limited { "Discovery rate limited by the RPC" } else { "Discovery scan failed" },
                    &[("blocks", format!("{start}..{end}")), ("reason", msg.chars().take(120).collect())],
                );
            }
            Err(_) => {
                failed += 1;
                crate::trace(&format!("flaunch scan {start}..{end} timed out"));
            }
        }
        start = end + 1;
    }
    crate::trace(&format!(
        "flaunch scan {from}..{to}: {ok} chunks ok, {failed} failed, {} logs",
        raw.len()
    ));
    let mut seen = std::collections::HashSet::new();
    let mut cands = Vec::new();
    for lg in raw {
        let Some(c) = decode_flaunch_log(&lg) else { continue };
        if seen.insert(c.pool_id) {
            cands.push(c);
        }
    }
    cands.sort_by(|a, b| b.block.cmp(&a.block)); // newest first
    cands.truncate(MAX_SCAN);
    cands
}

fn decode_flaunch_log(lg: &alloy::rpc::types::Log) -> Option<FlCand> {
    // alloy decodes topics AND the dynamic FlaunchParams tuple — never
    // hand-offset a log with dynamic fields.
    let ev = IFlaunchPositionManager::PoolCreated::decode_log(&lg.inner, true).ok()?;
    Some(FlCand {
        token: ev.data._memecoin,
        pool_id: ev.data._poolId,
        coin_is_0: ev.data._currencyFlipped,
        block: lg.block_number.unwrap_or(0),
        // On-chain names are attacker-controlled text — same trim the rest
        // of the table gets from view rendering; length-cap here.
        sym: ev.data._params.symbol.chars().take(12).collect(),
        token_uri: ev.data._params.tokenUri.clone(),
        flaunch_at: u64::try_from(ev.data._params.flaunchAt).unwrap_or(u64::MAX),
    })
}

/// Off-chain metadata cache for Flaunch coins, keyed by token. Filled by a
/// spawned task per coin, so a slow IPFS gateway only delays the socials
/// column — the scan loop never waits on it. An entry (even an empty one)
/// means "fetched or fetching": failures are not retried.
fn fl_meta_cache() -> &'static Mutex<std::collections::HashMap<Address, engine::Meta>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::HashMap<Address, engine::Meta>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn fl_meta(token: Address, token_uri: &str) -> engine::Meta {
    if let Ok(cache) = fl_meta_cache().lock() {
        if let Some(m) = cache.get(&token) {
            return m.clone();
        }
    }
    // A restart keeps what an earlier session fetched: the facts file holds
    // the metadata, so the IPFS gateway is asked once per token, ever.
    if let Some(f) = crate::facts::get(token) {
        if !f.meta.is_empty() {
            if let Ok(mut cache) = fl_meta_cache().lock() {
                cache.insert(token, f.meta.clone());
            }
            return f.meta;
        }
    }
    if !token_uri.trim().is_empty() {
        // Mark as in-flight BEFORE spawning, so the next 1.2s round doesn't
        // spawn a duplicate fetch while this one is still resolving.
        if let Ok(mut cache) = fl_meta_cache().lock() {
            cache.insert(token, engine::Meta::default());
        }
        let uri = token_uri.to_string();
        tokio::spawn(async move {
            let meta = engine::fetch_flaunch_meta(&uri).await;
            if !meta.is_empty() {
                let m2 = meta.clone();
                crate::facts::merge(token, move |f| f.meta = m2);
            }
            if let Ok(mut cache) = fl_meta_cache().lock() {
                cache.insert(token, meta);
            }
        });
    }
    engine::Meta::default()
}

/// One Flaunch row from what the round already paid for: metrics from the
/// batched StateView reads, swap count from the shared tape, supply from the
/// facts cache, symbol/meta from the launch event. No RPC of its own — the
/// same contract `build_row` has for Pons rows.
fn build_fl_row(c: &FlCand, sqrt: f64, liq: f64, my_bal: f64, swaps_in_window: usize, head: u64) -> Row {
    let meta = fl_meta(c.token, &c.token_uri);
    let supply = crate::facts::get(c.token).map(|f| f.supply).unwrap_or(0.0);
    let grad = Grad {
        token: c.token,
        kind: engine::PoolKind::FlaunchV4 { pool_id: c.pool_id, coin_is_0: c.coin_is_0 },
        quote: engine::Quote::Eth,
        sym: c.sym.clone(),
        // The hook's ~1% cut, for quote estimates — the pool's own lpFee is 0.
        fee: FLAUNCH_FEE_EST,
        launch_block: c.block,
        meta,
    };
    // flETH ≈ ETH 1:1, so the flETH-side virtual reserve IS pooled ETH. flETH
    // is token0 unless the launch flipped the pair (`coin_is_0`).
    let fleth0 = !c.coin_is_0;
    let pooled_eth = if sqrt > 0.0 {
        (if fleth0 { liq / sqrt } else { liq * sqrt }) / 1e18
    } else {
        0.0
    };
    let p_raw = sqrt * sqrt;
    let tokens_per_eth = if fleth0 { p_raw } else if p_raw > 0.0 { 1.0 / p_raw } else { 0.0 };
    let eth_per_token = if tokens_per_eth > 0.0 { 1.0 / tokens_per_eth } else { 0.0 };
    let mkt_cap_eth = supply * eth_per_token;
    let secs = ACTIVITY_WINDOW as f64 * SECS_PER_BLOCK;
    let tx_per_sec = if secs > 0.0 { swaps_in_window as f64 / secs } else { 0.0 };

    Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec, my_bal, head_block: head, verified: false }
}

/// A token's Flaunch pool, if the token was launched there — the Flaunch
/// equivalent of `fetch_launch_block`, for coins that arrive by CA, holdings or
/// the pool menu rather than through discovery.
pub struct FlaunchPool {
    pub pool_id: B256,
    pub coin_is_0: bool,
    pub launch_block: u64,
    pub token_uri: String,
}

pub async fn fetch_flaunch_pool<P: Provider>(provider: &P, token: Address) -> Option<FlaunchPool> {
    use alloy::sol_types::SolValue;
    let pm = IFlaunchPositionManager::new(FLAUNCH_PM, provider);
    let key = tokio::time::timeout(RPC_TIMEOUT, pm.poolKey(token).call())
        .await
        .ok()?
        .ok()?
        .key;
    // The documented empty-answer marker for a token Flaunch never launched.
    if key.tickSpacing.as_i32() == 0 {
        return None;
    }
    let pool_id: B256 = alloy::primitives::keccak256(key.abi_encode());
    fetch_flaunch_by_id(provider, pool_id).await.map(|(_, fl)| fl)
}

/// The reverse lookup: a pasted 32-byte Flaunch POOL ID back to its coin.
/// Flaunch listings show the pool id as often as the coin's address, and the
/// launch event is indexed by it — one topic-filtered getLogs answers with
/// the coin, its side, the launch block and the metadata URI. `None` for an
/// id Flaunch never launched (including plain v4 pool ids, which have no
/// PoolCreated log to find).
pub async fn fetch_flaunch_by_id<P: Provider>(
    provider: &P,
    pool_id: B256,
) -> Option<(Address, FlaunchPool)> {
    // The balanced transport steers this wide scan to an endpoint that can
    // answer it.
    let filter = Filter::new()
        .address(FLAUNCH_PM)
        .event_signature(IFlaunchPositionManager::PoolCreated::SIGNATURE_HASH)
        .topic1(pool_id)
        .from_block(0);
    let logs = tokio::time::timeout(RPC_TIMEOUT, provider.get_logs(&filter)).await.ok()?.ok()?;
    let lg = logs.first()?;
    let ev = IFlaunchPositionManager::PoolCreated::decode_log(&lg.inner, true).ok()?;
    Some((
        ev.data._memecoin,
        FlaunchPool {
            pool_id,
            coin_is_0: ev.data._currencyFlipped,
            launch_block: lg.block_number.unwrap_or(0),
            token_uri: ev.data._params.tokenUri.clone(),
        },
    ))
}

// ---- Big-fish sweep: find larger-cap graduations at ANY age ----
// The trade loop only sees the last ~10 min of launches. Bigger tokens graduate
// and keep trading for an hour+; to surface them without a per-refresh scan of
// hundreds of pools, we BATCH the metric reads (one JSON-RPC request = many
// eth_calls) and hit a SEPARATE endpoint (Alchemy) so the trade RPC is untouched.

const SEL_TOTALSUPPLY: [u8; 4] = [0x18, 0x16, 0x0d, 0xdd];
const SEL_SLOT0: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd];
const SEL_SYMBOL: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];

/// balanceOf(account) calldata.
fn balanceof_data(account: Address) -> Vec<u8> {
    let mut d = vec![0x70, 0xa0, 0x82, 0x31];
    d.extend_from_slice(&[0u8; 12]);
    d.extend_from_slice(account.as_slice());
    d
}

/// First 32-byte word of a return blob as U256 (0 if short).
fn u256_of(d: &[u8]) -> U256 {
    if d.len() < 32 { U256::ZERO } else { U256::from_be_slice(&d[..32]) }
}

/// Decode an ABI dynamic string return (symbol()).
fn parse_string(d: &[u8]) -> String {
    if d.len() < 64 { return String::new(); }
    let n = U256::from_be_slice(&d[32..64]);
    let len = if n > U256::from(256u64) { 0 } else { n.to::<usize>() }.min(d.len() - 64);
    String::from_utf8_lossy(&d[64..64 + len]).trim_matches('\0').to_string()
}

/// Batched `eth_call`: one HTTP request carries up to BATCH_SIZE calls. Results
/// returned by index (None on error). Hits `url` (the Discovery endpoint).
/// Outcomes are reported back to the balanced pool (when one is live) so a
/// rate-limited batch endpoint gets benched exactly like a rotated one.
async fn batch_call(client: &reqwest::Client, url: &str, calls: &[(Address, Vec<u8>)]) -> Vec<Option<Vec<u8>>> {
    let mut out: Vec<Option<Vec<u8>>> = vec![None; calls.len()];
    let mut i = 0;
    while i < calls.len() {
        let end = (i + BATCH_SIZE).min(calls.len());
        let body: Vec<serde_json::Value> = (i..end)
            .map(|k| {
                let (to, data) = &calls[k];
                serde_json::json!({
                    "jsonrpc": "2.0", "id": k, "method": "eth_call",
                    "params": [{"to": to.to_string(), "data": format!("0x{}", alloy::hex::encode(data))}, "latest"]
                })
            })
            .collect();
        let t0 = std::time::Instant::now();
        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if let Some(b) = crate::rpc::shared() {
                    b.report_raw(url, status.is_success(), status.as_u16() == 429, t0.elapsed());
                }
                if let Ok(arr) = resp.json::<Vec<serde_json::Value>>().await {
                    for item in arr {
                        let id = item.get("id").and_then(|v| v.as_u64());
                        let res = item.get("result").and_then(|v| v.as_str());
                        if let (Some(id), Some(res)) = (id, res) {
                            if let Ok(b) = alloy::hex::decode(res.trim_start_matches("0x")) {
                                if (id as usize) < out.len() { out[id as usize] = Some(b); }
                            }
                        }
                    }
                }
            }
            Err(_) => {
                if let Some(b) = crate::rpc::shared() {
                    b.report_raw(url, false, false, t0.elapsed());
                }
            }
        }
        i = end;
        if end < calls.len() {
            tokio::time::sleep(Duration::from_millis(200)).await; // spread CU under free-tier limit
        }
    }
    out
}

/// All WETH-paired graduations in [from,to], deduped, newest-first, capped.
/// getLogs is chunked (public RPC caps very wide ranges).
async fn scan_all<L: Provider>(logs: &L, from: u64, to: u64) -> Vec<(Address, Address, u64)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let (from, chunk) = clamp_scan("big-fish scan", from, to, 6_000);
    let mut lo = from;
    while lo <= to {
        let hi = (lo + chunk).min(to);
        let filter = Filter::new()
            .address(PONS_FACTORY)
            .event_signature(IPonsFactory::TokenLaunched::SIGNATURE_HASH)
            .from_block(lo)
            .to_block(hi);
        if let Ok(Ok(lg)) = tokio::time::timeout(RPC_TIMEOUT, logs.get_logs(&filter)).await {
            for l in lg {
                let tp = l.topics();
                if tp.len() < 4 { continue; }
                let token = Address::from_word(tp[1]);
                let data = l.data().data.clone();
                let b = data.as_ref();
                if b.len() < 64 { continue; }
                if Address::from_slice(&b[12..32]) != WETH { continue; }
                let pool = Address::from_slice(&b[44..64]);
                if !seen.insert(pool) { continue; }
                out.push((token, pool, l.block_number.unwrap_or(0)));
            }
        }
        if hi == to { break; }
        lo = hi + 1;
    }
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out.truncate(BIG_SCAN_CAP);
    out
}

/// The big-fish sweep: launches over a wide window → batched pooled-ETH
/// pre-filter → batched supply/price/symbol → Rows with mkt cap ≥ BIG_MKTCAP_ETH.
async fn big_fish_rows<L: Provider>(
    logs: &L,
    client: &reqwest::Client,
    url: &str,
    from: u64,
    to: u64,
    head: u64,
) -> Vec<Row> {
    let cands = scan_all(logs, from, to).await;
    if cands.is_empty() { return Vec::new(); }
    // Cheap pre-filter: pooled WETH held by the pool (one batched call each).
    let bal_calls: Vec<(Address, Vec<u8>)> =
        cands.iter().map(|(_, p, _)| (WETH, balanceof_data(*p))).collect();
    let bals = batch_call(client, url, &bal_calls).await;
    let survivors: Vec<(Address, Address, u64, f64)> = cands
        .iter()
        .zip(bals.iter())
        .filter_map(|((t, p, b), r)| {
            let eth = r.as_ref().map(|d| uf(u256_of(d)) / 1e18).unwrap_or(0.0);
            (eth >= BIG_POOL_MIN_ETH).then_some((*t, *p, *b, eth))
        })
        .collect();
    if survivors.is_empty() { return Vec::new(); }
    // Metrics for survivors: totalSupply, slot0 (sqrtPrice), symbol (3 batched each).
    let mut calls = Vec::with_capacity(survivors.len() * 3);
    for (t, p, _, _) in &survivors {
        calls.push((*t, SEL_TOTALSUPPLY.to_vec()));
        calls.push((*p, SEL_SLOT0.to_vec()));
        calls.push((*t, SEL_SYMBOL.to_vec()));
    }
    let res = batch_call(client, url, &calls).await;
    let mut rows = Vec::new();
    for (i, (t, p, b, eth)) in survivors.iter().enumerate() {
        let supply = res.get(i * 3).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 1e18).unwrap_or(0.0);
        let sqrt = res.get(i * 3 + 1).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 2f64.powi(96)).unwrap_or(0.0);
        let sym = res.get(i * 3 + 2).and_then(|o| o.as_ref()).map(|d| parse_string(d)).unwrap_or_default();
        let weth0 = WETH < *t;
        let p_raw = sqrt * sqrt;
        let tokens_per_eth = if weth0 { p_raw } else if p_raw > 0.0 { 1.0 / p_raw } else { 0.0 };
        let eth_per_token = if tokens_per_eth > 0.0 { 1.0 / tokens_per_eth } else { 0.0 };
        let mkt_cap_eth = supply * eth_per_token;
        if mkt_cap_eth < BIG_MKTCAP_ETH { continue; }
        let sym = if sym.is_empty() { format!("0x{}", &alloy::hex::encode(t.as_slice())[..6]) } else { sym };
        let grad = Grad {
            token: *t,
            kind: engine::PoolKind::V3 { pool_addr: *p, weth_is_token0: weth0 },
            quote: engine::Quote::Eth,
            sym,
            fee: 10_000,
            launch_block: *b,
            meta: engine::Meta::default(),
        };
        rows.push(Row { grad, pooled_eth: *eth, mkt_cap_eth, tx_per_sec: 0.0, my_bal: 0.0, head_block: head, verified: false });
    }
    rows
}

// ---- v4 pool scanner (USDG + WETH pools via PoolManager Initialize) ----
// v4 pools announce via PoolManager.Initialize; the pool_id is the event's first
// topic. Metrics come from StateView (getSlot0 + getLiquidity), batched. Only
// WETH- or USDG-quoted pools (what the engine can price/trade) are kept.

const USDG: Address = alloy::primitives::address!("5fc5360d0400a0fd4f2af552add042d716f1d168");
const INIT_TOPIC: B256 =
    alloy::primitives::b256!("dd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438");
const SEL_GETSLOT0: [u8; 4] = [0xc8, 0x15, 0x64, 0x1c];
const SEL_GETLIQ: [u8; 4] = [0xfa, 0x67, 0x93, 0xd5];

fn poolid_call(sel: [u8; 4], id: B256) -> Vec<u8> {
    let mut d = sel.to_vec();
    d.extend_from_slice(id.as_slice());
    d
}

/// Scan v4 Initialize events → (pool_id, token, quote, tick_spacing, fee, block).
/// Only WETH- or USDG-quoted pools are kept (the venues the engine can trade).
async fn scan_v4_inits<L: Provider>(
    logs: &L,
    from: u64,
    to: u64,
) -> Vec<(B256, Address, engine::Quote, i32, u32, u64)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let (from, chunk) = clamp_scan("v4-init scan", from, to, 6_000);
    let mut lo = from;
    while lo <= to {
        let hi = (lo + chunk).min(to);
        let filter = Filter::new()
            .address(POOL_MANAGER)
            .event_signature(INIT_TOPIC)
            .from_block(lo)
            .to_block(hi);
        if let Ok(Ok(lg)) = tokio::time::timeout(RPC_TIMEOUT, logs.get_logs(&filter)).await {
            for l in lg {
                let tp = l.topics();
                if tp.len() < 4 {
                    continue;
                }
                let id = tp[1];
                let c0 = Address::from_word(tp[2]);
                let c1 = Address::from_word(tp[3]);
                let (token, quote) = if c0 == WETH {
                    (c1, engine::Quote::Eth)
                } else if c1 == WETH {
                    (c0, engine::Quote::Eth)
                } else if c0 == USDG {
                    (c1, engine::Quote::Stable { token: USDG, decimals: 6 })
                } else if c1 == USDG {
                    (c0, engine::Quote::Stable { token: USDG, decimals: 6 })
                } else {
                    continue; // not a WETH/USDG pool → engine can't price it
                };
                // data: fee(uint24) | tickSpacing(int24) | hooks | sqrtPriceX96 | tick
                let data = l.data().data.clone();
                let b = data.as_ref();
                if b.len() < 64 {
                    continue;
                }
                let fee = ((b[29] as u32) << 16) | ((b[30] as u32) << 8) | (b[31] as u32);
                let ts_raw = ((b[61] as i32) << 16) | ((b[62] as i32) << 8) | (b[63] as i32);
                let tick_spacing = if ts_raw >= 0x0080_0000 { ts_raw - 0x0100_0000 } else { ts_raw };
                if !seen.insert(id) {
                    continue;
                }
                out.push((id, token, quote, tick_spacing, fee, l.block_number.unwrap_or(0)));
            }
        }
        if hi == to {
            break;
        }
        lo = hi + 1;
    }
    out.truncate(BIG_SCAN_CAP);
    out
}

/// v4 big fish: Initialize sweep → batched StateView metrics → Rows (mkt cap in
/// ETH-equivalent so v4/USDG pools rank honestly next to v3/ETH pools).
async fn v4_rows<L: Provider>(
    logs: &L,
    client: &reqwest::Client,
    url: &str,
    from: u64,
    to: u64,
    head: u64,
    eth_usd: f64,
) -> Vec<Row> {
    let cands = scan_v4_inits(logs, from, to).await;
    if cands.is_empty() || eth_usd <= 0.0 {
        return Vec::new();
    }
    // 4 batched calls each: getSlot0(id), getLiquidity(id), totalSupply, symbol.
    let mut calls = Vec::with_capacity(cands.len() * 4);
    for (id, token, _, _, _, _) in &cands {
        calls.push((STATE_VIEW, poolid_call(SEL_GETSLOT0, *id)));
        calls.push((STATE_VIEW, poolid_call(SEL_GETLIQ, *id)));
        calls.push((*token, SEL_TOTALSUPPLY.to_vec()));
        calls.push((*token, SEL_SYMBOL.to_vec()));
    }
    let res = batch_call(client, url, &calls).await;
    let mut rows = Vec::new();
    for (i, (id, token, quote, tick_spacing, fee, block)) in cands.iter().enumerate() {
        let sqrt = res.get(i * 4).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 2f64.powi(96)).unwrap_or(0.0);
        let liq = res.get(i * 4 + 1).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d))).unwrap_or(0.0);
        let supply = res.get(i * 4 + 2).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 1e18).unwrap_or(0.0);
        let sym = res.get(i * 4 + 3).and_then(|o| o.as_ref()).map(|d| parse_string(d)).unwrap_or_default();
        if sqrt <= 0.0 || liq <= 0.0 || supply <= 0.0 {
            continue;
        }
        // Normalize raw virtual reserves by each side's decimals (USDG is 6-dec).
        let qd = quote.decimals() as i32;
        let a_raw = liq / sqrt; // token0 raw reserve
        let b_raw = liq * sqrt; // token1 raw reserve
        let (pooled_quote, token_reserve) = if *token < quote.addr() {
            (b_raw / 10f64.powi(qd), a_raw / 1e18) // token = token0
        } else {
            (a_raw / 10f64.powi(qd), b_raw / 1e18) // token = token1
        };
        if token_reserve <= 0.0 {
            continue;
        }
        let quote_usd = if quote.is_eth() { eth_usd } else { 1.0 }; // USDG ≈ $1
        let mkt_cap_usd = supply * (pooled_quote / token_reserve) * quote_usd;
        let mkt_cap_eth = mkt_cap_usd / eth_usd;
        let pooled_eth = pooled_quote * quote_usd / eth_usd;
        // Skip empty shells: a set price with ~no liquidity inflates nominal mkt
        // cap but isn't tradeable. Require real pooled value AND a big cap.
        if pooled_eth < BIG_POOL_MIN_ETH || mkt_cap_eth < BIG_MKTCAP_ETH {
            continue;
        }
        let sym = if sym.is_empty() { format!("0x{}", &alloy::hex::encode(token.as_slice())[..6]) } else { sym };
        let grad = Grad {
            token: *token,
            kind: engine::PoolKind::V4 { pool_id: *id, tick_spacing: *tick_spacing },
            quote: *quote,
            sym,
            fee: *fee,
            launch_block: *block,
            meta: engine::Meta::default(),
        };
        rows.push(Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec: 0.0, my_bal: 0.0, head_block: head, verified: false });
    }
    rows
}

/// A pre-resolved verified pool (parsed from config) — priced at runtime, never
/// mined. All are v4 pools pinned by pool_id, quoted in USDG or WETH.
#[derive(Clone)]
pub struct VerifiedPool {
    pub token: Address,
    pub pool_id: B256,
    pub quote: engine::Quote,
    pub tick_spacing: i32,
    pub fee: u32,
    pub sym: String,
}

/// Price the hand-curated verified pools (batched StateView). Always surfaced
/// (curated), so no mkt-cap floor — only skip a pool that reads empty.
async fn verified_rows(
    client: &reqwest::Client,
    url: &str,
    pools: &[VerifiedPool],
    head: u64,
    eth_usd: f64,
) -> Vec<Row> {
    if pools.is_empty() || eth_usd <= 0.0 {
        return Vec::new();
    }
    let mut calls = Vec::with_capacity(pools.len() * 3);
    for p in pools {
        calls.push((STATE_VIEW, poolid_call(SEL_GETSLOT0, p.pool_id)));
        calls.push((STATE_VIEW, poolid_call(SEL_GETLIQ, p.pool_id)));
        calls.push((p.token, SEL_TOTALSUPPLY.to_vec()));
    }
    let res = batch_call(client, url, &calls).await;
    let mut rows = Vec::new();
    for (i, p) in pools.iter().enumerate() {
        let sqrt = res.get(i * 3).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 2f64.powi(96)).unwrap_or(0.0);
        let liq = res.get(i * 3 + 1).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d))).unwrap_or(0.0);
        let supply = res.get(i * 3 + 2).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 1e18).unwrap_or(0.0);
        if sqrt <= 0.0 || liq <= 0.0 {
            continue;
        }
        let qd = p.quote.decimals() as i32;
        let a_raw = liq / sqrt;
        let b_raw = liq * sqrt;
        let (pooled_quote, token_reserve) = if p.token < p.quote.addr() {
            (b_raw / 10f64.powi(qd), a_raw / 1e18)
        } else {
            (a_raw / 10f64.powi(qd), b_raw / 1e18)
        };
        if token_reserve <= 0.0 {
            continue;
        }
        let quote_usd = if p.quote.is_eth() { eth_usd } else { 1.0 };
        let mkt_cap_eth = supply * (pooled_quote / token_reserve) * quote_usd / eth_usd;
        let pooled_eth = pooled_quote * quote_usd / eth_usd;
        let grad = Grad {
            token: p.token,
            kind: engine::PoolKind::V4 { pool_id: p.pool_id, tick_spacing: p.tick_spacing },
            quote: p.quote,
            sym: p.sym.clone(),
            fee: p.fee,
            launch_block: 0,
            meta: engine::Meta::default(),
        };
        rows.push(Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec: 0.0, my_bal: 0.0, head_block: head, verified: true });
    }
    rows
}

/// The discovery rows, alive for the whole process so leaving the screen and
/// coming back does not start from nothing.
fn rows_cache() -> Arc<Mutex<Vec<Row>>> {
    static CACHE: std::sync::OnceLock<Arc<Mutex<Vec<Row>>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default).clone()
}

/// How long a discovered pool stays on the screen without being re-seen.
/// ~10 blocks/sec, so this is about two hours.
const ROW_TTL_BLOCKS: u64 = 72_000;
/// Ceiling on the list, so a busy day cannot grow it without bound.
const ROW_MAX: usize = 400;

/// Fold fresh rows + big fish INTO what is already on screen, then sort.
///
/// This used to replace the list outright, and that is why pools appeared,
/// vanished, and came back minutes later. `fresh` covers only what the last
/// scan window saw — on an endpoint that caps getLogs at ten blocks that is
/// eighty blocks, about EIGHT SECONDS of chain — and the big-fish sweep only
/// runs every few rounds. Anything discovered outside that sliver was dropped
/// on the next round and had to be re-found.
///
/// A pool that existed a moment ago still exists. Keep it, refresh it when it
/// is seen again, and let it go only when it has aged out.
fn merge_publish(shared: &Arc<Mutex<Vec<Row>>>, fresh: &[Row], big: &[Row]) {
    let mut cur = shared.lock().unwrap();
    let head = fresh
        .iter()
        .chain(big.iter())
        .map(|r| r.head_block)
        .chain(cur.iter().map(|r| r.head_block))
        .max()
        .unwrap_or(0);
    for r in fresh.iter().chain(big.iter()) {
        match cur.iter_mut().find(|x| x.grad.pool_key() == r.grad.pool_key()) {
            Some(slot) => *slot = r.clone(), // re-seen: take the newer metrics
            None => cur.push(r.clone()),
        }
    }
    // Age out on the LAUNCH block, not the last time we happened to look at
    // it: a quiet pool is old, not missing.
    cur.retain(|r| head.saturating_sub(r.grad.launch_block) <= ROW_TTL_BLOCKS);
    sort_rows(&mut cur);
    cur.truncate(ROW_MAX);
}

/// Tokens discovery has seen before, newest first.
///
/// The launch window is ten minutes wide, so without this the screen could only
/// ever show what graduated since you opened it — a coin found twenty minutes
/// ago fell off and was gone, including one you had just been looking at. The
/// window still decides what is NEW; this decides what is remembered.
const RECENTS_MAX: usize = 150;

fn recents_path() -> String {
    // Pons is Robinhood Chain's launchpad and this scan only runs there, so one
    // file needs no chain key.
    format!("{}/discovered-pons.json", crate::state_dir())
}

fn load_recents() -> Vec<(Address, Address, u64)> {
    let Ok(text) = std::fs::read_to_string(recents_path()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        crate::trace("recents: cache is not readable JSON, starting empty");
        return Vec::new();
    };
    let mut out = Vec::new();
    for it in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let get = |k: &str| it.get(k).and_then(|x| x.as_str()).unwrap_or("").parse::<Address>();
        if let (Ok(token), Ok(pool)) = (get("token"), get("pool")) {
            out.push((token, pool, it.get("block").and_then(|b| b.as_u64()).unwrap_or(0)));
        }
    }
    crate::trace(&format!("recents: loaded {} remembered tokens", out.len()));
    out
}

fn save_recents(rows: &[(Address, Address, u64)]) {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|(t, p, b)| {
            serde_json::json!({ "token": format!("{t:#x}"), "pool": format!("{p:#x}"), "block": b })
        })
        .collect();
    let Ok(text) = serde_json::to_string_pretty(&arr) else { return };
    // Written to a temporary name and renamed, so an interrupted write cannot
    // leave a half-file that fails to parse on the next open.
    let path = recents_path();
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Remember a token that was picked, so it is on the list next time even if it
/// was never a fresh graduation — a top-tokens choice, or one added by address.
pub fn remember(token: Address, pool: Address, block: u64) {
    let mut rows = load_recents();
    rows.retain(|(t, _, _)| *t != token);
    rows.insert(0, (token, pool, block));
    rows.truncate(RECENTS_MAX);
    save_recents(&rows);
}

/// Flaunch keeps its own recents file — the Pons cache and format are left
/// exactly as they were.
fn flaunch_recents_path() -> String {
    format!("{}/discovered-flaunch.json", crate::state_dir())
}

fn load_flaunch_recents() -> Vec<FlCand> {
    let Ok(text) = std::fs::read_to_string(flaunch_recents_path()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        crate::trace("flaunch recents: cache is not readable JSON, starting empty");
        return Vec::new();
    };
    let mut out = Vec::new();
    for it in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let s = |k: &str| it.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let (Ok(token), Ok(pool_id)) = (s("token").parse::<Address>(), s("pool_id").parse::<B256>()) else {
            continue;
        };
        out.push(FlCand {
            token,
            pool_id,
            coin_is_0: it.get("coin_is_0").and_then(|x| x.as_bool()).unwrap_or(false),
            block: it.get("block").and_then(|b| b.as_u64()).unwrap_or(0),
            sym: s("sym"),
            token_uri: s("token_uri"),
            flaunch_at: it.get("flaunch_at").and_then(|b| b.as_u64()).unwrap_or(0),
        });
    }
    crate::trace(&format!("flaunch recents: loaded {} remembered tokens", out.len()));
    out
}

fn save_flaunch_recents(rows: &[FlCand]) {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|c| {
            serde_json::json!({
                "token": format!("{:#x}", c.token),
                "pool_id": format!("{:#x}", c.pool_id),
                "coin_is_0": c.coin_is_0,
                "block": c.block,
                "sym": c.sym,
                "token_uri": c.token_uri,
                "flaunch_at": c.flaunch_at,
            })
        })
        .collect();
    let Ok(text) = serde_json::to_string_pretty(&arr) else { return };
    let path = flaunch_recents_path();
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// How many of the older (non-fresh) candidates get their metrics refreshed
/// per round, round-robin. The newest MAX_SCAN refresh every round; the rest
/// take turns, so the whole remembered list stays current within ~10s without
/// costing a full sweep every round.
const SWEEP_CHUNK: usize = 24;
/// New tokens whose immutable facts are fetched per round. Facts are 6 calls
/// each, once ever — the bound only smooths the burst when a fresh install
/// meets 150 remembered tokens at once.
const FACTS_PER_ROUND: usize = 4;

/// Background loop: rescan launches, refresh metrics, and publish rows into
/// `shared`. Ends when `stop`.
///
/// A round used to stream `full_row` over every remembered token — 10
/// sequential RPC calls each, most of them re-reading values that cannot
/// change, up to 1,500 calls every 1.2 seconds. A round is now:
///
///   1. one head-block read (micro-cached in the transport),
///   2. one incremental `getLogs` each for new Pons graduations and new
///      Flaunch launches,
///   3. two incremental `getLogs` for the swap tape (tx/sec) — one across
///      ALL v3 candidate pools at once, one across all Flaunch pool ids,
///   4. one batched `eth_call` for slot0/liquidity/balance of the rows that
///      are due a refresh,
///   5. immutable facts for tokens seen for the FIRST time (bounded, cached
///      to disk by src/facts.rs, never asked again). Flaunch tokens carry
///      symbol and metadata in the launch event itself, so only their total
///      supply ever needs a call.
async fn run_discovery<P: Provider + Clone + Send + Sync + 'static>(
    provider: P,
    trader: Address,
    shared: Arc<Mutex<Vec<Row>>>,
    stop: Arc<AtomicBool>,
    disc_url: Option<String>,
    _eth_usd: f64,
    _verified: Vec<VerifiedPool>,
) {
    let client = reqwest::Client::new();
    // How far the launch log has been read. Everything below hangs off this:
    // the window is scanned once, and after that only the blocks that are new.
    let mut scanned_to: Option<u64> = None;
    // Seeded from disk, so the screen has something to show the instant it
    // opens rather than an empty table waiting on a scan.
    let mut known: Vec<(Address, Address, u64)> = load_recents();
    // Flaunch launches ride the same incremental window, in their own list.
    let mut known_fl: Vec<FlCand> = load_flaunch_recents();
    // The rolling swap tape: (block, pool key) per swap, across every
    // candidate pool at once, trimmed to the activity window. Feeds tx/sec.
    // Keyed by B256 so v3 pools (address, widened) and Flaunch pools (pool
    // id) share one tape.
    let mut swaps: std::collections::VecDeque<(u64, B256)> = Default::default();
    let mut swaps_to: Option<u64> = None;
    // Round-robin cursor over the non-fresh tail of the candidate lists.
    let mut sweep_at: usize = 0;
    // When the head-block warning last reached the event log.
    let mut last_head_warn: Option<std::time::Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        // A head read that did not answer is not block zero. It used to fall
        // back to 0, which made the window below `0..0` — a real getLogs call
        // for the genesis block, issued every round, finding nothing and
        // spending the rate limit that the balance and price reads need. Under
        // a 429 that turned one failed request into a storm of them.
        let head = match tokio::time::timeout(RPC_TIMEOUT, provider.get_block_number()).await {
            Ok(Ok(h)) if h > 0 => h,
            _ => {
                crate::trace("discovery: no head block, skipping this round");
                // Once per quiet spell, not once per retry: during a rate-limit
                // rest this fires every 1.5s, and a wall of the same warning
                // buries the log entries worth reading.
                if last_head_warn.is_none_or(|t: std::time::Instant| t.elapsed().as_secs() >= 30) {
                    last_head_warn = Some(std::time::Instant::now());
                    crate::events::warn(
                        "Could not read the latest block number; discovery will keep retrying quietly",
                        &[("retry_in", "1.5s".to_string())],
                    );
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
                continue;
            }
        };
        // Only the blocks that have appeared since the last pass.
        //
        // This used to re-read the whole 6,000-block window every round, three
        // chunked getLogs calls at a time, once a second, forever — asking the
        // node the same question about the same blocks for as long as the
        // screen was open. That is what earned the 429s, and once rate-limited
        // every other read on the same endpoint (price, balance, metadata)
        // queued behind it. A round now costs one small query covering the
        // handful of blocks actually mined since the last one.
        let from = match scanned_to {
            None => head.saturating_sub(LAUNCH_WINDOW),
            Some(t) if head > t => t + 1,
            Some(_) => head + 1, // nothing new; the scan returns at once
        };
        // Only the public endpoint can serve wide log scans, and it rate
        // limits. While it rests, SKIP the scan and leave the cursor alone —
        // asking anyway is what kept it benched forever, and the unmoved
        // cursor means the skipped blocks are scanned the moment it is back.
        let wide_ok = crate::rpc::shared().is_none_or(|b| b.wide_ready());
        if from <= head && wide_ok {
            let (pons, fl) = scan_launchpads(&provider, from, head).await;
            for c in pons {
                crate::facts::record_launch(c.0, c.2);
                if !known.iter().any(|(t, _, _)| *t == c.0) {
                    known.push(c);
                }
            }
            for c in fl {
                if !known_fl.iter().any(|k| k.token == c.token) {
                    known_fl.push(c);
                }
            }
            scanned_to = Some(head);
        } else if !wide_ok {
            crate::trace("discovery: wide-logs endpoint resting, scan deferred");
        } else {
            scanned_to = Some(head);
        }
        // Newest first, and bounded — but NOT aged out by the launch window.
        // Falling off the window means "no longer a new graduation", not "no
        // longer worth showing"; dropping it was what made a coin you were
        // looking at vanish while you looked at it.
        known.sort_by(|a, b| b.2.cmp(&a.2));
        known.truncate(RECENTS_MAX);
        save_recents(&known);
        known_fl.sort_by(|a, b| b.block.cmp(&a.block));
        known_fl.truncate(RECENTS_MAX);
        save_flaunch_recents(&known_fl);

        // The swap tape, incrementally: one getLogs over every v3 candidate
        // pool at once, and one over the PoolManager filtered to every Flaunch
        // pool id. This replaces a per-pool history scan that asked the same
        // blocks about the same pools every round.
        let pools: Vec<Address> = known.iter().map(|(_, p, _)| *p).collect();
        let fl_ids: Vec<B256> = known_fl.iter().map(|c| c.pool_id).collect();
        let cutoff = head.saturating_sub(ACTIVITY_WINDOW);
        let sfrom = match swaps_to {
            None => cutoff,
            Some(t) => (t + 1).max(cutoff),
        };
        // Same deferral as the launch scan: a tape range that outgrew the
        // narrow cap needs the wide endpoint, and hammering it mid-rest keeps
        // it benched. The cursor waits, so nothing is missed.
        let tape_wide = head.saturating_sub(sfrom) >= 10;
        if sfrom <= head && !(pools.is_empty() && fl_ids.is_empty()) && (wide_ok || !tape_wide) {
            // The cursor only advances when EVERY tape read succeeded, so a
            // failed fetch never skips those blocks' swaps.
            let mut tape_ok = true;
            if !pools.is_empty() {
                let filter = Filter::new()
                    .address(pools)
                    .event_signature(SWAP_V3)
                    .from_block(sfrom)
                    .to_block(head);
                match tokio::time::timeout(RPC_TIMEOUT, provider.get_logs(&filter)).await {
                    Ok(Ok(lgs)) => {
                        for l in lgs {
                            if let Some(b) = l.block_number {
                                swaps.push_back((b, l.address().into_word()));
                            }
                        }
                    }
                    Ok(Err(e)) => { tape_ok = false; crate::trace(&format!("discovery: swap tape failed: {e}")); }
                    Err(_) => { tape_ok = false; crate::trace("discovery: swap tape timed out"); }
                }
            }
            if !fl_ids.is_empty() {
                let filter = Filter::new()
                    .address(POOL_MANAGER)
                    .event_signature(SWAP_V4)
                    .topic1(fl_ids)
                    .from_block(sfrom)
                    .to_block(head);
                match tokio::time::timeout(RPC_TIMEOUT, provider.get_logs(&filter)).await {
                    Ok(Ok(lgs)) => {
                        for l in lgs {
                            if let (Some(b), Some(id)) = (l.block_number, l.topics().get(1)) {
                                swaps.push_back((b, *id));
                            }
                        }
                    }
                    Ok(Err(e)) => { tape_ok = false; crate::trace(&format!("discovery: flaunch tape failed: {e}")); }
                    Err(_) => { tape_ok = false; crate::trace("discovery: flaunch tape timed out"); }
                }
            }
            if tape_ok {
                swaps_to = Some(head);
            }
        }
        while swaps.front().is_some_and(|(b, _)| *b < cutoff) {
            swaps.pop_front();
        }

        // Immutable facts for tokens met for the first time — once, ever.
        // Flaunch launches carry symbol and metadata in the event itself, so
        // only their total supply is ever asked of the chain (socials come
        // from IPFS off-loop, via fl_meta).
        let mut fetched = 0usize;
        for (t, p, _) in known.iter() {
            if fetched >= FACTS_PER_ROUND || stop.load(Ordering::Relaxed) {
                break;
            }
            let have = crate::facts::get(*t).is_some_and(|f| !f.sym.is_empty() && f.supply > 0.0);
            if !have {
                crate::facts::ensure(&provider, *t, Some(*p)).await;
                fetched += 1;
            }
        }
        for c in known_fl.iter() {
            if fetched >= FACTS_PER_ROUND || stop.load(Ordering::Relaxed) {
                break;
            }
            let have = crate::facts::get(c.token).is_some_and(|f| f.supply > 0.0);
            if !have {
                crate::facts::ensure_supply(&provider, c.token).await;
                let (sym, block) = (c.sym.clone(), c.block);
                crate::facts::merge(c.token, move |f| {
                    if f.sym.is_empty() {
                        f.sym = sym;
                    }
                    if f.launch_block.is_none() && block > 0 {
                        f.launch_block = Some(block);
                    }
                });
                fetched += 1;
            }
        }

        // Who is due a metrics refresh: every fresh candidate from BOTH
        // feeds, plus the next round-robin slice of the combined older tail.
        // A launch scheduled for later (flaunchAt in the future) would show a
        // price but refuse every swap — it renders as a zero-metric row (kept
        // under the market-cap floor) and spends no batch slot until it goes
        // live; the partition re-evaluates every round, so it appears then.
        #[derive(Clone)]
        enum Due {
            Pons(Address, Address, u64),
            Fl(FlCand),
        }
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (fl_live, fl_pending): (Vec<FlCand>, Vec<FlCand>) =
            known_fl.iter().cloned().partition(|c| c.flaunch_at <= now_secs);
        let mut due: Vec<Due> =
            known.iter().take(MAX_SCAN).map(|(t, p, b)| Due::Pons(*t, *p, *b)).collect();
        due.extend(fl_live.iter().take(MAX_SCAN).cloned().map(Due::Fl));
        let tail: Vec<Due> = known
            .iter()
            .skip(MAX_SCAN)
            .map(|(t, p, b)| Due::Pons(*t, *p, *b))
            .chain(fl_live.iter().skip(MAX_SCAN).cloned().map(Due::Fl))
            .collect();
        if !tail.is_empty() {
            for k in 0..SWEEP_CHUNK.min(tail.len()) {
                due.push(tail[(sweep_at + k) % tail.len()].clone());
            }
            sweep_at = (sweep_at + SWEEP_CHUNK) % tail.len();
        }
        // Skip rows whose facts have not arrived yet — a row with no symbol
        // and no supply renders as garbage and re-sorts to nowhere. (Flaunch
        // symbols come from the event; only the supply gates them.)
        due.retain(|d| match d {
            Due::Pons(t, _, _) => {
                crate::facts::get(*t).is_some_and(|f| !f.sym.is_empty() && f.supply > 0.0)
            }
            Due::Fl(c) => crate::facts::get(c.token).is_some_and(|f| f.supply > 0.0),
        });

        // One batched eth_call for the whole refresh set: slot0 + liquidity
        // per pool (v3 pools answer directly, Flaunch pools via StateView by
        // pool id), plus my balance per token when an account is loaded.
        use alloy::sol_types::SolCall;
        let with_bal = trader != Address::ZERO;
        let per = if with_bal { 3 } else { 2 };
        let mut calls: Vec<(Address, Vec<u8>)> = Vec::with_capacity(due.len() * per);
        for d in &due {
            match d {
                Due::Pons(t, p, _) => {
                    calls.push((*p, IV3Pool::slot0Call {}.abi_encode()));
                    calls.push((*p, IV3Pool::liquidityCall {}.abi_encode()));
                    if with_bal {
                        calls.push((*t, IERC20::balanceOfCall { owner: trader }.abi_encode()));
                    }
                }
                Due::Fl(c) => {
                    calls.push((STATE_VIEW, IStateView::getSlot0Call { poolId: c.pool_id }.abi_encode()));
                    calls.push((STATE_VIEW, IStateView::getLiquidityCall { poolId: c.pool_id }.abi_encode()));
                    if with_bal {
                        calls.push((c.token, IERC20::balanceOfCall { owner: trader }.abi_encode()));
                    }
                }
            }
        }
        // An honest pick: when every endpoint is resting, SKIP this round's
        // metrics instead of spending the recovering quota — rows keep their
        // previous values, which beats freshly-fetched refusals.
        let url = match crate::rpc::shared() {
            Some(b) => match b.ready_url(false) {
                Some(u) => u,
                None => {
                    crate::trace("discovery: all endpoints resting, metrics deferred");
                    tokio::time::sleep(Duration::from_millis(2000)).await;
                    continue;
                }
            },
            None => disc_url
                .clone()
                .filter(|u| !u.trim().is_empty() && !u.contains("YOUR_"))
                .unwrap_or_else(|| PUBLIC_RPC.to_string()),
        };
        let res = batch_call(&client, &url, &calls).await;

        let mut swap_count: std::collections::HashMap<B256, usize> = Default::default();
        for (_, key) in &swaps {
            *swap_count.entry(*key).or_default() += 1;
        }
        // Rows survive between rounds, keyed by token, and keep their place:
        // a refreshed row replaces its previous self, everything else stays
        // untouched, and the ranking is applied once per round.
        for (i, d) in due.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let sqrt = res
                .get(i * per)
                .and_then(|o| o.as_ref())
                .map(|d| uf(u256_of(d)) / 2f64.powi(96))
                .unwrap_or(0.0);
            let liq = res.get(i * per + 1).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d))).unwrap_or(0.0);
            let my_bal = if with_bal {
                res.get(i * per + 2).and_then(|o| o.as_ref()).map(|d| uf(u256_of(d)) / 1e18).unwrap_or(0.0)
            } else {
                0.0
            };
            // A batch that failed outright answers None for everything; keep
            // the previous row rather than publishing zeros over it.
            if sqrt <= 0.0 && liq <= 0.0 {
                continue;
            }
            let row = match d {
                Due::Pons(t, p, b) => {
                    let Some(f) = crate::facts::get(*t) else { continue };
                    let n = swap_count.get(&p.into_word()).copied().unwrap_or(0);
                    build_row(*t, *p, *b, &f, sqrt, liq, my_bal, n, head)
                }
                Due::Fl(c) => {
                    let n = swap_count.get(&c.pool_id).copied().unwrap_or(0);
                    build_fl_row(c, sqrt, liq, my_bal, n, head)
                }
            };
            let mut cur = shared.lock().unwrap();
            match cur.iter_mut().find(|r| r.grad.token == row.grad.token) {
                Some(slot) => *slot = row, // refresh, same position
                None => cur.push(row),     // genuinely new, at the end
            }
        }
        // Scheduled launches exist as zero-metric rows, so the moment their
        // time arrives the next round promotes them in place.
        for c in &fl_pending {
            let row = build_fl_row(c, 0.0, 0.0, 0.0, 0, head);
            let mut cur = shared.lock().unwrap();
            match cur.iter_mut().find(|r| r.grad.token == row.grad.token) {
                Some(slot) => *slot = row,
                None => cur.push(row),
            }
        }
        // One re-rank per round. The order is then stable until the next one.
        {
            let mut cur = shared.lock().unwrap();
            sort_rows(&mut cur);
        }
        tokio::time::sleep(Duration::from_millis(2000)).await;
    }
}

/// The live discovery screen. Returns the chosen graduation to enter, or None
/// on Esc/q. Fetching runs in a background task; the screen just renders the
/// shared rows (which fill in as they arrive) and handles keys.
pub async fn screen<P: Provider + Clone + Send + Sync + 'static>(
    term: &mut Term,
    provider: &P,
    trader: Address,
    disc_url: Option<String>,
    eth_usd: f64,
    verified: Vec<VerifiedPool>,
) -> eyre::Result<Option<Grad>> {
    // This screen owns the terminal now: take down any image the last one left.
    // Clearing also marks every placement stale, so the dashboard redraws its
    // logo when we return — no screen has to know about any other.
    crate::ui::image::clear();
    // The rows live in a process-wide cache, NOT on this screen's stack frame.
    // They used to die with the screen: leave to trade the coin you found,
    // come back, and everything you had discovered was gone until it was
    // re-fetched from scratch. Now returning shows the last list instantly
    // and the background task refreshes it in place.
    let shared: Arc<Mutex<Vec<Row>>> = rows_cache();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn(run_discovery(
        provider.clone(),
        trader,
        shared.clone(),
        stop.clone(),
        disc_url,
        eth_usd,
        verified,
    ));

    // Index-based selection: the cursor stays at the TOP by default (index 0 =
    // the best-ranked pool) for quick Enter, rather than following a pick down.
    let mut sel: usize = 0;
    let mut state = TableState::default();
    // Advances once per poll (~120ms), which is about the right speed to read
    // as motion rather than a flicker.
    let mut spinner: usize = 0;
    let mut msel = crate::ui::mouse::Selection::default();
    let mut copy_armed = false;
    let result: eyre::Result<Option<Grad>> = loop {
        let rows = {
            let mut r = shared.lock().unwrap().clone();
            r.retain(|row| row.mkt_cap_eth >= MIN_MKTCAP_ETH); // hide anything under the min cap
            r.truncate(DISPLAY_MAX); // keep the explore list short — top N by rank
            r
        };
        if rows.is_empty() {
            draw_scan_status(term, "Discovering token launches…", "Esc to go back", spinner)?;
            spinner = spinner.wrapping_add(1);
        } else {
            sel = sel.min(rows.len() - 1);
            state.select(Some(sel));
            let mut grabbed: Option<String> = None;
            term.draw(|f| {
                render_table(f, &rows, sel, &mut state);
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
                }
            }
        }

        crate::ui_alive();

        if event::poll(Duration::from_millis(120))? {
            let evt = event::read()?;
            if let Event::Mouse(m) = evt {
                if msel.on_mouse(m) {
                    copy_armed = true;
                }
            }
            if let Event::Key(k) = evt {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !rows.is_empty() {
                            sel = (sel + 1).min(rows.len() - 1);
                        }
                    }
                    KeyCode::Enter => break Ok(rows.get(sel).map(|r| r.grad.clone())),
                    KeyCode::Esc | KeyCode::Char('q') => break Ok(None),
                    _ => {}
                }
            }
        }
    };

    stop.store(true, Ordering::Relaxed);
    handle.abort();
    result
}

/// The trenches title, identical whether or not there are rows yet.
///
/// The loading state used a different title entirely, so the whole header
/// changed the moment results arrived — which is the shift that made the screen
/// look like it jumped.
fn trenches_title(rows: usize) -> Line<'static> {
    // The health light rides on the title, where it is visible whether the list
    // is empty or full. An empty list and a refused endpoint look identical
    // otherwise, and only one of them is worth waiting through.
    //
    // And with nothing found yet the title says so. "Trenches live (0)" reads
    // as a finished search that came back empty, which is a different thing
    // from one still running — and the keys it advertises do nothing until
    // there is a row to press them on.
    let v = env!("CARGO_PKG_VERSION");
    // No live count in the title: the list IS the count, and a number that
    // ticks up makes the whole header jitter. Keys wear their brackets, same
    // as everywhere else in the app.
    let text = if rows == 0 {
        format!(" Trenches Bot v{v} — discovering token launches…  [Esc] back ")
    } else {
        format!(" Trenches Bot v{v}  [j/k] select  [Enter] trade  [Esc] back ")
    };
    Line::from(vec![Span::raw(" "), health_dot(), Span::raw(text)])
}

/// Green answering, yellow refusing some, red nothing getting through.
fn health_dot() -> Span<'static> {
    use crate::rpcstats::Health;
    let tone = match crate::rpcstats::health() {
        Health::Ok => crate::view::Tone::Good,
        Health::Degraded => crate::view::Tone::Warn,
        Health::Down => crate::view::Tone::Bad,
    };
    Span::styled("●", Style::default().fg(crate::ui::widgets::tone_color(tone)))
}

/// A status screen: a spinner and a message.
///
/// The health light lives on the title, which this screen already draws, so
/// repeating it beside the message said the same thing twice.
fn draw_scan_status(term: &mut Term, msg: &str, foot: &str, frame: usize) -> eyre::Result<()> {
    // Braille dots: one cell wide in every font that has them, and they turn
    // rather than blink, so a stalled screen is obvious — a frozen spinner
    // looks different from a slow one, which a static "Scanning…" never did.
    const SPIN: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let spin = SPIN[frame % SPIN.len()];
    term.draw(|f| {
        crate::ui::image::clear();
        let block = crate::ui::widgets::themed_block_line(trenches_title(0));
        let body = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    spin,
                    Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Accent)),
                ),
                Span::raw(format!(" {msg}")),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                foot.to_string(),
                Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Dim)),
            )),
        ];
        let p = Paragraph::new(body).block(block).alignment(Alignment::Center);
        f.render_widget(p, f.area());
    })?;
    Ok(())
}

fn draw_status(term: &mut Term, msg: &str) -> eyre::Result<()> {
    term.draw(|f| {
        crate::ui::image::clear();
        let block = crate::ui::widgets::themed_block_line(trenches_title(0));
        let p = Paragraph::new(msg).block(block).alignment(Alignment::Center);
        f.render_widget(p, f.area());
    })?;
    Ok(())
}

/// Decode a 32-byte two's-complement int256 to f64.
fn i256_to_f64(d: &[u8]) -> f64 {
    if d.len() < 32 {
        return 0.0;
    }
    let u = U256::from_be_slice(&d[..32]);
    if u.bit(255) {
        -uf(U256::ZERO.wrapping_sub(u)) // negative: magnitude = 2^256 - u
    } else {
        uf(u)
    }
}

/// Recent v3 Swaps on a pool → (secs since `from`, ETH size, buyer, is_buy).
/// Swaps for one pool in [from, head]. `base` anchors the time axis (seconds
/// since THAT block), so incremental fetches line up with earlier ones —
/// `from` moves forward each round, the anchor must not.
async fn pool_swaps<L: Provider>(logs: &L, pool: Address, weth0: bool, from: u64, head: u64, base: u64) -> Vec<(f64, f64, Address, bool, B256)> {
    let filter = Filter::new().address(pool).event_signature(SWAP_V3).from_block(from).to_block(head);
    let lg = match tokio::time::timeout(RPC_TIMEOUT, logs.get_logs(&filter)).await {
        Ok(Ok(l)) => l,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for l in lg {
        let tp = l.topics();
        if tp.len() < 3 {
            continue;
        }
        let recipient = Address::from_word(tp[2]);
        let data = l.data().data.clone();
        let d = data.as_ref();
        if d.len() < 64 {
            continue;
        }
        let weth_amt = if weth0 { i256_to_f64(&d[0..32]) } else { i256_to_f64(&d[32..64]) };
        let is_buy = weth_amt > 0.0; // WETH INTO the pool = a buy
        let eth = weth_amt.abs() / 1e18;
        let secs = l.block_number.unwrap_or(base).saturating_sub(base) as f64 * SECS_PER_BLOCK;
        out.push((secs, eth, recipient, is_buy, l.transaction_hash.unwrap_or_default()));
    }
    out
}


/// Static Verified-pools browser (Shift-F): the hand-curated pools from config
/// (stock tokens etc.), shown INSTANTLY — no pricing, no mining. Select one to
/// trade (the engine prices it then, like any pool). Returns the chosen Grad.
pub async fn screen_verified(term: &mut Term, verified: Vec<VerifiedPool>) -> eyre::Result<Option<Grad>> {
    // This screen owns the terminal now: take down any image the last one left.
    // Clearing also marks every placement stale, so the dashboard redraws its
    // logo when we return — no screen has to know about any other.
    crate::ui::image::clear();
    if verified.is_empty() {
        draw_status(term, "\nNo verified pools configured (config.json → verified_pools).\n\nEsc to go back")?;
        loop {
            crate::ui_alive();
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(_) = event::read()? {
                    return Ok(None);
                }
            }
        }
    }
    let mut sel: usize = 0;
    loop {
        term.draw(|f| {
            let header = ratatui::widgets::Row::new(["", "sym", "quote", "fee", "token"])
                .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Info)).add_modifier(Modifier::BOLD));
            let trows: Vec<ratatui::widgets::Row> = verified
                .iter()
                .map(|v| {
                    let q = if v.quote.is_eth() { "ETH" } else { "USDG" };
                    let pct = v.fee as f64 / 10_000.0;
                    // These fees are real — a v4 pool id is a hash of its own
                    // parameters, so an id that resolves to a live pool proves
                    // them. Some of these pools genuinely charge tens of
                    // percent, which makes them untradeable however normal the
                    // ticker beside them looks. Colour is the only thing
                    // standing between that and a very expensive keystroke.
                    let fee_style = if pct >= 5.0 {
                        Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Bad))
                            .add_modifier(Modifier::BOLD)
                    } else if pct > 1.0 {
                        Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Warn))
                    } else {
                        Style::default()
                    };
                    ratatui::widgets::Row::new(vec![
                        ratatui::widgets::Cell::from(String::new()),
                        ratatui::widgets::Cell::from(v.sym.clone()),
                        ratatui::widgets::Cell::from(q.to_string()),
                        ratatui::widgets::Cell::from(format!("{pct:.2}%")).style(fee_style),
                        ratatui::widgets::Cell::from(format!("{:#x}", v.token)),
                    ])
                })
                .collect();
            let widths = [
                Constraint::Length(2),
                Constraint::Length(12),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Min(20),
            ];
            let table = ratatui::widgets::Table::new(trows, widths)
                .header(header)
                .row_highlight_style(Style::default().bg(crate::ui::widgets::bg_selection()).add_modifier(Modifier::BOLD))
                .highlight_symbol("▸ ")
                .column_spacing(1)
                .block(crate::ui::widgets::themed_block(format!(
                    " Verified Tokens (static) — {}   j/k select · Enter trade · Esc back ",
                    verified.len()
                )));
            let mut st = TableState::default();
            st.select(Some(sel));
            f.render_stateful_widget(table, f.area(), &mut st);
        })?;
        crate::ui_alive();
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(verified.len() - 1),
                    KeyCode::Enter => {
                        let v = &verified[sel];
                        return Ok(Some(Grad {
                            token: v.token,
                            kind: engine::PoolKind::V4 { pool_id: v.pool_id, tick_spacing: v.tick_spacing },
                            quote: v.quote,
                            sym: v.sym.clone(),
                            fee: v.fee,
                            launch_block: 0,
                            meta: engine::Meta::default(),
                        }));
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

// ============================ Top tokens view ('t') ============================
// A tabbed screen: [leaderboard] = chain-wide top tokens by market cap from the
// Blockscout token API; [big-fish] = recent v3 pools (≤~60m) above a mkt-cap
// floor, scanned on-chain. Both selectable → Grad → trade.

/// One row of the Blockscout token leaderboard.
struct LeaderRow {
    sym: String,
    token: Address,
    mkt_cap_usd: f64,
    vol_usd: f64,
    holders: u64,
    pooled: f64,       // pool's numeraire balance (ETH or USDG), filled by enrich_pooled
    pooled_unit: &'static str, // "ETH" / "USDG" / "" (none found)
}

/// Compact USD in millions/thousands: $4.20M, $840k, $120.
fn usd_m(x: f64) -> String {
    if x >= 1e6 {
        format!("${:.2}M", x / 1e6)
    } else if x >= 1e3 {
        format!("${:.0}k", x / 1e3)
    } else {
        format!("${:.0}", x)
    }
}

/// Parse a JSON field that Blockscout may encode as a number OR a string.
fn json_f64(v: &serde_json::Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0)
}

/// Fetch the chain-wide token leaderboard from Blockscout, ranked by market cap
/// (USD). Needs a browser UA (Cloudflare 403s the default agent). Top ~25,
/// excluding WETH and zero-cap tokens. Pooled balance is filled later.
async fn blockscout_top(client: &reqwest::Client) -> Vec<LeaderRow> {
    let url = "https://robinhoodchain.blockscout.com/api/v2/tokens?type=ERC-20";
    let req = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
        .header("Accept", "application/json")
        .send();
    // Every failure below used to return an empty list, which the screen drew
    // as "Top Tokens (0)" — identical to a chain with no tokens on it. An empty
    // result must never be able to mean "the request failed" in silence.
    let resp = match tokio::time::timeout(RPC_TIMEOUT, req).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            crate::trace(&format!("top tokens: request failed: {e}"));
            return Vec::new();
        }
        Err(_) => {
            crate::trace("top tokens: request timed out");
            return Vec::new();
        }
    };
    let status = resp.status();
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            crate::trace(&format!("top tokens: HTTP {status}, body was not JSON: {e}"));
            return Vec::new();
        }
    };
    let items = match json.get("items").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => {
            crate::trace(&format!("top tokens: HTTP {status}, no `items` array in the response"));
            return Vec::new();
        }
    };
    let mut rows: Vec<LeaderRow> = Vec::new();
    let mut skipped = 0usize;
    for it in items {
        let addr_s = it.get("address_hash").or_else(|| it.get("address")).and_then(|v| v.as_str()).unwrap_or("");
        let token = match addr_s.parse::<Address>() { Ok(a) => a, Err(_) => continue };
        if token == WETH { continue; }
        let mc_usd = it.get("circulating_market_cap").map(json_f64).unwrap_or(0.0);
        // Dropped for want of a market cap. Counted, because "every token was
        // skipped" and "there were no tokens" look the same on screen.
        if mc_usd <= 0.0 {
            skipped += 1;
            continue;
        }
        let sym = it.get("symbol").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let vol_usd = it.get("volume_24h").map(json_f64).unwrap_or(0.0);
        let holders = it.get("holders_count").or_else(|| it.get("holders")).map(|v| json_f64(v) as u64).unwrap_or(0);
        rows.push(LeaderRow { sym, token, mkt_cap_usd: mc_usd, vol_usd, holders, pooled: 0.0, pooled_unit: "" });
    }
    crate::trace(&format!(
        "top tokens: {} of {} items usable ({skipped} had no market cap)",
        rows.len(),
        items.len()
    ));
    rows.sort_by(|a, b| b.mkt_cap_usd.partial_cmp(&a.mkt_cap_usd).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(25);
    rows
}

/// Fill each leaderboard row's pooled balance (the pool's WETH depth, in ETH) by
/// resolving its v3 WETH pool and reading WETH.balanceOf — all batched (getPool
/// for every token×fee, then balanceOf on the survivors). USDG/v4-only tokens are
/// left blank for now (the v4 depth path is dormant).
async fn enrich_pooled(client: &reqwest::Client, url: &str, rows: &mut [LeaderRow]) {
    let fees = [10000u32, 3000, 500, 100];
    let mut pool_calls = Vec::with_capacity(rows.len() * fees.len());
    for r in rows.iter() {
        for &fee in &fees {
            let data = IV3Factory::getPoolCall { tokenA: r.token, tokenB: WETH, fee: fee.try_into().unwrap() }.abi_encode();
            pool_calls.push((V3_FACTORY, data));
        }
    }
    let pool_res = batch_call(client, url, &pool_calls).await;
    // (row idx, pool addr) for every non-zero pool found.
    let mut pools: Vec<(usize, Address)> = Vec::new();
    for ri in 0..rows.len() {
        for fi in 0..fees.len() {
            if let Some(d) = pool_res.get(ri * fees.len() + fi).and_then(|o| o.as_ref()) {
                if d.len() >= 32 {
                    let pool = Address::from_word(B256::from_slice(&d[d.len() - 32..]));
                    if pool != Address::ZERO {
                        pools.push((ri, pool));
                    }
                }
            }
        }
    }
    let bal_calls: Vec<(Address, Vec<u8>)> = pools.iter().map(|(_, p)| (WETH, balanceof_data(*p))).collect();
    let bals = batch_call(client, url, &bal_calls).await;
    for ((ri, _), b) in pools.iter().zip(bals.iter()) {
        let eth = b.as_ref().map(|d| uf(u256_of(d)) / 1e18).unwrap_or(0.0);
        if eth > rows[*ri].pooled {
            rows[*ri].pooled = eth;
            rows[*ri].pooled_unit = "ETH";
        }
    }
}

/// Resolve a bare token address to a tradable v3 WETH-pool Grad (the most-liquid
/// fee tier). None if the token has no live WETH pool.
async fn resolve_v3_grad<P: Provider>(
    provider: &P,
    token: Address,
    sym: &str,
) -> Result<Option<Grad>, String> {
    let factory = IV3Factory::new(V3_FACTORY, provider);
    let mut errors = 0usize;
    let mut last = String::new();
    for fee in [10000u32, 3000, 500, 100] {
        match factory.getPool(token, WETH, fee.try_into().unwrap()).call().await {
            Err(e) => {
                // A lookup that never completed says nothing about whether a
                // pool exists. Reporting it as "no live WETH pool" told the user
                // a fact about the chain based on a failed request.
                errors += 1;
                last = e.to_string();
                crate::trace(&format!("resolve {sym} fee {fee}: getPool failed: {e}"));
            }
            Ok(p) => {
            let addr = p.pool;
            if addr != Address::ZERO {
                let liq = match IV3Pool::new(addr, provider).liquidity().call().await {
                    Ok(l) => l._0,
                    Err(e) => {
                        errors += 1;
                        last = e.to_string();
                        crate::trace(&format!("resolve {sym} fee {fee}: liquidity read failed: {e}"));
                        continue;
                    }
                };
                if liq > 0 {
                    return Ok(Some(Grad {
                        token,
                        kind: engine::PoolKind::V3 { pool_addr: addr, weth_is_token0: WETH < token },
                        quote: engine::Quote::Eth,
                        sym: sym.to_string(),
                        fee,
                        launch_block: 0,
                        meta: engine::Meta::default(),
                    }));
                }
                crate::trace(&format!("resolve {sym} fee {fee}: pool {addr:#x} has no liquidity"));
            }
            }
        }
    }
    if errors > 0 {
        return Err(format!("could not check ({errors} lookups failed: {last})"));
    }
    crate::trace(&format!("resolve {sym}: no WETH pool with liquidity at any fee tier"));
    Ok(None)
}

/// The top-tokens screen ('t'): tabbed leaderboard + big-fish. Returns the chosen
/// token as a Grad (leaderboard picks are resolved to their v3 pool on Enter).
pub async fn screen_top_tokens<P: Provider>(term: &mut Term, provider: &P, disc_url: Option<String>) -> eyre::Result<Option<Grad>> {
    // This screen owns the terminal now: take down any image the last one left.
    // Clearing also marks every placement stale, so the dashboard redraws its
    // logo when we return — no screen has to know about any other.
    crate::ui::image::clear();
    draw_status(term, "\nLoading top tokens…\n\nEsc to go back")?;
    let client = reqwest::Client::new();
    // A configured-but-empty discovery_rpc used to win over the fallback here:
    // `Some("")` is not `None`, so the batch POSTed to "" and every row's
    // pooled depth silently read as zero — which emptied the whole screen.
    // Prefer the balanced pool's pick; it already knows what is healthy.
    let url = crate::rpc::shared()
        .map(|b| b.pick_url(false))
        .or_else(|| disc_url.filter(|u| !u.trim().is_empty() && !u.contains("YOUR_")))
        .unwrap_or_else(|| PUBLIC_RPC.to_string());
    let mut leader = blockscout_top(&client).await;
    enrich_pooled(&client, &url, &mut leader).await; // fill pooled ETH depth

    // Only keep what can actually be traded.
    //
    // Blockscout ranks every ERC-20 on the chain by market cap, and most of the
    // top of that list — bridged stablecoins, wrapped majors — has no WETH pool
    // with liquidity in it. The screen listed them anyway, so the top entries
    // were the ones Enter could not open, and it answered "no live WETH pool"
    // to a row it had just offered. `pooled` is already the pool's WETH depth,
    // so it is exactly the test for whether a row is worth showing.
    let listed = leader.len();
    leader.retain(|r| r.pooled > 0.0);
    crate::trace(&format!(
        "top tokens: {} of {listed} have a funded WETH pool; the rest are not tradable here",
        leader.len()
    ));

    let mut sel: usize = 0;
    let mut note = String::new();
    loop {
        let n = leader.len();
        term.draw(|f| {
            let header = ratatui::widgets::Row::new(["#", "sym", "mkt cap", "pooled", "vol 24h", "holders", "token"])
                .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Info)).add_modifier(Modifier::BOLD));
            let trows: Vec<ratatui::widgets::Row> = leader.iter().enumerate().map(|(i, r)| {
                let pooled = if r.pooled > 0.0 { format!("{:.2} {}", r.pooled, r.pooled_unit) } else { "—".to_string() };
                ratatui::widgets::Row::new([
                    format!("{}", i + 1),
                    r.sym.clone(),
                    usd_m(r.mkt_cap_usd),
                    pooled,
                    usd_m(r.vol_usd),
                    format!("{}", r.holders),
                    format!("{:#x}", r.token),
                ])
            }).collect();
            let widths = [Constraint::Length(4), Constraint::Length(12), Constraint::Length(11), Constraint::Length(13), Constraint::Length(10), Constraint::Length(9), Constraint::Min(20)];
            // One title in both states — a different string when the list is
            // empty makes the whole header jump the moment results land.
            let title = format!(" Top Tokens ({}) j/k select · Enter trade · Esc back{} ", leader.len(), note);
            let mut st = TableState::default();
            if n > 0 { st.select(Some(sel.min(n - 1))); }
            let table = ratatui::widgets::Table::new(trows, widths).header(header)
                .row_highlight_style(Style::default().bg(crate::ui::widgets::bg_selection()).add_modifier(Modifier::BOLD))
                .highlight_symbol("▸ ").column_spacing(1)
                .block(crate::ui::widgets::themed_block(title));
            f.render_stateful_widget(table, f.area(), &mut st);
        })?;
        crate::ui_alive();
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => { if n > 0 { sel = (sel + 1).min(n - 1); } }
                    KeyCode::Enter => {
                        if let Some(r) = leader.get(sel) {
                            draw_status(term, "\nResolving pool…")?;
                            match resolve_v3_grad(provider, r.token, &r.sym).await {
                                Ok(Some(g)) => return Ok(Some(g)),
                                Ok(None) => {
                                    note = format!("  · {}: no live WETH pool", r.sym)
                                }
                                Err(why) => note = format!("  · {}: {why}", r.sym),
                            }
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

/// Which launchpad a discovery row came from, for the `src` column. Every V3
/// grad in this feed is a Pons graduation; FlaunchV4 is Flaunch by identity.
fn venue_tag(g: &Grad) -> &'static str {
    match g.kind {
        engine::PoolKind::V3 { .. } => "pons",
        engine::PoolKind::FlaunchV4 { .. } => "flnch",
        // Still on its curve — a different thing to trade than a pool, and the
        // column is the place that says so.
        engine::PoolKind::PonsCurve { .. } => "pons2",
        engine::PoolKind::V4 { .. } => "v4",
    }
}

fn render_table(f: &mut Frame, rows: &[Row], sel: usize, state: &mut TableState) {
    // Split: table on top, a details box (socials for the selected row) below.
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).split(f.area());
    let header = ratatui::widgets::Row::new([
        "", "source", "symbol", "pooled ETH", "mkt cap", "metadata", "tx/sec", "age", "mine", "pool",
    ])
    .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Info)).add_modifier(Modifier::BOLD));

    let trows: Vec<ratatui::widgets::Row> = rows
        .iter()
        .map(|r| {
            let age = {
                // Real age from the current block captured at each metrics refresh
                // (~2s), so it ticks up instead of being relative to the newest pool.
                let s = age_secs(r) as u64;
                crate::view::age_compact(s as f64)
            };
            let mine = if r.my_bal > 0.0 { "●" } else { "" };
            let active = r.tx_per_sec >= HOT_TX_PER_SEC;
            let fire = is_fire(r); // 🔥 only if active AND cap ≥ 4 ETH
            let meta_full = r.grad.meta.score() >= 4;
            ratatui::widgets::Row::new(vec![
                Cell::from(if fire { "🔥" } else { "" }),
                Cell::from(venue_tag(&r.grad))
                    .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Dim))),
                Cell::from(r.grad.sym.clone()).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(format!("{:.4}", r.pooled_eth)),
                Cell::from(format!("{:.3} ETH", r.mkt_cap_eth)),
                Cell::from(format!("{}/{}", r.grad.meta.score(), META_FIELDS))
                    .style(Style::default().fg(if meta_full { crate::ui::widgets::tone_color(crate::view::Tone::Good) } else { crate::ui::widgets::tone_color(crate::view::Tone::Normal) })),
                Cell::from(format!("{:.2}", r.tx_per_sec))
                    .style(Style::default().fg(if active { crate::ui::widgets::tone_color(crate::view::Tone::Good) } else { crate::ui::widgets::tone_color(crate::view::Tone::Normal) })),
                Cell::from(age),
                Cell::from(mine).style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Warn))),
                Cell::from(r.grad.pool_display()).style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Normal))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Length(12),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Min(20),
    ];
    let table = ratatui::widgets::Table::new(trows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(crate::ui::widgets::bg_selection()).add_modifier(Modifier::BOLD))
        .highlight_symbol("▸ ")
        .column_spacing(1)
        // Fixed-width count: a title that grows from "(1)" to "(12)" shifts
        // every word after it, so the header appears to jitter as launches land.
        .block(crate::ui::widgets::themed_block_line(trenches_title(rows.len())));
    f.render_stateful_widget(table, chunks[0], state);

    // Details box for the selected pool — the actual X / telegram / website.
    let detail: Vec<Line> = match rows.get(sel) {
        Some(r) => {
            let m = &r.grad.meta;
            let fld = |label: &str, v: &str| {
                if v.trim().is_empty() {
                    // Only the ABSENCE marker is dimmed — it carries no info.
                    Line::from(format!("{:<10} —", label))
                        .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Dim)))
                } else {
                    // Real socials are readable content, not chrome.
                    Line::from(format!("{:<10} {}", label, v))
                        .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Normal)))
                }
            };
            // Links only. The symbol, meta score and address are already in
            // the row above, so repeating them here just costs a line.
            vec![
                fld("x/twitter", &m.twitter),
                fld("telegram", &m.telegram),
                fld("website", &m.website),
            ]
        }
        // Same shape as a filled row, so the box never changes height and the
        // rows below it hold still.
        None => vec![
            Line::from(format!("{:<10} —", "x/twitter")),
            Line::from(format!("{:<10} —", "telegram")),
            Line::from(format!("{:<10} —", "website")),
        ],
    };
    let p = Paragraph::new(detail).block(crate::ui::widgets::themed_block(""));
    f.render_widget(p, chunks[1]);
}

#[cfg(test)]
mod flaunch_discovery_tests {
    use super::*;
    use alloy::primitives::{address, b256, keccak256, Bytes};
    use alloy::sol_types::SolValue;

    // A real PoolCreated log captured from Robinhood Chain block 22316018
    // (the FREE launch) via eth_getLogs on the Flaunch PositionManager.
    const TOPIC0: B256 = b256!("88f75d7341103964abd68b657439ace486e110bfda589da9374fe9d69213c7c8");
    const POOL_ID: B256 = b256!("d38591585417dbf8a98e03a7db6f96df59b336914c623abfdd7790613c87afc0");
    const DATA: &str = "000000000000000000000000450e67ad2abf5e47eb41f68e414f62ae7a29f7d1000000000000000000000000507e64349fe74b744a9e26634ae9f3466ff629b900000000000000000000000000000000000000000000000000000000000002d00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000120000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000001a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000065d673f25b5878df2a2e8a5203fe2b2846c5cbba00000000000000000000000000000000000000000000000000000000000025e400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000000000044672656500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000446524545000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000035697066733a2f2f516d6358654c3867583279554739717069356e565469694373795456744c716b725369666f4156476d45724546310000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000002540be4000000000000000000000000000000000000000000000000000000000000000000";

    fn fixture_log() -> alloy::primitives::Log {
        alloy::primitives::Log::new_unchecked(
            FLAUNCH_PM,
            vec![TOPIC0, POOL_ID],
            Bytes::from(alloy::hex::decode(DATA).unwrap()),
        )
    }

    #[test]
    fn pool_created_decodes_from_a_real_log() {
        // The declared event signature must match the deployed contract's —
        // a drifted sol! declaration fails here, not silently on-chain.
        assert_eq!(IFlaunchPositionManager::PoolCreated::SIGNATURE_HASH, TOPIC0);
        let ev = IFlaunchPositionManager::PoolCreated::decode_log(&fixture_log(), true).unwrap();
        assert_eq!(ev.data._poolId, POOL_ID);
        assert_eq!(ev.data._memecoin, address!("450e67ad2abf5e47eb41f68e414f62ae7a29f7d1"));
        assert!(!ev.data._currencyFlipped);
        assert_eq!(ev.data._params.symbol, "FREE");
        assert!(ev.data._params.tokenUri.starts_with("ipfs://"));
        assert_eq!(u64::try_from(ev.data._params.flaunchAt).unwrap(), 0);
    }

    #[test]
    fn pool_id_recomputes_from_the_pool_key() {
        // fetch_flaunch_pool derives the pool id from poolKey(token) — the
        // derivation must land on the id the launch event indexed.
        let ev = IFlaunchPositionManager::PoolCreated::decode_log(&fixture_log(), true).unwrap();
        let key = crate::contracts::PoolKey {
            currency0: crate::contracts::FLETH,
            currency1: ev.data._memecoin,
            fee: alloy::primitives::aliases::U24::ZERO,
            tickSpacing: crate::contracts::FLAUNCH_TICK_SPACING.try_into().unwrap(),
            hooks: FLAUNCH_PM,
        };
        assert_eq!(keccak256(key.abi_encode()), POOL_ID);
    }

    /// A direct public-RPC provider for the opt-in live tests. The app itself
    /// routes through the balanced transport; these tests deliberately pin the
    /// endpoint so a failure means the chain, not the routing.
    async fn logs_provider() -> Option<impl Provider + Send + Sync> {
        alloy::providers::ProviderBuilder::new().on_builtin(PUBLIC_RPC).await.ok()
    }

    /// Live: scan a recent window on the public RPC, decode at least one
    /// launch, and round-trip its pool through StateView + poolKey().
    /// Run: cargo test flaunch_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn flaunch_live_scan_finds_pools() {
        let lp = logs_provider().await.expect("public RPC reachable");
        let head = lp.get_block_number().await.expect("head block");
        // ~28 hours of blocks: launches are steady but not minutely.
        let cands = scan_flaunch(&lp, head.saturating_sub(1_000_000), head).await;
        println!("found {} launches in the window", cands.len());
        assert!(!cands.is_empty(), "no PoolCreated logs in a 1M-block window");
        let c = &cands[0];
        println!("newest: {} {} pool {:#x}", c.sym, c.token, c.pool_id);
        let sv = IStateView::new(STATE_VIEW, &lp);
        let slot0 = sv.getSlot0(c.pool_id).call().await.expect("StateView answers");
        assert!(uf(slot0.sqrtPriceX96) > 0.0, "pool has a price");
        let fl = fetch_flaunch_pool(&lp, c.token).await.expect("poolKey() round-trips");
        assert_eq!(fl.pool_id, c.pool_id, "poolKey-derived id equals the event's");
        assert_eq!(fl.launch_block, c.block);
    }

    /// Live: a 0.001-ETH buy of the newest launch must pass eth_call from a
    /// funded account (the coin's own treasury holds ETH-free tokens, so use a
    /// known-funded EOA: the FlaunchZap deployer would do; any address with
    /// ETH works since eth_call checks balance for value-bearing calls).
    /// Run: cargo test flaunch_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn flaunch_live_buy_preflights() {
        let lp = logs_provider().await.expect("public RPC reachable");
        let head = lp.get_block_number().await.expect("head block");
        let cands = scan_flaunch(&lp, head.saturating_sub(1_000_000), head).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let c = cands.iter().find(|c| c.flaunch_at <= now).expect("a live launch");
        let data = crate::v4::flaunch_swap_calldata(c.token, true, 1_000_000_000_000_000, 0);
        // A funded holder: the flETH contract itself always carries ETH.
        let from = crate::contracts::FLETH;
        let tx = alloy::rpc::types::TransactionRequest::default()
            .to(crate::contracts::UNIVERSAL_ROUTER)
            .input(data.into())
            .value(U256::from(1_000_000_000_000_000u64))
            .from(from);
        let out = lp.call(&tx).await;
        println!("preflight {} -> {:?}", c.sym, out.as_ref().map(|b| b.len()));
        assert!(out.is_ok(), "buy preflight reverted: {:?}", out.err());
    }
}

#[cfg(test)]
mod pons_v2_tests {
    use super::*;

    /// v1 and v2 both call their event `TokenLaunched`, and both factories are
    /// live. They MUST hash differently, or one launchpad's launches would be
    /// decoded with the other's layout — silently, since the topic is all that
    /// tells them apart.
    #[test]
    fn the_two_launch_events_cannot_be_confused() {
        assert_ne!(
            IPonsFactory::TokenLaunched::SIGNATURE_HASH,
            IPonsV2Factory::TokenLaunched::SIGNATURE_HASH,
            "v1 and v2 TokenLaunched must not share a topic"
        );
    }

    /// A v2 log decodes to the token, its curve, and the asset it is priced in.
    /// The curve matters most: before graduation there is no pool at all, so it
    /// is the only place a price can come from.
    #[test]
    fn a_v2_launch_decodes_to_its_curve_and_quote_asset() {
        use alloy::primitives::{address, b256, Bytes, LogData};
        let token = address!("a1186a1bcde151634440e5f51ae998c61e465f5d");
        let curve = address!("bc39b6502e1a6ab36e4a5c5026a35f08342a0a9c");
        let pair = address!("5fc5360d0400a0fd4f2af552add042d716f1d168"); // USDG
        let mut data = Vec::new();
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(pair.as_slice()); // pairToken
        data.extend_from_slice(&[0u8; 32]); // launchConfigId
        let mut thr = [0u8; 32];
        thr[24..].copy_from_slice(&5_000_000_000_000_000_000u64.to_be_bytes());
        data.extend_from_slice(&thr); // graduationThreshold

        let lg = alloy::rpc::types::Log {
            inner: alloy::primitives::Log {
                address: PONS_V2_FACTORY,
                data: LogData::new_unchecked(
                    vec![
                        IPonsV2Factory::TokenLaunched::SIGNATURE_HASH,
                        token.into_word(),
                        curve.into_word(),
                        b256!("0000000000000000000000000000000000000000000000000000000000000001"),
                    ],
                    Bytes::from(data),
                ),
            },
            block_number: Some(42),
            ..Default::default()
        };

        let c = decode_pons_v2_log(&lg).expect("should decode");
        assert_eq!(c.token, token);
        assert_eq!(c.curve, curve);
        assert_eq!(c.pair_token, pair, "a launch priced in USDG, not ETH");
        assert_eq!(c.threshold, U256::from(5_000_000_000_000_000_000u64));
        assert_eq!(c.block, 42);
    }

    /// A truncated or foreign log must decode to nothing rather than to
    /// garbage — this runs over every log the scan collects.
    #[test]
    fn a_foreign_log_is_refused() {
        use alloy::primitives::{Bytes, LogData};
        let lg = alloy::rpc::types::Log {
            inner: alloy::primitives::Log {
                address: PONS_V2_FACTORY,
                data: LogData::new_unchecked(
                    vec![IPonsFactory::TokenLaunched::SIGNATURE_HASH],
                    Bytes::from(vec![0u8; 96]),
                ),
            },
            ..Default::default()
        };
        assert!(decode_pons_v2_log(&lg).is_none(), "wrong topic must not decode");
    }
}
