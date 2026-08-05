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
    IERC20, IFlaunchPositionManager, IPonsCurve, IPonsFactory, IPonsV2Factory, IStateView,
    IV3Factory, IV3Pool,
    FLAUNCH_FEE_EST, flaunch_pm, pons_factory, pons_v2_factory, pool_manager, state_view,
    v3_factory, weth,
};
use crate::engine;

type Term = Terminal<CrosstermBackend<Stdout>>;

const SWAP_V3: B256 =
    alloy::primitives::b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");

const SECS_PER_BLOCK: f64 = 0.1; // ~10 blocks/sec on Robinhood Chain
const ACTIVITY_WINDOW: u64 = 200; // ~20 s sample for tx/sec
const MAX_SCAN: usize = 48; // newest launches to track (bounds RPC)
const CONCURRENCY: usize = 12; // in-flight RPC cap
const RPC_TIMEOUT: Duration = Duration::from_secs(6);
const SOCIAL_FIELDS: u8 = 7; // logo, description, twitter, telegram, discord, website, farcaster
const HOT_TX_PER_SEC: f64 = 1.0; // threshold for the 🔥 (active) marker + top-of-list
const HOT_MKTCAP_ETH: f64 = 5.0; // 🔥 fire needs cap ≥ this (and < 1 min old); bumped to top

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
    pub socials: engine::TokenSocials, // on-chain socials/metadata (score() = filled fields)
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
            | engine::PoolKind::FlaunchV4 { pool_id, .. }
            | engine::PoolKind::PonsV2Pool { pool_id, .. } => pool_id,
        }
    }
    /// Human display of the venue id — the 20-byte pool ADDRESS for v3, the
    /// 32-byte pool_id for v4. Never the zero-padded form.
    pub fn pool_display(&self) -> String {
        match self.kind {
            engine::PoolKind::V3 { pool_addr, .. } => format!("{pool_addr:#x}"),
            engine::PoolKind::PonsCurve { curve, .. } => format!("{curve:#x}"),
            engine::PoolKind::V4 { pool_id, .. }
            | engine::PoolKind::FlaunchV4 { pool_id, .. }
            | engine::PoolKind::PonsV2Pool { pool_id, .. } => format!("{pool_id:#x}"),
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
    /// Progress toward graduation, 0.0..=1.0+, where the venue defines one.
    /// pons v1: WETH paired / threshold, from `graduationStatus`. pons v2: the
    /// curve's raised quote / its threshold. None: not yet measured, or the
    /// venue has no such concept (Flaunch).
    grad_pct: Option<f64>,
    /// Latched by the factory (`graduatedAt != 0`), not inferred from the
    /// percentage — paired WETH can fall back below the threshold afterwards,
    /// and graduation does not un-happen.
    graduated: bool,
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
            .then(b.grad.socials.score().cmp(&a.grad.socials.score()))
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

/// Every pons launch is a 1% pool — fixed by the launchpad, not chosen per
/// coin, which is what makes the pool derivable from the token alone.
fn pons_fee() -> alloy::primitives::Uint<24, 1> {
    alloy::primitives::Uint::<24, 1>::from(10_000u32)
}

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
    pub config_id: U256,
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
        config_id: U256::from_be_slice(&d[32..64]),
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
    (pair == weth()).then_some((token, pool, lg.block_number.unwrap_or(0)))
}

/// A pons launch at CREATION: the token, and the block it appeared in.
///
/// The pool is not in the event because it does not need to be — pons creates
/// the token and its WETH pool in one transaction, so the pool is
/// `getPool(token, WETH, 10000)` on the v3 factory. Resolved by the caller,
/// which has a provider; this stays a pure decode.
fn decode_pons_deploy(lg: &alloy::rpc::types::Log) -> Option<(Address, u64)> {
    let topics = lg.topics();
    if topics.len() < 4 {
        return None;
    }
    let token = Address::from_word(topics[1]);
    let b = lg.data().data.clone();
    let b = b.as_ref();
    if b.len() < 32 {
        return None;
    }
    // Only WETH-paired launches, exactly as the graduation decoder insists.
    let pair = Address::from_slice(&b[12..32]);
    (pair == weth()).then_some((token, lg.block_number.unwrap_or(0)))
}

/// Put a row on the list for every launch known, whether or not it has been
/// measured yet.
///
/// The list is a STREAM. A launch belongs on it the moment it is seen, and its
/// numbers fill in behind — rather than the row waiting until a metrics round
/// reaches it, which is why a chain producing a launch every few seconds
/// showed six of them.
///
/// Existing rows are left exactly as they are: this only adds what is missing,
/// so nothing measured is overwritten with zeros.
fn seed_rows(
    shared: &Arc<Mutex<Vec<Row>>>,
    known: &[(Address, Address, u64)],
    known_fl: &[FlCand],
    head: u64,
) -> usize {
    let mut cur = shared.lock().unwrap();
    let mut added = 0;
    for (token, pool, block) in known {
        if cur.iter().any(|r| r.grad.token == *token) {
            continue;
        }
        let f = crate::token_metadata_chain_id::get(*token).unwrap_or_default();
        cur.push(build_row(*token, *pool, *block, &f, 0.0, 0.0, 0.0, 0, head));
        added += 1;
    }
    for c in known_fl {
        if cur.iter().any(|r| r.grad.token == c.token) {
            continue;
        }
        cur.push(build_fl_row(c, 0.0, 0.0, 0.0, 0, head));
        added += 1;
    }
    if added > 0 {
        // In canonical order immediately. A streamed row that appears at the
        // bottom and jumps to the top a round later reads as two events.
        sort_rows(&mut cur);
    }
    added
}

/// Turn raw factory logs into launch candidates, whichever launchpad emitted
/// them.
///
/// Split out so the polled scan and the live subscription decode identically.
/// Two decoders for one event shape is two places for a launch to be missed,
/// and they would drift the first time an ABI changed.
pub fn sort_launch_logs(
    logs: &[alloy::rpc::types::Log],
) -> (Vec<(Address, Address, u64)>, Vec<FlCand>, Vec<PonsV2Cand>, Vec<(Address, u64)>) {
    let mut seen = std::collections::HashSet::new();
    let (mut pons, mut fl, mut v2) = (Vec::new(), Vec::new(), Vec::new());
    let mut deployed: Vec<(Address, u64)> = Vec::new();
    for lg in logs {
        if lg.address() == pons_v2_factory() {
            if let Some(c) = decode_pons_v2_log(lg) {
                if seen.insert(c.token.into_word()) {
                    v2.push(c);
                }
            }
        } else if lg.address() == pons_factory() {
            // Graduation carries the pool; creation does not, and is left for
            // the caller to resolve. Both are the same coin, so whichever
            // arrives first wins and the other is a duplicate.
            if lg.topics().first() == Some(&IPonsFactory::TokenDeployed::SIGNATURE_HASH) {
                if let Some((token, block)) = decode_pons_deploy(lg) {
                    if seen.insert(token.into_word()) {
                        deployed.push((token, block));
                    }
                }
            } else if let Some(c) = decode_pons_log(lg) {
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
    (pons, fl, v2, deployed)
}

/// One chunked getLogs covers EVERY launchpad: both factory addresses, both
/// event topics, dispatched by the emitting address. Scanning them separately
/// doubled the widest, most rate-limited request the app makes, every round.
/// The launches in `from..to`, and HOW FAR the scan actually got.
///
/// The fourth value is the last block confirmed read — the end of the last
/// unbroken run of successful chunks. It is not `to` when a chunk was refused,
/// and the caller must not move its cursor past it: a chunk the provider
/// rejected is blocks nobody looked at, and skipping them loses every launch
/// inside them permanently.
fn note_v2_supply<P: Provider + Clone + Send + Sync + 'static>(provider: &P, c: &PonsV2Cand) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<Address>>> = std::sync::Mutex::new(None);
    {
        let Ok(mut g) = SEEN.lock() else { return };
        if !g.get_or_insert_with(Default::default).insert(c.token) {
            return;
        }
    }
    let (p, token, id) = (provider.clone(), c.token, c.config_id);
    tokio::spawn(async move {
        let fac = IPonsV2Factory::new(pons_v2_factory(), &p);
        let call = fac.getLaunchConfig(id);
        match tokio::time::timeout(RPC_TIMEOUT, call.call()).await {
            Ok(Ok(cfg)) => {
                let supply = uf(cfg.supply) / 1e18;
                if supply > 0.0 {
                    crate::trace(&format!("pons-v2 supply: {token} = {supply}"));
                    crate::token_metadata_chain_id::merge(token, move |f| {
                        if f.supply <= 0.0 {
                            f.supply = supply;
                        }
                    });
                }
            }
            _ => crate::trace(&format!("pons-v2 supply: config {id} read failed for {token}")),
        }
    });
}

fn launch_addresses() -> Vec<Address> {
    [pons_factory(), flaunch_pm(), pons_v2_factory()]
        .into_iter()
        .filter(|a| !a.is_zero())
        .collect()
}

fn launch_topics() -> Vec<B256> {
    vec![
        IPonsFactory::TokenDeployed::SIGNATURE_HASH,
        IPonsFactory::TokenLaunched::SIGNATURE_HASH,
        IFlaunchPositionManager::PoolCreated::SIGNATURE_HASH,
        IPonsV2Factory::TokenLaunched::SIGNATURE_HASH,
    ]
}

async fn scan_launchpads<P: Provider>(
    provider: &P,
    from: u64,
    to: u64,
) -> (Vec<(Address, Address, u64)>, Vec<FlCand>, Vec<PonsV2Cand>, Option<u64>) {
    let mut logs = Vec::new();
    let (from, chunk) = clamp_scan("launch scan", from, to, LOG_CHUNK);
    let mut start = from;
    let (mut ok, mut failed) = (0u32, 0u32);
    // How far the scan is certain of, and whether it is still certain.
    let mut covered: Option<u64> = None;
    let mut unbroken = true;
    while start <= to {
        let end = (start + chunk - 1).min(to);
        // Only the launchpads this chain actually has. A zero address means
        // "not deployed here", and asking a node to watch for logs from
        // address zero is a filter that can only ever match nothing — paid for
        // on every round, on a metered endpoint.
        let pads = launch_addresses();
        if pads.is_empty() {
            return (Vec::new(), Vec::new(), Vec::new(), Some(to));
        }
        let filter = Filter::new()
            .address(pads)
            .event_signature(launch_topics())
            .from_block(start)
            .to_block(end);
        match tokio::time::timeout(RPC_TIMEOUT, provider.get_logs(&filter)).await {
            Ok(Ok(l)) => {
                ok += 1;
                // Only extend the confirmed run while it is unbroken: a later
                // success does not fill the hole a failure left behind.
                if unbroken {
                    covered = Some(end);
                }
                logs.extend(l);
            }
            // Say so. An empty trenches screen should never be able to mean
            // "the query failed" without leaving a trace of it.
            Ok(Err(e)) => {
                failed += 1;
                crate::trace(&format!("launch scan {start}..{end} failed: {e}"));
                unbroken = false;
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
                unbroken = false;
            }
        }
        start = end + 1;
    }
    let (mut pons, mut fl, v2, deployed) = sort_launch_logs(&logs);
    // A creation names no pool, so ask the factory for it. One call per NEW
    // coin, and only for coins this scan has not already seen graduate.
    for (token, block) in deployed {
        if pons.iter().any(|(t, _, _)| *t == token) {
            continue;
        }
        let f = IV3Factory::new(v3_factory(), provider);
        let call = f.getPool(token, weth(), pons_fee());
        match tokio::time::timeout(RPC_TIMEOUT, call.call()).await {
            Ok(Ok(p)) if !p.pool.is_zero() => pons.push((token, p.pool, block)),
            _ => crate::trace(&format!("launch scan: no 1% pool for {token} yet")),
        }
    }
    if !v2.is_empty() {
        crate::trace(&format!("launch scan: {} pons v2 launch(es)", v2.len()));
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
    (pons, fl, v2, covered)
}

/// One row from cached on-chain metadata + batch-read metrics. No RPC of its own: the
/// metadata was fetched once ever, the metrics arrive from the round's single
/// batched `eth_call`, and the swap count from the round's single tape scan.
#[allow(clippy::too_many_arguments)]
fn build_row(
    token: Address,
    pool_addr: Address,
    block: u64,
    f: &crate::token_metadata_chain_id::TokenMetadata,
    sqrt: f64,
    liq: f64,
    my_bal: f64,
    swaps_in_window: usize,
    head: u64,
) -> Row {
    let grad = Grad {
        token,
        kind: engine::PoolKind::V3 { pool_addr, weth_is_token0: weth() < token },
        quote: engine::Quote::Eth,
        sym: f.sym.clone(),
        fee: if f.fee > 0 { f.fee } else { 10_000 },
        launch_block: block,
        socials: f.socials.clone(),
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

    Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec, my_bal, grad_pct: None, graduated: false, head_block: head, verified: false }
}

/// Seconds since a row's pool graduated (from the block delta at measure time).
/// The newest head any round has seen. The clock every age is measured
/// against.
static LIVE_HEAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_head(h: u64) {
    LIVE_HEAD.fetch_max(h, std::sync::atomic::Ordering::Relaxed);
}

/// How old a launch is, against the LIVE head rather than the row's own.
///
/// `head_block` is stamped on a row when it is measured, and only some rows
/// are re-measured each round. Reading age from it means a row refreshed two
/// minutes ago reports the age it had two minutes ago — so a list sorted
/// newest-first displays 1m, 1m, 2m, 1m, 2m, which looks like broken sorting
/// and is actually every row telling the time from a different clock.
///
/// Same mistake as the row TTL had: a clock cannot live inside the thing it is
/// timing. Falls back to the row's own stamp only before any head is known.
fn age_secs(r: &Row) -> f64 {
    let head = LIVE_HEAD.load(std::sync::atomic::Ordering::Relaxed).max(r.head_block);
    head.saturating_sub(r.grad.launch_block) as f64 * SECS_PER_BLOCK
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
            .address(flaunch_pm())
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
fn fl_meta_cache() -> &'static Mutex<std::collections::HashMap<Address, engine::TokenSocials>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::HashMap<Address, engine::TokenSocials>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn fl_meta(token: Address, token_uri: &str) -> engine::TokenSocials {
    if let Ok(cache) = fl_meta_cache().lock() {
        if let Some(m) = cache.get(&token) {
            return m.clone();
        }
    }
    // A restart keeps what an earlier session fetched: the metadata file holds
    // the metadata, so the IPFS gateway is asked once per token, ever.
    if let Some(f) = crate::token_metadata_chain_id::get(token) {
        if !f.socials.is_empty() {
            if let Ok(mut cache) = fl_meta_cache().lock() {
                cache.insert(token, f.socials.clone());
            }
            return f.socials;
        }
    }
    if !token_uri.trim().is_empty() {
        // Mark as in-flight BEFORE spawning, so the next 1.2s round doesn't
        // spawn a duplicate fetch while this one is still resolving.
        if let Ok(mut cache) = fl_meta_cache().lock() {
            cache.insert(token, engine::TokenSocials::default());
        }
        let uri = token_uri.to_string();
        tokio::spawn(async move {
            let meta = engine::fetch_flaunch_meta(&uri).await;
            if !meta.is_empty() {
                let m2 = meta.clone();
                crate::token_metadata_chain_id::merge(token, move |f| f.socials = m2);
            }
            if let Ok(mut cache) = fl_meta_cache().lock() {
                cache.insert(token, meta);
            }
        });
    }
    engine::TokenSocials::default()
}

/// One Flaunch row from what the round already paid for: metrics from the
/// batched StateView reads, swap count from the shared tape, supply from the
/// on-chain metadata cache, symbol/socials from the launch event. No RPC of its own — the
/// same contract `build_row` has for Pons rows.
/// A row for a pons v2 launch still on its bonding curve.
///
/// `quote_reserve` includes the PHANTOM balance that sets the opening price, so
/// it is the right number for price and the wrong one for depth. `raised` is
/// what the curve actually holds, and that is what goes in the pooled column —
/// otherwise every launch would look better funded than it is, by exactly the
/// amount nobody deposited.
fn build_v2_row(
    c: &PonsV2Cand,
    quote_reserve: f64,
    token_reserve: f64,
    raised: f64,
    my_bal: f64,
    head: u64,
) -> Row {
    let f = crate::token_metadata_chain_id::get(c.token);
    let sym = f.as_ref().map(|f| f.sym.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| {
        format!("0x{}", &alloy::hex::encode(c.token.as_slice())[..6])
    });
    let supply = f.as_ref().map(|f| f.supply).unwrap_or(0.0);
    // Native quote is 18-dec like ETH; an ERC-20 quote carries its own, and
    // the launch is priced in THAT asset rather than in ETH.
    let quote = if c.pair_token == Address::ZERO {
        engine::Quote::Eth
    } else {
        engine::Quote::Stable {
            token: c.pair_token,
            // The QUOTE asset's decimals, not the launch token's. USDG is 6,
            // and reading a 6-dec reserve as 18 shows the price as 0.000000.
            decimals: crate::token_metadata_chain_id::get(c.pair_token).and_then(|f| f.decimals).unwrap_or(18),
        }
    };
    let qd = quote.decimals() as i32;
    let pooled = raised / 10f64.powi(qd);
    // Price is the reserve ratio, both sides in their own decimals.
    let q = quote_reserve / 10f64.powi(qd);
    let t = token_reserve / 1e18;
    let quote_per_token = if t > 0.0 { q / t } else { 0.0 };
    let grad = Grad {
        token: c.token,
        kind: engine::PoolKind::PonsCurve { curve: c.curve, quote: c.pair_token },
        quote,
        sym,
        // The curve's own fee, read per launch when one is opened. Zero here
        // means "not known yet", not "free".
        fee: 0,
        launch_block: c.block,
        socials: engine::TokenSocials::default(),
    };
    Row {
        grad,
        pooled_eth: pooled,
        mkt_cap_eth: supply * quote_per_token,
        tx_per_sec: 0.0,
        my_bal,
        grad_pct: None,
        graduated: false,
        head_block: head,
        verified: false,
    }
}

fn build_fl_row(c: &FlCand, sqrt: f64, liq: f64, my_bal: f64, swaps_in_window: usize, head: u64) -> Row {
    let socials = fl_meta(c.token, &c.token_uri);
    let supply = crate::token_metadata_chain_id::get(c.token).map(|f| f.supply).unwrap_or(0.0);
    let grad = Grad {
        token: c.token,
        kind: engine::PoolKind::FlaunchV4 { pool_id: c.pool_id, coin_is_0: c.coin_is_0 },
        quote: engine::Quote::Eth,
        sym: c.sym.clone(),
        // The hook's ~1% cut, for quote estimates — the pool's own lpFee is 0.
        fee: FLAUNCH_FEE_EST,
        launch_block: c.block,
        socials,
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

    Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec, my_bal, grad_pct: None, graduated: false, head_block: head, verified: false }
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
    let pm = IFlaunchPositionManager::new(flaunch_pm(), provider);
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
        .address(flaunch_pm())
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
            .address(pons_factory())
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
                if Address::from_slice(&b[12..32]) != weth() { continue; }
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
        cands.iter().map(|(_, p, _)| (weth(), balanceof_data(*p))).collect();
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
        let weth0 = weth() < *t;
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
            socials: engine::TokenSocials::default(),
        };
        rows.push(Row { grad, pooled_eth: *eth, mkt_cap_eth, tx_per_sec: 0.0, my_bal: 0.0, grad_pct: None, graduated: false, head_block: head, verified: false });
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
            .address(pool_manager())
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
                let (token, quote) = if c0 == weth() {
                    (c1, engine::Quote::Eth)
                } else if c1 == weth() {
                    (c0, engine::Quote::Eth)
                } else if c0 == USDG {
                    (c1, engine::Quote::Stable { token: USDG, decimals: 6 })
                } else if c1 == USDG {
                    (c0, engine::Quote::Stable { token: USDG, decimals: 6 })
                } else {
                    continue; // not a weth()/USDG pool → engine can't price it
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
        calls.push((state_view(), poolid_call(SEL_GETSLOT0, *id)));
        calls.push((state_view(), poolid_call(SEL_GETLIQ, *id)));
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
            socials: engine::TokenSocials::default(),
        };
        rows.push(Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec: 0.0, my_bal: 0.0, grad_pct: None, graduated: false, head_block: head, verified: false });
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
        calls.push((state_view(), poolid_call(SEL_GETSLOT0, p.pool_id)));
        calls.push((state_view(), poolid_call(SEL_GETLIQ, p.pool_id)));
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
            socials: engine::TokenSocials::default(),
        };
        rows.push(Row { grad, pooled_eth, mkt_cap_eth, tx_per_sec: 0.0, my_bal: 0.0, grad_pct: None, graduated: false, head_block: head, verified: true });
    }
    rows
}

/// The discovery rows, alive for the whole process so leaving the screen and
/// coming back does not start from nothing.
fn rows_cache() -> Arc<Mutex<Vec<Row>>> {
    static CACHE: std::sync::OnceLock<Arc<Mutex<Vec<Row>>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default).clone()
}

/// Ceiling on the list, so a busy day cannot grow it without bound.
///
/// The ONLY thing that removes a row. There was an age cut as well — two
/// hours — and between them the list could shrink while you watched it: seven
/// launches, then three. The Solana side has never done that; it accumulates
/// everything the socket has seen since it connected and caps by count, and a
/// session's launches are what a session is for. This does the same.
const ROW_MAX: usize = 1_000;

/// How many of the older (non-fresh) candidates get their metrics refreshed
/// per round, round-robin. The newest MAX_SCAN refresh every round; the rest
/// take turns, so the whole remembered list stays current within ~10s without
/// costing a full sweep every round.
const SWEEP_CHUNK: usize = 24;
/// New tokens whose on-chain metadata is fetched per round — 6 calls
/// each, once ever — the bound only smooths the burst when a fresh install
/// meets 150 remembered tokens at once.
const FACTS_PER_ROUND: usize = 16;

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
///      to disk by src/token_metadata_chain_id.rs, never asked again). Flaunch tokens carry
///      symbol and metadata in the launch event itself, so only their total
///      supply ever needs a call.
/// Fold what the socket has delivered into the candidate lists and put a row
/// on the list for each new launch — the streaming half of discovery.
///
/// Called from two places: once at the top of every round, and from the wait
/// at the bottom of the loop THE MOMENT the socket delivers. When this
/// returns, the launch is visible; its numbers follow on the round cadence.
#[allow(clippy::too_many_arguments)]
async fn ingest_live<P: Provider + Clone + Send + Sync + 'static>(
    stream: &crate::launch_stream::LaunchStream,
    provider: &P,
    known: &mut Vec<(Address, Address, u64)>,
    known_fl: &mut Vec<FlCand>,
    known_v2: &mut Vec<PonsV2Cand>,
    shared: &Arc<Mutex<Vec<Row>>>,
    head: u64,
) {
    let live = stream.drain();
    if live.is_empty() {
        return;
    }
    let (mut pons, fl, v2, deployed) = sort_launch_logs(&live);
    // A creation pushed over the socket is the earliest anything can know
    // about a coin. Its pool is one call away, and the whole point of the
    // socket is not waiting for the next scan.
    for (token, block) in deployed {
        if pons.iter().any(|(t, _, _)| *t == token) {
            continue;
        }
        let fac = IV3Factory::new(v3_factory(), provider);
        let call = fac.getPool(token, weth(), pons_fee());
        if let Ok(Ok(p)) = tokio::time::timeout(RPC_TIMEOUT, call.call()).await {
            if !p.pool.is_zero() {
                pons.push((token, p.pool, block));
            }
        }
    }
    let (mut n_pons, mut n_fl, mut n_v2) = (0usize, 0usize, 0usize);
    for c in pons {
        crate::token_metadata_chain_id::record_launch(c.0, c.2);
        if !known.iter().any(|(t, _, _)| *t == c.0) {
            crate::trace(&format!("launch: pons {} block {} via ws", c.0, c.2));
            known.push(c);
            n_pons += 1;
        }
    }
    for c in fl {
        if !known_fl.iter().any(|k| k.token == c.token) {
            crate::trace(&format!("launch: flaunch {} {} block {} via ws", c.sym, c.token, c.block));
            known_fl.push(c);
            n_fl += 1;
        }
    }
    for c in v2 {
        if !known_v2.iter().any(|k| k.token == c.token) {
            crate::trace(&format!("launch: pons-v2 {} curve {} block {} via ws", c.token, c.curve, c.block));
            note_v2_supply(provider, &c);
            known_v2.push(c);
            n_v2 += 1;
        }
    }
    if n_pons + n_fl + n_v2 > 0 {
        crate::trace(&format!(
            "launch stream: {n_pons} pons + {n_fl} flaunch + {n_v2} pons v2, live"
        ));
        // On the list before this function returns, not at the next round.
        let seeded = seed_rows(shared, known, known_fl, head);
        if seeded > 0 {
            crate::trace(&format!("discovery: {seeded} streamed row(s) on the list"));
        }
    }
}

/// Discovery is a STREAM, and this is the whole model:
///
///  0. The list is THIS session's. Nothing is loaded from disk and nothing is
///     fetched from before the task started — the socket is the source, and
///     the scan exists only to cover its gaps while we run.
///  1. A launch joins the list the MOMENT it is seen — from the websocket, or
///     from the gap-cover scan. Its identity (token, pool, symbol, block) comes
///     from the event; its numbers start empty.
///  2. Nothing ever removes a row except the count cap, oldest first. Not age,
///     not depth, not "it has no trades". A session shows what launched during
///     it.
///  3. Measurement is best-effort and runs behind. It refreshes rows in place
///     and never decides whether one is worth showing.
///  4. Closing the screen changes the BUDGET — fewer facts per round, no sweep
///     of the older tail, a longer wait between rounds — and nothing else. The
///     list still grows while you are away.
///
/// Each of those was learned by breaking it. Rows used to wait for their facts
/// (so six of a hundred launches appeared), age out at two hours (so a list of
/// seven became three while being watched), and stop being built entirely when
/// the screen closed (so leaving and returning showed the list you left).
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
    // Consecutive rounds the cursor could not move, for the warning below.
    let mut stalled: u32 = 0;
    // Empty on purpose. This list is THIS session's launches — nothing is
    // loaded from disk, and nothing reaches back before the task started. The
    // page is for what is launching, not for what once launched.
    let mut known: Vec<(Address, Address, u64)> = Vec::new();
    // Flaunch launches ride the same incremental window, in their own list.
    let mut known_fl: Vec<FlCand> = Vec::new();
    // pons v2 launches still on their curve. Not persisted: a curve is a
    // TRANSIENT state — it graduates into a pool and the entry stops being
    // true — so a saved one would come back as a venue that no longer exists.
    let mut known_v2: Vec<PonsV2Cand> = Vec::new();
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
    // The live feed. Runs beside the scan below, not instead of it — see
    // `launch_stream`. Empty ws config makes this a no-op and the scan carries
    // on exactly as before.
    let stream = crate::launch_stream::spawn(
        crate::chain_id(),
        crate::ws_pool(),
        launch_addresses(),
        launch_topics(),
    );

    // Runs for the life of the process, NOT for the life of the screen.
    //
    // This used to be `while !stop`, which read as "stop when nobody is
    // looking" and meant "end the task". Leaving the screen sets that flag, and
    // `discovery_task` only ever creates ONE task — so the first time you left
    // discovery, the loop exited, nothing respawned it, and every later visit
    // showed the rows frozen at the moment you walked away. `stop` is the
    // visibility gate below, and only that.
    loop {
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
            // From the current block, not from the past. The scan exists to
            // cover the socket's gaps WHILE WE RUN — a disconnect, a dropped
            // message — never to reach into history. Prefetching old launches
            // is exactly what this page is not for.
            None => head,
            Some(t) if head > t => t + 1,
            Some(_) => head + 1, // nothing new; the scan returns at once
        };
        // Only the public endpoint can serve wide log scans, and it rate
        // limits. While it rests, SKIP the scan and leave the cursor alone —
        // asking anyway is what kept it benched forever, and the unmoved
        // cursor means the skipped blocks are scanned the moment it is back.
        note_head(head);
        // Whatever the socket pushed since the last round, FIRST — before the
        // scan, and regardless of whether the scan runs at all.
        //
        // This is the half that makes the guarantee: the scan below is skipped
        // when the wide-logs endpoint is resting, and narrowed when it is rate
        // limited, and neither of those touches the stream. A launch that
        // arrives during a deferred scan is on screen anyway.
        ingest_live(&stream, &provider, &mut known, &mut known_fl, &mut known_v2, &shared, head)
            .await;
        // Nobody is looking: keep collecting launches, stop paying for the
        // rest.
        //
        // The screen used to abort this task on the way out, so discovery only
        // existed while you were staring at it — step away to trade and come
        // back to the same list you left. Now the loop runs for the life of
        // the app and only the EXPENSIVE half is gated: the metric batch and
        // the swap tape are what cost bandwidth, and refreshing depth and
        // tx/sec for a table nobody is reading is the one thing worth skipping.
        //
        // What still runs is the part that makes the list grow: the websocket
        // drain above, and the launch scan below. Both are near-free — one is
        // pushed, the other is a single windowed getLogs — so returning shows
        // what launched while you were gone rather than what was there when
        // you left.
        let visible = !stop.load(Ordering::Relaxed);
        let wide_ok = crate::rpc::shared().is_none_or(|b| b.wide_ready());
        if from <= head && wide_ok {
            let (pons, fl, v2, covered) = scan_launchpads(&provider, from, head).await;
            for c in pons {
                crate::token_metadata_chain_id::record_launch(c.0, c.2);
                if !known.iter().any(|(t, _, _)| *t == c.0) {
                    crate::trace(&format!("launch: pons {} block {} via scan", c.0, c.2));
                    known.push(c);
                }
            }
            for c in fl {
                if !known_fl.iter().any(|k| k.token == c.token) {
                    crate::trace(&format!("launch: flaunch {} {} block {} via scan", c.sym, c.token, c.block));
                    known_fl.push(c);
                }
            }
            for c in v2 {
                if !known_v2.iter().any(|k| k.token == c.token) {
                    crate::trace(&format!("launch: pons-v2 {} curve {} block {} via scan", c.token, c.curve, c.block));
                    note_v2_supply(&provider, &c);
                    known_v2.push(c);
                }
            }
            // Only as far as the scan actually READ.
            //
            // This used to be `Some(head)` unconditionally, so a chunk the
            // provider refused — a free-tier block-range cap, a timeout —
            // advanced the cursor past blocks nobody had looked at. Those
            // blocks were never scanned again, and every launch inside them
            // was missed for good. That is the "sometimes a token never shows
            // up" that no amount of waiting fixed.
            //
            // Unchanged on a total failure, so the same range is retried next
            // round rather than abandoned.
            if let Some(c) = covered {
                scanned_to = Some(c);
                stalled = 0;
            } else {
                stalled += 1;
                // Loudly, and only once a spell: a cursor that cannot move is
                // a screen that has quietly stopped finding launches, and the
                // per-chunk errors above do not say that this is happening.
                if stalled % 30 == 1 {
                    crate::events::warn(
                        "Discovery cannot read new blocks — launches are being missed",
                        &[
                            ("from", from.to_string()),
                            ("to", head.to_string()),
                            ("attempts", stalled.to_string()),
                        ],
                    );
                }
            }
        } else if !wide_ok {
            crate::trace("discovery: wide-logs endpoint resting, scan deferred");
        } else {
            scanned_to = Some(head);
        }
        // Newest first, and bounded by the same cap as the rows. In memory
        // only: writing these to disk was how last week's launches greeted
        // every new session.
        known.sort_by(|a, b| b.2.cmp(&a.2));
        known.truncate(ROW_MAX);
        known_fl.sort_by(|a, b| b.block.cmp(&a.block));
        known_fl.truncate(ROW_MAX);
        known_v2.sort_by(|a, b| b.block.cmp(&a.block));
        known_v2.truncate(ROW_MAX);

        // NOT a `continue` any more.
        //
        // Skipping the rest of the round while the screen was closed meant
        // launches piled into `known` and no ROWS were ever built from them —
        // so leaving the trenches list and coming back showed the same list
        // you left, and it only started growing once you were watching it.
        // Which is precisely backwards: the screen you are not looking at is
        // the one that should be catching up.
        //
        // It still costs less when nobody is looking: the round-robin sweep of
        // the older tail is dropped, so only the newest candidates are
        // measured, and the wait at the end of the round is longer.
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
                    .address(pool_manager())
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
        // A candidate with no symbol and no supply is never turned into a row,
        // so stopping fact-fetching while the screen is closed meant launches
        // piled up and NONE of them became visible — the list still did not
        // grow while you were away, even after the round stopped being skipped.
        //
        // Fewer per round when nobody is watching, not none.
        let facts_budget = if visible { FACTS_PER_ROUND } else { FACTS_PER_ROUND.div_ceil(2) };
        let mut fetched = 0usize;
        for (t, p, _) in known.iter() {
            if fetched >= facts_budget {
                break;
            }
            let have = crate::token_metadata_chain_id::get(*t).is_some_and(|f| !f.sym.is_empty() && f.supply > 0.0);
            if !have {
                crate::token_metadata_chain_id::ensure(&provider, *t, Some(*p)).await;
                fetched += 1;
            }
        }
        for c in known_fl.iter() {
            if fetched >= facts_budget {
                break;
            }
            let have = crate::token_metadata_chain_id::get(c.token).is_some_and(|f| f.supply > 0.0);
            if !have {
                crate::token_metadata_chain_id::ensure_supply(&provider, c.token).await;
                let (sym, block) = (c.sym.clone(), c.block);
                crate::token_metadata_chain_id::merge(c.token, move |f| {
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

        // Every launch on the list, immediately. Measurement follows.
        let seeded = seed_rows(&shared, &known, &known_fl, head);
        if seeded > 0 {
            crate::trace(&format!("discovery: {seeded} new row(s) on the list"));
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
            /// A pons v2 launch on its curve. Refreshed by reading the curve
            /// itself — there is no pool to read.
            V2(PonsV2Cand),
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
        due.extend(known_v2.iter().take(MAX_SCAN).copied().map(Due::V2));
        let tail: Vec<Due> = known
            .iter()
            .skip(MAX_SCAN)
            .map(|(t, p, b)| Due::Pons(*t, *p, *b))
            .chain(fl_live.iter().skip(MAX_SCAN).cloned().map(Due::Fl))
            .collect();
        // Only while someone is watching. Refreshing depth and tx/sec for rows
        // nobody is reading is the one thing genuinely worth skipping — new
        // launches are not.
        if visible && !tail.is_empty() {
            for k in 0..SWEEP_CHUNK.min(tail.len()) {
                due.push(tail[(sweep_at + k) % tail.len()].clone());
            }
            sweep_at = (sweep_at + SWEEP_CHUNK) % tail.len();
        }
        // NOT filtered on whether the facts have arrived.
        //
        // This used to drop any launch whose symbol and supply were not yet
        // known, on the grounds that a row without them renders as garbage.
        // True, briefly — and the cost was that with facts arriving a few per
        // round, most launches never became rows at all. A list that shows six
        // of the last hundred is not tidier, it is wrong.
        //
        // A row appears the moment its launch is seen and fills in as the
        // facts land. Sorting is by block, which is known from the event, so
        // nothing re-sorts to nowhere while it waits.

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
                    calls.push((state_view(), IStateView::getSlot0Call { poolId: c.pool_id }.abi_encode()));
                    calls.push((state_view(), IStateView::getLiquidityCall { poolId: c.pool_id }.abi_encode()));
                    if with_bal {
                        calls.push((c.token, IERC20::balanceOfCall { owner: trader }.abi_encode()));
                    }
                }
                // A curve answers for itself. Two reads fill the same two
                // metric slots a pool uses: its reserves (price) and what it
                // has actually raised (depth, and progress to graduation).
                Due::V2(c) => {
                    calls.push((c.curve, IPonsCurve::getReservesCall {}.abi_encode()));
                    calls.push((c.curve, IPonsCurve::realQuoteReserveCall {}.abi_encode()));
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

        // Graduation, in its own tiny batch. It cannot ride in the main one —
        // that batch is a fixed stride of `per` calls per row and the decode
        // indexes into it — and only the pons rows have the question to ask.
        let pons_tokens: Vec<Address> = due
            .iter()
            .filter_map(|d| match d {
                Due::Pons(t, _, _) => Some(*t),
                _ => None,
            })
            .collect();
        let mut grad_map: std::collections::HashMap<Address, (f64, bool)> = Default::default();
        if !pons_tokens.is_empty() {
            let gcalls: Vec<(Address, Vec<u8>)> = pons_tokens
                .iter()
                .map(|t| (pons_factory(), IPonsFactory::graduationStatusCall { token: *t }.abi_encode()))
                .collect();
            let gres = batch_call(&client, &url, &gcalls).await;
            for (t, o) in pons_tokens.iter().zip(gres.iter()) {
                if let Some(d) = o.as_ref() {
                    if d.len() >= 96 {
                        let paired = uf(U256::from_be_slice(&d[..32]));
                        let thresh = uf(U256::from_be_slice(&d[32..64]));
                        let done = U256::from_be_slice(&d[64..96]) != U256::ZERO;
                        if thresh > 0.0 {
                            grad_map.insert(*t, (paired / thresh, done));
                        }
                    }
                }
            }
        }

        let mut swap_count: std::collections::HashMap<B256, usize> = Default::default();
        for (_, key) in &swaps {
            *swap_count.entry(*key).or_default() += 1;
        }
        // Rows survive between rounds, keyed by token, and keep their place:
        // a refreshed row replaces its previous self, everything else stays
        // untouched, and the ranking is applied once per round.
        for (i, d) in due.iter().enumerate() {
            // Stop ranking, not stop existing. `return` here ended the task
            // outright, which was the second way a closed screen killed
            // discovery for the rest of the session.
            if stop.load(Ordering::Relaxed) {
                break;
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
            let mut row = match d {
                Due::Pons(t, p, b) => {
                    let Some(f) = crate::token_metadata_chain_id::get(*t) else { continue };
                    let n = swap_count.get(&p.into_word()).copied().unwrap_or(0);
                    build_row(*t, *p, *b, &f, sqrt, liq, my_bal, n, head)
                }
                Due::Fl(c) => {
                    let n = swap_count.get(&c.pool_id).copied().unwrap_or(0);
                    build_fl_row(c, sqrt, liq, my_bal, n, head)
                }
                // The generic slots hold single numbers; a curve's first read
                // returns TWO, so it is decoded from the raw response here
                // rather than through the pool-shaped path above.
                Due::V2(c) => {
                    let raw = res.get(i * per).and_then(|o| o.as_ref());
                    let (qr, tr) = match raw {
                        Some(d) if d.len() >= 64 => (
                            uf(U256::from_be_slice(&d[..32])),
                            uf(U256::from_be_slice(&d[32..64])),
                        ),
                        _ => continue,
                    };
                    let mut r = build_v2_row(c, qr, tr, liq, my_bal, head);
                    // The curve knows its own progress: `liq` here is its real
                    // raised quote, and the candidate carries the threshold.
                    let thresh = uf(c.threshold);
                    if thresh > 0.0 {
                        r.grad_pct = Some(liq / thresh);
                    }
                    r
                }
            };
            if let Due::Pons(t, _, _) = d {
                if let Some((pct, done)) = grad_map.get(t) {
                    row.grad_pct = Some(*pct);
                    row.graduated = *done;
                }
            }
            let mut cur = shared.lock().unwrap();
            match cur.iter_mut().find(|r| r.grad.token == row.grad.token) {
                Some(slot) => {
                    // A refresh whose grad batch failed answers None; the
                    // previous answer is better than forgetting it, and
                    // graduation never un-happens.
                    if row.grad_pct.is_none() {
                        row.grad_pct = slot.grad_pct;
                    }
                    row.graduated |= slot.graduated;
                    *slot = row; // refresh, same position
                }
                None => cur.push(row), // genuinely new, at the end
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
        // Age out, cap, then re-rank — once per round, in that order.
        //
        // This was written, documented and never called. It lived in
        // `merge_publish`, which nothing invoked, and `#![allow(dead_code)]` at
        // the top of this module meant the compiler never said so. The rows
        // above are inserted and refreshed by token and otherwise kept
        // forever, which is why a Flaunch launch nobody bought was still on
        // the screen three hours later, and why the list had no ceiling.
        //
        // The reference is the LIVE head, not the newest `head_block` among
        // The list is capped, and nothing else takes a row off it.
        {
            let mut cur = shared.lock().unwrap();
            let before = cur.len();
            sort_rows(&mut cur);
            // Newest first, so the truncation drops the oldest.
            cur.truncate(ROW_MAX);
            if cur.len() < before {
                crate::trace(&format!(
                    "discovery: {} oldest row(s) dropped at the cap ({} -> {})",
                    before - cur.len(),
                    before,
                    cur.len()
                ));
            }
        }
        // The wait between rounds is interruptible: the socket rings, the
        // launch is ingested and ON SCREEN, and the wait resumes for whatever
        // time is left. This is the difference between listening and
        // streaming — the old sleep meant a launch could sit in the buffer for
        // up to six seconds after the node had already pushed it to us.
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(if visible { 2000 } else { 6000 });
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = stream.wait() => {
                    ingest_live(&stream, &provider, &mut known, &mut known_fl, &mut known_v2, &shared, head)
                        .await;
                }
            }
        }
    }
}

/// Start discovery with the SESSION, not with the first visit to a screen.
///
/// The Solana feed has always worked this way — `ensure_launch_feed` runs from
/// dashboard start, and its trenches screen opens onto everything collected
/// since. EVM discovery began at the first `f` press, so the first visit could
/// only show what the backstop scan recovers: ten minutes, against a session
/// that may be hours old. Idempotent; the screen calls it too.
pub fn ensure_discovery<P: Provider + Clone + Send + Sync + 'static>(
    provider: &P,
    trader: Address,
    disc_url: Option<String>,
    eth_usd: f64,
    verified: Vec<VerifiedPool>,
) {
    let (stop, fresh) = discovery_task();
    if fresh {
        // Background budgets until a screen opens and clears the flag.
        stop.store(true, Ordering::Relaxed);
        tokio::spawn(run_discovery(
            provider.clone(),
            trader,
            rows_cache(),
            stop,
            disc_url,
            eth_usd,
            verified,
        ));
    }
}

/// The one discovery task's visibility flag, and whether this call created it.
///
/// `true` means the screen is closed. The task keeps running either way — see
/// the visibility gate in `run_discovery` — so this decides how much work it
/// does, not whether it exists.
fn discovery_task() -> (Arc<AtomicBool>, bool) {
    static TASK: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
    let mut created = false;
    let flag = TASK.get_or_init(|| {
        created = true;
        Arc::new(AtomicBool::new(false))
    });
    (flag.clone(), created)
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
    // ONE discovery task per chain, for the life of the app.
    //
    // Reopening the screen used to start another, and now that the task no
    // longer dies on the way out that would stack one per visit. The flag is
    // shared instead: opening clears it, leaving sets it, and the running task
    // reads it as "is anyone looking".
    ensure_discovery(provider, trader, disc_url, eth_usd, verified);
    let (stop, _) = discovery_task();
    stop.store(false, Ordering::Relaxed);

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
            let r = shared.lock().unwrap().clone();
            // NOT truncated, and deliberately not sized to a screen either.
            // The list was capped at ten while the pane had room for fifty, so
            // a four-minute-old launch was already unreachable on the one
            // screen whose job is showing what launched. Terminals differ; the
            // table is stateful, so it shows what fits and scrolls for the
            // rest, and the cap was the only thing stopping it.
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
                if crate::ui::widgets::theme_key(term, k.code)? { continue; }
                match k.code {
                    // A launchpad producing one a minute outgrows what j/k
                    // can cross a row at a time.
                    KeyCode::PageUp => sel = sel.saturating_sub(10),
                    KeyCode::PageDown => sel = (sel + 10).min(rows.len().saturating_sub(1)),
                    KeyCode::Home => sel = 0,
                    KeyCode::End => sel = rows.len().saturating_sub(1),
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

    // The task is NOT aborted. `stop` now means "the screen is closed", which
    // parks the expensive half and leaves the launch feed collecting — see the
    // visibility gate in `run_discovery`.
    stop.store(true, Ordering::Relaxed);
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
    // With nothing found yet the title says only what it always says. It used
    // to add "discovering token launches…", which the body of the screen was
    // already saying in the middle of an otherwise empty page — the same
    // sentence twice, once in the place meant to hold still.
    //
    // The keys it advertises DO change, because those are what the screen can
    // do rather than what it is currently doing: with no rows there is nothing
    // to select or trade, and offering keys that do nothing is worse than
    // offering none.
    let v = env!("CARGO_PKG_VERSION");
    // No live count in the title: the list IS the count, and a number that
    // ticks up makes the whole header jitter. Keys wear their brackets, same
    // as everywhere else in the app.
    let text = if rows == 0 {
        format!(" Trenches Bot v{v}  [Esc] back ")
    } else {
        format!(" Trenches Bot v{v}  [j/k] select  [Enter] trade  [Esc] back ")
    };
    // Whether the live feed is up, and how much it has delivered.
    //
    // Without this there is no way to tell a quiet chain from a websocket that
    // was never configured — both look like a list that stopped growing, and
    // only one of them is worth doing something about. Silence is the thing a
    // status light exists to break.
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
        let is_buy = weth_amt > 0.0; // weth() INTO the pool = a buy
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
                    // The quote side is whatever the pool was launched
                    // against — increasingly a tokenized equity, not a
                    // stablecoin. Naming it "USDG" regardless labels an
                    // NVDA-quoted pool with the wrong asset.
                    let q = if v.quote.is_eth() {
                        "ETH".to_string()
                    } else {
                        crate::token_metadata_chain_id::get(v.quote.addr())
                            .map(|f| f.sym)
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "quote".to_string())
                    };
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
                if crate::ui::widgets::theme_key(term, k.code)? { continue; }
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
                            socials: engine::TokenSocials::default(),
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

/// Compact money in millions/thousands: $4.20M, $840k, $120.
///
/// Takes USD, renders in the reader's currency — the same rule as everywhere
/// else. This screen was the last one still writing a dollar sign of its own,
/// so a leaderboard read in euros had every market cap labelled in dollars.
fn usd_m(x: f64) -> String {
    let sym = crate::base_currency::symbol();
    let x = crate::base_currency::from_usd(x);
    if x >= 1e6 {
        format!("{sym}{:.2}M", x / 1e6)
    } else if x >= 1e3 {
        format!("{sym}{:.0}k", x / 1e3)
    } else {
        format!("{sym}{:.0}", x)
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
        if token == weth() { continue; }
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
/// The assets a listed token might be paired against, and the rate needed to
/// compare one pool's depth against another's.
///
/// The leaderboard used to probe WETH alone, so anything quoted in something
/// else read as "0.00 ETH" — USDE at $3.9B market cap and $30M of daily volume
/// showed no liquidity at all, which is not a small error on a screen whose one
/// job is ranking what is worth trading.
///
/// `usd` is what one unit is worth, for choosing between a WETH pool and a USDG
/// one. ETH's is passed in; a dollar stablecoin is a dollar.
struct QuoteAsset {
    token: Address,
    sym: &'static str,
    decimals: i32,
}

/// A function, not a const: the wrapped-native token is per chain now, and a
/// table baked at compile time would name Robinhood's WETH on every chain.
fn quotes() -> [QuoteAsset; 2] {
    [
        QuoteAsset { token: weth(), sym: "ETH", decimals: 18 },
        // 6-dec, not 18. Reading a USDG reserve as 18 shows $2.6M of depth as 0.
        QuoteAsset { token: USDG, sym: "USDG", decimals: 6 },
    ]
}

async fn enrich_pooled(
    client: &reqwest::Client,
    url: &str,
    rows: &mut [LeaderRow],
    eth_usd: f64,
) {
    let fees = [10000u32, 3000, 500, 100];
    // One batch for every (row, quote, fee) triple. Three times the lookups of
    // the WETH-only version, on a screen that is opened deliberately and reads
    // once — cheap next to listing a $3.9B token as having no pool.
    let mut pool_calls = Vec::with_capacity(rows.len() * quotes().len() * fees.len());
    for r in rows.iter() {
        for q in quotes().iter() {
            for &fee in &fees {
                let data = IV3Factory::getPoolCall {
                    tokenA: r.token,
                    tokenB: q.token,
                    fee: fee.try_into().unwrap(),
                }
                .abi_encode();
                pool_calls.push((v3_factory(), data));
            }
        }
    }
    let pool_res = batch_call(client, url, &pool_calls).await;
    // (row idx, quote idx, pool addr) for every non-zero pool found.
    let mut pools: Vec<(usize, usize, Address)> = Vec::new();
    for (i, res) in pool_res.iter().enumerate() {
        let Some(d) = res.as_ref() else { continue };
        if d.len() < 32 {
            continue;
        }
        let pool = Address::from_word(B256::from_slice(&d[d.len() - 32..]));
        if pool == Address::ZERO {
            continue;
        }
        let ri = i / (quotes().len() * fees.len());
        let qi = (i / fees.len()) % quotes().len();
        pools.push((ri, qi, pool));
    }
    // Each pool's balance of ITS OWN quote asset — not of WETH, which is what
    // made a USDG pool read as empty.
    let bal_calls: Vec<(Address, Vec<u8>)> =
        pools.iter().map(|(_, qi, p)| (quotes()[*qi].token, balanceof_data(*p))).collect();
    let bals = batch_call(client, url, &bal_calls).await;
    // Keep the DEEPEST pool per row, compared in dollars so a 2.6M USDG pool
    // beats a dust WETH one instead of losing to it on the raw number.
    let mut best_usd = vec![0.0f64; rows.len()];
    for ((ri, qi, _), b) in pools.iter().zip(bals.iter()) {
        let q = &quotes()[*qi];
        let amt = b.as_ref().map(|d| uf(u256_of(d)) / 10f64.powi(q.decimals)).unwrap_or(0.0);
        let rate = if q.token == weth() { eth_usd } else { 1.0 };
        let usd = amt * rate;
        // With no ETH price yet, dollars cannot rank anything — fall back to
        // the raw amount rather than silently preferring whichever came last.
        let score = if rate > 0.0 { usd } else { amt };
        if score > best_usd[*ri] {
            best_usd[*ri] = score;
            rows[*ri].pooled = amt;
            rows[*ri].pooled_unit = q.sym;
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
    let factory = IV3Factory::new(v3_factory(), provider);
    let mut errors = 0usize;
    let mut last = String::new();
    for fee in [10000u32, 3000, 500, 100] {
        match factory.getPool(token, weth(), fee.try_into().unwrap()).call().await {
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
                        kind: engine::PoolKind::V3 { pool_addr: addr, weth_is_token0: weth() < token },
                        quote: engine::Quote::Eth,
                        sym: sym.to_string(),
                        fee,
                        launch_block: 0,
                        socials: engine::TokenSocials::default(),
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
    crate::trace(&format!("resolve {sym}: no weth() pool with liquidity at any fee tier"));
    Ok(None)
}

/// The top-tokens screen ('t'): tabbed leaderboard + big-fish. Returns the chosen
/// token as a Grad (leaderboard picks are resolved to their v3 pool on Enter).
pub async fn screen_top_tokens<P: Provider>(term: &mut Term, provider: &P, disc_url: Option<String>, eth_usd: f64) -> eyre::Result<Option<Grad>> {
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
    // Depth in whatever the token is actually paired against, not in WETH
    // regardless. See `enrich_pooled`.
    enrich_pooled(&client, &url, &mut leader, eth_usd).await;

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
        "top tokens: {} of {listed} have a funded quote pool; the rest are not tradable here",
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
                if crate::ui::widgets::theme_key(term, k.code)? { continue; }
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => { if n > 0 { sel = (sel + 1).min(n - 1); } }
                    KeyCode::Enter => {
                        if let Some(r) = leader.get(sel) {
                            draw_status(term, "\nResolving pool…")?;
                            match resolve_v3_grad(provider, r.token, &r.sym).await {
                                Ok(Some(g)) => return Ok(Some(g)),
                                Ok(None) => {
                                    note = format!("  · {}: no live weth() pool", r.sym)
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
        engine::PoolKind::V3 { .. } => "Pons",
        // Spelled out. "flnch" saves one column and costs the reader a
        // guess at which launchpad they are looking at, which is the only
        // thing this column is for.
        engine::PoolKind::FlaunchV4 { .. } => "Flaunch",
        // Still on its curve — a different thing to trade than a pool, and the
        // column is the place that says so.
        engine::PoolKind::PonsCurve { .. } => "Pons v2",
        // Graduated: same launchpad, now a pool.
        engine::PoolKind::PonsV2Pool { .. } => "Pons v2",
        engine::PoolKind::V4 { .. } => "Uniswap",
    }
}

fn render_table(f: &mut Frame, rows: &[Row], sel: usize, state: &mut TableState) {
    // Split: table on top, a details box (socials for the selected row) below.
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).split(f.area());
    let header = ratatui::widgets::Row::new([
        "", "source", "symbol", "pooled ETH", "mkt cap", "grad", "tx/sec", "age", "mine", "pool",
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
            ratatui::widgets::Row::new(vec![
                Cell::from(if fire { "🔥" } else { "" }),
                Cell::from(venue_tag(&r.grad))
                    .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Dim))),
                Cell::from(r.grad.sym.clone()).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::from(format!("{:.4}", r.pooled_eth)),
                Cell::from(format!("{:.3} ETH", r.mkt_cap_eth)),
                // Graduation: latched "grad", a live percentage, or nothing
                // where the venue has no such concept.
                match (r.graduated, r.grad_pct) {
                    (true, _) => Cell::from("grad")
                        .style(Style::default().fg(crate::ui::widgets::tone_color(crate::view::Tone::Good))),
                    (false, Some(p)) => Cell::from(format!("{:.0}%", (p * 100.0).min(999.0))),
                    (false, None) => Cell::from(""),
                },
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
        // Wide enough for "flaunch" spelled out — see `venue_tag`.
        Constraint::Length(7),
        Constraint::Length(12),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(5),
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
        .block(
            crate::ui::widgets::themed_block_line(trenches_title(rows.len())).title_bottom(
                // Where you are in a list that no longer fits. Bottom border,
                // not the header: the header is fixed text, and a counter that
                // ticks up there moves every word after it.
                Line::from(format!(
                    " {}/{}  ↑↓ jk · PgUp/PgDn · Home/End ",
                    (sel + 1).min(rows.len().max(1)),
                    rows.len()
                ))
                .right_aligned(),
            ),
        );
    f.render_stateful_widget(table, chunks[0], state);

    // Details box for the selected pool — the actual X / telegram / website.
    let detail: Vec<Line> = match rows.get(sel) {
        Some(r) => {
            let m = &r.grad.socials;
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
            flaunch_pm(),
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
            currency0: crate::contracts::fleth(),
            currency1: ev.data._memecoin,
            fee: alloy::primitives::aliases::U24::ZERO,
            tickSpacing: crate::contracts::FLAUNCH_TICK_SPACING.try_into().unwrap(),
            hooks: flaunch_pm(),
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
        let sv = IStateView::new(state_view(), &lp);
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
        let from = crate::contracts::fleth();
        let tx = alloy::rpc::types::TransactionRequest::default()
            .to(crate::contracts::universal_router())
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
                address: pons_v2_factory(),
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
                address: pons_v2_factory(),
                data: LogData::new_unchecked(
                    vec![IPonsFactory::TokenLaunched::SIGNATURE_HASH],
                    Bytes::from(vec![0u8; 96]),
                ),
            },
            ..Default::default()
        };
        assert!(decode_pons_v2_log(&lg).is_none(), "wrong topic must not decode");
    }

    /// Age is told by ONE clock, and that clock is the live head.
    ///
    /// A row measured six rounds ago must not still report the age it had then.
    /// This is what made the whole screen look alive while it was frozen: the
    /// task that advances the head was the same task that refreshes the
    /// metrics, so when it died, every age stopped with it — and a list of
    /// unchanging ages reads as a list that simply has not changed.
    #[test]
    fn age_is_measured_against_the_live_head_not_the_row_stamp() {
        fn row(launch_block: u64, measured_at: u64) -> Row {
            Row {
                grad: Grad {
                    token: Address::ZERO,
                    kind: engine::PoolKind::V3 { pool_addr: Address::ZERO, weth_is_token0: true },
                    quote: engine::Quote::Eth,
                    sym: "T".into(),
                    fee: 3000,
                    launch_block,
                    socials: Default::default(),
                },
                pooled_eth: 0.0,
                mkt_cap_eth: 0.0,
                tx_per_sec: 0.0,
                my_bal: 0.0,
                grad_pct: None,
                graduated: false,
                head_block: measured_at,
                verified: false,
            }
        }
        // Measured long ago, at a head only two blocks past its launch.
        let stale = row(1_000, 1_002);
        let then = age_secs(&stale);
        // The chain moves on. Nothing re-measured this row.
        note_head(1_100);
        let now = age_secs(&stale);
        assert!(now > then, "the age must grow with the chain: {then} -> {now}");
        assert!(
            (now - 100.0 * SECS_PER_BLOCK).abs() < f64::EPSILON,
            "and be told from the live head, not the row's own"
        );
    }

}
